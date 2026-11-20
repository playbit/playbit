#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::db::CreateOptions;
use crate::storage::mem::MemStorage;
use alloc::format;
use alloc::vec;

#[derive(Default)]
struct TestStaging(Vec<u8>);
impl crate::storage::ValueRead for TestStaging {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        crate::storage::SliceValue(&self.0).read_at(offset, out)
    }
}
impl SyncStaging for TestStaging {
    fn clear(&mut self) -> Result<()> {
        self.0.clear();
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}
fn drain_sender<S: Storage>(
    send: &mut SyncSender<'_, S>,
    input: &[u8],
    out: &mut Vec<u8>,
) -> Result<SyncStatus> {
    let status = send.process(input);
    let mut chunk = [0; 1024];
    loop {
        let n = send.write_output(&mut chunk)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    status
}

const T0: u64 = crate::clock::EPOCH_UNIX_SECS + 5000;
const HASH_KEY: [u8; 32] = [0xaa; 32];

fn mk_db() -> Db<MemStorage> {
    let mut opts = CreateOptions::syncable(HASH_KEY, 0);
    opts.branch_factor = 4; // deeper trees with small data sets
    Db::create(MemStorage::new(), &opts).unwrap()
}

#[test]
fn independent_lww_tears_a_locally_atomic_replicated_name_mapping() {
    // Reproduce the motivating failure using the real default resolver. Both
    // offline renames are atomic locally, but both new aliases survive sync.
    let a = mk_db();
    let b = mk_db();
    put_all(&a, T0, &[(b"entry/10", b"notes"), (b"name/notes", b"10")]);
    pump(&b, &a);
    for (db, name, wall) in [(&a, b"work".as_slice(), T0 + 1), (&b, b"personal", T0 + 2)] {
        let mut tx = db.begin_write(wall).unwrap();
        tx.put(b"entry/10", name).unwrap();
        tx.del(b"name/notes").unwrap();
        let key = [b"name/".as_slice(), name].concat();
        tx.put(&key, b"10").unwrap();
        tx.commit().unwrap();
    }
    pump(&a, &b);
    pump(&b, &a);
    for db in [&a, &b] {
        let read = db.begin_read().unwrap();
        assert_eq!(read.get(b"entry/10").unwrap().unwrap().as_ref(), b"personal");
        assert_eq!(read.get(b"name/work").unwrap().unwrap().as_ref(), b"10");
        assert_eq!(read.get(b"name/personal").unwrap().unwrap().as_ref(), b"10");
    }
}

fn name_fixture_update(index: &mut crate::LocalIndexes<'_, '_, MemStorage>) -> Result<()> {
    // Three known names and two IDs bound this regression fixture. Production
    // volume repair must seek only affected names instead of rebuilding a catalog.
    for name in [b"notes".as_slice(), b"work", b"personal"] {
        let mut winner = None;
        for id in [b"10", b"20"] {
            let key = [b"entry/".as_slice(), id].concat();
            if index.source_get(&key)?.is_some_and(|value| value.as_ref() == name) {
                winner = Some(id);
            }
        }
        let key = [b"name/".as_slice(), name].concat();
        match winner {
            Some(id) => index.put(&key, id)?,
            None => {
                index.del(&key)?;
            }
        }
    }
    Ok(())
}

fn name_fixture_put(db: &Db<MemStorage>, id: &[u8], name: &[u8], wall: u64) -> Vec<u8> {
    let mut tx = db.begin_write_with_delta(wall, 4096).unwrap();
    tx.put(&[b"entry/".as_slice(), id].concat(), name).unwrap();
    name_fixture_update(&mut tx.local_indexes()).unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    bytes
}

fn name_fixture_receive(db: &Db<MemStorage>, bytes: &[u8]) -> SyncOutcome {
    let mut rx = DeltaReceiver::new(db, Lww, T0 + 3, 4096).unwrap();
    // Fragment down to one byte, independently of the source transaction size.
    for fragment in bytes.chunks(1) {
        rx.process(fragment).unwrap();
    }
    rx.repair_local_indexes(name_fixture_update).unwrap().finish().unwrap()
}

#[test]
fn local_name_projection_converges_under_reordered_duplicate_offline_renames() {
    // Each committed snapshot has exactly the alias selected by its source
    // record, even before convergence and with the older rename delivered last.
    for reverse in [false, true] {
        let a = mk_db();
        let b = mk_db();
        let initial = name_fixture_put(&a, b"10", b"notes", T0);
        name_fixture_receive(&b, &initial);
        let da = name_fixture_put(&a, b"10", b"work", T0 + 1);
        let db = name_fixture_put(&b, b"10", b"personal", T0 + 2);
        let old = a.begin_read().unwrap();
        if reverse {
            name_fixture_receive(&b, &da);
            name_fixture_receive(&a, &db);
        } else {
            name_fixture_receive(&a, &db);
            name_fixture_receive(&b, &da);
        }
        for target in [&a, &b] {
            for delta in [&initial, &da, &db] {
                assert!(!name_fixture_receive(target, delta).committed);
            }
            let read = target.begin_read().unwrap();
            assert_eq!(read.get(b"entry/10").unwrap().unwrap().as_ref(), b"personal");
            assert_eq!(read.local_get(b"name/personal").unwrap().unwrap().as_ref(), b"10");
            assert!(read.local_get(b"name/work").unwrap().is_none());
            assert!(read.local_get(b"name/notes").unwrap().is_none());
            assert!(read.get(b"name/personal").unwrap().is_none());
            target.verify().unwrap();
        }
        assert_eq!(old.get(b"entry/10").unwrap().unwrap().as_ref(), b"work");
        assert_eq!(old.local_get(b"name/work").unwrap().unwrap().as_ref(), b"10");
        assert_eq!(
            a.begin_read().unwrap().root_hash().unwrap(),
            b.begin_read().unwrap().root_hash().unwrap()
        );
    }
}

#[test]
fn competing_names_preserve_both_records_and_reveal_next_candidate_after_rename() {
    // Offline entries choose one deterministic name owner. Renaming the owner
    // exposes the remaining candidate without deleting or renaming its data.
    for reverse in [false, true] {
        let a = mk_db();
        let b = mk_db();
        let da = name_fixture_put(&a, b"10", b"notes", T0);
        let db = name_fixture_put(&b, b"20", b"notes", T0);
        if reverse {
            name_fixture_receive(&b, &da);
            name_fixture_receive(&a, &db);
        } else {
            name_fixture_receive(&a, &db);
            name_fixture_receive(&b, &da);
        }
        for target in [&a, &b] {
            let read = target.begin_read().unwrap();
            assert_eq!(read.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"20");
            assert_eq!(read.get(b"entry/10").unwrap().unwrap().as_ref(), b"notes");
            assert_eq!(read.get(b"entry/20").unwrap().unwrap().as_ref(), b"notes");
        }
        let rename = name_fixture_put(&b, b"20", b"work", T0 + 1);
        name_fixture_receive(&a, &rename);
        for target in [&a, &b] {
            let read = target.begin_read().unwrap();
            assert_eq!(read.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"10");
            assert_eq!(read.local_get(b"name/work").unwrap().unwrap().as_ref(), b"20");
        }
    }
}

#[test]
fn delta_index_repair_failure_rolls_back_source_and_never_runs_on_invalid_input() {
    // Repair errors abort both namespaces and clock publication. Incomplete,
    // corrupt, denied and duplicate deltas cannot enter the repair callback.
    let a = mk_db();
    let b = mk_db();
    let delta = name_fixture_put(&a, b"10", b"notes", T0);
    let before = b.clock();
    let mut rx = DeltaReceiver::new_with_delta(&b, Lww, T0, 4096).unwrap();
    rx.process(&delta).unwrap();
    let result = rx.repair_local_indexes(|index| {
        name_fixture_update(index)?;
        Err::<(), _>(Error::Storage(91))
    });
    assert_eq!(result.err(), Some(Error::Storage(91)));
    assert_eq!(b.clock(), before);
    let read = b.begin_read().unwrap();
    assert!(read.get(b"entry/10").unwrap().is_none());
    assert!(read.local_get(b"name/notes").unwrap().is_none());
    for failed in [false, true] {
        let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
        if failed {
            assert!(rx.process(&[0; 32]).is_err());
        } else {
            rx.process(&delta[..delta.len() - 1]).unwrap();
        }
        assert_eq!(
            rx.repair_local_indexes::<Error>(|_| panic!("invalid input reached repair")).err(),
            Some(Error::Protocol)
        );
    }
    let mut rx = DeltaReceiver::new(&b, |_: &Delta<'_>| Resolution::KeepLocal, T0, 4096).unwrap();
    rx.process(&delta).unwrap();
    assert!(
        !rx.repair_local_indexes::<Error>(|_| panic!("denied source reached repair"))
            .unwrap()
            .finish()
            .unwrap()
            .committed
    );
    name_fixture_receive(&b, &delta);
    let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
    rx.process(&delta).unwrap();
    assert!(
        !rx.repair_local_indexes::<Error>(|_| panic!("duplicate reached repair"))
            .unwrap()
            .finish()
            .unwrap()
            .committed
    );
}

#[test]
fn full_sync_repairs_local_names_without_transferring_remote_secondary_records() {
    // Full sync transfers only source state, then derives recipient names in its
    // commit. An untrusted source mapping cannot become recipient authority.
    let a = mk_db();
    let b = mk_db();
    name_fixture_put(&a, b"10", b"notes", T0);
    let mut tx = a.begin_write(T0 + 1).unwrap();
    tx.local_indexes().put(b"name/forged", b"99").unwrap();
    tx.commit().unwrap();
    for fail in [true, false] {
        let mut out = Vec::new();
        let mut rx =
            SyncReceiver::new_with_delta(&b, Lww, TestStaging::default(), T0, 4096, &mut out)
                .unwrap();
        exchange(&mut rx, &a, out, 3).unwrap();
        let result = rx.repair_local_indexes(|index| {
            name_fixture_update(index)?;
            if fail { Err(Error::Storage(91)) } else { Ok(()) }
        });
        if fail {
            assert_eq!(result.err(), Some(Error::Storage(91)));
            assert!(b.begin_read().unwrap().get(b"entry/10").unwrap().is_none());
            assert!(b.begin_read().unwrap().local_get(b"name/notes").unwrap().is_none());
        } else {
            let (outcome, Some(CommittedDelta::Bytes(delta))) =
                result.unwrap().finish_with_delta().unwrap()
            else {
                panic!()
            };
            assert!(outcome.committed);
            visit_delta(&delta, 4096, |key, _| assert_eq!(key, b"entry/10")).unwrap();
        }
    }
    let read = b.begin_read().unwrap();
    assert_eq!(read.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"10");
    assert!(read.local_get(b"name/forged").unwrap().is_none());
    b.verify().unwrap();
    let mut out = Vec::new();
    let mut rx = SyncReceiver::new(&b, Lww, TestStaging::default(), T0, &mut out).unwrap();
    exchange(&mut rx, &a, out, 1).unwrap();
    assert!(
        !rx.repair_local_indexes::<Error>(|_| panic!("equal roots reached repair"))
            .unwrap()
            .finish()
            .unwrap()
            .committed
    );
    let mut out = Vec::new();
    let rx = SyncReceiver::new(&b, Lww, TestStaging::default(), T0, &mut out).unwrap();
    assert_eq!(
        rx.repair_local_indexes::<Error>(|_| panic!("incomplete sync reached repair")).err(),
        Some(Error::Protocol)
    );
}

/// Drive a complete session over an in-memory pipe, delivering bytes in
/// `drip`-sized fragments to exercise the framer.
fn pump_with<R: SyncResolver>(
    dst: &Db<MemStorage>,
    src: &Db<MemStorage>,
    resolver: R,
    _cap: usize,
    drip: usize,
) -> Result<SyncOutcome> {
    let mut r_out = Vec::new();
    let mut recv = SyncReceiver::new(dst, resolver, TestStaging::default(), T0, &mut r_out)?;
    exchange(&mut recv, src, r_out, drip)?;
    recv.finish()
}

fn exchange<R: SyncResolver>(
    recv: &mut SyncReceiver<'_, MemStorage, R, TestStaging>,
    src: &Db<MemStorage>,
    mut r_out: Vec<u8>,
    drip: usize,
) -> Result<()> {
    let mut send = SyncSender::new(src)?;
    let mut s_out = Vec::new();
    let (mut r_done, mut s_done) = (false, false);
    let mut rounds = 0;
    while !r_done || !s_done {
        rounds += 1;
        assert!(rounds < 100_000, "sync made no progress");
        if !r_out.is_empty() {
            let data = core::mem::take(&mut r_out);
            for chunk in data.chunks(drip.max(1)) {
                s_done = drain_sender(&mut send, chunk, &mut s_out)? == SyncStatus::Done;
            }
        } else if !s_out.is_empty() {
            let data = core::mem::take(&mut s_out);
            for chunk in data.chunks(drip.max(1)) {
                r_done = recv.process(chunk, &mut r_out)? == SyncStatus::Done;
                assert!(recv.repair.count <= 32);
            }
        } else {
            // Both directions idle before completion: the protocol
            // stalled, which would be a bug.
            panic!("sync stalled (r_done={r_done} s_done={s_done})");
        }
    }
    Ok(())
}

fn pump(dst: &Db<MemStorage>, src: &Db<MemStorage>) -> SyncOutcome {
    pump_with(dst, src, Lww, DEFAULT_MSG_CAP, usize::MAX).unwrap()
}

fn put_all(db: &Db<MemStorage>, wall: u64, kvs: &[(&[u8], &[u8])]) {
    let mut w = db.begin_write(wall).unwrap();
    for (k, v) in kvs {
        w.put(k, v).unwrap();
    }
    w.commit().unwrap();
}

fn contents(db: &Db<MemStorage>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let r = db.begin_read().unwrap();

    r.iter().map(|kv| kv.map(|(k, v)| (k.to_vec(), v.to_vec()))).collect::<Result<_>>().unwrap()
}

fn root(db: &Db<MemStorage>) -> Option<(u8, [u8; 32])> {
    db.begin_read().unwrap().root_hash().unwrap()
}

/// Build one wire frame (test helper mirroring the sender).
fn frame(typ: u16, flags: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let at = frame_begin(&mut out, typ, flags);
    out.extend_from_slice(payload);
    frame_end(&mut out, at);
    out
}

fn pull_response(status: u8, root_level: u8, root_hash: [u8; 32], clock: Clock) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(status);
    p.push(root_level);
    p.extend_from_slice(&[0; 2]);
    p.extend_from_slice(&root_hash);
    p.extend_from_slice(&clock.encode());
    frame(T_PULL_RES, 0, &p)
}

/// A fresh receiver with its emitted PullRequest discarded, ready to be
/// fed crafted response frames.
fn crafted_receiver(db: &Db<MemStorage>) -> SyncReceiver<'_, MemStorage, Lww, TestStaging> {
    let mut out = Vec::new();
    SyncReceiver::new(db, Lww, TestStaging::default(), T0, &mut out).unwrap()
}

fn pushed_put(db: &Db<MemStorage>, time: u64, key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut tx = db.begin_write_with_delta(time, 4096).unwrap();
    tx.put(key, value).unwrap();
    match tx.commit_with_delta().unwrap() {
        CommittedDelta::Bytes(bytes) => bytes,
        _ => panic!("small transaction requested pull"),
    }
}

#[test]
fn full_receipt_batches_exact_limits_and_preserves_capture_and_origin_clocks() {
    for count in [0u32, 1, 31, 32, 33, 1000] {
        let (source, target, captured) = (mk_db(), mk_db(), mk_db());
        let mut write = source.begin_write(T0).unwrap();
        let origin = write.clock();
        for n in 0..count {
            let key = n.to_be_bytes();
            if n % 7 == 0 {
                write.put(&key, b"before deletion").unwrap();
                write.del(&key).unwrap();
            } else {
                write.put(&key, &vec![n as u8; 30 + n as usize % 3]).unwrap();
            }
        }
        write.commit().unwrap();
        let clock_before = target.clock();
        let mut request = Vec::new();
        let mut receiver = SyncReceiver::new_with_delta(
            &target,
            Lww,
            TestStaging::default(),
            T0,
            256 * 1024,
            &mut request,
        )
        .unwrap();
        exchange(&mut receiver, &source, request, 97).unwrap();
        assert_eq!(receiver.applied, count as usize);
        assert_eq!(
            receiver.repair.count,
            if count == 0 { 0 } else { ((count - 1) % 32 + 1) as u8 }
        );
        if (1..=32).contains(&count) {
            assert_eq!(
                receiver.wtxn.as_ref().unwrap().root_hash().unwrap(),
                None,
                "repair must be deferred"
            );
        }
        assert_eq!(root(&target), None, "internal repair must not publish a partial transaction");
        assert_eq!(target.clock(), clock_before);
        let (outcome, delta) = receiver.finish_with_delta().unwrap();
        assert_eq!(outcome.applied, count as usize);
        assert_eq!(outcome.committed, count != 0);
        assert_eq!(root(&target), root(&source));
        assert_eq!(contents(&target), contents(&source));
        if count == 0 {
            assert!(delta.is_none());
            assert_eq!(target.clock(), clock_before);
        } else {
            let Some(CommittedDelta::Bytes(bytes)) = delta else {
                panic!("bounded capture must fit")
            };
            let mut seen = 0;
            visit_delta(&bytes, 256 * 1024, |_, state| {
                seen += 1;
                match state {
                    EntState::Present { clock, .. } | EntState::Deleted { clock } => {
                        assert_eq!(clock, origin)
                    }
                    EntState::Missing => panic!("stored entries cannot be missing"),
                }
            })
            .unwrap();
            assert_eq!(seen, count);
            let mut pushed = DeltaReceiver::new(&captured, Lww, T0, 256 * 1024).unwrap();
            pushed.process(&bytes).unwrap();
            pushed.finish().unwrap();
            assert_eq!(root(&captured), root(&source));
        }
    }
}

#[test]
fn full_receipt_uses_current_parent_interval_after_a_flush() {
    use crate::storage::SliceValue;
    let db = mk_db();
    let reference = mk_db();
    let mut txn = db.begin_write(T0).unwrap();
    let mut ordinary = reference.begin_write(T0).unwrap();
    let mut repair = PendingRepair::default();
    let clock = txn.clock();
    for n in 1..=32u32 {
        let key = n.to_be_bytes();
        let value = (0u32..)
            .find_map(|nonce| {
                let value = nonce.to_le_bytes();
                let hash = crate::hash::entry_hash(&HASH_KEY, &key, 0, &value);
                encoding::is_boundary(hash[0], 4).then_some(value)
            })
            .unwrap();
        ordinary.put(&key, &value).unwrap();
        repair.prepare(&mut txn, &key).unwrap();
        let stable =
            txn.sync_put_from_deferred(&key, &SliceValue(&value), 0, clock).unwrap().unwrap();
        repair.changed(&key, stable);
    }
    assert_eq!(repair.count, 32);
    assert_eq!(txn.root_hash().unwrap(), None);
    // The original snapshot was empty. A stale interval lookup would give
    // []..None here, instead of the newly created neighboring boundaries.
    repair.prepare(&mut txn, &16u32.to_be_bytes()).unwrap();
    assert_eq!(repair.count, 0);
    assert_eq!(
        repair.interval,
        Some((16u32.to_be_bytes().to_vec(), Some(17u32.to_be_bytes().to_vec())))
    );
    assert_eq!(txn.root_hash().unwrap(), ordinary.root_hash().unwrap());
    let key = 16u32.to_be_bytes();
    let stable =
        txn.sync_put_from_deferred(&key, &SliceValue(b"changed"), 0, clock).unwrap().unwrap();
    repair.changed(&key, stable);
    ordinary.put(&key, b"changed").unwrap();
    // Crossing the captured upper boundary forces repair before the next
    // write, even though the preceding batch contained only one change.
    repair.prepare(&mut txn, &17u32.to_be_bytes()).unwrap();
    assert_eq!(repair.count, 0);
    assert_eq!(txn.root_hash().unwrap(), ordinary.root_hash().unwrap());
    repair.flush(&mut txn).unwrap();
    txn.commit().unwrap();
    ordinary.commit().unwrap();
    assert_eq!(root(&db), root(&reference));
}

#[test]
fn full_receipt_rolls_back_after_an_internal_flush_and_later_error_or_drop() {
    let entries: Vec<_> = (0..33u32)
        .map(|n| {
            let key = n.to_be_bytes().to_vec();
            let mut value = Vec::new();
            let clock = Clock { time: n + 2, count: 0 };
            let hash = crate::hash::entry_hash(&HASH_KEY, &key, 0, b"new");
            encoding::encode_ent_val(&mut value, &hash, 0, clock, b"new");
            (key, value)
        })
        .collect();
    let borrowed: Vec<_> = entries.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
    for corrupt in [false, true] {
        let db = mk_db();
        let before = db.clock();
        let mut receiver = receiver_awaiting_nodes(&db, 1);
        receiver.process(&nodes_response(0, F_MORE, &borrowed), &mut Vec::new()).unwrap();
        assert_eq!(receiver.applied, 33);
        assert_eq!(receiver.repair.count, 1);
        assert!(
            receiver.wtxn.as_ref().unwrap().root_hash().unwrap().is_some(),
            "the first 32 leaves were repaired privately"
        );
        assert_eq!(root(&db), None);
        if corrupt {
            assert_eq!(
                receiver.process(&nodes_response(0, 0x8000, &[]), &mut Vec::new()),
                Err(Error::Protocol)
            );
            assert!(receiver.finish().is_err());
        } else {
            drop(receiver);
        }
        assert_eq!(root(&db), None);
        assert!(contents(&db).is_empty());
        assert_eq!(db.clock(), before);
        db.begin_write(T0).unwrap().commit().unwrap();
    }
}

#[test]
fn empty_source_resolves_local_only_keys_and_preserves_atomicity() {
    let (source, target) = (mk_db(), mk_db());
    let mut tx = target.begin_write(T0).unwrap();
    for n in 0..65u32 {
        tx.put(&n.to_be_bytes(), b"offline").unwrap();
    }
    tx.local_indexes().put(b"recovery", b"retained").unwrap();
    tx.commit().unwrap();
    let before = root(&target);
    let pinned = target.begin_read().unwrap();
    let kept = pump_with(&target, &source, Lww, 1024, 1).unwrap();
    assert_eq!(kept.end, SyncEnd::RemoteEmpty);
    assert_eq!(kept.deltas, 65);
    assert!(!kept.committed);
    assert_eq!(root(&target), before);
    let count = core::cell::Cell::new(0);
    let failing = |delta: &SyncDelta<'_>| {
        assert!(matches!(delta.remote, SyncEntState::Missing));
        count.set(count.get() + 1);
        if count.get() == 40 { Err(Error::Protocol) } else { Ok(Resolution::UseRemote) }
    };
    assert_eq!(pump_with(&target, &source, failing, 1024, 1), Err(Error::Protocol));
    assert_eq!(root(&target), before);
    let removed =
        pump_with(&target, &source, |_: &SyncDelta<'_>| Ok(Resolution::UseRemote), 1024, 1)
            .unwrap();
    assert_eq!(removed.applied, 65);
    assert_eq!(root(&target), None);
    assert_eq!(pinned.get(&0u32.to_be_bytes()).unwrap().unwrap().as_ref(), b"offline");
    assert_eq!(
        target.begin_read().unwrap().local_get(b"recovery").unwrap().unwrap().as_ref(),
        b"retained"
    );
}

#[test]
fn full_receipt_mirror_forgets_many_keys_without_changing_resolver_order_or_snapshot() {
    let (source, target) = (mk_db(), mk_db());
    let mut old = target.begin_write(T0).unwrap();
    let old_clock = old.clock();
    let mut new = source.begin_write(T0 + 1).unwrap();
    for n in 0..1000u32 {
        old.put(&n.to_be_bytes(), b"old").unwrap();
        if n % 2 != 0 {
            new.put(&n.to_be_bytes(), b"new").unwrap();
        }
    }
    old.commit().unwrap();
    new.commit().unwrap();
    let mut seen = Vec::new();
    let outcome = pump_with(
        &target,
        &source,
        |delta: &SyncDelta<'_>| {
            let n = u32::from_be_bytes(delta.key.try_into().unwrap());
            seen.push(n);
            let SyncEntState::Present { value, clock } = delta.local else {
                panic!("initial local entry must remain visible to resolver")
            };
            assert_eq!(clock, old_clock);
            let mut bytes = [0; 3];
            value.read_exact_at(0, &mut bytes).unwrap();
            assert_eq!(&bytes, b"old");
            assert_eq!(matches!(delta.remote, SyncEntState::Missing), n % 2 == 0);
            Ok(Resolution::UseRemote)
        },
        DEFAULT_MSG_CAP,
        19,
    )
    .unwrap();
    assert_eq!(outcome.applied, 1000);
    assert_eq!(seen, (0..1000u32).collect::<Vec<_>>());
    assert_eq!(contents(&target), contents(&source));
    assert_eq!(root(&target), root(&source));
}

