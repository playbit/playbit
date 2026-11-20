//! Run explicitly on a quiet reference device; no network or renderer is modeled.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::{
    CreateOptions, Db,
    sync::{Lww, PreparedSync, SyncReceiver, SyncSender, SyncStaging, SyncStatus},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
fn path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "kvdb-prepared-bench-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn scratch(capacity: usize) -> FileStaging {
    use std::os::unix::fs::OpenOptionsExt;
    let path = path();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    std::fs::remove_file(path).unwrap();
    FileStaging::with_capacity(file, capacity).unwrap()
}
struct Fixture {
    db: Db<RedbStorage>,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::measured(None)
    }
    fn measured(metrics: Option<Arc<IoMetrics>>) -> Self {
        Self::measured_growth(metrics, false)
    }
    fn measured_growth(metrics: Option<Arc<IoMetrics>>, initialize_growth: bool) -> Self {
        use std::os::unix::fs::OpenOptionsExt;
        let path = path();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let mut builder = ::redb::Builder::new();
        builder.set_cache_size(512 * 1024);
        let backend = FileBackend::new(file).unwrap();
        let database = if let Some(metrics) = metrics {
            builder
                .create_with_backend(MeasuredBackend { inner: backend, metrics, initialize_growth })
                .unwrap()
        } else {
            builder.create_with_backend(backend).unwrap()
        };
        let storage = RedbStorage::from_database(database);
        let db = Db::create(storage, &CreateOptions::syncable([49; 32], 1)).unwrap();
        Self { db, path }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
struct Journal {
    staging: FileStaging,
    bytes: Arc<AtomicUsize>,
}
impl ValueRead for Journal {
    fn len(&self) -> usize {
        self.staging.len()
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        self.staging.read_at(offset, out)
    }
}
impl SyncStaging for Journal {
    fn clear(&mut self) -> Result<()> {
        self.staging.clear()
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.staging.append(bytes)?;
        self.bytes.store(self.staging.len(), Ordering::Relaxed);
        Ok(())
    }
}
fn populate(db: &Db<RedbStorage>, records: usize, payload: usize, generation: usize) {
    let mut tx = db.begin_write(1_800_000_000 + generation as u64).unwrap();
    for i in 0..records {
        let n = payload / records + usize::from(i < payload % records);
        let value = vec![(generation % 251) as u8; n];
        tx.put(&(i as u64).to_be_bytes(), &value).unwrap();
    }
    tx.commit().unwrap();
}
#[derive(Default)]
struct Measurements {
    files: Vec<u64>,
    snapshot: Vec<u64>,
    receive: Vec<u64>,
    receiver_process: Vec<u64>,
    merge: Vec<u64>,
    endpoint: Vec<u64>,
    journal: Vec<u64>,
}
fn ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().try_into().unwrap()
}
fn transfer(source: &Db<RedbStorage>, target: &Db<RedbStorage>, m: &mut Measurements) {
    let bytes = Arc::new(AtomicUsize::new(0));
    let files = Instant::now();
    let entry = scratch(crate::sync::MAX_FRAME);
    let journal = Journal { staging: scratch(128 * 1024 * 1024), bytes: bytes.clone() };
    m.files.push(ns(files.elapsed()));
    let mut request = Vec::new();
    let start = Instant::now();
    let mut rx =
        PreparedSync::new(target, entry, journal, 128 * 1024 * 1024, &mut request).unwrap();
    m.snapshot.push(ns(start.elapsed()));
    let mut tx = SyncSender::new(source).unwrap();
    let mut fragment = vec![0; 256 * 1024];
    let start = Instant::now();
    let mut receiver_ns = 0;
    let mut done = false;
    while !done {
        assert!(!request.is_empty());
        tx.process(&request).unwrap();
        request.clear();
        loop {
            let n = tx.write_output(&mut fragment).unwrap();
            if n == 0 {
                break;
            }
            let process = Instant::now();
            done = rx.process(&fragment[..n], &mut request).unwrap() == SyncStatus::Done;
            receiver_ns += ns(process.elapsed());
        }
    }
    tx.process(&request).unwrap();
    let finish = Instant::now();
    let changes = rx.finish().unwrap();
    receiver_ns += ns(finish.elapsed());
    m.receive.push(ns(start.elapsed()));
    m.receiver_process.push(receiver_ns);
    m.journal.push(bytes.load(Ordering::Relaxed) as u64);
    let start = Instant::now();
    let merge = changes.merge(target, Lww, 1_800_001_000, 65536).unwrap();
    merge.finish_with_delta().unwrap();
    let merge_ns = ns(start.elapsed());
    m.merge.push(merge_ns);
    m.endpoint.push(m.files.last().unwrap() + m.snapshot.last().unwrap() + receiver_ns + merge_ns);
    assert_eq!(
        source.begin_read().unwrap().root_hash().unwrap(),
        target.begin_read().unwrap().root_hash().unwrap()
    );
}
fn transfer_immediate(source: &Db<RedbStorage>, target: &Db<RedbStorage>, m: &mut Measurements) {
    let files = Instant::now();
    let entry = scratch(crate::sync::MAX_FRAME);
    m.files.push(ns(files.elapsed()));
    let mut request = Vec::new();
    let start = Instant::now();
    let mut rx =
        SyncReceiver::new_with_delta(target, Lww, entry, 1_800_001_000, 65536, &mut request)
            .unwrap();
    m.snapshot.push(ns(start.elapsed()));
    let mut tx = SyncSender::new(source).unwrap();
    let mut fragment = vec![0; 256 * 1024];
    let start = Instant::now();
    let mut receiver_ns = 0;
    let mut done = false;
    while !done {
        assert!(!request.is_empty());
        tx.process(&request).unwrap();
        request.clear();
        loop {
            let n = tx.write_output(&mut fragment).unwrap();
            if n == 0 {
                break;
            }
            let process = Instant::now();
            done = rx.process(&fragment[..n], &mut request).unwrap() == SyncStatus::Done;
            receiver_ns += ns(process.elapsed());
        }
    }
    tx.process(&request).unwrap();
    m.receive.push(ns(start.elapsed()));
    m.receiver_process.push(receiver_ns);
    m.journal.push(0);
    // The immediate receiver has already resolved/written entries during
    // process. This phase only flushes remaining tree repair and commits.
    let start = Instant::now();
    rx.finish_with_delta().unwrap();
    let commit_ns = ns(start.elapsed());
    m.merge.push(commit_ns);
    m.endpoint.push(m.files.last().unwrap() + m.snapshot.last().unwrap() + receiver_ns + commit_ns);
    assert_eq!(
        source.begin_read().unwrap().root_hash().unwrap(),
        target.begin_read().unwrap().root_hash().unwrap()
    );
}
fn report(mode: &str, case: &str, metric: &str, samples: &mut [u64]) -> u64 {
    samples.sort_unstable();
    let p = |percent: usize| samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)];
    eprintln!(
        "{mode}_sync case={case} metric={metric} samples={} p50={} p95={} p99={} max={}",
        samples.len(),
        p(50),
        p(95),
        p(99),
        samples.last().unwrap()
    );
    p(99)
}
impl Measurements {
    fn report(&mut self, mode: &str, case: &str, budget_ns: u64) {
        report(mode, case, "file_creation_ns", &mut self.files);
        let snapshot = report(
            mode,
            case,
            if mode == "prepared" { "snapshot_ns" } else { "receiver_start_ns" },
            &mut self.snapshot,
        );
        report(mode, case, "same_process_transfer_ns", &mut self.receive);
        report(mode, case, "receiver_process_ns", &mut self.receiver_process);
        report(
            mode,
            case,
            if mode == "prepared" { "merge_commit_ns" } else { "final_commit_ns" },
            &mut self.merge,
        );
        let endpoint = report(mode, case, "receiver_endpoint_ns", &mut self.endpoint);
        report(mode, case, "journal_bytes", &mut self.journal);
        if mode == "prepared" {
            assert!(snapshot < 100_000, "snapshot p99 exceeds 100us");
        }
        assert!(endpoint < budget_ns, "receiver endpoint p99 exceeds budget");
    }
}
#[test]
#[ignore = "100 samples per fixture; run optimized on a quiet reference device"]
fn benchmark_prepared_sync() {
    benchmark("prepared", transfer);
}

