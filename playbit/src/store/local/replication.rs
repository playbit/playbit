//! Ordinary replication addresses reserved controls or one stable named backing.
use super::*;
use crate::store::{DatabaseId, object_database, volume_index};
use std::cell::RefCell;

pub struct ReplicationDatabasePage {
    pub databases: Vec<DatabaseId>,
    pub next: Option<crate::store::EntryId>,
}

#[cfg(feature = "connected-engine")]
struct OwnerPermissionSources<'view, 'local, 'remote> {
    local_defaults: &'view Acl,
    remote_defaults: &'view Acl,
    local_index: &'view volume_index::IndexSnapshot<'local, RedbStorage>,
    remote_index: &'view volume_index::IndexSnapshot<'remote, RedbStorage>,
}

fn observe_source(
    policy: &DatabasePolicy,
    delta: &kvdb::sync::Delta<'_>,
    entries: &RefCell<BTreeSet<crate::store::EntryId>>,
    objects: &RefCell<BTreeSet<Id>>,
) -> Result<()> {
    if policy.database == DatabaseId::Index && volume_index::is_index_key(delta.key) {
        let id = volume_index::validate_source(delta)?;
        let mut touched = entries.borrow_mut();
        if touched.len() == volume_index::MAX_REPAIR_IDS && !touched.contains(&id) {
            return Err(Error::LimitExceeded);
        }
        touched.insert(id);
    } else if let Some(object) = policy.validate_source(delta, None)? {
        let mut touched = objects.borrow_mut();
        if touched.len() == 64 && !touched.contains(&object) {
            return Err(Error::LimitExceeded);
        }
        touched.insert(object);
    }
    Ok(())
}

fn validate_pending(
    tx: &kvdb::WriteTxn<'_, RedbStorage>,
    database: DatabaseId,
    objects: &BTreeSet<Id>,
) -> Result<()> {
    match database {
        DatabaseId::Index => {
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        }
        DatabaseId::Entry(_) => object_database::validate_pending(tx, objects),
        DatabaseId::Revocations => Ok(()),
    }
}