#[test]
fn captured_full_receipt_forgets_local_entries_and_requires_pull() {
    let (source, target) = (mk_db(), mk_db());
    let mut old = target.begin_write(T0).unwrap();
    for n in 0..64u32 {
        old.put(&n.to_be_bytes(), b"local-only").unwrap();
    }
    old.commit().unwrap();
    put_all(&source, T0 + 1, &[(b"remote", b"retained")]);
    let before = root(&target);
    let mut request = Vec::new();
    let mut receiver = SyncReceiver::new_with_delta(
        &target,
        |_: &SyncDelta<'_>| Ok(Resolution::UseRemote),
        TestStaging::default(),
        T0 + 1,
        4096,
        &mut request,
    )
    .unwrap();
    exchange(&mut receiver, &source, request, 17).unwrap();
    assert_eq!(receiver.applied, 65);
    assert_eq!(root(&target), before, "forgetting remains private until the single commit");
    let (outcome, delta) = receiver.finish_with_delta().unwrap();
    assert!(outcome.committed);
    assert!(matches!(delta, Some(CommittedDelta::PullRequired)));
    assert_eq!(contents(&target), contents(&source));
    assert_eq!(root(&target), root(&source));
}

#[test]
fn full_receipt_resolver_error_after_a_flush_discards_all_received_changes() {
    let db = mk_db();
    let clock_before = db.clock();
    let mut receiver = SyncReceiver::new(
        &db,
        |delta: &SyncDelta<'_>| {
            if delta.key == 33u32.to_be_bytes() {
                Err(Error::Storage(0xfeed))
            } else {
                Ok(Resolution::UseRemote)
            }
        },
        TestStaging::default(),
        T0,
        &mut Vec::new(),
    )
    .unwrap();
    receiver
        .process(&pull_response(S_DIFF, 1, [9; 32], Clock { time: 1, count: 0 }), &mut Vec::new())
        .unwrap();
    for n in 0..34u32 {
        let key = n.to_be_bytes();
        let mut value = Vec::new();
        let hash = crate::hash::entry_hash(&HASH_KEY, &key, 0, b"new");
        encoding::encode_ent_val(&mut value, &hash, 0, Clock { time: n + 2, count: 0 }, b"new");
        let result =
            receiver.process(&nodes_response(0, F_MORE, &[(&key, &value)]), &mut Vec::new());
        if n == 33 {
            assert_eq!(result, Err(Error::Storage(0xfeed)));
            assert!(receiver.wtxn.as_ref().unwrap().root_hash().unwrap().is_some());
        } else {
            result.unwrap();
        }
    }
    assert!(receiver.finish_with_delta().is_err());
    assert_eq!(root(&db), None);
    assert_eq!(db.clock(), clock_before);
    assert!(contents(&db).is_empty());
    db.begin_write(T0).unwrap().commit().unwrap();
}

#[test]
fn full_receipt_dirty_bounds_have_a_fixed_three_kib_allowance() {
    // Four retained user-key buffers plus one temporary replacement;
    // the struct includes every Vec/Option header, counter and flag.
    // Interval lookup's three keys run only after old bounds were cleared.
    let bytes = 5 * crate::KEY_SIZE_MAX + core::mem::size_of::<PendingRepair>();
    std::println!(
        "receiver dirty bounds: {bytes} bytes; struct: {} bytes",
        core::mem::size_of::<PendingRepair>()
    );
    assert!(bytes <= 3 * 1024);
}

