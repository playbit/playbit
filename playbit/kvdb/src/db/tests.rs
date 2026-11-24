#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::storage::mem::MemStorage;
use alloc::vec;

const T0: u64 = crate::clock::EPOCH_UNIX_SECS + 1000;

fn plain_db() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap()
}

fn sync_db() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0xd15c)).unwrap()
}

#[test]
fn recovery_source_scan_retains_tombstones_and_its_pinned_clock() {
    let db = sync_db();
    let empty = db.begin_read().unwrap();
    assert_eq!(empty.clock().unwrap(), Clock::ZERO);
    assert_eq!(empty.next_source_key(b"", false).unwrap(), None);
    let mut tx = db.begin_write(T0).unwrap();
    tx.put(b"a", b"removed").unwrap();
    tx.put(b"b", b"retained").unwrap();
    let first = tx.clock();
    tx.commit().unwrap();
    let pinned = db.begin_read().unwrap();
    let mut tx = db.begin_write(T0 + 1).unwrap();
    tx.del(b"a").unwrap();
    tx.local_indexes().put(b"a", b"derived").unwrap();
    {
        let index = tx.local_indexes();
        assert!(index.source_get_with_clock(b"a").unwrap().is_none());
        let (clock, value) = index.source_get_with_clock(b"b").unwrap().unwrap();
        assert_eq!(clock, first);
        assert_eq!(value.as_ref(), b"retained");
    }
    let second = tx.clock();
    tx.commit().unwrap();
    let current = db.begin_read().unwrap();
    assert_eq!(empty.clock().unwrap(), Clock::ZERO);
    assert_eq!(pinned.clock().unwrap(), first);
    assert_eq!(current.clock().unwrap(), second);
    assert_eq!(pinned.get(b"a").unwrap().unwrap().as_ref(), b"removed");
    assert_eq!(current.next_source_key(b"", false).unwrap(), Some(b"a".to_vec()));
    assert_eq!(current.next_source_key(b"a", false).unwrap(), Some(b"a".to_vec()));
    assert_eq!(current.next_source_key(b"a", true).unwrap(), Some(b"b".to_vec()));
    assert_eq!(current.next_source_key(b"b", true).unwrap(), None);
    assert_eq!(current.next_key(b"", false).unwrap(), Some(b"b".to_vec()));
    assert_eq!(current.next_key(b"b", true).unwrap(), None);
    assert_eq!(current.next_source_key(&[0; KEY_SIZE_MAX + 1], false), Err(Error::KeySize));
    assert_eq!(current.next_key(&[0; KEY_SIZE_MAX + 1], false), Err(Error::KeySize));
}

#[test]
fn reopening_preserves_snapshot_clock_and_local_records_in_both_modes() {
    for db in [plain_db(), sync_db()] {
        assert_eq!(db.begin_read().unwrap().next_key(b"", false).unwrap(), None);
        let mut tx = db.begin_write(T0).unwrap();
        tx.put(b"entry", b"source").unwrap();
        tx.local_indexes().put(b"name/notes", b"entry").unwrap();
        tx.commit().unwrap();
        let clock = db.begin_read().unwrap().clock().unwrap();
        let reopened = Db::open(db.storage, &OpenOptions::default()).unwrap();
        let read = reopened.begin_read().unwrap();
        assert_eq!(read.clock().unwrap(), clock);
        assert_eq!(read.get(b"entry").unwrap().unwrap().as_ref(), b"source");
        assert_eq!(read.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"entry");
        assert_eq!(read.next_source_key(b"", false).unwrap(), Some(b"entry".to_vec()));
        assert_eq!(read.next_source_key(b"entry", true).unwrap(), None);
    }
}

#[test]
fn retained_value_readers_reject_corrupt_flags_in_every_read_path() {
    let db = sync_db();
    let mut tx = db.begin_write(T0).unwrap();
    tx.put(b"key", b"body").unwrap();
    tx.commit().unwrap();
    let key = TxnInner::<MemStorage>::skey(b"key");
    let mut raw = db.storage.begin(true).unwrap();
    let mut bytes = raw.get(&key).unwrap().unwrap().to_vec();
    // A valid encoded entry plus an unknown flag must never be treated as a
    // live value by copy/recovery, ordinary reads, or a source-index repair.
    let flag = if bytes.len() >= encoding::ENT_HEADER_FULL { 32 } else { 1 };
    bytes[flag] |= 0x80;
    raw.put(&key, &bytes).unwrap();
    raw.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.value_reader(b"key").err(), Some(Error::Corrupt));
    assert_eq!(read.value_len(b"key"), Err(Error::Corrupt));
    assert_eq!(read.get_with_clock(b"key").err(), Some(Error::Corrupt));
    assert_eq!(
        read.visit::<Error>(b"", false, |_, value| {
            let mut out = vec![0; value.len()];
            value.read_exact_at(0, &mut out)?;
            panic!("corrupt entry reached visitor")
        }),
        Err(Error::Corrupt)
    );
}

#[test]
fn local_indexes_commit_abort_and_share_the_source_snapshot() {
    // A source record and its derived name become visible together, while an
    // old reader retains both old values and an abandoned writer exposes neither.
    for db in [plain_db(), sync_db()] {
        let mut tx = db.begin_write(T0).unwrap();
        tx.put(b"entry", b"notes").unwrap();
        let mut index = tx.local_indexes();
        assert_eq!(index.source_get(b"entry").unwrap().unwrap().as_ref(), b"notes");
        index.put(b"name/notes", b"entry").unwrap();
        assert_eq!(index.get(b"name/notes").unwrap().unwrap().as_ref(), b"entry");
        tx.commit().unwrap();
        let old = db.begin_read().unwrap();
        for commit in [false, true] {
            let mut tx = db.begin_write(T0 + 1).unwrap();
            tx.put(b"entry", b"work").unwrap();
            let mut index = tx.local_indexes();
            assert!(index.del(b"name/notes").unwrap());
            assert!(!index.del(b"name/missing").unwrap());
            index.put(b"name/work", b"entry").unwrap();
            assert_eq!(index.source_get(b"entry").unwrap().unwrap().as_ref(), b"work");
            if commit {
                tx.commit().unwrap();
            } else {
                drop(tx);
            }
            let current = db.begin_read().unwrap();
            assert_eq!(
                current.get(b"entry").unwrap().unwrap().as_ref(),
                if commit { b"work".as_slice() } else { b"notes".as_slice() }
            );
            assert_eq!(current.local_get(b"name/work").unwrap().is_some(), commit);
            assert_eq!(current.local_get(b"name/notes").unwrap().is_some(), !commit);
        }
        assert_eq!(old.get(b"entry").unwrap().unwrap().as_ref(), b"notes");
        assert_eq!(old.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"entry");
        assert!(old.local_get(b"name/work").unwrap().is_none());
    }
}

#[test]
fn local_indexes_are_disjoint_and_never_enter_roots_or_deltas() {
    // The same user key can exist in both spaces. Secondary-only commits must
    // neither alter the replicated tree nor publish secondary values/tombstones.
    let db = sync_db();
    let mut tx = db.begin_write(T0).unwrap();
    tx.put(b"same", b"source").unwrap();
    tx.commit().unwrap();
    let root = db.begin_read().unwrap().root_hash().unwrap();
    for remove in [false, true] {
        let mut tx = db.begin_write_with_delta(T0 + 1, 4096).unwrap();
        let mut index = tx.local_indexes();
        if remove {
            assert!(index.del(b"same").unwrap());
        } else {
            index.put(b"same", b"derived").unwrap();
            assert_eq!(index.get(b"same").unwrap().unwrap().as_ref(), b"derived");
        }
        assert_eq!(index.source_get(b"same").unwrap().unwrap().as_ref(), b"source");
        assert_eq!(tx.root_hash().unwrap(), root);
        tx.verify().unwrap();
        let crate::CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
        crate::sync::visit_delta(&bytes, 4096, |_, _| panic!("local data leaked")).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(read.get(b"same").unwrap().unwrap().as_ref(), b"source");
        assert_eq!(read.local_get(b"same").unwrap().is_some(), !remove);
        assert_eq!(read.iter().count(), 1);
        assert_eq!(read.root_hash().unwrap(), root);
        db.verify().unwrap();
    }
}