#[test]
#[ignore = "100 samples per fixture; run optimized on a quiet reference device"]
fn benchmark_immediate_sync() {
    benchmark("immediate", transfer_immediate);
}

fn benchmark(mode: &str, transfer: fn(&Db<RedbStorage>, &Db<RedbStorage>, &mut Measurements)) {
    const RECORDS: usize = 10_000;
    const SAMPLES: usize = 100;
    let source = Fixture::new();
    populate(&source.db, RECORDS, 4 * 1024 * 1024, 0);
    eprintln!(
        "{mode}_sync backend=redb fsync=ordinary cache_bytes=524288 records=10000 initial_payload_bytes=4194304 catchup_records=1000 catchup_payload_bytes=1048576 fragments=262144 samples_per_case=100; snapshot excludes file creation; transfer includes sender+receiver; receiver_endpoint sums same-sample file_creation+snapshot+receiver_process+merge_commit; no network/UI/authorization"
    );
    if mode == "immediate" {
        eprintln!(
            "immediate_sync receiver_start includes writer+snapshot; receiver_process includes merge work; final_commit includes pending repair+durable commit; journal_bytes=0; live writer held across transfer"
        );
    }
    let mut initial = Measurements::default();
    for _ in 0..SAMPLES {
        let target = Fixture::new();
        transfer(&source.db, &target.db, &mut initial);
    }
    let target = Fixture::new();
    transfer(&source.db, &target.db, &mut Measurements::default());
    let mut catchup = Measurements::default();
    for generation in 1..=SAMPLES {
        populate(&source.db, 1000, 1024 * 1024, generation);
        transfer(&source.db, &target.db, &mut catchup);
    }
    initial.report(mode, "initial", 2_000_000_000);
    catchup.report(mode, "catchup", 500_000_000);
}