#[test]
fn streamed_lww_compares_past_first_fragment_and_propagates_read_errors() {
    use crate::storage::{SliceValue, ValueRead};
    let clock = Clock { time: 1, count: 0 };
    let a = vec![3; 12000];
    let mut b = a.clone();
    b[11000] = 4;
    let a = SliceValue(&a);
    let b = SliceValue(&b);
    let delta = SyncDelta {
        key: b"k",
        local: SyncEntState::Present { value: &a, clock },
        remote: SyncEntState::Present { value: &b, clock },
    };
    assert_eq!(SyncResolver::resolve(&mut Lww, &delta), Ok(Resolution::UseRemote));
    struct Failed;
    impl ValueRead for Failed {
        fn len(&self) -> usize {
            12000
        }
        fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize> {
            Err(Error::Storage(44))
        }
    }
    let delta = SyncDelta { remote: SyncEntState::Present { value: &Failed, clock }, ..delta };
    assert_eq!(SyncResolver::resolve(&mut Lww, &delta), Err(Error::Storage(44)));
}
#[test]
fn sender_receipt_and_transfer_keep_the_snapshot_when_source_changes() {
    let source = mk_db();
    let target = mk_db();
    put_all(&source, T0, &[(b"key", b"before")]);
    let mut request = Vec::new();
    let mut receiver =
        SyncReceiver::new(&target, Lww, TestStaging::default(), T0, &mut request).unwrap();
    let mut sender = SyncSender::new(&source).unwrap();
    let receipt = sender.root_hash().unwrap();
    // A retained sender must neither block a source writer nor acknowledge
    // a later tree which this transfer never supplied to its recipient.
    put_all(&source, T0 + 1, &[(b"key", b"after"), (b"new", b"later")]);
    assert_ne!(receipt, source.begin_read().unwrap().root_hash().unwrap());
    let mut buffer = [0; 113];
    let mut done = false;
    while !done {
        sender.process(&core::mem::take(&mut request)).unwrap();
        loop {
            let count = sender.write_output(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            done = receiver.process(&buffer[..count], &mut request).unwrap() == SyncStatus::Done;
        }
    }
    sender.process(&request).unwrap();
    receiver.finish().unwrap();
    assert_eq!(receipt, sender.root_hash().unwrap());
    assert_eq!(receipt, target.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(target.begin_read().unwrap().get(b"key").unwrap().unwrap().as_ref(), b"before");
    assert!(target.begin_read().unwrap().get(b"new").unwrap().is_none());
}

#[test]
fn streamed_large_frames_keep_only_fixed_parser_scratch_across_arbitrary_chunks() {
    let a = mk_db();
    let b = mk_db();
    let value = vec![23; DEFAULT_MSG_CAP + 137];
    put_all(&a, T0, &[(b"large", &value), (b"small", b"end")]);
    let mut request = Vec::new();
    let mut rx = SyncReceiver::new(&b, Lww, TestStaging::default(), T0, &mut request).unwrap();
    let mut tx = SyncSender::new(&a).unwrap();
    let mut buffer = [0; 8191];
    let mut done = false;
    while !done {
        tx.process(&core::mem::take(&mut request)).unwrap();
        loop {
            let n = tx.write_output(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            for chunk in buffer[..n].chunks(113) {
                done = rx.process(chunk, &mut request).unwrap() == SyncStatus::Done;
                assert!(rx.frame.key.capacity() <= 1024);
                assert!(rx.frame.control.capacity() <= 64);
                assert!(tx.output.capacity() <= 1024);
                assert!(rx.pending.len() <= 256);
            }
        }
    }
    tx.process(&request).unwrap();
    assert_eq!(rx.finish().unwrap().applied, 2);
    assert_eq!(root(&a), root(&b));
}
#[test]
fn receiver_preserves_multiframe_results_at_every_split() {
    let clock = Clock { time: 2, count: 0 };
    let entries = [(b"a".as_slice(), b"first".as_slice()), (b"b", b""), (b"c", b"last")];
    let stored: Vec<Vec<u8>> = entries
        .iter()
        .map(|(key, value)| {
            let mut bytes = Vec::new();
            let hash = crate::hash::entry_hash(&HASH_KEY, key, 0, value);
            encoding::encode_ent_val(&mut bytes, &hash, 0, clock, value);
            bytes
        })
        .collect();
    for corrupt in [false, true] {
        let mut bytes = pull_response(S_DIFF, 1, [9; 32], clock);
        bytes.extend(nodes_response(0, F_MORE, &[(b"a", &stored[0]), (b"b", &stored[1])]));
        bytes.extend(nodes_response(0, if corrupt { 0x8000 } else { 0 }, &[(b"c", &stored[2])]));
        let mut expected = None;
        for split in 0..=bytes.len() {
            let db = mk_db();
            let before_clock = db.clock();
            let mut rx = crafted_receiver(&db);
            let mut out = Vec::new();
            let result = rx
                .process(&bytes[..split], &mut out)
                .and_then(|_| rx.process(&[], &mut out))
                .and_then(|_| rx.process(&bytes[split..], &mut out));
            assert!(contents(&db).is_empty(), "changes stay private until finish");
            if corrupt {
                assert_eq!(result, Err(Error::Protocol), "split {split}");
                assert_eq!(rx.process(&[], &mut out), Err(Error::Protocol));
                assert!(rx.finish().is_err());
                assert!(contents(&db).is_empty());
                assert_eq!(db.clock(), before_clock);
                db.begin_write(T0).unwrap().commit().unwrap();
            } else {
                assert_eq!(result, Ok(SyncStatus::Done), "split {split}");
                let outcome = rx.finish().unwrap();
                assert_eq!(outcome.applied, 3);
                assert!(outcome.committed);
                assert_eq!(
                    contents(&db),
                    entries.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect::<Vec<_>>()
                );
                let observed = (root(&db), db.clock(), out);
                if let Some(expected) = &expected {
                    assert_eq!(&observed, expected, "split {split}");
                } else {
                    expected = Some(observed);
                }
            }
        }
    }
}
#[test]
fn streamed_applied_entry_rolls_back_after_later_bad_frame_or_cancel() {
    let key = b"a";
    let clock = Clock { time: 2, count: 0 };
    let mut stored = Vec::new();
    encoding::encode_ent_val(
        &mut stored,
        &crate::hash::entry_hash(&HASH_KEY, key, 0, b"new"),
        0,
        clock,
        b"new",
    );
    for corrupt in [false, true] {
        let db = mk_db();
        let before = db.clock();
        let mut rx = receiver_awaiting_nodes(&db, 1);
        let first = nodes_response(0, F_MORE, &[(key, &stored)]);
        for byte in &first {
            rx.process(core::slice::from_ref(byte), &mut Vec::new()).unwrap();
        }
        assert_eq!(rx.applied, 1);
        assert!(!db.begin_read().unwrap().has(key).unwrap());
        if corrupt {
            let bad = nodes_response(0, 0x8000, &[]);
            assert_eq!(rx.process(&bad, &mut Vec::new()), Err(Error::Protocol));
            assert_eq!(rx.process(&[], &mut Vec::new()), Err(Error::Protocol));
            assert!(rx.finish().is_err());
        } else {
            drop(rx);
        }
        assert_eq!(db.clock(), before);
        assert!(!db.begin_read().unwrap().has(key).unwrap());
        db.begin_write(T0).unwrap().commit().unwrap();
    }
}
#[test]
fn delta_visitor_borrows_original_values_and_preserves_tombstone_clocks() {
    let source = mk_db();
    pushed_put(&source, T0, b"deleted", b"old");
    let mut tx = source.begin_write_with_delta(T0 + 1, 4096).unwrap();
    tx.del(b"deleted").unwrap();
    tx.put(b"present", b"value").unwrap();
    let expected_clock = tx.clock();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    let mut seen = Vec::new();
    visit_delta(&bytes, 4096, |key, state| {
        assert!(
            key.as_ptr() >= bytes.as_ptr()
                && key.as_ptr() < bytes.as_ptr().wrapping_add(bytes.len())
        );
        if let EntState::Present { value, .. } = state {
            assert!(
                value.as_ptr() >= bytes.as_ptr()
                    && value.as_ptr() < bytes.as_ptr().wrapping_add(bytes.len())
            );
        }
        seen.push((key, state));
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![
            (b"deleted".as_slice(), EntState::Deleted { clock: expected_clock }),
            (b"present".as_slice(), EntState::Present { value: b"value", clock: expected_clock }),
        ]
    );
}

#[test]
fn delta_visitor_checks_complete_wire_even_when_caller_ignores_entries() {
    let source = mk_db();
    let bytes = pushed_put(&source, T0, b"key", b"value");
    for end in 0..bytes.len() {
        assert!(visit_delta(&bytes[..end], 4096, |_, _| {}).is_err());
    }
    assert!(
        visit_delta(&bytes, bytes.len() - 1, |_, _| panic!("oversize must fail before visiting"))
            .is_err()
    );
    let mut extra = bytes.clone();
    extra.push(0);
    let len = extra.len() as u32;
    extra[..4].copy_from_slice(&len.to_le_bytes());
    assert!(visit_delta(&extra, 4096, |_, _| {}).is_err());
    let mut malformed = bytes.clone();
    malformed[28..32].copy_from_slice(&2u32.to_le_bytes());
    assert!(visit_delta(&malformed, 4096, |_, _| {}).is_err());
    for offset in [4, 6, 8, 11] {
        let mut malformed = bytes.clone();
        malformed[offset] = 255;
        assert!(visit_delta(&malformed, 4096, |_, _| {}).is_err());
    }
}

#[test]
fn combined_deltas_match_sequential_lww_and_preserve_tombstones() {
    let source = mk_db();
    let mut tx = source.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"a", b"old").unwrap();
    tx.put(b"b", b"delete me").unwrap();
    tx.put(b"c", b"retained").unwrap();
    let CommittedDelta::Bytes(first) = tx.commit_with_delta().unwrap() else { panic!() };
    let mut tx = source.begin_write_with_delta(T0 + 1, 4096).unwrap();
    tx.put(b"a", b"new").unwrap();
    tx.del(b"b").unwrap();
    tx.put(b"d", b"added").unwrap();
    let CommittedDelta::Bytes(second) = tx.commit_with_delta().unwrap() else { panic!() };
    let combined = combine_deltas(&first, &second, 4096).unwrap();
    assert_eq!(combined, combine_deltas(&second, &first, 4096).unwrap());
    assert_eq!(combined, combine_deltas(&combined, &first, 4096).unwrap());
    assert_eq!(&combined[20..28], &second[20..28]);
    let sequential = mk_db();
    apply_push(&sequential, &first, Lww);
    apply_push(&sequential, &second, Lww);
    let target = mk_db();
    apply_push(&target, &combined, Lww);
    assert_eq!(root(&target), root(&sequential));
    assert_eq!(root(&target), root(&source));
    assert!(!apply_push(&target, &first, Lww).committed);
    assert!(target.begin_read().unwrap().get(b"b").unwrap().is_none());
    let empty = project_delta(&first, 4096, |_| false).unwrap();
    assert_eq!(combine_deltas(&empty, &first, 4096).unwrap(), first);
    assert_eq!(combine_deltas(&empty, &empty, 4096).unwrap(), empty);
}

#[test]
fn combined_deltas_use_the_same_equal_clock_tiebreak_in_either_order() {
    let a = mk_db();
    let b = mk_db();
    let lower = pushed_put(&a, T0, b"key", b"a");
    let higher = pushed_put(&b, T0, b"key", b"z");
    assert_eq!(&lower[20..28], &higher[20..28]);
    assert_eq!(combine_deltas(&lower, &higher, 4096).unwrap(), higher);
    assert_eq!(combine_deltas(&higher, &lower, 4096).unwrap(), higher);
    let mut tx = a.begin_write_with_delta(T0 + 1, 4096).unwrap();
    tx.del(b"key").unwrap();
    let CommittedDelta::Bytes(deleted) = tx.commit_with_delta().unwrap() else { panic!() };
    let c = mk_db();
    let present = pushed_put(&c, T0 + 1, b"key", b"present");
    assert_eq!(&deleted[20..28], &present[20..28]);
    assert_eq!(combine_deltas(&deleted, &present, 4096).unwrap(), present);
    assert_eq!(combine_deltas(&present, &deleted, 4096).unwrap(), present);
}

#[test]
fn combined_deltas_reject_invalid_superseded_entries_identity_and_union_overflow() {
    let source = mk_db();
    let first = pushed_put(&source, T0, b"a", b"old");
    let second = pushed_put(&source, T0 + 1, b"a", b"new");
    for end in 0..first.len() {
        assert!(combine_deltas(&first[..end], &second, 4096).is_err());
        assert!(combine_deltas(&second, &first[..end], 4096).is_err());
    }
    for (offset, value) in [(10, 8), (12, first[12] ^ 1), (6, 1), (8, 2), (32, 0), (28, 2)] {
        let mut bad = first.clone();
        bad[offset] = value;
        assert!(combine_deltas(&bad, &second, 4096).is_err());
        assert!(combine_deltas(&second, &bad, 4096).is_err());
    }
    assert!(combine_deltas(&first, &second, first.len() - 1).is_err());
    let other = pushed_put(&source, T0 + 2, b"b", b"value");
    let cap = first.len().max(other.len());
    assert!(combine_deltas(&first, &other, cap).is_err());
    let combined = combine_deltas(&first, &other, 4096).unwrap();
    assert_eq!(combine_deltas(&first, &other, combined.len()).unwrap(), combined);
}

#[test]
fn delta_projection_preserves_identity_clocks_tombstones_and_empty_frames() {
    let source = mk_db();
    let target = mk_db();
    let initial = pushed_put(&source, T0, b"b", b"delete me");
    apply_push(&target, &initial, Lww);
    let mut tx = source.begin_write_with_delta(T0 + 1, 4096).unwrap();
    tx.put(b"a", b"visible").unwrap();
    tx.del(b"b").unwrap();
    tx.put(b"c", b"private").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    assert_eq!(project_delta(&bytes, 4096, |_| true).unwrap(), bytes);
    let projected = project_delta(&bytes, 4096, |key| key != b"c").unwrap();
    assert_eq!(&projected[4..28], &bytes[4..28]);
    assert_eq!(u32::from_le_bytes(projected[28..32].try_into().unwrap()), 2);
    apply_push(&target, &projected, Lww);
    let read = target.begin_read().unwrap();
    assert_eq!(&*read.get(b"a").unwrap().unwrap(), b"visible");
    assert!(read.get(b"b").unwrap().is_none());
    assert!(read.get(b"c").unwrap().is_none());
    drop(read);
    let before = root(&target);
    assert!(!apply_push(&target, &projected, Lww).committed);
    assert_eq!(root(&target), before);
    // A delayed older value must not resurrect the retained tombstone.
    apply_push(&target, &initial, Lww);
    assert!(target.begin_read().unwrap().get(b"b").unwrap().is_none());
    let empty = project_delta(&bytes, 4096, |_| false).unwrap();
    assert_eq!(empty.len(), DELTA_HEADER);
    assert_eq!(&empty[4..28], &bytes[4..28]);
    assert_eq!(&empty[28..], &[0; 4]);
    assert!(!apply_push(&target, &empty, Lww).committed);
}

#[test]
fn delta_projection_validates_hidden_entries_and_complete_framing() {
    let db = mk_db();
    let mut tx = db.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"a", &[7; 40]).unwrap();
    tx.put(b"b", &[8; 40]).unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    let first_stored = 32 + 6 + 1;
    let second = first_stored + u32::from_le_bytes(bytes[34..38].try_into().unwrap()) as usize;
    let second_stored = second + 6 + 1;
    let mut malformed = Vec::new();
    for (offset, value) in [
        (4, 0),
        (6, 1),
        (8, 2),
        (10, 0),
        (10, 129),
        (11, 1),
        (32, 0),
        (second + 6, b'a'),
        (second_stored + 32, 0x80),
        (second_stored + 32, ENT_FLAG_TOMBSTONE),
    ] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        malformed.push(bad);
    }
    for range in [20..28, second_stored + 33..second_stored + 41] {
        let mut bad = bytes.clone();
        bad[range].fill(0);
        malformed.push(bad);
    }
    let mut future = bytes.clone();
    future[second_stored + 33..second_stored + 41].fill(255);
    malformed.push(future);
    let mut count = bytes.clone();
    count[28..32].fill(0);
    malformed.push(count);
    let mut length = bytes.clone();
    length[second + 2..second + 6].fill(255);
    malformed.push(length);
    let mut trailing = bytes.clone();
    trailing.push(0);
    frame_end(&mut trailing, 0);
    malformed.push(trailing);
    let mut concatenated = bytes.clone();
    concatenated.extend_from_slice(&bytes);
    malformed.push(concatenated);
    for bad in malformed {
        assert!(project_delta(&bad, 4096, |_| false).is_err());
    }
    for end in 0..bytes.len() {
        assert!(project_delta(&bytes[..end], 4096, |_| false).is_err());
    }
    assert!(project_delta(&bytes, bytes.len() - 1, |_| panic!("oversize before policy")).is_err());
    for branch in [2, 128] {
        let mut valid = bytes.clone();
        valid[10] = branch;
        assert_eq!(project_delta(&valid, valid.len(), |_| true).unwrap(), valid);
    }
}

#[test]
fn delta_projection_enforces_the_concrete_64k_bound_without_extra_buffers() {
    const CAP: usize = 64 * 1024;
    let db = mk_db();
    let mut tx = db.begin_write_with_delta(T0, CAP).unwrap();
    tx.put(b"a", &vec![3; CAP - DELTA_HEADER - 6 - 1 - encoding::ENT_HEADER_FULL]).unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    assert_eq!(bytes.len(), CAP);
    let projected = project_delta(&bytes, CAP, |_| true).unwrap();
    assert_eq!(projected, bytes);
    assert!(projected.capacity() <= CAP);
    assert!(project_delta(&bytes, CAP - 1, |_| false).is_err());
    assert_eq!(project_delta(&bytes, CAP, |_| false).unwrap().len(), DELTA_HEADER);
}

fn apply_push<R: Resolver>(db: &Db<MemStorage>, bytes: &[u8], resolver: R) -> SyncOutcome {
    let mut rx = DeltaReceiver::new(db, resolver, T0, 4096).unwrap();
    for fragment in bytes.chunks(3) {
        rx.process(fragment).unwrap();
    }
    rx.finish().unwrap()
}

#[test]
fn pull_entry_cannot_hide_exhausted_clock_behind_smaller_header() {
    for count in [u32::MAX - 2, u32::MAX - 1, u32::MAX] {
        let db = mk_db();
        let before = db.clock();
        let mut rx = receiver_awaiting_nodes(&db, 1);
        let mut stored = Vec::new();
        let hash = crate::hash::entry_hash(&HASH_KEY, b"k", 0, b"value");
        encoding::encode_ent_val(&mut stored, &hash, 0, Clock { time: u32::MAX, count }, b"value");
        let message = nodes_response(0, 0, &[(b"k", &stored)]);
        assert_eq!(rx.process(&message, &mut Vec::new()), Err(Error::ClockExhausted));
        assert!(rx.finish().is_err());
        assert_eq!(db.clock(), before);
        assert_eq!(root(&db), None);
        db.begin_write(T0).unwrap().commit().unwrap();
    }
}

#[test]
fn pull_rejects_exhausting_remote_clocks_without_poisoning_local_writes() {
    for count in [u32::MAX - 2, u32::MAX - 1, u32::MAX] {
        let db = mk_db();
        put_all(&db, T0, &[(b"safe", b"local")]);
        let before = db.clock();
        let mut rx = crafted_receiver(&db);
        let msg = pull_response(S_DIFF, 1, [9; 32], Clock { time: u32::MAX, count });
        assert_eq!(rx.process(&msg, &mut Vec::new()), Err(Error::ClockExhausted));
        assert!(rx.finish().is_err());
        assert_eq!(db.clock(), before);
        let mut tx = db.begin_write(T0 + 1).unwrap();
        tx.put(b"still", b"writable").unwrap();
        tx.commit().unwrap();
    }
}

#[test]
fn pushed_exhausting_clocks_roll_back_empty_and_nonempty_messages() {
    let a = mk_db();
    let valid = pushed_put(&a, T0, b"k", b"value");
    for count in [u32::MAX - 2, u32::MAX - 1, u32::MAX] {
        for empty in [false, true] {
            let db = mk_db();
            let before = db.clock();
            let mut bytes = valid.clone();
            bytes[20..28].copy_from_slice(&Clock { time: u32::MAX, count }.encode());
            if empty {
                bytes.truncate(DELTA_HEADER);
                bytes[..4].copy_from_slice(&(DELTA_HEADER as u32).to_le_bytes());
                bytes[28..32].fill(0);
            }
            let mut rx = DeltaReceiver::new(&db, Lww, T0, 4096).unwrap();
            assert_eq!(rx.process(&bytes), Err(Error::ClockExhausted));
            assert!(rx.finish().is_err());
            assert_eq!(db.clock(), before);
            assert_eq!(root(&db), None);
            let mut tx = db.begin_write(T0 + 1).unwrap();
            tx.put(b"still", b"writable").unwrap();
            tx.commit().unwrap();
        }
    }
}

#[test]
fn received_last_admissible_clock_allows_next_local_commit() {
    let source = mk_db();
    let mut tx = source.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"k", b"value").unwrap();
    tx.set_clock(Clock { time: u32::MAX, count: u32::MAX - 3 }).unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else {
        panic!("small transaction requested pull");
    };
    for push in [false, true] {
        let db = mk_db();
        if push {
            apply_push(&db, &bytes, Lww);
        } else {
            pump(&db, &source);
        }
        assert_eq!(root(&db), root(&source));
        assert_eq!(db.clock(), Clock { time: u32::MAX, count: u32::MAX - 2 });
        let mut tx = db.begin_write(T0).unwrap();
        tx.put(b"next", b"local").unwrap();
        tx.commit().unwrap();
        assert_eq!(db.clock(), Clock { time: u32::MAX, count: u32::MAX - 1 });
        assert_eq!(
            db.begin_read().unwrap().get(b"next").unwrap().as_deref(),
            Some(b"local".as_slice())
        );
    }
}

