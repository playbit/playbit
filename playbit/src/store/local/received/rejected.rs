//! Qualified AP cuts retain rejected local state before canonical publication.
use super::*;
use crate::store::object_contract::Principal;
use crate::store::volume_index::{EntryChange, EntryKind, IndexSnapshot};
use std::io::{Read, Write};

const MAX_ENTRIES: usize = 128;
const MAX_CUTS: usize = 32;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILES: usize = 256;
const RECORD_LIMIT: usize = 64 * 1024;
// Opening a redb copy may resize its free-page bookkeeping. Charge that space
// before publication and check the actual aggregate after closing each handle.
const DATABASE_OPEN_RESERVE: u64 = 1024 * 1024;
const PREFIX: &[u8] = b"rejected-cut/";
const MANIFEST: &str = "rejected.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(in crate::store::local) struct RejectedCut {
    pub operation: Id,
    pub volume: Id,
    pub entries: Vec<EntryId>,
    pub bytes: u64,
    pub(in crate::store::local) files: BTreeMap<String, [u8; 32]>,
}

pub(in crate::store::local) fn key(operation: Id) -> Vec<u8> {
    [PREFIX, operation.as_bytes()].concat()
}

pub(in crate::store::local) fn cuts(index: &Database) -> Result<Vec<RejectedCut>> {
    let read = index.begin_read()?;
    let mut result = Vec::new();
    let mut bytes = 0u64;
    for item in read.local_iter_from(PREFIX)? {
        let (key, value) = item?;
        if !key.starts_with(PREFIX) {
            break;
        }
        if result.len() == MAX_CUTS || value.len() > RECORD_LIMIT {
            return Err(Error::LimitExceeded);
        }
        let cut: RejectedCut = decode(&value)?;
        cut.validate()?;
        if key.as_ref() != self::key(cut.operation) {
            return Err(Error::corrupt("rejected cut operation differs from its key"));
        }
        bytes = bytes.checked_add(cut.bytes).ok_or(Error::LimitExceeded)?;
        if bytes > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        result.push(cut);
    }
    Ok(result)
}

impl RejectedCut {
    pub(in crate::store::local) fn verify_index(&self, index: &Database) -> Result<()> {
        if self.files.get("index.kvdb") != Some(&database_digest(index)?) {
            return Err(Error::corrupt("rejected cut index digest differs from its manifest"));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.operation.as_bytes() == &[0; 16]
            || self.volume.as_bytes() == &[0; 16]
            || self.entries.is_empty()
            || self.entries.len() > MAX_ENTRIES
            || self.entries.windows(2).any(|pair| pair[0] >= pair[1])
            || self.bytes > MAX_BYTES
            || self.files.len() > MAX_FILES
            || !self.files.contains_key("index.kvdb")
        {
            return Err(Error::corrupt(
                "rejected cut manifest exceeds its limits or lacks an index",
            ));
        }
        for name in self.files.keys() {
            if name.is_empty()
                || name.len() > 128
                || name.contains('/')
                || name.contains('\\')
                || name == "."
                || name == ".."
            {
                return Err(Error::corrupt("rejected cut contains an invalid file name"));
            }
        }
        Ok(())
    }

    pub(super) fn read(directory: &Path) -> Result<Option<Self>> {
        let path = directory.join(MANIFEST);
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(error)),
        };
        if file.metadata().map_err(io)?.len() > RECORD_LIMIT as u64 {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        let cut: Self = decode(&bytes)?;
        cut.validate()?;
        Ok(Some(cut))
    }

    pub(in crate::store::local) fn verify(&self, directory: &Path) -> Result<()> {
        self.validate()?;
        let mut remaining = self
            .bytes
            .checked_sub(RECORD_LIMIT as u64)
            .ok_or(Error::corrupt("rejected cut byte budget is smaller than its manifest"))?;
        for (name, expected) in &self.files {
            let path = directory.join("rejected").join(name);
            if name.ends_with(".kvdb") {
                if database_digest(&open_replica(&path, 512 * 1024)?)? != *expected {
                    return Err(Error::corrupt(
                        "rejected cut database digest differs from its manifest",
                    ));
                }
                remaining = remaining
                    .checked_sub(fs::metadata(&path).map_err(io)?.len())
                    .ok_or(Error::LimitExceeded)?;
                continue;
            }
            let mut file = fs::File::open(&path).map_err(io)?;
            remaining = remaining
                .checked_sub(file.metadata().map_err(io)?.len())
                .ok_or(Error::LimitExceeded)?;
            let mut hash = blake3::Hasher::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).map_err(io)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            if hash.finalize().as_bytes() != expected {
                return Err(Error::corrupt("rejected cut file digest differs from its manifest"));
            }
        }
        Ok(())
    }
}

