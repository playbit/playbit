//! Capture immutable file versions from the exact index delta sent to AP.
use super::*;
use std::collections::BTreeMap;

/// Never scan the current pending queue here: it can already contain newer
/// revisions whose index declarations have not reached AP yet. The delta byte
/// limit also bounds the number of declarations retained by this job.
pub(crate) fn blob_versions(volume: Id, bytes: &[u8]) -> Result<Vec<BlobVersion>> {
    let mut versions = BTreeMap::new();
    let mut result: Result<()> = Ok(());
    kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
        if result.is_err() {
            return;
        }
        result = (|| {
            let EntState::Present { value, .. } = state else { return Ok(()) };
            if let Some(version) = blob_version(volume, key, value)? {
                versions.insert((version.address.entry, version.revision), version);
            }
            Ok(())
        })();
    })?;
    result?;
    Ok(versions.into_values().collect())
}

fn blob_version(volume: Id, key: &[u8], value: &[u8]) -> Result<Option<BlobVersion>> {
    let (revision, kind) = if key.len() == 16 && key[0] == ENTRY_ID_BYTE {
        let info = info(value)?;
        (info.created_revision, info.kind)
    } else if (key.len() == 17 && key[..2] == [META_BYTE, CONTENT])
        || (key.len() == 32 && key[..2] == [META_BYTE, REVISION])
    {
        let change = revision(value)?;
        let kind = match change.change {
            EntryChange::Created { info } => info.kind,
            EntryChange::Content { kind } => kind,
            _ => return Ok(None),
        };
        (change.revision, kind)
    } else {
        return Ok(None);
    };
    let EntryKind::Blob { len_bytes, digest, encrypted_key, .. } = kind else {
        return Ok(None);
    };
    let address = EntryAddress { volume, entry: source_entry_id(key)? };
    Ok(Some(BlobVersion { address, revision, len_bytes, digest, encrypted_key }))
}