#[test]
fn local_index_seeks_are_ordered_bounded_and_validate_keys_before_mutation() {
    // Prefix seeks visit only local records, and invalid keys cannot enter
    // either namespace or consume a pending source publication.
    let db = sync_db();
    let mut tx = db.begin_write(T0).unwrap();
    tx.put(b"source", b"value").unwrap();
    let mut index = tx.local_indexes();
    for key in [b"name/z", b"name/a", b"name/m"] {
        index.put(key, key).unwrap();
    }
    assert_eq!(index.put(b"", b"invalid"), Err(Error::InvalidKey));
    assert_eq!(index.del(b""), Err(Error::InvalidKey));
    assert_eq!(index.get(b"").err(), Some(Error::InvalidKey));
    let oversized = vec![1; crate::KEY_SIZE_MAX + 1];
    assert_eq!(index.put(&oversized, b"invalid"), Err(Error::KeySize));
    assert_eq!(index.iter_from(&oversized).err(), Some(Error::KeySize));
    let maximum = vec![0xff; crate::KEY_SIZE_MAX];
    index.put(&maximum, b"maximum").unwrap();
    let keys: Vec<_> = index.iter_from(b"name/m").unwrap().map(|v| v.unwrap().0.to_vec()).collect();
    assert_eq!(keys, vec![b"name/m".to_vec(), b"name/z".to_vec(), maximum]);
    assert_eq!(index.source_iter_from(b"").count(), 1);
    assert_eq!(index.next_source_key(b"", false).unwrap(), Some(b"source".to_vec()));
    assert_eq!(index.next_source_key(b"source", true).unwrap(), None);
    assert_eq!(index.next_local_key(b"name/m", true).unwrap(), Some(b"name/z".to_vec()));
    assert_eq!(index.next_local_key(&oversized, false), Err(Error::KeySize));
    assert_eq!(tx.local_get(b"name/a").unwrap().as_deref(), Some(&b"name/a"[..]));
    assert_eq!(tx.next_local_key(b"", false).unwrap(), Some(b"name/a".to_vec()));
    assert_eq!(tx.next_local_key(b"name/m", true).unwrap(), Some(b"name/z".to_vec()));
    assert_eq!(tx.next_local_key(&oversized, false), Err(Error::KeySize));
    tx.commit().unwrap();
    let snapshot = db.begin_read().unwrap();
    assert_eq!(snapshot.local_iter_from(b"").unwrap().count(), 4);
    assert_eq!(snapshot.next_local_key(b"name/m", false).unwrap(), Some(b"name/m".to_vec()));
    assert_eq!(snapshot.next_local_key(b"name/m", true).unwrap(), Some(b"name/z".to_vec()));
    assert_eq!(snapshot.next_local_key(&vec![0xff; crate::KEY_SIZE_MAX], true).unwrap(), None);
    assert_eq!(snapshot.next_local_key(&oversized, false), Err(Error::KeySize));
    drop(snapshot);
    db.verify().unwrap();
}

#[test]
fn local_index_disk_corruption_is_reported_without_repairing_it() {
    // Verification allows exactly one local namespace after metadata; malformed
    // local keys and any unrelated trailing namespace remain corruption.
    for key in [vec![encoding::LOCAL_KEY_BYTE], vec![0x82, 1], vec![0x81; 513]] {
        let db = sync_db();
        let mut tx = db.storage.begin(true).unwrap();
        tx.put(&key, b"bad").unwrap();
        tx.commit().unwrap();
        assert_eq!(db.verify(), Err(Error::Corrupt));
    }
    let db = sync_db();
    let mut tx = db.storage.begin(true).unwrap();
    tx.put(&[encoding::LOCAL_KEY_BYTE], b"bad").unwrap();
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.local_iter_from(b"").unwrap().next().unwrap().err(), Some(Error::Corrupt));
}

