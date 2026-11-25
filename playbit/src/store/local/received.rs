//! Detached, bounded-cache bootstrap receipt for every named database.
use super::initialization::sync_directory;
use super::*;
use crate::store::{DatabaseId, EntryId, object_database, volume_index};
mod namespace;
mod publication;
pub(super) mod rejected;
use publication::{PendingPublication, ReceivedDirectory};
use std::sync::atomic::{AtomicUsize, Ordering};
const STAGED_CACHE_BYTES: usize = 512 * 1024;
const MAX_PREPARATIONS: usize = 2;

#[derive(Default)]
pub(super) struct ReceivePreparations {
    active: Arc<AtomicUsize>,
    #[cfg(test)]
    #[allow(
        clippy::type_complexity,
        reason = "The hook interrupts a durable publication boundary."
    )]
    pub(super) after_checkpoint:
        Mutex<Option<Arc<dyn Fn(PublicationCheckpoint) -> Result<()> + Send + Sync>>>,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationCheckpoint {
    Intent,
    Backing(DatabaseId),
    Index,
    Readiness,
}
#[cfg(test)]
thread_local! {
    static CRASH_AT: std::cell::Cell<Option<PublicationCheckpoint>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
pub(super) fn crash_at(checkpoint: PublicationCheckpoint) {
    CRASH_AT.set(Some(checkpoint));
}
fn next_preparation(active: usize) -> Option<usize> {
    if active < MAX_PREPARATIONS { Some(active + 1) } else { None }
}
struct ReceiveCredit(Arc<AtomicUsize>);
impl ReceiveCredit {
    fn enter(preparations: &ReceivePreparations) -> Result<Self> {
        preparations
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, next_preparation)
            .map_err(|_| Error::LocalContention("received volume preparation limit"))?;
        Ok(Self(preparations.active.clone()))
    }
}
impl Drop for ReceiveCredit {
    fn drop(&mut self) {
        let active = self.0.fetch_sub(1, Ordering::AcqRel);
        assert!(active > 0 && active <= MAX_PREPARATIONS, "receive releases its own credit");
    }
}

/// Only reserved controls remain open. Named backings open one at a time;
/// their identities and completion markers live in the staged persistent index.
pub struct ReceivedVolume {
    operation: Id,
    volume: Id,
    identity: Volume,
    database_key: [u8; 32],
    authority_generation: u64,
    authority_entry_generation: u64,
    authority_qualified: bool,
    #[cfg(feature = "connected-engine")]
    pub(crate) pending_backing: Option<DatabaseId>,
    #[cfg(feature = "connected-engine")]
    pub(crate) owner_baseline: Option<SnapshotToken>,
    entry_scope: Option<EntryId>,
    metadata: Database,
    revocations: Database,
    _directory: ReceivedDirectory,
    _credit: ReceiveCredit,
}

