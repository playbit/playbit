//! Opaque volume logs for multiplayer experiments.
//!
//! `put` sends one online append and waits for AP durability. Retain the append
//! ID and repeat it with the same payload to resolve an uncertain result.
//! `get` returns available cached entries offline; an empty tail requires AP.
//! Keep the returned local cursor; delivery can repeat stable entry IDs.
//!
//! Payloads are stored and returned byte for byte without encryption or
//! interpretation. Callers own serialization, confidentiality and cryptographic
//! authenticity. Plaintext payloads are readable by AP.
use crate::engine::{Result, ipc, profile_path, tool_client::DataContext};
use crate::runtime::host::{DataCall, DataOperation, DataResult};
pub use crate::store::{
    EntryId, Id,
    bloblog::{Cursor, Entry, MAX_BLOB_BYTES, MAX_BLOBS, MAX_BYTES, Page},
};
use std::sync::Arc;
use tokio::sync::Semaphore;

fn slots() -> &'static Arc<Semaphore> {
    static SLOTS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(8)))
}
async fn call(volume: Id, operation: DataOperation) -> Result<DataResult> {
    // Fail admission instead of accumulating arbitrarily many waiting payloads.
    let permit =
        slots().clone().try_acquire_owned().map_err(|_| "too many concurrent bloblog calls")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        crate::engine::profile::initialize()?;
        let profile = profile_path();
        let exchange = |request| ipc::call_archive(&profile, &request)?.into_owned();
        let mut context = DataContext::open_volume(volume, &exchange)?;
        let result = context.execute(DataCall { volume: *volume.as_bytes(), operation });
        context.finish(result)
    })
    .await?
}

/// Create an explicitly named append log before publishing any data.
pub async fn create(volume: Id, name: String) -> Result<crate::store::volume_index::VolumeEntry> {
    crate::store::volume_index::validate_name(&name)?;
    let permit =
        slots().clone().try_acquire_owned().map_err(|_| "too many concurrent bloblog calls")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        crate::engine::profile::initialize()?;
        let profile = profile_path();
        let reply = ipc::call_archive(
            &profile,
            &crate::engine::Request::BloblogCreate { session: None, volume, name },
        )?
        .into_owned()?;
        match reply {
            crate::engine::reply::Reply::VolumeEntry { entry, .. } => Ok(entry),
            _ => Err("unexpected append log creation reply".into()),
        }
    })
    .await?
}

/// Append one bounded batch of arbitrary bytes, including empty blobs.
/// Reuse the same ID and payload after an uncertain result. Different IDs append
/// distinct batches. The runtime never resubmits a failed invocation.
pub async fn put(
    volume: impl Into<Id>,
    log: EntryId,
    append_id: Id,
    blobs: Vec<Vec<u8>>,
) -> Result<Cursor> {
    crate::store::bloblog::validate_blobs(&blobs)?;
    match call(
        volume.into(),
        DataOperation::BloblogAppend {
            log: *log.as_bytes(),
            append_id: *append_id.as_bytes(),
            blobs,
        },
    )
    .await?
    {
        DataResult::BloblogAppended(cursor) => Ok(cursor),
        _ => Err("unexpected bloblog append reply".into()),
    }
}

/// Read up to 128 entries / 64 KiB. Follow `more` with the returned cursor to
/// drain cached entries. None begins at the start. An empty local tail fetches a
/// bounded remote page; offline tails fail. `more=false` describes local coverage.
pub async fn get(volume: impl Into<Id>, log: EntryId, cursor: Option<Cursor>) -> Result<Page> {
    match call(volume.into(), DataOperation::BloblogRead { log: *log.as_bytes(), cursor }).await? {
        DataResult::BloblogPage(page) => Ok(page),
        _ => Err("unexpected bloblog page reply".into()),
    }
}
