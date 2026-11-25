//! Bounded committed changes and local application of complete AP deltas.
use super::*;
use crate::store::{DatabasePolicy, ReplicaReader, object_database, volume_index};
use kvdb::sync::{Resolution, Resolver};
use kvdb::{CommittedDelta, DeltaReceiver};
use std::collections::BTreeSet;
use std::collections::VecDeque;
const MAX_DELTA: usize = 65536;
const _: () = assert!(41 + MAX_DELTA <= LIMIT);
const MAX_QUEUED: usize = 1024 * 1024;
struct Change {
    ticket: u64,
    volume: Id,
    database: crate::store::DatabaseId,
    bytes: Vec<u8>,
}
#[derive(Default)]
pub(super) struct Changes {
    queue: VecDeque<Change>,
    bytes: usize,
    gap: bool,
    issued: u64,
}
impl Changes {
    pub(super) fn discard_volume(&mut self, volume: Id) {
        self.queue.retain(|change| {
            if change.volume != volume {
                return true;
            }
            self.bytes -= change.bytes.len();
            false
        });
    }

    pub(super) fn retire_backings(&mut self, volume: Id, entries: &BTreeSet<EntryId>) {
        for change in &mut self.queue {
            if change.volume == volume
                && matches!(change.database, DatabaseId::Entry(entry) if entries.contains(&entry))
            {
                self.bytes -= change.bytes.len();
                change.bytes.clear();
            }
        }
    }
    fn invalidate(&mut self) {
        self.queue.clear();
        self.bytes = 0;
        self.gap = true;
    }
    pub(super) fn push(
        &mut self,
        volume: Id,
        database: crate::store::DatabaseId,
        delta: CommittedDelta,
    ) {
        let CommittedDelta::Bytes(bytes) = delta else {
            self.invalidate();
            return;
        };
        self.push_bytes(volume, database, bytes);
    }
    fn push_bytes(&mut self, volume: Id, database: DatabaseId, bytes: Vec<u8>) {
        if self.gap {
            return;
        }
        if self.queue.len() >= 64
            || bytes.len() > MAX_DELTA
            || self.bytes + bytes.len() > MAX_QUEUED
        {
            self.invalidate();
            return;
        }
        let Some(ticket) = self.issued.checked_add(1) else {
            self.invalidate();
            return;
        };
        self.issued = ticket;
        self.bytes += bytes.len();
        self.queue.push_back(Change { ticket, volume, database, bytes });
    }
}

fn validate_pending(
    tx: &kvdb::WriteTxn<'_, OpfsStorage>,
    policy: &DatabasePolicy,
    objects: &BTreeSet<Id>,
) -> crate::store::Result<()> {
    match policy.database {
        DatabaseId::Index => {
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        }
        DatabaseId::Entry(_)
            if matches!(
                policy.entry.as_ref().map(|e| e.kind),
                Some(crate::store::EntryType::ObjectDb)
            ) =>
        {
            object_database::validate_pending(tx, objects)
        }
        _ => Ok(()),
    }
}

pub(super) fn validate_snapshot(
    tx: &kvdb::WriteTxn<'_, OpfsStorage>,
    policy: &DatabasePolicy,
) -> crate::store::Result<()> {
    match policy.database {
        DatabaseId::Index => {
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        }
        DatabaseId::Entry(_)
            if matches!(
                policy.entry.as_ref().map(|e| e.kind),
                Some(crate::store::EntryType::ObjectDb)
            ) =>
        {
            object_database::validate_all(tx)
        }
        _ => Ok(()),
    }
}

struct LiveFrame<'a> {
    host: &'a Host,
    policy: &'a DatabasePolicy,
    entry_scope: Option<EntryId>,
    entries: RefCell<BTreeSet<EntryId>>,
    objects: RefCell<BTreeSet<Id>>,
}
impl LiveFrame<'_> {
    fn validate(&self, delta: &kvdb::sync::Delta<'_>) -> crate::store::Result<()> {
        let database = self.policy.database;
        if let Some(entry) = self.entry_scope {
            selected_key(self.host, database, entry, delta.key)?;
        }
        if database == DatabaseId::Index && volume_index::is_index_key(delta.key) {
            let id = volume_index::validate_source(delta)?;
            let mut ids = self.entries.borrow_mut();
            if ids.len() == 64 && !ids.contains(&id) {
                return Err(crate::store::Error::LimitExceeded);
            }
            ids.insert(id);
        } else if let Some(id) = self.policy.validate_source(delta, None)? {
            let mut ids = self.objects.borrow_mut();
            if ids.len() == 64 && !ids.contains(&id) {
                return Err(crate::store::Error::LimitExceeded);
            }
            ids.insert(id);
        }
        Ok(())
    }
}
impl Host {
    pub(super) fn committed_change(&self, volume: Id, database: DatabaseId, delta: CommittedDelta) {
        if self.replication_allowed(volume) {
            self.changes.borrow_mut().push(volume, database, delta);
        }
    }

