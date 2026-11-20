//! Deterministic, bounded block envelopes shared by native and web clients.
use super::{Error, Id, Result};

pub const BLOB_CHUNK_BYTES: usize = 64 * 1024;
pub const BLOB_ENVELOPE_VERSION: u8 = 1;
pub const BLOB_HEADER_BYTES: usize = 9;
const TAG_BYTES: u64 = 16;
const DOMAIN: &[u8] = b"app.playbit.volume.blob.v1\0";
pub const MAX_BLOB_PLAINTEXT_BYTES: u64 = playbit_api::abi::files::MAX_FILE_EXTENT;
/// The transfer bound includes authentication overhead for a full-size File.
pub const MAX_BLOB_ENVELOPE_BYTES: u64 = MAX_BLOB_PLAINTEXT_BYTES
    + BLOB_HEADER_BYTES as u64
    + TAG_BYTES * MAX_BLOB_PLAINTEXT_BYTES.div_ceil(BLOB_CHUNK_BYTES as u64);

fn block_count(length: u64) -> u64 {
    length.div_ceil(BLOB_CHUNK_BYTES as u64).max(1)
}

/// Exact opaque AP body size, including the authenticated empty-content block.
pub fn blob_envelope_len(length: u64) -> Result<u64> {
    if length > MAX_BLOB_PLAINTEXT_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(BLOB_HEADER_BYTES as u64 + length + TAG_BYTES * block_count(length))
}

/// A content-scoped cipher key. Distinct volumes and plaintext digests select
/// distinct keys; nonce counters are used only within one immutable content.
pub fn blob_content_key(secret: &[u8; 32], volume: Id, digest: &[u8; 32]) -> [u8; 32] {
    let mut material = [0; 80];
    material[..32].copy_from_slice(secret);
    material[32..48].copy_from_slice(volume.as_bytes());
    material[48..].copy_from_slice(digest);
    blake3::derive_key("app.playbit.volume.blob-cipher.v1", &material)
}

pub fn blob_block_nonce(index: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&index.to_be_bytes());
    nonce
}

pub fn blob_block_aad(
    volume: Id,
    digest: &[u8; 32],
    length: u64,
    index: u64,
) -> [u8; DOMAIN.len() + 64] {
    let mut aad = [0; DOMAIN.len() + 64];
    let offset = DOMAIN.len();
    aad[..offset].copy_from_slice(DOMAIN);
    aad[offset..offset + 16].copy_from_slice(volume.as_bytes());
    aad[offset + 16..offset + 48].copy_from_slice(digest);
    aad[offset + 48..offset + 56].copy_from_slice(&length.to_le_bytes());
    aad[offset + 56..].copy_from_slice(&index.to_le_bytes());
    aad
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
mod stream;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub use stream::{open_blob_to, seal_blob_to};
