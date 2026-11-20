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
    /// Finish only after the complete backing transfer is durable and verified.
    pub fn finish_database(&self, database: DatabaseId) -> Result<()> {
        let DatabaseId::Entry(id) = database else { return Ok(()) };
        let policy = DatabasePolicy::read_source(&self.metadata, database)?;
        self.with_database(database, |db| {
            validate_named_backing(&self._directory.0.join("scratch"), db, &policy)
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
        staged_policy.authorize_reader(ReplicaReader::Participant(access.account))?;
        let operations = self.access_operations(access, received.volume)?;
        let guards = operations.lock()?;
        let index = self.db(received.volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read(&index, database)?;
        self.admit_access(access, &policy.volume)?;
        policy.authorize_reader(ReplicaReader::Participant(access.account))?;
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
            ReplicaReader::Participant(access.account),
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
        let credentials = self.credentials(volume)?;
        let identity = self.volume(volume)?;
        let operation = Id::generate()?;
        let parent = self.path.join(crate::paths::RECEIVED);
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&parent).map_err(io)?;
        let directory = ReceivedDirectory::new(parent.join(operation.to_string()));
        fs::DirBuilder::new().mode(0o700).create(&directory.0).map_err(io)?;
        let metadata = create_replica(
            &directory.0.join(DatabaseId::Index.file_name()),
            DatabaseId::Index.hash_key(&credentials.database_key, volume),
            STAGED_CACHE_BYTES,
        )?;
        let revocations = create_replica(
            &directory.0.join(DatabaseId::Revocations.file_name()),
            DatabaseId::Revocations.hash_key(&credentials.database_key, volume),
            STAGED_CACHE_BYTES,
        )?;
        Ok(ReceivedVolume {
            operation,
            volume,
            identity,
            database_key: credentials.database_key,
            authority_generation: self.authority_generation(volume)?,
            authority_entry_generation: 0,
            authority_qualified: false,
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

    /// Revocation-first bootstrap accepts only contraction before root admission.
    pub fn adopt_received_revocations(
        &self,
        access: &Access,
        received: &ReceivedVolume,
    ) -> Result<bool> {
        let id = access
            .session_id
            .as_ref()
            .ok_or(Error::AccessDenied("session required for revocation bootstrap"))?;
        let operations = self.access_operations(access, received.volume)?;
        let _guards = operations.lock()?;
        let session = self.session(id)?;
        if session.replica_volume() != received.volume
            || access.account != self.account.id
            || !received.identity.same_identity(&self.volume(received.volume)?)
            || self.credentials(received.volume)?.database_key != received.database_key
        {
            return Err(Error::AccessDenied("revocation stage binding"));
        }
        let target = self.db(received.volume, DatabaseId::Revocations)?;
        let before = replica_root(&target)?;
        self.merge_received_revocations(received.volume, &received.revocations, &target, true)?;
        let changed = before != replica_root(&target)?;
        self.check_received_denial(access)?;
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
        let revocations = self.db(volume, DatabaseId::Revocations)?;
        let before_revocations = replica_root(&revocations)?;
        let pending = if let Some(pending) = self.pending_publication(volume)? {
            self.check_publication_binding(access, received, &pending)?;
            pending
        } else {
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
                access.account,
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
                access.account,
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
        if received.database_key != self.credentials(volume)?.database_key {
            return Err(Error::AccessDenied("staged database credentials changed"));
        }
        let descriptor = self.volume(volume)?;
        if !MetadataPolicy::read(&received.metadata)?.volume.same_identity(&descriptor) {
            return Err(Error::AccessDenied("replica volume mismatch"));
        }
        if let Some(entry) = received.entry_scope {
            return self.prepare_received_entry_publication(access, received, entry);
        }
        let metadata = self.db(volume, DatabaseId::Index)?;
        let before = replica_root(&metadata)?;
        if before.is_some() {
            self.authorize_received(access, &MetadataPolicy::read(&metadata)?, true)?;
        } else {
            self.check_received_denial(access)?;
        }
        {
            let policy = MetadataPolicy::read(&received.metadata)?;
            self.authorize_received(access, &policy, false)?;
            self.prepare_namespace_plan(access, received, &policy)?;
        }
        self.prepare_rejected_cut(access, received)?;
        rejected::merge_local_index(
            &self.path.join(crate::paths::STAGING),
            received,
            &metadata,
            access.account,
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
        self.check_received_denial(access)
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
        policy.metadata_readable(access.account)?;
        self.admit_received_entry(access, received, &policy.volume)?;
        self.prepare_namespace_plan(access, received, &policy.volume)?;
        self.prepare_rejected_cut(access, received)?;
        let revocations = self.db(received.volume, DatabaseId::Revocations)?;
        super::scoped::ScopedSnapshot::for_reconciliation(
            &revocations,
            DatabaseId::Revocations,
            &policy,
            access.account,
            access.build_id.as_deref(),
        )?
        .merge_into(&received.revocations)?;
        let retained = self.db(received.volume, DatabaseId::Index)?;
        if received.rejected_entry(entry)? {
            rejected::merge_local_index(
                &self.path.join(crate::paths::STAGING),
                received,
                &retained,
                access.account,
            )?;
        } else if replica_root(&retained)?.is_some() {
            super::scoped::ScopedSnapshot::for_reconciliation(
                &retained,
                DatabaseId::Index,
                &policy,
                access.account,
                access.build_id.as_deref(),
            )?
            .merge_into(&received.metadata)?;
        }
        let current = DatabasePolicy::read_source(&received.metadata, DatabaseId::Entry(entry))?;
        if !current.volume.volume().same_identity(&self.volume(received.volume)?) {
            return Err(Error::AccessDenied("replica volume mismatch"));
        }
        current.metadata_readable(access.account)?;
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
        let Some(id) = &access.session_id else { return Ok(()) };
        if self.session_denial(id)?.is_some() {
            return Err(Error::AccessDenied("session participant revoked"));
        }
        let mut session = self.session(id)?;
        if let Some(version) = &access.build_id {
            session.resolved_build_id = version.clone();
        }
        if ![session.replica_volume()].contains(&received.volume) {
            let grant = self.personal_grant(access, &session, received.volume)?;
            if !grant.write
                && let Some(entry) = received.entry_scope
                && !DatabasePolicy::read_source(&received.metadata, DatabaseId::Entry(entry))?
                    .metadata_readable(access.account)?
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
                                crate::store::object_contract::Principal::Human(access.account),
                                Permission::Append,
                            )
                            .is_ok();
                    if quarantined || (received.rejected_entry(id)? && !append) {
                        rejected::merge_backing(
                            &self.path.join(crate::paths::STAGING),
                            source,
                            &target,
                            &policy,
                            access.account,
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
        if let Some(id) = &access.session_id {
            let mut session = self.session(id)?;
            if !retained && let Some(version) = &access.build_id {
                session.resolved_build_id = version.clone();
            }
            if session.replica_volume() != policy.volume.id {
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
        policy.authorize(access.account, Permission::Read)
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
        if let Err(error) = policy.validate_sync_source(delta, None) {
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