#[test]
fn pull_preserves_newer_same_content_clock_against_intermediate_conflict() {
    for value in [b"A".to_vec(), vec![b'A'; 100]] {
        let (a, b, conflict) = (mk_db(), mk_db(), mk_db());
        put_all(&a, T0 + 10, &[(b"k", &value)]);
        pump(&b, &a);
        put_all(&a, T0 + 30, &[(b"k", &value)]);
        put_all(&conflict, T0 + 20, &[(b"k", b"B")]);
        assert_ne!(root(&a), root(&b));
        assert_eq!(pump(&b, &a).applied, 1);
        assert_eq!(pump(&b, &conflict).applied, 0);
        assert_eq!(root(&a), root(&b));
        assert_eq!(b.begin_read().unwrap().get(b"k").unwrap().as_deref(), Some(value.as_slice()));
    }
}

#[test]
fn newer_same_value_clock_survives_old_conflict_via_push_and_pull() {
    for value in [b"A".to_vec(), vec![b'A'; 100]] {
        let (source, old, pushed, pulled) = (mk_db(), mk_db(), mk_db(), mk_db());
        let first = pushed_put(&source, T0 + 10, b"k", &value);
        let intermediate = pushed_put(&old, T0 + 20, b"k", b"B");
        let latest = pushed_put(&source, T0 + 30, b"k", &value);
        apply_push(&pushed, &first, Lww);
        apply_push(&pulled, &first, Lww);
        assert_ne!(root(&source), root(&pushed), "one-leaf roots must include version clocks");
        assert_eq!(apply_push(&pushed, &latest, Lww).applied, 1);
        assert_eq!(pump(&pulled, &source).applied, 1);
        for db in [&pushed, &pulled] {
            assert!(!apply_push(db, &intermediate, Lww).committed);
            assert_eq!(
                db.begin_read().unwrap().get(b"k").unwrap().as_deref(),
                Some(value.as_slice())
            );
            assert_eq!(root(db), root(&source));
        }
        // Crossing compact/full storage layouts also preserves versions.
        let next_value = if value.len() == 1 { vec![b'C'; 100] } else { b"C".to_vec() };
        let next = pushed_put(&source, T0 + 40, b"k", &next_value);
        apply_push(&pushed, &next, Lww);
        pump(&pulled, &pushed);
        assert_eq!(root(&pulled), root(&source));
        assert!(!apply_push(&pulled, &latest, Lww).committed);
    }
}

#[test]
fn imported_origin_clock_survives_delta_capture_and_pull() {
    let (source, imported, pushed, pulled) = (mk_db(), mk_db(), mk_db(), mk_db());
    let original = pushed_put(&source, T0 + 100, b"k", b"signed state");
    let origin = source.clock();
    let mut tx = imported.begin_write_with_delta(T0, 4096).unwrap();
    tx.put_with_clock(b"k", b"signed state", origin).unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    assert_eq!(root(&source), root(&imported));
    apply_push(&pushed, &bytes, Lww);
    pump(&pulled, &imported);
    assert_eq!(root(&source), root(&pushed));
    assert_eq!(root(&source), root(&pulled));
    assert!(!apply_push(&pushed, &original, Lww).committed);
    let mut next = imported.begin_write(T0).unwrap();
    next.put(b"k", b"local update").unwrap();
    next.commit().unwrap();
    pump(&pushed, &imported);
    assert_eq!(contents(&pushed), contents(&imported));
}

#[test]
fn pull_and_push_merges_publish_only_committed_applied_entries() {
    let (a, b, c, d) = (mk_db(), mk_db(), mk_db(), mk_db());
    let original = pushed_put(&a, T0 + 10, b"k", b"value");
    let mut receiver = DeltaReceiver::new_with_delta(&b, Lww, T0 + 20, 4096).unwrap();
    receiver.process(&original).unwrap();
    let (_, Some(CommittedDelta::Bytes(relay))) = receiver.finish_with_delta().unwrap() else {
        panic!()
    };
    apply_push(&c, &relay, Lww);
    assert_eq!(root(&a), root(&c));
    let mut receiver = DeltaReceiver::new_with_delta(&b, Lww, T0 + 30, 4096).unwrap();
    receiver.process(&original).unwrap();
    assert!(receiver.finish_with_delta().unwrap().1.is_none());
    let mut output = Vec::new();
    let mut receiver =
        SyncReceiver::new_with_delta(&d, Lww, TestStaging::default(), T0 + 40, 4096, &mut output)
            .unwrap();
    let mut sender = SyncSender::new(&a).unwrap();
    loop {
        let request = core::mem::take(&mut output);
        let mut response = Vec::new();
        drain_sender(&mut sender, &request, &mut response).unwrap();
        if receiver.process(&response, &mut output).unwrap() == SyncStatus::Done {
            break;
        }
    }
    let (_, Some(CommittedDelta::Bytes(relay))) = receiver.finish_with_delta().unwrap() else {
        panic!()
    };
    let e = mk_db();
    apply_push(&e, &relay, Lww);
    assert_eq!(root(&a), root(&e));
    assert_eq!(e.begin_read().unwrap().get(b"k").unwrap().as_deref(), Some(&b"value"[..]));
}

#[test]
fn push_fragmentation_duplicates_reordering_and_pull_interoperate() {
    let (a, b, c) = (mk_db(), mk_db(), mk_db());
    let first = pushed_put(&a, T0, b"k", b"first");
    let last = pushed_put(&a, T0 + 20, b"k", b"last");
    apply_push(&b, &last, Lww);
    assert!(!apply_push(&b, &first, Lww).committed);
    assert!(!apply_push(&b, &last, Lww).committed);
    pump(&c, &b);
    assert_eq!(root(&a), root(&b));
    assert_eq!(root(&b), root(&c));
    assert_eq!(c.begin_read().unwrap().get(b"k").unwrap().as_deref(), Some(&b"last"[..]));
    let r = b.begin_read().unwrap();
    let stored = r.raw().get(b"\0k").unwrap().unwrap();
    assert_eq!(EntVal::decode(stored).unwrap().clock, a.clock());
}

#[test]
fn push_tombstones_custom_resolver_and_missing_keys() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&b, T0, &[(b"local-only", b"keep")]);
    let first = pushed_put(&a, T0, b"k", b"value");
    apply_push(&b, &first, Lww);
    let mut tx = a.begin_write_with_delta(T0 + 20, 4096).unwrap();
    tx.del(b"k").unwrap();
    let CommittedDelta::Bytes(deleted) = tx.commit_with_delta().unwrap() else { panic!() };
    assert!(!apply_push(&b, &deleted, |_: &Delta<'_>| Resolution::KeepLocal).committed);
    assert!(b.begin_read().unwrap().has(b"k").unwrap());
    apply_push(&b, &deleted, |d: &Delta<'_>| {
        assert_eq!(d.key, b"k", "missing delta keys are not deletions");
        Resolution::UseRemote
    });
    assert_eq!(
        a.begin_read().unwrap().is_del(b"k").unwrap(),
        b.begin_read().unwrap().is_del(b"k").unwrap()
    );
    assert!(b.begin_read().unwrap().has(b"local-only").unwrap());
    apply_push(&b, &first, Lww);
    assert!(!b.begin_read().unwrap().has(b"k").unwrap());
}

#[test]
fn push_incomplete_or_malformed_input_rolls_back_and_poison_is_sticky() {
    let a = mk_db();
    let mut tx = a.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"a", b"one").unwrap();
    tx.put(b"b", b"two").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    for length in 0..bytes.len() {
        let b = mk_db();
        let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
        assert_eq!(rx.process(&bytes[..length]).unwrap(), SyncStatus::Running);
        assert!(rx.finish().is_err());
        assert_eq!(root(&b), None);
    }
    // The first entry merges privately before the malformed second entry
    // fails. Neither the merge nor the transaction clock may escape.
    let mut malformed = bytes.clone();
    let last = malformed.len() - 1;
    malformed.truncate(last);
    malformed[..4].copy_from_slice(&(last as u32).to_le_bytes());
    let b = mk_db();
    let before = b.clock();
    let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
    assert!(rx.process(&malformed).is_err());
    assert!(rx.process(&bytes).is_err());
    assert!(rx.finish().is_err());
    assert_eq!(b.clock(), before);
    assert_eq!(root(&b), None);
    // A complete message followed by trailing data must not commit either.
    let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
    assert_eq!(rx.process(&bytes).unwrap(), SyncStatus::Done);
    assert!(rx.process(&[0]).is_err());
    assert!(rx.finish().is_err());
    assert_eq!(root(&b), None);
}

#[test]
fn push_bounds_configuration_and_commit_boundary() {
    let a = mk_db();
    let mut tx = a.begin_write_with_delta(T0, 48).unwrap();
    tx.put(b"k", &[1; 64]).unwrap();
    assert!(matches!(tx.commit_with_delta().unwrap(), CommittedDelta::PullRequired));
    assert!(a.begin_read().unwrap().has(b"k").unwrap());
    let bytes = pushed_put(&a, T0 + 1, b"k", b"smaller");
    let b = mk_db();
    let mut rx = DeltaReceiver::new(&b, Lww, T0, bytes.len() - 1).unwrap();
    assert!(rx.process(&bytes[..4]).is_err());
    assert!(rx.finish().is_err());
    for offset in [4, 6, 8, 10, 12] {
        let mut bad = bytes.clone();
        bad[offset] ^= 0x80;
        let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
        assert!(rx.process(&bad).is_err(), "accepted corruption at {offset}");
        assert!(rx.finish().is_err());
    }
    let mut tx = a.begin_write_with_delta(T0 + 2, 4096).unwrap();
    tx.del_forget(b"k").unwrap();
    assert!(matches!(tx.commit_with_delta().unwrap(), CommittedDelta::PullRequired));
    let mut tx = a.begin_write_with_delta(T0 + 3, 4096).unwrap();
    tx.put(b"aborted", b"hidden").unwrap();
    drop(tx);
    assert!(!a.begin_read().unwrap().has(b"aborted").unwrap());
}

#[test]
fn captured_push_replaces_entries_and_keeps_pull_required_sticky() {
    let a = mk_db();
    let mut tx = a.begin_write_with_delta(T0, 160).unwrap();
    // Replacing the same entry must subtract its previous captured size.
    // Neither the old value nor its size may survive in the publication.
    tx.put(b"k", &[1; 64]).unwrap();
    tx.put(b"k", b"latest").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else {
        panic!("replacement fits the capture budget")
    };
    let b = mk_db();
    assert_eq!(apply_push(&b, &bytes, Lww).applied, 1);
    assert_eq!(contents(&b), vec![(b"k".to_vec(), b"latest".to_vec())]);
    assert_eq!(root(&a), root(&b));

    let mut tx = a.begin_write_with_delta(T0 + 1, 48).unwrap();
    tx.put(b"k", &[2; 64]).unwrap();
    tx.put(b"k", b"small again").unwrap();
    tx.put(b"other", b"also committed").unwrap();
    assert!(matches!(tx.commit_with_delta().unwrap(), CommittedDelta::PullRequired));
    pump(&b, &a);
    assert_eq!(root(&a), root(&b));
    assert_eq!(contents(&a), contents(&b));
}

