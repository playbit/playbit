//! A received publisher World holds an AP-authorized database projection.
//! Its public Merkle context is not a volume credential or a blob encryption key.
use super::*;
use crate::store::DatabaseId;

#[cfg(feature = "connected-engine")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorldReplicaRoute {
    Ordinary,
    PublisherWorld,
    CredentialedRoot,
}

const KV_APPEND: &[u8] = b"world-kv-append/";

pub(super) fn kv_append_key(clock: kvdb::Clock) -> Vec<u8> {
    [KV_APPEND, &clock.encode()].concat()
}

pub(super) fn kv_append_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    [kvdb::hash::entry_hash(&[0; 32], key, 0, value).as_slice(), key].concat()
}

#[cfg(feature = "connected-engine")]
fn kv_append_parts(record: &[u8]) -> Result<(&[u8], &[u8])> {
    let (digest, key) =
        record.split_at_checked(32).ok_or(Error::corrupt("invalid World KV append digest"))?;
    crate::store::key_value::validate_key(key)?;
    Ok((digest, key))
}

#[cfg(feature = "connected-engine")]
fn clear_world_append_markers(
    source: &Database,
    clocks: impl ExactSizeIterator<Item = kvdb::Clock>,
) -> Result<()> {
    if clocks.len() != 0 {
        let mut tx = source.begin_write(now_unix_secs())?;
        for clock in clocks {
            tx.local_indexes().del(&kv_append_key(clock))?;
        }
        tx.commit()?;
    }
    Ok(())
}

impl LocalStore {
    #[cfg(feature = "connected-engine")]
    pub(crate) fn world_replica_route(
        &self,
        volume: Id,
        session: Option<&Session>,
    ) -> Result<WorldReplicaRoute> {
        if !self.uses_world_projection(volume)? {
            return Ok(WorldReplicaRoute::Ordinary);
        }
        let session = session.ok_or(Error::AccessDenied("World projection requires a session"))?;
        if session.is_distinct_world(volume) {
            session.validate_publisher_world(volume)?;
            return Ok(WorldReplicaRoute::PublisherWorld);
        }
        // A root-backed invite independently delivers the true credentials.
        // Its released transport also works on APs without World projections.
        if session.shared_volume.is_none()
            && session.root_volume == volume
            && self.session(&session.id)?.same_identity(session)
        {
            self.credentials(volume)?;
            return Ok(WorldReplicaRoute::CredentialedRoot);
        }
        Err(Error::AccessDenied("publisher World binding"))
    }