pub struct ReceiptPage {
    pub receipts: Vec<(DatabaseId, DurableRevision)>,
    pub next: Option<DatabaseId>,
}
impl ReceivedVolume {
    /// Trusted receiver calls this only after every receipt and its final
    /// authorization recheck qualify the same AP source cut.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn qualify_authority(&mut self) {
        self.authority_qualified = true;
    }
    pub fn volume(&self) -> Id {
        self.volume
    }
    pub fn entry_scope(&self) -> Option<EntryId> {
        self.entry_scope
    }
    pub fn with_database<T>(
        &self,
        database: DatabaseId,
        f: impl FnOnce(&Database) -> Result<T>,
    ) -> Result<T> {
        match database {
            DatabaseId::Index => f(&self.metadata),
            DatabaseId::Revocations => f(&self.revocations),
            DatabaseId::Entry(id) => {
                if self.entry_scope.is_some_and(|entry| entry != id) {
                    return Err(Error::AccessDenied("backing outside received entry scope"));
                }
                let policy = DatabasePolicy::read_source(&self.metadata, database)?;
                if policy.entry.as_ref().and_then(|entry| entry.protocol).is_none() {
                    return Err(Error::InvalidInput("staged entry is not a database"));
                }
                let path = self._directory.0.join(database.file_name());
                let db = if path.try_exists().map_err(io)? {
                    open_replica(&path, STAGED_CACHE_BYTES)?
                } else {
                    create_replica(
                        &path,
                        database.hash_key(&self.database_key, self.volume),
                        STAGED_CACHE_BYTES,
                    )?
                };
                if db.meta().hash_key != database.hash_key(&self.database_key, self.volume) {
                    return Err(Error::AccessDenied("staged database identity mismatch"));
                }
                f(&db)
            }
        }
    }
    pub fn database_page(&self, after: Option<EntryId>) -> Result<ReplicationDatabasePage> {
        let tx = self.metadata.begin_read()?;
        MetadataPolicy::validate_format(&tx)?;
        if let Some(id) = self.entry_scope {
            let entry = volume_index::IndexSnapshot::new(tx).source_entry_policy(id)?;
            let databases = if after.is_none() && entry.protocol.is_some() {
                vec![DatabaseId::Entry(id)]
            } else {
                Vec::new()
            };
            return Ok(ReplicationDatabasePage { databases, next: None });
        }
        let page = volume_index::IndexSnapshot::new(tx).source_headers(after, 64)?;
        let databases = page
            .entries
            .into_iter()
            .filter(|entry| !entry.deleted && entry.protocol.is_some())
            .map(|entry| DatabaseId::Entry(entry.id))
            .collect();
        Ok(ReplicationDatabasePage { databases, next: page.next })
    }
    /// A missing AP backing has no accepted contents, even if the receiver opened
    /// its scratch file. A qualified AP capture may still publish ready siblings.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn discard_pending_backing(&mut self, database: DatabaseId) -> Result<()> {
        assert!(matches!(database, DatabaseId::Entry(_)) && self.entry_scope.is_none());
        fs::remove_file(self._directory.0.join(database.file_name())).map_err(io)?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.local_indexes().put(format!("received-pending/{database}").as_bytes(), &[1])?;
        tx.commit()?;
        self.pending_backing.get_or_insert(database);
        Ok(())
    }
    /// Finish only after the complete backing transfer is durable and verified.
    pub fn finish_database(&self, database: DatabaseId) -> Result<()> {
        let DatabaseId::Entry(id) = database else { return Ok(()) };
        let policy = DatabasePolicy::read_source(&self.metadata, database)?;
        self.with_database(database, |db| {
            validate_named_backing(&self._directory.0.join("scratch"), db, &policy, None)
        })?;
        sync_directory(&self._directory.0)?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        volume_index::mark_source_backing_ready(&mut tx, id)?;
        tx.commit()?;
        Ok(())
    }
    /// Apply only into detached storage. Completeness and source qualification
    /// belong to the caller; a page alone never marks the backing ready.
    pub fn receive_log_page(&self, id: EntryId, page: &crate::store::bloblog::Page) -> Result<()> {
        let database = DatabaseId::Entry(id);
        let policy = DatabasePolicy::read_source(&self.metadata, database)?;
        if policy.entry.as_ref().and_then(|entry| entry.protocol)
            != Some(volume_index::DatabaseProtocol::BlobLog)
        {
            return Err(Error::InvalidInput("staged entry is not an append log"));
        }
        self.with_database(database, |db| {
            let log = crate::store::bloblog::Log {
                database: db,
                volume: self.volume,
                entry: id,
                encryption_key: None,
                now: now_unix_secs(),
            };
            log.apply_page(page)
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceivedVolumeOutcome {
    pub metadata_changed: bool,
    pub revocations_changed: bool,
    pub databases_changed: bool,
}
impl ReceivedVolumeOutcome {
    pub fn changed(self) -> bool {
        self.metadata_changed || self.revocations_changed || self.databases_changed
    }
}

impl LocalStore {
    /// Resume an index-only interrupted upload from a verified local snapshot.
    /// This copies content into the private stage, never a hosted receipt.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn stage_local_backing(
        &self,
        access: &Access,
        received: &ReceivedVolume,
        database: DatabaseId,
    ) -> Result<SnapshotToken> {
        let DatabaseId::Entry(id) = database else {
            return Err(Error::InvalidInput("local bootstrap requires a named backing"));
        };
        let staged_policy = DatabasePolicy::read_source(&received.metadata, database)?;
        let actor = self.data_actor(access, staged_policy.volume.volume())?;
        staged_policy.authorize_reader(ReplicaReader::SessionParticipant(actor))?;
        let operations = self.access_operations(access, received.volume)?;
        let guards = operations.lock()?;
        let index = self.db(received.volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read(&index, database)?;
        self.admit_access(access, &policy.volume)?;
        policy.authorize_reader(ReplicaReader::SessionParticipant(actor))?;
        let local = volume_index::IndexSnapshot::new(index.begin_read()?);
        if !local.backing_ready(id)? {
            return Err(Error::NotFound("local backing is not initialized"));
        }
        let key = volume_index::entry_key(id);
        let source_birth = index.begin_read()?;
        let staged_birth = received.metadata.begin_read()?;
        let same_birth =
            match (source_birth.get_with_clock(&key)?, staged_birth.get_with_clock(&key)?) {
                (Some((a_clock, a)), Some((b_clock, b))) => {
                    a_clock == b_clock && a.as_ref() == b.as_ref()
                }
                _ => false,
            };
        if !same_birth {
            return Err(Error::AccessDenied("local bootstrap creation differs"));
        }
        let source = self.entry_backing(received.volume, id, false)?;
        let snapshot = OutboundSnapshot::new(&source)?;
        let token = self.source_snapshot_token(received.volume)?;
        drop(guards);
        if policy.entry.as_ref().and_then(|entry| entry.protocol)
            == Some(volume_index::DatabaseProtocol::BlobLog)
        {
            received.with_database(database, |target| {
                snapshot.copy_log_into(&self.path.join(crate::paths::STAGING), target)
            })?;
            return Ok(token);
        }
        let projection = snapshot.project(
            &self.path.join(crate::paths::STAGING),
            &policy,
            ReplicaReader::SessionParticipant(actor),
        )?;
        received.with_database(database, |target| {
            merge_named_replica(
                &self.path.join(crate::paths::STAGING),
                projection.database(),
                target,
                &staged_policy,
            )
        })?;
        Ok(token)
    }

    pub fn prepare_received_volume(&self, volume: Id) -> Result<ReceivedVolume> {
        let credit = ReceiveCredit::enter(&self.received)?;
        let database_key = self.local_database_key(volume)?;
        let identity = self.volume(volume)?;
        let operation = Id::generate()?;
        let parent = self.path.join(crate::paths::RECEIVED);
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&parent).map_err(io)?;
        let directory = ReceivedDirectory::new(parent.join(operation.to_string()));
        fs::DirBuilder::new().mode(0o700).create(&directory.0).map_err(io)?;
        let metadata = create_replica(
            &directory.0.join(DatabaseId::Index.file_name()),
            DatabaseId::Index.hash_key(&database_key, volume),
            STAGED_CACHE_BYTES,
        )?;
        let revocations = create_replica(
            &directory.0.join(DatabaseId::Revocations.file_name()),
            DatabaseId::Revocations.hash_key(&database_key, volume),
            STAGED_CACHE_BYTES,
        )?;
        Ok(ReceivedVolume {
            operation,
            volume,
            identity,
            database_key,
            authority_generation: self.authority_generation(volume)?,
            authority_entry_generation: 0,
            authority_qualified: false,
            #[cfg(feature = "connected-engine")]
            pending_backing: None,
            #[cfg(feature = "connected-engine")]
            owner_baseline: None,
            entry_scope: None,
            metadata,
            revocations,
            _directory: directory,
            _credit: credit,
        })
    }

    pub fn prepare_received_entry(
        &self,
        address: crate::store::volume_index::EntryAddress,
    ) -> Result<ReceivedVolume> {
        let mut stage = self.prepare_received_volume(address.volume)?;
        stage.entry_scope = Some(address.entry);
        (stage.authority_generation, stage.authority_entry_generation) =
            self.entry_authority_generations(address.volume, address.entry)?;
        Ok(stage)
    }

    /// Seal only the two qualified sources. A retirement witness carries no
    /// current ACL or revocations and cannot clear a data-access fence.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn confirm_entry_retirement(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
        events: [EntryId; 2],
    ) -> Result<()> {
        use volume_index::namespace::{observations::Ledger, payloads, receipt, source::cut};
        let entry = stage.entry_scope.ok_or(Error::InvalidInput("selected retirement required"))?;
        let captured = stage.metadata.begin_read()?;
        let rows = receipt::capture(&captured, Some(entry))?;
        let witnessed = rows.iter().map(|row| row.source.id()).collect::<Result<Vec<_>>>()?;
        let remote = volume_index::IndexSnapshot::new(stage.metadata.begin_read()?);
        let winner = remote.source_retirement_event(entry)?;
        if remote.source_birth_event(entry)? != events[0]
            || witnessed.len() != 2
            || !witnessed.contains(&events[0])
            || !witnessed.contains(&winner)
        {
            return Err(Error::AccessDenied("AP retirement differs from selected sources"));
        }
        let operations = self.access_operations(access, stage.volume)?;
        let _guards = operations.lock()?;
        if self.entry_authority_generations(stage.volume, entry)?
            != (stage.authority_generation, stage.authority_entry_generation)
            || self.local_database_key(stage.volume)? != stage.database_key
        {
            return Err(Error::AccessDenied("entry retirement capture was superseded"));
        }
        let index = self.db(stage.volume, DatabaseId::Index)?;
        let policy = MetadataPolicy::read(&index)?;
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        if snapshot.selected_retirement(entry)?.is_none_or(|(current, _)| current[0] != events[0])
            || !policy.volume().same_identity(&stage.identity)
        {
            return Err(Error::AccessDenied("entry retirement binding changed"));
        }
        self.admit_retirement_writer(access, entry, &policy, &snapshot)?;
        drop(snapshot);
        let mut tx = index.begin_write(now_unix_secs())?;
        let mut local = tx.local_indexes();
        let before = Ledger::load(&local)?;
        let mut after = before.clone();
        after.apply_receipt(&rows)?;
        let mut changed = before != after;
        if changed {
            after.write_changes(&before, &mut local)?;
        }
        drop(after);
        drop(before);
        let retained =
            2 * 1024 + rows.iter().map(|row| row.source.observed.heap_bytes()).sum::<usize>();
        payloads::Usage::validate_local(&local)?;
        let mut scan = cut::Scan::materializing(retained)?;
        for row in &rows {
            let bytes = payloads::capture(&captured, &row.source, &mut scan, retained)?;
            changed |= payloads::retain_new(&mut local, row, &bytes, retained)?;
        }
        if !changed {
            return Ok(());
        }
        cut::project(&tx)?.write_current(&mut tx.local_indexes())?;
        tx.commit()?;
        self.changes.observed_changed(Some(stage.volume));
        Ok(())
    }

    /// Revocation-first bootstrap accepts only contraction before root admission.
    pub fn adopt_received_revocations(
        &self,
        access: &Access,
        received: &ReceivedVolume,
    ) -> Result<bool> {
        let operations = self.access_operations(access, received.volume)?;
        let _guards = operations.lock()?;
        let admitted = if let Some(id) = &access.session_id {
            let session = self.session(id)?;
            session.replica_volume() == received.volume
                || session.is_distinct_world(received.volume)
        } else {
            self.has_direct_volume_grant(received.volume)?
        };
        if !admitted
            || access.account != self.account.id
            || !received.identity.same_identity(&self.volume(received.volume)?)
            || self.local_database_key(received.volume)? != received.database_key
        {
            return Err(Error::AccessDenied("revocation stage binding"));
        }
        let target = self.db(received.volume, DatabaseId::Revocations)?;
        let before = replica_root(&target)?;
        self.merge_received_revocations(received.volume, &received.revocations, &target, true)?;
        let changed = before != replica_root(&target)?;
        self.check_received_denial(access)?;
        if access.session_id.is_none() {
            self.check_participant(received.volume, access.account)?;
        }
        Ok(changed)
    }

    pub fn adopt_received_volume(
        &self,
        access: &Access,
        received: &ReceivedVolume,
    ) -> Result<ReceivedVolumeOutcome> {
        let _timing = crate::trace::Span::new("storage.adopt_received_volume");
        let volume = received.volume;
        let operations = self.access_operations(access, volume)?;
        let gate = self.volume_operation(volume)?;
        let _guards = operations.lock_for_publication(&gate)?;
        let owner_policy = received.identity.owner == self.account.id && received.identity.personal;
        let tied_acl = if owner_policy {
            let index = self.db(volume, DatabaseId::Index)?;
            let previous = index.begin_read()?;
            let incoming = received.metadata.begin_read()?;
            let acl = previous.get_with_clock(&DEFAULT_PERMISSIONS_KEY)?;
            let other = incoming.get_with_clock(&DEFAULT_PERMISSIONS_KEY)?;
            matches!((acl, other), (Some((a, left)), Some((b, right))) if a == b && left.as_ref() != right.as_ref())
        } else {
            false
        };
        let revocations = self.db(volume, DatabaseId::Revocations)?;
        let before_revocations = replica_root(&revocations)?;
        let pending = if let Some(pending) = self.pending_publication(volume)? {
            self.check_publication_binding(access, received, &pending)?;
            pending
        } else {
            // A ready sibling may have applied newer AP data through its own
            // live stream. Never classify that data against an older capture.
            #[cfg(feature = "connected-engine")]
            if let Some(baseline) = received.owner_baseline
                && gate.snapshot_token()? != baseline
            {
                return Err(Error::LocalContention(Error::OWNER_CAPTURE_CHANGED));
            }
            self.prepare_received_publication(access, received)?;
            self.begin_received_publication(access, received, &gate)?
        };
        #[cfg(test)]
        self.received_checkpoint(PublicationCheckpoint::Intent)?;
        let metadata = self.db(volume, DatabaseId::Index)?;
        let before = replica_root(&metadata)?;
        let name_changed =
            self.volume(volume)?.name != MetadataPolicy::read(&received.metadata)?.volume.name;
        // Replay completes an already admitted decision. Current reads and any
        // new incoming data still authorize normally. Rejected local facts use
        // the sealed canonical cut; accepted facts keep ordinary clock order.
        self.merge_received_control(access, received, DatabaseId::Revocations)?;
        let databases_changed = self.adopt_named_databases(access, received, &pending)?;
        sync_directory(&self.path.join(crate::paths::VOLUMES).join(volume.to_string()))?;
        if name_changed {
            self.prepare_volume_name(volume)?;
        }
        #[cfg(test)]
        self.received_before_commit(DatabaseId::Index)?;
        self.merge_received_control(access, received, DatabaseId::Index)?;
        #[cfg(test)]
        self.received_checkpoint(PublicationCheckpoint::Index)?;
        self.apply_namespace_plan(received, pending.namespace_digest)?;
        self.publish_received_readiness(received, &pending)?;
        #[cfg(test)]
        self.received_checkpoint(PublicationCheckpoint::Readiness)?;
        if name_changed {
            self.project_volume_name(volume)?;
        }
        let outcome = ReceivedVolumeOutcome {
            metadata_changed: before != replica_root(&metadata)?,
            revocations_changed: before_revocations != replica_root(&revocations)?,
            databases_changed,
        };
        self.finish_received_publication(received, &pending, &gate)?;
        if tied_acl {
            self.restate_volume_acl(volume, self.index(volume)?.default_permissions())?;
        }
        if owner_policy && (tied_acl || outcome.metadata_changed || outcome.revocations_changed) {
            self.notify_volume_invitation_policy(&received.identity);
        }
        self.changes.observed_changed(Some(volume));
        Ok(outcome)
    }

    fn merge_received_control(
        &self,
        access: &Access,
        received: &ReceivedVolume,
        database: DatabaseId,
    ) -> Result<()> {
        let source = match database {
            DatabaseId::Index => &received.metadata,
            DatabaseId::Revocations => &received.revocations,
            _ => return Err(Error::InvalidInput("received control database required")),
        };
        let target = self.db(received.volume, database)?;
        if database == DatabaseId::Index
            && rejected::merge_canonical_index(
                &self.path.join(crate::paths::STAGING),
                received,
                &target,
                self.data_actor(access, &self.volume(received.volume)?)?,
            )?
        {
            return Ok(());
        }
        if let Some(entry) = received.entry_scope {
            let policy = DatabasePolicy::read_source(&received.metadata, DatabaseId::Entry(entry))?;
            super::scoped::ScopedSnapshot::for_reconciliation(
                source,
                database,
                &policy,
                self.data_actor(access, policy.volume.volume())?,
                access.build_id.as_deref(),
            )?
            .merge_into(&target)
        } else if database == DatabaseId::Index {
            merge_index_replica(
                &self.path.join(crate::paths::STAGING),
                source,
                &target,
                kvdb::Lww,
                |tx| MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES),
            )
        } else {
            self.merge_received_revocations(received.volume, source, &target, false)
        }
    }

    fn merge_received_revocations(
        &self,
        volume: Id,
        source: &Database,
        target: &Database,
        contraction_only: bool,
    ) -> Result<()> {
        let before = replica_root(target)?;
        merge_revocations(
            &self.path.join(crate::paths::STAGING),
            source,
            target,
            contraction_only,
        )?;
        if before != replica_root(target)? {
            self.changes.observed_changed(Some(volume));
        }
        Ok(())
    }

    fn prepare_received_publication(
        &self,
        access: &Access,
        received: &ReceivedVolume,
    ) -> Result<()> {
        self.check_received_quarantine(received)?;
        let volume = received.volume;
        if received.database_key != self.local_database_key(volume)? {
            return Err(Error::AccessDenied("staged database credentials changed"));
        }
        let descriptor = self.volume(volume)?;
        let direct_clock = self.check_received_identity(access, received, &descriptor)?;
        if let Some(entry) = received.entry_scope {
            return self.prepare_received_entry_publication(access, received, entry);
        }
        let metadata = self.db(volume, DatabaseId::Index)?;
        let before = replica_root(&metadata)?;
        if before.is_some() {
            let previous = MetadataPolicy::read(&metadata)?;
            if received.authority_qualified
                && let Some(id) = access.session_id()
                && let session = self.session(id)?
                && session.is_distinct_world(volume)
            {
                // A qualified World cut can grant a new build or restore catalog
                // Read. The retained policy identifies the baseline. Both the
                // received and final policies below must admit the actual binding.
                previous.validate_world_binding(&session)?;
            } else if direct_clock.is_none() {
                self.authorize_received(access, &previous, true)?;
            }
        } else {
            self.check_received_denial(access)?;
        }
        {
            let policy = MetadataPolicy::read(&received.metadata)?;
            self.authorize_received(access, &policy, false)?;
            let confirmed = self.prepare_namespace_plan(access, received, &policy)?;
            self.prepare_rejected_cut(access, received, &confirmed)?;
        }
        rejected::merge_local_index(
            &self.path.join(crate::paths::STAGING),
            received,
            &metadata,
            self.data_actor(access, &descriptor)?,
        )?;
        let effective = MetadataPolicy::read(&received.metadata)?;
        if !effective.volume.same_identity(&descriptor) {
            return Err(Error::AccessDenied("replica volume mismatch"));
        }
        self.authorize_received(access, &effective, false)?;
        let revocations = self.db(volume, DatabaseId::Revocations)?;
        self.merge_received_revocations(
            received.volume,
            &received.revocations,
            &revocations,
            false,
        )?;
        self.check_received_denial(access)?;
        if access.session_id.is_none() {
            self.check_participant(volume, access.account)?;
        }
        Ok(())
    }

    fn check_received_identity(
        &self,
        access: &Access,
        received: &ReceivedVolume,
        descriptor: &Volume,
    ) -> Result<Option<u64>> {
        let direct_clock = self.direct_offer_clock(access, descriptor)?;
        let policy = MetadataPolicy::read(&received.metadata)?;
        if !policy.volume.same_identity(descriptor) {
            return Err(Error::AccessDenied("replica volume mismatch"));
        }
        if let Some(clock) = direct_clock {
            self.check_direct_acl(access.account, &policy, &received.metadata, clock)?;
        }
        Ok(direct_clock)
    }

    fn check_received_quarantine(&self, received: &ReceivedVolume) -> Result<()> {
        if received.authority_qualified {
            return Ok(());
        }
        let index = self.db(received.volume, DatabaseId::Index)?;
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        let mut after = None;
        loop {
            let page = received.database_page(after)?;
            for database in page.databases {
                if let DatabaseId::Entry(entry) = database
                    && snapshot.backing_quarantined(entry)?
                    && received._directory.0.join(database.file_name()).is_file()
                {
                    return Err(Error::AccessDenied(
                        "accepted backing requires a qualified AP cut",
                    ));
                }
            }
            let Some(next) = page.next else { return Ok(()) };
            after = Some(next);
        }
    }

    fn prepare_received_entry_publication(
        &self,
        access: &Access,
        received: &ReceivedVolume,
        entry: EntryId,
    ) -> Result<()> {
        let policy = DatabasePolicy::read_source(&received.metadata, DatabaseId::Entry(entry))?;
        policy.metadata_readable(self.data_actor(access, policy.volume.volume())?)?;
        self.admit_received_entry(access, received, &policy.volume)?;
        let confirmed = self.prepare_namespace_plan(access, received, &policy.volume)?;
        self.prepare_rejected_cut(access, received, &confirmed)?;
        let revocations = self.db(received.volume, DatabaseId::Revocations)?;
        super::scoped::ScopedSnapshot::for_reconciliation(
            &revocations,
            DatabaseId::Revocations,
            &policy,
            self.data_actor(access, policy.volume.volume())?,
            access.build_id.as_deref(),
        )?
        .merge_into(&received.revocations)?;
        let retained = self.db(received.volume, DatabaseId::Index)?;
        if received.rejected_entry(entry)? {
            rejected::merge_local_index(
                &self.path.join(crate::paths::STAGING),
                received,
                &retained,
                self.data_actor(access, policy.volume.volume())?,
            )?;
        } else if replica_root(&retained)?.is_some() {
            super::scoped::ScopedSnapshot::for_reconciliation(
                &retained,
                DatabaseId::Index,
                &policy,
                self.data_actor(access, policy.volume.volume())?,
                access.build_id.as_deref(),
            )?
            .merge_into(&received.metadata)?;
        }
        let current = DatabasePolicy::read_source(&received.metadata, DatabaseId::Entry(entry))?;
        if !current.volume.volume().same_identity(&self.volume(received.volume)?) {
            return Err(Error::AccessDenied("replica volume mismatch"));
        }
        current.metadata_readable(self.data_actor(access, current.volume.volume())?)?;
        self.admit_received_entry(access, received, &current.volume)
    }

    fn admit_received_entry(
        &self,
        access: &Access,
        received: &ReceivedVolume,
        policy: &MetadataPolicy,
    ) -> Result<()> {
        if participant_revoked(&received.revocations, access.account)? {
            return Err(Error::AccessDenied("revoked volume participant"));
        }
        self.direct_offer_clock(access, policy.volume())?;
        let Some(id) = &access.session_id else { return Ok(()) };
        if self.session_denial(id)?.is_some() {
            return Err(Error::AccessDenied("session participant revoked"));
        }
        let mut session = self.session(id)?;
        if let Some(version) = &access.build_id {
            session.resolved_build_id = version.clone();
        }
        if session.is_distinct_world(received.volume) {
            policy.admit_world(access.account, &session)?;
        } else if session.replica_volume() != received.volume {
            let grant = self.personal_grant(access, &session, received.volume)?;
            if !grant.write
                && let Some(entry) = received.entry_scope
                && !DatabasePolicy::read_source(&received.metadata, DatabaseId::Entry(entry))?
                    .metadata_readable(self.data_actor(access, policy.volume())?)?
            {
                return Err(Error::AccessDenied("personal volume is read-only"));
            }
        }
        if session.replica_volume() == received.volume {
            policy.admit(access.account, &session, received.volume)?;
        } else {
            self.index(session.replica_volume())?.admit(
                access.account,
                &session,
                session.replica_volume(),
            )?;
        }
        if session.replica_volume() != received.volume {
            self.check_participant(session.replica_volume(), access.account)?;
        }
        Ok(())
    }

    fn adopt_named_databases(
        &self,
        access: &Access,
        received: &ReceivedVolume,
        pending: &PendingPublication,
    ) -> Result<bool> {
        let mut changed = false;
        let mut after = None;
        loop {
            let page = received.published_database_page(pending, after)?;
            for database in page.0 {
                let DatabaseId::Entry(id) = database else {
                    continue;
                };
                let policy = DatabasePolicy::read_source(&received.metadata, database)?;
                let index = self.db(received.volume, DatabaseId::Index)?;
                let quarantined = volume_index::IndexSnapshot::new(index.begin_read()?)
                    .backing_quarantined(id)?;
                if quarantined && !received.authority_qualified {
                    return Err(Error::AccessDenied(
                        "accepted backing requires a qualified AP cut",
                    ));
                }
                let target = self.entry_backing(received.volume, id, true)?;
                let before = replica_root(&target)?;
                #[cfg(test)]
                self.received_before_commit(database)?;
                received.with_database(database, |source| {
                    let log = policy.entry.as_ref().and_then(|entry| entry.protocol)
                        == Some(volume_index::DatabaseProtocol::BlobLog);
                    let append = log
                        && policy
                            .authorize(
                                self.data_actor(access, policy.volume.volume())?,
                                Permission::Append,
                            )
                            .is_ok();
                    if quarantined || (received.rejected_entry(id)? && !append) {
                        rejected::merge_backing(
                            &self.path.join(crate::paths::STAGING),
                            source,
                            &target,
                            &policy,
                            self.data_actor(access, policy.volume.volume())?,
                            quarantined || log,
                        )
                    } else if log {
                        merge_append_log(source, &target, received.volume, id)
                    } else {
                        merge_named_replica(
                            &self.path.join(crate::paths::STAGING),
                            source,
                            &target,
                            &policy,
                        )
                    }
                })?;
                let committed = before != replica_root(&target)?;
                if committed {
                    self.changes.observed_database_changed(received.volume, database);
                }
                changed |= committed;
                #[cfg(test)]
                self.received_checkpoint(PublicationCheckpoint::Backing(database))?;
            }
            let Some(next) = page.1 else { return Ok(changed) };
            after = Some(next);
        }
    }

    fn publish_received_readiness(
        &self,
        received: &ReceivedVolume,
        pending: &PendingPublication,
    ) -> Result<()> {
        let database = self.db(received.volume, DatabaseId::Index)?;
        let mut after = None;
        loop {
            let page = received.published_database_page(pending, after)?;
            let mut tx = database.begin_write(now_unix_secs())?;
            for database in page.0 {
                let DatabaseId::Entry(id) = database else {
                    continue;
                };
                if self
                    .path
                    .join(crate::paths::VOLUMES)
                    .join(received.volume.to_string())
                    .join(database.file_name())
                    .try_exists()
                    .map_err(io)?
                {
                    volume_index::mark_backing_ready(&mut tx, id)?;
                }
            }
            tx.commit()?;
            let Some(next) = page.1 else { return Ok(()) };
            after = Some(next);
        }
    }
    #[cfg(test)]
    fn received_checkpoint(&self, checkpoint: PublicationCheckpoint) -> Result<()> {
        if CRASH_AT.get() == Some(checkpoint) {
            // A child test exits without unwinding or closing any database,
            // exercising committed storage rather than graceful destructor I/O.
            std::process::exit(73);
        }
        if let Some(hook) = self
            .received
            .after_checkpoint
            .lock()
            .map_err(|_| Error::Internal("receipt checkpoint hook lock is poisoned"))?
            .clone()
        {
            hook(checkpoint)?;
        }
        Ok(())
    }

    fn check_received_denial(&self, access: &Access) -> Result<()> {
        if let Some(id) = &access.session_id {
            if self.session_denial(id)?.is_some() {
                return Err(Error::AccessDenied("session participant revoked"));
            }
            let session = self.session(id)?;
            if self
                .db(session.replica_volume(), crate::store::DatabaseId::Revocations)?
                .begin_read()?
                .has(access.account.to_string().as_bytes())?
            {
                return Err(Error::AccessDenied("revoked session participant"));
            }
        }
        Ok(())
    }

    // Bootstrap admission borrows its target policy; at most one bounded root
    // policy is materialized here and released before transfer scratch. The
    // caller may retain the target as its one replay policy.
    fn authorize_received(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        retained: bool,
    ) -> Result<()> {
        self.check_received_denial(access)?;
        self.direct_offer_clock(access, policy.volume())?;
        if access.session_id.is_none() {
            self.check_participant(policy.volume.id, access.account)?;
        }
        if let Some(id) = &access.session_id {
            let mut session = self.session(id)?;
            if !retained && let Some(version) = &access.build_id {
                session.resolved_build_id = version.clone();
            }
            if session.is_distinct_world(policy.volume.id) {
                self.check_participant(policy.volume.id, access.account)?;
                policy.admit_world(access.account, &session)?;
            } else if session.replica_volume() != policy.volume.id {
                self.personal_grant(access, &session, policy.volume.id)?;
            }
            if session.replica_volume() == policy.volume.id {
                policy.admit(access.account, &session, session.replica_volume())?;
            } else {
                let root = MetadataPolicy::read_bounded(
                    &*self.db(session.replica_volume(), crate::store::DatabaseId::Index)?,
                    MetadataPolicy::MAX_MATERIALIZED_BYTES,
                )?;
                root.admit(access.account, &session, session.replica_volume())?;
            }
        }
        policy.authorize(self.data_actor(access, policy.volume())?, Permission::Read)
    }
    #[cfg(test)]
    fn received_before_commit(&self, database: crate::store::DatabaseId) -> Result<()> {
        if let Some(hook) = self
            .before_creation_commit
            .lock()
            .map_err(|_| Error::Internal("creation commit hook lock is poisoned"))?
            .clone()
        {
            hook(database)?;
        }
        Ok(())
    }
}