#[test]
fn completed_receivers_expose_pending_values_and_allow_validation_rollback() {
    let source = mk_db();
    let target = mk_db();
    let mut tx = source.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"new", b"pending").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    let before = (target.clock(), root(&target));
    let mut delta = DeltaReceiver::new(&target, Lww, T0 + 1, 4096).unwrap();
    assert!(matches!(delta.pending_transaction(), Err(Error::Protocol)));
    assert_eq!(delta.process(&bytes).unwrap(), SyncStatus::Done);
    let tx = delta.pending_transaction().unwrap();
    assert_eq!(tx.get(b"new").unwrap().unwrap().as_ref(), b"pending");
    assert_eq!(tx.root_hash().unwrap(), root(&source));
    drop(delta);
    assert_eq!((target.clock(), root(&target)), before);
    let mut output = Vec::new();
    let mut full =
        SyncReceiver::new(&target, Lww, TestStaging::default(), T0 + 1, &mut output).unwrap();
    assert!(matches!(full.pending_transaction(), Err(Error::Protocol)));
    exchange(&mut full, &source, output, 17).unwrap();
    assert_eq!(full.pending_transaction().unwrap().root_hash().unwrap(), root(&source));
    let mut entries = 0;
    full.pending_transaction()
        .unwrap()
        .visit(b"", false, |key, value| -> Result<bool> {
            assert_eq!(key, b"new");
            assert_eq!(value.len(), 7);
            entries += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(entries, 1);
    drop(full);
    assert_eq!((target.clock(), root(&target)), before);
    // A repeated validated reception can still commit the original delta.
    assert_eq!(apply_push(&target, &bytes, Lww).applied, 1);
    assert_eq!(root(&target), root(&source));
}

#[test]
fn unfinished_captured_receivers_rollback_and_release_the_writer() {
    let db = mk_db();
    let before = db.clock();
    let rx =
        SyncReceiver::new_with_delta(&db, Lww, TestStaging::default(), T0, 4096, &mut Vec::new())
            .unwrap();
    assert!(matches!(rx.finish_with_delta(), Err(Error::Protocol)));
    let rx = DeltaReceiver::new_with_delta(&db, Lww, T0, 4096).unwrap();
    assert!(matches!(rx.finish_with_delta(), Err(Error::Protocol)));
    assert_eq!(db.clock(), before);
    assert_eq!(root(&db), None);
    db.begin_write(T0).unwrap().commit().unwrap();
}

#[test]
fn push_rejects_invalid_clocks_keys_and_lengths_before_publication() {
    let source = mk_db();
    let mut tx = source.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"a", b"one").unwrap();
    tx.put(b"b", b"two").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else {
        panic!("small transaction fits")
    };
    let first_value_len = u32::from_le_bytes(bytes[34..38].try_into().unwrap()) as usize;
    let second_key = DELTA_HEADER + 6 + 1 + first_value_len + 6;
    let mut zero_clock = bytes.clone();
    zero_clock[20..28].fill(0);
    let mut future_entry = bytes.clone();
    // Both entries retain T0; advertise an older, nonzero transaction clock.
    future_entry[20..28].copy_from_slice(&Clock { time: 1, count: 0 }.encode());
    let mut empty_key = bytes.clone();
    empty_key[32..34].fill(0);
    let mut duplicate_key = bytes.clone();
    duplicate_key[second_key] = b'a';
    let mut trailing = bytes.clone();
    trailing.push(0);
    let len = trailing.len() as u32;
    trailing[..4].copy_from_slice(&len.to_le_bytes());
    for invalid in [zero_clock, future_entry, empty_key, duplicate_key, trailing] {
        let db = mk_db();
        let before = db.clock();
        let mut rx = DeltaReceiver::new_with_delta(&db, Lww, T0, 4096).unwrap();
        assert_eq!(rx.process(&invalid), Err(Error::Protocol));
        assert_eq!(rx.process(&bytes), Err(Error::Protocol));
        assert!(matches!(rx.finish_with_delta(), Err(Error::Protocol)));
        assert_eq!(db.clock(), before);
        assert_eq!(root(&db), None);
        // Rejection releases the writer and a fresh receiver can retry.
        assert_eq!(apply_push(&db, &bytes, Lww).applied, 2);
        assert_eq!(root(&db), root(&source));
    }
    let db = mk_db();
    let mut rx = DeltaReceiver::new(&db, Lww, T0, bytes.len() - 1).unwrap();
    assert_eq!(rx.process(&bytes), Err(Error::Protocol));
    assert!(rx.finish().is_err());
    let mut rx = DeltaReceiver::new(&db, Lww, T0, bytes.len()).unwrap();
    assert_eq!(rx.process(&bytes), Ok(SyncStatus::Done));
    assert_eq!(rx.process(&[]), Ok(SyncStatus::Done));
    assert_eq!(rx.finish().unwrap().applied, 2);
    assert_eq!(root(&db), root(&source));
}

#[test]
fn framer_partial_multi_and_compaction() {
    let mut f = Framer::default();
    let a = frame(T_BYE, 0, &[0]);
    let b = frame(T_BYE, 7, &[1, 2]);
    // partial delivery, one byte at a time
    for &byte in &a[..a.len() - 1] {
        f.push(&[byte]);
        assert!(f.next().unwrap().is_none());
    }
    f.push(&[a[a.len() - 1]]);
    assert_eq!(f.next().unwrap(), Some((T_BYE, 0, alloc::vec![0])));
    assert!(f.next().unwrap().is_none()); // buffer fully drained
    // two frames in one push
    let mut both = a.clone();
    both.extend_from_slice(&b);
    f.push(&both);
    assert_eq!(f.next().unwrap(), Some((T_BYE, 0, alloc::vec![0])));
    // push while a consumed frame is still buffered (compaction path)
    f.push(&a);
    assert_eq!(f.next().unwrap(), Some((T_BYE, 7, alloc::vec![1, 2])));
    assert_eq!(f.next().unwrap(), Some((T_BYE, 0, alloc::vec![0])));
    // undersized and oversized frame headers are rejected
    let mut f = Framer::default();
    f.push(&[4, 0, 0, 0, 0, 0, 0, 0]);
    assert!(f.next().is_err());
    let mut f = Framer::default();
    f.push(&(MAX_FRAME as u32 + 1).to_le_bytes());
    f.push(&[0; 4]);
    assert!(f.next().is_err());
}

#[test]
fn lww_resolver_direct() {
    let clock = Clock { time: 5, count: 0 };
    let older = Clock { time: 4, count: 9 };
    let mut lww = Lww;
    let cases: &[(EntState<'_>, EntState<'_>, Resolution)] = &[
        (EntState::Missing, EntState::Missing, Resolution::KeepLocal),
        (EntState::Missing, EntState::Deleted { clock }, Resolution::UseRemote),
        (EntState::Deleted { clock }, EntState::Missing, Resolution::KeepLocal),
        (
            EntState::Present { value: b"x", clock },
            EntState::Deleted { clock: older },
            Resolution::KeepLocal,
        ),
        (
            EntState::Present { value: b"x", clock: older },
            EntState::Deleted { clock },
            Resolution::UseRemote,
        ),
        // equal clocks: present beats deleted, larger bytes win
        (
            EntState::Deleted { clock },
            EntState::Present { value: b"x", clock },
            Resolution::UseRemote,
        ),
        (
            EntState::Present { value: b"a", clock },
            EntState::Present { value: b"b", clock },
            Resolution::UseRemote,
        ),
        (
            EntState::Present { value: b"b", clock },
            EntState::Present { value: b"a", clock },
            Resolution::KeepLocal,
        ),
    ];
    for (local, remote, want) in cases {
        let delta = Delta { key: b"k", local: *local, remote: *remote };
        assert_eq!(Resolver::resolve(&mut lww, &delta), *want, "{delta:?}");
    }
}

#[test]
fn receiver_rejects_malformed_pull_responses() {
    let mk = mk_db;
    // DIFF with an invalid root level
    let a = mk();
    let mut rx = crafted_receiver(&a);
    let msg = pull_response(S_DIFF, 0, [9; 32], Clock { time: 1, count: 0 });
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));
    // a failed session refuses further input and cannot finish
    assert!(matches!(rx.process(&[], &mut Vec::new()), Err(Error::Protocol)));
    assert!(rx.finish().is_err());

    // error statuses map to specific errors
    for (status, want) in [
        (S_ERR_VERSION, Error::Version),
        (S_ERR_HASHKEY, Error::HashKeyMismatch),
        (S_ERR_BRANCH_FACTOR, Error::BranchFactorMismatch),
        (0x7f, Error::Remote),
    ] {
        let db = mk();
        let mut rx = crafted_receiver(&db);
        let msg = pull_response(status, 1, [0; 32], Clock::ZERO);
        assert_eq!(rx.process(&msg, &mut Vec::new()).unwrap_err(), want);
    }

    // truncated payload
    let db = mk();
    let mut rx = crafted_receiver(&db);
    let msg = frame(T_PULL_RES, 0, &[S_DIFF, 1, 0]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));

    // unexpected frame type for the state
    let db = mk();
    let mut rx = crafted_receiver(&db);
    let msg = frame(T_NODES_RES, 0, &[0; 8]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));

    // finish before the session completed
    let db = mk();
    let rx = crafted_receiver(&db);
    assert!(matches!(rx.finish(), Err(Error::Protocol)));
}

/// Drive a crafted receiver into AwaitNodes for a root at `root_level`.
fn receiver_awaiting_nodes(
    db: &Db<MemStorage>,
    root_level: u8,
) -> SyncReceiver<'_, MemStorage, Lww, TestStaging> {
    let mut rx = crafted_receiver(db);
    let msg = pull_response(S_DIFF, root_level, [9; 32], Clock { time: 1, count: 0 });
    let mut out = Vec::new();
    rx.process(&msg, &mut out).unwrap();
    assert!(!out.is_empty(), "receiver should request the root's children");
    rx
}

#[test]
fn a_branch_span_cannot_exceed_pending_traversal_bound() {
    let db = mk_db();
    let mut rx = receiver_awaiting_nodes(&db, 2);
    for n in 0..256u32 {
        let key = format!("{n:08}");
        let frame = nodes_response(1, F_MORE, &[(key.as_bytes(), &[0; 32])]);
        rx.process(&frame, &mut Vec::new()).unwrap();
    }
    let frame = nodes_response(1, F_MORE, &[(b"99999999", &[0; 32])]);
    assert_eq!(rx.process(&frame, &mut Vec::new()), Err(Error::Capacity));
    assert!(rx.finish().is_err());
}

fn nodes_response(level: u8, flags: u16, entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(level);
    p.extend_from_slice(&[0; 3]);
    p.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (k, v) in entries {
        p.extend_from_slice(&(k.len() as u16).to_le_bytes());
        p.extend_from_slice(&(v.len() as u32).to_le_bytes());
        p.extend_from_slice(k);
        p.extend_from_slice(v);
    }
    frame(T_NODES_RES, flags, &p)
}

#[test]
fn receiver_rejects_malformed_nodes_responses() {
    let a = mk_db();
    // level mismatch
    let mut rx = receiver_awaiting_nodes(&a, 1);
    let msg = nodes_response(5, 0, &[(b"k", b"v")]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));

    // unordered keys
    let b = mk_db();
    let mut rx = receiver_awaiting_nodes(&b, 1);
    let ent = {
        let hash = crate::hash::entry_hash(&HASH_KEY, b"b", 0, b"v");
        let mut buf = Vec::new();
        crate::encoding::encode_ent_val(&mut buf, &hash, 0, Clock { time: 1, count: 0 }, b"v");
        buf
    };
    let msg = nodes_response(0, 0, &[(b"b", &ent), (b"a", &ent)]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));

    // empty child list for a branch level
    let c = mk_db();
    let mut rx = receiver_awaiting_nodes(&c, 2);
    let msg = nodes_response(1, 0, &[]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));

    // branch child whose value is not a 32-byte hash
    let d = mk_db();
    let mut rx = receiver_awaiting_nodes(&d, 2);
    let msg = nodes_response(1, 0, &[(b"", b"short")]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));

    // entry lengths overrunning the payload
    let e = mk_db();
    let mut rx = receiver_awaiting_nodes(&e, 1);
    let mut p = Vec::new();
    p.push(0u8);
    p.extend_from_slice(&[0; 3]);
    p.extend_from_slice(&1u32.to_le_bytes());
    p.extend_from_slice(&100u16.to_le_bytes()); // key_len far beyond payload
    p.extend_from_slice(&0u32.to_le_bytes());
    let msg = frame(T_NODES_RES, 0, &p);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));
}

#[test]
fn receiver_rejects_entries_this_replica_could_not_have_written() {
    fn entry(key: &[u8], userval: &[u8], clock: Clock) -> Vec<u8> {
        let hash = crate::hash::entry_hash(&HASH_KEY, key, 0, userval);
        let mut buf = Vec::new();
        crate::encoding::encode_ent_val(&mut buf, &hash, 0, clock, userval);
        buf
    }
    let real = Clock { time: 1, count: 0 };
    // A value past VALUE_SIZE_MAX still fits the peer's frame, but could
    // never be served onward in one frame from here.
    let oversized = entry(b"k", &vec![7u8; crate::VALUE_SIZE_MAX + 1], real);
    // The zero clock is the sentinel that ranks below every entry; an
    // entry carrying it would win against nothing and lose to anything.
    let unclocked = entry(b"k", b"v", Clock::ZERO);
    for stored in [oversized, unclocked] {
        let db = mk_db();
        let mut rx = receiver_awaiting_nodes(&db, 1);
        let msg = nodes_response(0, 0, &[(b"k", &stored)]);
        assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));
        assert!(db.begin_read().unwrap().get(b"k").unwrap().is_none(), "nothing was applied");
    }
    // The same limits hold when the key exists locally with other content.
    let db = mk_db();
    put_all(&db, T0, &[(b"k", b"local")]);
    let mut rx = receiver_awaiting_nodes(&db, 1);
    let msg =
        nodes_response(0, 0, &[(b"k", &entry(b"k", &vec![7u8; crate::VALUE_SIZE_MAX + 1], real))]);
    assert!(matches!(rx.process(&msg, &mut Vec::new()), Err(Error::Protocol)));
    // A maximum-size value is accepted as it is from a local write.
    let db = mk_db();
    let mut rx = receiver_awaiting_nodes(&db, 1);
    let msg =
        nodes_response(0, 0, &[(b"k", &entry(b"k", &vec![7u8; crate::VALUE_SIZE_MAX], real))]);
    assert!(rx.process(&msg, &mut Vec::new()).is_ok());
}