    /// A blind receipt contains no readable AP root. Remember only the exact
    /// local snapshot submitted, independently of full replica durability.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn publish_pending_world_backing(
        &self,
        volume: Id,
        database: DatabaseId,
        publish: impl FnOnce(&Database) -> Result<()>,
    ) -> Result<()> {
        assert!(matches!(database, DatabaseId::Entry(_)));
        let key = format!("world-submitted/{volume}/{database}");
        self.with_outbound_replica(
            volume,
            database,
            crate::store::ReplicaReader::Service,
            |source| {
                let Some(root) = replica_root(source)? else { return Ok(()) };
                if self
                    .durable_revision(volume, database)?
                    .is_some_and(|ack| ack.root == Some(root))
                    || get(&self.metadata, key.as_bytes())? == Some(root)
                {
                    return Ok(());
                }
                publish(source)?;
                let _profile = self.lock_profile()?;
                profile_format::enable_world_projection(self)?;
                put(&self.metadata, key.as_bytes(), &root)
            },
        )
    }

    /// A formerly readable backing can contain stale values written by other
    /// players. Only locally authored KV births can be offered under blind Append.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn publish_pending_world_appends(
        &self,
        volume: Id,
        database: DatabaseId,
        mut publish: impl FnMut(&Database) -> Result<()>,
    ) -> Result<()> {
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        policy.authorize_reader(ReplicaReader::Service)?;
        let source = self.db(volume, database)?;
        // Freeze the upper bound; local appends during I/O belong to later work.
        let through = source.clock();
        let mut start = KV_APPEND.to_vec();
        let mut retry_singly_through = None;
        loop {
            let snapshot = source.begin_read()?;
            // AP admits a batch atomically. Retry only a conflicting batch one birth at a time.
            let limit = if retry_singly_through.is_some() { 1 } else { 16 };
            let mut markers = Vec::with_capacity(limit);
            for row in snapshot.local_iter_from(&start)?.take(limit) {
                let (marker, key) = row?;
                let Some(clock) = marker.strip_prefix(KV_APPEND) else { break };
                let clock = kvdb::Clock::decode(clock)
                    .ok_or(Error::corrupt("invalid World KV append clock"))?;
                if clock > through {
                    break;
                }
                markers.push((clock, key.to_vec()));
            }
            let Some(&(last, _)) = markers.last() else { return Ok(()) };
            let submission =
                empty_detached_replica(&self.path.join(crate::paths::STAGING), &source)?;
            let mut tx = submission.database().begin_write(now_unix_secs())?;
            let mut changed = false;
            for (clock, record) in &markers {
                let (digest, key) = kv_append_parts(record)?;
                if let Some(value) = snapshot.value_reader(key)?
                    && value.clock() == Some(*clock)
                    && kvdb::hash::entry_hash_from(&[0; 32], key, 0, &value)? == digest
                {
                    tx.put_from_with_clock(key, &value, *clock)?;
                    changed = true;
                }
            }
            tx.commit()?;
            drop(snapshot);
            let result = if changed { publish(submission.database()) } else { Ok(()) };
            match result {
                Ok(()) => {
                    clear_world_append_markers(&source, markers.iter().map(|(clock, _)| *clock))?;
                }
                Err(Error::AlreadyExists("World KV append")) => {
                    if markers.len() > 1 {
                        retry_singly_through = Some(last);
                        continue;
                    }
                    // Keep this one refused birth and its marker for reconciliation.
                }
                Err(error) => return Err(error),
            }
            if retry_singly_through.is_some_and(|end| last >= end) {
                retry_singly_through = None;
            }
            // The iterator seeks inclusively; every marker ends at its fixed-width clock.
            start = kv_append_key(last);
            start.push(0);
        }
    }

    /// A successful full upload or authenticated pull also settles local births
    /// present in that exact snapshot. Never use the potentially newer local root.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn acknowledge_world_append_snapshot(
        &self,
        volume: Id,
        database: DatabaseId,
        acknowledged: &Database,
        receipt_root: crate::store::ReplicaRoot,
    ) -> Result<()> {
        let accepted = acknowledged.begin_read()?;
        if accepted.root_hash()? != receipt_root {
            // A merged upload or a locally supplemented stage lacks exact AP evidence.
            return Ok(());
        }
        let source = self.db(volume, database)?;
        let local = source.begin_read()?;
        let mut matched = Vec::with_capacity(64);
        for row in local.local_iter_from(KV_APPEND)? {
            let (marker, record) = row?;
            let Some(suffix) = marker.strip_prefix(KV_APPEND) else { break };
            let (digest, key) = kv_append_parts(&record)?;
            let clock = kvdb::Clock::decode(suffix)
                .ok_or(Error::corrupt("invalid World KV append clock"))?;
            if let Some(value) = accepted.value_reader(key)?
                && value.clock() == Some(clock)
                && kvdb::hash::entry_hash_from(&[0; 32], key, 0, &value)? == digest
            {
                matched.push(clock);
            }
            if matched.len() == 64 {
                clear_world_append_markers(&source, matched.iter().copied())?;
                matched.clear();
            }
        }
        clear_world_append_markers(&source, matched.iter().copied())
    }

    /// Normal live publication can settle the same birth before reconnect.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn acknowledge_world_kv_appends(
        &self,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<bool> {
        let db = self.db(volume, database)?;
        let snapshot = db.begin_read()?;
        if !snapshot.next_local_key(KV_APPEND, false)?.is_some_and(|key| key.starts_with(KV_APPEND))
        {
            return Ok(false);
        }
        let mut records = Vec::new();
        kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
            if let kvdb::sync::EntState::Present { clock, value } = state {
                records.push((kv_append_key(clock), key, value));
            }
        })?;
        let mut tx = db.begin_write(now_unix_secs())?;
        let mut changed = false;
        for (marker, key, value) in records {
            if snapshot
                .local_get(&marker)?
                .is_some_and(|stored| stored.as_ref() == kv_append_record(key, value))
            {
                changed |= tx.local_indexes().del(&marker)?;
            }
        }
        if changed {
            tx.commit()?;
        }
        Ok(changed)
    }

    /// Authenticated World attachment can reuse a database with independent Root
    /// credentials. Both local hash contexts may contain World append evidence.
    pub(crate) fn has_world_attachment(&self, volume: Id) -> Result<bool> {
        let tx = self.metadata.begin_read()?;
        Ok(tx.has(format!("world-projection/{volume}").as_bytes())?
            || tx.has(format!("world-credentialed/{volume}").as_bytes())?)
    }

    /// Only an authenticated attached World and its independently credentialed
    /// Root can share this backing's append evidence. Personal selections and
    /// unrelated session scopes continue to use their own replication path.
    pub(crate) fn world_append_scope(&self, session: &Session, volume: Id) -> Result<bool> {
        if !self.has_world_attachment(volume)? {
            return Ok(false);
        }
        if session.is_distinct_world(volume) {
            session.validate_publisher_world(volume)?;
            return Ok(true);
        }
        if session.shared_volume.is_none() && session.root_volume == volume {
            self.credentials(volume)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn uses_world_projection(&self, volume: Id) -> Result<bool> {
        Ok(self.metadata.begin_read()?.has(format!("world-projection/{volume}").as_bytes())?)
    }

    pub(super) fn local_database_key(&self, volume: Id) -> Result<[u8; 32]> {
        let tx = self.metadata.begin_read()?;
        if tx.has(format!("world-projection/{volume}").as_bytes())? {
            return Ok(DatabaseId::world_projection_key(volume));
        }
        let bytes = tx
            .get(format!("credentials/{volume}").as_bytes())?
            .ok_or(Error::NotFound("local record"))?;
        Ok(decode::<Credentials>(&bytes)?.database_key)
    }

    // An authenticated refusal can apply to just one session/build even when
    // this profile has a healthy World subscription through another session.
    pub(super) fn check_world_admission(&self, session: &Session) -> Result<()> {
        if self.world_denied(session)? {
            return Err(Error::AccessDenied("session World admission"));
        }
        Ok(())
    }

    pub(crate) fn world_denied(&self, session: &Session) -> Result<bool> {
        let key = format!("world-denied/{}/{}", session.id, session.resolved_build_id);
        Ok(self.metadata.begin_read()?.has(key.as_bytes())?)
    }

    #[cfg(any(feature = "connected-engine", test))]
    pub(crate) fn set_world_denied(&self, session: &Session, denied: bool) -> Result<()> {
        let operations = self.operations(&[session.root_volume])?;
        let _guards = operations.lock()?;
        let _profile = self.lock_profile()?;
        let key = format!("world-denied/{}/{}", session.id, session.resolved_build_id);
        if self.metadata.begin_read()?.has(key.as_bytes())? == denied {
            return Ok(());
        }
        profile_format::enable_world_projection(self)?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if denied {
            tx.put(key.as_bytes(), &[1])?;
        } else {
            tx.del(key.as_bytes())?;
        }
        tx.commit()?;
        self.changes.observed_changed(Some(session.root_volume));
        Ok(())
    }

    /// A release before World attachment could initialize the publisher World
    /// after creating a Shared session without provisioning its original build.
    #[cfg(any(feature = "connected-engine", test))]
    pub(crate) fn provision_local_session_world(&self, session: &Session) -> Result<()> {
        if session.app.publisher == self.account.id
            && session.creator == self.account.id
            && session.is_distinct_world(session.root_volume)
            && self.is_local_session(&session.id)?
            && self.automatic_binding(session.root_volume)?.is_some_and(|binding| {
                binding.role == crate::package::AutomaticVolumeRole::Root
                    && binding.app == session.app
            })
            // An initialized room can open while its recovered World has no
            // policy yet. A later sync provisions the build once the index arrives.
            && (self.has_volume_index(session.root_volume)?
                || !self.shared_session_initialized(&session.id)?)
        {
            self.provision_version(
                &Access::owner(self.account.id),
                session.root_volume,
                &session.resolved_build_id,
            )?;
        }
        Ok(())
    }

    /// The caller received the descriptor from authenticated AP Open. The
    /// subsequent qualified pull must still admit the complete policy before use.
    #[cfg(any(feature = "connected-engine", test))]
    pub(crate) fn prepare_world_replica(&self, session: &Session, world: &Volume) -> Result<()> {
        session.validate_publisher_world(world.id)?;
        if world.owner != session.app.publisher
            || world.owner == self.account.id
            || world.personal
            || !world.share_key_with_service
            || !self.session(&session.id)?.same_identity(session)
        {
            return Err(Error::AccessDenied("publisher World replica identity"));
        }
        let operations = self.operations(&[world.id])?;
        let _guards = operations.lock()?;
        self.require_volume_available(world.id)?;
        {
            let _profile = self.lock_profile()?;
            if let Some(current) =
                get::<Volume>(&self.metadata, format!("volume/{}", world.id).as_bytes())?
            {
                if !current.same_identity(world) {
                    return Err(Error::AccessDenied("publisher World replica differs"));
                }
                if !self.uses_world_projection(world.id)? {
                    let key = format!("world-credentialed/{}", world.id);
                    if !self.metadata.begin_read()?.has(key.as_bytes())? {
                        self.credentials(world.id)?;
                        profile_format::enable_world_projection(self)?;
                        let mut tx = self.metadata.begin_write(now_unix_secs())?;
                        tx.put(key.as_bytes(), b"")?;
                        tx.commit()?;
                    }
                    return Ok(());
                }
            } else {
                profile_format::enable_world_projection(self)?;
                let mut tx = self.metadata.begin_write(now_unix_secs())?;
                tx.put(format!("world-projection/{}", world.id).as_bytes(), b"")?;
                tx.put(format!("volume/{}", world.id).as_bytes(), &encode(world)?)?;
                tx.commit()?;
            }
        }
        // The durable marker selects the same tree identity after a crash or a
        // later credentialed invitation. Neither event may replace offline edits.
        let dir = self.path.join(crate::paths::VOLUMES).join(world.id.to_string());
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(io)?;
        for database in [DatabaseId::Index, DatabaseId::Revocations] {
            self.replica_db(world.id, database, Some(DatabaseId::world_projection_key(world.id)))?;
        }
        Ok(())
    }
}
