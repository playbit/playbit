//! Bounded operations over an ordinary named KV backing. Admission belongs to
//! the caller's index/volume boundary; a root identifies content, not authority.
use super::{Error, Result};
use kvdb::storage::{Storage, ValueRead};

pub const MAX_KEYS: usize = 64;
/// One bounded prefix per listed value, from the same authorized read snapshot.
pub const PREVIEW_BYTES: usize = 128;
pub const MAX_BYTES: usize = 60 * 1024;
pub const MAX_STORED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Root {
    pub level: u8,
    pub digest: [u8; 32],
}
impl From<(u8, [u8; 32])> for Root {
    fn from((level, digest): (u8, [u8; 32])) -> Self {
        Self { level, digest }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Key {
    pub bytes: Vec<u8>,
    pub len_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Page {
    pub keys: Vec<Key>,
    pub next: Option<Vec<u8>>,
    pub root: Option<Root>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Chunk {
    pub root: Root,
    pub offset: u64,
    pub total_bytes: u64,
    pub bytes: Vec<u8>,
}

pub fn validate_key(key: &[u8]) -> Result<()> {
    if key.is_empty() || key.len() > kvdb::KEY_SIZE_MAX {
        return Err(Error::InvalidInput("KV key must contain 1–511 bytes"));
    }
    Ok(())
}

pub fn page<S: Storage>(
    tx: &kvdb::ReadTxn<'_, S>,
    after: Option<&[u8]>,
    expected: Option<Root>,
    limit: usize,
) -> Result<Page> {
    if limit == 0 || limit > MAX_KEYS || (after.is_some() && expected.is_none()) {
        return Err(Error::InvalidInput("invalid KV page cursor or limit"));
    }
    if let Some(after) = after {
        validate_key(after)?;
    }
    let root = tx.root_hash()?.map(Root::from);
    check_root(root, expected)?;
    let mut cursor = after.unwrap_or_default().to_vec();
    let mut exclusive = after.is_some();
    let mut keys = Vec::with_capacity(limit);
    // Visit source keys, including tombstones. next_key's tombstone skipping
    // loop has no per-call work bound. An empty page may still have a cursor.
    for _ in 0..MAX_KEYS {
        let Some(next) = tx.next_source_key(&cursor, exclusive)? else {
            return Ok(Page { keys, next: None, root });
        };
        validate_key(&next)?;
        if let Some(length) = tx.value_len(&next)? {
            if length > MAX_STORED_BYTES {
                return Err(Error::LimitExceeded);
            }
            let reader = tx.value_reader(&next)?.ok_or(Error::NotFound("KV key"))?;
            let mut preview = vec![0; length.min(PREVIEW_BYTES)];
            reader.read_exact_at(0, &mut preview)?;
            keys.push(Key { bytes: next.clone(), len_bytes: length as u64, preview });
        }
        cursor = next;
        exclusive = true;
        if keys.len() == limit {
            break;
        }
    }
    assert!(!cursor.is_empty() && keys.len() <= limit, "bounded source progress");
    Ok(Page { keys, next: Some(cursor), root })
}

fn check_root(current: Option<Root>, expected: Option<Root>) -> Result<()> {
    if expected.is_some() && current != expected {
        return Err(Error::LocalContention("KV snapshot changed"));
    }
    Ok(())
}

fn chunk_len(total: u64, offset: u64, limit: u32) -> Result<usize> {
    if offset > total || total > MAX_STORED_BYTES as u64 || limit == 0 || limit as usize > MAX_BYTES
    {
        return Err(Error::InvalidInput("invalid KV read range"));
    }
    Ok((total - offset).min(u64::from(limit)) as usize)
}

pub fn read<S: Storage>(
    tx: &kvdb::ReadTxn<'_, S>,
    key: &[u8],
    offset: u64,
    limit: u32,
    expected: Option<Root>,
) -> Result<Chunk> {
    validate_key(key)?;
    if offset != 0 && expected.is_none() {
        return Err(Error::InvalidInput("later KV chunks require a snapshot root"));
    }
    let root = tx.root_hash()?.map(Root::from);
    check_root(root, expected)?;
    let reader = tx.value_reader(key)?.ok_or(Error::NotFound("KV key"))?;
    let total_bytes = reader.len() as u64;
    let mut bytes = vec![0; chunk_len(total_bytes, offset, limit)?];
    reader.read_exact_at(offset as usize, &mut bytes)?;
    Ok(Chunk {
        root: root.ok_or(Error::corrupt("key-value chunk has no replica root"))?,
        offset,
        total_bytes,
        bytes,
    })
}

/// Supplied writes need no Read grant. Comparing a bounded supplied value for
/// idempotence copies at most one 4-KiB window of the existing value.
#[cfg(any(target_arch = "wasm32", feature = "local-store", test))]
pub(crate) fn identical(reader: &impl ValueRead, supplied: &[u8]) -> Result<bool> {
    if reader.len() != supplied.len() {
        return Ok(false);
    }
    let mut scratch = [0; 4096];
    for (index, chunk) in supplied.chunks(scratch.len()).enumerate() {
        reader.read_exact_at(index * scratch.len(), &mut scratch[..chunk.len()])?;
        if &scratch[..chunk.len()] != chunk {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