fn validate_named_backing(
    scratch: &Path,
    database: &Database,
    policy: &DatabasePolicy,
    writer: Option<DataActor>,
) -> Result<()> {
    if let Some(entry) = &policy.entry
        && entry.protocol == Some(volume_index::DatabaseProtocol::BlobLog)
    {
        return crate::store::bloblog::Log {
            database,
            volume: policy.volume.volume.id,
            entry: entry.id,
            encryption_key: None,
            now: now_unix_secs(),
        }
        .validate();
    }
    let rejected = std::cell::RefCell::new(None);
    let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
        if let Err(error) = policy.validate_sync_source_as(delta, writer) {
            *rejected.borrow_mut() = Some(error);
        }
        Ok(kvdb::sync::Resolution::KeepLocal)
    };
    // A pinned sender replay into itself visits no identical source entries;
    // use an empty detached target so every source is validated.
    let checked = empty_detached_replica(scratch, database)?;
    merge_replica(scratch, database, checked.database(), resolver, |_| {
        rejected.borrow_mut().take().map_or(Ok(()), Err)
    })?;
    if matches!(
        policy.entry.as_ref().map(|entry| entry.kind),
        Some(crate::store::EntryType::ObjectDb)
    ) {
        object_database::validate_all(&database.begin_write(now_unix_secs())?)?;
    }
    Ok(())
}

