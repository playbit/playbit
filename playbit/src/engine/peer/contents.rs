//! Entry-specific transfers over the authenticated owner/recipient relay. The remote
//! entry ID comes from the AP event; file revisions and log cursors stay sealed.
use super::*;
use crate::store::{BLOB_CHUNK_BYTES, DatabaseId, EntryId, bloblog, volume_index::BlobVersion};
use std::io::{Read, Seek, Write};

const MAX_SELECTION: usize = 2048;
const TRANSFER_DEADLINE: Duration = Duration::from_secs(60);
// After receiving the body, leave room to return the outcome before the relay goes idle.
const OWNER_ADMISSION: Duration = Duration::from_secs(IDLE.as_secs() / 2);

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
    runtime.block_on(async {
        send(&mut stream, &mut cipher, PULL_FILE, &serde_json::to_vec(version)?).await?;
        let file = receive_file(store, version, &mut stream, &mut cipher, || Ok(())).await?;
        stream.finish()?;
        Ok(file)
    })
}

/// Deliver one pending immutable body. Only the owner's encrypted receipt for
/// this operation's exact version can settle the sender's durable outbox.
pub(in crate::engine) fn push_file(
    runtime: &Handle,
    connection: &ApConnection,
    store: &LocalStore,
    version: &BlobVersion,
) -> Result<()> {
    let volume = version.address.volume;
    let database = DatabaseId::Entry(version.address.entry);
    let owner = store.volume(volume)?.owner;
    let access = Access::private_volume_peer(store.account().id, owner);
    let file: Result<_> = runtime.block_on(async {
        tokio::select! {
            biased;
            closed = connection.closed() => Err(closed.into()),
            sealed = tokio::time::timeout(IDLE, store.encrypt_peer_blob_upload(&access, version)) => {
                Ok(sealed.map_err(|_| personal::PreparationBusy::Local)??)
            }
        }
    });
    let file = file?;
    let key = volume_key(store, volume, database)?;
    let (mut stream, mut cipher, _) = begin_request(
        runtime,
        connection,
        store,
        None,
        volume,
        database,
        SyncDirection::Push,
        key,
    )?;
    let selection = serde_json::to_vec(version)?;
    let transfer: Result<()> = runtime.block_on(async {
        send(&mut stream, &mut cipher, PUSH_FILE, &selection).await?;
        ready(&mut stream, &mut cipher).await?;
        // The transport can buffer the whole file before the owner receives it.
        // Give it the body transfer budget, then allow the usual receipt interval.
        let receipt_deadline = tokio::time::Instant::now() + TRANSFER_DEADLINE + IDLE;
        let sent = send_file(file, &mut stream, &mut cipher, || {
            store.validate_peer_blob_upload(&access, version)
        })
        .await;
        // Local preparation/authorization failures are already typed. An
        // owner may have sent its encrypted refusal while the write failed.
        if sent.as_ref().is_err_and(|e| {
            !e.is::<crate::network::ap::ClientError>() && !e.is::<tokio::time::error::Elapsed>()
        }) {
            return sent;
        }
        let (kind, receipt) = tokio::time::timeout_at(
            receipt_deadline,
            read_peer_frame(&mut stream, &mut cipher, MAX_SELECTION),
        )
        .await??;
        sent?;
        verify_upload_receipt(version, kind, &receipt)?;
        stream.finish()?;
        Ok(())
    });
    if let Err(error) = transfer {
        if matches!(
            error.downcast_ref::<failure::Failure>(),
            Some(failure::Failure::PrivateFileOwnerPreparationBusy {})
        ) {
            return Err(personal::PreparationBusy::Owner.into());
        }
        if matches!(
            error.downcast_ref::<failure::Failure>(),
            Some(failure::Failure::PrivateFileOwnerRead {})
        ) {
            // Only this authenticated owner's entry-scoped upload can defer
            // the version because the owner cannot receive its plaintext.
            return Err(crate::store::Error::AccessDenied(
                crate::store::Error::PRIVATE_FILE_OWNER_READ,
            )
            .into());
        }
        return Err(error);
    }
    Ok(store.acknowledge_peer_blob_upload(&access, version)?)
}

