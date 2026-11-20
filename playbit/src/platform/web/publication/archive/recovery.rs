//! Current policy gates discovery of local-only retained cuts. The worker
//! verifies the recorded file seal before attaching its read-only index slot.
use super::*;
mod restore;
use crate::store::{
    object_contract::Principal,
    volume_index::{RecoveryCursor, RejectedEntry, RejectedEntryPage},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CutRequest {
    volume: Id,
    after: Option<Id>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageRequest {
    volume: Id,
    cut: Id,
    slot: u32,
    after: Option<u16>,
}

fn decode_cut(generation: Id, bytes: &[u8]) -> Result<Cut> {
    if bytes.len() > RECORD_BYTES as usize {
        return Err(Error::LimitExceeded);
    }
    let cut: Cut = serde_json::from_slice(bytes).map_err(map_error)?;
    cut.validate()?;
    if cut.generation != generation {
        return Err(Error::InvalidArgument);
    }
    Ok(cut)
}

fn retained_cut(volume: &Volume, generation: Id) -> Result<Cut> {
    let read = volume.index.begin_read().map_err(map_error)?;
    let bytes = read.local_get(&key(generation)).map_err(map_error)?.ok_or(Error::NotFound)?;
    decode_cut(generation, &bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_cut_get(length: usize) -> i32 {
    call(length, |host, bytes| {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            volume: Id,
            cut: Id,
        }
        let request: Request = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(request.volume)?;
        host.authorize(&volume, None, Permission::Read)?;
        serde_json::to_vec(&retained_cut(&volume, request.cut)?).map_err(map_error)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_cut(length: usize) -> i32 {
    call(length, |host, bytes| {
        let request: CutRequest = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(request.volume)?;
        host.authorize(&volume, None, Permission::Read)?;
        let read = volume.index.begin_read().map_err(map_error)?;
        let start = request.after.map(key).unwrap_or_else(|| PREFIX.to_vec());
        let mut result = None;
        for row in read.local_iter_from(&start).map_err(map_error)? {
            let (key, bytes) = row.map_err(map_error)?;
            if !key.starts_with(PREFIX) {
                break;
            }
            let id: [u8; 16] = key[PREFIX.len()..].try_into().map_err(map_error)?;
            let generation = Id::from_bytes(id);
            if Some(generation) == request.after {
                continue;
            }
            result = Some(decode_cut(generation, &bytes)?);
            break;
        }
        serde_json::to_vec(&result).map_err(map_error)
    })
}

// A missing accepted birth may use retained permissions, but only with catalog
// Read. An existing entry always uses its current permissions, even if retired.
fn authorize_read(
    host: &Host,
    volume: &Volume,
    policy: &MetadataPolicy,
    current: &IndexSnapshot<'_, OpfsStorage>,
    retained: &IndexSnapshot<'_, OpfsStorage>,
    entry: EntryId,
) -> Result<()> {
    authority::check(host, volume, Some(entry))?;
    let authorize = |snapshot: &IndexSnapshot<'_, OpfsStorage>| {
        snapshot.authorize_retained_read(
            policy.volume(),
            policy.default_permissions(),
            Principal::Human(host.account),
            entry,
        )
    };
    match authorize(current) {
        Err(crate::store::Error::NotFound(_)) => {
            policy.authorize(host.account, Permission::Read).map_err(store_error)?;
            authorize(retained).map_err(store_error)
        }
        result => result.map_err(store_error),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_recovery_entries(length: usize) -> i32 {
    call(length, |host, bytes| {
        if PREPARED.with(|stage| stage.borrow().is_some()) {
            return Err(Error::Unavailable);
        }
        let request: PageRequest = serde_json::from_slice(bytes).map_err(map_error)?;
        let volume = host.replica_volume(request.volume)?;
        let policy = host.authorize(&volume, None, Permission::Read)?;
        let cut = retained_cut(&volume, request.cut)?;
        let offset = request.after.map_or(0, |after| usize::from(after) + 1);
        if offset > cut.entries.len()
            || request.slot <= 64
            || host.database(request.slot as usize).is_ok()
        {
            return Err(Error::InvalidArgument);
        }
        let original =
            Db::open(OpfsStorage(request.slot), &Default::default()).map_err(map_error)?;
        if !original.meta().syncable
            || original.meta().hash_key != DatabaseId::Index.hash_key(&volume.key, volume.id)
        {
            return Err(Error::InvalidArgument);
        }
        let current = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let retained = IndexSnapshot::new(original.begin_read().map_err(map_error)?);
        let end = (offset + 64).min(cut.entries.len());
        let mut page = RejectedEntryPage { entries: Vec::new(), next: None };
        for index in offset..end {
            let entry = cut.entries[index];
            match authorize_read(host, &volume, &policy, &current, &retained, entry) {
                Ok(()) => page.entries.push(RejectedEntry {
                    cursor: RecoveryCursor { cut: request.cut, offset: index as u16 },
                    entry: retained.summary(entry).map_err(store_error)?,
                }),
                Err(Error::AccessDenied) => {}
                Err(error) => return Err(error),
            }
        }
        if end < cut.entries.len() {
            page.next = Some(RecoveryCursor { cut: request.cut, offset: (end - 1) as u16 });
        }
        serde_json::to_vec(&page).map_err(map_error)
    })
}
