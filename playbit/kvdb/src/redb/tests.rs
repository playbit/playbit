#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;

#[test]
fn volume_write_epoch_fences_aborts_but_not_reads_or_busy_writers() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let storage = RedbStorage::open(file.path()).unwrap();
    let epoch = Arc::new(WriteEpoch::default());
    storage.bind_write_epoch(epoch.clone()).unwrap();
    storage.bind_write_epoch(epoch.clone()).unwrap();
    assert_eq!(
        storage.bind_write_epoch(Arc::new(WriteEpoch::default())),
        Err(Error::InvalidArgument)
    );
    drop(storage.begin(false).unwrap());
    assert_eq!(epoch.current(), 0);
    assert_eq!(epoch.snapshot(), Ok(0));
    let mut aborted = storage.begin(true).unwrap();
    aborted.put(b"unpublished", b"bytes").unwrap();
    assert!(matches!(storage.begin(true), Err(Error::Busy)));
    assert_eq!(epoch.current(), 1);
    assert_eq!(epoch.snapshot(), Err(Error::Busy));
    drop(aborted);
    assert!(storage.begin(false).unwrap().get(b"unpublished").unwrap().is_none());
    assert_eq!(epoch.current(), 1);
    assert_eq!(epoch.snapshot(), Ok(1));
    let mut committed = storage.begin(true).unwrap();
    committed.put(b"published", b"bytes").unwrap();
    committed.commit().unwrap();
    assert_eq!(epoch.current(), 2);
    assert!(storage.begin(false).unwrap().get(b"published").unwrap().is_some());
}

#[test]
fn exhausted_volume_write_epoch_refuses_before_backend_mutation_and_releases_credit() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let storage = RedbStorage::open(file.path()).unwrap();
    let epoch =
        Arc::new(WriteEpoch { epoch: AtomicU64::new(u64::MAX), active: AtomicUsize::new(0) });
    storage.bind_write_epoch(epoch.clone()).unwrap();
    for _ in 0..2 {
        assert!(matches!(storage.begin(true), Err(Error::Capacity)));
        assert!(!storage.writer_active.load(Ordering::Acquire));
        assert_eq!(epoch.current(), u64::MAX);
        assert_eq!(epoch.snapshot(), Ok(u64::MAX));
        assert!(storage.begin(false).unwrap().get(b"unpublished").unwrap().is_none());
    }
}

// Share the real epoch across two files and hold both backend writers. Neither
// an early commit nor a pinned old read may qualify a partially completed cut.
#[test]
fn volume_snapshot_fence_requires_all_backend_writers_to_finish() {
    let files = [tempfile::NamedTempFile::new().unwrap(), tempfile::NamedTempFile::new().unwrap()];
    let stores = files.map(|file| {
        crate::Db::create(
            RedbStorage::open(file.path()).unwrap(),
            &crate::CreateOptions::syncable([7; 32], 1),
        )
        .unwrap()
    });
    let epoch = Arc::new(WriteEpoch::default());
    for storage in &stores {
        storage.bind_write_epoch(epoch.clone()).unwrap();
    }
    let before = epoch.snapshot().unwrap();
    let pinned = stores[0].begin_read().unwrap();
    let mut first = stores[0].begin_write(crate::clock::EPOCH_UNIX_SECS + 1).unwrap();
    let second = stores[1].begin_write(crate::clock::EPOCH_UNIX_SECS + 1).unwrap();
    first.put(b"new", b"content").unwrap();
    first.local_indexes().put(b"name/new", b"new").unwrap();
    assert_eq!(epoch.snapshot(), Err(Error::Busy));
    first.commit().unwrap();
    assert_eq!(epoch.snapshot(), Err(Error::Busy));
    drop(second);
    assert_ne!(epoch.snapshot().unwrap(), before);
    assert!(pinned.get(b"new").unwrap().is_none());
    assert!(pinned.local_get(b"name/new").unwrap().is_none());
    let current = stores[0].begin_read().unwrap();
    assert!(current.get(b"new").unwrap().is_some());
    assert!(current.local_get(b"name/new").unwrap().is_some());
    assert!(current.clock().unwrap() > pinned.clock().unwrap());
}
#[test]
fn retained_visits_preserve_bounds_errors_and_snapshot_isolation() {
    fn check(storage: &impl Storage) {
        let mut writer = storage.begin(true).unwrap();
        for key in [b"a", b"c", b"e"] {
            writer.put(key, key).unwrap();
        }
        writer.commit().unwrap();
        let snapshot = storage.begin(false).unwrap();
        let mut writer = storage.begin(true).unwrap();
        writer.put(b"c", b"changed").unwrap();
        let mut seen = Vec::new();
        writer
            .visit(b"b", false, false, &mut |key, value| {
                let mut bytes = vec![0; value.len()];
                value.read_exact_at(0, &mut bytes)?;
                seen.push((key.to_vec(), bytes));
                Ok(true)
            })
            .unwrap();
        assert_eq!(
            seen,
            vec![(b"c".to_vec(), b"changed".to_vec()), (b"e".to_vec(), b"e".to_vec())]
        );
        writer.commit().unwrap();
        for (exclusive, reverse, expected) in [
            (false, false, vec![b"c", b"e"]),
            (true, false, vec![b"e"]),
            (false, true, vec![b"c", b"a"]),
            (true, true, vec![b"a"]),
        ] {
            let mut seen = Vec::new();
            snapshot
                .visit(b"c", exclusive, reverse, &mut |key, value| {
                    let mut byte = [0];
                    assert_eq!(value.len(), 1);
                    value.read_exact_at(0, &mut byte)?;
                    assert_eq!(key, byte);
                    seen.push(key.to_vec());
                    Ok(true)
                })
                .unwrap();
            assert_eq!(seen, expected.iter().map(|k| k.to_vec()).collect::<Vec<_>>());
        }
        let mut calls = 0;
        snapshot
            .visit(b"", false, false, &mut |_, _| {
                calls += 1;
                Ok(false)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            snapshot.visit(b"", false, false, &mut |_, _| Err(Error::Storage(91))),
            Err(Error::Storage(91))
        );
        snapshot.visit(b"z", false, false, &mut |_, _| panic!("past end")).unwrap();
        snapshot.visit(b"", false, true, &mut |_, _| panic!("before start")).unwrap();
    }
    check(&crate::storage::mem::MemStorage::new());
    let file = tempfile::NamedTempFile::new().unwrap();
    check(&RedbStorage::open(file.path()).unwrap());
}

#[test]
fn visit_reads_only_requested_ranges_of_large_values() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let storage = RedbStorage::open(file.path()).unwrap();
    let mut writer = storage.begin(true).unwrap();
    let bytes = vec![0x5a; 2 * 1024 * 1024];
    writer.put(b"large", &bytes).unwrap();
    // Exercise the write-table path before commit, then the snapshot path.
    fn check(txn: &impl StorageTxn) {
        txn.visit(b"large", false, false, &mut |key, value| {
            assert_eq!(key, b"large");
            assert_eq!(value.len(), 2 * 1024 * 1024);
            let mut header = [0; 41];
            value.read_exact_at(0, &mut header)?;
            assert_eq!(header, [0x5a; 41]);
            assert_eq!(value.read_at(value.len(), &mut header)?, 0);
            assert_eq!(value.read_at(value.len() + 1, &mut header), Err(Error::InvalidArgument));
            Ok(false)
        })
        .unwrap();
    }
    check(&writer);
    writer.commit().unwrap();
    check(&storage.begin(false).unwrap());
}
use crate::{CreateOptions, Db};

