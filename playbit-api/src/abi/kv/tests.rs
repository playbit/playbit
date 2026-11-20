#![allow(clippy::unwrap_used, reason = "Fixtures assert exact KV frame validation.")]
extern crate std;
use super::*;
use std::{vec, vec::Vec};

#[test]
fn pages_round_trip_binary_keys_and_tombstone_only_progress() {
    let root = Some(Root { level: 7, digest: [0x91; 32] });
    let keys = [
        Key { bytes: &[0, 0xff], len_bytes: MAX_STORED_BYTES },
        Key { bytes: &[0xfe; MAX_KEY_BYTES], len_bytes: 0 },
    ];
    let mut output = vec![0; MAX_PAGE_BYTES];
    let length = encode_page(&mut output, root, keys.into_iter(), Some(keys[1].bytes)).unwrap();
    let page = decode_page(&output[..length]).unwrap();
    assert_eq!(page.root, root);
    assert_eq!(page.keys.collect::<Vec<_>>(), keys);
    assert_eq!(page.next, Some(keys[1].bytes));
    for end in 0..length {
        assert!(decode_page(&output[..end]).is_err());
    }
    output[length] = 0;
    assert!(decode_page(&output[..length + 1]).is_err());
    let length = encode_page(&mut output, root, core::iter::empty(), Some(b"progress")).unwrap();
    let page = decode_page(&output[..length]).unwrap();
    assert_eq!(page.keys.len(), 0);
    assert_eq!(page.next, Some(b"progress".as_slice()));
}

#[test]
fn malformed_and_oversized_frames_fail_explicitly() {
    let mut output = vec![0; MAX_PAGE_BYTES];
    let keys = [Key { bytes: b"z", len_bytes: 1 }, Key { bytes: b"a", len_bytes: 1 }];
    assert!(encode_page(&mut output, Some(Root::default()), keys.into_iter(), None).is_err());
    assert!(encode_page(&mut output, None, core::iter::empty(), Some(b"cursor")).is_err());
    assert_eq!(
        encode_page(&mut [0; 39], None, core::iter::empty(), None),
        Err(Error::BufferTooSmall)
    );
    let length = encode_page(&mut output, None, core::iter::empty(), None).unwrap();
    output[0] = 2;
    assert!(decode_page(&output[..length]).is_err());
    assert!(root(0, Root { level: 1, digest: [0; 32] }).is_err());
    assert!(root(1, Root { level: 256, digest: [0; 32] }).is_err());
    let keys = [Key { bytes: &[0xff; MAX_KEY_BYTES], len_bytes: MAX_STORED_BYTES }; MAX_KEYS];
    assert!(encode_page(&mut output, Some(Root::default()), keys.into_iter(), None).is_err());
}

#[test]
fn chunks_round_trip_and_validate_remaining_length() {
    let bytes = vec![0x81; MAX_VALUE_BYTES];
    let chunk = Chunk {
        root: Root { level: 4, digest: [9; 32] },
        total_bytes: MAX_STORED_BYTES,
        offset: 13,
        bytes: &bytes,
    };
    let mut output = vec![0; MAX_CHUNK_BYTES];
    let length = encode_chunk(&mut output, chunk).unwrap();
    let read = decode_chunk(&output[..length]).unwrap();
    assert_eq!(
        (read.root, read.total_bytes, read.offset, read.bytes),
        (chunk.root, chunk.total_bytes, chunk.offset, chunk.bytes)
    );
    assert!(encode_chunk(&mut output, Chunk { offset: u64::MAX, ..chunk }).is_err());
    assert!(encode_chunk(&mut output, Chunk { total_bytes: 0, ..chunk }).is_err());
    assert!(
        encode_chunk(&mut output, Chunk { total_bytes: MAX_STORED_BYTES + 1, ..chunk }).is_err()
    );
    assert_eq!(encode_chunk(&mut [0; 49], chunk), Err(Error::BufferTooSmall));
    for end in 0..49 {
        assert!(decode_chunk(&output[..end]).is_err());
    }
}

#[test]
fn key_counts_and_cursors_must_fit_the_remaining_frame() {
    let mut output = [0; 64];
    let keys = [Key { bytes: b"a", len_bytes: 1 }, Key { bytes: b"b", len_bytes: 2 }];
    let length = encode_page(&mut output, Some(Root::default()), keys.into_iter(), None).unwrap();
    assert_eq!(length, 62);
    assert_eq!(decode_page(&output[..length]).unwrap().keys.len(), 2);
    // All lengths fit individually; the advertised count cannot fit together.
    output[34..38].copy_from_slice(&3u32.to_le_bytes());
    assert!(matches!(decode_page(&output[..length]), Err(Error::InvalidArgument)));
    output[34..38].copy_from_slice(&2u32.to_le_bytes());
    output[38..40].copy_from_slice(&3u16.to_le_bytes());
    assert!(matches!(decode_page(&output[..length]), Err(Error::InvalidArgument)));
    output[34..38].copy_from_slice(&0u32.to_le_bytes());
    output[38..40].copy_from_slice(&512u16.to_le_bytes());
    assert!(matches!(decode_page(&output[..length]), Err(Error::InvalidArgument)));
}
