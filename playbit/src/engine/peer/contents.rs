//! Entry-specific pulls over the authenticated owner/recipient relay. The remote
//! entry ID comes from the AP event; file revisions and log cursors stay sealed.
use super::*;
use crate::store::{BLOB_CHUNK_BYTES, DatabaseId, EntryId, bloblog, volume_index::BlobVersion};
use std::io::{Read, Seek, Write};

const MAX_SELECTION: usize = 2048;
const TRANSFER_DEADLINE: Duration = Duration::from_secs(60);

pub(in crate::engine) fn pull_file(
    runtime: &Handle,
    connection: &ApConnection,
    store: &LocalStore,
    version: &BlobVersion,
) -> Result<std::fs::File> {
    let volume = version.address.volume;
    let database = DatabaseId::Entry(version.address.entry);
    let key = volume_key(store, volume, database)?;
    let (mut stream, mut cipher, _) = begin_request(
        runtime,
        connection,
        store,
        None,
        volume,
        database,
        SyncDirection::Pull,
        key,
    )?;
    let mut file =
        crate::store::blob_transfer_file(&store.profile_path().join(crate::paths::STAGING))?;
    let mut remaining = crate::store::blob_envelope_len(version.len_bytes)?;
    // The caller owns a blocking file worker and an immutable-transfer credit.
    runtime.block_on(async {
        tokio::time::timeout(TRANSFER_DEADLINE, async {
            send(&mut stream, &mut cipher, PULL_FILE, &serde_json::to_vec(version)?).await?;
            while remaining > 0 {
                let (kind, bytes) = receive(&mut stream, &mut cipher).await?;
                let count = bytes.len() as u64;
                if kind != DATA || count == 0 || bytes.len() > BLOB_CHUNK_BYTES || count > remaining
                {
                    return Err("Invalid private file chunk".into());
                }
                file.write_all(&bytes)?;
                remaining -= count;
            }
            let (kind, data) = receive(&mut stream, &mut cipher).await?;
            if kind != FINISHED || !data.is_empty() {
                return Err("Invalid private file completion".into());
            }
            stream.finish()?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await?
    })?;
    file.rewind()?;
    Ok(file)
}

pub(in crate::engine) fn pull_log_page(
    runtime: &Runtime,
    connection: &ApConnection,
    store: &LocalStore,
    volume: Id,
    log: EntryId,
    cursor: Option<&bloblog::Cursor>,
) -> Result<bloblog::ReplicaPage> {
    let database = DatabaseId::Entry(log);
    let key = volume_key(store, volume, database)?;
    let (mut stream, mut cipher, _) = begin_request(
        runtime.handle(),
        connection,
        store,
        None,
        volume,
        database,
        SyncDirection::Pull,
        key,
    )?;
    runtime.block_on(async {
        send(&mut stream, &mut cipher, PULL_LOG, &serde_json::to_vec(&cursor)?).await?;
        let (kind, bytes) = receive(&mut stream, &mut cipher).await?;
        if kind != RECEIPT || bytes.len() > MAX_SELECTION {
            return Err("Invalid private log page header".into());
        }
        let mut receipt: bloblog::ReplicaPage = serde_json::from_slice(&bytes)?;
        if !receipt.page.entries.is_empty() {
            return Err("Unexpected entries in private log header".into());
        }
        let (kind, bytes) = receive(&mut stream, &mut cipher).await?;
        if kind != DATA || bytes.len() > bloblog::MAX_BYTES {
            return Err("Invalid private log page".into());
        }
        receipt.page.entries = bloblog::decode(&bytes)?;
        stream.finish()?;
        Ok(receipt)
    })
}

/// Return true when an entry-specific pull consumed this request.
pub(super) fn respond(
    runtime: &Handle,
    store: &LocalStore,
    access: &Access,
    event: &PeerEvent,
    stream: &mut ApStream,
    cipher: &mut Cipher,
) -> Result<bool> {
    if !event.session_id.is_empty() || event.direction != SyncDirection::Pull {
        return Ok(false);
    }
    let (kind, selection) = runtime.block_on(receive(stream, cipher))?;
    if kind == PULL_DATABASE {
        if !selection.is_empty() {
            return Err("Invalid private database selection".into());
        }
        return Ok(false);
    }
    let volume = Id::parse(&event.volume_id)?;
    let result = (|| -> crate::store::Result<()> {
        let DatabaseId::Entry(entry) = event.database_id else {
            return Err(crate::store::Error::InvalidInput(
                "private entry selection requires an entry",
            ));
        };
        if selection.len() > MAX_SELECTION {
            return Err(crate::store::Error::LimitExceeded);
        }
        match kind {
            PULL_FILE => {
                let version: BlobVersion = serde_json::from_slice(&selection).map_err(error)?;
                if version.address.volume != volume || version.address.entry != entry {
                    return Err(crate::store::Error::AccessDenied("private file event scope"));
                }
                send_file(runtime, store, access, &version, stream, cipher)?;
            }
            PULL_LOG => {
                let cursor: Option<bloblog::Cursor> =
                    serde_json::from_slice(&selection).map_err(error)?;
                let mut receipt = store.private_log_page(access, volume, entry, cursor.as_ref())?;
                let entries = std::mem::take(&mut receipt.page.entries);
                let bytes = bloblog::encode(&entries)?;
                let header = serde_json::to_vec(&receipt).map_err(error)?;
                runtime
                    .block_on(async {
                        send(stream, cipher, RECEIPT, &header).await?;
                        send(stream, cipher, DATA, &bytes).await
                    })
                    .map_err(error)?;
            }
            _ => return Err(crate::store::Error::InvalidInput("unknown private entry selection")),
        }
        Ok(())
    })();
    if let Err(error) = result {
        runtime.block_on(send(stream, cipher, ERROR, &failure::encode(&error)?))?;
    }
    stream.finish()?;
    Ok(true)
}
fn send_file(
    runtime: &Handle,
    store: &LocalStore,
    access: &Access,
    version: &BlobVersion,
    stream: &mut ApStream,
    cipher: &mut Cipher,
) -> crate::store::Result<()> {
    let mut file = store.encrypt_peer_blob_version(access, version)?;
    let mut chunk = vec![0; BLOB_CHUNK_BYTES];
    runtime
        .block_on(async {
            tokio::time::timeout(TRANSFER_DEADLINE, async {
                loop {
                    let count = file.read(&mut chunk)?;
                    if count == 0 {
                        break;
                    }
                    store.validate_blob_version(access, version)?;
                    send(stream, cipher, DATA, &chunk[..count]).await?;
                }
                send(stream, cipher, FINISHED, &[]).await
            })
            .await?
        })
        .map_err(error)?;
    Ok(())
}
