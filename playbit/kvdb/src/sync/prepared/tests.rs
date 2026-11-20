#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::{
    CreateOptions,
    storage::{SliceValue, mem::MemStorage},
};
use alloc::format;
const WALL: u64 = crate::clock::EPOCH_UNIX_SECS + 7000;
#[derive(Default)]
struct Bytes(Vec<u8>);
impl ValueRead for Bytes {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        SliceValue(&self.0).read_at(offset, out)
    }
}
impl SyncStaging for Bytes {
    fn clear(&mut self) -> Result<()> {
        self.0.clear();
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}
fn db() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([32; 32], 1)).unwrap()
}
fn put(db: &Db<MemStorage>, key: &[u8], value: &[u8], wall: u64) -> Clock {
    let mut tx = db.begin_write(wall).unwrap();
    tx.put(key, value).unwrap();
    let clock = tx.clock();
    tx.commit().unwrap();
    clock
}
fn exchange(
    rx: &mut PreparedSync<'_, MemStorage, Bytes, Bytes>,
    src: &Db<MemStorage>,
    mut request: Vec<u8>,
) -> Result<()> {
    let mut sender = SyncSender::new(src)?;
    let mut chunk = [0; 137];
    loop {
        if request.is_empty() {
            return Err(Error::Protocol);
        }
        sender.process(&request)?;
        request.clear();
        let mut done = false;
        loop {
            let n = sender.write_output(&mut chunk)?;
            if n == 0 {
                break;
            }
            for input in chunk[..n].chunks(3) {
                done = rx.process(input, &mut request)? == SyncStatus::Done;
            }
        }
        if done {
            sender.process(&request)?;
            return Ok(());
        }
    }
}
fn prepare(dst: &Db<MemStorage>, src: &Db<MemStorage>) -> PreparedChanges<Bytes> {
    let mut request = Vec::new();
    let mut rx =
        PreparedSync::new(dst, Bytes::default(), Bytes::default(), 2 * 1024 * 1024, &mut request)
            .unwrap();
    exchange(&mut rx, src, request).unwrap();
    rx.finish().unwrap()
}

