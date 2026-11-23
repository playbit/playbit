//! Independent blob versions. Durable publication and upload receipts live in
//! local index records; JavaScript owns bounded OPFS preparation and WebCrypto.
use super::*;
use crate::store::volume_index::{BlobVersion, EntryAddress, IndexSnapshot};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct FileReference {
    pub(super) volume: Id,
    pub(super) entry: EntryId,
    pub(super) revision: EntryId,
    pub(super) len_bytes: u64,
    pub(super) digest: String,
    pub(super) encrypted_key: String,
    pub(super) offset: u64,
    pub(super) limit: u32,
}
impl FileReference {
    pub(super) fn version(&self) -> Result<BlobVersion> {
        Ok(BlobVersion {
            address: EntryAddress { volume: self.volume, entry: self.entry },
            revision: self.revision,
            len_bytes: self.len_bytes,
            digest: digest(&self.digest)?,
            encrypted_key: digest(&self.encrypted_key)?,
        })
    }
    pub(super) fn from_version(version: BlobVersion, offset: u64, limit: u32) -> Self {
        Self {
            volume: version.address.volume,
            entry: version.address.entry,
            revision: version.revision,
            len_bytes: version.len_bytes,
            digest: hex(&version.digest),
            encrypted_key: hex(&version.encrypted_key),
            offset,
            limit,
        }
    }
}
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(super) fn digest(text: &str) -> Result<[u8; 32]> {
    if text.len() != 64
        || !text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidArgument);
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| Error::InvalidArgument)?;
    }
    Ok(bytes)
}
pub(super) fn pending_key(version: &BlobVersion) -> Vec<u8> {
    let mut key = b"blob-pending/".to_vec();
    key.extend_from_slice(version.address.entry.as_bytes());
    key.extend_from_slice(version.revision.as_bytes());
    key
}
impl Host {
    pub(super) fn authorize_blob(
        &self,
        volume: &Volume,
        version: &BlobVersion,
        write: bool,
        service: bool,
    ) -> Result<()> {
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let policy = if write {
            authority::check(self, volume, Some(version.address.entry))?;
            let policy = self.admit_volume(volume, Permission::Write)?;
            snapshot
                .authorize_blob_upload(
                    policy.volume(),
                    policy.default_permissions(),
                    self.actor(volume),
                    version.address.entry,
                    version.revision,
                )
                .map_err(store_error)?;
            policy
        } else {
            authority::check(self, volume, Some(version.address.entry))?;
            let policy = self.admit_volume(volume, Permission::Read)?;
            snapshot
                .authorize_retained_read(
                    policy.volume(),
                    policy.default_permissions(),
                    self.actor(volume),
                    version.address.entry,
                )
                .map_err(store_error)?;
            policy
        };
        snapshot.validate_blob_version(version).map_err(store_error)?;
        if service {
            if !policy.volume().share_key_with_service {
                return Err(Error::AccessDenied);
            }
            snapshot
                .authorize_retained_read(
                    policy.volume(),
                    policy.default_permissions(),
                    crate::store::PLAYBIT_REPLICA_PRINCIPAL,
                    version.address.entry,
                )
                .map_err(store_error)?;
        }
        Ok(())
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_digest(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, LIMIT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    publish_output(blake3::hash(&input[..length]).as_bytes().to_vec())
}
// Input: fixed volume identity followed by the exact projected index delta.
// Decode in Rust so browser publication uses the same immutable-version rules.
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_publication(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 16 + crate::store::MAX_REPLICA_DELTA_BYTES) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = (|| {
        if length < 16 {
            return Err(Error::InvalidArgument);
        }
        let volume = Id::from_bytes(input[..16].try_into().expect("checked fixed volume length"));
        let versions =
            crate::store::volume_index::published_blob_versions(volume, &input[16..length])
                .map_err(map_error)?;
        let references: Vec<_> = versions
            .into_iter()
            .map(|version| FileReference::from_version(version, 0, 0))
            .collect();
        serde_json::to_vec(&references).map_err(map_error)
    })();
    result.map_or_else(|error| error as i32, publish_output)
}

// Input: volume identity and optional exclusive entry/revision cursor. Pending
// records retain older versions; ACK never changes the current content head.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    volume: Id,
    after: Option<(EntryId, EntryId)>,
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_pending(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 512) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|cell| {
        let host = cell.borrow();
        let host = host.as_ref().ok_or(Error::Unavailable)?;
        let args: Pending = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let volume = host.replica_volume(args.volume)?;
        let tx = volume.index.begin_read().map_err(map_error)?;
        let mut start = b"blob-pending/".to_vec();
        if let Some((entry, revision)) = args.after {
            start.extend_from_slice(entry.as_bytes());
            start.extend_from_slice(revision.as_bytes());
        }
        let mut versions = Vec::new();
        let mut scanned = 0;
        let mut next = None;
        for item in tx.local_iter_from(&start).map_err(map_error)? {
            let (key, bytes) = item.map_err(map_error)?;
            if !key.starts_with(b"blob-pending/") {
                break;
            }
            let version: BlobVersion = serde_json::from_slice(&bytes).map_err(map_error)?;
            if version.address.volume != volume.id || key.as_ref() != pending_key(&version) {
                return Err(Error::Internal);
            }
            if args.after == Some((version.address.entry, version.revision)) {
                continue;
            }
            if scanned == 64 {
                break;
            }
            scanned += 1;
            next = Some((version.address.entry, version.revision));
            match host.authorize_blob(&volume, &version, true, true) {
                Ok(()) => versions.push(FileReference::from_version(version, 0, 0)),
                Err(Error::AccessDenied | Error::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        serde_json::to_vec(&(versions, if scanned == 64 { next } else { None })).map_err(map_error)
    });
    result.map_or_else(|error| error as i32, publish_output)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_acknowledge(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 1024) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|cell| {
        let host = cell.borrow();
        let host = host.as_ref().ok_or(Error::Unavailable)?;
        let reference: FileReference =
            serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let version = reference.version()?;
        let volume = host.replica_volume(version.address.volume)?;
        host.authorize_blob(&volume, &version, true, true)?;
        let mut tx = volume.index.begin_write(unsafe { now() } as u64).map_err(map_error)?;
        let key = pending_key(&version);
        let mut local = tx.local_indexes();
        if let Some(bytes) = local.get(&key).map_err(map_error)? {
            let pending: BlobVersion = serde_json::from_slice(&bytes).map_err(map_error)?;
            if pending != version {
                return Err(Error::Internal);
            }
        }
        local.del(&key).map_err(map_error)?;
        tx.commit().map_err(map_error)?;
        blob_cache::acknowledge(host, &version)
    });
    result.map_or_else(|error| error as i32, |()| 0)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorization {
    reference: FileReference,
    write: bool,
    service: bool,
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_authorize(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 1200) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|cell| {
        let host = cell.borrow();
        let host = host.as_ref().ok_or(Error::Unavailable)?;
        let request: Authorization = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        let version = request.reference.version()?;
        let volume = host.replica_volume(version.address.volume)?;
        host.authorize_blob(&volume, &version, request.write, request.service)
    });
    result.map_or_else(|error| error as i32, |()| 0)
}

// The imported wall clock reports seconds. Floor sub-microsecond precision;
// reject values outside i64 rather than relying on saturating float casts.
pub(super) fn publication_time() -> Result<i64> {
    let micros = (unsafe { now() } * 1_000_000.0).floor();
    if !micros.is_finite() || micros < i64::MIN as f64 || micros >= 9_223_372_036_854_775_808.0 {
        return Err(Error::Internal);
    }
    Ok(micros as i64)
}
