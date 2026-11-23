//! Serialized Wasm ownership for an in-process UI embedding. Engine lends
//! authoritative services; guest turns and semantic scenes stay with this owner.
use crate::{
    engine::{
        Result, Services,
        connected::{ReplicaNotice, ReplicaSource},
        data_host::DataHost,
        host::LocalHost,
        in_process_host::{InProcessHost, Inbox},
        session_binding::Binding,
    },
    package::Package,
    runtime::{
        ExecutionBackend, Runtime, ViewNode,
        syscall::{Event, EventKind, GuestInstance, HostError, validate_input},
    },
    store::Id,
};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const MAX_APPS: usize = 4;
fn admits(active: usize, retired: usize) -> bool {
    active.checked_add(retired).is_some_and(|count| count < MAX_APPS)
}
#[derive(Default)]
struct Changes {
    sources: BTreeSet<ReplicaSource>,
    gap: bool,
}
#[derive(Clone, Default)]
pub(crate) struct Notices(Arc<Mutex<Changes>>);
impl Notices {
    pub(crate) fn received(&self, notices: &[ReplicaNotice]) {
        let mut changes = self.0.lock().expect("embedding changes lock poisoned");
        for notice in notices.iter().filter(|notice| notice.dirty) {
            if changes.sources.len() == 32 && !changes.sources.contains(&notice.source) {
                changes.gap = true;
            } else {
                changes.sources.insert(notice.source);
            }
        }
    }
}
struct Resident {
    id: Id,
    session: String,
    store: Arc<crate::store::LocalStore>,
    observed: Option<(Id, u64)>,
    binding: Binding,
    reads: Vec<Id>,
    writes: Vec<Id>,
    changed: Arc<AtomicBool>,
    guest: GuestInstance,
    inbox: Inbox,
    scene: Option<ViewNode>,
    encoded: Vec<u8>,
    logs: Vec<String>,
    log_next: u64,
    dirty: bool,
    draining: bool,
    failure: Option<String>,
}
pub(crate) struct Apps {
    running: Vec<Resident>,
    // Accepted storage keeps capacity after its delivery owner is retired.
    retired: Vec<Inbox>,
    notices: Notices,
}
impl Default for Apps {
    fn default() -> Self {
        Self {
            running: Vec::with_capacity(MAX_APPS),
            retired: Vec::with_capacity(MAX_APPS),
            notices: Notices::default(),
        }
    }
}
impl Apps {
    #[cfg(feature = "ios")]
    pub(crate) fn deactivate_all(&mut self) {
        for app in &mut self.running {
            app.draining = true;
            app.guest.deactivate();
        }
    }
    #[cfg(feature = "ios")]
    pub(crate) fn shutdown_pending(&self) -> Result<bool> {
        let mut pending = self.retired.iter().any(|inbox| !inbox.idle());
        for app in &self.running {
            pending |= self.drain_pending(app.id)?;
        }
        Ok(pending)
    }
    pub(crate) fn sessions(&self) -> Vec<String> {
        self.running.iter().map(|app| app.session.clone()).collect()
    }
    pub(crate) fn notices(&self) -> Notices {
        self.notices.clone()
    }
    #[cfg(test)]
    pub(crate) fn id(&self, session: &str) -> Option<Id> {
        self.running.iter().find(|app| app.session == session).map(|app| app.id)
    }
    /// Management preparation happens on the embedding worker, before guest
    /// admission. Every guest data call subsequently uses async begin/take.
    pub(crate) fn open(
        &mut self,
        services: &Services,
        session: &str,
        blobs: crate::engine::blob::BlobTransport,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Id> {
        self.retired.retain(|inbox| !inbox.idle());
        if !admits(self.running.len(), self.retired.len()) {
            return Err(
                "Four apps have active or draining work. Close an app before opening another."
                    .into(),
            );
        }
        let record = services.store.session(session)?;
        if let Some(reason) = services.store.session_denial(session)? {
            return Err(reason.into());
        }
        let package = Package::open(
            &services.profile_path().join(crate::paths::BUILDS).join(&record.resolved_build_id),
        )?;
        let (binding, mut context) = Binding::prepare(services, session, &package)
            .map_err(|error| format!("App admission failed: {error:?}"))?;
        let id = Id::from_bytes(context.identity.instance_id);
        let (reads, writes) = scopes(&context.volumes);
        let changed = Arc::new(AtomicBool::new(false));
        let backend = LocalHost {
            data: DataHost {
                store: services.store.clone(),
                access: binding.access.clone(),
                changed: changed.clone(),
            },
            blobs: blobs.clone(),
            session: session.into(),
        };
        let store = services.store.clone();
        let (mut host, inbox) = InProcessHost::start(
            Box::new(backend.clone()),
            Box::new(backend.clone()),
            Box::new(backend),
            crate::engine::in_process_host::scoped_revision(
                store,
                Some(binding.delivery_volumes()),
            ),
            wake,
        )?;
        binding
            .bind_session_info(&mut host)
            .map_err(|error| format!("Session info admission failed: {error:?}"))?;
        context.volumes = host
            .enable_files(
                services,
                binding.access.clone(),
                Some(session.into()),
                Some(blobs),
                &context.volumes,
            )
            .map_err(|error| format!("File admission failed: {error:?}"))?;
        let runtime = Runtime::with_backend(if cfg!(target_os = "ios") {
            ExecutionBackend::Pulley
        } else {
            ExecutionBackend::Native
        })?;
        let guest = runtime.create_guest(&package.wasm()?, context, Box::new(host))?;
        self.running.push(Resident {
            id,
            session: session.into(),
            store: services.store.clone(),
            observed: services.store.delivery_scope_revision(None, &reads).0,
            binding,
            reads,
            writes,
            changed,
            guest,
            inbox,
            scene: None,
            encoded: vec![],
            logs: vec![],
            log_next: 0,
            dirty: false,
            draining: false,
            failure: None,
        });
        Ok(id)
    }
    fn app(&self, id: Id) -> Result<&Resident> {
        self.running
            .iter()
            .find(|app| app.id == id)
            .ok_or_else(|| "The app instance is closed".into())
    }
    pub(crate) fn session(&self, id: Id) -> Result<&str> {
        Ok(&self.app(id)?.session)
    }
    pub(crate) fn generation(&self, id: Id) -> Result<u64> {
        Ok(self.app(id)?.guest.generation())
    }
    pub(crate) fn frame_bytes(&self, id: Id) -> Result<&[u8]> {
        let app = self.app(id)?;
        if let Some(error) = &app.failure {
            return Err(error.clone().into());
        }
        Ok(&app.encoded)
    }
    pub(crate) fn scene(&self, id: Id) -> Result<&ViewNode> {
        let app = self.app(id)?;
        if let Some(error) = &app.failure {
            return Err(error.clone().into());
        }
        app.scene.as_ref().ok_or_else(|| "The app has not produced its initial scene".into())
    }
    pub(crate) fn logs(&self, id: Id) -> Result<&[String]> {
        Ok(&self.app(id)?.logs)
    }
    pub(crate) fn log_batch(
        &self,
        id: Id,
        epoch: Id,
        after: Option<crate::engine::LogCursor>,
    ) -> Result<Option<crate::engine::LogBatch>> {
        let app = self.app(id)?;
        crate::engine::logs::batch(
            crate::engine::LogCursor {
                engine: epoch,
                instance: app.guest.generation(),
                next: app.log_next,
            },
            &app.logs,
            after,
        )
    }
    pub(crate) fn input(&mut self, id: Id, event: Event) -> Result<()> {
        validate_input(self.scene(id)?, &event)
            .map_err(|error| format!("Guest input rejected: {error:?}"))?;
        let app = self
            .running
            .iter_mut()
            .find(|app| app.id == id)
            .expect("input scene names this instance");
        if app.draining {
            return Err("The app is draining accepted work".into());
        }
        app.guest
            .queue_event(app.guest.generation(), event)
            .map_err(|error| format!("Guest input rejected: {error:?}").into())
    }
    pub(crate) fn deactivate(&mut self, id: Id) -> Result<()> {
        let app =
            self.running.iter_mut().find(|app| app.id == id).ok_or("The app instance is closed")?;
        app.draining = true;
        app.guest.deactivate();
        Ok(())
    }
    pub(crate) fn drain_pending(&self, id: Id) -> Result<bool> {
        let app = self.app(id)?;
        if let Some(error) = &app.failure {
            return Err(error.clone().into());
        }
        if app.guest.drain_failed()
            || (app.draining
                && app.guest.drain_pending()
                && !app.guest.has_ready_work(Instant::now())
                && !app.inbox.has_ready()
                && !app.inbox.waiting())
        {
            return Err(
                "The app could not drain its accepted work. Committed data is retained.".into()
            );
        }
        Ok(app.guest.drain_pending() || !app.inbox.idle())
    }
    pub(crate) fn close(&mut self, id: Id) -> Result<()> {
        let index =
            self.running.iter().position(|app| app.id == id).ok_or("The app instance is closed")?;
        let app = self.running.remove(index);
        let inbox = app.inbox.clone();
        drop(app);
        if !inbox.idle() {
            self.retired.push(inbox);
        }
        assert!(
            self.running.len() + self.retired.len() <= MAX_APPS,
            "retired work retains its app admission"
        );
        Ok(())
    }
    /// Returns whether a scene changed. Delivery backpressure never stops
    /// admitted storage completions or scoped reader refreshes.
    pub(crate) fn advance(&mut self, active: bool) -> bool {
        let changes =
            std::mem::take(&mut *self.notices.0.lock().expect("embedding changes lock poisoned"));
        for app in &mut self.running {
            if changes.gap {
                app.dirty = true;
            }
            for source in &changes.sources {
                if match source {
                    ReplicaSource::Account => true,
                    ReplicaSource::Volume(volume) => app.reads.contains(volume),
                    ReplicaSource::Entry(address) => app.reads.contains(&address.volume),
                } {
                    app.dirty = true;
                }
            }
        }
        let mut presented = false;
        for index in 0..self.running.len() {
            let (before, current) = self.running.split_at_mut(index);
            let (app, after) = current.split_first_mut().expect("index names an admitted app");
            if app.failure.is_some() {
                continue;
            }
            let result = (|| -> Result<()> {
                if let Some(volumes) = app
                    .binding
                    .refresh()
                    .map_err(|error| format!("App access changed: {error:?}"))?
                {
                    let volumes = app
                        .inbox
                        .prepare_files(&volumes)
                        .map_err(|error| format!("File context changed: {error:?}"))?;
                    let (reads, writes) = scopes(&volumes);
                    app.guest
                        .replace_volumes(volumes)
                        .map_err(|error| format!("App context changed: {error:?}"))?;
                    app.reads = reads;
                    app.writes = writes;
                    app.dirty = true;
                }
                if let Some(volumes) = app
                    .inbox
                    .retry_file_roots()
                    .map_err(|error| format!("File context changed: {error:?}"))?
                {
                    let (reads, writes) = scopes(&volumes);
                    app.dirty |= app
                        .guest
                        .replace_volumes(volumes)
                        .map_err(|error| format!("File context changed: {error:?}"))?;
                    app.reads = reads;
                    app.writes = writes;
                }
                // Initial/reconnect merges and explicit management/tool writes
                // also publish this bounded, scoped history. Observe it only on
                // actual owner turns; do not consume the AP publisher's feed or
                // invent a freshness wake. Consent refresh above owns the scope.
                let (revision, changed) =
                    app.store.delivery_scope_revision(app.observed, &app.reads);
                if revision.is_none() {
                    return Err("The app's data change source is unavailable".into());
                }
                app.observed = revision;
                if changed.is_some() {
                    app.dirty = true;
                }
                if app.dirty && active && !app.draining {
                    match app.guest.queue_event(
                        app.guest.generation(),
                        Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
                    ) {
                        Ok(()) => {
                            app.dirty = false;
                        }
                        Err(HostError::LimitExceeded) => {}
                        Err(error) => {
                            return Err(format!("Guest data notification failed: {error:?}").into());
                        }
                    }
                }
                for (generation, token, creation) in app.inbox.ready() {
                    let result = if creation {
                        app.guest.notify_creation(generation, token)
                    } else {
                        app.guest.notify_data(generation, token)
                    };

                    match result {
                        Ok(()) => {}
                        Err(HostError::LimitExceeded) => app.inbox.defer(token),
                        Err(error) => {
                            return Err(format!("Guest completion failed: {error:?}").into());
                        }
                    }
                }
                if (active || app.draining) && app.guest.has_ready_work(Instant::now()) {
                    let frame = app.guest.pump()?;
                    if let Some(bytes) = frame.view_bytes() {
                        app.encoded = bytes.to_vec();
                    }
                    let (logs, view) = frame.into_native_parts();
                    app.log_next = app
                        .log_next
                        .checked_add(logs.len() as u64)
                        .expect("app log sequence exhausted");
                    app.logs.extend(logs);
                    crate::engine::logs::retain_recent_logs(&mut app.logs);
                    if let Some(view) = view {
                        app.scene = Some(view);
                        presented = true;
                    }
                }
                Ok(())
            })();
            if app.changed.swap(false, Ordering::AcqRel) {
                for other in before.iter_mut().chain(after) {
                    if app.writes.iter().any(|volume| other.reads.contains(volume)) {
                        other.dirty = true;
                    }
                }
            }
            if let Err(error) = result {
                app.failure = Some(error.to_string());
                presented = true;
            }
        }
        presented
    }
    pub(crate) fn wake_after(&self, now: Instant) -> Option<Duration> {
        self.running
            .iter()
            .filter(|app| app.failure.is_none())
            .filter_map(|app| {
                if (app.dirty && !app.draining)
                    || app.inbox.has_ready()
                    || app.guest.has_ready_work(now)
                {
                    Some(Duration::ZERO)
                } else {
                    app.guest.wake_after(now)
                }
            })
            .min()
    }
    #[cfg(test)]
    pub(crate) fn settled(&self) -> bool {
        self.running.iter().all(|app| {
            app.failure.is_some()
                || ((!app.dirty || app.draining)
                    && app.inbox.idle()
                    && !app.guest.has_ready_work(Instant::now()))
        })
    }
    #[cfg(any(feature = "test-support", feature = "desktop-qualification"))]
    pub(crate) fn work_pending(&self, id: Id) -> Result<bool> {
        let app = self.app(id)?;
        if let Some(error) = &app.failure {
            return Err(error.clone().into());
        }
        Ok((app.dirty && !app.draining)
            || !app.inbox.idle()
            || app.guest.has_ready_work(Instant::now()))
    }
}
fn scopes(volumes: &[crate::runtime::host::AvailableVolume]) -> (Vec<Id>, Vec<Id>) {
    use crate::runtime::host::VolumeRights;
    (
        volumes
            .iter()
            .filter(|volume| volume.backing == playbit_api::abi::data::VOLUME_PLAYBIT)
            .map(|volume| Id::from_bytes(volume.id))
            .collect(),
        volumes
            .iter()
            .filter(|volume| volume.rights.0 & VolumeRights::WRITE.0 != 0)
            .map(|volume| Id::from_bytes(volume.id))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