fn collect(db: &Db<MemStorage>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let r = db.begin_read().unwrap();

    r.iter()
        .map(|kv| {
            let (k, v) = kv.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect()
}

#[test]
fn unchanged_deferred_write_preserves_tree_and_does_not_consume_capture_budget() {
    let db = sync_db();
    let value = vec![0x42; 8192];
    let mut initial = db.begin_write(T0).unwrap();
    let origin = initial.clock();
    initial.put(b"retained", &value).unwrap();
    initial.commit().unwrap();
    let root = db.begin_read().unwrap().root_hash().unwrap();
    // The entry is larger than this capture budget. Capturing an unchanged
    // write would incorrectly convert the empty publication to PullRequired.
    let mut txn = db.begin_write_with_delta(T0 + 1, 128).unwrap();
    assert_eq!(
        txn.sync_put_from_deferred(b"retained", &crate::storage::SliceValue(&value), 0, origin)
            .unwrap(),
        None,
    );
    assert_eq!(txn.root_hash().unwrap(), root);
    let crate::sync::CommittedDelta::Bytes(bytes) = txn.commit_with_delta().unwrap() else {
        panic!("an unchanged entry must not consume capture budget")
    };
    let mut captured = 0;
    crate::sync::visit_delta(&bytes, 128, |_, _| captured += 1).unwrap();
    assert_eq!(captured, 0);
    let read = db.begin_read().unwrap();
    assert_eq!(read.get(b"retained").unwrap().unwrap().as_ref(), value.as_slice());
    assert_eq!(read.root_hash().unwrap(), root);
}

#[test]
fn replay_delta_uses_the_pinned_committed_entry_without_advancing_clocks() {
    let db = sync_db();
    let mut tx = db.begin_write(T0).unwrap();
    tx.put(b"k", b"first").unwrap();
    tx.commit().unwrap();
    let retained = db.begin_read().unwrap();
    let original = retained.get_with_clock(b"k").unwrap().unwrap().0;
    let mut tx = db.begin_write(T0 + 1).unwrap();
    tx.put(b"k", b"second").unwrap();
    tx.commit().unwrap();
    let current_clock = db.clock();
    let current_root = db.begin_read().unwrap().root_hash().unwrap();
    for _ in 0..2 {
        let crate::sync::CommittedDelta::Bytes(bytes) =
            retained.delta_for_key(b"k", 4096).unwrap().unwrap()
        else {
            panic!()
        };
        let peer = sync_db();
        let mut receive = crate::DeltaReceiver::new(&peer, crate::Lww, T0, 4096).unwrap();
        receive.process(&bytes).unwrap();
        receive.finish().unwrap();
        let read = peer.begin_read().unwrap();
        let (clock, value) = read.get_with_clock(b"k").unwrap().unwrap();
        assert_eq!(clock, original);
        assert_eq!(&*value, b"first");
    }
    assert_eq!(db.clock(), current_clock);
    assert_eq!(db.begin_read().unwrap().root_hash().unwrap(), current_root);
    assert!(matches!(
        retained.delta_for_key(b"k", 1).unwrap(),
        Some(crate::sync::CommittedDelta::PullRequired)
    ));
    assert!(retained.delta_for_key(b"missing", 4096).unwrap().is_none());
    assert_eq!(plain_db().begin_read().unwrap().delta_for_key(b"k", 4096).err(), Some(Error::Mode));
    let mut tx = db.begin_write(T0 + 2).unwrap();
    tx.del(b"k").unwrap();
    tx.commit().unwrap();
    let crate::sync::CommittedDelta::Bytes(bytes) =
        db.begin_read().unwrap().delta_for_key(b"k", 4096).unwrap().unwrap()
    else {
        panic!()
    };
    let mut count = 0;
    crate::sync::visit_delta(&bytes, 4096, |key, value| {
        assert_eq!(key, b"k");
        assert!(matches!(value, crate::sync::EntState::Deleted { .. }));
        count += 1;
    })
    .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn clock_and_value_share_the_snapshot_and_pending_transaction() {
    let plain = plain_db();
    assert_eq!(plain.begin_read().unwrap().get_with_clock(b"k").err(), Some(Error::Mode));
    let db = sync_db();
    let origin = Clock::now(T0);
    let mut tx = db.begin_write(T0).unwrap();
    tx.put_with_clock(b"k", b"first", origin).unwrap();
    let (clock, value) = tx.get_with_clock(b"k").unwrap().unwrap();
    assert_eq!(clock, origin);
    assert_eq!(&*value, b"first");
    tx.commit().unwrap();
    let retained = db.begin_read().unwrap();
    let mut tx = db.begin_write(T0 + 1).unwrap();
    let next = tx.clock();
    tx.put(b"k", b"second").unwrap();
    tx.commit().unwrap();
    let (clock, value) = retained.get_with_clock(b"k").unwrap().unwrap();
    assert_eq!(clock, origin);
    assert_eq!(&*value, b"first");
    let current = db.begin_read().unwrap();
    let (clock, value) = current.get_with_clock(b"k").unwrap().unwrap();
    assert_eq!(clock, next);
    assert_eq!(&*value, b"second");
    drop(current);
    let mut tx = db.begin_write(T0 + 2).unwrap();
    tx.del(b"k").unwrap();
    assert!(tx.get_with_clock(b"k").unwrap().is_none());
    drop(tx);
    assert_eq!(&*db.begin_read().unwrap().get_with_clock(b"k").unwrap().unwrap().1, b"second");
    let mut tx = db.begin_write(T0 + 3).unwrap();
    tx.del(b"k").unwrap();
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.get_with_clock(b"k").unwrap().is_none());
    assert!(read.get_with_clock(b"missing").unwrap().is_none());
    assert_eq!(read.get_with_clock(b"").err(), Some(Error::InvalidKey));
}

#[test]
fn put_with_clock_checks_mode_arguments_and_exhaustion_before_changes() {
    let plain = plain_db();
    let mut tx = plain.begin_write(T0).unwrap();
    assert_eq!(tx.put_with_clock(b"k", b"v", Clock::now(T0)), Err(Error::Mode));
    assert!(!tx.has(b"k").unwrap());
    drop(tx);
    let db = sync_db();
    let mut tx = db.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"retained", b"before").unwrap();
    let before = tx.root_hash().unwrap();
    let clock = tx.clock();
    for origin in [
        Clock::ZERO,
        Clock::from_u64(u64::MAX),
        Clock::from_u64(u64::MAX - 1),
        Clock::from_u64(u64::MAX - 2),
    ] {
        let expected =
            if origin.is_zero() { Error::InvalidArgument } else { Error::ClockExhausted };
        assert_eq!(tx.put_with_clock(b"retained", b"after", origin), Err(expected));
        assert_eq!(tx.root_hash().unwrap(), before);
        assert_eq!(tx.clock(), clock);
    }
    assert_eq!(tx.put_with_clock(b"", b"v", clock), Err(Error::InvalidKey));
    assert_eq!(
        tx.put_with_clock(b"k", &vec![0; crate::VALUE_SIZE_MAX + 1], clock),
        Err(Error::ValueSize)
    );
    assert_eq!(tx.root_hash().unwrap(), before);
    assert_eq!(tx.clock(), clock);
    let crate::sync::CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else {
        panic!()
    };
    let peer = sync_db();
    let mut receiver = crate::sync::DeltaReceiver::new(&peer, crate::sync::Lww, T0, 4096).unwrap();
    receiver.process(&delta).unwrap();
    receiver.finish().unwrap();
    assert_eq!(collect(&peer), vec![(b"retained".to_vec(), b"before".to_vec())]);
    assert_eq!(peer.begin_read().unwrap().root_hash().unwrap(), before);
    let mut next = db.begin_write(T0).unwrap();
    next.put(b"next", b"works").unwrap();
    next.commit().unwrap();
}

#[test]
fn put_with_clock_last_admissible_origin_allows_next_local_commit() {
    let db = sync_db();
    let origin = Clock::from_u64(u64::MAX - 3);
    let mut tx = db.begin_write(T0).unwrap();
    tx.put_with_clock(b"imported", b"value", origin).unwrap();
    assert_eq!(tx.clock(), Clock::from_u64(u64::MAX - 2));
    tx.commit().unwrap();
    let mut next = db.begin_write(T0).unwrap();
    assert_eq!(next.clock(), Clock::from_u64(u64::MAX - 1));
    next.put(b"local", b"value").unwrap();
    next.commit().unwrap();
    assert_eq!(collect(&db).len(), 2);
    // The guarantee is one subsequent writer, not unlimited clock range.
    assert!(matches!(db.begin_write(T0), Err(Error::ClockExhausted)));
}

#[test]
fn put_with_clock_overwrites_and_keeps_next_local_write_monotonic() {
    let db = sync_db();
    let origin = Clock::now(T0 + 100);
    let mut tx = db.begin_write(T0).unwrap();
    tx.put_with_clock(b"k", b"new", origin).unwrap();
    assert!(tx.clock() > origin);
    let first = tx.root_hash().unwrap();
    tx.put_with_clock(b"k", b"new", origin).unwrap();
    assert_eq!(tx.root_hash().unwrap(), first);
    let older = Clock::now(T0 + 50);
    tx.put_with_clock(b"k", b"resolved", older).unwrap();
    assert_eq!(tx.get(b"k").unwrap().as_deref(), Some(&b"resolved"[..]));
    let committed_clock = tx.clock();
    tx.commit().unwrap();
    let mut next = db.begin_write(T0).unwrap();
    assert!(next.clock() > committed_clock);
    next.put(b"k", b"local").unwrap();
    next.commit().unwrap();
}

#[test]
fn crud_both_modes() {
    for db in [plain_db(), sync_db()] {
        let mut w = db.begin_write(T0).unwrap();
        w.put(b"a", b"1").unwrap();
        w.put(b"b", b"2").unwrap();
        w.put(b"c", &[9u8; 100]).unwrap(); // full-layout in syncable mode
        assert_eq!(w.get(b"a").unwrap().as_deref(), Some(&b"1"[..])); // read-your-writes
        w.commit().unwrap();

        let r = db.begin_read().unwrap();
        assert_eq!(r.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(r.get(b"c").unwrap().as_deref(), Some(&[9u8; 100][..]));
        assert_eq!(r.get(b"nope").unwrap().as_deref(), None);
        assert!(r.has(b"b").unwrap());
        drop(r);

        let mut w = db.begin_write(T0 + 1).unwrap();
        assert!(w.del(b"b").unwrap());
        assert!(!w.del(b"b").unwrap()); // second delete is a no-op
        assert!(!w.del(b"nope").unwrap());
        w.commit().unwrap();

        assert_eq!(
            collect(&db),
            vec![(b"a".to_vec(), b"1".to_vec()), (b"c".to_vec(), vec![9u8; 100])]
        );
    }
}

#[test]
fn tombstone_semantics() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"k", b"v").unwrap();
    w.commit().unwrap();

    let mut w = db.begin_write(T0 + 1).unwrap();
    assert!(w.del(b"k").unwrap());
    let del_clock = w.clock();
    w.commit().unwrap();

    let r = db.begin_read().unwrap();
    assert_eq!(r.get(b"k").unwrap().as_deref(), None);
    assert_eq!(r.is_del(b"k").unwrap(), Some(del_clock));
    assert_eq!(r.is_del(b"never").unwrap(), None);
    assert_eq!(r.iter().count(), 0);
    drop(r);

    // del_forget erases the tombstone: key looks like it never existed
    let mut w = db.begin_write(T0 + 2).unwrap();
    assert!(w.del_forget(b"k").unwrap());
    w.commit().unwrap();
    let r = db.begin_read().unwrap();
    assert_eq!(r.is_del(b"k").unwrap(), None);
    assert_eq!(r.root_hash().unwrap(), None);
}

#[test]
fn plain_mode_rejects_sync_ops() {
    let db = plain_db();
    let r = db.begin_read().unwrap();
    assert!(matches!(r.is_del(b"k"), Err(Error::Mode)));
    assert!(matches!(r.root_hash(), Err(Error::Mode)));
}

#[test]
fn iteration_order_and_ranges() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    for k in [&b"d"[..], b"a", b"c", b"b", b"e"] {
        w.put(k, k).unwrap();
    }
    w.del(b"c").unwrap();
    w.commit().unwrap();
    let r = db.begin_read().unwrap();
    let keys: Vec<Vec<u8>> = r.iter().map(|kv| kv.unwrap().0.to_vec()).collect();
    assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec(), b"d".to_vec(), b"e".to_vec()]);
    let keys: Vec<Vec<u8>> = r.iter_from(b"b").map(|kv| kv.unwrap().0.to_vec()).collect();
    assert_eq!(keys, vec![b"b".to_vec(), b"d".to_vec(), b"e".to_vec()]);
}