fn verify_upload_receipt(version: &BlobVersion, kind: u8, receipt: &[u8]) -> Result<()> {
    if kind != RECEIPT || serde_json::from_slice::<BlobVersion>(receipt)? != *version {
        return Err("Private file upload receipt differs from its version".into());
    }
    Ok(())
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

/// Dispatch an encrypted selector. A direct database push starts with READY.
pub(super) fn respond(
    runtime: &Handle,
    store: &LocalStore,
    access: &Access,
    event: &PeerEvent,
    stream: &mut ApStream,
    cipher: &mut Cipher,
    updates: &LiveUpdates,
) -> Result<bool> {
    let (kind, selection) = runtime.block_on(receive_bounded(stream, cipher, MAX_SELECTION))?;
    if matches!(
        (event.direction, kind),
        (SyncDirection::Pull, PULL_DATABASE) | (SyncDirection::Push, READY)
    ) {
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
        match (event.direction, kind) {
            (SyncDirection::Pull, PULL_FILE) | (SyncDirection::Push, PUSH_FILE) => {
                let version: BlobVersion = serde_json::from_slice(&selection).map_err(error)?;
                if version.address.volume != volume || version.address.entry != entry {
                    return Err(crate::store::Error::AccessDenied("private file event scope"));
                }
                if kind == PUSH_FILE {
                    accept_file(runtime, store, access, &version, stream, cipher, updates)?;
                } else {
                    let file = store.encrypt_peer_blob_version(access, &version)?;
                    runtime
                        .block_on(send_file(file, stream, cipher, || {
                            store.validate_blob_version(access, &version)
                        }))
                        .map_err(error)?;
                }
            }
            (SyncDirection::Pull, PULL_LOG) => {
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
fn accept_file(
    runtime: &Handle,
    store: &LocalStore,
    access: &Access,
    version: &BlobVersion,
    stream: &mut ApStream,
    cipher: &mut Cipher,
    updates: &LiveUpdates,
) -> crate::store::Result<()> {
    store.validate_peer_blob_upload(access, version)?;
    let mut file = runtime
        .block_on(async {
            send(stream, cipher, READY, &[]).await?;
            receive_file(store, version, stream, cipher, || {
                store.validate_peer_blob_upload(access, version)
            })
            .await
        })
        .map_err(|failure| match failure.downcast::<crate::store::Error>() {
            Ok(store) => *store,
            Err(other) => error(other),
        })?;
    let available = runtime.block_on(async {
        tokio::select! {
            biased;
            // The writer keeps the stream open until our receipt. Any more
            // input, reset, or EOF after FINISHED retires this staged request.
            _ = stream.receive_with_body_limit(0) => {
                Err(error("private file upload ended before local admission"))
            }
            received = tokio::time::timeout(OWNER_ADMISSION, store.receive_peer_blob_upload(access, version, &mut file)) => {
                received.map_err(|_| crate::store::Error::LocalContention(personal::OWNER_BUSY))?
            }
        }
    })?;
    if available {
        updates.feeds.file_available(version);
        updates.dirty.store(true, std::sync::atomic::Ordering::Release);
        updates.wake.notify();
    }
    let receipt = serde_json::to_vec(version).map_err(error)?;
    runtime.block_on(send(stream, cipher, RECEIPT, &receipt)).map_err(error)
}

async fn send_file(
    mut file: std::fs::File,
    stream: &mut ApStream,
    cipher: &mut Cipher,
    authorize: impl Fn() -> crate::store::Result<()>,
) -> Result<()> {
    tokio::time::timeout(TRANSFER_DEADLINE, async {
        let mut chunk = vec![0; BLOB_CHUNK_BYTES];
        loop {
            let count = file.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            authorize()?;
            send(stream, cipher, DATA, &chunk[..count]).await?;
        }
        send(stream, cipher, FINISHED, &[]).await
    })
    .await?
}

// File I/O here is on the calling bulk worker. A transfer holds one staging
// descriptor, one bounded frame and no store operation lock while waiting.
async fn receive_file(
    store: &LocalStore,
    version: &BlobVersion,
    stream: &mut ApStream,
    cipher: &mut Cipher,
    authorize: impl Fn() -> crate::store::Result<()>,
) -> Result<std::fs::File> {
    let mut remaining = crate::store::blob_envelope_len(version.len_bytes)?;
    let mut file =
        crate::store::blob_transfer_file(&store.profile_path().join(crate::paths::STAGING))?;
    tokio::time::timeout(TRANSFER_DEADLINE, async {
        while remaining > 0 {
            let (kind, bytes) = receive_bounded(stream, cipher, BLOB_CHUNK_BYTES).await?;
            let count = bytes.len() as u64;
            if kind != DATA || count == 0 || count > remaining {
                return Err("Invalid private file chunk".into());
            }
            authorize()?;
            file.write_all(&bytes)?;
            remaining -= count;
        }
        let (kind, data) = receive_bounded(stream, cipher, MAX_SELECTION).await?;
        if kind != FINISHED || !data.is_empty() {
            return Err("Invalid private file completion".into());
        }
        file.rewind()?;
        Ok(file)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_upload_receipt_identifies_its_exact_revision() -> Result<()> {
        // Matching file contents can occur in distinct entries and revisions.
        // Neither a previous receipt nor a transport completion settles a write.
        let mut version = BlobVersion {
            address: crate::store::volume_index::EntryAddress {
                volume: Id::generate()?,
                entry: EntryId::generate()?,
            },
            revision: EntryId::generate()?,
            len_bytes: 7,
            digest: [1; 32],
            encrypted_key: [2; 32],
        };
        let receipt = serde_json::to_vec(&version)?;
        verify_upload_receipt(&version, RECEIPT, &receipt)?;
        assert!(verify_upload_receipt(&version, FINISHED, &receipt).is_err());
        version.revision = EntryId::generate()?;
        assert!(verify_upload_receipt(&version, RECEIPT, &receipt).is_err());
        Ok(())
    }
}
