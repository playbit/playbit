#![allow(clippy::unwrap_used, reason = "Fixtures assert successful independent AEAD blocks.")]
use super::*;
use crate::store::{EntryId, volume_index::EntryAddress};

fn seal(plaintext: &[u8], volume: Id) -> (Vec<u8>, BlobVersion) {
    let digest = *blake3::hash(plaintext).as_bytes();
    let mut envelope = Vec::new();
    let encrypted_key = seal_blob_to(
        &[7; 32],
        volume,
        &digest,
        plaintext.len() as u64,
        &mut &plaintext[..],
        &mut envelope,
    )
    .unwrap();
    assert_eq!(blob_envelope_len(plaintext.len() as u64).unwrap(), envelope.len() as u64);
    assert_eq!(encrypted_key, *blake3::hash(&envelope).as_bytes());
    (
        envelope,
        BlobVersion {
            address: EntryAddress { volume, entry: EntryId::from_bytes([1; 15]).unwrap() },
            revision: EntryId::from_bytes([2; 15]).unwrap(),
            len_bytes: plaintext.len() as u64,
            digest,
            encrypted_key,
        },
    )
}

fn open(envelope: &[u8], version: &BlobVersion) -> Result<Vec<u8>> {
    let mut plaintext = Vec::new();
    open_blob_to(&[7; 32], version, &mut &envelope[..], &mut plaintext)?;
    Ok(plaintext)
}

#[test]
fn block_boundaries_empty_content_and_deterministic_cross_volume_separation() {
    for length in [0, 1, 15, 16, 17, 65535, 65536, 65537, 128 * 1024 + 1, 8 * 1024 * 1024] {
        let plaintext = (0..length).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        let volume = Id::from_bytes([3; 16]);
        let (envelope, version) = seal(&plaintext, volume);
        assert_eq!(open(&envelope, &version).unwrap(), plaintext);
        assert_eq!(seal(&plaintext, volume).0, envelope);
        assert_ne!(seal(&plaintext, Id::from_bytes([4; 16])).0, envelope);
    }
}

#[test]
fn maximum_file_extent_round_trips_with_authentication_overhead() {
    // The declared File extent must survive the complete encryption path, not
    // just admission. One additional plaintext byte is rejected before I/O.
    let plaintext = vec![0x5a; playbit_api::abi::files::MAX_FILE_EXTENT as usize];
    let (envelope, version) = seal(&plaintext, Id::from_bytes([3; 16]));
    assert_eq!(envelope.len() as u64, MAX_BLOB_ENVELOPE_BYTES);
    assert_eq!(open(&envelope, &version).unwrap(), plaintext);
    assert!(matches!(blob_envelope_len(version.len_bytes + 1), Err(Error::LimitExceeded)));
}

#[test]
fn every_small_header_ciphertext_and_tag_corruption_is_rejected() {
    let (envelope, version) = seal(b"authenticated content", Id::from_bytes([3; 16]));
    for index in 0..envelope.len() {
        let mut modified = envelope.clone();
        modified[index] ^= 1;
        let mut reference = version.clone();
        // Even an index containing the forged envelope digest cannot make an
        // unauthenticated block valid for the declared plaintext digest.
        reference.encrypted_key = *blake3::hash(&modified).as_bytes();
        assert!(open(&modified, &reference).is_err());
    }
    for length in 0..envelope.len() {
        assert!(open(&envelope[..length], &version).is_err());
    }
    let mut extended = envelope.clone();
    extended.push(0);
    assert!(open(&extended, &version).is_err());
    let mut replaced = version.clone();
    replaced.address.volume = Id::from_bytes([4; 16]);
    assert!(open(&envelope, &replaced).is_err());
    replaced = version.clone();
    replaced.digest[0] ^= 1;
    assert!(open(&envelope, &replaced).is_err());
    assert!(open_blob_to(&[8; 32], &version, &mut &envelope[..], &mut std::io::sink()).is_err());
}

#[test]
fn complete_blocks_cannot_be_reordered_spliced_or_truncated() {
    let plaintext = (0..3 * BLOB_CHUNK_BYTES).map(|n| (n % 251) as u8).collect::<Vec<_>>();
    let (envelope, version) = seal(&plaintext, Id::from_bytes([3; 16]));
    let block_size = BLOB_CHUNK_BYTES + TAG_BYTES as usize;
    let mut reversed = envelope.clone();
    let body = &mut reversed[BLOB_HEADER_BYTES..];
    body[..block_size].copy_from_slice(
        &envelope[BLOB_HEADER_BYTES + block_size..BLOB_HEADER_BYTES + 2 * block_size],
    );
    body[block_size..2 * block_size]
        .copy_from_slice(&envelope[BLOB_HEADER_BYTES..BLOB_HEADER_BYTES + block_size]);
    let mut reference = version.clone();
    reference.encrypted_key = *blake3::hash(&reversed).as_bytes();
    assert!(open(&reversed, &reference).is_err());
    let (other, _) = seal(&vec![9; plaintext.len()], version.address.volume);
    let mut spliced = envelope.clone();
    spliced[BLOB_HEADER_BYTES..BLOB_HEADER_BYTES + block_size]
        .copy_from_slice(&other[BLOB_HEADER_BYTES..BLOB_HEADER_BYTES + block_size]);
    reference.encrypted_key = *blake3::hash(&spliced).as_bytes();
    assert!(open(&spliced, &reference).is_err());
    assert!(open(&envelope[..envelope.len() - block_size], &version).is_err());
}

struct Irregular<'a> {
    source: &'a [u8],
    count: usize,
}
impl Read for Irregular<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = bytes.len().min(self.source.len()).min(self.count);
        bytes[..count].copy_from_slice(&self.source[..count]);
        self.source = &self.source[count..];
        Ok(count)
    }
}

#[test]
fn irregular_reader_chunks_and_private_destination_failures_are_explicit() {
    let plaintext = vec![0x5a; BLOB_CHUNK_BYTES + 17];
    let (envelope, version) = seal(&plaintext, Id::from_bytes([3; 16]));
    for count in [1, 7, 15, 17, 4095] {
        let mut received = Vec::new();
        open_blob_to(
            &[7; 32],
            &version,
            &mut Irregular { source: &envelope, count },
            &mut received,
        )
        .unwrap();
        assert_eq!(received, plaintext);
        let mut sealed = Vec::new();
        let digest = seal_blob_to(
            &[7; 32],
            version.address.volume,
            &version.digest,
            version.len_bytes,
            &mut Irregular { source: &plaintext, count },
            &mut sealed,
        )
        .unwrap();
        assert_eq!(sealed, envelope);
        assert_eq!(digest, version.encrypted_key);
    }
    struct Refuse;
    impl Write for Refuse {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::StorageFull.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(open_blob_to(&[7; 32], &version, &mut &envelope[..], &mut Refuse).is_err());
    assert!(
        seal_blob_to(
            &[7; 32],
            version.address.volume,
            &[0; 32],
            version.len_bytes,
            &mut &plaintext[..],
            &mut std::io::sink()
        )
        .is_err()
    );
}