#[test]
fn pending_visit_reads_writes_without_changing_snapshot_or_committing() {
    for db in [plain_db(), sync_db()] {
        let mut initial = db.begin_write(T0).unwrap();
        initial.put(b"a", b"old").unwrap();
        initial.put(b"b", b"remove").unwrap();
        initial.commit().unwrap();
        let before = db.clock();
        let snapshot = db.begin_read().unwrap();
        let mut pending = db.begin_write(T0 + 1).unwrap();
        pending.put(b"a", b"new").unwrap();
        pending.del(b"b").unwrap();
        pending.put(b"c", &[7; 8192]).unwrap();
        assert_eq!(
            pending.next_source_key(b"b", false).unwrap(),
            Some(if db.meta().syncable { b"b".to_vec() } else { b"c".to_vec() })
        );
        let mut callbacks = 0;
        assert!(
            pending
                .visit_value(b"c", |clock, value| -> Result<()> {
                    assert_eq!(clock, db.meta().syncable.then_some(pending.clock()));
                    assert_eq!(value.len(), 8192);
                    let mut tail = [0; 3];
                    value.read_exact_at(8189, &mut tail)?;
                    assert_eq!(tail, [7; 3]);
                    callbacks += 1;
                    Ok(())
                })
                .unwrap()
        );
        for key in [b"b".as_slice(), b"absent"] {
            assert!(
                !pending
                    .visit_value(key, |_, _| -> Result<()> {
                        panic!("missing keys and tombstones have no live value")
                    })
                    .unwrap()
            );
        }
        assert_eq!(callbacks, 1);
        // Short values use a compact header; callbacks must see only user bytes.
        assert!(
            pending
                .visit_value(b"a", |clock, value| -> Result<()> {
                    assert_eq!(clock, db.meta().syncable.then_some(pending.clock()));
                    assert_eq!(value.len(), 3);
                    let mut bytes = [0; 3];
                    value.read_exact_at(0, &mut bytes)?;
                    assert_eq!(&bytes, b"new");
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(
            pending.visit_value(b"c", |_, _| Err::<(), _>(Error::InvalidArgument)),
            Err(Error::InvalidArgument)
        );
        let mut seen = Vec::new();
        pending
            .visit(b"", false, |key, value| -> Result<bool> {
                let mut prefix = [0; 3];
                value.read_exact_at(0, &mut prefix)?;
                seen.push((key.to_vec(), value.len(), prefix));
                Ok(true)
            })
            .unwrap();
        assert_eq!(seen, vec![(b"a".to_vec(), 3, *b"new"), (b"c".to_vec(), 8192, [7; 3])]);
        let mut count = 0;
        pending
            .visit(b"a", true, |key, _| -> Result<bool> {
                assert_eq!(key, b"c");
                count += 1;
                Ok(false)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            pending.visit(b"", false, |_, _| Err::<bool, _>(Error::InvalidArgument)),
            Err(Error::InvalidArgument)
        );
        assert_eq!(snapshot.get(b"a").unwrap().unwrap().as_ref(), b"old");
        drop(pending);
        assert_eq!(db.clock(), before);
        assert_eq!(db.begin_read().unwrap().get(b"a").unwrap().unwrap().as_ref(), b"old");
        assert!(!db.begin_read().unwrap().has(b"c").unwrap());
    }
}

#[test]
fn live_visit_matches_iteration_bounds_and_pinned_snapshot() {
    fn collect<S: Storage>(
        tx: &ReadTxn<'_, S>,
        start: &[u8],
        exclusive: bool,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut entries = Vec::new();
        tx.visit(start, exclusive, |key, value| -> Result<bool> {
            let mut bytes = vec![0; value.len()];
            value.read_exact_at(0, &mut bytes)?;
            assert_eq!(value.read_at(value.len(), &mut [0; 1])?, 0);
            assert_eq!(value.read_at(value.len() + 1, &mut []), Err(Error::InvalidArgument));
            entries.push((key.to_vec(), bytes));
            Ok(true)
        })
        .unwrap();
        entries
    }
    for db in [plain_db(), sync_db()] {
        assert!(collect(&db.begin_read().unwrap(), b"", false).is_empty());
        let mut w = db.begin_write(T0).unwrap();
        for (key, len) in [(b"a", 0), (b"b", 30), (b"c", 31), (b"d", 99)] {
            w.put(key, &vec![key[0]; len]).unwrap();
        }
        w.del(b"b").unwrap();
        w.commit().unwrap();
        let old = db.begin_read().unwrap();
        let expected: Vec<_> = old
            .iter()
            .map(|entry| {
                let (key, value) = entry.unwrap();
                (key.to_vec(), value.to_vec())
            })
            .collect();
        assert_eq!(collect(&old, b"", false), expected);
        assert_eq!(collect(&old, b"c", false), expected[1..]);
        assert_eq!(collect(&old, b"c", true), expected[2..]);
        assert!(collect(&old, b"z", false).is_empty());
        let mut w = db.begin_write(T0 + 1).unwrap();
        w.put(b"b", b"reinsert").unwrap();
        w.put(b"c", b"changed").unwrap();
        w.commit().unwrap();
        assert_eq!(collect(&old, b"", false), expected);
        assert_ne!(collect(&db.begin_read().unwrap(), b"", false), expected);
        assert_eq!(
            old.visit(&[0; KEY_SIZE_MAX + 1], false, |_, _| Ok::<_, Error>(true)),
            Err(Error::KeySize)
        );
    }
}

#[test]
fn live_visit_stops_before_later_corruption_and_preserves_callback_error() {
    #[derive(Debug, PartialEq)]
    enum CallerError {
        Kvdb(Error),
        Denied,
    }
    impl From<Error> for CallerError {
        fn from(error: Error) -> Self {
            Self::Kvdb(error)
        }
    }
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"good").unwrap();
    w.commit().unwrap();
    let storage = db.close();
    let mut raw = storage.begin(true).unwrap();
    raw.put(b"\0b", &[1, 2]).unwrap();
    raw.commit().unwrap();
    let db = Db::open(storage, &Default::default()).unwrap();
    let tx = db.begin_read().unwrap();
    let mut calls = 0;
    tx.visit(b"", false, |_, _| {
        calls += 1;
        Ok::<_, CallerError>(false)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(tx.visit(b"", false, |_, _| Err(CallerError::Denied)), Err(CallerError::Denied));
    assert_eq!(
        tx.visit(b"", false, |_, _| Ok::<_, CallerError>(true)),
        Err(CallerError::Kvdb(Error::Corrupt))
    );
}

#[test]
fn live_visit_retains_bounded_reader_corruption_rules() {
    for (key, value, expected) in [
        (b"\0bad".to_vec(), vec![0; 9], Error::Corrupt),
        (
            b"\0bad".to_vec(),
            {
                let mut v = vec![0; 10];
                v[1] = 2;
                v
            },
            Error::Corrupt,
        ),
        (
            b"\0bad".to_vec(),
            {
                let mut v = vec![0; 41];
                v[32] = 2;
                v
            },
            Error::Corrupt,
        ),
        (vec![0], vec![0; 10], Error::InvalidKey),
        (vec![0; KEY_SIZE_MAX + 2], vec![0; 10], Error::KeySize),
    ] {
        let storage = sync_db().close();
        let mut raw = storage.begin(true).unwrap();
        raw.put(&key, &value).unwrap();
        raw.commit().unwrap();
        let db = Db::open(storage, &Default::default()).unwrap();
        let tx = db.begin_read().unwrap();
        assert_eq!(tx.next_key(b"", false), Err(expected));
        assert_eq!(tx.visit(b"", false, |_, _| Ok::<_, Error>(true)), Err(expected));
    }
}

// This backend permits only a retained visit, with a synthetic 16 MiB
// value that rejects reads larger than 64 bytes. Any fallback seek, length
// lookup or snapshot pin panics, so the test proves the traversal boundary.
#[derive(Default)]
struct VisitOnlyStorage {
    visits: core::cell::Cell<usize>,
    bytes: core::cell::Cell<usize>,
    fault: core::cell::Cell<u8>,
}
impl Storage for VisitOnlyStorage {
    type Txn<'db> = &'db Self;
    fn begin(&self, write: bool) -> Result<&Self> {
        assert!(!write);
        Ok(self)
    }
    fn props(&self) -> StorageProps {
        MemStorage::new().props()
    }
}
impl StorageTxn for &VisitOnlyStorage {
    type ValueReader = crate::storage::SliceValue<'static>;
    type Guard<'g>
        = &'g [u8]
    where
        Self: 'g;
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        use crate::storage::ValueRead;
        struct Large<'a>(&'a VisitOnlyStorage);
        impl ValueRead for Large<'_> {
            fn len(&self) -> usize {
                encoding::ENT_HEADER_FULL + 16 * 1024 * 1024
            }
            fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
                assert!(out.len() <= 64, "unbounded value read");
                match self.0.fault.get() {
                    1 => return Ok(0),
                    2 => return Err(Error::Storage(91)),
                    _ => {}
                }
                if offset > self.len() {
                    return Err(Error::InvalidArgument);
                }
                let n = out.len().min(self.len() - offset);
                for (i, byte) in out[..n].iter_mut().enumerate() {
                    *byte = if offset + i < encoding::ENT_HEADER_FULL { 0 } else { 0x5a };
                }
                self.0.bytes.set(self.0.bytes.get() + n);
                Ok(n)
            }
        }
        self.visits.set(self.visits.get() + 1);
        assert_eq!(key, b"\0");
        assert!(!exclusive && !reverse);
        for key in [b"\0large-a", b"\0large-b", b"\0large-c"] {
            if !visitor(key, &Large(self))? {
                break;
            }
        }
        Ok(())
    }
    fn snapshot_value(&self, _: &[u8]) -> Result<Option<Self::ValueReader>> {
        panic!("snapshot pin")
    }
    fn get<'g>(&'g self, _: &[u8]) -> Result<Option<Self::Guard<'g>>> {
        panic!("lookup")
    }
    fn value_len(&self, _: &[u8]) -> Result<Option<usize>> {
        panic!("length lookup")
    }
    fn read_value_at(&self, _: &[u8], _: usize, _: &mut [u8]) -> Result<Option<usize>> {
        panic!("range lookup")
    }
    fn next_key(&self, _: &[u8], _: bool) -> Result<Option<Vec<u8>>> {
        panic!("key seek")
    }
    fn prev_key(&self, _: &[u8], _: bool) -> Result<Option<Vec<u8>>> {
        panic!("key seek")
    }
    fn next_from<'g>(
        &'g self,
        _: &[u8],
        _: bool,
    ) -> Result<Option<(Self::Guard<'g>, Self::Guard<'g>)>> {
        panic!("entry seek")
    }
    fn prev_from<'g>(
        &'g self,
        _: &[u8],
        _: bool,
    ) -> Result<Option<(Self::Guard<'g>, Self::Guard<'g>)>> {
        panic!("entry seek")
    }
    fn put_from(&mut self, _: &[u8], _: &dyn crate::storage::ValueRead) -> Result<()> {
        panic!("write")
    }
    fn put(&mut self, _: &[u8], _: &[u8]) -> Result<()> {
        panic!("write")
    }
    fn del(&mut self, _: &[u8]) -> Result<bool> {
        panic!("write")
    }
    fn commit(self) -> Result<()> {
        panic!("commit")
    }
}