fn tmpfile(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("kvdb-redb-test-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_file(&p);
    p
}

#[derive(Debug)]
struct CountReads {
    file: ::redb::backends::FileBackend,
    large: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl ::redb::StorageBackend for CountReads {
    fn len(&self) -> std::io::Result<u64> {
        self.file.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        if out.len() > 512 * 1024 {
            self.large.fetch_add(1, Ordering::Relaxed);
        }
        self.file.read(offset, out)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write(offset, bytes)
    }
    fn close(&self) -> std::io::Result<()> {
        self.file.close()
    }
}
#[test]
fn pinned_snapshot_reads_one_large_backend_page_and_survives_later_writes() {
    let path = tmpfile("pin-single-read");
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let file =
        std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
    let backend = CountReads {
        file: ::redb::backends::FileBackend::new(file).unwrap(),
        large: count.clone(),
    };
    let mut builder = ::redb::Builder::new();
    builder.set_cache_size(512 * 1024);
    let store = RedbStorage::from_database(builder.create_with_backend(backend).unwrap());
    let pattern = Pattern { len: 4 * 1024 * 1024, largest_read: Default::default() };
    let mut write = store.begin(true).unwrap();
    assert!(matches!(write.snapshot_value(b"key"), Err(Error::Mode)));
    write.put_from(b"key", &pattern).unwrap();
    write.commit().unwrap();
    count.store(0, Ordering::Relaxed);
    let read = store.begin(false).unwrap();
    let pin = read.snapshot_value(b"key").unwrap().unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    drop(read);
    let mut got = [0; 4096];
    let mut want = [0; 4096];
    for offset in (0..pattern.len).step_by(got.len()) {
        pin.read_exact_at(offset, &mut got).unwrap();
        pattern.read_exact_at(offset, &mut want).unwrap();
        assert_eq!(got, want);
    }
    assert_eq!(count.load(Ordering::Relaxed), 1, "fragment reads reuse the actual page pin");
    let mut write = store.begin(true).unwrap();
    write.put(b"key", b"replacement").unwrap();
    write.commit().unwrap();
    pin.read_exact_at(8192, &mut got).unwrap();
    pattern.read_exact_at(8192, &mut want).unwrap();
    assert_eq!(got, want);
    drop(pin);
    drop(store);
    std::fs::remove_file(path).unwrap();
}
struct Pattern {
    len: usize,
    largest_read: std::cell::Cell<usize>,
}
impl ValueRead for Pattern {
    fn len(&self) -> usize {
        self.len
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        if offset > self.len {
            return Err(Error::InvalidArgument);
        }
        let n = out.len().min(self.len - offset);
        self.largest_read.set(self.largest_read.get().max(n));
        for (i, v) in out[..n].iter_mut().enumerate() {
            *v = ((offset + i) % 251) as u8;
        }
        Ok(n)
    }
}
fn stage(name: &str) -> FileStaging {
    use std::os::unix::fs::OpenOptionsExt;
    let path = tmpfile(name);
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    std::fs::remove_file(path).unwrap();
    FileStaging::new(file).unwrap()
}
#[test]
fn maximum_legal_value_streams_through_file_staging_with_bounded_reads() {
    use crate::sync::{Lww, SyncReceiver, SyncSender, SyncStatus};
    let source_path = tmpfile("stream-large-source");
    let target_path = tmpfile("stream-large-target");
    let open = |path: &std::path::Path| {
        let mut builder = ::redb::Builder::new();
        builder.set_cache_size(512 * 1024);
        Db::create(
            RedbStorage::from_database(builder.create(path).unwrap()),
            &CreateOptions::syncable([71; 32], 1),
        )
        .unwrap()
    };
    let source = open(&source_path);
    let target = open(&target_path);
    let key = vec![b'k'; crate::KEY_SIZE_MAX];
    let value = Pattern { len: crate::VALUE_SIZE_MAX, largest_read: Default::default() };
    let mut w = source.begin_write(1_800_000_000).unwrap();
    w.put_from(&key, &value).unwrap();
    w.commit().unwrap();
    assert!(value.largest_read.get() <= 4096);
    let mut request = Vec::new();
    let mut rx =
        SyncReceiver::new(&target, Lww, stage("stream-large-stage"), 1_800_000_001, &mut request)
            .unwrap();
    let mut tx = SyncSender::new(&source).unwrap();
    let mut chunk = vec![0; 256 * 1024];
    let mut done = false;
    let mut bytes = 0;
    while !done {
        assert!(!request.is_empty());
        tx.process(&std::mem::take(&mut request)).unwrap();
        loop {
            let n = tx.write_output(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            bytes += n;
            done = rx.process(&chunk[..n], &mut request).unwrap() == SyncStatus::Done;
            assert!(request.capacity() <= 1024);
        }
    }
    tx.process(&request).unwrap();
    assert_eq!(rx.finish().unwrap().applied, 1);
    assert!(bytes > crate::VALUE_SIZE_MAX);
    let read = target.begin_read().unwrap();
    assert_eq!(read.value_len(&key).unwrap(), Some(value.len));
    let pinned = read.value_reader(&key).unwrap().unwrap();
    let mut got = [0; 4096];
    let mut want = [0; 4096];
    let mut offset = 0;
    while offset < value.len {
        let n = value.read_at(offset, &mut want).unwrap();
        assert_eq!(pinned.read_at(offset, &mut got).unwrap(), n);
        assert_eq!(&got[..n], &want[..n]);
        offset += n;
    }
    assert_eq!(read.root_hash().unwrap(), source.begin_read().unwrap().root_hash().unwrap());
    drop(pinned);
    drop(read);
    drop(tx);
    drop(source);
    drop(target);
    std::fs::remove_file(source_path).unwrap();
    std::fs::remove_file(target_path).unwrap();
}
#[test]
fn maximum_legal_value_prepared_sync_uses_bounded_journal_reads() {
    use crate::sync::{Lww, PreparedSync, SyncSender, SyncStaging, SyncStatus};
    struct Checked(FileStaging);
    impl ValueRead for Checked {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
            assert!(out.len() <= 4096);
            self.0.read_at(offset, out)
        }
    }
    impl SyncStaging for Checked {
        fn clear(&mut self) -> Result<()> {
            self.0.clear()
        }
        fn append(&mut self, bytes: &[u8]) -> Result<()> {
            assert!(bytes.len() <= 4096);
            self.0.append(bytes)
        }
    }
    let source_path = tmpfile("prepared-large-source");
    let target_path = tmpfile("prepared-large-target");
    let open = |path: &std::path::Path| {
        let mut builder = ::redb::Builder::new();
        builder.set_cache_size(512 * 1024);
        Db::create(
            RedbStorage::from_database(builder.create(path).unwrap()),
            &CreateOptions::syncable([71; 32], 1),
        )
        .unwrap()
    };
    let source = open(&source_path);
    let target = open(&target_path);
    let key = vec![b'k'; crate::KEY_SIZE_MAX];
    let value = Pattern { len: crate::VALUE_SIZE_MAX, largest_read: Default::default() };
    let mut w = source.begin_write(1_800_000_000).unwrap();
    w.put_from(&key, &value).unwrap();
    w.commit().unwrap();
    assert!(value.largest_read.get() <= 4096);
    let mut request = Vec::new();
    let journal = Checked(
        FileStaging::with_capacity(stage("prepared-large-journal").file, 128 * 1024 * 1024)
            .unwrap(),
    );
    let mut rx = PreparedSync::new(
        &target,
        stage("prepared-large-stage"),
        journal,
        128 * 1024 * 1024,
        &mut request,
    )
    .unwrap();
    let mut tx = SyncSender::new(&source).unwrap();
    let mut chunk = vec![0; 256 * 1024];
    let mut done = false;
    let mut bytes = 0;
    while !done {
        assert!(!request.is_empty());
        tx.process(&std::mem::take(&mut request)).unwrap();
        loop {
            let n = tx.write_output(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            bytes += n;
            done = rx.process(&chunk[..n], &mut request).unwrap() == SyncStatus::Done;
            assert!(request.capacity() <= 1024);
        }
    }
    tx.process(&request).unwrap();
    // Real redb writer admission remains available for the whole receive.
    let mut concurrent = target.begin_write(1_800_000_002).unwrap();
    concurrent.put(b"local-progress", b"ok").unwrap();
    concurrent.commit().unwrap();
    let changes = rx.finish().unwrap();
    let merge = changes.merge(&target, Lww, 1_800_000_003, 65536).unwrap();
    let (outcome, delta) = merge.finish_with_delta().unwrap();
    assert_eq!(outcome.applied, 1);
    assert!(matches!(delta, Some(crate::sync::CommittedDelta::PullRequired)));
    assert!(bytes > crate::VALUE_SIZE_MAX);
    let read = target.begin_read().unwrap();
    assert_eq!(read.value_len(&key).unwrap(), Some(value.len));
    let pinned = read.value_reader(&key).unwrap().unwrap();
    let mut got = [0; 4096];
    let mut want = [0; 4096];
    let mut offset = 0;
    while offset < value.len {
        let n = value.read_at(offset, &mut want).unwrap();
        assert_eq!(pinned.read_at(offset, &mut got).unwrap(), n);
        assert_eq!(&got[..n], &want[..n]);
        offset += n;
    }
    assert_eq!(&*read.get(b"local-progress").unwrap().unwrap(), b"ok");
    drop(pinned);
    drop(read);
    drop(tx);
    drop(source);
    drop(target);
    std::fs::remove_file(source_path).unwrap();
    std::fs::remove_file(target_path).unwrap();
}
#[test]
fn file_staging_explicit_journal_capacity_is_enforced_before_append() {
    use crate::sync::SyncStaging;
    let mut journal = FileStaging::with_capacity(stage("stage-capacity").file, 17).unwrap();
    journal.append(&[7; 17]).unwrap();
    assert_eq!(journal.append(&[8]), Err(Error::Capacity));
    assert_eq!(journal.len(), 17);
    assert_eq!(journal.file.metadata().unwrap().len(), 17);
    journal.clear().unwrap();
    assert_eq!(journal.append(&[8; 18]), Err(Error::Capacity));
    assert_eq!(journal.len(), 0);
    journal.append(&[9; 3]).unwrap();
    assert_eq!(journal.len(), 3);
    assert_eq!(journal.file.metadata().unwrap().len(), 17);
}
#[test]
fn reused_staging_never_exposes_previous_entry_tail() {
    use crate::sync::SyncStaging;
    let mut staging = stage("stage-reuse");
    let mut high_water = 0;
    for (generation, length) in [8192, 1, 4095, 4096, 16384, 0, 31].into_iter().enumerate() {
        staging.clear().unwrap();
        let mut output = [0xa5; 257];
        assert_eq!(staging.len(), 0);
        assert_eq!(staging.read_at(0, &mut output).unwrap(), 0);
        assert_eq!(output, [0xa5; 257]);
        let value = vec![generation as u8; length];
        for fragment in value.chunks(137) {
            staging.append(fragment).unwrap();
        }
        assert_eq!(staging.len(), length);
        high_water = high_water.max(length);
        assert_eq!(staging.file.metadata().unwrap().len(), high_water as u64);
        let mut offset = 0;
        while offset < length {
            output.fill(0xa5);
            let count = staging.read_at(offset, &mut output).unwrap();
            assert!(count > 0 && count <= (length - offset).min(output.len()));
            assert_eq!(&output[..count], &value[offset..offset + count]);
            assert!(output[count..].iter().all(|&byte| byte == 0xa5));
            offset += count;
        }
        assert_eq!(staging.read_at(length, &mut output).unwrap(), 0);
        assert_eq!(staging.read_at(length + 1, &mut output), Err(Error::InvalidArgument));
    }
}

#[test]
fn failed_staging_append_does_not_advance_logical_length() {
    use crate::sync::SyncStaging;
    let path = tmpfile("stage-readonly");
    std::fs::write(&path, b"ok old tail").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    // A read-only descriptor deterministically fails writes without
    // modifying the existing bytes; the logical state must stay intact.
    let mut staging = FileStaging { file, len: 2, capacity: crate::sync::MAX_FRAME };
    assert_eq!(staging.append(b"new"), Err(Error::Storage(8)));
    assert_eq!(staging.len(), 2);
    let mut output = [0xa5; 16];
    assert_eq!(staging.read_at(0, &mut output).unwrap(), 2);
    assert_eq!(&output[..2], b"ok");
    assert_eq!(&output[2..], &[0xa5; 14]);
    staging.clear().unwrap();
    assert_eq!(staging.append(b"new"), Err(Error::Storage(8)));
    assert_eq!(staging.len(), 0);
    assert_eq!(staging.read_at(0, &mut output).unwrap(), 0);
}

#[test]
fn file_staging_clear_and_failed_streamed_write_do_not_publish_partial_bytes() {
    use crate::sync::SyncStaging;
    let mut staging = stage("stage-reset");
    staging.append(b"longer value").unwrap();
    staging.clear().unwrap();
    staging.append(b"ok").unwrap();
    let mut buf = [0; 20];
    assert_eq!(staging.read_at(0, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"ok");
    assert!(staging.read_at(3, &mut buf).is_err());
    struct Fails;
    impl ValueRead for Fails {
        fn len(&self) -> usize {
            9000
        }
        fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
            if offset != 0 {
                return Err(Error::Storage(91));
            }
            out.fill(7);
            Ok(out.len())
        }
    }
    let path = tmpfile("failed-reservation");
    let store = RedbStorage::open(&path).unwrap();
    let mut tx = store.begin(true).unwrap();
    tx.put(b"safe", b"prior").unwrap();
    tx.commit().unwrap();
    let mut tx = store.begin(true).unwrap();
    assert_eq!(tx.put_from(b"unsafe", &Fails), Err(Error::Storage(91)));
    drop(tx);
    let read = store.begin(false).unwrap();
    assert!(read.get(b"unsafe").unwrap().is_none());
    assert_eq!(read.get(b"safe").unwrap().as_deref(), Some(&b"prior"[..]));
    drop(read);
    drop(store);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn contract_basics() {
    let path = tmpfile("contract");
    let s = RedbStorage::open(&path).unwrap();

    // reads on a fresh database see nothing
    let r = s.begin(false).unwrap();
    assert!(r.get(b"a").unwrap().is_none());
    assert!(r.next_from(b"", false).unwrap().is_none());
    r.visit(b"", false, false, &mut |_, _| panic!("nothing to visit")).unwrap();
    drop(r);

    let mut w = s.begin(true).unwrap();
    w.put(b"a", b"1").unwrap();
    w.put(b"c", b"3").unwrap();
    assert_eq!(w.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    let r = s.begin(false).unwrap();
    assert!(r.get(b"a").unwrap().is_none()); // uncommitted invisible
    assert!(matches!(s.begin(true), Err(Error::Busy)));
    w.commit().unwrap();
    assert!(r.get(b"a").unwrap().is_none()); // snapshot stable
    drop(r);

    {
        let r = s.begin(false).unwrap();
        let (k, _) = r.next_from(b"b", false).unwrap().unwrap();
        assert_eq!(&*k, b"c");
        let (k, _) = r.prev_from(b"b", false).unwrap().unwrap();
        assert_eq!(&*k, b"a");
        assert!(r.next_from(b"c", true).unwrap().is_none());
    }

    // abort releases the writer and discards changes
    let mut w = s.begin(true).unwrap();
    w.put(b"x", b"9").unwrap();
    drop(w);
    let mut w = s.begin(true).unwrap();
    assert!(w.get(b"x").unwrap().is_none());
    assert!(w.del(b"a").unwrap());
    w.commit().unwrap();

    let _ = std::fs::remove_file(&path);
}

#[test]
fn key_value_and_range_views_agree_before_and_after_commit() {
    fn check(tx: &impl StorageTxn, rows: &[(&[u8], &[u8])]) {
        for &(key, value) in rows {
            assert_eq!(tx.value_len(key).unwrap(), Some(value.len()));
            let mut output = vec![0xa5; value.len() + 3];
            assert_eq!(tx.read_value_at(key, 0, &mut output).unwrap(), Some(value.len()));
            assert_eq!(&output[..value.len()], value);
            assert_eq!(&output[value.len()..], &[0xa5; 3]);
            assert_eq!(tx.read_value_at(key, value.len(), &mut output).unwrap(), Some(0));
            assert_eq!(
                tx.read_value_at(key, value.len() + 1, &mut output),
                Err(Error::InvalidArgument)
            );
            assert_eq!(tx.get(key).unwrap().as_deref(), Some(value));
        }
        for (index, &(key, value)) in rows.iter().enumerate() {
            assert_eq!(tx.next_key(key, false).unwrap().as_deref(), Some(key));
            assert_eq!(tx.prev_key(key, false).unwrap().as_deref(), Some(key));
            assert_eq!(
                tx.next_key(key, true).unwrap().as_deref(),
                rows.get(index + 1).map(|r| r.0)
            );
            assert_eq!(
                tx.prev_key(key, true).unwrap().as_deref(),
                index.checked_sub(1).map(|i| rows[i].0)
            );
            let (next, next_value) = tx.next_from(key, false).unwrap().unwrap();
            assert_eq!((&*next, &*next_value), (key, value));
            let (prev, prev_value) = tx.prev_from(key, false).unwrap().unwrap();
            assert_eq!((&*prev, &*prev_value), (key, value));
        }
    }
    let path = tmpfile("consistent-views");
    let store = RedbStorage::open(&path).unwrap();
    let empty = store.begin(false).unwrap();
    assert_eq!(empty.value_len(b"missing").unwrap(), None);
    assert_eq!(empty.read_value_at(b"missing", 0, &mut [0; 1]).unwrap(), None);
    assert!(empty.snapshot_value(b"missing").unwrap().is_none());
    assert!(empty.next_key(b"", false).unwrap().is_none());
    assert!(empty.prev_key(b"z", false).unwrap().is_none());
    let rows: &[(&[u8], &[u8])] = &[(b"a", b""), (b"b", b"one"), (b"c", b"three")];
    let mut write = store.begin(true).unwrap();
    assert!(matches!(write.snapshot_value(b"a"), Err(Error::Mode)));
    for &(key, value) in rows {
        write.put(key, value).unwrap();
    }
    check(&write, rows);
    write.commit().unwrap();
    assert!(empty.next_key(b"", false).unwrap().is_none());
    let retained = store.begin(false).unwrap();
    check(&retained, rows);
    let pin = retained.snapshot_value(b"b").unwrap().unwrap();
    let mut write = store.begin(true).unwrap();
    write.del(b"a").unwrap();
    write.put(b"b", b"changed").unwrap();
    check(&write, &[(b"b", b"changed"), (b"c", b"three")]);
    write.commit().unwrap();
    check(&retained, rows);
    let mut old = [0; 3];
    pin.read_exact_at(0, &mut old).unwrap();
    assert_eq!(&old, b"one");
    check(&store.begin(false).unwrap(), &[(b"b", b"changed"), (b"c", b"three")]);
    drop(pin);
    drop(retained);
    drop(empty);
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn props_and_error_paths() {
    let path = tmpfile("errors");
    let s = RedbStorage::open(&path).unwrap();
    let p = s.props();
    assert_eq!(p.retention, Retention::Persistent);
    assert!(p.durable_program_crash);
    assert_eq!(p.durable_power_loss, !cfg!(target_vendor = "apple"));

    // a second open of a locked database file fails cleanly
    assert!(matches!(RedbStorage::open(&path), Err(Error::Storage(_))));

    // read transactions reject mutation and commit
    let mut r = s.begin(false).unwrap();
    assert!(matches!(r.put(b"k", b"v"), Err(Error::ReadOnly)));
    assert!(matches!(r.del(b"k"), Err(Error::ReadOnly)));
    let r2 = s.begin(false).unwrap();
    assert!(matches!(r2.commit(), Err(Error::ReadOnly)));
    drop(r);

    // write-transaction reads before the table exists see nothing
    let w = s.begin(true).unwrap();
    assert!(w.get(b"k").unwrap().is_none());
    assert!(w.next_from(b"", false).unwrap().is_none());
    assert!(w.prev_from(b"\xff", false).unwrap().is_none());
    drop(w);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn error_mappers() {
    let io = || ::redb::StorageError::Io(std::io::Error::other("x"));
    assert!(matches!(store_err(io()), Error::Storage(5)));
    assert!(matches!(txn_err(::redb::TransactionError::Storage(io())), Error::Storage(2)));
    assert!(matches!(commit_err(::redb::CommitError::Storage(io())), Error::Storage(6)));
    assert!(matches!(table_err(&::redb::TableError::Storage(io())), Error::Storage(3)));
    assert!(matches!(
        table_err(&::redb::TableError::TableDoesNotExist("t".to_string())),
        Error::Storage(4)
    ));
    assert!(matches!(db_err(::redb::DatabaseError::Storage(io())), Error::Storage(1)));
}

#[test]
fn mismatched_table_type_is_reported() {
    let path = tmpfile("mismatch");
    {
        // create a table named "kvdb" with a different type signature
        let db = ::redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        let bad: ::redb::TableDefinition<'_, u64, u64> = ::redb::TableDefinition::new("kvdb");
        txn.open_table(bad).unwrap();
        txn.commit().unwrap();
    }
    let s = RedbStorage::from_database(::redb::Database::create(&path).unwrap());
    assert!(matches!(s.begin(false), Err(Error::Storage(_))));
    assert!(s.begin(true).is_ok()); // typed open fails at first table use
    let w = s.begin(true).unwrap();
    assert!(matches!(w.get(b"k"), Err(Error::Storage(_))));
    assert!(matches!(w.next_from(b"", false), Err(Error::Storage(_))));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn shared_db_serves_readers_during_a_write_and_keeps_clocks_increasing() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Db<RedbStorage>>();

    let path = tmpfile("shared");
    let db = std::sync::Arc::new(
        Db::create(RedbStorage::open(&path).unwrap(), &CreateOptions::syncable([3; 32], 1))
            .unwrap(),
    );
    let mut w = db.begin_write(1_800_000_000).unwrap();
    w.put(b"seed", b"0").unwrap();
    w.commit().unwrap();

    // A reader on another thread completes while this thread holds the
    // writer with uncommitted changes, and sees only committed state.
    let mut w = db.begin_write(1_800_000_001).unwrap();
    w.put(b"seed", b"1").unwrap();
    let reader = {
        let db = db.clone();
        std::thread::spawn(move || {
            db.begin_read().unwrap().get(b"seed").unwrap().map(|v| v.to_vec())
        })
    };
    assert_eq!(reader.join().unwrap(), Some(b"0".to_vec()));
    w.commit().unwrap();

    // Writers on several threads take turns (a losing writer sees Busy
    // and retries); every committed clock is distinct and increasing.
    let clocks = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for thread in 0..4u64 {
            let db = &db;
            let clocks = &clocks;
            scope.spawn(move || {
                for i in 0..25u64 {
                    let mut w = loop {
                        match db.begin_write(1_800_000_002 + i) {
                            Err(Error::Busy) => std::thread::yield_now(),
                            other => break other.unwrap(),
                        }
                    };
                    w.put(format!("t{thread}-{i}").as_bytes(), b"v").unwrap();
                    let clock = w.clock();
                    w.commit().unwrap();
                    clocks.lock().unwrap().push(clock);
                }
            });
        }
    });
    let mut clocks = clocks.into_inner().unwrap();
    assert_eq!(clocks.len(), 100);
    let latest = *clocks.iter().max().unwrap();
    clocks.sort();
    clocks.dedup();
    assert_eq!(clocks.len(), 100, "every commit needs its own clock");
    assert_eq!(db.clock(), latest);
    assert_eq!(db.begin_read().unwrap().iter().count(), 101);
    let _ = std::fs::remove_file(&path);
}

/// A point at which one armed commit stops, after redb has made it
/// durable and before `Db` learns that it succeeded, until the test
/// lets it continue. Commits that were not armed pass straight through.
#[derive(Default)]
struct Pause {
    armed: std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl Pause {
    /// Make the next commit stop here. Returns the channel that reports
    /// the stop and the one that ends it.
    fn arm(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_rx) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        *self.armed.lock().unwrap() = Some((reached, resume_rx));
        (reached_rx, resume)
    }

    fn hold(&self) {
        // Taken out before waiting, so commits that pass through
        // meanwhile do not queue behind the held one.
        let armed = self.armed.lock().unwrap().take();
        if let Some((reached, resume)) = armed {
            reached.send(()).unwrap();
            // A dropped sender (the test failed) lets the commit finish.
            let _ = resume.recv();
        }
    }
}

/// redb storage whose commits can be paused between the durable write
/// and the report back to `Db`.
struct PausingStorage {
    inner: RedbStorage,
    after_commit: std::sync::Arc<Pause>,
}

struct PausingTxn<'db> {
    inner: RedbTxn<'db>,
    after_commit: &'db Pause,
}

impl Storage for PausingStorage {
    type Txn<'db> = PausingTxn<'db>;
    fn begin(&self, write: bool) -> Result<PausingTxn<'_>> {
        Ok(PausingTxn { inner: self.inner.begin(write)?, after_commit: &self.after_commit })
    }
    fn props(&self) -> StorageProps {
        self.inner.props()
    }
}

impl StorageTxn for PausingTxn<'_> {
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        self.inner.visit(key, exclusive, reverse, visitor)
    }
    type ValueReader = RedbValueReader;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<Self::ValueReader>> {
        let _ = key;
        self.inner.snapshot_value(key)
    }
    type Guard<'g>
        = Guard<'g>
    where
        Self: 'g;
    fn get<'g>(&'g self, key: &[u8]) -> Result<Option<Guard<'g>>> {
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
    fn next_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(Guard<'g>, Guard<'g>)>> {
        self.inner.next_from(key, excl)
    }
    fn prev_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(Guard<'g>, Guard<'g>)>> {
        self.inner.prev_from(key, excl)
    }
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.inner.put(key, val)
    }
    fn del(&mut self, key: &[u8]) -> Result<bool> {
        self.inner.del(key)
    }
    fn commit(self) -> Result<()> {
        self.inner.commit()?;
        self.after_commit.hold();
        Ok(())
    }
}

