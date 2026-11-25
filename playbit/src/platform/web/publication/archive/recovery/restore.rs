//! Prepare a fresh entry off the active index; publish only its completed body.
use super::*;
use crate::store::{bloblog::Log, object_contract::EntryAcl, volume_index::EntryInfo};

thread_local! {
    static RESTORE: RefCell<Option<Restore>> = const { RefCell::new(None) };
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Begin {
    volume: Id,
    cut: Id,
    entry: EntryId,
    name: String,
    slot: u32,
}
struct Restore {
    request: Begin,
    volume: Rc<Volume>,
    retained: Database,
    root: Option<(u8, [u8; 32])>,
    id: EntryId,
    info: EntryInfo,
    body: Option<(Database, Database)>,
    after: Option<Vec<u8>>,
    log_validated: bool,
    log_cursor: Option<crate::store::bloblog::Cursor>,
    complete: bool,
}

fn free_slot(host: &Host, slot: u32) -> Result<()> {
    if slot <= 64 || host.database(slot as usize).is_ok() {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}
fn source(host: &Host, volume: &Volume, retained: &Database, request: &Begin) -> Result<EntryInfo> {
    let policy = host.admit_volume(volume, Permission::Write)?;
    let current = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
    let archived = IndexSnapshot::new(retained.begin_read().map_err(map_error)?);
    authorize_read(host, volume, &policy, &current, &archived, request.entry)?;
    match policy.authorize(host.actor(volume), Permission::Write) {
        Ok(()) => {}
        Err(crate::store::Error::AccessDenied(_)) => {
            policy.authorize(host.actor(volume), Permission::Append).map_err(store_error)?;
        }
        Err(error) => return Err(store_error(error)),
    }
    let permissions = match current.permissions(request.entry) {
        Ok(permissions) => permissions,
        Err(crate::store::Error::NotFound(_)) => {
            archived.permissions(request.entry).map_err(store_error)?
        }
        Err(error) => return Err(store_error(error)),
    };
    if matches!(permissions, EntryAcl::Replace(_)) {
        policy.authorize(host.actor(volume), Permission::ManagePermissions).map_err(store_error)?;
        if policy.volume().owner != host.account {
            return Err(Error::AccessDenied);
        }
    }
    match current.lookup(&request.name) {
        Ok(_) => return Err(Error::Conflict),
        Err(crate::store::Error::NotFound(_)) => {}
        Err(error) => return Err(store_error(error)),
    }
    let mut info = archived.source_entry(request.entry).map_err(store_error)?.info;
    if info.protocol == Some(DatabaseProtocol::BlobLog) {
        // Check again on every preparation/publication turn, as for log upload.
        policy.authorize(host.actor(volume), Permission::Write).map_err(store_error)?;
    }
    if !matches!(info.kind, EntryKind::Blob { .. } | EntryKind::Directory {})
        && (!archived.source_backing_ready(request.entry).map_err(store_error)?
            || archived.schema_pending(request.entry).map_err(store_error)?)
    {
        return Err(Error::Unavailable);
    }
    info.origin = volume_index::namespace::Origin { installation: host.installation, label: None };
    info.name = request.name.rsplit('/').next().expect("validated recovery path").to_owned();
    info.namespace = Default::default();
    info.initial_permissions = permissions;
    Ok(info)
}
impl Restore {
    fn check(&self, host: &Host) -> Result<()> {
        if PREPARED.with(|stage| stage.borrow().is_some())
            || !Rc::ptr_eq(&host.replica_volume(self.volume.id)?, &self.volume)
            || self.volume.index.begin_read().map_err(map_error)?.root_hash().map_err(map_error)?
                != self.root
        {
            return Err(Error::Conflict);
        }
        let mut current = source(host, &self.volume, &self.retained, &self.request)?;
        current.created_revision = self.info.created_revision;
        if current != self.info {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_begin(length: usize) -> i32 {
    call(length, |host, bytes| {
        if RESTORE.with(|state| state.borrow().is_some())
            || PREPARED.with(|stage| stage.borrow().is_some())
        {
            return Err(Error::Unavailable);
        }
        let request: Begin = serde_json::from_slice(bytes).map_err(map_error)?;
        volume_index::validate_name(&request.name).map_err(store_error)?;
        free_slot(host, request.slot)?;
        let volume = host.replica_volume(request.volume)?;
        let cut = retained_cut(&volume, request.cut)?;
        if cut.entries.binary_search(&request.entry).is_err() {
            return Err(Error::AccessDenied);
        }
        let retained =
            Db::open(OpfsStorage(request.slot), &Default::default()).map_err(map_error)?;
        if !retained.meta().syncable
            || retained.meta().hash_key != DatabaseId::Index.hash_key(&volume.key, volume.id)
        {
            return Err(Error::InvalidArgument);
        }
        let mut info = source(host, &volume, &retained, &request)?;
        let id = databases::random_entry()?;
        info.created_revision = databases::random_entry()?;
        info.validate().map_err(store_error)?;
        let root = volume.index.begin_read().map_err(map_error)?.root_hash().map_err(map_error)?;
        let blob = restored_blob(&info, &cut, volume.id, id, request.entry)?;
        let database = info.protocol.is_some();
        let complete = !database;
        let output = serde_json::to_vec(&serde_json::json!({
            "entry": id, "blob": blob, "database": database,
        }))
        .map_err(map_error)?;
        RESTORE.with(|state| {
            *state.borrow_mut() = Some(Restore {
                request,
                volume,
                retained,
                root,
                id,
                info,
                body: None,
                after: None,
                log_validated: false,
                log_cursor: None,
                complete,
            })
        });
        Ok(output)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_backing(length: usize) -> i32 {
    call(length, |host, bytes| {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            source: u32,
            destination: u32,
            replica_id: u64,
        }
        let request: Request = serde_json::from_slice(bytes).map_err(map_error)?;
        RESTORE.with(|state| {
            let mut state = state.borrow_mut();
            let state = state.as_mut().ok_or(Error::Unavailable)?;
            state.check(host)?;
            if state.body.is_some()
                || state.complete
                || request.source == request.destination
                || request.source == state.request.slot
                || request.destination == state.request.slot
                || request.replica_id == 0
            {
                return Err(Error::InvalidArgument);
            }
            free_slot(host, request.source)?;
            free_slot(host, request.destination)?;
            let source =
                Db::open(OpfsStorage(request.source), &Default::default()).map_err(map_error)?;
            if !source.meta().syncable
                || source.meta().hash_key
                    != DatabaseId::Entry(state.request.entry)
                        .hash_key(&state.volume.key, state.volume.id)
            {
                return Err(Error::InvalidArgument);
            }
            let destination = Db::create(
                OpfsStorage(request.destination),
                &CreateOptions::syncable(
                    DatabaseId::Entry(state.id).hash_key(&state.volume.key, state.volume.id),
                    request.replica_id,
                ),
            )
            .map_err(map_error)?;
            state.body = Some((source, destination));
            Ok(Vec::new())
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_copy() -> i32 {
    call(0, |host, _| {
        RESTORE.with(|state| {
            let mut state = state.borrow_mut();
            let state = state.as_mut().ok_or(Error::Unavailable)?;
            state.check(host)?;
            if state.complete {
                return Ok(b"true".to_vec());
            }
            let (original, target) = state.body.as_ref().ok_or(Error::Unavailable)?;
            if state.info.protocol == Some(DatabaseProtocol::BlobLog) {
                let original = Log {
                    database: original,
                    volume: state.volume.id,
                    entry: state.request.entry,
                    encryption_key: None,
                    now: unsafe { now() } as u64,
                };
                if !state.log_validated {
                    state.after =
                        original.validate_page(state.after.as_deref(), 64).map_err(store_error)?;
                    state.log_validated = state.after.is_none();
                    return Ok(b"false".to_vec());
                }
                let page = original.get(state.log_cursor.as_ref()).map_err(store_error)?;
                let entries: Vec<_> =
                    page.entries.into_iter().map(|entry| entry.recover_into(state.id)).collect();
                Log {
                    database: target,
                    volume: state.volume.id,
                    entry: state.id,
                    encryption_key: None,
                    now: unsafe { now() } as u64,
                }
                .apply(&entries, true)
                .map_err(store_error)?;
                state.complete = !page.more;
                state.log_cursor = Some(page.cursor);
            } else {
                let read = original.begin_read().map_err(map_error)?;
                let mut tx = target.begin_write(unsafe { now() } as u64).map_err(map_error)?;
                let mut after = state.after.clone();
                let mut complete = false;
                for _ in 0..64 {
                    let Some(key) = read
                        .next_key(after.as_deref().unwrap_or_default(), after.is_some())
                        .map_err(map_error)?
                    else {
                        complete = true;
                        break;
                    };
                    let value =
                        read.value_reader(&key).map_err(map_error)?.ok_or(Error::Internal)?;
                    tx.put_from_with_clock(&key, &value, value.clock().ok_or(Error::Internal)?)
                        .map_err(map_error)?;
                    after = Some(key);
                }
                if complete && matches!(state.info.kind, EntryKind::ObjectDb { .. }) {
                    crate::store::object_database::validate_all(&tx).map_err(store_error)?;
                }
                tx.commit().map_err(map_error)?;
                state.after = after;
                state.complete = complete;
            }
            serde_json::to_vec(&state.complete).map_err(map_error)
        })
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Publish {
    generation: Id,
    blob: Option<blob::FileReference>,
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_publish(length: usize) -> i32 {
    call(length, |host, bytes| {
        let request: Publish = serde_json::from_slice(bytes).map_err(map_error)?;
        RESTORE.with(|cell| {
            let mut state = cell.borrow_mut();
            let restore = state.as_ref().ok_or(Error::Unavailable)?;
            restore.check(host)?;
            if !restore.complete || request.generation.as_bytes() == &[0; 16] {
                return Err(Error::Unavailable);
            }
            let version = if let EntryKind::Blob { len_bytes, digest, encrypted_key, .. } =
                &restore.info.kind
            {
                let version = request.blob.ok_or(Error::Unavailable)?.version()?;
                if version.address
                    != (volume_index::EntryAddress { volume: restore.volume.id, entry: restore.id })
                    || version.revision != restore.info.created_revision
                    || version.len_bytes != *len_bytes
                    || version.digest != *digest
                    || version.encrypted_key != *encrypted_key
                {
                    return Err(Error::InvalidArgument);
                }
                Some(version)
            } else {
                if request.blob.is_some()
                    || (restore.info.protocol.is_some() && restore.body.is_none())
                {
                    return Err(Error::InvalidArgument);
                }
                None
            };
            let output = serde_json::to_vec(&restore.id).map_err(map_error)?;
            if let Some(version) = &version {
                blob_cache::protect(host, version)?;
            }
            let mut tx = restore
                .volume
                .index
                .begin_write_with_delta(
                    unsafe { now() } as u64,
                    crate::store::MAX_REPLICA_DELTA_BYTES,
                )
                .map_err(map_error)?;
            volume_index::create_at_path(&mut tx, restore.id, &restore.request.name, &restore.info)
                .map_err(store_error)?;
            volume_index::mark_backing_ready(&mut tx, restore.id).map_err(store_error)?;
            if let Some(version) = version {
                host.record_blob_upload(&mut tx, &version)?;
            } else if restore.info.protocol.is_some() {
                tx.local_indexes()
                    .put(&backing::key(restore.id), request.generation.as_bytes())
                    .map_err(map_error)?;
            }
            let delta = tx.commit_with_delta().map_err(map_error)?;
            host.committed_change(restore.volume.id, DatabaseId::Index, delta);
            state.take();
            Ok(output)
        })
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_abort() -> i32 {
    crate::platform::failure::guard();
    RESTORE.with(|state| state.borrow_mut().take());
    0
}

fn restored_blob(
    info: &EntryInfo,
    cut: &Cut,
    volume: Id,
    id: EntryId,
    entry: EntryId,
) -> Result<Option<blob::FileReference>> {
    let blob = if let EntryKind::Blob { len_bytes, digest, encrypted_key, .. } = &info.kind {
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        if !cut.files.contains_key(&format!("{name}.blob")) {
            return Err(Error::Unavailable);
        }
        Some(blob::FileReference::from_version(
            volume_index::BlobVersion {
                address: volume_index::EntryAddress { volume, entry: id },
                revision: info.created_revision,
                len_bytes: *len_bytes,
                digest: *digest,
                encrypted_key: *encrypted_key,
            },
            0,
            65536,
        ))
    } else if matches!(info.kind, EntryKind::Directory {}) {
        None
    } else {
        if !cut.files.contains_key(&format!("{}.kvdb", entry)) {
            return Err(Error::Unavailable);
        }
        None
    };
    Ok(blob)
}
