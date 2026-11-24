//! One full synchronization, yielded only between completed durable units.
//! The continuation owns its finite catalog and first failure until terminal completion.
use super::*;

enum Phase {
    Registrations,
    Owned,
    Sessions,
    Files,
    Finish,
}

pub(crate) struct Synchronization {
    phase: Phase,
    volumes: Vec<Id>,
    sessions: Vec<Session>,
    synchronized: Vec<usize>,
    unavailable_sessions: BTreeSet<String>,
    next: usize,
    total: usize,
    scope: ClientScope,
    first_error: Option<Box<dyn std::error::Error + Send + Sync>>,
    // Wall time covers the entire synchronization, including foreground loans.
    _timing: crate::trace::Interval,
}

#[cfg(all(feature = "test-support", feature = "app-host"))]
impl crate::engine::Engine {
    /// Let real-AP tests interleave local first use after the owner and listener passes.
    pub fn sync_at_first_session_for_test(&self) -> bool {
        self.synchronization
            .as_ref()
            .is_some_and(|sync| matches!(sync.phase, Phase::Sessions) && sync.next == 0)
    }
}

impl Synchronization {
    pub(in crate::engine) fn new(
        store: &LocalStore,
        scope: ClientScope,
        timing: crate::trace::Interval,
    ) -> Result<Self> {
        let volumes = remote_volumes(store)?
            .into_iter()
            .filter(|volume| volume.owner == store.account().id && scope.admits(volume.id))
            .map(|volume| volume.id)
            .collect::<Vec<_>>();
        Ok(Self {
            total: volumes.len(),
            volumes,
            phase: Phase::Registrations,
            sessions: Vec::new(),
            synchronized: Vec::new(),
            unavailable_sessions: BTreeSet::new(),
            next: 0,
            scope,
            first_error: None,
            _timing: timing,
        })
    }