#[test]
fn live_visit_uses_one_range_and_only_requested_value_bytes() {
    let db = Db {
        storage: VisitOnlyStorage::default(),
        meta: *sync_db().meta(),
        clock: CommittedClock::new(Clock::ZERO),
    };
    let tx = db.begin_read().unwrap();
    let mut calls = 0;
    tx.visit(b"", false, |key, value| -> Result<bool> {
        assert_eq!(key, [b"large-a", b"large-b", b"large-c"][calls]);
        calls += 1;
        assert_eq!(value.len(), 16 * 1024 * 1024);
        let mut bytes = [0; 64];
        value.read_exact_at(value.len() - bytes.len(), &mut bytes)?;
        assert_eq!(bytes, [0x5a; 64]);
        Ok(true)
    })
    .unwrap();
    assert_eq!(calls, 3);
    assert_eq!(db.storage.visits.get(), 1);
    assert_eq!(db.storage.bytes.get(), 3 * (41 + 64));
    for (fault, expected) in [(1, Error::Corrupt), (2, Error::Storage(91))] {
        db.storage.fault.set(fault);
        assert_eq!(tx.visit(b"", false, |_, _| Ok::<_, Error>(true)), Err(expected));
    }
}

#[test]
fn snapshot_isolation_and_single_writer() {
    let db = plain_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"x", b"1").unwrap();
    let r = db.begin_read().unwrap();
    assert!(!r.has(b"x").unwrap());
    assert!(matches!(db.begin_write(T0), Err(Error::Busy)));
    w.commit().unwrap();
    assert!(!r.has(b"x").unwrap()); // snapshot unchanged
    assert!(db.begin_read().unwrap().has(b"x").unwrap());
}

#[test]
fn clock_advances_and_survives_reopen() {
    let db = sync_db();
    assert!(db.clock().is_zero());
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"1").unwrap();
    let c1 = w.clock();
    w.commit().unwrap();
    assert_eq!(db.clock(), c1);

    // same wall second: count disambiguates
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"b", b"2").unwrap();
    let c2 = w.clock();
    w.commit().unwrap();
    assert!(c2 > c1);

    // aborted transaction leaves the clock alone
    let w = db.begin_write(T0).unwrap();
    let c_aborted = w.clock();
    assert!(c_aborted > c2);
    drop(w);
    assert_eq!(db.clock(), c2);

    // reopen: committed clock is durable
    let storage = db.close();
    let db = Db::open(storage, &Default::default()).unwrap();
    assert_eq!(db.clock(), c2);
    assert!(db.meta().syncable);
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"c", b"3").unwrap();
    assert!(w.clock() > c2);
    w.commit().unwrap();
}

#[test]
fn readers_never_observe_an_uncommitted_clock() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"1").unwrap();
    let c1 = w.clock();
    w.commit().unwrap();
    assert_eq!(db.clock(), c1);

    // While a write transaction is open, its clock is not the committed one.
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"b", b"2").unwrap();
    let c2 = w.clock();
    assert!(c2 > c1);
    assert_eq!(w.committed_clock(), c1);
    assert_eq!(db.clock(), c1, "an open transaction must not publish its clock");
    assert!(db.begin_read().unwrap().get(b"b").unwrap().is_none());
    w.commit().unwrap();
    assert_eq!(db.clock(), c2);
}

/// Storage whose next commit can be made to fail after the writer has
/// done all of its work, as a full disk or a torn write would.
struct FailingStorage {
    inner: MemStorage,
    fail_next_commit: core::cell::Cell<bool>,
}

#[test]
fn failed_source_and_index_commit_exposes_neither_change_nor_publishable_delta() {
    // An actual backend commit failure must not publish the source delta,
    // update its clock, or leak a partially updated persistent name mapping.
    let storage =
        FailingStorage { inner: MemStorage::new(), fail_next_commit: core::cell::Cell::new(false) };
    let db = Db::create(storage, &CreateOptions::syncable([7; 32], 1)).unwrap();
    let mut tx = db.begin_write(T0).unwrap();
    tx.put(b"entry", b"notes").unwrap();
    tx.local_indexes().put(b"name/notes", b"entry").unwrap();
    tx.commit().unwrap();
    let before = db.clock();
    let root = db.begin_read().unwrap().root_hash().unwrap();
    db.storage.fail_next_commit.set(true);
    let mut tx = db.begin_write_with_delta(T0 + 1, 4096).unwrap();
    tx.put(b"entry", b"work").unwrap();
    tx.local_indexes().del(b"name/notes").unwrap();
    tx.local_indexes().put(b"name/work", b"entry").unwrap();
    assert_eq!(tx.commit_with_delta().err(), Some(Error::Storage(28)));
    assert_eq!(db.clock(), before);
    let read = db.begin_read().unwrap();
    assert_eq!(read.root_hash().unwrap(), root);
    assert_eq!(read.get(b"entry").unwrap().unwrap().as_ref(), b"notes");
    assert_eq!(read.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"entry");
    assert!(read.local_get(b"name/work").unwrap().is_none());
}
struct FailingTxn<'db> {
    inner: crate::storage::mem::MemTxn<'db>,
    fail: &'db core::cell::Cell<bool>,
}
impl Storage for FailingStorage {
    type Txn<'db> = FailingTxn<'db>;
    fn begin(&self, write: bool) -> Result<FailingTxn<'_>> {
        Ok(FailingTxn { inner: self.inner.begin(write)?, fail: &self.fail_next_commit })
    }
    fn props(&self) -> StorageProps {
        self.inner.props()
    }
}
impl StorageTxn for FailingTxn<'_> {
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        self.inner.visit(key, exclusive, reverse, visitor)
    }
    type ValueReader = crate::storage::mem::MemValueReader;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<Self::ValueReader>> {
        let _ = key;
        self.inner.snapshot_value(key)
    }
    type Guard<'g>
        = &'g [u8]
    where
        Self: 'g;
    fn get<'g>(&'g self, key: &[u8]) -> Result<Option<&'g [u8]>> {
        self.inner.get(key)
    }
    #[allow(unused_variables)]
    fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
        self.inner.value_len(key)
    }
    #[allow(unused_variables)]
    fn read_value_at(&self, key: &[u8], offset: usize, out: &mut [u8]) -> Result<Option<usize>> {
        self.inner.read_value_at(key, offset, out)
    }
    #[allow(unused_variables)]
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_key(key, exclusive)
    }
    #[allow(unused_variables)]
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.prev_key(key, exclusive)
    }
    #[allow(unused_variables)]
    fn put_from(&mut self, key: &[u8], value: &dyn crate::storage::ValueRead) -> Result<()> {
        self.inner.put_from(key, value)
    }
    fn next_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
        self.inner.next_from(key, excl)
    }
    fn prev_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
        self.inner.prev_from(key, excl)
    }
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.inner.put(key, val)
    }
    fn del(&mut self, key: &[u8]) -> Result<bool> {
        self.inner.del(key)
    }
    fn commit(self) -> Result<()> {
        if self.fail.replace(false) {
            // Dropping the inner transaction discards it, like a backend
            // that could not make the changes durable.
            return Err(Error::Storage(28));
        }
        self.inner.commit()
    }
}

#[test]
fn a_failed_commit_leaves_the_committed_clock_untouched() {
    let storage =
        FailingStorage { inner: MemStorage::new(), fail_next_commit: core::cell::Cell::new(false) };
    let db = Db::create(storage, &CreateOptions::syncable([7; 32], 0xd15c)).unwrap();
    assert_eq!(db.props().retention, crate::storage::Retention::Ephemeral);
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"1").unwrap();
    let c1 = w.clock();
    w.commit().unwrap();
    assert_eq!(db.clock(), c1);

    db.storage.fail_next_commit.set(true);
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"b", b"2").unwrap();
    assert!(w.del_forget(b"a").unwrap());
    let failed = w.clock();
    assert!(failed > c1);
    assert!(matches!(w.commit(), Err(Error::Storage(28))));
    assert_eq!(db.clock(), c1, "a failed commit must not advance the committed clock");
    let read = db.begin_read().unwrap();
    assert!(read.get(b"b").unwrap().is_none());
    assert!(read.get(b"a").unwrap().is_some(), "a failed commit must leave the data as it was");
    drop(read);

    // The next writer continues from the committed clock, not the failed one.
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"c", b"3").unwrap();
    let c3 = w.clock();
    assert!(c3 > c1);
    w.commit().unwrap();
    assert_eq!(db.clock(), c3);
    let storage = db.close();
    let db = Db::open(storage, &Default::default()).unwrap();
    assert_eq!(db.clock(), c3);
}

