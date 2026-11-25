//! Root-index local records cover every volume using this execution directory,
//! independently of current grants. Protection precedes reference publication;
//! a failed or uncertain publication can only leave extra retained contents.
use super::*;
use crate::store::volume_index::BlobVersion;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retention {
    known: bool,
    confirmed: bool,
    pending: u64,
}
fn key(digest: &Digest) -> Vec<u8> {
    let mut key = b"blob-retention/".to_vec();
    key.extend_from_slice(digest);
    key
}
fn protection(version: &BlobVersion) -> Vec<u8> {
    let mut key = b"blob-protection/".to_vec();
    key.extend_from_slice(version.address.volume.as_bytes());
    key.extend_from_slice(version.address.entry.as_bytes());
    key.extend_from_slice(version.revision.as_bytes());
    key
}
fn read(tx: &kvdb::WriteTxn<'_, OpfsStorage>, digest: &Digest) -> Result<Retention> {
    tx.local_get(&key(digest))
        .map_err(map_error)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(map_error))
        .transpose()
        .map(|value| value.unwrap_or_default())
}
fn write(
    tx: &mut kvdb::WriteTxn<'_, OpfsStorage>,
    digest: &Digest,
    state: &Retention,
) -> Result<()> {
    tx.local_indexes()
        .put(&key(digest), &serde_json::to_vec(state).map_err(map_error)?)
        .map_err(map_error)
}
pub(super) fn evictable(host: &Host, digest: &Digest) -> Result<bool> {
    let root = host.volumes.borrow()[0].index.clone();
    let tx = root.begin_read().map_err(map_error)?;
    let Some(bytes) = tx.local_get(&key(digest)).map_err(map_error)? else {
        return Ok(false);
    };
    let state: Retention = serde_json::from_slice(&bytes).map_err(map_error)?;
    Ok(state.known && state.confirmed && state.pending == 0)
}
pub(super) fn forget(host: &Host, digest: &Digest) -> Result<()> {
    let root = host.volumes.borrow()[0].index.clone();
    let mut tx = root.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    let state = read(&tx, digest)?;
    if !state.known || !state.confirmed || state.pending != 0 {
        return Err(Error::Internal);
    }
    tx.local_indexes().del(&key(digest)).map_err(map_error)?;
    tx.commit().map_err(map_error)
}
pub(super) fn saved(host: &Host, digest: &Digest, fresh: bool, confirmed: bool) -> Result<()> {
    let root = host.volumes.borrow()[0].index.clone();
    {
        let read = root.begin_read().map_err(map_error)?;
        if let Some(bytes) = read.local_get(&key(digest)).map_err(map_error)? {
            let state: Retention = serde_json::from_slice(&bytes).map_err(map_error)?;
            if state.confirmed || !confirmed {
                return Ok(());
            }
        } else if !fresh && !confirmed {
            return Ok(());
        }
    }
    let mut tx = root.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    let exists = tx.local_get(&key(digest)).map_err(map_error)?.is_some();
    let mut state = read(&tx, digest)?;
    if !exists {
        state.known = fresh;
    }
    state.confirmed |= confirmed;
    write(&mut tx, digest, &state)?;
    tx.commit().map_err(map_error)
}
pub(in crate::platform::web) fn protect(host: &Host, version: &BlobVersion) -> Result<()> {
    if !host.replication_allowed(version.address.volume) {
        return Ok(());
    }
    host.blob_cache.borrow_mut().content(version.digest)?;
    let root = host.volumes.borrow()[0].index.clone();
    let mut tx = root.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    let protection = protection(version);
    if tx.local_get(&protection).map_err(map_error)?.is_some() {
        return Ok(());
    }
    let mut state = read(&tx, &version.digest)?;
    state.pending = state.pending.checked_add(1).ok_or(Error::LimitExceeded)?;
    write(&mut tx, &version.digest, &state)?;
    tx.local_indexes().put(&protection, &version.digest).map_err(map_error)?;
    tx.commit().map_err(map_error)
}
pub(in crate::platform::web) fn acknowledge(host: &Host, version: &BlobVersion) -> Result<()> {
    let root = host.volumes.borrow()[0].index.clone();
    let mut tx = root.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    let protection = protection(version);
    let mut state = read(&tx, &version.digest)?;
    if let Some(bytes) = tx.local_get(&protection).map_err(map_error)? {
        if bytes.as_ref() != version.digest {
            return Err(Error::Internal);
        }
        state.pending = state.pending.checked_sub(1).ok_or(Error::Internal)?;
        tx.local_indexes().del(&protection).map_err(map_error)?;
    }
    state.confirmed = true;
    write(&mut tx, &version.digest, &state)?;
    tx.commit().map_err(map_error)
}