impl LocalStore {
    /// The caller holds the publication gate. No live source is modified here.
    pub(super) fn prepare_rejected_cut(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
    ) -> Result<()> {
        if !stage.authority_qualified {
            return Ok(());
        }
        if let Some(cut) = RejectedCut::read(&stage._directory.0)? {
            if cut.volume != stage.volume || cut.operation != stage.operation {
                return Err(Error::corrupt(
                    "rejected cut does not match its staged volume or operation",
                ));
            }
            return cut.verify(&stage._directory.0);
        }
        let index = self.db(stage.volume, DatabaseId::Index)?;
        let entries = self.find_rejected_entries(access, stage, &index)?;
        if entries.is_empty() {
            return Ok(());
        }
        let retained = cuts(&index)?;
        if retained.len() == MAX_CUTS {
            return Err(Error::LimitExceeded);
        }
        let used = retained
            .iter()
            .try_fold(0u64, |sum, cut| sum.checked_add(cut.bytes).ok_or(Error::LimitExceeded))?;
        let available = MAX_BYTES.checked_sub(used).ok_or(Error::LimitExceeded)?;
        let archive = stage._directory.0.join("rejected");
        match fs::remove_dir_all(&archive) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        fs::DirBuilder::new().mode(0o700).create(&archive).map_err(io)?;
        let mut cut = RejectedCut {
            operation: stage.operation,
            volume: stage.volume,
            entries,
            bytes: 0,
            files: BTreeMap::new(),
        };
        self.copy_rejected_backings(&mut cut, stage, &index, available)?;
        // Replay files are transient; Drop removes them after publication while
        // keeping this archive. Reserve the complete manifest budget as well.
        cut.bytes = cut.bytes.checked_add(RECORD_LIMIT as u64).ok_or(Error::LimitExceeded)?;
        if cut.bytes > available {
            return Err(Error::LimitExceeded);
        }
        cut.validate()?;
        let bytes = encode(&cut)?;
        if bytes.len() > RECORD_LIMIT {
            return Err(Error::LimitExceeded);
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage._directory.0.join(MANIFEST))
            .map_err(io)?;
        file.write_all(&bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        sync_directory(&archive)?;
        sync_directory(&stage._directory.0)
    }
    fn find_rejected_entries(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
        index: &Database,
    ) -> Result<Vec<EntryId>> {
        let accepted = MetadataPolicy::read(&stage.metadata)?;
        let current = index.begin_read()?;
        let mut entries = Vec::new();
        let mut after = None;
        loop {
            let page = IndexSnapshot::new(index.begin_read()?).source_headers(after, 64)?;
            for entry in page.entries {
                if stage.entry_scope.is_some_and(|scope| scope != entry.id) {
                    continue;
                }
                let rejected = volume_index::rejected_sources(
                    &current,
                    &stage.metadata,
                    &accepted,
                    access.account,
                    entry.id,
                )?;
                let backing_differs =
                    self.rejected_entry_backing(access, stage, &accepted, entry.id)?;
                if rejected || backing_differs {
                    if entries.len() == MAX_ENTRIES {
                        return Err(Error::LimitExceeded);
                    }
                    entries.push(entry.id);
                }
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        Ok(entries)
    }

    fn rejected_entry_backing(
        &self,
        access: &Access,
        stage: &ReceivedVolume,
        accepted: &MetadataPolicy,
        entry: EntryId,
    ) -> Result<bool> {
        let index = self.db(stage.volume, DatabaseId::Index)?;
        if IndexSnapshot::new(index.begin_read()?).backing_quarantined(entry)? {
            return Ok(false);
        }
        if retains_log_appends(&stage.metadata, access.account, entry)? {
            return Ok(false);
        }
        let writable = IndexSnapshot::new(stage.metadata.begin_read()?).authorize_staged_entry(
            accepted.volume(),
            accepted.default_permissions(),
            Principal::Human(access.account),
            entry,
            Permission::Write,
        );
        let denied = match writable {
            Ok(()) => false,
            Err(Error::AccessDenied(_) | Error::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        let backing = DatabaseId::Entry(entry);
        let local_path = self
            .path
            .join(crate::paths::VOLUMES)
            .join(stage.volume.to_string())
            .join(backing.file_name());
        let received_path = stage._directory.0.join(backing.file_name());
        if denied && local_path.is_file() && !received_path.is_file() {
            return Ok(true);
        }
        Ok(if denied && local_path.is_file() && received_path.is_file() {
            let local = self.entry_backing(stage.volume, entry, false)?;
            stage.with_database(backing, |remote| {
                let policy = DatabasePolicy::read_source(&stage.metadata, backing)?;
                if policy.entry.as_ref().and_then(|entry| entry.protocol)
                    == Some(volume_index::DatabaseProtocol::BlobLog)
                {
                    let read = local.begin_read()?;
                    return Ok(read
                        .iter_from(b"p/")
                        .next()
                        .transpose()?
                        .is_some_and(|(key, _)| key.starts_with(b"p/")));
                }
                rejected_backing(
                    &self.path.join(crate::paths::STAGING),
                    remote,
                    &local,
                    &policy,
                    access.account,
                )
            })?
        } else {
            false
        })
    }

    fn copy_rejected_backings(
        &self,
        cut: &mut RejectedCut,
        stage: &ReceivedVolume,
        index: &Database,
        available: u64,
    ) -> Result<()> {
        let archive = stage._directory.0.join("rejected");
        let volume = self.path.join(crate::paths::VOLUMES).join(stage.volume.to_string());
        copy(
            cut,
            &archive,
            &volume.join(DatabaseId::Index.file_name()),
            "index.kvdb",
            available,
            Some(database_digest(index)?),
        )?;
        for entry in cut.entries.clone() {
            let database = DatabaseId::Entry(entry);
            let path = volume.join(database.file_name());
            if path.is_file() {
                let source = self.entry_backing(stage.volume, entry, false)?;
                copy(
                    cut,
                    &archive,
                    &path,
                    &database.file_name(),
                    available,
                    Some(database_digest(&source)?),
                )?;
            }
            self.copy_rejected_blobs(cut, &archive, index, entry, available)?;
        }
        Ok(())
    }

    fn copy_rejected_blobs(
        &self,
        cut: &mut RejectedCut,
        archive: &Path,
        index: &Database,
        entry: EntryId,
        available: u64,
    ) -> Result<()> {
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        let mut cursor = None;
        loop {
            let page = snapshot.revisions(entry, cursor, 64)?;
            for revision in page.revisions {
                let kind = match revision.change {
                    EntryChange::Created { info } => Some(info.kind),
                    EntryChange::Content { kind } => Some(kind),
                    _ => None,
                };
                if let Some(EntryKind::Blob { digest, .. }) = kind {
                    let name = format!("{}.blob", hex(&digest));
                    if cut.files.contains_key(&name) {
                        continue;
                    }
                    let _cache = self
                        .blob_cache
                        .lock()
                        .map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
                    let path = [self.blob_path(&digest, false), self.blob_path(&digest, true)]
                        .into_iter()
                        .find(|path| path.is_file());
                    if let Some(path) = path {
                        copy(cut, archive, &path, &name, available, None)?;
                    }
                }
            }
            let Some(next) = page.next else { break };
            cursor = Some(next);
        }
        Ok(())
    }
}

pub(super) fn merge_backing(
    scratch: &Path,
    source: &Database,
    target: &Database,
    policy: &DatabasePolicy,
    account: Id,
    replace_all: bool,
) -> Result<()> {
    let failure = std::cell::RefCell::new(None);
    merge_replica(
        scratch,
        source,
        target,
        |delta: &kvdb::sync::SyncDelta<'_>| {
            if replace_all {
                Ok(kvdb::sync::Resolution::UseRemote)
            } else {
                match backing_resolution(delta, policy, account) {
                    Ok(resolution) => Ok(resolution),
                    Err(error) => {
                        *failure.borrow_mut() = Some(error);
                        Ok(kvdb::sync::Resolution::KeepLocal)
                    }
                }
            }
        },
        |tx| {
            failure.borrow_mut().take().map_or(Ok(()), Err)?;
            if policy
                .entry
                .as_ref()
                .is_some_and(|entry| entry.kind == crate::store::EntryType::ObjectDb)
            {
                object_database::validate_all(tx)?;
            }
            Ok(())
        },
    )
}

pub(super) fn backing_resolution(
    delta: &kvdb::sync::SyncDelta<'_>,
    policy: &DatabasePolicy,
    account: Id,
) -> Result<kvdb::sync::Resolution> {
    use kvdb::sync::{Lww, Resolution, SyncDelta, SyncResolver};
    let ranked = Lww.resolve(delta)?;
    if ranked != Resolution::KeepLocal {
        return Ok(ranked);
    }
    let proposed = SyncDelta { key: delta.key, local: delta.remote, remote: delta.local };
    match policy.validate_sync_source(&proposed, Some(account)) {
        Ok(_) => Ok(ranked),
        Err(Error::AccessDenied(_)) => Ok(Resolution::UseRemote),
        Err(error) => Err(error),
    }
}

fn rejected_backing(
    scratch: &Path,
    source: &Database,
    target: &Database,
    policy: &DatabasePolicy,
    account: Id,
) -> Result<bool> {
    use kvdb::sync::{Lww, Resolution, SyncReceiver, SyncResolver, SyncSender, SyncStatus};
    if replica_root(source)? == replica_root(target)? {
        return Ok(false);
    }
    let rejected = std::cell::Cell::new(false);
    let failure = std::cell::RefCell::new(None);
    let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
        if Lww.resolve(delta)? == Resolution::KeepLocal {
            match backing_resolution(delta, policy, account) {
                Ok(Resolution::UseRemote) => rejected.set(true),
                Ok(_) => {}
                Err(error) => *failure.borrow_mut() = Some(error),
            }
        }
        Ok(Resolution::KeepLocal)
    };
    let mut sender = SyncSender::new(source)?;
    let mut request = Vec::with_capacity(256 * 1024);
    let mut receiver =
        SyncReceiver::new(target, resolver, sync_staging(scratch)?, now_unix_secs(), &mut request)?;
    let mut fragment = vec![0; 256 * 1024];
    loop {
        sender.process(&request)?;
        request.clear();
        let mut done = false;
        loop {
            let count = sender.write_output(&mut fragment)?;
            if count == 0 {
                break;
            }
            done |= receiver.process(&fragment[..count], &mut request)? == SyncStatus::Done;
        }
        if done {
            sender.process(&request)?;
            // Inspection never finishes the receiver or publishes a transaction.
            drop(receiver);
            failure.borrow_mut().take().map_or(Ok(()), Err)?;
            return Ok(rejected.get());
        }
        if request.is_empty() {
            return Err(Error::Internal("replica rejection inspection made no progress"));
        }
    }
}

impl ReceivedVolume {
    pub(super) fn rejected_entry(&self, entry: EntryId) -> Result<bool> {
        Ok(RejectedCut::read(&self._directory.0)?
            .is_some_and(|cut| cut.entries.binary_search(&entry).is_ok()))
    }
}

pub(super) fn merge_local_index(
    scratch: &Path,
    stage: &ReceivedVolume,
    local: &Database,
    account: Id,
) -> Result<()> {
    use kvdb::sync::{Lww, Resolution, SyncResolver};
    let cut = RejectedCut::read(&stage._directory.0)?;
    let policy = MetadataPolicy::read(&stage.metadata)?;
    let accepted = IndexSnapshot::new(stage.metadata.begin_read()?);
    let rejected = std::cell::RefCell::new(None);
    let resolver = |delta: &kvdb::sync::SyncDelta<'_>| {
        if stage.entry_scope.is_some() && !volume_index::is_index_key(delta.key) {
            return Ok(Resolution::KeepLocal);
        }
        if volume_index::is_index_key(delta.key) {
            let entry = match volume_index::source_entry_id(delta.key) {
                Ok(entry) => entry,
                Err(error) => {
                    *rejected.borrow_mut() = Some(error);
                    return Ok(Resolution::KeepLocal);
                }
            };
            if stage.entry_scope.is_some_and(|scope| scope != entry) {
                return Ok(Resolution::KeepLocal);
            }
            if cut.as_ref().is_some_and(|cut| cut.entries.binary_search(&entry).is_ok()) {
                let admission = volume_index::with_sync_delta(delta, |change| {
                    accepted.authorize_source(
                        policy.volume(),
                        policy.default_permissions(),
                        account,
                        change,
                    )
                });
                match admission {
                    Ok(_) => {}
                    Err(Error::AccessDenied(_)) => return Ok(Resolution::KeepLocal),
                    Err(error) => {
                        *rejected.borrow_mut() = Some(error);
                        return Ok(Resolution::KeepLocal);
                    }
                }
            }
            if let Err(error) = volume_index::validate_sync_source(delta) {
                *rejected.borrow_mut() = Some(error);
                return Ok(Resolution::KeepLocal);
            }
        }
        Lww.resolve(delta)
    };
    merge_sender(
        scratch,
        kvdb::sync::SyncSender::new(local)?,
        &stage.metadata,
        resolver,
        |tx| {
            rejected.borrow_mut().take().map_or(Ok(()), Err)?;
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        },
        volume_index::repair_all,
    )
}

pub(super) fn merge_canonical_index(
    scratch: &Path,
    stage: &ReceivedVolume,
    target: &Database,
    account: Id,
) -> Result<bool> {
    use kvdb::sync::{Lww, Resolution, SyncResolver};
    let Some(cut) = RejectedCut::read(&stage._directory.0)? else {
        return Ok(false);
    };
    if !stage.authority_qualified || cut.volume != stage.volume || cut.operation != stage.operation
    {
        return Err(Error::AccessDenied("canonical replacement requires its qualified AP cut"));
    }
    let unavailable = missing_rejected_backings(stage, &cut, account)?;
    let rejected = std::cell::RefCell::new(None);
    let resolver =
        |delta: &kvdb::sync::SyncDelta<'_>| match canonical_index_resolution(stage, &cut, delta) {
            Ok(Some(resolution)) => Ok(resolution),
            Ok(None) => Lww.resolve(delta),
            Err(error) => {
                *rejected.borrow_mut() = Some(error);
                Ok(Resolution::KeepLocal)
            }
        };
    let repaired = std::cell::Cell::new(false);
    merge_sender(
        scratch,
        kvdb::sync::SyncSender::new(&stage.metadata)?,
        target,
        resolver,
        |tx| {
            rejected.borrow_mut().take().map_or(Ok(()), Err)?;
            MetadataPolicy::validate_pending(tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)
        },
        |local| {
            repair_canonical_cut(local, &cut, &unavailable)?;
            repaired.set(true);
            Ok(())
        },
    )?;
    if !repaired.get() {
        // Equal roots do not certify replica-local records. Backing-only
        // rejection still needs its cut and name/quarantine repair before
        // clearing the intent. The volume gate pins the accepted source cut.
        let mut tx = target.begin_write(now_unix_secs())?;
        repair_canonical_cut(&mut tx.local_indexes(), &cut, &unavailable)?;
        tx.commit()?;
    }
    Ok(true)
}

fn canonical_index_resolution(
    stage: &ReceivedVolume,
    cut: &RejectedCut,
    delta: &kvdb::sync::SyncDelta<'_>,
) -> Result<Option<kvdb::sync::Resolution>> {
    use kvdb::sync::Resolution;
    if volume_index::is_index_key(delta.key) {
        let entry = volume_index::source_entry_id(delta.key)?;
        if stage.entry_scope.is_some_and(|scope| scope != entry) {
            return Ok(Some(Resolution::KeepLocal));
        }
        let canonical = cut.entries.binary_search(&entry).is_ok();
        volume_index::with_sync_delta(delta, |change| {
            let mut change = *change;
            if canonical {
                change.local = kvdb::sync::EntState::Missing;
            }
            volume_index::validate_source(&change).map(|_| ())
        })?;
        if canonical {
            return Ok(Some(Resolution::UseRemote));
        }
    }
    Ok(None)
}

fn repair_canonical_cut(
    local: &mut kvdb::LocalIndexes<'_, '_, RedbStorage>,
    cut: &RejectedCut,
    unavailable: &[EntryId],
) -> Result<()> {
    volume_index::repair_all(local)?;
    for entries in cut.entries.chunks(volume_index::MAX_REPAIR_IDS) {
        volume_index::repair(local, entries)?;
    }
    local.put(&key(cut.operation), &encode(cut)?)?;
    for entry in unavailable {
        volume_index::quarantine_backing(local, *entry)?;
    }
    Ok(())
}

fn missing_rejected_backings(
    stage: &ReceivedVolume,
    cut: &RejectedCut,
    account: Id,
) -> Result<Vec<EntryId>> {
    let mut unavailable = Vec::new();
    for entry in &cut.entries {
        let file = DatabaseId::Entry(*entry).file_name();
        if cut.files.contains_key(&file)
            && !stage._directory.0.join(file).is_file()
            && !retains_log_appends(&stage.metadata, account, *entry)?
        {
            unavailable.push(*entry);
        }
    }
    Ok(unavailable)
}

fn retains_log_appends(index: &Database, account: Id, entry: EntryId) -> Result<bool> {
    let policy = match DatabasePolicy::read_source(index, DatabaseId::Entry(entry)) {
        Ok(policy) => policy,
        Err(Error::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    if policy.entry.as_ref().and_then(|entry| entry.protocol)
        != Some(volume_index::DatabaseProtocol::BlobLog)
    {
        return Ok(false);
    }
    match policy.authorize(Principal::Human(account), Permission::Append) {
        Ok(()) => Ok(true),
        Err(Error::AccessDenied(_) | Error::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

fn copy(
    cut: &mut RejectedCut,
    directory: &Path,
    source: &Path,
    name: &str,
    maximum: u64,
    expected_database: Option<[u8; 32]>,
) -> Result<()> {
    if cut.files.len() == MAX_FILES {
        return Err(Error::LimitExceeded);
    }
    let mut source = fs::File::open(source).map_err(io)?;
    let length = source.metadata().map_err(io)?.len();
    let charge = length
        .checked_add(if expected_database.is_some() { DATABASE_OPEN_RESERVE } else { 0 })
        .ok_or(Error::LimitExceeded)?;
    let total = cut.bytes.checked_add(charge).ok_or(Error::LimitExceeded)?;
    if total > maximum {
        return Err(Error::LimitExceeded);
    }
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(name))
        .map_err(io)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let count = source.read(&mut buffer).map_err(io)?;
        if count == 0 {
            break;
        }
        copied = copied.checked_add(count as u64).ok_or(Error::LimitExceeded)?;
        if copied > length {
            return Err(Error::LocalContention("recovery cut changed"));
        }
        destination.write_all(&buffer[..count]).map_err(io)?;
        hash.update(&buffer[..count]);
    }
    if copied != length {
        return Err(Error::LocalContention("recovery cut changed"));
    }
    destination.sync_all().map_err(io)?;
    drop(destination);
    let digest = if let Some(expected) = expected_database {
        let actual = database_digest(&open_replica(&directory.join(name), 512 * 1024)?)?;
        if actual != expected {
            return Err(Error::LocalContention("recovery database changed during capture"));
        }
        if fs::metadata(directory.join(name)).map_err(io)?.len() > charge {
            return Err(Error::LimitExceeded);
        }
        actual
    } else {
        *hash.finalize().as_bytes()
    };
    cut.bytes = total;
    cut.files.insert(name.to_owned(), digest);
    Ok(())
}

// redb updates container housekeeping on open/close. Seal the logical snapshot,
// including local readiness metadata, rather than mutable container bytes.
fn database_digest(database: &Database) -> Result<[u8; 32]> {
    database.verify()?;
    let read = database.begin_read()?;
    let mut hash = blake3::Hasher::new();
    hash.update(&encode(&(read.root_hash()?, read.clock()?.as_u64(), database.meta().hash_key))?);
    for item in read.local_iter_from(b"")? {
        let (key, value) = item?;
        hash.update(&(key.len() as u64).to_le_bytes());
        hash.update(&key);
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(&value);
    }
    Ok(*hash.finalize().as_bytes())
}