#[test]
fn failed_commit_never_returns_publishable_delta() {
    let db = Db::create(
        FailingStorage { inner: MemStorage::new(), fail_next_commit: core::cell::Cell::new(false) },
        &CreateOptions::syncable([7; 32], 1),
    )
    .unwrap();
    let mut tx = db.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"k", b"value").unwrap();
    db.storage.fail_next_commit.set(true);
    assert!(matches!(tx.commit_with_delta(), Err(Error::Storage(28))));
    assert!(!db.begin_read().unwrap().has(b"k").unwrap());
    assert_eq!(db.clock(), Clock::ZERO);
}

#[test]
fn exhausted_stored_clock_refuses_writes_without_panicking() {
    for count in [u32::MAX - 1, u32::MAX] {
        let db = sync_db();
        let mut txn = db.storage.begin(true).unwrap();
        let mut meta = db.meta;
        meta.clock = Clock { time: u32::MAX, count };
        txn.put(&META_KEY, &meta.encode()).unwrap();
        txn.commit().unwrap();
        assert!(matches!(db.begin_write(T0), Err(Error::ClockExhausted)));
        assert!(db.begin_read().unwrap().get(b"k").unwrap().is_none());
        assert_eq!(
            Metadata::decode(db.storage.begin(false).unwrap().get(&META_KEY).unwrap().unwrap())
                .unwrap()
                .clock,
            meta.clock
        );
    }
}

#[test]
fn unsupported_metadata_fields_refuse_open_and_syncable_writes() {
    for syncable in [false, true] {
        for (offset, value) in [(5, 2), (5, 3), (5, u8::MAX), (7, 1)] {
            let db = if syncable { sync_db() } else { plain_db() };
            let mut metadata = db.meta.encode();
            metadata[offset] = value;
            let mut tx = db.storage.begin(true).unwrap();
            tx.put(&META_KEY, &metadata).unwrap();
            tx.commit().unwrap();
            if syncable {
                assert!(matches!(db.begin_write(T0), Err(Error::Version)));
            }
            assert_eq!(db.storage.begin(false).unwrap().get(&META_KEY).unwrap().unwrap(), metadata);
            assert!(matches!(Db::open(db.close(), &OpenOptions::default()), Err(Error::Version)));
        }
    }
}

#[test]
fn create_open_errors() {
    let s = MemStorage::new();
    let db = Db::create(s, &CreateOptions::plain()).unwrap();
    let s = db.close();
    assert!(matches!(Db::create(s, &CreateOptions::plain()), Err(Error::Exists)));

    assert!(matches!(Db::open(MemStorage::new(), &Default::default()), Err(Error::NotFound)));

    let mut bad = CreateOptions::syncable([0; 32], 1);
    bad.branch_factor = 1;
    assert!(matches!(Db::create(MemStorage::new(), &bad), Err(Error::InvalidArgument)));

    let db = Db::open_or_create(MemStorage::new(), &CreateOptions::plain(), &Default::default())
        .unwrap();
    let s = db.close();
    let db = Db::open_or_create(s, &CreateOptions::plain(), &Default::default()).unwrap();
    drop(db);
}

#[test]
fn key_and_value_limits() {
    let db = plain_db();
    let mut w = db.begin_write(T0).unwrap();
    assert!(matches!(w.put(b"", b"v"), Err(Error::InvalidKey)));
    assert!(matches!(w.put(&[0u8; KEY_SIZE_MAX + 1], b"v"), Err(Error::KeySize)));
    w.put(&[0xffu8; KEY_SIZE_MAX], b"v").unwrap(); // max size ok
    let oversized = alloc::vec![0u8; crate::VALUE_SIZE_MAX + 1];
    assert!(matches!(w.put(b"big", &oversized), Err(Error::ValueSize)));
    w.commit().unwrap();
    // (a compile-time assertion in the sync module pins VALUE_SIZE_MAX
    // to the wire format: max key + max value fill one frame exactly)
}

#[test]
fn write_txn_read_helpers_props_and_bytes_impls() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"1").unwrap();
    assert!(w.has(b"a").unwrap());
    assert_eq!(w.is_del(b"a").unwrap(), None);
    assert!(w.root_hash().unwrap().is_some());
    assert_eq!(w.iter().count(), 1);
    assert_eq!(w.iter_from(b"a").count(), 1);
    w.commit().unwrap();

    let p = db.props();
    assert_eq!(p.retention, crate::storage::Retention::Ephemeral);

    let r = db.begin_read().unwrap();
    let v = r.get(b"a").unwrap().unwrap();
    assert_eq!(v.as_ref(), b"1");
    assert_eq!(alloc::format!("{v:?}"), "Bytes(1 bytes)");
}

#[test]
fn plain_del_forget_and_rebuild_tree_mode() {
    let db = plain_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"k", b"v").unwrap();
    assert!(w.del_forget(b"k").unwrap());
    assert!(!w.del_forget(b"k").unwrap());
    assert!(matches!(w.rebuild_tree(), Err(Error::Mode)));
    w.commit().unwrap();
}

#[test]
fn rebuild_tree_reproduces_incremental_root() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    for i in 0..50u32 {
        w.put(alloc::format!("k{i}").as_bytes(), b"v").unwrap();
    }
    let before = w.root_hash().unwrap();
    w.rebuild_tree().unwrap();
    assert_eq!(w.root_hash().unwrap(), before);
    w.commit().unwrap();
}

#[test]
fn iterator_done_latch_and_corruption_reporting() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"1").unwrap();
    w.commit().unwrap();
    {
        let r = db.begin_read().unwrap();
        let mut it = r.iter();
        assert!(it.next().is_some());
        assert!(it.next().is_none());
        assert!(it.next().is_none()); // exhausted iterator stays done
    }

    // Inject a corrupt stored leaf value via the raw storage; reads
    // must surface Corrupt instead of panicking.
    let storage = db.close();
    {
        let mut t = storage.begin(true).unwrap();
        t.put(b"\x00bad", &[1, 2]).unwrap(); // shorter than any header
        t.commit().unwrap();
    }
    let db = Db::open(storage, &Default::default()).unwrap();
    let r = db.begin_read().unwrap();
    assert!(matches!(r.get(b"bad"), Err(Error::Corrupt)));
    assert!(matches!(r.is_del(b"bad"), Err(Error::Corrupt)));
    let mut it = r.iter();
    assert!(it.next().unwrap().is_ok()); // "a" is intact
    assert!(matches!(it.next(), Some(Err(Error::Corrupt))));
    assert!(it.next().is_none());
}

/// A backend whose transactions fail every operation, to exercise the
/// error-propagation arms that MemStorage can never produce.
struct BrokenStorage;
struct BrokenTxn;
impl Storage for BrokenStorage {
    type Txn<'db> = BrokenTxn;
    fn begin(&self, _write: bool) -> Result<BrokenTxn> {
        Ok(BrokenTxn)
    }
    fn props(&self) -> StorageProps {
        MemStorage::new().props()
    }
}
impl StorageTxn for BrokenTxn {
    fn visit(
        &self,
        _key: &[u8],
        _exclusive: bool,
        _reverse: bool,
        _visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        Err(Error::Storage(99))
    }
    type ValueReader = crate::storage::SliceValue<'static>;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<Self::ValueReader>> {
        let _ = key;
        Err(Error::Storage(99))
    }
    type Guard<'g> = &'g [u8];
    fn get<'g>(&'g self, _key: &[u8]) -> Result<Option<&'g [u8]>> {
        Err(Error::Storage(99))
    }
    #[allow(unused_variables)]
    fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
        Err(Error::Storage(99))
    }
    #[allow(unused_variables)]
    fn read_value_at(&self, key: &[u8], offset: usize, out: &mut [u8]) -> Result<Option<usize>> {
        Err(Error::Storage(99))
    }
    #[allow(unused_variables)]
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Err(Error::Storage(99))
    }
    #[allow(unused_variables)]
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Err(Error::Storage(99))
    }
    #[allow(unused_variables)]
    fn put_from(&mut self, key: &[u8], value: &dyn crate::storage::ValueRead) -> Result<()> {
        Err(Error::Storage(99))
    }
    fn next_from<'g>(&'g self, _key: &[u8], _excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
        Err(Error::Storage(99))
    }
    fn prev_from<'g>(&'g self, _key: &[u8], _excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
        Err(Error::Storage(99))
    }
    fn put(&mut self, _key: &[u8], _val: &[u8]) -> Result<()> {
        Err(Error::Storage(99))
    }
    fn del(&mut self, _key: &[u8]) -> Result<bool> {
        Err(Error::Storage(99))
    }
    fn commit(self) -> Result<()> {
        Err(Error::Storage(99))
    }
}