#[derive(Debug, Default)]
struct IoMetrics {
    writes: AtomicU64,
    bytes: AtomicU64,
    max_write: AtomicU64,
    write_ns: AtomicU64,
    sync_ns: AtomicU64,
    resize_ns: AtomicU64,
}
impl IoMetrics {
    fn take(&self) -> [u64; 6] {
        [&self.writes, &self.bytes, &self.max_write, &self.write_ns, &self.sync_ns, &self.resize_ns]
            .map(|counter| counter.swap(0, Ordering::Relaxed))
    }
}
#[derive(Debug)]
struct MeasuredBackend {
    inner: FileBackend,
    metrics: Arc<IoMetrics>,
    initialize_growth: bool,
}
impl ::redb::StorageBackend for MeasuredBackend {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let start = Instant::now();
        let result = self.inner.write(offset, bytes);
        self.metrics.write_ns.fetch_add(ns(start.elapsed()), Ordering::Relaxed);
        self.metrics.writes.fetch_add(1, Ordering::Relaxed);
        self.metrics.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.metrics.max_write.fetch_max(bytes.len() as u64, Ordering::Relaxed);
        result
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        let start = Instant::now();
        let result = (|| {
            let previous = if self.initialize_growth { self.inner.len()? } else { len };
            self.inner.set_len(len)?;
            if previous < len {
                let zeroes = [0; 64 * 1024];
                let mut offset = previous;
                while offset < len {
                    let size = (len - offset).min(zeroes.len() as u64) as usize;
                    // Use the measured write path: byte/call/time totals must
                    // include initialization rather than hiding its cost.
                    self.write(offset, &zeroes[..size])?;
                    offset += size as u64;
                }
            }
            Ok(())
        })();
        self.metrics.resize_ns.fetch_add(ns(start.elapsed()), Ordering::Relaxed);
        result
    }
    fn sync_data(&self) -> std::io::Result<()> {
        let start = Instant::now();
        let result = self.inner.sync_data();
        self.metrics.sync_ns.fetch_add(ns(start.elapsed()), Ordering::Relaxed);
        result
    }
    fn close(&self) -> std::io::Result<()> {
        self.inner.close()
    }
}

