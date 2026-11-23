//! Retained offline cuts belong to the persistent index, outside replication.
//! The worker seals private files before this module records their references.
use super::*;
use crate::store::volume_index::{EntryChange, EntryKind};
use std::collections::BTreeSet;
mod recovery;

const PREFIX: &[u8] = b"web-rejected/";
const MAX_CUTS: usize = 32;
const MAX_ENTRIES: usize = 128;
const MAX_FILES: usize = 256;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const RECORD_BYTES: u64 = 64 * 1024;

#[derive(Default)]
pub(super) struct State {
    entries: Option<Vec<EntryId>>,
    sealed: bool,
    pub(super) rejected_backings: BTreeSet<EntryId>,
    pub(super) quarantine: BTreeSet<EntryId>,
    pub(super) restored_quarantine: bool,
}
impl State {
    pub(super) fn started(&self) -> bool {
        self.entries.is_some()
    }
    pub(super) fn validate(&self) -> Result<()> {
        if (self.started() || !self.rejected_backings.is_empty()) && !self.sealed {
            Err(Error::Unavailable)
        } else {
            Ok(())
        }
    }
    pub(super) fn reject_backing(&mut self, entry: EntryId, quarantine: bool) -> Result<()> {
        if self.rejected_backings.len() == MAX_ENTRIES && !self.rejected_backings.contains(&entry) {
            return Err(Error::LimitExceeded);
        }
        self.rejected_backings.insert(entry);
        if quarantine {
            self.quarantine.insert(entry);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    bytes: u64,
    digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cut {
    generation: Id,
    entries: Vec<EntryId>,
    bytes: u64,
    files: BTreeMap<String, Seal>,
}
impl Cut {
    fn validate(&self) -> Result<()> {
        valid_entries(&self.entries)?;
        if self.generation.as_bytes() == &[0; 16]
            || self.files.is_empty()
            || self.files.len() > MAX_FILES
            || !self.files.contains_key("index.kvdb")
        {
            return Err(Error::InvalidArgument);
        }
        let mut bytes = RECORD_BYTES;
        for (name, seal) in &self.files {
            let valid = if name == "index.kvdb" {
                true
            } else if let Some(id) = name.strip_suffix(".kvdb") {
                EntryId::parse(id).is_ok_and(|id| self.entries.binary_search(&id).is_ok())
            } else if let Some(digest) = name.strip_suffix(".blob") {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            } else {
                false
            };
            if !valid {
                return Err(Error::InvalidArgument);
            }
            bytes = bytes.checked_add(seal.bytes).ok_or(Error::LimitExceeded)?;
        }
        if bytes != self.bytes || bytes > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }
}
fn valid_entries(entries: &[EntryId]) -> Result<()> {
    if entries.is_empty()
        || entries.len() > MAX_ENTRIES
        || entries.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}
fn key(generation: Id) -> Vec<u8> {
    [PREFIX, generation.as_bytes()].concat()
}
fn available(index: &Database) -> Result<u64> {
    let read = index.begin_read().map_err(map_error)?;
    let mut count = 0;
    let mut remaining = MAX_BYTES;
    for row in read.local_iter_from(PREFIX).map_err(map_error)? {
        let (key, value) = row.map_err(map_error)?;
        if !key.starts_with(PREFIX) {
            break;
        }
        if count == MAX_CUTS || value.len() > RECORD_BYTES as usize {
            return Err(Error::LimitExceeded);
        }
        let cut: Cut = serde_json::from_slice(&value).map_err(map_error)?;
        cut.validate()?;
        if key.as_ref() != self::key(cut.generation) {
            return Err(Error::InvalidArgument);
        }
        remaining = remaining.checked_sub(cut.bytes).ok_or(Error::LimitExceeded)?;
        count += 1;
    }
    if count == MAX_CUTS || remaining < RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(remaining)
}
fn qualified(stage: &Preparation, host: &Host) -> Result<()> {
    stage.current_authority(host, DatabaseId::Index)?;
    if !stage.authority_qualified.get() || stage.source_failed.get() {
        return Err(Error::AccessDenied);
    }
    Ok(())
}
fn backings(stage: &Preparation, entries: &[EntryId]) -> Result<Vec<EntryId>> {
    let snapshot = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?);
    let mut result = Vec::new();
    for entry in entries {
        if snapshot.source_header(*entry).map_err(store_error)?.protocol.is_some()
            && (snapshot.source_backing_ready(*entry).map_err(store_error)?
                || snapshot.backing_quarantined(*entry).map_err(store_error)?)
        {
            result.push(*entry);
        }
    }
    Ok(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_archive_plan(length: usize) -> i32 {
    call(length, |host, bytes| {
        let entries: Vec<EntryId> = serde_json::from_slice(bytes).map_err(map_error)?;
        valid_entries(&entries)?;
        let stage = prepared()?;
        qualified(&stage, host)?;
        if stage.archive.borrow().started() {
            return Err(Error::Unavailable);
        }
        if stage
            .archive
            .borrow()
            .rejected_backings
            .iter()
            .any(|entry| entries.binary_search(entry).is_err())
        {
            return Err(Error::InvalidArgument);
        }
        let current = stage.original.index.begin_read().map_err(map_error)?;
        let policy = MetadataPolicy::read(&stage.accepted_index).map_err(store_error)?;
        for entry in &entries {
            if stage.entry_scope.is_some_and(|(selected, _)| *entry != selected)
                || (!stage.archive.borrow().rejected_backings.contains(entry)
                    && !volume_index::rejected_sources(
                        &current,
                        &stage.accepted_index,
                        &policy,
                        host.actor(&stage.volume),
                        *entry,
                    )
                    .map_err(store_error)?)
            {
                return Err(Error::AccessDenied);
            }
        }
        let remaining = available(&stage.volume.index)?;
        let backings = backings(&stage, &entries)?;
        let output = serde_json::to_vec(&serde_json::json!({
            "available": remaining, "reserve": RECORD_BYTES, "backings": backings,
        }))
        .map_err(map_error)?;
        stage.archive.borrow_mut().entries = Some(entries);
        stage.validated.set(false);
        Ok(output)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_archive_blobs(length: usize) -> i32 {
    call(length, |host, bytes| {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            entry: EntryId,
            after: Option<EntryId>,
        }
        let request: Request = serde_json::from_slice(bytes).map_err(map_error)?;
        let stage = prepared()?;
        qualified(&stage, host)?;
        if stage
            .archive
            .borrow()
            .entries
            .as_ref()
            .is_none_or(|entries| entries.binary_search(&request.entry).is_err())
        {
            return Err(Error::AccessDenied);
        }
        let snapshot = IndexSnapshot::new(stage.original.index.begin_read().map_err(map_error)?);
        let page = snapshot.revisions(request.entry, request.after, 64).map_err(store_error)?;
        let mut blobs = Vec::new();
        for revision in page.revisions {
            let kind = match revision.change {
                EntryChange::Created { info } => Some(info.kind),
                EntryChange::Content { kind } => Some(kind),
                _ => None,
            };
            if let Some(EntryKind::Blob { digest, len_bytes, .. }) = kind {
                blobs.push((digest, len_bytes));
            }
        }
        serde_json::to_vec(&(blobs, page.next)).map_err(map_error)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_volume_archive_commit(length: usize) -> i32 {
    call(length, |host, bytes| {
        if bytes.len() > RECORD_BYTES as usize {
            return Err(Error::LimitExceeded);
        }
        let cut: Cut = serde_json::from_slice(bytes).map_err(map_error)?;
        cut.validate()?;
        let stage = prepared()?;
        qualified(&stage, host)?;
        if cut.generation != stage.generation
            || stage.archive.borrow().entries.as_ref() != Some(&cut.entries)
            || stage.archive.borrow().sealed
        {
            return Err(Error::InvalidArgument);
        }
        for entry in backings(&stage, &cut.entries)? {
            if !cut.files.contains_key(&format!("{entry}.kvdb")) {
                return Err(Error::Unavailable);
            }
        }
        if cut.bytes > available(&stage.volume.index)? {
            return Err(Error::LimitExceeded);
        }
        reconcile_sources(&stage, host.actor(&stage.volume), &cut.entries)?;
        let mut tx = stage.volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
        tx.local_indexes().put(&key(cut.generation), bytes).map_err(map_error)?;
        for entry in &stage.archive.borrow().quarantine {
            volume_index::quarantine_backing(&mut tx.local_indexes(), *entry)
                .map_err(store_error)?;
        }
        tx.commit().map_err(map_error)?;
        stage.archive.borrow_mut().sealed = true;
        stage.validated.set(false);
        Ok(Vec::new())
    })
}

// Both inputs are detached and fenced by the same qualified AP source cut.
// Admitted local facts survive beside rejected ones, including Append births.
fn reconcile_sources(stage: &Preparation, actor: DataActor, entries: &[EntryId]) -> Result<()> {
    use kvdb::sync::{Resolution, SyncDelta, SyncReceiver, SyncSender, SyncStatus};
    let policy = MetadataPolicy::read(&stage.accepted_index).map_err(store_error)?;
    let accepted = IndexSnapshot::new(stage.accepted_index.begin_read().map_err(map_error)?);
    let failure = RefCell::new(None);
    let resolver = |delta: &SyncDelta<'_>| match source_resolution(
        &accepted, &policy, actor, entries, delta,
    ) {
        Ok(resolution) => Ok(resolution),
        Err(error) => {
            *failure.borrow_mut() = Some(store_error(error));
            Ok(Resolution::KeepLocal)
        }
    };
    let mut sender = SyncSender::new(&stage.accepted_index).map_err(map_error)?;
    let mut request = Vec::with_capacity(SYNC_FRAGMENT);
    let mut receiver = SyncReceiver::new(
        &stage.volume.index,
        resolver,
        OpfsStaging::new(0).map_err(map_error)?,
        unsafe { now() } as u64,
        &mut request,
    )
    .map_err(map_error)?;
    let mut fragment = vec![0; SYNC_FRAGMENT];
    let mut remaining = INPUT_LIMIT;
    loop {
        sender.process(&request).map_err(map_error)?;
        request.clear();
        let mut done = false;
        loop {
            let count = sender.write_output(&mut fragment).map_err(map_error)?;
            if count == 0 {
                break;
            }
            remaining = remaining.checked_sub(count).ok_or(Error::LimitExceeded)?;
            done |= receiver.process(&fragment[..count], &mut request).map_err(map_error)?
                == SyncStatus::Done;
        }
        if done {
            sender.process(&request).map_err(map_error)?;
            break;
        }
        if request.is_empty() {
            return Err(Error::InvalidArgument);
        }
    }
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    MetadataPolicy::validate_pending(
        receiver.pending_transaction().map_err(map_error)?,
        MetadataPolicy::MAX_MATERIALIZED_BYTES,
    )
    .map_err(store_error)?;
    receiver
        .repair_local_indexes(volume_index::repair_all)
        .map_err(store_error)?
        .finish()
        .map_err(map_error)?;
    Ok(())
}

fn source_resolution(
    accepted: &IndexSnapshot<'_, OpfsStorage>,
    policy: &MetadataPolicy,
    actor: DataActor,
    entries: &[EntryId],
    delta: &kvdb::sync::SyncDelta<'_>,
) -> crate::store::Result<kvdb::sync::Resolution> {
    use kvdb::sync::{Lww, Resolution, SyncDelta, SyncResolver};
    let ranked = Lww.resolve(delta)?;
    if ranked == Resolution::KeepLocal
        && volume_index::source_entry_id(delta.key)
            .is_ok_and(|entry| entries.binary_search(&entry).is_ok())
    {
        let proposed = SyncDelta { key: delta.key, local: delta.remote, remote: delta.local };
        match volume_index::with_sync_delta(&proposed, |change| {
            accepted.authorize_source(policy.volume(), policy.default_permissions(), actor, change)
        }) {
            Ok(()) => {}
            Err(crate::store::Error::AccessDenied(_)) => return Ok(Resolution::UseRemote),
            Err(error) => return Err(error),
        }
    }
    Ok(ranked)
}