    pub(super) fn advance(&mut self, remote: &mut Remote, store: &Arc<LocalStore>) -> Result<bool> {
        match self.phase {
            Phase::Registrations => self.registrations(remote, store)?,
            Phase::Owned => self.owned(remote, store)?,
            Phase::Sessions => self.session(remote, store)?,
            Phase::Files => self.files(remote, store)?,
            Phase::Finish => {
                let result = self.first_error.take().map_or(Ok(()), Err);
                remote.finish_synchronization(store, result, &self.unavailable_sessions)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn registrations(&mut self, remote: &Remote, store: &LocalStore) -> Result<()> {
        // Account catch-up can overlap locally queued registrations. Preserve
        // the post-account pass before admitting this catalog's owner roots.
        let result = remote.connection.as_ref().ok_or_else(|| "AP disconnected".into()).and_then(
            |connection| remote.sync_registrations(store, connection, self.scope.selected()),
        );
        match result {
            Ok(deferred) => {
                if let Some(&first) = deferred.first() {
                    self.volumes.retain(|volume| !deferred.contains(volume));
                    self.total = self.volumes.len();
                    self.retain_error(Err(sync_failure::volume(
                        store,
                        first,
                        wire::SharedServiceUpgradeRequired,
                    )));
                }
            }
            Err(error) => {
                self.retain_error(Err(error));
                self.next = self.volumes.len();
                if self.scope.provider() {
                    self.phase = Phase::Finish;
                    return Ok(());
                }
            }
        }
        self.phase = Phase::Owned;
        Ok(())
    }

    fn owned(&mut self, remote: &mut Remote, store: &Arc<LocalStore>) -> Result<()> {
        if let Some(volume) = self.volumes.get(self.next).copied() {
            remote.progress.set(
                Some(TransferProgress::Volumes { current: self.next + 1, total: self.total }),
                &remote.completion,
            );
            let result = store
                .volume(volume)
                .map_err(Into::into)
                .and_then(|descriptor| remote.sync_owned_volume(store, descriptor));
            let deferred = result
                .as_ref()
                .is_err_and(|error| error.is::<wire::SharedServiceUpgradeRequired>());
            self.retain_error(result.map_err(|error| sync_failure::volume(store, volume, error)));
            if !deferred && let Some(live) = &remote.live {
                self.retain_error(
                    remote
                        .runtime
                        .block_on(live.publisher().account_volume_ready(volume))
                        .map_err(|error| sync_failure::volume(store, volume, error)),
                );
            }
            self.next += 1;
            return Ok(());
        }
        if self.scope.provider() {
            return self.begin_files(store);
        }
        self.retain_error(remote.start_listeners(store));
        let sessions = store.sessions()?;
        // Retain the same catalog snapshot as synchronous full sync. Successful
        // session indices add no second catalog or new sidebar/live-slot limit.
        self.total = sessions.len();
        self.sessions = sessions;
        self.next = 0;
        self.phase = Phase::Sessions;
        Ok(())
    }

    fn session(&mut self, remote: &mut Remote, store: &Arc<LocalStore>) -> Result<()> {
        let Some(session) = self.sessions.get(self.next) else {
            self.retain_error(remote.checkpoint_pending_changes(store));
            return self.begin_files(store);
        };
        let index = self.next;
        self.next += 1;
        remote.progress.set(
            Some(TransferProgress::Sessions { current: self.next, total: self.total }),
            &remote.completion,
        );
        match remote.sync_required_session(store, session) {
            Ok(()) => {
                self.synchronized.push(index);
                let world = remote.sync_optional_world(store, session);
                self.retain_error(world);
            }
            Err(error) if error.is::<SessionBuildUnavailable>() => {
                self.unavailable_sessions.insert(session.id.clone());
            }
            Err(error) => {
                if store.session_denial(&session.id).is_ok_and(|denial| denial.is_some())
                    && let Some(listener) =
                        remote.listeners.lock().expect("mutex is not poisoned").remove(&session.id)
                {
                    listener.abort();
                }
                let error = sync_failure::volume(store, session.replica_volume(), error);
                self.retain_error(Err(error));
            }
        }
        Ok(())
    }

    fn begin_files(&mut self, store: &LocalStore) -> Result<()> {
        self.volumes = remote_volumes(store)?
            .into_iter()
            .filter(|volume| self.scope.admits(volume.id))
            .map(|volume| volume.id)
            .collect();
        self.total = self.volumes.len();
        self.next = 0;
        self.phase = Phase::Files;
        Ok(())
    }

    fn files(&mut self, remote: &mut Remote, store: &LocalStore) -> Result<()> {
        let Some(volume) = self.volumes.get(self.next).copied() else {
            self.phase = Phase::Finish;
            return Ok(());
        };
        self.next += 1;
        remote.progress.set(
            Some(TransferProgress::Files { current: self.next, total: self.total }),
            &remote.completion,
        );
        self.retain_error(
            remote
                .sync_blob_selected(
                    store,
                    volume,
                    self.synchronized.iter().map(|index| &self.sessions[*index]),
                )
                .map_err(|error| sync_failure::volume(store, volume, error)),
        );
        Ok(())
    }

    fn retain_error(&mut self, result: Result<()>) {
        if let Err(error) = result {
            initialization::retain_error(&mut self.first_error, error);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful isolated fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn account_only_provider_never_admits_volumes_or_session_phase() {
        // Account discovery must stay useful for an empty drive without using
        // FullClient's meaning of an unrestricted volume/session catalog.
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let first = store.create_volume("First", true).unwrap();
        store.create_volume("Unrelated", true).unwrap();
        let timing = || crate::trace::Interval::new("replication.sync");
        let mut empty =
            Synchronization::new(&store, ClientScope::Provider(vec![]), timing()).unwrap();
        assert!(empty.volumes.is_empty());
        let completion = crate::engine::completion::Completion::unix().unwrap();
        let mut remote = Remote::open(directory.path(), completion.wake.clone()).unwrap();
        empty.phase = Phase::Owned;
        assert!(!empty.advance(&mut remote, &store).unwrap());
        assert!(matches!(empty.phase, Phase::Files));
        assert!(empty.sessions.is_empty());
        assert!(empty.volumes.is_empty());
        let selected =
            Synchronization::new(&store, ClientScope::Provider(vec![first.id]), timing()).unwrap();
        assert_eq!(selected.volumes, [first.id]);

        let package = crate::package::Package::new(
            "test".into(),
            "Test".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::new(),
        )
        .unwrap();
        let package = crate::package::signature::testing::signed(&store, package).unwrap();
        let builds = directory.path().join(crate::paths::BUILDS);
        std::fs::create_dir_all(&builds).unwrap();
        std::fs::write(builds.join(package.id().unwrap()), package.to_bytes().unwrap()).unwrap();
        store.remember_imported_app(&package).unwrap();
        let local = store
            .open_app_volume(
                package.identity().unwrap(),
                crate::package::AutomaticVolumeRole::Local,
            )
            .unwrap();
        let mut selected =
            Synchronization::new(&store, ClientScope::Provider(vec![first.id, local.id]), timing())
                .unwrap();
        assert_eq!(selected.volumes, [first.id]);
        selected.begin_files(&store).unwrap();
        assert_eq!(selected.volumes, [first.id]);
    }

    #[test]
    fn the_first_sync_error_survives_later_units_and_terminal_completion() {
        // A later unit's success/error cannot replace the original terminal failure.
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let completion = crate::engine::completion::Completion::unix().unwrap();
        let mut remote = Remote::open(directory.path(), completion.wake.clone()).unwrap();
        let mut state = Synchronization::new(
            &store,
            ClientScope::FullClient,
            crate::trace::Interval::new("replication.sync"),
        )
        .unwrap();
        let first = store.create_volume("first\nwaiting for an authorized peer", true).unwrap();
        let cause = sync_failure::volume(&store, first.id, "access denied: volume permission");
        assert!(!waiting_peer(cause.as_ref()), "a name cannot select private-peer recovery");
        state.retain_error(Err(cause));
        state.retain_error(Ok(()));
        state.retain_error(Err("later volume failed".into()));
        state.phase = Phase::Finish;
        assert_eq!(
            state.advance(&mut remote, &store).unwrap_err().to_string(),
            format!(
                "Couldn’t sync volume \"first\\nwaiting for an authorized peer\" ({})\naccess denied: volume permission",
                first.id
            )
        );
    }
}

#[cfg(all(test, feature = "app-host"))]
mod registration_tests;