#[test]
fn sender_rejects_bad_requests() {
    // wrong protocol version in the handshake
    let a = mk_db();
    put_all(&a, T0, &[(b"x", b"1")]);
    let mut tx = SyncSender::new(&a).unwrap();
    let mut p = Vec::new();
    p.extend_from_slice(&99u16.to_le_bytes()); // version
    p.push(4); // branch factor
    p.push(0); // root level
    p.extend_from_slice(&[0; 8]); // fingerprint
    p.extend_from_slice(&[0; 32]); // root hash
    let msg = frame(T_PULL_REQ, 0, &p);
    let mut out = Vec::new();
    assert!(matches!(drain_sender(&mut tx, &msg, &mut out), Err(Error::Version)));
    assert!(!out.is_empty(), "sender reports the error status to the peer");

    // An incompatible discriminator wins even when no current-version body exists.
    let mut tx = SyncSender::new(&a).unwrap();
    let msg = frame(T_PULL_REQ, 0, &99u16.to_le_bytes());
    let mut out = Vec::new();
    assert!(matches!(drain_sender(&mut tx, &msg, &mut out), Err(Error::Version)));
    assert_eq!(out[8], S_ERR_VERSION);

    // branch-factor mismatch between real replicas
    let small = mk_db(); // branch factor 4
    put_all(&small, T0, &[(b"x", b"1")]);
    let mut opts = CreateOptions::syncable(HASH_KEY, 0);
    opts.branch_factor = 8;
    let other = Db::create(MemStorage::new(), &opts).unwrap();
    let err = pump_with(&other, &small, Lww, DEFAULT_MSG_CAP, usize::MAX).unwrap_err();
    assert_eq!(err, Error::BranchFactorMismatch, "not reported as a key mismatch");

    // a Serving sender rejects nonsense NodesRequests
    fn serving(db: &Db<MemStorage>) -> SyncSender<'_, MemStorage> {
        let mut tx = SyncSender::new(db).unwrap();
        // real handshake from a differing receiver
        let peer = mk_db();
        put_all(&peer, T0 + 5, &[(b"z", b"9")]);
        let mut req = Vec::new();
        let _rx = SyncReceiver::new(&peer, Lww, TestStaging::default(), T0, &mut req).unwrap();
        drain_sender(&mut tx, &req, &mut Vec::new()).unwrap();
        tx
    }
    let nodes_req = |level: u8, ukey: &[u8]| {
        let mut p = vec![1, 0, 0, 0];
        p.push(level);
        p.push(0);
        p.extend_from_slice(&(ukey.len() as u16).to_le_bytes());
        p.extend_from_slice(ukey);
        frame(T_NODES_REQ, 0, &p)
    };
    let db = mk_db();
    put_all(&db, T0, &[(b"b", b"1")]);
    // level 0 is not a branch level
    let mut tx = serving(&db);
    assert!(matches!(
        drain_sender(&mut tx, &nodes_req(0, b""), &mut Vec::new()),
        Err(Error::Protocol)
    ));
    // chunk head that does not exist ("a" sorts before the only leaf "b")
    let mut tx = serving(&db);
    assert!(matches!(
        drain_sender(&mut tx, &nodes_req(1, b"a"), &mut Vec::new()),
        Err(Error::Protocol)
    ));
    // empty level high above the root
    let mut tx = serving(&db);
    assert!(matches!(
        drain_sender(&mut tx, &nodes_req(9, b""), &mut Vec::new()),
        Err(Error::Protocol)
    ));
}

#[test]
fn mirror_resolver_forgets_local_only_entries() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&a, T0, &[(b"shared", b"s"), (b"local-only", b"x")]);
    put_all(&b, T0 + 1, &[(b"shared", b"s")]);
    // resolver that mirrors the remote exactly, deleting local extras
    let mirror = |_: &SyncDelta<'_>| Ok(Resolution::UseRemote);
    let o = pump_with(&a, &b, mirror, DEFAULT_MSG_CAP, usize::MAX).unwrap();
    assert_eq!(o.end, SyncEnd::Merged);
    assert_eq!(o.applied, 2); // shared clock updated and local-only removed
    assert_eq!(
        contents(&a),
        alloc::vec![(b"shared".to_vec(), b"s".to_vec())],
        "local-only entry must be forgotten"
    );
}

#[test]
fn mirror_resolver_covers_local_only_chunk_heads_beside_shared_subtrees() {
    // An equal branch hash covers its members, but its next local chunk head
    // may precede the remote span's end. Vary the content-defined boundaries
    // while retaining identical clocks/bytes for every shared leaf.
    for seed in 0..128u32 {
        // Keep the root above the snapshot threshold so this continues to
        // exercise hash/range skipping and tree discovery.
        let value = seed.to_be_bytes().repeat(SMALL_RANGE_BYTES / 16);
        let (source, target) = (mk_db(), mk_db());
        let mut tx = source.begin_write(T0).unwrap();
        for n in 0..16u32 {
            tx.put(&(2 * n).to_be_bytes(), &value).unwrap();
        }
        tx.commit().unwrap();
        pump(&target, &source);
        let mut tx = target.begin_write(T0 + 1).unwrap();
        for n in 0..16u32 {
            tx.put(&(2 * n + 1).to_be_bytes(), &value).unwrap();
        }
        tx.commit().unwrap();
        let pinned = target.begin_read().unwrap();
        let mut seen = Vec::new();
        let outcome = pump_with(
            &target,
            &source,
            |delta: &SyncDelta<'_>| {
                assert!(matches!(delta.remote, SyncEntState::Missing));
                seen.push(u32::from_be_bytes(delta.key.try_into().unwrap()));
                Ok(Resolution::UseRemote)
            },
            1024,
            19,
        )
        .unwrap();
        assert_eq!(seen, (0..16u32).map(|n| 2 * n + 1).collect::<Vec<_>>(), "boundary seed {seed}");
        assert_eq!(outcome.applied, 16);
        assert_eq!(contents(&target), contents(&source), "boundary seed {seed}");
        assert_eq!(root(&target), root(&source));
        assert_eq!(pinned.get(&0u32.to_be_bytes()).unwrap().unwrap().as_ref(), value);
        assert_eq!(pinned.get(&1u32.to_be_bytes()).unwrap().unwrap().as_ref(), value);
        assert!(!pump_with(&target, &source, Lww, 1024, 19).unwrap().committed);
        target.verify().unwrap();
    }
}

#[test]
fn no_difference_short_circuit() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&a, T0, &[(b"x", b"1"), (b"y", b"2")]);
    put_all(&b, T0, &[(b"y", b"2"), (b"x", b"1")]);
    let o = pump(&a, &b);
    assert_eq!(o.end, SyncEnd::NoDifference);
    assert_eq!(o.deltas, 0);
    assert!(!o.committed);
}

#[test]
fn identical_delta_visits_current_resolver_without_commit_or_repair() {
    let db = mk_db();
    let mut tx = db.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"entry", b"known").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    let clock = db.clock();
    let root = db.begin_read().unwrap().root_hash().unwrap();
    let visits = core::cell::Cell::new(0);
    let admit = |delta: &Delta<'_>| {
        visits.set(visits.get() + 1);
        assert_eq!(delta.key, b"entry");
        assert_eq!(delta.local, delta.remote);
        Resolution::KeepLocal
    };
    let mut receiver = DeltaReceiver::new(&db, admit, T0 + 1, 4096).unwrap();
    assert_eq!(receiver.process(&bytes).unwrap(), SyncStatus::Done);
    let outcome = receiver
        .repair_local_indexes::<Error>(|_| panic!("an exact replay cannot reach repair"))
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(visits.get(), 1);
    assert_eq!(outcome.deltas, 0);
    assert_eq!(outcome.applied, 0);
    assert!(!outcome.committed);
    assert_eq!(db.clock(), clock);
    assert_eq!(db.begin_read().unwrap().root_hash().unwrap(), root);
}

#[test]
fn remote_empty() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&a, T0, &[(b"x", b"1")]);
    let o = pump(&a, &b);
    assert_eq!(o.end, SyncEnd::RemoteEmpty);
    assert_eq!(contents(&a).len(), 1); // local data untouched
}

#[test]
fn pull_into_empty_copies_everything() {
    let (a, b) = (mk_db(), mk_db());
    let kvs: Vec<(Vec<u8>, Vec<u8>)> = (0..300u32)
        .map(|i| {
            (
                format!("key{i:04}").into_bytes(),
                // straddle the compact/full threshold
                vec![i as u8; (i % 70) as usize],
            )
        })
        .collect();
    {
        let mut w = b.begin_write(T0).unwrap();
        for (k, v) in &kvs {
            w.put(k, v).unwrap();
        }
        w.commit().unwrap();
    }
    let o = pump(&a, &b);
    assert_eq!(o.end, SyncEnd::Merged);
    assert_eq!(o.applied, 300);
    assert!(o.committed);
    assert_eq!(root(&a), root(&b));
    assert_eq!(contents(&a), contents(&b));
    // second pull: nothing to do
    assert_eq!(pump(&a, &b).end, SyncEnd::NoDifference);
}

#[test]
fn lww_and_tombstones_converge_bidirectionally() {
    let (a, b) = (mk_db(), mk_db());
    // shared history
    put_all(&a, T0, &[(b"shared", b"s"), (b"dead", b"d"), (b"conflict", b"old")]);
    pump(&b, &a);
    assert_eq!(root(&a), root(&b));
    // diverge: b updates "conflict" later than a; a deletes "dead";
    // each adds its own key
    put_all(&a, T0 + 10, &[(b"conflict", b"from-a"), (b"only-a", b"1")]);
    {
        let mut w = a.begin_write(T0 + 11).unwrap();
        w.del(b"dead").unwrap();
        w.commit().unwrap();
    }
    put_all(&b, T0 + 20, &[(b"conflict", b"from-b"), (b"only-b", b"2")]);

    let o1 = pump(&a, &b); // a pulls b
    assert_eq!(o1.end, SyncEnd::Merged);
    let o2 = pump(&b, &a); // b pulls a
    assert_eq!(o2.end, SyncEnd::Merged);

    assert_eq!(root(&a), root(&b), "replicas must converge");
    let want: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (b"conflict".to_vec(), b"from-b".to_vec()), // b wrote later
        (b"only-a".to_vec(), b"1".to_vec()),
        (b"only-b".to_vec(), b"2".to_vec()),
        (b"shared".to_vec(), b"s".to_vec()),
    ];
    assert_eq!(contents(&a), want);
    // the deletion propagated as a tombstone, with a's deletion clock
    let (ca, cb) = (
        a.begin_read().unwrap().is_del(b"dead").unwrap(),
        b.begin_read().unwrap().is_del(b"dead").unwrap(),
    );
    assert!(ca.is_some());
    assert_eq!(ca, cb, "tombstone clock must be preserved verbatim");
    // once converged: no further differences either way
    assert_eq!(pump(&a, &b).end, SyncEnd::NoDifference);
    assert_eq!(pump(&b, &a).end, SyncEnd::NoDifference);
}

#[test]
fn merged_entries_keep_origin_clocks() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&b, T0 + 100, &[(b"k", b"v")]);
    let origin_clock = b.clock();
    pump(&a, &b);
    // read a's stored entry directly and check the clock
    let r = a.begin_read().unwrap();
    let g = r.raw().get(b"\x00k").unwrap().unwrap();
    let ent = EntVal::decode(g).unwrap();
    assert_eq!(ent.clock, origin_clock);
}

#[test]
fn hashkey_mismatch_is_rejected() {
    let a = mk_db();
    let b = {
        let mut opts = CreateOptions::syncable([0x55; 32], 2);
        opts.branch_factor = 4;
        Db::create(MemStorage::new(), &opts).unwrap()
    };
    put_all(&b, T0, &[(b"x", b"1")]);
    let err = pump_with(&a, &b, Lww, DEFAULT_MSG_CAP, usize::MAX).unwrap_err();
    assert_eq!(err, Error::HashKeyMismatch);
}

#[test]
fn tiny_messages_and_fragmented_delivery() {
    let (a, b) = (mk_db(), mk_db());
    let mut w = b.begin_write(T0).unwrap();
    for i in 0..200u32 {
        w.put(format!("k{i:03}").as_bytes(), &vec![7u8; 40 + (i % 30) as usize]).unwrap();
    }
    w.commit().unwrap();
    // 1 KiB frames force MORE chains; 7-byte delivery exercises framing
    let o = pump_with(&a, &b, Lww, 1, 7).unwrap(); // cap clamps to 1024
    assert_eq!(o.end, SyncEnd::Merged);
    assert_eq!(o.applied, 200);
    assert_eq!(root(&a), root(&b));
}

#[test]
fn custom_resolver_keep_local() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&a, T0, &[(b"k", b"mine")]);
    put_all(&b, T0 + 50, &[(b"k", b"theirs")]);
    let keep_all = |_: &SyncDelta<'_>| Ok(Resolution::KeepLocal);
    let o = pump_with(&a, &b, keep_all, DEFAULT_MSG_CAP, usize::MAX).unwrap();
    assert_eq!(o.end, SyncEnd::Merged);
    assert_eq!(o.deltas, 1);
    assert_eq!(o.applied, 0);
    assert!(!o.committed);
    assert_eq!(contents(&a), vec![(b"k".to_vec(), b"mine".to_vec())]);
}

#[test]
fn equal_clock_ties_resolve_symmetrically() {
    // Force exactly equal clocks by writing with identical wall times
    // on replicas with identical clock histories.
    let (a, b) = (mk_db(), mk_db());
    put_all(&a, T0, &[(b"k", b"aaa")]);
    put_all(&b, T0, &[(b"k", b"zzz")]);
    assert_eq!(a.clock(), b.clock());
    pump(&a, &b);
    pump(&b, &a);
    assert_eq!(root(&a), root(&b), "tiebreak must converge");
    assert_eq!(contents(&a), vec![(b"k".to_vec(), b"zzz".to_vec())]); // larger bytes win
}

/// Every pair from this finite domain is checked through all four receive
/// paths against the specified clock/liveness/byte ordering. Roots and stored
/// entry bytes include tombstones and origin clocks, not just visible values.
#[test]
fn exhaustive_conflicts_agree_across_pull_push_combined_and_prepared_sync() {
    type State<'a> = Option<(u64, Option<&'a [u8]>)>;
    let states: [State<'_>; 9] = [
        None,
        Some((T0, None)),
        Some((T0 + 1, None)),
        Some((T0, Some(b""))),
        Some((T0, Some(b"a"))),
        Some((T0, Some(b"aa"))),
        Some((T0, Some(&[3; 30]))),
        Some((T0, Some(&[3; 31]))),
        Some((T0 + 1, Some(&[3; 31]))),
    ];
    let fixture = |state: State<'_>| {
        let db = mk_db();
        let mut tx = db.begin_write(T0 - 1).unwrap();
        tx.put(b"a", b"left neighbor").unwrap();
        tx.put(b"z", b"right neighbor").unwrap();
        if state.is_some() {
            tx.put(b"k", b"seed for deletion").unwrap();
        }
        tx.commit().unwrap();
        let mut tx = db.begin_write_with_delta(state.map_or(T0, |(wall, _)| wall), 4096).unwrap();
        if let Some((_, value)) = state {
            match value {
                Some(value) => tx.put(b"k", value).unwrap(),
                None => {
                    assert!(tx.del(b"k").unwrap());
                }
            }
        }
        let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else {
            panic!("fixture delta fits its capacity");
        };
        (db, delta)
    };
    fn rank(state: State<'_>) -> (u64, u8, &[u8]) {
        match state {
            None => (0, 0, &[]),
            Some((wall, None)) => (wall, 1, &[]),
            Some((wall, Some(value))) => (wall, 2, value),
        }
    }
    for (a_index, &a_state) in states.iter().enumerate() {
        for (b_index, &b_state) in states.iter().enumerate() {
            let winner = if rank(a_state) >= rank(b_state) { a_state } else { b_state };
            let (expected, _) = fixture(winner);
            let expected_entry =
                expected.begin_read().unwrap().raw().get(b"\0k").unwrap().map(|v| v.to_vec());
            let check = |db: &Db<MemStorage>| {
                db.verify().unwrap();
                assert_eq!(root(db), root(&expected), "states {a_index}/{b_index}");
                assert_eq!(contents(db), contents(&expected), "states {a_index}/{b_index}");
                assert_eq!(
                    db.begin_read().unwrap().raw().get(b"\0k").unwrap().map(|v| v.to_vec()),
                    expected_entry,
                    "stored entry differs for states {a_index}/{b_index}"
                );
            };

            let (a, a_delta) = fixture(a_state);
            let (b, b_delta) = fixture(b_state);
            pump_with(&a, &b, Lww, DEFAULT_MSG_CAP, 1).unwrap();
            pump_with(&b, &a, Lww, DEFAULT_MSG_CAP, 7).unwrap();
            check(&a);
            check(&b);
            assert_eq!(pump(&a, &b).end, SyncEnd::NoDifference);

            let (pushed, _) = fixture(a_state);
            let before = root(&pushed);
            let mut rx = DeltaReceiver::new(&pushed, Lww, T0 + 2, 4096).unwrap();
            for byte in &b_delta {
                rx.process(core::slice::from_ref(byte)).unwrap();
                assert_eq!(root(&pushed), before, "uncommitted delta became visible");
            }
            rx.finish().unwrap();
            check(&pushed);
            assert!(!apply_push(&pushed, &b_delta, Lww).committed);

            let combined = combine_deltas(&a_delta, &b_delta, 4096).unwrap();
            assert_eq!(combined, combine_deltas(&b_delta, &a_delta, 4096).unwrap());
            assert_eq!(combined, combine_deltas(&combined, &a_delta, 4096).unwrap());
            let (combined_target, _) = fixture(None);
            apply_push(&combined_target, &combined, Lww);
            check(&combined_target);

            let (prepared_target, _) = fixture(a_state);
            let (prepared_source, _) = fixture(b_state);
            let before = root(&prepared_target);
            let mut request = Vec::new();
            let mut rx = PreparedSync::new(
                &prepared_target,
                TestStaging::default(),
                TestStaging::default(),
                4096,
                &mut request,
            )
            .unwrap();
            let mut sender = SyncSender::new(&prepared_source).unwrap();
            let mut chunk = [0; 11];
            for round in 0..100 {
                assert!(!request.is_empty(), "prepared sync stalled at round {round}");
                sender.process(&request).unwrap();
                request.clear();
                let mut done = false;
                loop {
                    let n = sender.write_output(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    for byte in &chunk[..n] {
                        done = rx.process(core::slice::from_ref(byte), &mut request).unwrap()
                            == SyncStatus::Done;
                    }
                }
                assert_eq!(root(&prepared_target), before, "preparation changed the live tree");
                if done {
                    sender.process(&request).unwrap();
                    break;
                }
                assert!(round < 99, "prepared sync failed to complete");
            }
            let prepared = rx.finish().unwrap();
            prepared
                .merge(&prepared_target, Lww, T0 + 2, 4096)
                .unwrap()
                .finish_with_delta()
                .unwrap();
            check(&prepared_target);
        }
    }
}