fn append_relayer(policy: &DatabasePolicy, principal: DataActor) -> Result<bool> {
    match policy.authorize(principal, Permission::Write) {
        Ok(()) => Ok(false),
        Err(Error::AccessDenied(_)) => {
            policy.authorize(principal, Permission::Append)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn validate_replica(tx: &kvdb::WriteTxn<'_, RedbStorage>, policy: &DatabasePolicy) -> Result<()> {
    match policy.database {
        DatabaseId::Index => {
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        }
        DatabaseId::Entry(_)
            if matches!(
                policy.entry.as_ref().map(|entry| entry.kind),
                Some(crate::store::EntryType::ObjectDb)
            ) =>
        {
            object_database::validate_all(tx)
        }
        _ => Ok(()),
    }
}

fn check_entry_delivery(
    access: &Access,
    entry: crate::store::EntryId,
    database: DatabaseId,
    readable: bool,
    bytes: &[u8],
) -> Result<()> {
    if matches!(database, DatabaseId::Entry(id) if id != entry) {
        return Err(Error::AccessDenied("backing is outside selected entry"));
    }
    let policy_keys = crate::store::entry_policy_keys(access.build_id.as_deref())?;
    let account = access.account.to_string();
    let mut rejected = false;
    kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
        let admitted = match database {
            DatabaseId::Entry(id) => readable && id == entry,
            DatabaseId::Revocations => key == account.as_bytes(),
            DatabaseId::Index => {
                (volume_index::source_entry_id(key).is_ok_and(|id| id == entry)
                    && (readable || !volume_index::is_entry_history(key)))
                    || policy_keys.iter().any(|allowed| key == allowed)
            }
        };
        rejected |= !admitted;
    })?;
    if rejected {
        return Err(Error::AccessDenied("delivery is outside selected entry"));
    }
    Ok(())
}

impl LocalStore {
    /// Break owner recovery's control/data dependency using exact local ACL
    /// commits. This does not qualify incoming state or acknowledge an Index.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn restore_owner_service_read(
        &self,
        volume: Id,
        received: &Database,
        publish: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<bool> {
        let identity = self.volume(volume)?;
        if identity.owner != self.account.id
            || !identity.share_key_with_service
            || self.durable_revision(volume, DatabaseId::Index)?.is_none()
        {
            return Ok(false);
        }
        let operations = self.operations(&[volume])?;
        let guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let local_tx = index.begin_read()?;
        let remote_tx = received.begin_read()?;
        // Decode one bounded policy at a time; retain only its actual defaults.
        // The 3-MiB record charge bounds each retained ACL below 200 KiB.
        let local_defaults = {
            let policy =
                MetadataPolicy::read_snapshot(&local_tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
            if !identity.same_identity(policy.volume()) {
                return Err(Error::AccessDenied("owner recovery volume identity mismatch"));
            }
            self.admit_access(&Access::owner(self.account.id), &policy)?;
            policy.permissions
        };
        let remote_defaults = {
            let policy =
                MetadataPolicy::read_snapshot(&remote_tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
            if !identity.same_identity(policy.volume()) {
                return Err(Error::AccessDenied("owner recovery volume identity mismatch"));
            }
            policy.permissions
        };
        let local_index = volume_index::IndexSnapshot::new(local_tx);
        let remote_index = volume_index::IndexSnapshot::new(remote_tx);
        drop(guards);
        self.publish_owner_service_restorations(
            &identity,
            OwnerPermissionSources {
                local_defaults: &local_defaults,
                remote_defaults: &remote_defaults,
                local_index: &local_index,
                remote_index: &remote_index,
            },
            publish,
        )
    }

    #[cfg(feature = "connected-engine")]
    fn publish_owner_service_restorations(
        &self,
        identity: &Volume,
        sources: OwnerPermissionSources<'_, '_, '_>,
        mut publish: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<bool> {
        use crate::store::catalog::PLAYBIT_REPLICA_PRINCIPAL;
        let OwnerPermissionSources { local_defaults, remote_defaults, local_index, remote_index } =
            sources;
        // Existing 8-MiB bootstrap credit: transfer buffers are released before
        // this phase, which keeps 64 headers and one at-most-65-KiB delta. Each
        // entry decode uses the existing 3-MiB allowance, one at a time.
        let mut after = None;
        let mut scanned = 0;
        let mut published = false;
        loop {
            let page = remote_index.source_headers(after, 64)?;
            scanned += page.entries.len();
            if scanned > volume_index::namespace::MAX_ENTRIES {
                return Err(Error::LimitExceeded);
            }
            for entry in
                page.entries.into_iter().filter(|entry| !entry.deleted && entry.protocol.is_some())
            {
                match remote_index.authorize_staged_entry(
                    identity,
                    remote_defaults,
                    PLAYBIT_REPLICA_PRINCIPAL,
                    entry.id,
                    Permission::Read,
                ) {
                    Ok(()) => continue,
                    Err(Error::AccessDenied(_)) => {}
                    Err(error) => return Err(error),
                }
                match local_index.authorize(
                    identity,
                    local_defaults,
                    PLAYBIT_REPLICA_PRINCIPAL,
                    entry.id,
                    Permission::Read,
                ) {
                    Ok(()) => {}
                    Err(Error::AccessDenied(_) | Error::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                }
                let Some(bytes) = local_index.permission_restoration_delta(
                    self.account.id,
                    entry.id,
                    remote_index,
                )?
                else {
                    continue;
                };
                self.require_account_authority()?;
                publish(&bytes)?;
                published = true;
            }
            let Some(next) = page.next else { return Ok(published) };
            after = Some(next);
        }
    }

    /// Pin only this entry's publishable source facts. No catalog Read is
    /// required or granted; the AP independently admits every supplied fact.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn entry_publication_snapshot(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
    ) -> Result<Option<OutboundReplica>> {
        let operations = self.access_operations(access, address.volume)?;
        let guards = operations.lock()?;
        self.check_authority(access, address.volume, Some(address.entry))?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let policy = MetadataPolicy::read(&index)?;
        if self.admit_access(access, &policy)?.is_some_and(|grant| !grant.write) {
            return Ok(None);
        }
        let snapshot = match super::scoped::ScopedSnapshot::for_publication(
            &index,
            &policy,
            address.entry,
            self.data_actor(access, policy.volume())?,
        ) {
            Ok(snapshot) => snapshot,
            Err(Error::AccessDenied(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        drop(guards);
        snapshot.project(&self.path.join(crate::paths::STAGING)).map(Some)
    }

    /// Advisory outbox selection. A remote receiver still admits every source
    /// under its current policy; denied local appends remain pending.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn replica_upload_allowed(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
    ) -> Result<bool> {
        let operations = self.access_operations(access, volume)?;
        let guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        if let DatabaseId::Entry(entry) = database
            && volume_index::IndexSnapshot::new(index.begin_read()?).backing_quarantined(entry)?
        {
            return Ok(false);
        }
        let policy = DatabasePolicy::read(&index, database)?;
        self.check_authority(
            access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        if self.admit_access(access, &policy.volume)?.is_some_and(|grant| !grant.write) {
            return Ok(false);
        }
        if database == DatabaseId::Revocations {
            return Ok(policy.volume.volume().owner == access.account);
        }
        let principal = self.data_actor(access, policy.volume.volume())?;
        for permission in [Permission::Write, Permission::Append] {
            match policy.authorize(principal, permission) {
                Ok(()) => return Ok(true),
                Err(Error::AccessDenied(_)) => {}
                Err(error) => return Err(error),
            }
        }
        if database != DatabaseId::Index {
            return Ok(false);
        }
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        drop(guards);
        let mut after = None;
        loop {
            let page = snapshot.headers(after, 64)?;
            for entry in page.entries.into_iter().filter(|entry| !entry.deleted) {
                for permission in [Permission::Write, Permission::Append] {
                    match snapshot.authorize(
                        policy.volume.volume(),
                        policy.volume.default_permissions(),
                        principal,
                        entry.id,
                        permission,
                    ) {
                        Ok(()) => return Ok(true),
                        Err(Error::AccessDenied(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            let Some(next) = page.next else { return Ok(false) };
            after = Some(next);
        }
    }

    pub fn with_replica<T>(
        &self,
        volume: Id,
        database: DatabaseId,
        f: impl FnOnce(&Database) -> Result<T>,
    ) -> Result<T> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        if matches!(database, DatabaseId::Entry(_)) {
            DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?
                .ordinary_protocol()?;
        }
        let credentials = self.credentials(volume)?;
        let db = self.replica_db(volume, database, Some(credentials.database_key))?;
        f(&db)
    }

    pub fn merge_received_delta(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<kvdb::SyncOutcome> {
        self.receive_delta(access, volume, database, None, bytes, None)
    }

    /// A trusted selected-entry stream cannot publish catalog or unrelated facts.
    pub fn merge_received_entry_delta(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<kvdb::SyncOutcome> {
        self.receive_delta(access, address.volume, database, Some(address.entry), bytes, None)
    }

    /// Authenticated AP acceptance accompanies one exact selected index delta.
    /// Source, retained payload, observation and current control commit together.
    pub fn merge_received_namespace_delta(
        &self,
        access: &Access,
        address: volume_index::EntryAddress,
        bytes: &[u8],
        evidence: &[u8],
    ) -> Result<kvdb::SyncOutcome> {
        volume_index::namespace::live::validate_evidence(evidence)?;
        self.receive_delta(
            access,
            address.volume,
            DatabaseId::Index,
            Some(address.entry),
            bytes,
            Some(evidence),
        )
    }

    fn receive_delta(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        entry: Option<crate::store::EntryId>,
        bytes: &[u8],
        evidence: Option<&[u8]>,
    ) -> Result<kvdb::SyncOutcome> {
        self.require_replication_supported(volume)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read_source(&index, database)?;
        let grant = self.admit_access(access, &policy.volume)?;
        let actor = self.data_actor(access, policy.volume.volume())?;
        policy.ordinary_protocol()?;
        if let Some(entry) = entry {
            let readable = DatabasePolicy::read_source(&index, DatabaseId::Entry(entry))?
                .metadata_readable(actor)?;
            if !readable && grant.is_some_and(|grant| !grant.write) {
                return Err(Error::AccessDenied("personal volume is read-only"));
            }
            check_entry_delivery(access, entry, database, readable, bytes)?;
        } else {
            policy.authorize_reader(ReplicaReader::SessionParticipant(actor))?;
        }
        self.apply_received_delta(volume, database, &policy, bytes, entry.zip(evidence))
    }

    fn apply_received_delta(
        &self,
        volume: Id,
        database: DatabaseId,
        policy: &DatabasePolicy,
        bytes: &[u8],
        namespace: Option<(crate::store::EntryId, &[u8])>,
    ) -> Result<kvdb::SyncOutcome> {
        use kvdb::{Resolver, sync::Resolution};
        let entries = RefCell::new(BTreeSet::new());
        let objects = RefCell::new(BTreeSet::new());
        let rejected = RefCell::new(None);
        let resolver =
            |delta: &kvdb::sync::Delta<'_>| match observe_source(policy, delta, &entries, &objects)
            {
                Ok(()) => kvdb::Lww.resolve(delta),
                Err(error) => {
                    *rejected.borrow_mut() = Some(error);
                    Resolution::KeepLocal
                }
            };
        let name_changed = self.prepare_received_volume_name(volume, database, bytes)?;
        let backing = self.db(volume, database)?;
        let mut receiver = kvdb::DeltaReceiver::new(
            &backing,
            resolver,
            now_unix_secs(),
            crate::store::MAX_REPLICA_DELTA_BYTES,
        )?;
        receiver.process(bytes)?;
        if let Some(error) = rejected.borrow_mut().take() {
            return Err(error);
        }
        if database == DatabaseId::Index
            || matches!(
                policy.entry.as_ref().map(|entry| entry.kind),
                Some(crate::store::EntryType::ObjectDb)
            )
        {
            validate_pending(receiver.pending_transaction()?, database, &objects.borrow())?;
        }
        let ids: Vec<_> = entries.borrow().iter().copied().collect();
        let receiver = if let Some((entry, evidence)) = namespace {
            receiver.qualify_local_indexes(|local| -> Result<bool> {
                let changed = volume_index::namespace::live::apply(local, entry, bytes, evidence)?;
                volume_index::repair(local, &ids)?;
                volume_index::namespace::source::cut::project(local)?.write_current(local)?;
                Ok(changed)
            })?
        } else {
            receiver.repair_local_indexes(|local| -> Result<()> {
                if database == DatabaseId::Index {
                    volume_index::repair(local, &ids)?;
                    volume_index::namespace::source::cut::project(local)?.write_current(local)?;
                }
                Ok(())
            })?
        };
        let outcome = receiver.finish()?;
        if outcome.committed {
            self.changes.observed_database_changed(volume, database);
        }
        if name_changed {
            self.project_volume_name(volume)?;
        }
        Ok(outcome)
    }

    fn prepare_received_volume_name(
        &self,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<bool> {
        let mut name_changed = false;
        if database == DatabaseId::Index {
            kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
                name_changed |= key == VOLUME_KEY
            })?;
            if name_changed {
                self.prepare_volume_name(volume)?;
            }
        }
        Ok(name_changed)
    }

    pub fn outbound_delta(
        &self,
        volume: Id,
        database: DatabaseId,
        reader: ReplicaReader,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        crate::store::project_replica_delta(&policy, reader, bytes)
    }

    /// Local writer publication, as opposed to a trusted replica projection.
    /// The session and every current entry fence are checked under the same cut.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn publication_delta(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        if let DatabaseId::Entry(entry) = database
            && volume_index::IndexSnapshot::new(index.begin_read()?).backing_quarantined(entry)?
        {
            return Err(Error::NotFound("accepted backing is pending"));
        }
        let policy = DatabasePolicy::read(&index, database)?;
        if self.admit_access(access, &policy.volume)?.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("read-only personal selection"));
        }
        let actor = self.data_actor(access, policy.volume.volume())?;
        let entries = crate::store::authorize_publication_delta(&index, &policy, actor, bytes)?;
        if entries.is_empty() {
            self.check_authority(access, volume, None)?;
        }
        for entry in entries {
            self.check_authority(access, volume, Some(entry))?;
        }
        crate::store::project_replica_delta(&policy, ReplicaReader::Service, bytes)
    }

    /// Scan at most 64 source entry IDs. Reserved control databases occur only
    /// on the first page. Sparse pages retain a cursor even without live entries.
    pub fn replication_database_page(
        &self,
        volume: Id,
        after: Option<crate::store::EntryId>,
    ) -> Result<ReplicationDatabasePage> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let mut databases = if after.is_none() {
            vec![DatabaseId::Index, DatabaseId::Revocations]
        } else {
            Vec::new()
        };
        if index.begin_read()?.root_hash()?.is_none() {
            return Ok(ReplicationDatabasePage { databases, next: None });
        }
        MetadataPolicy::read(&index)?;
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read()?);
        let page = snapshot.headers(after, 64)?;
        for entry in page.entries {
            if !entry.deleted && entry.protocol == Some(volume_index::DatabaseProtocol::Kvdb) {
                databases.push(DatabaseId::Entry(entry.id));
            }
        }
        Ok(ReplicationDatabasePage { databases, next: page.next })
    }

    pub fn with_authorized_replica<T>(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        permission: Option<Permission>,
        f: impl FnOnce(&Database, &DatabasePolicy) -> Result<T>,
    ) -> Result<T> {
        self.require_replication_supported(volume)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        self.check_authority(
            access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        let grant = self.admit_access(access, &policy.volume)?;
        if permission.is_some_and(|p| p != Permission::Read) && grant.is_some_and(|g| !g.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        if let Some(permission) = permission {
            policy.authorize(self.data_actor(access, policy.volume.volume())?, permission)?;
        } else if access.session_id.is_none() && access.account != policy.volume.volume.owner {
            return Err(Error::AccessDenied("replica owner or session admission required"));
        }
        policy.ordinary_protocol()?;
        f(&*self.db(volume, database)?, &policy)
    }

    pub fn with_outbound_replica<T>(
        &self,
        volume: Id,
        database: DatabaseId,
        reader: ReplicaReader,
        f: impl FnOnce(&Database) -> Result<T>,
    ) -> Result<T> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        let backing = self.db(volume, database)?;
        let snapshot = OutboundSnapshot::new(&backing)?;
        drop(guards);
        f(snapshot.project(&self.path.join(crate::paths::STAGING), &policy, reader)?.database())
    }

    pub fn projection_root(
        &self,
        volume: Id,
        database: DatabaseId,
        reader: ReplicaReader,
    ) -> Result<ReplicaRoot> {
        self.require_replication_supported(volume)?;
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        let policy = DatabasePolicy::read(&*self.db(volume, DatabaseId::Index)?, database)?;
        projection_root(&*self.db(volume, database)?, &policy, reader)
    }
}

impl LocalStore {
    /// The trusted transport derives a scoped key without creating a backing.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn replica_hash_key(&self, volume: Id, database: DatabaseId) -> Result<[u8; 32]> {
        Ok(database.hash_key(&self.credentials(volume)?.database_key, volume))
    }

    /// A peer is an authenticated relayer. Current Write permits forwarding
    /// accepted data; an Append-only relayer may introduce only its own births.
    /// Editor stamps in forwarded records are not authenticated authorship.
    fn authorize_replica_merge(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        policy: &DatabasePolicy,
    ) -> Result<()> {
        self.check_authority(
            access,
            volume,
            match database {
                DatabaseId::Entry(entry) => Some(entry),
                _ => None,
            },
        )?;
        let grant = self.admit_access(access, &policy.volume)?;
        if grant.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        policy.ordinary_protocol()?;
        Ok(())
    }

    pub fn merge_authorized_replica(
        &self,
        access: &Access,
        volume: Id,
        database: DatabaseId,
        source: &Database,
    ) -> Result<()> {
        use kvdb::sync::{Lww, Resolution, SyncResolver};
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let policy = DatabasePolicy::read_source(&index, database)?;
        self.authorize_replica_merge(access, volume, database, &policy)?;
        let actor = self.data_actor(access, policy.volume.volume())?;
        let append = append_relayer(&policy, actor)?;
        let before = volume_index::IndexSnapshot::new(index.begin_read()?);
        let rejected = RefCell::new(None);
        let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
            let admitted = if database == DatabaseId::Index && volume_index::is_index_key(delta.key)
            {
                volume_index::with_sync_delta(delta, |source| {
                    before.authorize_source(
                        &policy.volume.volume,
                        policy.volume.default_permissions(),
                        actor,
                        source,
                    )
                })
            } else {
                // Reserved policy writes always require the authenticated owner.
                let writer = (append || !matches!(database, DatabaseId::Entry(_))).then_some(actor);
                policy.validate_sync_source_as(delta, writer).map(|_| ())
            };
            if let Err(error) = admitted {
                *rejected.borrow_mut() = Some(error);
                return Ok(Resolution::KeepLocal);
            }
            Lww.resolve(delta)
        };
        let target = self.db(volume, database)?;
        let validate = |tx: &kvdb::WriteTxn<'_, RedbStorage>| {
            rejected.borrow_mut().take().map_or(Ok(()), Err)?;
            validate_replica(tx, &policy)
        };
        let scratch = self.path.join(crate::paths::STAGING);
        if database == DatabaseId::Index {
            // This resolver admits the authenticated relayer and validates every
            // index source. Publish qualified controls in that same transaction;
            // detached copies through merge_index_replica remain source-only.
            merge_sender(
                &scratch,
                kvdb::sync::SyncSender::new(source)?,
                &target,
                resolver,
                validate,
                |local| {
                    volume_index::repair_all(local)?;
                    volume_index::namespace::source::cut::project(local)?.write_current(local)
                },
            )?;
        } else {
            merge_replica(&scratch, source, &target, resolver, validate)?;
        }
        Ok(())
    }
}