#[test]
#[ignore = "storage diagnosis: run optimized on a quiet reference device"]
fn benchmark_pinned_snapshot_writes() {
    use std::os::unix::fs::MetadataExt;
    let cases = [
        ("unpinned", false, false, false),
        ("pinned", true, false, false),
        #[cfg(target_os = "macos")]
        ("pinned_preallocated", true, true, false),
        ("pinned_initialized_growth", true, false, true),
    ];
    for (case, pinned, preallocate, initialize_growth) in cases {
        let metrics = Arc::new(IoMetrics::default());
        let setup = Instant::now();
        let fixture = Fixture::measured_growth(Some(metrics.clone()), initialize_growth);
        populate(&fixture.db, 32, 32 * 4096, 0);
        let setup_ns = ns(setup.elapsed());
        let before = std::fs::metadata(&fixture.path).unwrap().len();
        // Diagnostic only: reserve extents without changing redb's logical
        // file length or writing into any database pages. Keep the descriptor
        // alive through the workload and count the reservation separately.
        #[cfg(target_os = "macos")]
        let _allocation = if preallocate {
            use std::os::fd::AsRawFd;
            let file =
                std::fs::OpenOptions::new().read(true).write(true).open(&fixture.path).unwrap();
            let mut reservation = libc::fstore_t {
                fst_flags: libc::F_ALLOCATEALL | libc::F_ALLOCATEPERSIST,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: 64 * 1024 * 1024,
                fst_bytesalloc: 0,
            };
            let start = Instant::now();
            // SAFETY: a live writable file and correctly sized fstore_t are
            // retained throughout this synchronous call.
            let result =
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut reservation) };
            assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
            eprintln!(
                "pinned_writes case={case} preallocate_ns={} reserved_bytes={}",
                ns(start.elapsed()),
                reservation.fst_bytesalloc
            );
            assert_eq!(file.metadata().unwrap().len(), before);
            Some(file)
        } else {
            None
        };
        #[cfg(not(target_os = "macos"))]
        assert!(!preallocate);
        let allocated_before = std::fs::metadata(&fixture.path).unwrap().blocks() * 512;
        let snapshot = if pinned { Some(fixture.db.begin_read().unwrap()) } else { None };
        let mut transaction = Vec::with_capacity(1000);
        let mut begin = Vec::with_capacity(1000);
        let mut put = Vec::with_capacity(1000);
        let mut commit = Vec::with_capacity(1000);
        let mut io: [Vec<u64>; 6] = std::array::from_fn(|_| Vec::with_capacity(1000));
        metrics.take();
        for sample in 0..1000u64 {
            let mut value = [0; 4096];
            value[..8].copy_from_slice(&sample.to_le_bytes());
            let start = Instant::now();
            let mut tx = fixture.db.begin_write(1_800_000_100).unwrap();
            begin.push(ns(start.elapsed()));
            let operation = Instant::now();
            tx.put(&0u64.to_be_bytes(), &value).unwrap();
            put.push(ns(operation.elapsed()));
            let operation = Instant::now();
            tx.commit().unwrap();
            commit.push(ns(operation.elapsed()));
            transaction.push(ns(start.elapsed()));
            for (samples, value) in io.iter_mut().zip(metrics.take()) {
                samples.push(value);
            }
        }
        let after = std::fs::metadata(&fixture.path).unwrap().len();
        let allocated_after = std::fs::metadata(&fixture.path).unwrap().blocks() * 512;
        eprintln!(
            "pinned_writes case={case} rows=32 value_bytes=4096 transactions=1000 setup_ns={setup_ns} file_before={before} file_after={after} allocated_before={allocated_before} allocated_after={allocated_after}"
        );
        report("pinned_writes", case, "begin_ns", &mut begin);
        report("pinned_writes", case, "put_ns", &mut put);
        report("pinned_writes", case, "commit_ns", &mut commit);
        report("pinned_writes", case, "transaction_ns", &mut transaction);
        for (metric, samples) in [
            "write_calls",
            "write_bytes",
            "max_write_bytes",
            "file_write_ns",
            "fsync_ns",
            "resize_ns",
        ]
        .into_iter()
        .zip(&mut io)
        {
            report("pinned_writes", case, metric, samples);
        }
        if let Some(read) = snapshot {
            assert_eq!(&*read.get(&0u64.to_be_bytes()).unwrap().unwrap(), &[0; 4096]);
        }
    }
}