#[test]
fn lww_resolvers_match_regression_cases() {
    fn bytes(fields: &[u64]) -> Vec<u8> {
        let mut value = vec![fields[3] as u8; fields[2] as usize];
        if let Some(last) = value.last_mut() {
            *last = fields[4] as u8;
        }
        value
    }
    fn state<'a>(fields: &[u64], value: &'a [u8]) -> EntState<'a> {
        let clock = Clock::from_u64(fields[1]);
        match fields[0] {
            0 => EntState::Missing,
            1 => EntState::Deleted { clock },
            2 => EntState::Present { clock, value },
            _ => panic!("invalid vector state"),
        }
    }
    fn streamed<'a>(state: EntState<'_>, value: &'a dyn ValueRead) -> SyncEntState<'a> {
        match state {
            EntState::Missing => SyncEntState::Missing,
            EntState::Deleted { clock } => SyncEntState::Deleted { clock },
            EntState::Present { clock, .. } => SyncEntState::Present { clock, value },
        }
    }
    let mut count = 0;
    for line in include_str!("../../tests/fixtures/kvdb-sync.txt").lines() {
        let mut words = line.split_whitespace();
        let fields: Vec<u64> = (0..10).map(|_| words.next().unwrap().parse().unwrap()).collect();
        let expected = if words.next().unwrap().parse::<bool>().unwrap() {
            Resolution::UseRemote
        } else {
            Resolution::KeepLocal
        };
        assert!(words.next().is_none());
        let a = bytes(&fields[..5]);
        let b = bytes(&fields[5..]);
        let local = state(&fields[..5], &a);
        let remote = state(&fields[5..], &b);
        assert_eq!(
            Resolver::resolve(&mut Lww, &Delta { key: b"k", local, remote }),
            expected,
            "{line}"
        );
        let a = crate::storage::SliceValue(&a);
        let b = crate::storage::SliceValue(&b);
        let result = SyncResolver::resolve(
            &mut Lww,
            &SyncDelta { key: b"k", local: streamed(local, &a), remote: streamed(remote, &b) },
        );
        assert_eq!(result.unwrap(), expected, "{line}");
        count += 1;
    }
    assert_eq!(count, 2401);
}

#[test]
fn three_replicas_converge_via_random_pairwise_syncs() {
    let mut rng: u64 = 0xfeed_f00d_dead_beef;
    let mut xorshift = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let dbs = [mk_db(), mk_db(), mk_db()];
    // interleave edits and random pairwise syncs
    for round in 0..30u64 {
        let i = (xorshift() % 3) as usize;
        let wall = T0 + round * 3;
        let mut w = dbs[i].begin_write(wall).unwrap();
        for _ in 0..(1 + xorshift() % 5) {
            let k = alloc::format!("k{}", xorshift() % 40);
            match xorshift() % 4 {
                0 => {
                    w.del(k.as_bytes()).unwrap();
                }
                _ => {
                    let v = alloc::format!("v{}", xorshift() % 1000);
                    w.put(k.as_bytes(), v.as_bytes()).unwrap();
                }
            }
        }
        w.commit().unwrap();
        let (dst, src) = ((xorshift() % 3) as usize, (xorshift() % 3) as usize);
        if dst != src {
            pump(&dbs[dst], &dbs[src]);
        }
    }
    // quiesce: full pairwise passes until all roots agree
    for _ in 0..4 {
        for dst in 0..3 {
            for src in 0..3 {
                if dst != src {
                    pump(&dbs[dst], &dbs[src]);
                }
            }
        }
    }
    let roots: Vec<_> = dbs.iter().map(root).collect();
    assert_eq!(roots[0], roots[1]);
    assert_eq!(roots[1], roots[2]);
    assert_eq!(contents(&dbs[0]), contents(&dbs[1]));
    assert_eq!(contents(&dbs[1]), contents(&dbs[2]));
    // and every pair reports no difference
    for dst in 0..3 {
        for src in 0..3 {
            if dst != src {
                assert_eq!(pump(&dbs[dst], &dbs[src]).end, SyncEnd::NoDifference);
            }
        }
    }
}

#[test]
fn plain_db_cannot_sync() {
    let plain = Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap();
    let mut out = Vec::new();
    assert!(matches!(
        SyncReceiver::new(&plain, Lww, TestStaging::default(), T0, &mut out).err(),
        Some(Error::Mode)
    ));
    assert!(matches!(SyncSender::new(&plain).err(), Some(Error::Mode)));
}

#[test]
fn garbage_input_is_rejected_not_panicking() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&b, T0, &[(b"x", b"1")]);
    let mut out = Vec::new();
    let mut recv = SyncReceiver::new(&a, Lww, TestStaging::default(), T0, &mut out).unwrap();
    // truncated/hostile frames must produce Protocol errors, not panics
    let hostile: &[&[u8]] = &[
        &[0xff; 3],
        &[8, 0, 0, 0, 99, 0, 0, 0],        // unknown type
        &[255, 255, 255, 255, 2, 0, 0, 0], // absurd size
    ];
    let mut got_err = false;
    for h in hostile {
        if recv.process(h, &mut Vec::new()).is_err() {
            got_err = true;
            break;
        }
    }
    assert!(got_err);
    // a failed session refuses to finish
    assert!(recv.finish().is_err());

    let mut send = SyncSender::new(&b).unwrap();
    assert!(send.process(&[8, 0, 0, 0, 4, 0, 0, 0]).is_err());
}

#[test]
fn writer_is_held_during_session_and_released_after() {
    let (a, b) = (mk_db(), mk_db());
    put_all(&b, T0, &[(b"x", b"1")]);
    let mut out = Vec::new();
    let recv = SyncReceiver::new(&a, Lww, TestStaging::default(), T0, &mut out).unwrap();
    assert!(matches!(a.begin_write(T0), Err(Error::Busy)));
    drop(recv); // aborted session releases the writer
    a.begin_write(T0).unwrap().commit().unwrap();
}

/// Sync efficiency: once two large replicas differ in one entry, the
/// data exchanged must be near-logarithmic, not proportional to db size.
#[test]
fn incremental_sync_is_sublinear() {
    let (a, b) = (mk_db(), mk_db());
    {
        let mut w = b.begin_write(T0).unwrap();
        for i in 0..2000u32 {
            w.put(format!("key{i:05}").as_bytes(), &[3u8; 20]).unwrap();
        }
        w.commit().unwrap();
    }
    pump(&a, &b);
    assert_eq!(root(&a), root(&b));
    // one small change on b
    put_all(&b, T0 + 60, &[(b"key01000", b"changed")]);

    // hand-rolled pump that counts bytes
    let mut r_out = Vec::new();
    let mut recv = SyncReceiver::new(&a, Lww, TestStaging::default(), T0 + 70, &mut r_out).unwrap();
    let mut send = SyncSender::new(&b).unwrap();
    let mut s_out = Vec::new();
    let mut transferred = 0usize;
    loop {
        if !r_out.is_empty() {
            let d = core::mem::take(&mut r_out);
            transferred += d.len();
            drain_sender(&mut send, &d, &mut s_out).unwrap();
        } else if !s_out.is_empty() {
            let d = core::mem::take(&mut s_out);
            transferred += d.len();
            recv.process(&d, &mut r_out).unwrap();
        } else {
            break;
        }
    }
    let o = recv.finish().unwrap();
    assert_eq!(o.end, SyncEnd::Merged);
    assert_eq!(o.applied, 1);
    assert_eq!(root(&a), root(&b));
    // full db is ~2000 * ~40B stored ≈ 80+ KB; a one-entry sync should
    // move only a handful of chunks
    assert!(transferred < 8 * 1024, "transferred {transferred} bytes");
}

#[test]
fn session_start_respects_the_output_capacity() {
    let db = mk_db();
    let mut full = vec![0; DEFAULT_MSG_CAP - 10];
    assert!(matches!(
        SyncReceiver::new(&db, Lww, TestStaging::default(), T0, &mut full),
        Err(Error::Capacity)
    ));
    assert_eq!(full.len(), DEFAULT_MSG_CAP - 10, "nothing is appended on refusal");
}

#[test]
fn equal_clock_ties_fall_back_to_value_length_after_a_common_prefix() {
    let clock = Clock { time: 7, count: 1 };
    let mut lww = Lww;
    let (short, long) = (crate::storage::SliceValue(b"ab"), crate::storage::SliceValue(b"abc"));
    fn present<'a>(value: &'a dyn crate::storage::ValueRead, clock: Clock) -> SyncEntState<'a> {
        SyncEntState::Present { value, clock }
    }
    let longer_remote =
        SyncDelta { key: b"k", local: present(&short, clock), remote: present(&long, clock) };
    assert_eq!(SyncResolver::resolve(&mut lww, &longer_remote).unwrap(), Resolution::UseRemote);
    let longer_local =
        SyncDelta { key: b"k", local: present(&long, clock), remote: present(&short, clock) };
    assert_eq!(SyncResolver::resolve(&mut lww, &longer_local).unwrap(), Resolution::KeepLocal);
    let same =
        SyncDelta { key: b"k", local: present(&short, clock), remote: present(&short, clock) };
    assert_eq!(SyncResolver::resolve(&mut lww, &same).unwrap(), Resolution::KeepLocal);
}

#[test]
fn streamed_value_views_reject_reads_past_their_end() {
    use crate::storage::ValueRead;
    let source = crate::storage::SliceValue(b"header+payload");
    let view = ValueSlice { source: &source, start: 7 };
    let mut out = [0; 8];
    assert_eq!(view.len(), 7);
    assert_eq!(view.read_at(0, &mut out).unwrap(), 7);
    assert_eq!(&out[..7], b"payload");
    assert_eq!(view.read_at(7, &mut out).unwrap(), 0);
    assert_eq!(view.read_at(8, &mut out), Err(Error::InvalidArgument));
}

#[test]
fn pending_repair_bounds_widen_in_both_directions() {
    let db = mk_db();
    put_all(&db, T0, &[(b"a", b"1"), (b"m", b"2"), (b"z", b"3")]);
    let mut txn = db.begin_write(T0 + 1).unwrap();
    let mut repair = PendingRepair::default();
    repair.prepare(&mut txn, b"m").unwrap();
    repair.changed(b"m", true);
    assert_eq!(
        (repair.lo.as_slice(), repair.hi.as_slice(), repair.stable),
        (&b"m"[..], &b"m"[..], true)
    );
    repair.prepare(&mut txn, b"z").unwrap();
    repair.changed(b"z", true);
    repair.prepare(&mut txn, b"a").unwrap();
    repair.changed(b"a", true);
    assert_eq!(
        (repair.lo.as_slice(), repair.hi.as_slice(), repair.stable),
        (&b"a"[..], &b"z"[..], false)
    );
    assert_eq!(repair.count, 3);
    repair.flush(&mut txn).unwrap();
    assert_eq!(repair.count, 0);
    txn.commit().unwrap();
}

#[test]
fn receiver_rejects_input_after_completion_and_inconsistent_entry_counts() {
    // bytes after the session has ended
    let a = mk_db();
    let mut rx = crafted_receiver(&a);
    let done = pull_response(S_NODIFF, 0, [0; 32], Clock { time: 1, count: 0 });
    assert_eq!(rx.process(&done, &mut Vec::new()).unwrap(), SyncStatus::Done);
    assert_eq!(rx.process(&[0], &mut Vec::new()), Err(Error::Protocol));

    // a span that would exceed the retained-entry bound
    let b = mk_db();
    let mut rx = receiver_awaiting_nodes(&b, 1);
    let mut p = vec![0u8, 0, 0, 0];
    p.extend_from_slice(&5000u32.to_le_bytes());
    assert_eq!(rx.process(&frame(T_NODES_RES, 0, &p), &mut Vec::new()), Err(Error::Capacity));

    // a count that promises an entry the frame does not carry
    let c = mk_db();
    let mut rx = receiver_awaiting_nodes(&c, 1);
    let mut p = vec![0u8, 0, 0, 0];
    p.extend_from_slice(&1u32.to_le_bytes());
    assert_eq!(rx.process(&frame(T_NODES_RES, 0, &p), &mut Vec::new()), Err(Error::Protocol));

    // payload bytes left over after the last promised entry
    let d = mk_db();
    let mut rx = receiver_awaiting_nodes(&d, 1);
    let mut p = vec![0u8, 0, 0, 0];
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(b"trailing");
    assert_eq!(rx.process(&frame(T_NODES_RES, 0, &p), &mut Vec::new()), Err(Error::Protocol));
}

#[test]
fn delta_visitor_rejects_zero_clocks_unordered_keys_and_entries_beyond_the_header_clock() {
    let db = mk_db();
    let mut tx = db.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"a", b"1").unwrap();
    tx.put(b"b", b"2").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    let mut count = 0;
    visit_delta(&bytes, 4096, |_, _| count += 1).unwrap();
    assert_eq!(count, 2);

    // header clock of zero
    let mut zero = bytes.clone();
    zero[20..28].fill(0);
    assert_eq!(visit_delta(&zero, 4096, |_, _| ()), Err(Error::Protocol));

    // entries out of order: swap the two entries in place (equal sizes)
    let entry_len = (bytes.len() - DELTA_HEADER) / 2;
    let mut swapped = bytes[..DELTA_HEADER].to_vec();
    swapped.extend_from_slice(&bytes[DELTA_HEADER + entry_len..]);
    swapped.extend_from_slice(&bytes[DELTA_HEADER..DELTA_HEADER + entry_len]);
    assert_eq!(visit_delta(&swapped, 4096, |_, _| ()), Err(Error::Protocol));

    // an entry clock above the advertised database clock (compact layout
    // stores the big-endian clock at bytes 2..10 of the stored value)
    let mut ahead = bytes.clone();
    let stored = DELTA_HEADER + 6 + 1;
    ahead[stored + 2..stored + 10].fill(0xff);
    assert_eq!(visit_delta(&ahead, 4096, |_, _| ()), Err(Error::Protocol));
}

