#![allow(clippy::unwrap_used, reason = "Fixtures assert bounded KV storage outcomes.")]
use super::*;
use kvdb::{CreateOptions, Db, storage::mem::MemStorage};

fn database() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 1)).unwrap()
}

#[test]
fn bounded_pages_continue_across_tombstones_with_only_value_prefixes() {
    let database = database();
    let mut tx = database.begin_write(1000).unwrap();
    for index in 0u16..193 {
        let key = index.to_be_bytes();
        tx.put(&key, if index == 192 { &[0; MAX_BYTES][..] } else { b"" }).unwrap();
        if index < 192 {
            tx.del(&key).unwrap();
        }
    }
    tx.commit().unwrap();
    let tx = database.begin_read().unwrap();
    let mut after = None;
    let mut root = None;
    let mut found = Vec::new();
    let mut pages = 0;
    loop {
        let page = page(&tx, after.as_deref(), root, MAX_KEYS).unwrap();
        pages += 1;
        found.extend(page.keys);
        root = page.root;
        after = page.next;
        if after.is_none() {
            break;
        }
        assert!(pages <= 4);
    }
    assert_eq!(pages, 4);
    assert_eq!(
        found,
        vec![Key {
            bytes: 192u16.to_be_bytes().to_vec(),
            len_bytes: MAX_BYTES as u64,
            preview: vec![0; PREVIEW_BYTES],
        }]
    );
}

#[test]
fn value_prefixes_are_bounded_and_belong_to_the_listed_snapshot() {
    // A maximum stored value contributes only 128 bytes; a later write cannot alter an open snapshot's prefix.
    let database = database();
    let mut large = vec![0xa5; MAX_STORED_BYTES];
    large[..PREVIEW_BYTES].copy_from_slice(&[0x31; PREVIEW_BYTES]);
    let mut tx = database.begin_write(1000).unwrap();
    tx.put(b"a-empty", b"").unwrap();
    tx.put(b"b-binary", &[0, 0xff, 0x80]).unwrap();
    tx.put(b"c-large", &large).unwrap();
    tx.commit().unwrap();
    let snapshot = database.begin_read().unwrap();
    let first = page(&snapshot, None, None, MAX_KEYS).unwrap();
    assert_eq!(
        first.keys,
        vec![
            Key { bytes: b"a-empty".to_vec(), len_bytes: 0, preview: vec![] },
            Key { bytes: b"b-binary".to_vec(), len_bytes: 3, preview: vec![0, 0xff, 0x80] },
            Key {
                bytes: b"c-large".to_vec(),
                len_bytes: MAX_STORED_BYTES as u64,
                preview: vec![0x31; PREVIEW_BYTES]
            },
        ]
    );
    assert!(first.root.is_some());
    assert_eq!(first.next, None);
    let mut tx = database.begin_write(1001).unwrap();
    tx.put(b"c-large", &[0xc7; PREVIEW_BYTES + 1]).unwrap();
    tx.commit().unwrap();
    assert_eq!(page(&snapshot, None, first.root, MAX_KEYS).unwrap(), first);
    assert!(matches!(
        page(&database.begin_read().unwrap(), None, first.root, MAX_KEYS),
        Err(Error::LocalContention(_))
    ));
    let current = page(&database.begin_read().unwrap(), None, None, MAX_KEYS).unwrap();
    assert_ne!(current.root, first.root);
    assert_eq!(current.keys[2].preview, vec![0xc7; PREVIEW_BYTES]);
    assert_eq!(current.keys[2].len_bytes, (PREVIEW_BYTES + 1) as u64);
}

#[test]
fn binary_keys_and_chunk_roots_bind_every_continuation_to_content() {
    let database = database();
    let key = [0xff; kvdb::KEY_SIZE_MAX];
    let mut tx = database.begin_write(1000).unwrap();
    tx.put(&key, &[0xab; MAX_BYTES + 1]).unwrap();
    tx.commit().unwrap();
    let first = read(&database.begin_read().unwrap(), &key, 0, MAX_BYTES as u32, None).unwrap();
    assert_eq!(first.bytes.len(), MAX_BYTES);
    assert_eq!(first.total_bytes, MAX_BYTES as u64 + 1);
    let next =
        read(&database.begin_read().unwrap(), &key, MAX_BYTES as u64, 1, Some(first.root)).unwrap();
    assert_eq!(next.bytes, [0xab]);
    assert!(read(&database.begin_read().unwrap(), &key, 1, 1, None).is_err());
    // Same origin clock with different bytes must change the root too.
    let clock = database.begin_read().unwrap().get_with_clock(&key).unwrap().unwrap().0;
    let mut tx = database.begin_write(1000).unwrap();
    tx.put_with_clock(&key, &[0xcd; MAX_BYTES + 1], clock).unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        read(&database.begin_read().unwrap(), &key, 1, 1, Some(first.root)),
        Err(Error::LocalContention(_))
    ));
    assert!(matches!(
        page(&database.begin_read().unwrap(), Some(&key), Some(first.root), 1),
        Err(Error::LocalContention(_))
    ));
    for invalid in [&[][..], &[0; kvdb::KEY_SIZE_MAX + 1][..]] {
        assert!(validate_key(invalid).is_err());
    }
}

#[test]
fn read_ranges_reject_invalid_inputs_before_allocating() {
    let database = database();
    let mut tx = database.begin_write(1000).unwrap();
    tx.put(b"empty", b"").unwrap();
    tx.commit().unwrap();
    let tx = database.begin_read().unwrap();
    assert!(read(&tx, b"empty", 0, 1, None).unwrap().bytes.is_empty());
    for (offset, limit) in [(u64::MAX, 1), (0, 0), (0, MAX_BYTES as u32 + 1)] {
        assert!(read(&tx, b"empty", offset, limit, None).is_err());
    }
    assert!(page(&tx, Some(b"empty"), None, 1).is_err());
    assert!(page(&tx, None, None, MAX_KEYS + 1).is_err());
}