#[test]
#[ignore = "filesystem diagnosis: run optimized on a quiet reference device"]
fn benchmark_file_page_writes() {
    use ::redb::StorageBackend;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    const SAMPLES: usize = 1000;
    const PAGE: usize = 8192;
    for case in ["reused", "sparse", "initialized", "preallocated"] {
        #[cfg(not(target_os = "macos"))]
        if case == "preallocated" {
            continue;
        }
        let file_path = path();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file_path)
            .unwrap();
        std::fs::remove_file(&file_path).unwrap();
        let start = Instant::now();
        #[cfg(target_os = "macos")]
        if case == "preallocated" {
            use std::os::fd::AsRawFd;
            let mut reservation = libc::fstore_t {
                fst_flags: libc::F_ALLOCATEALL | libc::F_ALLOCATEPERSIST,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: (SAMPLES * PAGE) as i64,
                fst_bytesalloc: 0,
            };
            // SAFETY: writable file and correctly sized allocation request.
            let result =
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut reservation) };
            assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        }
        let inspect = file.try_clone().unwrap();
        let backend = FileBackend::new(file).unwrap();
        backend.set_len((SAMPLES * PAGE) as u64).unwrap();
        let mut bytes = [49; PAGE];
        if case == "initialized" {
            for page in 0..SAMPLES {
                backend.write((page * PAGE) as u64, &bytes).unwrap();
            }
        } else if case == "reused" {
            backend.write(0, &bytes).unwrap();
        }
        backend.sync_data().unwrap();
        let setup_ns = ns(start.elapsed());
        let allocated_before = inspect.metadata().unwrap().blocks() * 512;
        let mut writes = Vec::with_capacity(SAMPLES);
        let mut syncs = Vec::with_capacity(SAMPLES);
        let mut totals = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            bytes[..8].copy_from_slice(&(sample as u64).to_le_bytes());
            let offset = if case == "reused" { 0 } else { (sample * PAGE) as u64 };
            let start = Instant::now();
            backend.write(offset, &bytes).unwrap();
            writes.push(ns(start.elapsed()));
            let sync = Instant::now();
            backend.sync_data().unwrap();
            syncs.push(ns(sync.elapsed()));
            totals.push(ns(start.elapsed()));
        }
        let allocated_after = inspect.metadata().unwrap().blocks() * 512;
        let last = if case == "reused" { 0 } else { ((SAMPLES - 1) * PAGE) as u64 };
        let mut actual = [0; PAGE];
        backend.read(last, &mut actual).unwrap();
        assert_eq!(actual, bytes);
        eprintln!(
            "file_pages case={case} samples={SAMPLES} write_bytes={PAGE} setup_ns={setup_ns} allocated_before={allocated_before} allocated_after={allocated_after}"
        );
        report("file_pages", case, "write_ns", &mut writes);
        report("file_pages", case, "fsync_ns", &mut syncs);
        report("file_pages", case, "total_ns", &mut totals);
    }
}
