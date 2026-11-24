//! Attach the existing publisher World after the invited room is admitted.
use super::*;

enum WorldAdmission {
    Available(crate::store::Volume),
    Unavailable,
    Initializing(Box<dyn std::error::Error + Send + Sync>),
    Revoked,
}

impl Remote {
    pub(super) fn sync_invited_world(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
    ) -> Result<()> {
        self.with_invited_world(store, session, |remote| {
            remote.sync_session_volume(store, session, session.root_volume, false)
        })
    }

    /// A World grant may change between Open and a transfer. Only a new
    /// authenticated availability response may withdraw cached authority.
    pub(super) fn with_invited_world(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        capture: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        let controls = initialization::Source::WorldControls(session.replica_volume());
        let qualify = |remote: &mut Self| {
            remote
                .qualify_invited_world(store, session)
                .map_err(|error| sync_failure::volume(store, session.root_volume, error))
        };
        let result = (|| {
            if !qualify(self)? {
                return Ok(());
            }
            let result = capture(self);
            if let Err(error) = &result
                && !canceled(error.as_ref())
                && self.connection.as_ref().is_some_and(|connection| !connection.is_closed())
                && !qualify(self)?
            {
                return Ok(());
            }
            result
        })();
        // Once this room has a live World, pending named databases retain their
        // own volume retry. A partial control capture never completes this wait.
        if result.as_ref().is_err_and(|error| error.is::<initialization::Pending>())
            && self.initializations.tracked(session.root_volume)
            && !store.world_denied(session)?
            && self.has_live_volume(store, session.root_volume)?
        {
            self.initializations.forget(controls);
            return result;
        }
        self.finish_world_controls(controls, result)
    }

    fn finish_world_controls<T>(
        &mut self,
        controls: initialization::Source,
        result: Result<T>,
    ) -> Result<T> {
        let error = match result {
            Ok(value) => {
                self.initializations.forget(controls);
                return Ok(value);
            }
            Err(error) => error,
        };
        let now = Instant::now();
        if canceled(error.as_ref())
            || self.connection.as_ref().is_none_or(|connection| connection.is_closed())
        {
            // Reconnect owns the transport. Preserve the room's current attempt
            // until an authenticated connection can finish its control capture.
            self.initializations.interrupted(controls, now);
            return Err(error);
        }
        self.initializations.failed(
            controls,
            &sync_failure::reason(error.as_ref()).to_string(),
            now,
        )?;
        Err(if self.initializations.scheduled(controls) && !error.is::<initialization::Pending>() {
            Box::new(initialization::Pending(error))
        } else {
            error
        })
    }

