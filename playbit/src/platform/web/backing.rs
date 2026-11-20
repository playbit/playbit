//! Physical backing locations belong to the persistent index's local metadata.
use super::*;

pub(super) fn key(entry: EntryId) -> [u8; 16] {
    let mut key = [18; 16];
    key[1..].copy_from_slice(entry.as_bytes());
    key
}

pub(super) fn location(volume: &Volume, entry: EntryId) -> Result<Option<Id>> {
    let read = volume.index.begin_read().map_err(map_error)?;
    read.local_get(&key(entry))
        .map_err(map_error)?
        .map(|bytes| {
            let value: [u8; 16] = bytes.as_ref().try_into().map_err(|_| Error::Internal)?;
            if value == [0; 16] {
                return Err(Error::Internal);
            }
            Ok(Id::from_bytes(value))
        })
        .transpose()
}

pub(super) fn bind(volume: &Volume, entry: EntryId, generation: Id) -> Result<()> {
    if generation.as_bytes() == &[0; 16] {
        return Err(Error::InvalidArgument);
    }
    let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
    tx.local_indexes().put(&key(entry), generation.as_bytes()).map_err(map_error)?;
    tx.commit().map_err(map_error)
}