#[test]
fn delta_capture_ignores_records_once_pull_is_required() {
    let mut capture = DeltaCapture::new(64);
    assert!(capture.admits(b"k", 4));
    capture.record(b"k", b"1234");
    assert_eq!(capture.entries.len(), 1);
    // A record larger than the remaining budget converts the capture to a
    // pull requirement and discards what was retained.
    capture.record(b"big", &[0; 64]);
    assert!(capture.pull);
    assert!(capture.entries.is_empty());
    capture.record(b"k", b"1234");
    assert!(capture.entries.is_empty(), "records after a pull requirement are ignored");
    assert!(!capture.admits(b"k", 1));
    assert!(matches!(
        capture.encode(mk_db().meta(), Clock { time: 1, count: 0 }),
        CommittedDelta::PullRequired
    ));
}

#[test]
fn delta_receipt_requires_a_syncable_database_and_observes_later_remote_clocks() {
    let plain = Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap();
    assert!(matches!(DeltaReceiver::new(&plain, Lww, T0, 4096), Err(Error::Mode)));

    // The remote wrote far in the future relative to the local wall clock;
    // the merge must observe the entry clock before committing.
    let (a, b) = (mk_db(), mk_db());
    let bytes = pushed_put(&a, T0 + 10_000, b"k", b"v");
    let before = b.clock();
    let mut rx = DeltaReceiver::new(&b, Lww, T0, 4096).unwrap();
    rx.process(&bytes).unwrap();
    let outcome = rx.finish().unwrap();
    assert_eq!(outcome.applied, 1);
    assert!(b.clock() > before);
    assert!(b.clock() > a.clock() || b.clock().time >= a.clock().time);
    let r = b.begin_read().unwrap();
    let (clock, _) = r.get_with_clock(b"k").unwrap().unwrap();
    assert_eq!(clock, a.begin_read().unwrap().get_with_clock(b"k").unwrap().unwrap().0);
}

#[test]
fn sender_enforces_request_framing_and_output_draining_rules() {
    let db = mk_db();
    put_all(&db, T0, &[(b"x", b"1")]);
    let peer = mk_db();
    put_all(&peer, T0 + 5, &[(b"z", b"9")]);
    let mut req = Vec::new();
    let _rx = SyncReceiver::new(&peer, Lww, TestStaging::default(), T0, &mut req).unwrap();

    // output must be drained before the next request
    let mut tx = SyncSender::new(&db).unwrap();
    tx.process(&req).unwrap();
    assert_eq!(tx.process(&frame(T_BYE, 0, &[0])), Err(Error::Protocol));

    // an empty output buffer is a caller error, not "no output"
    let mut tx = SyncSender::new(&db).unwrap();
    assert_eq!(tx.write_output(&mut []), Err(Error::InvalidArgument));

    // more input than a legal request can span
    let mut tx = SyncSender::new(&db).unwrap();
    assert_eq!(tx.process(&vec![0; MAX_REQUEST_SIZE + 1]), Err(Error::Protocol));

    // requests never carry frame flags
    let mut tx = SyncSender::new(&db).unwrap();
    let mut flagged = req.clone();
    flagged[6] = 1;
    assert_eq!(tx.process(&flagged), Err(Error::Protocol));

    // bytes after a complete request in the same fragment
    let mut tx = SyncSender::new(&db).unwrap();
    let mut extra = req.clone();
    extra.push(0);
    assert_eq!(tx.process(&extra), Err(Error::Protocol));

    // a farewell must carry exactly the zero byte
    let mut tx = SyncSender::new(&db).unwrap();
    assert_eq!(tx.process(&frame(T_BYE, 0, &[1])), Err(Error::Protocol));
    let mut tx = SyncSender::new(&db).unwrap();
    assert_eq!(tx.process(&frame(T_BYE, 0, &[0])).unwrap(), SyncStatus::Done);
}

#[test]
fn delta_local_qualification_commits_without_changing_lww_or_raw_counts() {
    // An authenticated receipt may add knowledge about a losing source. Commit
    // its local rows atomically, preserve raw counts/root, and abort on failure.
    let source = mk_db();
    let target = mk_db();
    let mut tx = source.begin_write_with_delta(T0, 4096).unwrap();
    tx.put(b"entry", b"old").unwrap();
    let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    let mut tx = target.begin_write(T0 + 1).unwrap();
    tx.put(b"entry", b"new").unwrap();
    tx.commit().unwrap();
    let root = target.begin_read().unwrap().root_hash().unwrap();
    let mut receiver = DeltaReceiver::new(&target, Lww, T0 + 2, 4096).unwrap();
    receiver.process(&bytes).unwrap();
    let outcome = receiver
        .qualify_local_indexes(|local| -> Result<bool> {
            local.put(b"accepted", b"old")?;
            Ok(true)
        })
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(outcome.applied, 0);
    assert!(outcome.committed);
    let read = target.begin_read().unwrap();
    assert_eq!(read.root_hash().unwrap(), root);
    assert_eq!(read.get(b"entry").unwrap().unwrap().as_ref(), b"new");
    assert_eq!(read.local_get(b"accepted").unwrap().unwrap().as_ref(), b"old");
    drop(read);
    let mut replay = DeltaReceiver::new(&target, Lww, T0 + 3, 4096).unwrap();
    replay.process(&bytes).unwrap();
    assert!(
        !replay
            .qualify_local_indexes(|_| -> Result<bool> { Ok(false) })
            .unwrap()
            .finish()
            .unwrap()
            .committed
    );
    let mut rejected = DeltaReceiver::new(&target, Lww, T0 + 4, 4096).unwrap();
    rejected.process(&bytes).unwrap();
    assert!(
        rejected
            .qualify_local_indexes(|local| -> Result<bool> {
                local.put(b"uncommitted", b"bad")?;
                Err(Error::Protocol)
            })
            .is_err()
    );
    assert!(target.begin_read().unwrap().local_get(b"uncommitted").unwrap().is_none());
}

#[test]
fn snapshots_stream_past_the_chunk_entry_limit_in_one_flight() {
    let (source, target) = (mk_db(), mk_db());
    let mut tx = source.begin_write(T0).unwrap();
    for i in 0..(SPAN_ENTRY_MAX + 17) as u32 {
        tx.put(&i.to_be_bytes(), b"snapshot").unwrap();
    }
    // Tombstones and their originating clocks are part of the snapshot.
    tx.del(&0u32.to_be_bytes()).unwrap();
    tx.commit().unwrap();
    let mut request = Vec::new();
    let mut receiver =
        SyncReceiver::new(&target, Lww, TestStaging::default(), T0, &mut request).unwrap();
    let mut sender = SyncSender::new(&source).unwrap();
    for fragment in core::mem::take(&mut request).chunks(3) {
        sender.process(fragment).unwrap();
    }
    let mut buffer = [0; 113];
    let mut done = false;
    loop {
        let n = sender.write_output(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        done = receiver.process(&buffer[..n], &mut request).unwrap() == SyncStatus::Done;
        if !done {
            assert!(request.is_empty(), "snapshot needs no discovery request");
        }
        assert_eq!(receiver.span_entries, 0, "streamed leaves retain no span list");
    }
    assert!(done);
    assert_eq!(request, frame(T_BYE, 0, &[0]));
    receiver.finish().unwrap();
    assert_eq!(root(&source), root(&target));
    assert_eq!(contents(&source), contents(&target));
    target.verify().unwrap();
}

#[test]
fn small_range_threshold_counts_encoded_bytes_and_skips_identical_entries() {
    for extra in [0, 1] {
        let (source, target) = (mk_db(), mk_db());
        let value = vec![7; SMALL_RANGE_BYTES - 22 - 1 - encoding::ENT_HEADER_FULL + extra];
        put_all(&source, T0 + 1, &[(b"k", &value)]);
        put_all(&target, T0, &[(b"k", b"old")]);
        let mut request = Vec::new();
        let receiver =
            SyncReceiver::new(&target, Lww, TestStaging::default(), T0, &mut request).unwrap();
        let mut sender = SyncSender::new(&source).unwrap();
        let mut response = Vec::new();
        drain_sender(&mut sender, &request, &mut response).unwrap();
        assert_eq!(response[8], if extra == 0 { S_SNAPSHOT } else { S_DIFF });
        drop(receiver);
        assert_eq!(pump(&target, &source).applied, 1);
    }
    let (source, target) = (mk_db(), mk_db());
    put_all(&source, T0, &[(b"a", b"same"), (b"b", b"before"), (b"c", b"deleted")]);
    pump(&target, &source);
    let mut tx = source.begin_write(T0 + 1).unwrap();
    tx.put(b"b", b"after").unwrap();
    tx.del(b"c").unwrap();
    tx.commit().unwrap();
    let outcome = pump_with(&target, &source, Lww, DEFAULT_MSG_CAP, 1).unwrap();
    assert_eq!((outcome.deltas, outcome.applied), (2, 2));
    assert_eq!(root(&source), root(&target));
}

#[test]
fn pipelined_leaf_spans_preserve_order_and_skip_unchanged_siblings() {
    let options = CreateOptions::syncable(HASH_KEY, 1);
    let source = Db::create(MemStorage::new(), &options).unwrap();
    let target = Db::create(MemStorage::new(), &options).unwrap();
    let mut tx = source.begin_write(T0).unwrap();
    for i in 0..4096u32 {
        tx.put(&i.to_be_bytes(), &[3; 300]).unwrap();
    }
    tx.commit().unwrap();
    pump(&target, &source);
    let mut tx = source.begin_write(T0 + 1).unwrap();
    for i in 0..4096u32 {
        // Dense differences plus unchanged spans and a local-only tail test
        // both ordered batching and skipping across parent boundaries.
        if i % 128 < 64 {
            tx.put(&i.to_be_bytes(), &[4; 300]).unwrap();
        }
    }
    tx.commit().unwrap();
    put_all(&target, T0 + 2, &[(b"local-only", b"keep")]);
    let mut previous = Vec::new();
    let mut resolver = |delta: &SyncDelta<'_>| {
        assert!(previous.is_empty() || previous.as_slice() < delta.key);
        previous = delta.key.to_vec();
        SyncResolver::resolve(&mut Lww, delta)
    };
    let mut request = Vec::new();
    let mut receiver =
        SyncReceiver::new(&target, &mut resolver, TestStaging::default(), T0, &mut request)
            .unwrap();
    let mut sender = SyncSender::new(&source).unwrap();
    let mut batches = 0;
    let mut descriptors = 0;
    let mut buffer = [0; 997];
    loop {
        let typ = u16::from_le_bytes(request[4..6].try_into().unwrap());
        if typ == T_NODES_REQ {
            let count = u16::from_le_bytes(request[8..10].try_into().unwrap()) as usize;
            assert!((1..=REQUEST_BATCH_MAX).contains(&count));
            batches += 1;
            descriptors += count;
        }
        for fragment in core::mem::take(&mut request).chunks(7) {
            sender.process(fragment).unwrap();
        }
        if typ == T_BYE {
            break;
        }
        loop {
            let n = sender.write_output(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            receiver.process(&buffer[..n], &mut request).unwrap();
        }
        assert!(!request.is_empty());
    }
    assert!(descriptors > batches * 2, "multiple leaf spans must share each flight");
    assert_eq!(receiver.finish().unwrap().applied, 2048);
    assert_eq!(target.begin_read().unwrap().get(b"local-only").unwrap().unwrap().as_ref(), b"keep");
    pump(&source, &target);
    assert_eq!(root(&source), root(&target));
    target.verify().unwrap();
}

#[test]
fn snapshot_truncation_and_wrong_response_kind_abort_without_commit() {
    let source = mk_db();
    put_all(&source, T0, &[(b"a", b"one"), (b"b", b"two")]);
    let target = mk_db();
    let mut request = Vec::new();
    let receiver =
        SyncReceiver::new(&target, Lww, TestStaging::default(), T0, &mut request).unwrap();
    let mut response = Vec::new();
    drain_sender(&mut SyncSender::new(&source).unwrap(), &request, &mut response).unwrap();
    drop(receiver);
    for length in [0, 1, 51, 52, 53, response.len() - 1] {
        let mut receiver = crafted_receiver(&target);
        receiver.process(&response[..length], &mut Vec::new()).unwrap();
        assert_eq!(receiver.finish().err(), Some(Error::Protocol));
        assert_eq!(root(&target), None);
    }
    let mut bad = response.clone();
    bad[56..58].copy_from_slice(&T_NODES_RES.to_le_bytes());
    let mut receiver = crafted_receiver(&target);
    assert_eq!(receiver.process(&bad, &mut Vec::new()), Err(Error::Protocol));
    assert!(receiver.finish().is_err());
    assert_eq!(root(&target), None);
    pump(&target, &source);
    assert_eq!(root(&source), root(&target));
}

#[test]
fn streamed_range_rejects_keys_before_its_requested_bound() {
    let target = mk_db();
    let mut receiver = crafted_receiver(&target);
    receiver.state = RState::AwaitNodes {
        kind: 0,
        child_level: 1,
        cursor: vec![0, b'm'],
        exclusive: false,
        stop: Some(b"z".to_vec()),
        last: None,
        acc: Vec::new(),
    };
    let mut stored = Vec::new();
    let hash = crate::hash::entry_hash(&HASH_KEY, b"a", 0, b"outside");
    encoding::encode_ent_val(&mut stored, &hash, 0, Clock { time: 1, count: 1 }, b"outside");
    let mut response = nodes_response(0, 0, &[(b"a", &stored)]);
    response[4..6].copy_from_slice(&T_RANGE_RES.to_le_bytes());
    assert_eq!(receiver.process(&response, &mut Vec::new()), Err(Error::Protocol));
    assert!(receiver.finish().is_err());
    assert_eq!(root(&target), None);
}

#[test]
fn sender_rejects_unbounded_or_unordered_batches_before_emitting_responses() {
    let (source, target) = (mk_db(), mk_db());
    put_all(&source, T0, &[(b"k", b"value")]);
    let mut request = Vec::new();
    let _receiver =
        SyncReceiver::new(&target, Lww, TestStaging::default(), T0, &mut request).unwrap();
    let descriptor = |level, key: &[u8]| {
        let mut bytes = vec![level, 0];
        bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes
    };
    let mut invalid = vec![vec![0, 0, 0, 0], vec![(REQUEST_BATCH_MAX + 1) as u8, 0, 0, 0]];
    for (a, b) in [
        (descriptor(1, b"z"), descriptor(1, b"a")),
        (descriptor(1, b"a"), descriptor(1, b"a")),
        (descriptor(2, b"a"), descriptor(2, b"b")),
    ] {
        let mut bytes = vec![2, 0, 0, 0];
        bytes.extend(a);
        bytes.extend(b);
        invalid.push(bytes);
    }
    let mut oversized = vec![1, 0, 0, 0];
    oversized.extend(descriptor(1, &vec![1; crate::KEY_SIZE_MAX + 1]));
    invalid.push(oversized);
    for payload in invalid {
        let mut sender = SyncSender::new(&source).unwrap();
        drain_sender(&mut sender, &request, &mut Vec::new()).unwrap();
        assert_eq!(sender.process(&frame(T_NODES_REQ, 0, &payload)), Err(Error::Protocol));
        assert_eq!(sender.write_output(&mut [0; 100]).unwrap(), 0);
    }
}