    fn qualify_invited_world(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
    ) -> Result<bool> {
        let volume = session.root_volume;
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        let supported = self
            .runtime
            .block_on(connection.describe())?
            .features
            .iter()
            .any(|feature| feature == crate::network::ap::service::WORLD_REPLICA);
        let admission = if supported {
            self.world_admission(store, session)?
        } else if store.uses_world_projection(volume)? {
            WorldAdmission::Unavailable
        } else {
            return Ok(false);
        };
        if let WorldAdmission::Available(world) = admission {
            store.prepare_world_replica(session, &world)?;
            if store.world_denied(session)? {
                // Open qualifies the catalog, but cached entries can have newer
                // ACLs. Keep every app handle fenced throughout the full capture.
                // Uninitialized named payloads remain individually unavailable.
                // The qualified control capture has rechecked every entry ACL.
                self.pull_bound_volume(store, session, volume)?;
                store.set_world_denied(session, false)?;
            }
            return Ok(true);
        }

        // Shared remains usable when the publisher has not hosted this World,
        // or this account/build lacks catalog access to it.
        let initializing = matches!(admission, WorldAdmission::Initializing(_));
        if !initializing {
            match store.volume(volume) {
                Ok(_) => (),
                Err(crate::store::Error::NotFound(_)) => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
        store.set_world_denied(session, true)?;
        let live = self.live.as_mut().ok_or("live replication is unavailable")?;
        self.runtime.block_on(live.retire_session_world(volume, session))?;
        // An unavailable build says nothing about independently admitted sessions
        // or known entries. Only a bound participant revocation fences all routes.
        if matches!(admission, WorldAdmission::Revoked) {
            store.invalidate_authority(&Access::owner(store.account().id), volume)?;
        } else if !initializing && store.authority_refresh_required(volume)? {
            // A full-stream policy terminal may already have fenced the whole
            // replica. Requalify surviving selected entries with their own grants.
            let selected = live.active_selected(volume);
            self.requalify_world_entries(store, volume, selected)?;
        }
        if let WorldAdmission::Initializing(wait) = admission {
            return Err(wait);
        }
        Ok(false)
    }

    pub(super) fn retry_world_controls(&mut self, store: &Arc<LocalStore>) -> Result<bool> {
        let mut due = self.initializations.due_world_controls(Instant::now());
        if due.is_empty() {
            return Ok(false);
        }
        let sessions = store.sessions().inspect_err(|_| {
            for shared in &due {
                self.initializations.forget(initialization::Source::WorldControls(*shared));
            }
        })?;
        let mut failure = None;
        for session in sessions {
            let Some(shared) = session.shared_volume else { continue };
            let Some(index) = due.iter().position(|id| *id == shared) else { continue };
            due.swap_remove(index);
            let controls = initialization::Source::WorldControls(shared);
            self.initializations.start(controls);
            // Session admission is connection-bound. An earlier join may have
            // observed the wait on a transport that has since been replaced.
            let admitted = self.admit(store, &session);
            let result = match admitted {
                Ok(()) => self.sync_optional_world(store, &session),
                Err(error) => self.finish_world_controls(
                    controls,
                    Err(sync_failure::volume(store, session.replica_volume(), error)),
                ),
            };
            if let Err(error) = result {
                if self.connection.as_ref().is_none_or(|connection| connection.is_closed()) {
                    return Err(error);
                }
                initialization::retain_error(&mut failure, error);
            } else {
                self.initializations.forget(controls);
            }
        }
        for shared in due {
            self.initializations.forget(initialization::Source::WorldControls(shared));
        }
        failure.map_or(Ok(true), Err)
    }

    fn world_admission(&mut self, store: &LocalStore, session: &Session) -> Result<WorldAdmission> {
        let volume = session.root_volume;
        // Describe, Open and the following capture must share the same
        // connection-bound admission. Reconnect owns transport replacement.
        let result = self.app_request(&ServiceRequest::WorldReplica {
            operation_id: Id::generate()?.to_string(),
            session_id: session.id.clone(),
            build_id: session.resolved_build_id.clone(),
            volume_id: volume.to_string(),
            request: crate::network::ap::operation::WorldReplicaRequest::Open,
        });
        match result {
            Ok(response) => {
                let data = response.data.ok_or("missing World availability response")?;
                let descriptor = data.get("volume").ok_or("invalid World availability response")?;
                let world: Option<crate::store::Volume> =
                    serde_json::from_value(descriptor.clone())?;
                Ok(world.map_or(WorldAdmission::Unavailable, WorldAdmission::Available))
            }
            Err(error) => {
                if error.downcast_ref::<ServiceFailure>().is_some_and(|failure| {
                    failure.denial().is_none()
                        && failure.to_string() == "waiting for initial database synchronization"
                }) {
                    return Ok(WorldAdmission::Initializing(error));
                }
                let Some(denial) =
                    error.downcast_ref::<ServiceFailure>().and_then(ServiceFailure::denial)
                else {
                    return Err(error);
                };
                if denial.matches_session(
                    store.account().id,
                    &session.id,
                    &session.resolved_build_id,
                    [volume],
                ) {
                    return Ok(WorldAdmission::Revoked);
                }
                store.receive_ap_session_denial(denial, session)?;
                Err(error)
            }
        }
    }

    fn requalify_world_entries(
        &mut self,
        store: &Arc<LocalStore>,
        volume: Id,
        selected: Vec<crate::engine::live::ScopeKey>,
    ) -> Result<()> {
        let mut first_error = None;
        for scope in selected {
            let result = (|| -> Result<()> {
                let entry = scope.entry.expect("selected capture has an entry");
                if !store.entry_capture_required(volume, entry)? {
                    return Ok(());
                }
                let session = scope
                    .session
                    .as_ref()
                    .map(|(id, version)| {
                        let mut session = store.session(&crate::identifier::encode(id))?;
                        session.resolved_build_id = crate::identifier::encode(version);
                        Ok::<_, crate::store::Error>(session)
                    })
                    .transpose()?;
                if let Some(session) = &session {
                    self.admit(store, session)?;
                }
                self.synchronize_entry(
                    store,
                    crate::store::volume_index::EntryAddress { volume, entry },
                    session.as_ref(),
                )
            })();
            if let Err(error) = result {
                first_error.get_or_insert_with(|| sync_failure::recovery(store, &scope, error));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// A legacy root-backed invitation can select an already projected World.
/// Hash the pinned logical records in that invitation's credentialed context.
pub(in crate::engine) async fn wire_root(
    sender: &SyncSender<'_, kvdb::redb::RedbStorage>,
    key: [u8; 32],
) -> Result<crate::store::ReplicaRoot> {
    let mut scan = sender.projection_scan(key)?;
    while !scan.advance()? {
        tokio::task::yield_now().await;
    }
    Ok(scan.finish()?)
}