#[test]
fn one_completed_journal_builds_canonical_and_lww_copies() {
    let source = db();
    let local = db();
    let canonical = db();
    put(&source, b"name", b"accepted", WALL);
    for database in [&local, &canonical] {
        put(database, b"name", b"offline", WALL + 1);
        put(database, b"local-only", b"retained", WALL + 1);
    }
    let changes = prepare(&local, &source);
    changes
        .merge(&canonical, |_: &SyncDelta<'_>| Ok(Resolution::UseRemote), WALL + 2, 0)
        .unwrap()
        .finish_with_delta()
        .unwrap();
    changes.merge(&local, Lww, WALL + 2, 0).unwrap().finish_with_delta().unwrap();
    let accepted = canonical.begin_read().unwrap();
    assert_eq!(accepted.get(b"name").unwrap().unwrap().as_ref(), b"accepted");
    assert!(accepted.get(b"local-only").unwrap().is_none());
    let kept = local.begin_read().unwrap();
    assert_eq!(kept.get(b"name").unwrap().unwrap().as_ref(), b"offline");
    assert_eq!(kept.get(b"local-only").unwrap().unwrap().as_ref(), b"retained");
    assert_eq!(accepted.root_hash().unwrap(), source.begin_read().unwrap().root_hash().unwrap());
}

#[test]
fn completed_identical_exchange_has_no_differences_even_when_target_changes_later() {
    // Exercise the actual fragmented exchange. Empty describes its completed
    // journal, not the current database or authority after receipt.
    let src = db();
    let dst = db();
    assert!(prepare(&dst, &src).is_empty());
    put(&src, b"entry", b"supplied", WALL);
    assert!(!prepare(&dst, &src).is_empty());
    prepare(&dst, &src).merge(&dst, Lww, WALL, 65536).unwrap().finish_with_delta().unwrap();
    let identical = prepare(&dst, &src);
    assert!(identical.is_empty());
    put(&dst, b"later", b"keep", WALL + 1);
    assert!(identical.is_empty());
    assert!(!prepare(&dst, &src).is_empty());
}

#[test]
fn prepared_merge_derives_indexes_from_current_state_and_aborts_both_namespaces() {
    // A local rename after detached receipt wins against the older received
    // record. Repair sees that current winner plus a newly accepted source key.
    let src = db();
    let dst = db();
    put(&src, b"entry", b"remote", WALL);
    put(&src, b"other", b"accepted", WALL);
    let mut tx = src.begin_write(WALL + 1).unwrap();
    tx.local_indexes().put(b"forged", b"untrusted").unwrap();
    tx.commit().unwrap();
    let changes = prepare(&dst, &src);
    put(&dst, b"entry", b"newer local", WALL + 2);
    let old = dst.begin_read().unwrap();
    let before = dst.clock();
    let failed = changes.merge(&dst, Lww, WALL + 3, 4096).unwrap().repair_local_indexes(|index| {
        assert_eq!(index.source_get(b"entry")?.unwrap().as_ref(), b"newer local");
        assert_eq!(index.source_get(b"other")?.unwrap().as_ref(), b"accepted");
        index.put(b"derived", b"partially repaired")?;
        Err::<(), _>(Error::Storage(91))
    });
    assert_eq!(failed.err(), Some(Error::Storage(91)));
    assert_eq!(dst.clock(), before);
    assert!(dst.begin_read().unwrap().get(b"other").unwrap().is_none());
    assert!(dst.begin_read().unwrap().local_get(b"derived").unwrap().is_none());
    let changes = prepare(&dst, &src);
    let (outcome, Some(CommittedDelta::Bytes(delta))) = changes
        .merge(&dst, Lww, WALL + 3, 4096)
        .unwrap()
        .repair_local_indexes(|index| {
            let value = index.source_get(b"entry")?.unwrap().to_vec();
            index.put(b"derived", &value)
        })
        .unwrap()
        .finish_with_delta()
        .unwrap()
    else {
        panic!()
    };
    assert!(outcome.committed);
    visit_delta(&delta, 4096, |key, _| assert_eq!(key, b"other")).unwrap();
    assert_eq!(
        dst.begin_read().unwrap().local_get(b"derived").unwrap().unwrap().as_ref(),
        b"newer local"
    );
    assert!(dst.begin_read().unwrap().local_get(b"forged").unwrap().is_none());
    assert!(old.local_get(b"derived").unwrap().is_none());
    dst.verify().unwrap();
    let changes = prepare(&dst, &src);
    assert!(
        !changes
            .merge(&dst, Lww, WALL + 4, 4096)
            .unwrap()
            .repair_local_indexes::<Error>(|_| panic!("unchanged source reached repair"))
            .unwrap()
            .finish_with_delta()
            .unwrap()
            .0
            .committed
    );
}
#[test]
fn inspection_streams_values_without_a_writer_and_merges_current_local_state() {
    let src = db();
    let dst = db();
    for index in 0..128 {
        put(&src, format!("record/{index:03}").as_bytes(), &[7; 512], WALL);
    }
    put(&src, b"large", &vec![9; 1024 * 1024], WALL);
    let mut inspection = prepare(&dst, &src).inspect();
    assert!(std::mem::size_of_val(&inspection) <= 2048);
    let mut count = 0;
    let mut largest = 0;
    while inspection
        .next(|key, remote| {
            let SyncEntState::Present { value, .. } = remote else { panic!() };
            largest = largest.max(value.len());
            let mut chunk = [0; 4096];
            for offset in (0..value.len()).step_by(chunk.len()) {
                let n = value.read_at(offset, &mut chunk)?;
                assert!(n > 0);
                assert!(chunk[..n].iter().all(|byte| *byte == if key == b"large" { 9 } else { 7 }));
            }
            Ok(())
        })
        .unwrap()
    {
        count += 1;
        // An asynchronous caller may be waiting for record authentication
        // here. There is no writer lease preventing this local edit.
        if count == 16 {
            put(&dst, b"record/064", b"newer local", WALL + 2);
        }
    }
    assert_eq!(count, 129);
    assert_eq!(largest, 1024 * 1024);
    let changes = inspection.finish().unwrap();
    changes.merge(&dst, Lww, WALL + 3, 65536).unwrap().finish_with_delta().unwrap();
    let snapshot = dst.begin_read().unwrap();
    assert_eq!(snapshot.get(b"record/064").unwrap().unwrap().as_ref(), b"newer local");
    assert_eq!(snapshot.get(b"record/063").unwrap().unwrap().as_ref(), &[7; 512]);
    assert_eq!(snapshot.get(b"large").unwrap().unwrap().len(), 1024 * 1024);
}

#[test]
fn incomplete_failed_or_corrupt_inspection_cannot_finish() {
    let src = db();
    let dst = db();
    put(&src, b"a", b"first", WALL);
    put(&src, b"b", b"second", WALL);
    assert!(matches!(prepare(&dst, &src).inspect().finish(), Err(Error::Protocol)));
    let mut failed = prepare(&dst, &src).inspect();
    assert!(failed.next(|_, _| Ok(())).unwrap());
    assert!(matches!(failed.next(|_, _| Err(Error::Protocol)), Err(Error::Protocol)));
    assert!(matches!(failed.next(|_, _| Ok(())), Err(Error::Protocol)));
    assert!(matches!(failed.finish(), Err(Error::Protocol)));
    let mut changes = prepare(&dst, &src);
    changes.journal.0.pop();
    let mut corrupt = changes.inspect();
    assert!(corrupt.next(|_, _| Ok(())).unwrap());
    assert!(corrupt.next(|_, _| Ok(())).is_err());
    assert!(matches!(corrupt.finish(), Err(Error::Protocol)));
    assert!(dst.begin_read().unwrap().get(b"a").unwrap().is_none());
    let empty = db();
    let mut inspection = prepare(&dst, &empty).inspect();
    assert!(!inspection.next(|_, _| panic!("empty journal")).unwrap());
    assert!(!inspection.next(|_, _| panic!("completed journal")).unwrap());
    inspection.finish().unwrap().merge(&dst, Lww, WALL, 0).unwrap().finish_with_delta().unwrap();
}

#[test]
fn inspection_preserves_missing_and_tombstone_states() {
    let src = db();
    let dst = db();
    put(&src, b"deleted", b"old", WALL);
    let mut tx = src.begin_write(WALL + 1).unwrap();
    tx.del(b"deleted").unwrap();
    tx.commit().unwrap();
    put(&dst, b"local", b"kept", WALL);
    let mut inspection = prepare(&dst, &src).inspect();
    let mut deleted = 0;
    let mut missing = 0;
    while inspection
        .next(|key, remote| {
            match remote {
                SyncEntState::Deleted { clock } => {
                    assert_eq!(key, b"deleted");
                    assert!(!clock.is_zero());
                    deleted += 1;
                }
                SyncEntState::Missing => {
                    assert_eq!(key, b"local");
                    missing += 1;
                }
                _ => panic!("unexpected journal value"),
            }
            Ok(())
        })
        .unwrap()
    {}
    assert_eq!((deleted, missing), (1, 1));
    inspection.finish().unwrap();
}

#[test]
#[ignore = "reference-device cursor timing; run with --release --ignored --nocapture"]
fn inspection_metadata_latency() {
    let src = db();
    let dst = db();
    for index in 0..128 {
        put(&src, format!("record/{index:03}").as_bytes(), &[7; 512], WALL);
    }
    let mut timings = Vec::new();
    for _ in 0..10 {
        // Journal construction and payload consumption are outside this
        // metadata-only boundary. Product OPFS/crypto/merge are separate.
        let mut inspection = prepare(&dst, &src).inspect();
        loop {
            let start = std::time::Instant::now();
            let visited = inspection
                .next(|key, state| {
                    std::hint::black_box((key, state));
                    Ok(())
                })
                .unwrap();
            let elapsed = start.elapsed().as_nanos();
            if !visited {
                break;
            }
            timings.push(elapsed);
        }
        inspection.finish().unwrap();
    }
    timings.sort_unstable();
    assert_eq!(timings.len(), 1280);
    std::println!(
        "inspection metadata only: n={} p50_ns={} p99_ns={} max_ns={} cursor_bytes={} fixture=128x512B memory-journal",
        timings.len(),
        timings[639],
        timings[1267],
        timings[1279],
        core::mem::size_of::<PreparedInspection<Bytes>>()
    );
}

#[test]
fn receive_does_not_hold_writer_and_merge_preserves_concurrent_winners() {
    let (src, dst) = (db(), db());
    put(&src, b"conflict", b"remote", WALL);
    let remote_clock = put(&src, b"added", b"remote addition", WALL);
    put(&dst, b"conflict", b"initial", WALL - 1);
    put(&dst, b"local-only", b"retain", WALL - 1);
    let mut writer = dst.begin_write(WALL + 10).unwrap();
    let mut request = Vec::new();
    let mut rx =
        PreparedSync::new(&dst, Bytes::default(), Bytes::default(), 65536, &mut request).unwrap();
    // A live writer exists during constructor and every receive callback.
    writer.put(b"conflict", b"concurrent winner").unwrap();
    exchange(&mut rx, &src, request).unwrap();
    let changes = rx.finish().unwrap();
    writer.commit().unwrap();
    let (outcome, delta) =
        changes.merge(&dst, Lww, WALL + 11, 65536).unwrap().finish_with_delta().unwrap();
    assert_eq!(outcome.applied, 1);
    let read = dst.begin_read().unwrap();
    assert_eq!(&*read.get(b"conflict").unwrap().unwrap(), b"concurrent winner");
    assert_eq!(&*read.get(b"local-only").unwrap().unwrap(), b"retain");
    assert_eq!(&*read.get(b"added").unwrap().unwrap(), b"remote addition");
    let Some(CommittedDelta::Bytes(delta)) = delta else {
        panic!("expected captured change");
    };
    let copy = db();
    let mut receiver = DeltaReceiver::new(&copy, Lww, WALL + 12, 65536).unwrap();
    receiver.process(&delta).unwrap();
    receiver.finish().unwrap();
    let remote = copy.begin_read().unwrap();
    let entry = remote.raw().snapshot_value(b"\0added").unwrap().unwrap();
    assert_eq!(StreamEntry::new(&entry).unwrap().clock, remote_clock);
    assert!(remote.get(b"conflict").unwrap().is_none());
}
#[test]
fn tombstone_clock_no_change_and_explicit_missing_resolution() {
    let (src, dst) = (db(), db());
    put(&src, b"gone", b"value", WALL);
    prepare(&dst, &src).merge(&dst, Lww, WALL, 65536).unwrap().finish_with_delta().unwrap();
    let mut tx = src.begin_write(WALL + 1).unwrap();
    tx.del(b"gone").unwrap();
    let deleted = tx.clock();
    tx.commit().unwrap();
    prepare(&dst, &src).merge(&dst, Lww, WALL + 2, 65536).unwrap().finish_with_delta().unwrap();
    assert_eq!(dst.begin_read().unwrap().is_del(b"gone").unwrap(), Some(deleted));
    let before = dst.clock();
    let (outcome, delta) =
        prepare(&dst, &src).merge(&dst, Lww, WALL + 3, 65536).unwrap().finish_with_delta().unwrap();
    assert!(!outcome.committed);
    assert!(delta.is_none());
    assert_eq!(dst.clock(), before);
    put(&dst, b"local", b"old", WALL + 4);
    put(&src, b"remote", b"new", WALL + 4);
    let changes = prepare(&dst, &src);
    put(&dst, b"local", b"current", WALL + 5);
    let mut saw_current_missing = false;
    let resolver = |delta: &SyncDelta<'_>| {
        if delta.key == b"local" && matches!(delta.remote, SyncEntState::Missing) {
            let SyncEntState::Present { value, .. } = delta.local else { panic!() };
            let mut buf = [0; 7];
            value.read_exact_at(0, &mut buf)?;
            assert_eq!(&buf, b"current");
            saw_current_missing = true;
        }
        SyncResolver::resolve(&mut Lww, delta)
    };
    changes.merge(&dst, resolver, WALL + 6, 65536).unwrap().finish_with_delta().unwrap();
    assert!(saw_current_missing);
    assert!(dst.begin_read().unwrap().has(b"local").unwrap());
}
#[test]
fn incomplete_malformed_capacity_and_validation_leave_live_unchanged() {
    let (src, dst) = (db(), db());
    put(&src, b"new", b"payload", WALL);
    put(&dst, b"existing", b"safe", WALL);
    let before = (dst.clock(), dst.begin_read().unwrap().root_hash().unwrap());
    let mut output = Vec::new();
    let rx =
        PreparedSync::new(&dst, Bytes::default(), Bytes::default(), 65536, &mut output).unwrap();
    assert!(matches!(rx.finish(), Err(Error::Protocol)));
    output.clear();
    let mut rx =
        PreparedSync::new(&dst, Bytes::default(), Bytes::default(), 65536, &mut output).unwrap();
    assert_eq!(rx.process(&[0; 8], &mut output), Err(Error::Protocol));
    assert!(matches!(rx.finish(), Err(Error::Protocol)));
    output.clear();
    let mut rx =
        PreparedSync::new(&dst, Bytes::default(), Bytes::default(), 1, &mut output).unwrap();
    assert_eq!(exchange(&mut rx, &src, output), Err(Error::Capacity));
    assert!(matches!(rx.finish(), Err(Error::Protocol)));
    let pending = prepare(&dst, &src).merge(&dst, Lww, WALL + 1, 65536).unwrap();
    assert!(pending.pending_transaction().unwrap().has(b"new").unwrap());
    // Application validation rejects the pending state by dropping it.
    drop(pending);
    assert!(matches!(
        prepare(&dst, &src).merge(&dst, |_: &SyncDelta<'_>| Err(Error::Mode), WALL + 1, 65536),
        Err(Error::Mode)
    ));
    assert_eq!((dst.clock(), dst.begin_read().unwrap().root_hash().unwrap()), before);
    assert!(!dst.begin_read().unwrap().has(b"new").unwrap());
    put(&dst, b"after-failure", b"works", WALL + 2);
}
#[test]
fn damaged_journal_rejects_truncation_clock_order_and_duplicates_atomically() {
    let (src, dst) = (db(), db());
    put(&src, b"a", b"first", WALL);
    put(&src, b"b", b"second", WALL);
    let before = (dst.clock(), dst.begin_read().unwrap().root_hash().unwrap());
    for damage in 0..4 {
        let mut changes = prepare(&dst, &src);
        match damage {
            0 => {
                changes.journal.0.pop();
            }
            1 => {
                changes.journal.0[6..14].copy_from_slice(&Clock::from_u64(u64::MAX).encode());
            }
            2 => {
                changes.journal.0[HEADER] = b'z';
            }
            _ => {
                let duplicate = changes.journal.0.clone();
                changes.journal.0.extend_from_slice(&duplicate);
            }
        }
        assert!(matches!(changes.merge(&dst, Lww, WALL + 1, 65536), Err(Error::Corrupt)));
        assert_eq!((dst.clock(), dst.begin_read().unwrap().root_hash().unwrap()), before);
    }
    // Failure after an earlier journal entry was applied still aborts it.
    let mut calls = 0;
    let rejected = |delta: &SyncDelta<'_>| {
        calls += 1;
        if calls == 2 {
            return Err(Error::Mode);
        }
        SyncResolver::resolve(&mut Lww, delta)
    };
    assert!(matches!(prepare(&dst, &src).merge(&dst, rejected, WALL + 1, 65536), Err(Error::Mode)));
    assert_eq!((dst.clock(), dst.begin_read().unwrap().root_hash().unwrap()), before);
}
#[test]
fn prepared_changes_are_bound_to_hash_key_and_branch_factor() {
    let (src, dst) = (db(), db());
    put(&src, b"key", b"value", WALL);
    let other = Db::create(MemStorage::new(), &CreateOptions::syncable([33; 32], 1)).unwrap();
    assert!(matches!(
        prepare(&dst, &src).merge(&other, Lww, WALL, 65536),
        Err(Error::HashKeyMismatch)
    ));
    let mut options = CreateOptions::syncable([32; 32], 1);
    options.branch_factor = 4;
    let other = Db::create(MemStorage::new(), &options).unwrap();
    assert!(matches!(
        prepare(&dst, &src).merge(&other, Lww, WALL, 65536),
        Err(Error::BranchFactorMismatch)
    ));
}

