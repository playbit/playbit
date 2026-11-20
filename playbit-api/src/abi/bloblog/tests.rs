#![allow(clippy::unwrap_used, reason = "Tests assert exact bounded frame behavior.")]
use super::*;

#[test]
fn pages_round_trip_binary_entries_and_empty_progress() {
    // Borrowed decoding preserves every field and rejects every truncated frame.
    let entries = [
        Entry { log: [1; 15], id: [2; 16], clock: 3, nonce: [4; 16], data: &[0, 255, 128] },
        Entry { log: [1; 15], id: [5; 16], clock: 6, nonce: [7; 16], data: &[] },
    ];
    let mut bytes = vec![0; MAX_PAGE_BYTES];
    let length = encode_page(&mut bytes, entries.into_iter(), "opaque", true).unwrap();
    let page = decode_page(&bytes[..length]).unwrap();
    assert_eq!(page.entries.collect::<Vec<_>>(), entries);
    assert_eq!(page.cursor, "opaque");
    assert!(page.more);
    for end in 0..length {
        assert!(decode_page(&bytes[..end]).is_err());
    }
    assert!(decode_page(&bytes[..length + 1]).is_err());
    let length = encode_page(&mut bytes, [].into_iter(), "retained", false).unwrap();
    let page = decode_page(&bytes[..length]).unwrap();
    assert_eq!(page.entries.len(), 0);
    assert_eq!(page.cursor, "retained");
    assert!(!page.more);
    assert!(encode_page(&mut bytes, [].into_iter(), "retained", true).is_err());
}

#[test]
fn batch_and_page_limits_include_record_overhead() {
    // Exact byte/count boundaries apply before payload allocation or output writes.
    assert!(validate_lengths([0; MAX_BLOBS]).is_ok());
    assert!(validate_lengths([]).is_err());
    assert!(validate_lengths([0; MAX_BLOBS + 1]).is_err());
    assert!(validate_lengths([MAX_BLOB_BYTES + 1]).is_err());
    let tail = MAX_BYTES - 4 - 2 * 59 - MAX_BLOB_BYTES;
    assert!(validate_lengths([MAX_BLOB_BYTES, tail]).is_ok());
    assert!(validate_lengths([MAX_BLOB_BYTES, tail + 1]).is_err());
    let data = vec![1; MAX_BLOB_BYTES];
    let entry = Entry { log: [1; 15], id: [2; 16], clock: 3, nonce: [4; 16], data: &data };
    let entries = [entry, Entry { data: &data[..tail], ..entry }];
    let cursor = "x".repeat(MAX_CURSOR_BYTES);
    let mut bytes = vec![0; MAX_PAGE_BYTES];
    assert_eq!(encode_page(&mut bytes, entries.into_iter(), &cursor, false), Ok(MAX_PAGE_BYTES));
    assert_eq!(decode_page(&bytes).unwrap().entries.len(), 2);
    let mut short = [0xa5; 4];
    assert_eq!(
        encode_page(&mut short, entries.into_iter(), &cursor, false),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(short, [0xa5; 4]);
    assert!(encode_page(&mut bytes, [entry; 2].into_iter(), &cursor, false).is_err());
    assert!(
        encode_page(&mut bytes, [Entry { clock: 0, ..entry }].into_iter(), &cursor, false).is_err()
    );
}
