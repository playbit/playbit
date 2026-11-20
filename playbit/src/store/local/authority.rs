//! Negative authenticated evidence stays local to the receiving account.
//! Persistent generations prevent an old capture from clearing a newer fence.
use super::*;
use crate::store::{DatabaseId, EntryId};

use crate::store::authority::{
    Barrier, EntryBarrier, barrier_key, binding_key, capture_generation, catalog_key,
    ceiling_entry_key, entry_barrier_key, entry_key,
};

const MAX_SESSION_DENIAL_REASON_BYTES: usize = 256;
const MAX_SESSION_DENIAL_RECORD_BYTES: usize = 2 * 1024;
const _: () = assert!(MAX_SESSION_DENIAL_RECORD_BYTES >= 6 * MAX_SESSION_DENIAL_REASON_BYTES + 64);

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionDenial {
    generation: u64,
    reason: Option<String>,
}
impl SessionDenial {
    fn barrier(&self) -> Barrier {
        Barrier { generation: self.generation, fenced: self.reason.is_some() }
    }
}

impl LocalStore {
    /// Retain qualified negative evidence without changing replicated policy
    /// or discarding local edits. Every receipt invalidates older admission.
    pub fn record_session_denial(&self, binding: &Session, reason: &str) -> Result<bool> {
        let gate = self.session_operation(&binding.id)?;
        // Stricter local evidence must survive interrupted data publication.
        // Keep session admission serialized; never clear its backing fence.
        let _guard = gate.lock()?;
        let _profile = self.lock_profile()?;
        let current = self.session(&binding.id)?;
        if current != *binding {
            return Ok(false);
        }
        self.record_session_denial_locked(&current, reason)?;
        Ok(true)
    }

    fn record_session_denial_locked(&self, session: &Session, reason: &str) -> Result<()> {
        if reason.len() > MAX_SESSION_DENIAL_REASON_BYTES {
            return Err(Error::LimitExceeded);
        }
        let state = self.session_denial_state(&session.id)?;
        let state = SessionDenial {
            generation: state.barrier().invalidate().generation,
            reason: Some(reason.into()),
        };
        put(&self.metadata, format!("session-denial/{}", session.id).as_bytes(), &state)?;
        self.changes.observed_changed(None);
        Ok(())
    }

    fn session_denial_state(&self, session_id: &str) -> Result<SessionDenial> {
        validate_version(session_id)?;
        let key = format!("session-denial/{session_id}");
        let snapshot = self.metadata.begin_read()?;
        let Some(length) = snapshot.value_len(key.as_bytes())? else {
            return Ok(SessionDenial::default());
        };
        if length > MAX_SESSION_DENIAL_RECORD_BYTES {
            return Err(Error::corrupt("session denial record exceeds its size limit"));
        }
        let bytes = snapshot
            .get(key.as_bytes())?
            .ok_or(Error::corrupt("session denial record is missing"))?;
        let state: SessionDenial = decode(&bytes)?;
        if state
            .reason
            .as_ref()
            .is_some_and(|reason| reason.len() > MAX_SESSION_DENIAL_REASON_BYTES)
            || (state.generation == u64::MAX && state.reason.is_none())
        {
            return Err(Error::corrupt("session denial reason or generation is invalid"));
        }
        Ok(state)
    }