    pub(super) fn replication_allowed(&self, volume: Id) -> bool {
        use crate::package::AutomaticVolumeRole as Role;
        if self.deleted_user_volume(volume) {
            return false;
        }
        match self.automatic_role(volume) {
            Some(Role::Root) | None => true,
            Some(Role::Session) => self.session.borrow().shared_volume == Some(volume),
            Some(Role::User) => self.user_volume == Some(volume),
            Some(Role::UserSession | Role::Local | Role::LocalSession) => false,
        }
    }

    pub(super) fn replica_policy(
        &self,
        volume: &Volume,
        database: DatabaseId,
        initialize: bool,
    ) -> Result<DatabasePolicy> {
        if !self.replication_allowed(volume.id) {
            return Err(Error::AccessDenied);
        }
        let volumes = self.volumes.borrow();
        let empty =
            volume.index.begin_read().map_err(map_error)?.root_hash().map_err(map_error)?.is_none();
        let preparation = initialize
            && volume.record.owner == self.account
            && self.personal.borrow().iter().any(|v| v.id == volume.id);
        if initialize
            && database == DatabaseId::Index
            && empty
            && (volumes.iter().any(|v| v.id == volume.id)
                || preparation
                || self.automatic_role(volume.id)
                    == Some(crate::package::AutomaticVolumeRole::Root))
        {
            if preparation {
                self.authorize(&volumes[0], None, Permission::Read)?;
            }
            return DatabasePolicy::reserved(
                MetadataPolicy::new(volume.record.clone(), Acl::default()).map_err(store_error)?,
                database,
            )
            .map_err(store_error);
        }
        if preparation {
            self.authorize(&volumes[0], None, Permission::Read)?;
            if volume
                .revocations
                .begin_read()
                .map_err(map_error)?
                .has(self.account.to_string().as_bytes())
                .map_err(map_error)?
            {
                return Err(Error::AccessDenied);
            }
        } else {
            self.authorize(
                volume,
                match database {
                    DatabaseId::Entry(id) => Some(id),
                    _ => None,
                },
                Permission::Read,
            )?;
        }
        let policy = if preparation {
            DatabasePolicy::read_source(&volume.index, database)
        } else {
            DatabasePolicy::read(&volume.index, database)
        }
        .map_err(map_error)?;
        if !policy.volume.volume().same_identity(&volume.record) {
            return Err(Error::AccessDenied);
        }
        policy.ordinary_protocol().map_err(map_error)?;
        policy
            .authorize_reader(ReplicaReader::SessionParticipant(self.actor(volume)))
            .map_err(map_error)?;
        Ok(policy)
    }
    fn receive_delta(
        &self,
        volume: &Volume,
        database: DatabaseId,
        bytes: &[u8],
        entry_scope: Option<EntryId>,
        evidence: Option<&[u8]>,
    ) -> Result<()> {
        if let Some(entry) = entry_scope {
            let readable = self.authorize_entry_metadata(volume, entry)?;
            if !readable {
                if matches!(database, DatabaseId::Entry(_)) {
                    return Err(Error::AccessDenied);
                }
                let mut history = false;
                kvdb::sync::visit_delta(bytes, MAX_DELTA, |key, _| {
                    history |= database == DatabaseId::Index && volume_index::is_entry_history(key);
                })
                .map_err(map_error)?;
                if history {
                    return Err(Error::AccessDenied);
                }
            }
            DatabasePolicy::read(&volume.index, DatabaseId::Entry(entry))
                .map_err(store_error)?
                .authorize_reader(ReplicaReader::Service)
                .map_err(store_error)?;
            if matches!(database, DatabaseId::Entry(id) if id != entry) {
                return Err(Error::AccessDenied);
            }
        }
        if let DatabaseId::Entry(_) = database {
            let entry =
                crate::store::DatabasePolicy::read(&volume.index, database).map_err(store_error)?;
            if entry.entry.as_ref().is_some_and(|entry| {
                entry.protocol == Some(volume_index::DatabaseProtocol::BlobLog)
            }) {
                return bloblog::receive_inline(
                    self,
                    &databases::Address { volume: volume.id, database },
                    bytes,
                );
            }
        }
        self.receive_ordinary_delta(volume, database, bytes, entry_scope, evidence)
    }
    fn receive_ordinary_delta(
        &self,
        volume: &Volume,
        database: DatabaseId,
        bytes: &[u8],
        entry_scope: Option<EntryId>,
        evidence: Option<&[u8]>,
    ) -> Result<()> {
        let policy = if entry_scope.is_some() {
            DatabasePolicy::read(&volume.index, database).map_err(store_error)?
        } else {
            self.replica_policy(volume, database, false)?
        };
        let db = match database {
            DatabaseId::Index => volume.index.clone(),
            DatabaseId::Revocations => volume.revocations.clone(),
            DatabaseId::Entry(id) => self.entry_database(volume, id)?,
        };
        let frame = LiveFrame {
            host: self,
            policy: &policy,
            entry_scope,
            entries: RefCell::new(BTreeSet::new()),
            objects: RefCell::new(BTreeSet::new()),
        };
        let rejected = RefCell::new(None);
        let resolver = |delta: &kvdb::sync::Delta<'_>| match frame.validate(delta) {
            Ok(()) => Lww.resolve(delta),
            Err(error) => {
                *rejected.borrow_mut() = Some(error);
                Resolution::KeepLocal
            }
        };
        let mut receiver = DeltaReceiver::new(&db, resolver, unsafe { now() } as u64, MAX_DELTA)
            .map_err(map_error)?;
        receiver.process(bytes).map_err(map_error)?;
        if let Some(error) = rejected.borrow_mut().take() {
            return Err(map_error(error));
        }
        validate_pending(
            receiver.pending_transaction().map_err(map_error)?,
            &policy,
            &frame.objects.borrow(),
        )
        .map_err(map_error)?;
        let ids: Vec<_> = frame.entries.borrow().iter().copied().collect();
        let receiver = if let Some((entry, evidence)) = entry_scope.zip(evidence) {
            receiver
                .qualify_local_indexes(|local| -> crate::store::Result<bool> {
                    let changed =
                        volume_index::namespace::live::apply(local, entry, bytes, evidence)?;
                    volume_index::repair(local, &ids)?;
                    volume_index::namespace::source::cut::project(local)?.write_current(local)?;
                    Ok(changed)
                })
                .map_err(map_error)?
        } else {
            receiver
                .repair_local_indexes(|local| -> crate::store::Result<()> {
                    if database == DatabaseId::Index {
                        volume_index::repair(local, &ids)?;
                        volume_index::namespace::source::cut::project(local)?
                            .write_current(local)?;
                    }
                    Ok(())
                })
                .map_err(map_error)?
        };
        receiver.finish().map(|_| ()).map_err(map_error)
    }
    pub(super) fn merge_prepared(
        &self,
        address: &databases::Address,
        db: &Database,
        changes: kvdb::sync::PreparedChanges<OpfsStaging>,
    ) -> Result<()> {
        let volume = self.replica_volume(address.volume)?;
        let policy = self.replica_policy(&volume, address.database, true)?;
        self.merge_prepared_in(&volume, address, db, &changes, &policy, None)
    }
    pub(super) fn merge_prepared_in(
        &self,
        volume: &Volume,
        address: &databases::Address,
        db: &Database,
        changes: &kvdb::sync::PreparedChanges<OpfsStaging>,
        policy: &DatabasePolicy,
        entry_scope: Option<EntryId>,
    ) -> Result<()> {
        use kvdb::sync::SyncResolver;
        let rejected = RefCell::new(None);
        let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
            if !matches!(delta.remote, kvdb::sync::SyncEntState::Missing) {
                let admitted = if address.database == DatabaseId::Index
                    && volume_index::is_index_key(delta.key)
                {
                    volume_index::validate_sync_source(delta).and_then(|entry| {
                        if entry_scope.is_none_or(|selected| selected == entry) {
                            Ok(())
                        } else {
                            Err(crate::store::Error::AccessDenied("source outside selected entry"))
                        }
                    })
                } else {
                    policy.validate_sync_source(delta, None).and_then(|_| {
                        if let Some(entry) = entry_scope {
                            selected_key(self, address.database, entry, delta.key)?;
                        }
                        Ok(())
                    })
                };
                if let Err(error) = admitted {
                    *rejected.borrow_mut() = Some(error);
                    return Ok(Resolution::KeepLocal);
                }
            }
            SyncResolver::resolve(&mut Lww, delta)
        };
        let merged = changes.merge(db, resolver, unsafe { now() } as u64, 0).map_err(map_error)?;
        if let Some(error) = rejected.borrow_mut().take() {
            return Err(map_error(error));
        }
        validate_snapshot(merged.pending_transaction().map_err(map_error)?, policy)
            .map_err(store_error)?;
        merged
            .repair_local_indexes(|local| -> crate::store::Result<()> {
                if address.database == DatabaseId::Index {
                    match entry_scope {
                        Some(entry) => volume_index::repair(local, &[entry])?,
                        None => volume_index::repair_all(local)?,
                    }
                    // This is the authorized receive boundary. Publish controls
                    // from the validated causal cut in the same transaction;
                    // generic index repair alone cannot expose received births.
                    volume_index::namespace::source::cut::project(local)?.write_current(local)?;
                }
                Ok(())
            })
            .map_err(map_error)?
            .finish_with_delta()
            .map_err(map_error)?;
        if let DatabaseId::Entry(id) = address.database {
            let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
            volume_index::mark_source_backing_ready(&mut tx, id).map_err(map_error)?;
            tx.commit().map_err(map_error)?;
        }
        Ok(())
    }
}