#[test]
fn inspection_rejects_malformed_headers_and_unordered_journal_entries() {
    let (src, dst) = (db(), db());
    put(&src, b"a", b"first", WALL);
    put(&src, b"b", b"second", WALL);
    // An entry kind outside the journal's vocabulary.
    let mut changes = prepare(&dst, &src);
    changes.journal.0[14] = 5;
    let mut inspection = changes.inspect();
    assert!(matches!(inspection.next(|_, _| Ok(())), Err(Error::Corrupt)));
    assert!(matches!(inspection.finish(), Err(Error::Protocol)));
    // A second copy of the journal repeats keys that must strictly ascend.
    let mut changes = prepare(&dst, &src);
    let duplicate = changes.journal.0.clone();
    changes.journal.0.extend_from_slice(&duplicate);
    let mut inspection = changes.inspect();
    assert!(inspection.next(|_, _| Ok(())).unwrap());
    assert!(inspection.next(|_, _| Ok(())).unwrap());
    assert!(matches!(inspection.next(|_, _| Ok(())), Err(Error::Corrupt)));
}

#[test]
fn inspected_values_are_bounded_views_of_the_journal() {
    let (src, dst) = (db(), db());
    put(&src, b"k", b"journaled value", WALL);
    let mut inspection = prepare(&dst, &src).inspect();
    let mut seen = false;
    assert!(
        inspection
            .next(|key, remote| {
                assert_eq!(key, b"k");
                let SyncEntState::Present { value, .. } = remote else { panic!("live entry") };
                let mut out = [0; 8];
                assert_eq!(value.len(), 15);
                assert_eq!(value.read_at(10, &mut out).unwrap(), 5);
                assert_eq!(&out[..5], b"value");
                assert_eq!(value.read_at(15, &mut out).unwrap(), 0);
                assert_eq!(value.read_at(16, &mut out), Err(Error::InvalidArgument));
                seen = true;
                Ok(())
            })
            .unwrap()
    );
    assert!(seen);
    assert!(!inspection.next(|_, _| Ok(())).unwrap());
    inspection.finish().unwrap();
}

#[test]
fn merge_requires_a_syncable_database_and_can_forget_local_only_entries() {
    let (src, dst) = (db(), db());
    put(&src, b"shared", b"s", WALL);
    put(&dst, b"shared", b"s", WALL + 1);
    put(&dst, b"local-only", b"x", WALL + 1);
    let plain = Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap();
    assert!(matches!(prepare(&dst, &src).merge(&plain, Lww, WALL + 2, 65536), Err(Error::Mode)));
    let mirror = |_: &SyncDelta<'_>| Ok(Resolution::UseRemote);
    let (outcome, _) = prepare(&dst, &src)
        .merge(&dst, mirror, WALL + 2, 65536)
        .unwrap()
        .finish_with_delta()
        .unwrap();
    assert_eq!(outcome.applied, 2, "the shared clock and the forgotten key");
    let r = dst.begin_read().unwrap();
    assert!(r.get(b"local-only").unwrap().is_none());
    assert!(r.is_del(b"local-only").unwrap().is_none(), "forgotten, not tombstoned");
    assert_eq!(r.get(b"shared").unwrap().unwrap().as_ref(), b"s");
}