fn merge_append_log(
    source: &Database,
    target: &Database,
    volume: Id,
    entry: EntryId,
) -> Result<()> {
    use crate::store::bloblog::{Cursor, Log};
    let from = Log { database: source, volume, entry, encryption_key: None, now: now_unix_secs() };
    let to = Log { database: target, volume, entry, encryption_key: None, now: now_unix_secs() };
    let mut cursor = None;
    loop {
        let page = from.get(cursor.as_ref())?;
        to.apply(&page.entries, false)?;
        if !page.more {
            break;
        }
        cursor = Some(page.cursor);
    }
    if let Some(cursor) = get::<Cursor>(source, b"remote-cursor")? {
        to.validate_cursor(&cursor)?;
        if get::<Cursor>(target, b"remote-cursor")?.as_ref() != Some(&cursor) {
            put(target, b"remote-cursor", &cursor)?;
        }
    }
    Ok(())
}

fn merge_named_replica(
    scratch: &Path,
    source: &Database,
    target: &Database,
    policy: &DatabasePolicy,
) -> Result<()> {
    use kvdb::sync::{Lww, Resolution, SyncResolver};
    let rejected = std::cell::RefCell::new(None);
    let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
        if let Err(error) = policy.validate_sync_source(delta, None) {
            *rejected.borrow_mut() = Some(error);
            return Ok(Resolution::KeepLocal);
        }
        Lww.resolve(delta)
    };
    merge_replica(scratch, source, target, resolver, |tx| {
        rejected.borrow_mut().take().map_or(Ok(()), Err)?;
        if matches!(
            policy.entry.as_ref().map(|entry| entry.kind),
            Some(crate::store::EntryType::ObjectDb)
        ) {
            object_database::validate_all(tx)?;
        }
        Ok(())
    })
}