pub(super) fn selected_key(
    host: &Host,
    database: DatabaseId,
    entry: EntryId,
    key: &[u8],
) -> crate::store::Result<()> {
    let admitted = match database {
        DatabaseId::Entry(id) => id == entry,
        DatabaseId::Revocations => key == host.account.to_string().as_bytes(),
        DatabaseId::Index => {
            volume_index::source_entry_id(key).is_ok_and(|id| id == entry)
                || crate::store::entry_policy_keys(Some(&host.session.borrow().resolved_build_id))?
                    .iter()
                    .any(|allowed| key == allowed)
        }
    };
    if admitted {
        Ok(())
    } else {
        Err(crate::store::Error::AccessDenied("source outside selected entry"))
    }
}

// Worker framing uses full volume/database identities and a nonreusing ticket.
#[unsafe(no_mangle)]
pub extern "C" fn pb_live_peek() -> i32 {
    crate::platform::failure::guard();
    let result = HOST.with(|cell| {
        let host = cell.borrow().clone().ok_or(Error::Unavailable)?;
        let changes = host.changes.borrow();
        if changes.gap {
            return Err(map_error("local changes require gap recovery"));
        }
        let Some(change) = changes.queue.front() else {
            return Ok(0);
        };
        let volume = host.replica_volume(change.volume)?;
        let admission = host.admit_volume(&volume, Permission::Write).map(drop);
        let projected = host.is_projected_world(&volume);
        if !projected {
            admission?;
        }
        let policy = DatabasePolicy::read(&volume.index, change.database);
        let (bytes, catch_up) = match policy {
            Ok(policy) => match admission
                .and_then(|()| outbound_change(&host, &volume, &policy, &change.bytes))
            {
                Err(Error::AccessDenied | Error::Unavailable)
                    if projected
                        && policy
                            .entry
                            .as_ref()
                            .is_some_and(|entry| entry.kind == crate::store::EntryType::KvDb) =>
                {
                    // Retain the backing and append births. Reconcile this source
                    // against AP without blocking the next Shared publication.
                    (Vec::new(), true)
                }
                result => (result?, false),
            },
            Err(crate::store::Error::NotFound(_))
                if matches!(change.database, DatabaseId::Entry(_)) =>
            {
                admission?;
                (Vec::new(), false)
            }
            Err(error) => return Err(map_error(error)),
        };
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        output.extend_from_slice(change.volume.as_bytes());
        output.extend_from_slice(&change.database.to_bytes());
        output.extend_from_slice(&change.ticket.to_le_bytes());
        output.push(u8::from(catch_up));
        output.extend_from_slice(&bytes);
        assert!(output.len() <= 41 + MAX_DELTA, "live framing preserves the delta budget");
        Ok(output.len())
    });
    result.map_or_else(|error| error as i32, |length| length as i32)
}