#[test]
fn a_clock_is_published_after_its_commit_and_never_moves_backwards() {
    const T: u64 = 1_800_000_000;
    let path = tmpfile("paused");
    let pause = std::sync::Arc::new(Pause::default());
    let storage =
        PausingStorage { inner: RedbStorage::open(&path).unwrap(), after_commit: pause.clone() };
    let db = Db::create(storage, &CreateOptions::syncable([4; 32], 2)).unwrap();
    assert_eq!(db.props().retention, Retention::Persistent);
    let mut w = db.begin_write(T).unwrap();
    w.put(b"a", b"1").unwrap();
    w.put(b"gone", b"x").unwrap();
    let c1 = w.clock();
    w.commit().unwrap();
    assert_eq!(db.clock(), c1);

    let bound = std::time::Duration::from_secs(10);
    let c3 = std::thread::scope(|scope| {
        let (reached, resume) = pause.arm();
        let first = scope.spawn(|| {
            let mut w = db.begin_write(T + 1).unwrap();
            w.put(b"a", b"2").unwrap();
            assert!(w.del_forget(b"gone").unwrap());
            let clock = w.clock();
            w.commit().unwrap();
            clock
        });
        reached.recv_timeout(bound).expect("the first commit reaches storage");

        // Storage holds the first commit, but `Db` has not published its
        // clock yet: readers see the committed data, `clock()` still
        // reports the previous commit, and a writer that starts in this
        // gap continues from the committed clock rather than the stale
        // published one.
        let read = db.begin_read().unwrap();
        assert_eq!(read.get(b"a").unwrap().as_deref(), Some(&b"2"[..]));
        assert!(read.get(b"gone").unwrap().is_none());
        assert_eq!(read.iter().count(), 1);
        drop(read);
        assert_eq!(db.clock(), c1);
        let mut w = db.begin_write(T + 1).unwrap();
        w.put(b"b", b"3").unwrap();
        let c3 = w.clock();
        w.commit().unwrap();
        assert_eq!(db.clock(), c3);

        resume.send(()).unwrap();
        let c2 = first.join().unwrap();
        assert!(c1 < c2 && c2 < c3, "every commit needs its own, later clock");
        assert_eq!(db.clock(), c3, "a late publication must not move the clock backwards");
        c3
    });

    let db = Db::open(db.close(), &Default::default()).unwrap();
    assert_eq!(db.clock(), c3);
    let _ = std::fs::remove_file(&path);
}