/// A backend whose transactions report an empty keyspace, to cover the
/// iterator's backend-exhausted arm (real databases always hold at
/// least the metadata record, so MemStorage cannot produce it).
struct EmptyStorage;
struct EmptyTxn;
impl Storage for EmptyStorage {
    type Txn<'db> = EmptyTxn;
    fn begin(&self, _write: bool) -> Result<EmptyTxn> {
        Ok(EmptyTxn)
    }
    fn props(&self) -> StorageProps {
        MemStorage::new().props()
    }
}
impl StorageTxn for EmptyTxn {
    fn visit(
        &self,
        _key: &[u8],
        _exclusive: bool,
        _reverse: bool,
        _visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        Ok(())
    }
    type ValueReader = crate::storage::SliceValue<'static>;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<Self::ValueReader>> {
        let _ = key;
        Ok(None)
    }
    type Guard<'g> = &'g [u8];
    fn get<'g>(&'g self, _key: &[u8]) -> Result<Option<&'g [u8]>> {
        Ok(None)
    }
    #[allow(unused_variables)]
    fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
        Ok(None)
    }
    #[allow(unused_variables)]
    fn read_value_at(&self, key: &[u8], offset: usize, out: &mut [u8]) -> Result<Option<usize>> {
        Ok(None)
    }
    #[allow(unused_variables)]
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    #[allow(unused_variables)]
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    #[allow(unused_variables)]
    fn put_from(&mut self, key: &[u8], value: &dyn crate::storage::ValueRead) -> Result<()> {
        Ok(())
    }
    fn next_from<'g>(&'g self, _key: &[u8], _excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
        Ok(None)
    }
    fn prev_from<'g>(&'g self, _key: &[u8], _excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
        Ok(None)
    }
    fn put(&mut self, _key: &[u8], _val: &[u8]) -> Result<()> {
        Ok(())
    }
    fn del(&mut self, _key: &[u8]) -> Result<bool> {
        Ok(false)
    }
    fn commit(self) -> Result<()> {
        Ok(())
    }
}

#[test]
fn iterator_handles_backend_exhaustion() {
    let db = Db {
        storage: EmptyStorage,
        meta: Metadata {
            syncable: false,
            branch_factor: 0,
            replica_id: 0,
            clock: Clock::ZERO,
            hash_key: [0; 32],
        },
        clock: CommittedClock::new(Clock::ZERO),
    };
    {
        let r = db.begin_read().unwrap();
        assert_eq!(r.iter().count(), 0);
    }
    // exercise the remaining EmptyTxn trait methods
    let mut t = db.storage.begin(true).unwrap();
    assert_eq!(t.get(b"k").unwrap(), None);
    assert_eq!(t.prev_from(b"k", false).unwrap(), None);
    t.put(b"k", b"v").unwrap();
    assert!(!t.del(b"k").unwrap());
    t.commit().unwrap();
    assert_eq!(db.props().retention, crate::storage::Retention::Ephemeral);
}

#[test]
fn backend_errors_propagate() {
    let db = Db {
        storage: BrokenStorage,
        meta: Metadata {
            syncable: true,
            branch_factor: 32,
            replica_id: 0,
            clock: Clock::ZERO,
            hash_key: [0; 32],
        },
        clock: CommittedClock::new(Clock::ZERO),
    };
    let r = db.begin_read().unwrap();
    assert!(matches!(r.get(b"k"), Err(Error::Storage(99))));
    assert_eq!(r.visit(b"", false, |_, _| Ok::<_, Error>(true)), Err(Error::Storage(99)));
    assert!(matches!(r.is_del(b"k"), Err(Error::Storage(99))));
    assert!(matches!(r.root_hash(), Err(Error::Storage(99))));
    let mut it = r.iter();
    assert!(matches!(it.next(), Some(Err(Error::Storage(99)))));
    assert!(it.next().is_none()); // an errored iterator is done
    drop(it);
    let _ = r;
    // A syncable writer reads the committed clock first and reports the
    // failure; a plain writer reaches the write itself.
    assert!(matches!(db.begin_write(T0), Err(Error::Storage(99))));
    let plain = Db {
        storage: BrokenStorage,
        meta: Metadata { syncable: false, ..db.meta },
        clock: CommittedClock::new(Clock::ZERO),
    };
    let mut w = plain.begin_write(T0).unwrap();
    assert!(matches!(w.put(b"k", b"v"), Err(Error::Storage(99))));
    assert!(matches!(w.commit(), Err(Error::Storage(99))));
    // remaining BrokenTxn trait methods
    let mut t = BrokenTxn;
    assert!(matches!(t.del(b"k"), Err(Error::Storage(99))));
    assert!(matches!(t.prev_from(b"k", false), Err(Error::Storage(99))));
    assert!(matches!(BrokenTxn.commit(), Err(Error::Storage(99))));
    assert_eq!(db.props().retention, crate::storage::Retention::Ephemeral);
}

#[test]
fn replicas_with_same_data_but_different_versions_have_different_roots() {
    let mk = || Db::create(MemStorage::new(), &CreateOptions::syncable([3; 32], 1)).unwrap();
    let (a, b) = (mk(), mk());
    // different orders, different wall clocks
    let mut w = a.begin_write(T0).unwrap();
    w.put(b"x", b"1").unwrap();
    w.put(b"y", b"2").unwrap();
    w.commit().unwrap();
    let mut w = b.begin_write(T0 + 500).unwrap();
    w.put(b"y", b"2").unwrap();
    w.commit().unwrap();
    let mut w = b.begin_write(T0 + 900).unwrap();
    w.put(b"x", b"1").unwrap();
    w.commit().unwrap();
    let ra = a.begin_read().unwrap().root_hash().unwrap();
    let rb = b.begin_read().unwrap().root_hash().unwrap();
    assert_ne!(ra, rb);
    assert!(ra.is_some());
}

#[test]
fn deferred_delta_capture_requires_a_syncable_database() {
    let db = plain_db();
    assert!(matches!(db.begin_write_with_delta(T0, 64), Err(Error::Mode)));
}

#[test]
fn bounded_snapshot_reads_hide_headers_and_tombstones_in_both_layouts() {
    use crate::storage::ValueRead;
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"small", b"tiny").unwrap(); // compact header
    w.put(b"large", &[7u8; 100]).unwrap(); // full header
    w.put(b"gone", b"x").unwrap();
    w.commit().unwrap();
    let mut w = db.begin_write(T0 + 1).unwrap();
    assert!(w.del(b"gone").unwrap());
    w.commit().unwrap();
    let r = db.begin_read().unwrap();
    let mut out = [0; 8];
    for absent in [&b"missing"[..], b"gone"] {
        assert!(r.value_reader(absent).unwrap().is_none());
        assert_eq!(r.value_len(absent).unwrap(), None);
        assert_eq!(r.read_value_at(absent, 0, &mut out).unwrap(), None);
    }
    let small = r.value_reader(b"small").unwrap().unwrap();
    assert_eq!(small.clock(), Some(r.get_with_clock(b"small").unwrap().unwrap().0));
    assert_eq!(small.len(), 4);
    assert!(!small.is_empty());
    assert_eq!(small.read_at(1, &mut out).unwrap(), 3);
    assert_eq!(&out[..3], b"iny");
    assert_eq!(small.read_at(4, &mut out).unwrap(), 0);
    assert_eq!(small.read_at(5, &mut out), Err(Error::InvalidArgument));
    let large = r.value_reader(b"large").unwrap().unwrap();
    assert_eq!(large.clock(), small.clock());
    assert_eq!(large.len(), 100);
    assert_eq!(large.read_at(96, &mut out).unwrap(), 4);
    assert_eq!(out[..4], [7; 4]);
    assert_eq!(r.value_len(b"small").unwrap(), Some(4));
    assert_eq!(r.value_len(b"large").unwrap(), Some(100));
    assert_eq!(r.read_value_at(b"small", 2, &mut out).unwrap(), Some(2));
    assert_eq!(&out[..2], b"ny");
    assert_eq!(r.read_value_at(b"large", 96, &mut out).unwrap(), Some(4));
    assert_eq!(r.read_value_at(b"small", 5, &mut out), Err(Error::InvalidArgument));

    // Plain databases store the user value verbatim.
    let db = plain_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"k", b"plain value").unwrap();
    w.commit().unwrap();
    let r = db.begin_read().unwrap();
    assert_eq!(r.value_len(b"k").unwrap(), Some(11));
    assert_eq!(r.read_value_at(b"k", 6, &mut out).unwrap(), Some(5));
    assert_eq!(&out[..5], b"value");
    let v = r.value_reader(b"k").unwrap().unwrap();
    assert_eq!(v.clock(), None);
    assert_eq!(v.len(), 11);
    assert_eq!(v.read_at(0, &mut out).unwrap(), 8);
    assert_eq!(&out, b"plain va");
}