fn merge_revocations(
    scratch: &Path,
    source: &Database,
    target: &Database,
    contraction_only: bool,
) -> Result<()> {
    use kvdb::sync::{Lww, Resolution, SyncEntState, SyncResolver};
    let rejected = std::cell::RefCell::new(None);
    let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
        if std::str::from_utf8(delta.key).ok().and_then(|key| Id::parse(key).ok()).is_none() {
            *rejected.borrow_mut() = Some(Error::InvalidInput("invalid revocation account"));
            return Ok(Resolution::KeepLocal);
        }
        if contraction_only && !matches!(delta.remote, SyncEntState::Present { .. }) {
            return Ok(Resolution::KeepLocal);
        }
        Lww.resolve(delta)
    };
    merge_replica(scratch, source, target, resolver, |_| {
        rejected.borrow_mut().take().map_or(Ok(()), Err)
    })
}

impl ReceivedVolume {
    /// Hosted receipts are detached replica-local metadata, never source facts.
    pub fn record_receipt(&self, database: DatabaseId, receipt: &DurableRevision) -> Result<()> {
        let mut key = vec![volume_index::META_BYTE, 12];
        key.extend_from_slice(&database.to_bytes());
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.local_indexes().put(&key, &encode(receipt)?)?;
        tx.commit()?;
        Ok(())
    }
    /// Only an AP backing receipt from this transfer can certify equality.
    /// A missing receipt is distinct from an acknowledged empty database.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn hosted_backing_matches(&self, entry: EntryId, root: ReplicaRoot) -> Result<bool> {
        let mut key = vec![volume_index::META_BYTE, 12];
        key.extend_from_slice(&DatabaseId::Entry(entry).to_bytes());
        let tx = self.metadata.begin_read()?;
        let Some(value) = tx.local_get(&key)? else { return Ok(false) };
        if value.len() > 1024 {
            return Err(Error::corrupt("receipt value exceeds 1024 bytes"));
        }
        let receipt: DurableRevision = decode(&value)?;
        Ok(receipt.root == root)
    }
    pub fn receipt_page(&self, after: Option<DatabaseId>) -> Result<ReceiptPage> {
        let prefix = [volume_index::META_BYTE, 12];
        let mut start = prefix.to_vec();
        if let Some(after) = after {
            start.extend_from_slice(&after.to_bytes());
        }
        let tx = self.metadata.begin_read()?;
        let mut iter = tx.local_iter_from(&start)?;
        let mut records = Vec::with_capacity(64);
        while let Some((key, value)) = iter.next().transpose()? {
            if !key.starts_with(&prefix) {
                break;
            }
            if key.len() != 18 {
                return Err(Error::corrupt("receipt key must be 18 bytes"));
            }
            let database = DatabaseId::from_bytes(key[2..].try_into().expect("fixed ID suffix"))?;
            if Some(database) == after {
                continue;
            }
            if records.len() == 64 {
                let next = records.last().map(|(database, _)| *database);
                return Ok(ReceiptPage { receipts: records, next });
            }
            if value.len() > 1024 {
                return Err(Error::corrupt("receipt value exceeds 1024 bytes"));
            }
            records.push((database, decode(&value)?));
        }
        Ok(ReceiptPage { receipts: records, next: None })
    }
}