/// Drive one receiver session to completion against an in-memory sender.
fn pump<S: Storage, T: Storage>(dst: &Db<S>, src: &Db<T>, wall: u64) -> crate::sync::SyncOutcome {
    use crate::sync::{Lww, SyncReceiver, SyncSender, SyncStatus};
    let mut r_out = Vec::new();
    let path = tmpfile("sync-stage");
    let file =
        std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let mut recv =
        SyncReceiver::new(dst, Lww, FileStaging::new(file).unwrap(), wall, &mut r_out).unwrap();
    let mut send = SyncSender::new(src).unwrap();
    let mut s_out = Vec::new();
    let (mut r_done, mut s_done) = (false, false);
    while !r_done || !s_done {
        if !r_out.is_empty() {
            s_done = send.process(&std::mem::take(&mut r_out)).unwrap() == SyncStatus::Done;
            let mut chunk = [0; 4096];
            loop {
                let n = send.write_output(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                s_out.extend_from_slice(&chunk[..n]);
            }
        } else if !s_out.is_empty() {
            r_done =
                recv.process(&std::mem::take(&mut s_out), &mut r_out).unwrap() == SyncStatus::Done;
        } else {
            panic!("sync stalled");
        }
    }
    recv.finish().unwrap()
}

#[test]
fn a_merge_started_before_a_commit_is_published_still_commits_a_later_clock() {
    use crate::storage::mem::MemStorage;
    const T: u64 = 1_800_000_000;
    let path = tmpfile("merge-clock");
    let pause = std::sync::Arc::new(Pause::default());
    let storage =
        PausingStorage { inner: RedbStorage::open(&path).unwrap(), after_commit: pause.clone() };
    let db = Db::create(storage, &CreateOptions::syncable([5; 32], 3)).unwrap();
    let mut w = db.begin_write(T).unwrap();
    w.put(b"seed", b"0").unwrap();
    w.commit().unwrap();
    // A peer with one entry of its own, stamped well before this replica's clocks.
    let peer = Db::create(MemStorage::new(), &CreateOptions::syncable([5; 32], 4)).unwrap();
    let mut w = peer.begin_write(T - 3600).unwrap();
    w.put(b"remote", b"r").unwrap();
    w.commit().unwrap();

    let (merged, committed) = std::thread::scope(|scope| {
        let (reached, resume) = pause.arm();
        // A local write an hour later in wall time: its commit reaches
        // storage, then pauses before its clock is published.
        let writer = scope.spawn(|| {
            let mut w = db.begin_write(T + 3600).unwrap();
            w.put(b"local", b"l").unwrap();
            let clock = w.clock();
            w.commit().unwrap();
            clock
        });
        reached
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the write reaches storage");
        // A merge session starts in that gap, with an older wall reading.
        // Its transaction clock comes from the committed clock in
        // storage, not from the not-yet-published one, so the clock it
        // commits stays above the write it follows.
        let outcome = pump(&db, &peer, T);
        resume.send(()).unwrap();
        (outcome, writer.join().unwrap())
    });
    assert_eq!(merged.applied, 1);
    assert!(merged.committed);
    let reopened = Db::open(db.close(), &Default::default()).unwrap();
    assert!(
        reopened.clock() > committed,
        "merge clock {:?} must follow the committed clock {committed:?}",
        reopened.clock()
    );
    let read = reopened.begin_read().unwrap();
    assert!(read.get(b"local").unwrap().is_some());
    assert!(read.get(b"remote").unwrap().is_some());
    drop(read);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn full_db_on_redb_with_reopen() {
    let path = tmpfile("db");
    {
        let s = RedbStorage::open(&path).unwrap();
        let db = Db::create(s, &CreateOptions::syncable([9; 32], 77)).unwrap();
        let mut w = db.begin_write(1_800_000_000).unwrap();
        for i in 0..100u32 {
            w.put(format!("key{i:03}").as_bytes(), format!("val{i}").as_bytes()).unwrap();
        }
        w.del(b"key050").unwrap();
        w.commit().unwrap();
    }
    {
        // reopen from disk: everything survives
        let s = RedbStorage::open(&path).unwrap();
        let db = Db::open(s, &Default::default()).unwrap();
        assert_eq!(db.meta().replica_id, 77);
        assert!(!db.clock().is_zero());
        let r = db.begin_read().unwrap();
        assert_eq!(r.iter().count(), 99);
        assert_eq!(r.get(b"key007").unwrap().as_deref(), Some(&b"val7"[..]));
        assert!(r.is_del(b"key050").unwrap().is_some());
        assert!(r.root_hash().unwrap().is_some());
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn local_secondary_records_survive_reopen_and_aborted_renames_leave_no_alias() {
    // Reopen the real durable backend after both committed and abandoned writes.
    // Source and derived names must describe the same publication boundary.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.kvdb");
    let wall = crate::clock::EPOCH_UNIX_SECS + 7000;
    {
        let db =
            Db::create(RedbStorage::open(&path).unwrap(), &CreateOptions::syncable([9; 32], 77))
                .unwrap();
        let mut tx = db.begin_write(wall).unwrap();
        tx.put(b"entry/10", b"notes").unwrap();
        tx.local_indexes().put(b"name/notes", b"10").unwrap();
        tx.commit().unwrap();
        let mut tx = db.begin_write(wall + 1).unwrap();
        tx.put(b"entry/10", b"abandoned").unwrap();
        tx.local_indexes().del(b"name/notes").unwrap();
        tx.local_indexes().put(b"name/abandoned", b"10").unwrap();
    }
    for rename in [true, false] {
        let db = Db::open(
            RedbStorage::open(&path).unwrap(),
            &crate::OpenOptions { verify_on_open: true },
        )
        .unwrap();
        let read = db.begin_read().unwrap();
        assert!(read.local_get(b"name/abandoned").unwrap().is_none());
        let name = if rename { b"notes".as_slice() } else { b"work".as_slice() };
        assert_eq!(read.get(b"entry/10").unwrap().unwrap().as_ref(), name);
        assert_eq!(
            read.local_get(&[b"name/".as_slice(), name].concat()).unwrap().unwrap().as_ref(),
            b"10"
        );
        if rename {
            let mut tx = db.begin_write(wall + 2).unwrap();
            tx.put(b"entry/10", b"work").unwrap();
            tx.local_indexes().del(b"name/notes").unwrap();
            tx.local_indexes().put(b"name/work", b"10").unwrap();
            tx.commit().unwrap();
            assert_eq!(read.local_get(b"name/notes").unwrap().unwrap().as_ref(), b"10");
        } else {
            assert!(read.local_get(b"name/notes").unwrap().is_none());
        }
    }
}