#[test]
fn bounded_snapshot_reads_reject_unknown_entry_flags() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"ok", b"fine").unwrap();
    w.commit().unwrap();
    let storage = db.close();
    let mut raw = storage.begin(true).unwrap();
    // Unknown flag bits in both layouts. The compact decoder rejects them
    // itself; the full layout leaves the check to each reader.
    let mut compact = vec![0; encoding::ENT_HEADER_COMPACT + 3];
    compact[1] = 2;
    let mut full = vec![0; encoding::ENT_HEADER_FULL + 3];
    full[32] = 2;
    raw.put(b"\0compact", &compact).unwrap();
    raw.put(b"\0full", &full).unwrap();
    raw.commit().unwrap();
    let db = Db::open(storage, &Default::default()).unwrap();
    let r = db.begin_read().unwrap();
    for bad in [&b"compact"[..], b"full"] {
        assert!(matches!(r.value_reader(bad), Err(Error::Corrupt)));
        assert_eq!(r.value_len(bad), Err(Error::Corrupt));
        assert_eq!(r.read_value_at(bad, 0, &mut [0; 2]), Err(Error::Corrupt));
        assert!(matches!(r.get_with_clock(bad), Err(Error::Corrupt)));
    }
    assert!(matches!(r.get_with_clock(b"ok"), Ok(Some(_))));
}

#[test]
fn next_key_skips_tombstones_and_stops_at_the_end_of_the_user_keyspace() {
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"a", b"1").unwrap();
    w.put(b"b", b"2").unwrap();
    w.put(b"c", b"3").unwrap();
    w.commit().unwrap();
    let mut w = db.begin_write(T0 + 1).unwrap();
    assert!(w.del(b"b").unwrap());
    w.commit().unwrap();
    let r = db.begin_read().unwrap();
    assert_eq!(r.next_key(b"", false).unwrap().as_deref(), Some(&b"a"[..]));
    assert_eq!(r.next_key(b"a", false).unwrap().as_deref(), Some(&b"a"[..]));
    assert_eq!(r.next_key(b"a", true).unwrap().as_deref(), Some(&b"c"[..]));
    assert_eq!(r.next_key(b"b", false).unwrap().as_deref(), Some(&b"c"[..]));
    // Beyond the last live key only tree nodes and metadata remain.
    assert_eq!(r.next_key(b"c", true).unwrap(), None);
    assert_eq!(r.next_key(&[0; KEY_SIZE_MAX + 1], false), Err(Error::KeySize));

    // A plain database has no tree nodes after the user keys at all.
    let db = plain_db();
    let mut w = db.begin_write(T0).unwrap();
    w.put(b"only", b"1").unwrap();
    w.commit().unwrap();
    let r = db.begin_read().unwrap();
    assert_eq!(r.next_key(b"", false).unwrap().as_deref(), Some(&b"only"[..]));
    assert_eq!(r.next_key(b"only", true).unwrap(), None);
}

#[test]
fn put_from_checks_the_value_bound_and_writes_plain_databases_directly() {
    use crate::storage::SliceValue;
    let oversized = vec![0u8; crate::VALUE_SIZE_MAX + 1];
    let db = plain_db();
    let mut w = db.begin_write(T0).unwrap();
    assert_eq!(w.put_from(b"k", &SliceValue(&oversized)), Err(Error::ValueSize));
    w.put_from(b"k", &SliceValue(b"streamed")).unwrap();
    w.commit().unwrap();
    assert_eq!(collect(&db), vec![(b"k".to_vec(), b"streamed".to_vec())]);
    let db = sync_db();
    let mut w = db.begin_write(T0).unwrap();
    assert_eq!(w.put_from(b"k", &SliceValue(&oversized)), Err(Error::ValueSize));
}

#[test]
fn streamed_origin_copy_preserves_clock_and_checks_inputs_before_effects() {
    use crate::storage::{SliceValue, ValueRead};
    struct Bounded(usize);
    impl ValueRead for Bounded {
        fn len(&self) -> usize {
            self.0
        }
        fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
            assert!(out.len() <= 4096, "copy never requests a content-sized buffer");
            let count = out.len().min(self.len() - offset);
            out[..count].fill(0x42);
            Ok(count)
        }
    }
    let db = sync_db();
    let origin = Clock::from_u64(9_007_199_254_740_993);
    let mut tx = db.begin_write(T0).unwrap();
    let before = tx.clock();
    assert_eq!(
        tx.put_from_with_clock(b"oversize", &Bounded(crate::VALUE_SIZE_MAX + 1), origin),
        Err(Error::ValueSize)
    );
    assert_eq!(tx.clock(), before);
    assert!(tx.get(b"oversize").unwrap().is_none());
    assert_eq!(
        tx.put_from_with_clock(b"bad", &SliceValue(b"x"), Clock::ZERO),
        Err(Error::InvalidArgument)
    );
    tx.put_from_with_clock(b"large", &Bounded(crate::VALUE_SIZE_MAX), origin).unwrap();
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    let value = read.value_reader(b"large").unwrap().unwrap();
    assert_eq!(value.len(), crate::VALUE_SIZE_MAX);
    assert_eq!(value.clock(), Some(origin));
    assert!(!read.has(b"bad").unwrap());
    db.verify().unwrap();
    let plain = plain_db();
    assert_eq!(
        plain.begin_write(T0).unwrap().put_from_with_clock(b"k", &SliceValue(b"x"), origin),
        Err(Error::Mode)
    );
    let mut tx = db.begin_write(T0 + 1).unwrap();
    let before = tx.clock();
    assert_eq!(
        tx.put_from_with_clock(b"exhausted", &SliceValue(b"x"), Clock::from_u64(u64::MAX)),
        Err(Error::ClockExhausted)
    );
    assert_eq!(tx.clock(), before);
    assert!(tx.get(b"exhausted").unwrap().is_none());
}

#[test]
fn verified_open_checks_committed_tree_and_preserves_corruption() {
    // Opt-in open accepts canonical storage, rejects a corrupt root, and
    // leaves the corrupt bytes intact for diagnosis. Plain stores need no tree.
    let options = OpenOptions { verify_on_open: true };
    let db = Db::create(MemStorage::new(), &CreateOptions::syncable([5; 32], 1)).unwrap();
    db.verify().unwrap();
    let mut write = db.begin_write(1_800_000_000).unwrap();
    write.put(b"key", b"value").unwrap();
    write.verify().unwrap();
    write.commit().unwrap();
    let db = Db::open(db.close(), &options).unwrap();
    db.verify().unwrap();
    let storage = db.close();
    let mut raw = storage.begin(true).unwrap();
    raw.put(&[1], &[0; 32]).unwrap();
    raw.commit().unwrap();
    let unchecked = Db::open(storage, &OpenOptions::default()).unwrap();
    assert_eq!(unchecked.verify(), Err(Error::Corrupt));
    assert_eq!(unchecked.begin_read().unwrap().get(b"key").unwrap().unwrap().as_ref(), b"value");
    assert!(matches!(Db::open(unchecked.close(), &options), Err(Error::Corrupt)));
    let plain = Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap();
    assert_eq!(plain.verify(), Err(Error::Mode));
    let mut write = plain.begin_write(0).unwrap();
    assert_eq!(write.verify(), Err(Error::Mode));
    drop(write);
    Db::open(plain.close(), &options).unwrap();
}

#[test]
fn explicit_verification_rejects_inconsistent_tree_before_publication() {
    // Diagnostics verify before committing and propagate errors. Simulate an
    // internal maintenance bug; dropping the failed transaction must preserve
    // the previous snapshot/clock and release the writer for retry in any build.
    let db = Db::create(MemStorage::new(), &CreateOptions::syncable([5; 32], 1)).unwrap();
    let result = (|| {
        let mut write = db.begin_write(1_800_000_000)?;
        write.put(b"key", b"value")?;
        write.inner.txn.del(&[1])?;
        write.verify()?;
        write.commit()
    })();
    assert_eq!(result, Err(Error::Corrupt));
    assert_eq!(db.clock(), Clock::ZERO);
    assert!(!db.begin_read().unwrap().has(b"key").unwrap());
    let mut write = db.begin_write(1_800_000_000).unwrap();
    write.put(b"key", b"retry").unwrap();
    write.verify().unwrap();
    write.commit().unwrap();
    db.verify().unwrap();
}

#[test]
fn commit_does_not_implicitly_audit_untouched_storage() {
    // An invalid raw tail entry is a scan sentinel: ordinary commits must not
    // traverse it, even in debug builds. Explicit diagnostics must still find
    // it. This bypasses kvdb's write API only to detect accidental whole scans.
    let db = sync_db();
    let mut write = db.begin_write(T0).unwrap();
    write.put(b"key", b"value").unwrap();
    write.inner.txn.put(&[0xff, 0], b"scan sentinel").unwrap();
    let clock = write.clock();
    write.commit().unwrap();
    assert_eq!(db.clock(), clock);
    assert_eq!(collect(&db), vec![(b"key".to_vec(), b"value".to_vec())]);
    assert_eq!(db.verify(), Err(Error::Corrupt));
}