fn outbound_change(
    host: &Host,
    volume: &Volume,
    policy: &DatabasePolicy,
    bytes: &[u8],
) -> Result<Vec<u8>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if let DatabaseId::Entry(entry) = policy.database
        && volume_index::IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?)
            .backing_quarantined(entry)
            .map_err(store_error)?
    {
        return Err(Error::Unavailable);
    }
    // A qualified capture may have retired rejected source facts since this
    // queue record was committed. Never replay them after a later grant.
    let projected;
    let bytes = if policy.database == DatabaseId::Index {
        let read = volume.index.begin_read().map_err(map_error)?;
        let mut retained = BTreeSet::new();
        let mut failure = None;
        kvdb::sync::visit_delta(bytes, MAX_DELTA, |key, remote| {
            if !volume_index::is_index_key(key) {
                retained.insert(key);
                return;
            }
            match read.get_with_clock(key) {
                Ok(Some((clock, value))) => {
                    if matches!(remote, kvdb::sync::EntState::Present { clock: other, value: bytes }
                        if other == clock && bytes == value.as_ref())
                    {
                        retained.insert(key);
                    }
                }
                Ok(None) => {}
                Err(error) => failure = Some(map_error(error)),
            }
        })
        .map_err(map_error)?;
        if let Some(error) = failure {
            return Err(error);
        }
        if retained.is_empty() {
            return Ok(Vec::new());
        }
        projected = kvdb::sync::project_delta(bytes, MAX_DELTA, |key| retained.contains(key))
            .map_err(map_error)?;
        &projected
    } else {
        bytes
    };
    let entries =
        crate::store::authorize_publication_delta(&volume.index, policy, host.actor(volume), bytes)
            .map_err(store_error)?;
    if entries.is_empty() {
        authority::check(host, volume, None)?;
    }
    for entry in entries {
        authority::check(host, volume, Some(entry))?;
    }
    match crate::store::project_replica_delta(policy, ReplicaReader::Service, bytes) {
        Ok(bytes) => Ok(bytes),
        Err(crate::store::Error::AccessDenied(_)) => Ok(Vec::new()),
        Err(error) => Err(store_error(error)),
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_live_ack(ticket: u64) -> i32 {
    crate::platform::failure::guard();
    HOST.with(|cell| {
        let host = cell.borrow();
        let Some(host) = host.as_ref() else {
            return Error::Unavailable as i32;
        };
        let mut changes = host.changes.borrow_mut();
        if changes.gap {
            return map_error("local changes require gap recovery") as i32;
        }
        if changes.queue.front().map(|change| change.ticket) != Some(ticket) {
            return Error::InvalidArgument as i32;
        }
        let Some(change) = changes.queue.pop_front() else {
            return Error::InvalidArgument as i32;
        };
        changes.bytes -= change.bytes.len();
        0
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_live_rebase(gap_only: u32) -> i32 {
    crate::platform::failure::guard();
    HOST.with(|cell| {
        let host = cell.borrow();
        let Some(host) = host.as_ref() else {
            return Error::Unavailable as i32;
        };
        let mut changes = host.changes.borrow_mut();
        if gap_only != 0 && !changes.gap {
            return 0;
        }
        changes.queue.clear();
        changes.bytes = 0;
        changes.gap = false;
        0
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_live_apply(length: usize) -> i32 {
    apply(length, false, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_live_apply_entry(length: usize) -> i32 {
    apply(length, true, false)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_live_apply_namespace(length: usize) -> i32 {
    apply(length, true, true)
}

fn apply(length: usize, selected: bool, namespace: bool) -> i32 {
    crate::platform::failure::guard();
    let prefix = if selected { 47 } else { 32 };
    if !(prefix + 1..=prefix + MAX_DELTA).contains(&length) {
        return Error::InvalidArgument as i32;
    }
    let input = match admit_input(length, prefix + MAX_DELTA) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let volume = Id::from_bytes(input[..16].try_into().expect("admitted volume identity width"));
    let entry = if selected {
        match EntryId::from_bytes(input[16..31].try_into().expect("admitted entry identity width"))
        {
            Ok(entry) => Some(entry),
            Err(error) => return store_error(error) as i32,
        }
    } else {
        None
    };
    let offset = prefix - 16;
    let database = match DatabaseId::from_bytes(
        input[offset..prefix].try_into().expect("admitted database identity width"),
    ) {
        Ok(id) => id,
        Err(error) => return map_error(error) as i32,
    };
    HOST.with(|cell| {
        let host = cell.borrow();
        host.as_ref()
            .ok_or(Error::Unavailable)
            .and_then(|host| {
                let volume = host.replica_volume(volume)?;
                let (delta, evidence) = if namespace {
                    if database != DatabaseId::Index {
                        return Err(Error::InvalidArgument);
                    }
                    let (delta, evidence) =
                        volume_index::namespace::live::decode_payload(&input[prefix..length])
                            .map_err(|_| Error::InvalidArgument)?;
                    (delta, Some(evidence))
                } else {
                    (&input[prefix..length], None)
                };
                host.receive_delta(&volume, database, delta, entry, evidence)
            })
            .map_or_else(|error| error as i32, |()| 0)
    })
}