    pub fn session_denial(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self.session_denial_state(session_id)?.reason)
    }

    /// Capture before AP I/O. This establishes no permission or remote state.
    pub fn session_denial_generation(&self, binding: &Session) -> Result<u64> {
        let operations = self.session_dependency_operations(binding)?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        if self.session(&binding.id)? != *binding {
            return Err(Error::AccessDenied("session changed before admission"));
        }
        Ok(self.session_denial_state(&binding.id)?.generation)
    }

    /// A qualified successful reply clears only its unchanged binding and
    /// captured generation. Retaining the generation prevents a later ABA.
    pub fn clear_session_denial(&self, binding: &Session, generation: u64) -> Result<bool> {
        let operations = self.session_dependency_operations(binding)?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        if self.session(&binding.id)? != *binding {
            return Ok(false);
        }
        let mut state = self.session_denial_state(&binding.id)?;
        if !state.barrier().accepts(generation) {
            return Ok(false);
        }
        if state.reason.take().is_some() {
            put(&self.metadata, format!("session-denial/{}", binding.id).as_bytes(), &state)?;
            self.changes.observed_changed(None);
        }
        Ok(true)
    }

    /// Correlated AP evidence still requires the unchanged stored binding at
    /// publication. A delayed old-build denial never changes current authority.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn receive_ap_session_denial(
        &self,
        denial: &crate::network::ap::operation::ServiceDenial,
        binding: &Session,
    ) -> Result<bool> {
        let gate = self.session_operation(&binding.id)?;
        let _guard = gate.lock()?;
        let _profile = self.lock_profile()?;
        let current = self.session(&binding.id)?;
        if current != *binding
            || !denial.matches_session(
                self.account.id,
                &current.id,
                &current.resolved_build_id,
                [current.replica_volume()],
            )
        {
            return Ok(false);
        }
        self.record_session_denial_locked(&current, "session participant revoked")?;
        Ok(true)
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn receive_authority_terminal(
        &self,
        access: &Access,
        source: Id,
        changed: Id,
        cause: crate::network::ap::replication::TerminalCause,
    ) -> Result<bool> {
        use crate::network::ap::replication::TerminalCause;
        if cause == TerminalCause::EntryAuthorityChanged {
            return Err(Error::AccessDenied("entry terminal requires a selected entry scope"));
        }
        let session = access.session_id.as_ref().map(|id| self.session(id)).transpose()?;
        if changed != source
            && session.as_ref().is_none_or(|session| changed != session.replica_volume())
        {
            return Err(Error::AccessDenied(
                "terminal volume is outside subscription dependencies",
            ));
        }
        if access.account != self.account.id {
            return Err(Error::AccessDenied("terminal receiving account"));
        }
        if cause == TerminalCause::ServiceUnavailable {
            return Ok(false);
        }
        if cause == TerminalCause::ParticipantDenied
            && let Some(session) = session
        {
            if access.build_id().is_some_and(|build| build != session.resolved_build_id) {
                return Ok(false);
            }
            return self.record_session_denial(&session, "session participant revoked");
        }
        self.invalidate_authority(access, changed)?;
        Ok(true)
    }
    #[cfg(feature = "connected-engine")]
    pub(crate) fn receive_entry_authority_terminal(
        &self,
        access: &Access,
        source: crate::store::volume_index::EntryAddress,
        changed: Id,
        cause: crate::network::ap::replication::TerminalCause,
    ) -> Result<bool> {
        if cause != crate::network::ap::replication::TerminalCause::EntryAuthorityChanged {
            return self.receive_authority_terminal(access, source.volume, changed, cause);
        }
        if changed != source.volume {
            return Err(Error::AccessDenied("entry terminal changed a dependency"));
        }
        self.invalidate_entry_authority(access, source)?;
        Ok(true)
    }

    pub(crate) fn entry_authority_generations(
        &self,
        volume: Id,
        entry: EntryId,
    ) -> Result<(u64, u64)> {
        let database = self.db(volume, DatabaseId::Index)?;
        let tx = database.begin_read()?;
        let volume = tx.local_get(&barrier_key(self.account.id))?;
        let generation = Barrier::decode(volume.as_deref())?.generation;
        let marker = tx.local_get(&entry_barrier_key(self.account.id, entry))?;
        Ok((generation, EntryBarrier::decode(marker.as_deref())?.current(generation)?.generation))
    }

    #[cfg(any(feature = "connected-engine", test))]
    fn invalidate_entry_authority(
        &self,
        access: &Access,
        address: crate::store::volume_index::EntryAddress,
    ) -> Result<()> {
        if access.account != self.account.id {
            return Err(Error::AccessDenied("entry fence receiving account"));
        }
        let operations = self.operations(&[address.volume])?;
        let gate = self.volume_operation(address.volume)?;
        let _guards = operations.lock_for_publication(&gate)?;
        let database = self.db(address.volume, DatabaseId::Index)?;
        let mut tx = database.begin_write(now_unix_secs())?;
        let key = entry_barrier_key(access.account, address.entry);
        let next = {
            let local = tx.local_indexes();
            let volume = local.get(&barrier_key(access.account))?;
            let generation = Barrier::decode(volume.as_deref())?.generation;
            let marker = local.get(&key)?;
            EntryBarrier {
                volume_generation: generation,
                barrier: EntryBarrier::decode(marker.as_deref())?.current(generation)?.invalidate(),
            }
        };
        tx.local_indexes().put(&key, &next.encode())?;
        tx.commit()?;
        self.changes.observed_changed(Some(address.volume));
        Ok(())
    }
    #[cfg(feature = "connected-engine")]
    pub(crate) fn authority_refresh_required(&self, volume: Id) -> Result<bool> {
        let database = self.db(volume, DatabaseId::Index)?;
        let tx = database.begin_read()?;
        let marker = tx.local_get(&barrier_key(self.account.id))?;
        Ok(Barrier::decode(marker.as_deref())?.fenced)
    }

    /// Trusted authenticated receiver only; generation is captured before I/O.
    pub(crate) fn authority_generation(&self, volume: Id) -> Result<u64> {
        let database = self.db(volume, DatabaseId::Index)?;
        let tx = database.begin_read()?;
        let marker = tx.local_get(&barrier_key(self.account.id))?;
        Ok(Barrier::decode(marker.as_deref())?.generation)
    }

    #[cfg(any(feature = "connected-engine", test))]
    pub(crate) fn invalidate_authority(&self, access: &Access, volume: Id) -> Result<()> {
        if access.account != self.account.id {
            return Err(Error::AccessDenied("authority fence receiving account"));
        }
        let operations = self.operations(&[volume])?;
        let gate = self.volume_operation(volume)?;
        let _guards = operations.lock_for_publication(&gate)?;
        let database = self.db(volume, DatabaseId::Index)?;
        let mut tx = database.begin_write(now_unix_secs())?;
        let key = barrier_key(access.account);
        let next = {
            let local = tx.local_indexes();
            let marker = local.get(&key)?;
            Barrier::decode(marker.as_deref())?.invalidate()
        };
        tx.local_indexes().put(&key, &next.encode())?;
        tx.commit()?;
        self.changes.observed_changed(Some(volume));
        Ok(())
    }

    /// Caller holds the publication dependency gates. No capture grants access;
    /// it only removes negative evidence for its exact generation and scope.
    pub(super) fn clear_authority_locked(
        &self,
        volume: Id,
        generation: u64,
        entry: Option<(EntryId, u64)>,
    ) -> Result<()> {
        let database = self.db(volume, DatabaseId::Index)?;
        let mut tx = database.begin_write(now_unix_secs())?;
        let key = barrier_key(self.account.id);
        let mut barrier = {
            let local = tx.local_indexes();
            let marker = local.get(&key)?;
            Barrier::decode(marker.as_deref())?
        };
        if !barrier.accepts(generation) {
            return Ok(());
        }
        if let Some((entry, captured)) = entry {
            let key = entry_barrier_key(self.account.id, entry);
            let mut selected = {
                let local = tx.local_indexes();
                let marker = local.get(&key)?;
                EntryBarrier::decode(marker.as_deref())?.current(generation)?
            };
            if !selected.accepts(captured) {
                return Ok(());
            }
            selected.fenced = false;
            tx.local_indexes().put(
                &key,
                &EntryBarrier { volume_generation: generation, barrier: selected }.encode(),
            )?;
            tx.local_indexes().put(&catalog_key(self.account.id), &[1])?;
            tx.local_indexes().put(&ceiling_entry_key(self.account.id, entry), &[1])?;
            tx.local_indexes()
                .put(&entry_key(self.account.id, entry), &generation.to_le_bytes())?;
            tx.local_indexes().put(&binding_key(self.account.id), &generation.to_le_bytes())?;
        } else {
            barrier = barrier.cleared_full();
            tx.local_indexes().put(&key, &barrier.encode())?;
            if !barrier.fenced {
                tx.local_indexes().del(&catalog_key(self.account.id))?;
                tx.local_indexes().del(&binding_key(self.account.id))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(super) fn restrict_catalog_locked(&self, volume: Id, entry: EntryId) -> Result<()> {
        let database = self.db(volume, DatabaseId::Index)?;
        let mut tx = database.begin_write(now_unix_secs())?;
        tx.local_indexes().put(&catalog_key(self.account.id), &[1])?;
        tx.local_indexes().put(&ceiling_entry_key(self.account.id, entry), &[1])?;
        tx.commit()?;
        Ok(())
    }

    fn check_authority_marker(
        &self,
        account: Id,
        volume: Id,
        entry: Option<EntryId>,
    ) -> Result<()> {
        let database = self.db(volume, DatabaseId::Index)?;
        let tx = database.begin_read()?;
        let marker = tx.local_get(&barrier_key(account))?;
        let barrier = Barrier::decode(marker.as_deref())?;
        if let Some(entry) = entry {
            let selected = tx.local_get(&entry_barrier_key(account, entry))?;
            if EntryBarrier::decode(selected.as_deref())?.current(barrier.generation)?.fenced {
                return Err(Error::AccessDenied("entry authority requires authenticated catch-up"));
            }
        }
        let ceiling = tx.local_get(&catalog_key(account))?;
        if ceiling.as_deref().is_some_and(|bytes| bytes != [1]) {
            return Err(Error::corrupt("captured catalog authority marker must be one"));
        }
        let partial = ceiling.is_some();
        if partial {
            let entry = entry.ok_or(Error::AccessDenied("entry capture grants no catalog"))?;
            let allowed = tx.local_get(&ceiling_entry_key(account, entry))?;
            match allowed.as_deref() {
                Some([1]) => {}
                None => return Err(Error::AccessDenied("entry is outside captured scope")),
                _ => return Err(Error::corrupt("captured entry authority marker must be one")),
            }
        }
        let capture = if barrier.fenced
            && let Some(entry) = entry
        {
            let bytes = tx.local_get(&entry_key(account, entry))?;
            capture_generation(bytes.as_deref())?
        } else {
            None
        };
        if !barrier.permits(capture) {
            return Err(Error::AccessDenied("current authority requires authenticated catch-up"));
        }
        Ok(())
    }

    pub(super) fn check_dependency_authority(&self, account: Id, volume: Id) -> Result<()> {
        let database = self.db(volume, DatabaseId::Index)?;
        let tx = database.begin_read()?;
        let marker = tx.local_get(&barrier_key(account))?;
        let bytes = tx.local_get(&binding_key(account))?;
        let capture = capture_generation(bytes.as_deref())?;
        if !Barrier::decode(marker.as_deref())?.permits(capture) {
            return Err(Error::AccessDenied(
                "dependency authority requires authenticated catch-up",
            ));
        }
        Ok(())
    }

    /// Session control admission grants no catalog or entry data. A current
    /// authenticated selected capture qualifies its volume's control metadata.
    pub(crate) fn admit_binding(&self, access: &Access, volume: Id) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        if let Some(id) = &access.session_id
            && self.has_automatic_scopes(id)?
        {
            self.require_account_authority()?;
            let session = self.session(id)?;
            if access.account != self.account.id
                || (session.shared_volume.is_none() && session.creator != self.account.id)
                || access.build_id.as_deref() != Some(session.resolved_build_id.as_str())
                || self.session_denial(id)?.is_some()
            {
                return Err(Error::AccessDenied("local session admission changed"));
            }
            let default = session
                .app
                .volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)
                .map_err(Error::InvalidInput)?;
            if volume == default && session.root_volume == default {
                if session.shared_volume.is_some() && self.shared_session_initialized(id)? {
                    self.check_participant(session.replica_volume(), access.account)?;
                    self.check_dependency_authority(access.account, session.replica_volume())?;
                }
                return Ok(());
            }
        }
        self.admit_access(access, &self.index(volume)?)?;
        self.check_dependency_authority(access.account, volume)?;
        if let Some(id) = &access.session_id {
            let session = self.session(id)?;
            if session.replica_volume() != volume
                && ((session.shared_volume.is_some()
                    && self.shared_session_initialized(&session.id)?)
                    || !self.has_implicit_local_root(&session)?)
            {
                self.check_dependency_authority(access.account, session.replica_volume())?;
            }
        }
        Ok(())
    }

    pub(super) fn check_authority(
        &self,
        access: &Access,
        volume: Id,
        entry: Option<EntryId>,
    ) -> Result<()> {
        self.check_authority_marker(access.account, volume, entry)?;
        if let Some(id) = &access.session_id {
            let session = self.session(id)?;
            if session.replica_volume() != volume
                && ((session.shared_volume.is_some()
                    && self.shared_session_initialized(&session.id)?)
                    || !self.has_implicit_local_root(&session)?)
            {
                self.check_dependency_authority(access.account, session.replica_volume())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "authority/tests.rs"]
mod tests;
