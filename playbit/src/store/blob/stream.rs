//! Authenticate and hash blocks into caller-owned private staging only.
use super::*;
use crate::store::volume_index::BlobVersion;
use std::io::{Read, Write};

struct BlockCipher {
    key: ring::aead::LessSafeKey,
    volume: Id,
    digest: [u8; 32],
    length: u64,
}
impl BlockCipher {
    fn new(secret: &[u8; 32], volume: Id, digest: [u8; 32], length: u64) -> Result<Self> {
        blob_envelope_len(length)?;
        let key = ring::aead::UnboundKey::new(
            &ring::aead::AES_256_GCM,
            &blob_content_key(secret, volume, &digest),
        )
        .map_err(|_| Error::Internal("could not construct the blob encryption key"))?;
        Ok(Self { key: ring::aead::LessSafeKey::new(key), volume, digest, length })
    }

    fn seal(&self, index: u64, bytes: &mut [u8]) -> Result<ring::aead::Tag> {
        self.key
            .seal_in_place_separate_tag(
                ring::aead::Nonce::assume_unique_for_key(blob_block_nonce(index)),
                ring::aead::Aad::from(blob_block_aad(
                    self.volume,
                    &self.digest,
                    self.length,
                    index,
                )),
                bytes,
            )
            .map_err(|_| Error::Internal("blob encryption failed"))
    }

    fn open<'a>(&self, index: u64, bytes: &'a mut [u8]) -> Result<&'a mut [u8]> {
        self.key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(blob_block_nonce(index)),
                ring::aead::Aad::from(blob_block_aad(
                    self.volume,
                    &self.digest,
                    self.length,
                    index,
                )),
                bytes,
            )
            .map_err(|_| Error::corrupt("blob authentication failed"))
    }
}

fn header(length: u64) -> [u8; BLOB_HEADER_BYTES] {
    let mut bytes = [0; BLOB_HEADER_BYTES];
    bytes[0] = BLOB_ENVELOPE_VERSION;
    bytes[1..].copy_from_slice(&length.to_le_bytes());
    bytes
}

fn exact(source: &mut impl Read, bytes: &mut [u8]) -> Result<()> {
    source.read_exact(bytes).map_err(|error| Error::read_failure("read blob stream", error))
}

fn end(source: &mut impl Read) -> Result<()> {
    if source.read(&mut [0]).map_err(|error| Error::System(error.to_string()))? != 0 {
        return Err(Error::corrupt("blob stream has trailing bytes"));
    }
    Ok(())
}

fn write_hashed(target: &mut impl Write, hash: &mut blake3::Hasher, bytes: &[u8]) -> Result<()> {
    target.write_all(bytes).map_err(|error| Error::System(error.to_string()))?;
    hash.update(bytes);
    Ok(())
}

/// Stream one envelope. The destination is untrusted staging until success;
/// discard it on every error. Return the digest used as `encrypted_key`.
pub fn seal_blob_to(
    secret: &[u8; 32],
    volume: Id,
    digest: &[u8; 32],
    length: u64,
    source: &mut impl Read,
    target: &mut impl Write,
) -> Result<[u8; 32]> {
    let cipher = BlockCipher::new(secret, volume, *digest, length)?;
    let mut plaintext = blake3::Hasher::new();
    let mut encrypted = blake3::Hasher::new();
    write_hashed(target, &mut encrypted, &header(length))?;
    let mut remaining = length;
    let mut buffer = [0; BLOB_CHUNK_BYTES];
    for index in 0..block_count(length) {
        let count = remaining.min(BLOB_CHUNK_BYTES as u64) as usize;
        exact(source, &mut buffer[..count])?;
        plaintext.update(&buffer[..count]);
        let tag = cipher.seal(index, &mut buffer[..count])?;
        write_hashed(target, &mut encrypted, &buffer[..count])?;
        write_hashed(target, &mut encrypted, tag.as_ref())?;
        remaining -= count as u64;
    }
    end(source)?;
    if plaintext.finalize().as_bytes() != digest {
        return Err(Error::corrupt("blob plaintext digest does not match its declaration"));
    }
    Ok(*encrypted.finalize().as_bytes())
}

/// Stream into a private destination. Every tag, exact EOF and both digests
/// must pass before the caller may publish any destination bytes.
pub fn open_blob_to(
    secret: &[u8; 32],
    version: &BlobVersion,
    source: &mut impl Read,
    target: &mut impl Write,
) -> Result<()> {
    let cipher =
        BlockCipher::new(secret, version.address.volume, version.digest, version.len_bytes)?;
    let mut plaintext = blake3::Hasher::new();
    let mut encrypted = blake3::Hasher::new();
    let mut received_header = [0; BLOB_HEADER_BYTES];
    exact(source, &mut received_header)?;
    if received_header != header(version.len_bytes) {
        return Err(Error::corrupt("blob header differs from its declared format or length"));
    }
    encrypted.update(&received_header);
    let mut remaining = version.len_bytes;
    let mut buffer = [0; BLOB_CHUNK_BYTES + TAG_BYTES as usize];
    for index in 0..block_count(version.len_bytes) {
        let count = remaining.min(BLOB_CHUNK_BYTES as u64) as usize;
        let ciphertext = &mut buffer[..count + TAG_BYTES as usize];
        exact(source, ciphertext)?;
        encrypted.update(ciphertext);
        let opened = cipher.open(index, ciphertext)?;
        if opened.len() != count {
            return Err(Error::corrupt("decrypted blob chunk has the wrong length"));
        }
        write_hashed(target, &mut plaintext, opened)?;
        remaining -= count as u64;
    }
    end(source)?;
    if encrypted.finalize().as_bytes() != &version.encrypted_key
        || plaintext.finalize().as_bytes() != &version.digest
    {
        return Err(Error::corrupt(
            "blob ciphertext or plaintext digest does not match its declaration",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
