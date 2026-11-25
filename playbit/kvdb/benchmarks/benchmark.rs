//! Bottom-up, single-threaded storage benchmark. No UI or AP.
//!
//! Build without running: cargo build --release -p kvdb --features redb --example benchmark
//! Run fsync/memory cases, raw redb first: target/release/examples/benchmark
//! Include full-flush comparisons explicitly: target/release/examples/benchmark all
//! Run one case: target/release/examples/benchmark kvdb_delta_file_fsync 32
//! Paced diagnostic: benchmark --samples 100 --pause-ms 250 raw_redb_file_fsync 32
//!
//! Each case retains one initialized database handle, warms 100 overwrites, then
//! measures 1,000 overwrites of existing 16-byte keys with 32-byte values. Both
//! 32-key and 10,000-key fixtures use the same deterministic target/value sequence.
//! JSON lines are emitted only after verification and reopening, outside timing.
//! In-memory redb is a nondurable CPU comparison, not kvdb's cloning MemStorage.
//! Opt-in storage calls: benchmark --diagnostic-storage kvdb_syncable_memory 10000
//! This does not qualify encrypted/object storage, UI responsiveness, or AP.

#![allow(
    unsafe_code,
    reason = "The diagnostic allocator and native file preallocation require FFI."
)]
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use kvdb::redb::{RedbStorage, RedbTxn};
    use kvdb::{
        CreateOptions, Db,
        storage::{Storage, StorageProps, StorageTxn, ValueRead},
    };
    use redb::{
        Database, ReadableDatabase, ReadableTableMetadata, StorageBackend, TableDefinition,
    };
    use serde::Serialize;
    use serde_json::json;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        fs::OpenOptions,
        io::{self, Write},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const WARMUP: usize = 100;
    const SAMPLES: usize = 1_000;
    const CACHE_BYTES: usize = 512 * 1024;
    const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kvdb");
    const HASH_KEY: [u8; 32] = [0x42; 32];
    const WALL_SECS: u64 = 1;

    // Counting is process-wide, limited to the measured write. Counter and clock
    // overhead is included in the results; these are not uninstrumented timings.
    static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
    static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
    static REALLOCATIONS: AtomicU64 = AtomicU64::new(0);
    static REQUESTED_BYTES: AtomicU64 = AtomicU64::new(0);
    pub struct CountingAllocator;
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: forward the caller's unchanged allocation request.
            let result = unsafe { System.alloc(layout) };
            if !result.is_null() && TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                REQUESTED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            }
            result
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: forward the caller's unchanged allocation request.
            let result = unsafe { System.alloc_zeroed(layout) };
            if !result.is_null() && TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                REQUESTED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            }
            result
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            // SAFETY: forward the live pointer, original layout and new size.
            let result = unsafe { System.realloc(ptr, layout, size) };
            if !result.is_null() && TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
                REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                REQUESTED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
            }
            result
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: forward the live allocation and matching layout.
            unsafe { System.dealloc(ptr, layout) };
        }
    }
    #[derive(Default, Serialize)]
    struct AllocationCounts {
        allocations: u64,
        reallocations: u64,
        requested_bytes: u64,
    }
    struct AllocationWindow;
    impl AllocationWindow {
        fn start() -> Self {
            ALLOCATIONS.store(0, Ordering::Relaxed);
            REALLOCATIONS.store(0, Ordering::Relaxed);
            REQUESTED_BYTES.store(0, Ordering::Relaxed);
            TRACK_ALLOCATIONS.store(true, Ordering::SeqCst);
            Self
        }
        fn finish(self) -> AllocationCounts {
            TRACK_ALLOCATIONS.store(false, Ordering::SeqCst);
            AllocationCounts {
                allocations: ALLOCATIONS.load(Ordering::Relaxed),
                reallocations: REALLOCATIONS.load(Ordering::Relaxed),
                requested_bytes: REQUESTED_BYTES.load(Ordering::Relaxed),
            }
        }
    }
    impl Drop for AllocationWindow {
        fn drop(&mut self) {
            TRACK_ALLOCATIONS.store(false, Ordering::SeqCst);
        }
    }

    fn nanos(start: Instant) -> u64 {
        start.elapsed().as_nanos().try_into().unwrap()
    }
    fn thread_cpu_ns() -> io::Result<u64> {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime writes a timespec into this valid local pointer.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
    }
    fn thread_policy() -> Result<serde_json::Value> {
        #[cfg(target_os = "macos")]
        {
            unsafe extern "C" {
                fn pthread_get_qos_class_np(
                    thread: libc::pthread_t,
                    class: *mut u32,
                    priority: *mut i32,
                ) -> i32;
            }
            let (mut class, mut priority) = (0, 0);
            // SAFETY: pthread_self is live and both output pointers are valid.
            let error = unsafe {
                pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut priority)
            };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error).into());
            }
            Ok(json!({"requested_qos_class":class,"relative_priority":priority,
                "name":match class {0x21=>"user_interactive",0x19=>"user_initiated",0x15=>"default",
                    0x11=>"utility",0x09=>"background",0=>"unspecified",_=>"unknown"},
                "limitation":"requested class does not report overrides or process-wide policy limits"}))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(json!({"requested_qos_class":null,"limitation":"not a macOS QoS target"}))
        }
    }
    #[derive(Debug, Default)]
    struct Counters {
        values: [AtomicU64; 14],
    }
    #[derive(Clone, Copy, Default, Serialize)]
    struct IoCounts {
        reads: u64,
        read_bytes: u64,
        read_ns: u64,
        writes: u64,
        write_bytes: u64,
        write_ns: u64,
        lengths: u64,
        length_ns: u64,
        resizes: u64,
        resize_ns: u64,
        syncs: u64,
        sync_ns: u64,
        closes: u64,
        close_ns: u64,
    }
    impl Counters {
        fn reset(&self) {
            for value in &self.values {
                value.store(0, Ordering::Relaxed);
            }
        }
        fn add(&self, index: usize, value: u64) {
            self.values[index].fetch_add(value, Ordering::Relaxed);
        }
        fn snapshot(&self) -> IoCounts {
            let n: [u64; 14] = std::array::from_fn(|i| self.values[i].load(Ordering::Relaxed));
            IoCounts {
                reads: n[0],
                read_bytes: n[1],
                read_ns: n[2],
                writes: n[3],
                write_bytes: n[4],
                write_ns: n[5],
                lengths: n[6],
                length_ns: n[7],
                resizes: n[8],
                resize_ns: n[9],
                syncs: n[10],
                sync_ns: n[11],
                closes: n[12],
                close_ns: n[13],
            }
        }
    }
    #[derive(Debug)]
    enum Backend {
        Full(redb::backends::FileBackend),
        Fsync(kvdb::redb::FileBackend),
        Memory(Arc<redb::backends::InMemoryBackend>),
    }
    impl Backend {
        fn inner(&self) -> &dyn StorageBackend {
            match self {
                Self::Full(b) => b,
                Self::Fsync(b) => b,
                Self::Memory(b) => &**b,
            }
        }
    }
    #[derive(Debug)]
    struct CountedBackend {
        inner: Backend,
        counters: Arc<Counters>,
    }
    impl StorageBackend for CountedBackend {
        fn len(&self) -> io::Result<u64> {
            let start = Instant::now();
            let result = self.inner.inner().len();
            self.counters.add(7, nanos(start));
            self.counters.add(6, 1);
            result
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            let start = Instant::now();
            let result = self.inner.inner().read(offset, out);
            self.counters.add(2, nanos(start));
            self.counters.add(0, 1);
            self.counters.add(1, out.len() as u64);
            result
        }
        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            let start = Instant::now();
            let result = self.inner.inner().write(offset, data);
            self.counters.add(5, nanos(start));
            self.counters.add(3, 1);
            self.counters.add(4, data.len() as u64);
            result
        }
        fn set_len(&self, len: u64) -> io::Result<()> {
            let start = Instant::now();
            let result = self.inner.inner().set_len(len);
            self.counters.add(9, nanos(start));
            self.counters.add(8, 1);
            result
        }
        fn sync_data(&self) -> io::Result<()> {
            let start = Instant::now();
            let result = self.inner.inner().sync_data();
            self.counters.add(11, nanos(start));
            self.counters.add(10, 1);
            result
        }
        fn close(&self) -> io::Result<()> {
            let start = Instant::now();
            let result = self.inner.inner().close();
            self.counters.add(13, nanos(start));
            self.counters.add(12, 1);
            result
        }
    }

    #[derive(Clone, Copy)]
    enum Layer {
        Raw,
        Plain,
        Syncable,
        Delta,
    }
    impl Layer {
        fn name(self) -> &'static str {
            match self {
                Self::Raw => "raw_redb",
                Self::Plain => "kvdb_plain",
                Self::Syncable => "kvdb_syncable",
                Self::Delta => "kvdb_delta",
            }
        }
    }
    #[derive(Clone, Copy)]
    enum Medium {
        Full,
        Fsync,
        Memory,
    }
    impl Medium {
        fn name(self) -> &'static str {
            match self {
                Self::Full => "file_fullfsync",
                Self::Fsync => "file_fsync",
                Self::Memory => "memory",
            }
        }
    }
    struct Fixture {
        path: PathBuf,
        medium: Medium,
        memory: Arc<redb::backends::InMemoryBackend>,
        counters: Arc<Counters>,
    }
    impl Fixture {
        fn open(&self) -> Result<Database> {
            let inner = match self.medium {
                Medium::Memory => Backend::Memory(self.memory.clone()),
                Medium::Full | Medium::Fsync => {
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&self.path)?;
                    match self.medium {
                        Medium::Full => Backend::Full(redb::backends::FileBackend::new(file)?),
                        Medium::Fsync => Backend::Fsync(kvdb::redb::FileBackend::new(file)?),
                        Medium::Memory => unreachable!(),
                    }
                }
            };
            Ok(Database::builder()
                .set_cache_size(CACHE_BYTES)
                .create_with_backend(CountedBackend { inner, counters: self.counters.clone() })?)
        }
    }
    type KvResult<T> = std::result::Result<T, kvdb::Error>;
    const METHOD_COUNT: usize = 15;
    const METHOD_NAMES: [&str; METHOD_COUNT] = [
        "begin_read",
        "begin_write",
        "snapshot_value",
        "get",
        "value_len",
        "read_value_at",
        "next_key",
        "prev_key",
        "put_from",
        "next_from",
        "prev_from",
        "put",
        "del",
        "commit",
        "visit",
    ];
    #[derive(Clone, Copy)]
    enum Method {
        BeginRead,
        BeginWrite,
        SnapshotValue,
        Get,
        ValueLen,
        ReadValueAt,
        NextKey,
        PrevKey,
        PutFrom,
        NextFrom,
        PrevFrom,
        Put,
        Del,
        Commit,
        Visit,
    }
    #[derive(Clone, Copy, Default, Serialize)]
    struct MethodCounts {
        calls: u64,
        errors: u64,
        elapsed_ns: u64,
        key_bytes: u64,
        requested_value_bytes: u64,
        returned_bytes: u64,
        copied_bytes: u64,
        visited_records: u64,
        callback_ns: u64,
    }
    #[derive(Default)]
    struct StorageCounters {
        // Fixed counters only: no per-call strings, events, maps, or allocations.
        methods: [[AtomicU64; 9]; METHOD_COUNT],
        elapsed_ns: AtomicU64,
    }
    impl StorageCounters {
        fn reset(&self) {
            for method in &self.methods {
                for value in method {
                    value.store(0, Ordering::Relaxed);
                }
            }
            self.elapsed_ns.store(0, Ordering::Relaxed);
        }
        // Fixed counter columns are passed directly to keep probe overhead visible.
        #[allow(clippy::too_many_arguments)]
        fn record(
            &self,
            method: Method,
            elapsed: u64,
            error: bool,
            key: usize,
            requested: usize,
            returned: usize,
            copied: usize,
        ) {
            let values = [
                1,
                u64::from(error),
                elapsed,
                key as u64,
                requested as u64,
                returned as u64,
                copied as u64,
            ];
            for (counter, value) in self.methods[method as usize].iter().zip(values) {
                counter.fetch_add(value, Ordering::Relaxed);
            }
            self.elapsed_ns.fetch_add(elapsed, Ordering::Relaxed);
        }
        fn elapsed(&self) -> u64 {
            self.elapsed_ns.load(Ordering::Relaxed)
        }
        fn snapshot(&self) -> [MethodCounts; METHOD_COUNT] {
            std::array::from_fn(|i| {
                let n: [u64; 9] =
                    std::array::from_fn(|j| self.methods[i][j].load(Ordering::Relaxed));
                MethodCounts {
                    calls: n[0],
                    errors: n[1],
                    elapsed_ns: n[2],
                    key_bytes: n[3],
                    requested_value_bytes: n[4],
                    returned_bytes: n[5],
                    copied_bytes: n[6],
                    visited_records: n[7],
                    callback_ns: n[8],
                }
            })
        }
    }
    struct DiagnosticStorage {
        inner: RedbStorage,
        counters: Arc<StorageCounters>,
    }
    struct DiagnosticTxn<'db> {
        inner: RedbTxn<'db>,
        counters: &'db StorageCounters,
        write: bool,
    }
    impl Storage for DiagnosticStorage {
        type Txn<'db>
            = DiagnosticTxn<'db>
        where
            Self: 'db;
        fn begin(&self, write: bool) -> KvResult<Self::Txn<'_>> {
            let start = Instant::now();
            let result = self.inner.begin(write);
            let elapsed = nanos(start);
            self.counters.record(
                if write { Method::BeginWrite } else { Method::BeginRead },
                elapsed,
                result.is_err(),
                0,
                0,
                0,
                0,
            );
            result.map(|inner| DiagnosticTxn { inner, counters: &self.counters, write })
        }
        fn props(&self) -> StorageProps {
            self.inner.props()
        }
    }
    impl<'db> StorageTxn for DiagnosticTxn<'db> {
        type ValueReader = <RedbTxn<'db> as StorageTxn>::ValueReader;
        type Guard<'g>
            = <RedbTxn<'db> as StorageTxn>::Guard<'g>
        where
            Self: 'g;
        fn snapshot_value(&self, key: &[u8]) -> KvResult<Option<Self::ValueReader>> {
            let start = Instant::now();
            let result = self.inner.snapshot_value(key);
            let elapsed = nanos(start);
            let bytes = result.as_ref().ok().and_then(|v| v.as_ref()).map_or(0, ValueRead::len);
            self.counters.record(
                Method::SnapshotValue,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                bytes,
                0,
            );
            result
        }
        fn get<'g>(&'g self, key: &[u8]) -> KvResult<Option<Self::Guard<'g>>> {
            let start = Instant::now();
            let result = self.inner.get(key);
            let elapsed = nanos(start);
            let bytes = result.as_ref().ok().and_then(|v| v.as_ref()).map_or(0, |v| v.len());
            self.counters.record(
                Method::Get,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                bytes,
                if self.write { bytes } else { 0 },
            );
            result
        }
        fn value_len(&self, key: &[u8]) -> KvResult<Option<usize>> {
            let start = Instant::now();
            let result = self.inner.value_len(key);
            self.counters.record(
                Method::ValueLen,
                nanos(start),
                result.is_err(),
                key.len(),
                0,
                0,
                0,
            );
            result
        }
        fn read_value_at(
            &self,
            key: &[u8],
            offset: usize,
            out: &mut [u8],
        ) -> KvResult<Option<usize>> {
            let start = Instant::now();
            let result = self.inner.read_value_at(key, offset, out);
            let elapsed = nanos(start);
            let bytes = result.as_ref().ok().and_then(|v| *v).unwrap_or(0);
            self.counters.record(
                Method::ReadValueAt,
                elapsed,
                result.is_err(),
                key.len(),
                out.len(),
                bytes,
                bytes,
            );
            result
        }
        fn next_key(&self, key: &[u8], exclusive: bool) -> KvResult<Option<Vec<u8>>> {
            let start = Instant::now();
            let result = self.inner.next_key(key, exclusive);
            let elapsed = nanos(start);
            let bytes = result.as_ref().ok().and_then(|v| v.as_ref()).map_or(0, Vec::len);
            self.counters.record(
                Method::NextKey,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                bytes,
                bytes,
            );
            result
        }
        fn prev_key(&self, key: &[u8], exclusive: bool) -> KvResult<Option<Vec<u8>>> {
            let start = Instant::now();
            let result = self.inner.prev_key(key, exclusive);
            let elapsed = nanos(start);
            let bytes = result.as_ref().ok().and_then(|v| v.as_ref()).map_or(0, Vec::len);
            self.counters.record(
                Method::PrevKey,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                bytes,
                bytes,
            );
            result
        }
        fn visit(
            &self,
            key: &[u8],
            exclusive: bool,
            reverse: bool,
            visitor: &mut kvdb::storage::ValueVisitor<'_>,
        ) -> KvResult<()> {
            let mut visited = 0u64;
            let mut borrowed_bytes = 0usize;
            let mut callback_ns = 0u64;
            let start = Instant::now();
            let result = self.inner.visit(key, exclusive, reverse, &mut |key, value| {
                visited += 1;
                borrowed_bytes += key.len() + value.len();
                let callback_start = Instant::now();
                let result = visitor(key, value);
                callback_ns += nanos(callback_start);
                result
            });
            let elapsed = nanos(start);
            self.counters.record(
                Method::Visit,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                borrowed_bytes,
                0,
            );
            let counts = &self.counters.methods[Method::Visit as usize];
            counts[7].fetch_add(visited, Ordering::Relaxed);
            counts[8].fetch_add(callback_ns, Ordering::Relaxed);
            // A visit contains caller work. Exclude that interval from aggregate
            // storage time; nested diagnostic calls account for themselves.
            self.counters.elapsed_ns.fetch_sub(callback_ns, Ordering::Relaxed);
            result
        }
        fn put_from(&mut self, key: &[u8], value: &dyn ValueRead) -> KvResult<()> {
            let start = Instant::now();
            let result = self.inner.put_from(key, value);
            let elapsed = nanos(start);
            self.counters.record(
                Method::PutFrom,
                elapsed,
                result.is_err(),
                key.len(),
                value.len(),
                0,
                0,
            );
            result
        }
        fn next_from<'g>(
            &'g self,
            key: &[u8],
            exclusive: bool,
        ) -> KvResult<Option<(Self::Guard<'g>, Self::Guard<'g>)>> {
            let start = Instant::now();
            let result = self.inner.next_from(key, exclusive);
            let elapsed = nanos(start);
            let bytes =
                result.as_ref().ok().and_then(|v| v.as_ref()).map_or(0, |(k, v)| k.len() + v.len());
            self.counters.record(
                Method::NextFrom,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                bytes,
                if self.write { bytes } else { 0 },
            );
            result
        }
        fn prev_from<'g>(
            &'g self,
            key: &[u8],
            exclusive: bool,
        ) -> KvResult<Option<(Self::Guard<'g>, Self::Guard<'g>)>> {
            let start = Instant::now();
            let result = self.inner.prev_from(key, exclusive);
            let elapsed = nanos(start);
            let bytes =
                result.as_ref().ok().and_then(|v| v.as_ref()).map_or(0, |(k, v)| k.len() + v.len());
            self.counters.record(
                Method::PrevFrom,
                elapsed,
                result.is_err(),
                key.len(),
                0,
                bytes,
                if self.write { bytes } else { 0 },
            );
            result
        }
        fn put(&mut self, key: &[u8], value: &[u8]) -> KvResult<()> {
            let start = Instant::now();
            let result = self.inner.put(key, value);
            self.counters.record(
                Method::Put,
                nanos(start),
                result.is_err(),
                key.len(),
                value.len(),
                0,
                0,
            );
            result
        }
        fn del(&mut self, key: &[u8]) -> KvResult<bool> {
            let start = Instant::now();
            let result = self.inner.del(key);
            self.counters.record(Method::Del, nanos(start), result.is_err(), key.len(), 0, 0, 0);
            result
        }
        fn commit(self) -> KvResult<()> {
            let start = Instant::now();
            let result = self.inner.commit();
            self.counters.record(Method::Commit, nanos(start), result.is_err(), 0, 0, 0, 0);
            result
        }
    }

    enum Engine {
        Raw(Database),
        Kvdb { db: Db<RedbStorage>, capture: bool },
        Diagnostic { db: Db<DiagnosticStorage>, capture: bool },
    }
    struct Operation {
        total_ns: u64,
        begin_ns: u64,
        put_ns: u64,
        commit_ns: u64,
        thread_cpu_ns: u64,
        delta: Option<kvdb::sync::CommittedDelta>,
        storage_put_ns: Option<u64>,
    }
    impl Engine {
        fn create(
            fixture: &Fixture,
            layer: Layer,
            diagnostics: Option<&Arc<StorageCounters>>,
        ) -> Result<Self> {
            let database = fixture.open()?;
            Ok(match layer {
                Layer::Raw => Self::Raw(database),
                Layer::Plain | Layer::Syncable | Layer::Delta => {
                    let options = match layer {
                        Layer::Plain => CreateOptions::plain(),
                        _ => CreateOptions::syncable(HASH_KEY, 1),
                    };
                    let storage = RedbStorage::from_database(database);
                    let capture = matches!(layer, Layer::Delta);
                    match diagnostics {
                        Some(counters) => Self::Diagnostic {
                            db: Db::create(
                                DiagnosticStorage { inner: storage, counters: counters.clone() },
                                &options,
                            )?,
                            capture,
                        },
                        None => Self::Kvdb { db: Db::create(storage, &options)?, capture },
                    }
                }
            })
        }
        fn reopen(
            fixture: &Fixture,
            layer: Layer,
            diagnostics: Option<&Arc<StorageCounters>>,
        ) -> Result<Self> {
            let database = fixture.open()?;
            Ok(match layer {
                Layer::Raw => Self::Raw(database),
                _ => {
                    let storage = RedbStorage::from_database(database);
                    let capture = matches!(layer, Layer::Delta);
                    match diagnostics {
                        Some(counters) => Self::Diagnostic {
                            db: Db::open(
                                DiagnosticStorage { inner: storage, counters: counters.clone() },
                                &Default::default(),
                            )?,
                            capture,
                        },
                        None => Self::Kvdb { db: Db::open(storage, &Default::default())?, capture },
                    }
                }
            })
        }
        fn seed(&self, expected: &[[u8; 32]]) -> Result<()> {
            match self {
                Self::Raw(db) => {
                    let tx = db.begin_write()?;
                    {
                        let mut table = tx.open_table(TABLE)?;
                        for (i, value) in expected.iter().enumerate() {
                            table.insert(&key(i)[..], &value[..])?;
                        }
                    }
                    tx.commit()?;
                }
                Self::Kvdb { db, .. } => seed_kvdb(db, expected)?,
                Self::Diagnostic { db, .. } => seed_kvdb(db, expected)?,
            }
            Ok(())
        }
        fn write(
            &self,
            key: &[u8; 16],
            value: &[u8; 32],
            diagnostics: Option<&StorageCounters>,
        ) -> Result<Operation> {
            let start = Instant::now();
            let cpu_start = thread_cpu_ns()?;
            match self {
                Self::Raw(db) => {
                    let tx = db.begin_write()?;
                    let begun = Instant::now();
                    {
                        let mut table = tx.open_table(TABLE)?;
                        table.insert(&key[..], &value[..])?;
                    }
                    let put = Instant::now();
                    tx.commit()?;
                    let cpu_elapsed = thread_cpu_ns()?
                        .checked_sub(cpu_start)
                        .ok_or("CPU clock went backwards")?;
                    let committed = Instant::now();
                    Ok(Operation {
                        total_ns: committed.duration_since(start).as_nanos() as u64,
                        begin_ns: begun.duration_since(start).as_nanos() as u64,
                        put_ns: put.duration_since(begun).as_nanos() as u64,
                        commit_ns: committed.duration_since(put).as_nanos() as u64,
                        thread_cpu_ns: cpu_elapsed,
                        delta: None,
                        storage_put_ns: None,
                    })
                }
                Self::Kvdb { db, capture } => {
                    write_kvdb(db, *capture, key, value, start, cpu_start, None)
                }
                Self::Diagnostic { db, capture } => {
                    write_kvdb(db, *capture, key, value, start, cpu_start, diagnostics)
                }
            }
        }
        fn verify(&self, expected: &[[u8; 32]]) -> Result<()> {
            match self {
                Self::Raw(db) => {
                    let tx = db.begin_read()?;
                    let table = tx.open_table(TABLE)?;
                    if table.len()? != expected.len() as u64 {
                        return Err("raw record count mismatch".into());
                    }
                    for (i, value) in expected.iter().enumerate() {
                        if table.get(&key(i)[..])?.is_none_or(|stored| stored.value() != value) {
                            return Err(format!("raw readback mismatch at key {i}").into());
                        }
                    }
                }
                Self::Kvdb { db, .. } => verify_kvdb(db, expected)?,
                Self::Diagnostic { db, .. } => verify_kvdb(db, expected)?,
            }
            Ok(())
        }
    }

    fn seed_kvdb<S: Storage>(db: &Db<S>, expected: &[[u8; 32]]) -> Result<()> {
        let mut tx = db.begin_write(WALL_SECS)?;
        for (i, value) in expected.iter().enumerate() {
            tx.put(&key(i), value)?;
        }
        tx.commit()?;
        Ok(())
    }
    fn write_kvdb<S: Storage>(
        db: &Db<S>,
        capture: bool,
        key: &[u8; 16],
        value: &[u8; 32],
        start: Instant,
        cpu_start: u64,
        diagnostics: Option<&StorageCounters>,
    ) -> Result<Operation> {
        let mut tx = if capture {
            db.begin_write_with_delta(WALL_SECS, 64 * 1024)?
        } else {
            db.begin_write(WALL_SECS)?
        };
        let before_storage = diagnostics.map(StorageCounters::elapsed);
        let begun = Instant::now();
        tx.put(key, value)?;
        let put = Instant::now();
        let storage_put_ns =
            diagnostics.zip(before_storage).map(|(counts, before)| counts.elapsed() - before);
        let delta = if capture {
            Some(tx.commit_with_delta()?)
        } else {
            tx.commit()?;
            None
        };
        let cpu_elapsed =
            thread_cpu_ns()?.checked_sub(cpu_start).ok_or("CPU clock went backwards")?;
        let committed = Instant::now();
        Ok(Operation {
            total_ns: committed.duration_since(start).as_nanos() as u64,
            begin_ns: begun.duration_since(start).as_nanos() as u64,
            put_ns: put.duration_since(begun).as_nanos() as u64,
            commit_ns: committed.duration_since(put).as_nanos() as u64,
            thread_cpu_ns: cpu_elapsed,
            delta,
            storage_put_ns,
        })
    }
    fn verify_kvdb<S: Storage>(db: &Db<S>, expected: &[[u8; 32]]) -> Result<()> {
        let tx = db.begin_read()?;
        let mut count = 0;
        for entry in tx.iter() {
            entry?;
            count += 1;
        }
        if count != expected.len() {
            return Err("kvdb record count mismatch".into());
        }
        for (i, value) in expected.iter().enumerate() {
            if tx.get(&key(i))?.is_none_or(|stored| &*stored != value) {
                return Err(format!("kvdb readback mismatch at key {i}").into());
            }
        }
        Ok(())
    }

    fn key(index: usize) -> [u8; 16] {
        let mut key = [0x6b; 16];
        key[..8].copy_from_slice(&(index as u64).to_be_bytes());
        key
    }
    fn value(iteration: usize) -> [u8; 32] {
        let mut value = [0x76; 32];
        value[..8].copy_from_slice(&(iteration as u64).to_le_bytes());
        value
    }
    #[derive(Serialize)]
    struct Sample {
        index: usize,
        key_index: usize,
        total_ns: u64,
        begin_ns: u64,
        put_ns: u64,
        commit_ns: u64,
        software_plus_non_sync_io_ns: u64,
        thread_cpu_ns: u64,
        io: IoCounts,
        allocation: AllocationCounts,
        captured_delta_bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        storage: Option<[MethodCounts; METHOD_COUNT]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        storage_put_ns: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        put_outside_storage_calls_ns: Option<u64>,
    }
    fn distribution(samples: &[Sample], field: impl Fn(&Sample) -> u64) -> serde_json::Value {
        let mut values: Vec<u64> = samples.iter().map(field).collect();
        values.sort_unstable();
        let at = |percent: usize| values[(values.len() * percent).div_ceil(100).saturating_sub(1)];
        json!({"p50":at(50),"p90":at(90),"p95":at(95),"p99":at(99),"max":values[values.len()-1]})
    }
    fn case(
        directory: &Path,
        layer: Layer,
        medium: Medium,
        keys: usize,
        diagnostic: bool,
        sample_count: usize,
        pause_ms: u64,
    ) -> Result<()> {
        let name = format!("{}_{}", layer.name(), medium.name());
        let fixture = Fixture {
            path: directory.join(format!("{name}-{keys}.redb")),
            medium,
            memory: Arc::new(redb::backends::InMemoryBackend::new()),
            counters: Arc::new(Counters::default()),
        };
        let diagnostics = diagnostic.then(|| Arc::new(StorageCounters::default()));
        let engine = Engine::create(&fixture, layer, diagnostics.as_ref())?;
        let mut expected: Vec<[u8; 32]> = (0..keys).map(|i| value(i + 1_000_000)).collect();
        engine.seed(&expected)?;
        engine.verify(&expected)?;
        let mut samples = Vec::with_capacity(sample_count);
        for iteration in 0..WARMUP + sample_count {
            if iteration >= WARMUP && pause_ms != 0 {
                std::thread::sleep(Duration::from_millis(pause_ms));
            }
            let key_index = (iteration * 7_919) % keys;
            let key = key(key_index);
            let value = value(iteration);
            fixture.counters.reset();
            if let Some(counters) = &diagnostics {
                counters.reset();
            }
            let allocation = (iteration >= WARMUP).then(AllocationWindow::start);
            let operation = engine.write(&key, &value, diagnostics.as_deref())?;
            let allocation =
                allocation.map_or_else(AllocationCounts::default, AllocationWindow::finish);
            let io = fixture.counters.snapshot();
            let storage = diagnostics.as_ref().map(|counters| counters.snapshot());
            expected[key_index] = value;
            let captured_delta_bytes = match operation.delta {
                Some(kvdb::sync::CommittedDelta::Bytes(bytes)) => bytes.len(),
                Some(kvdb::sync::CommittedDelta::PullRequired) => {
                    return Err("one-key delta unexpectedly exceeded cap".into());
                }
                None => 0,
            };
            if iteration >= WARMUP {
                let remainder = operation
                    .total_ns
                    .checked_sub(io.sync_ns)
                    .ok_or("paired sync duration exceeds total write duration")?;
                if io.syncs != 1 {
                    return Err(
                        format!("expected one backend sync per commit, got {}", io.syncs).into()
                    );
                }
                if let Some(methods) = &storage {
                    if methods[Method::BeginWrite as usize].calls != 1
                        || methods[Method::Commit as usize].calls != 1
                        || methods[Method::BeginRead as usize].calls != 0
                        || methods.iter().any(|method| method.errors != 0)
                    {
                        return Err(
                            "unexpected transaction count or error in diagnostic write".into()
                        );
                    }
                    let total_storage_ns: u64 =
                        methods.iter().map(|method| method.elapsed_ns - method.callback_ns).sum();
                    if total_storage_ns > operation.total_ns {
                        return Err(
                            "storage-call durations exceed their containing transaction".into()
                        );
                    }
                }
                samples.push(Sample {
                    index: iteration - WARMUP,
                    key_index,
                    total_ns: operation.total_ns,
                    begin_ns: operation.begin_ns,
                    put_ns: operation.put_ns,
                    commit_ns: operation.commit_ns,
                    software_plus_non_sync_io_ns: remainder,
                    thread_cpu_ns: operation.thread_cpu_ns,
                    io,
                    allocation,
                    captured_delta_bytes,
                    storage,
                    storage_put_ns: operation.storage_put_ns,
                    put_outside_storage_calls_ns: operation
                        .storage_put_ns
                        .map(|storage_ns| {
                            operation
                                .put_ns
                                .checked_sub(storage_ns)
                                .ok_or("storage calls exceed containing put interval")
                        })
                        .transpose()?,
                });
            }
        }
        engine.verify(&expected)?;
        drop(engine);
        let reopened = Engine::reopen(&fixture, layer, diagnostics.as_ref())?;
        reopened.verify(&expected)?;
        drop(reopened);
        let output = json!({
            "benchmark":"storage-bottom-up", "version":1, "case":name, "diagnostic_storage":diagnostic,
            "storage_method_order":if diagnostic { Some(METHOD_NAMES) } else { None },
            "os":std::env::consts::OS, "arch":std::env::consts::ARCH,
            "thread_policy":thread_policy()?,
            "keys":keys, "key_bytes":16, "value_bytes":32, "cache_bytes":CACHE_BYTES,
            "warmup":WARMUP, "sample_count":sample_count, "pause_before_write_ms":pause_ms,
            "durability":match medium {
                Medium::Full if cfg!(target_vendor = "apple") => "File::sync_data / fullfsync",
                Medium::Full => "File::sync_data / platform default",
                Medium::Fsync => "fsync", Medium::Memory => "none",
            }, "power_loss_qualification":false,
            "memory_case_is_nondurable":matches!(medium, Medium::Memory),
            "cpu_only_comparison":matches!(medium, Medium::Memory),
            "file_path":if matches!(medium, Medium::Memory) { None } else { Some(&fixture.path) },
            "verified":{"all_values_before_measurement":true,"all_values_after_measurement":true,
                "all_values_after_handle_reopen":true,"crash_or_power_loss":false},
            "method":{
                "clock":"std::time::Instant; per-sample same-clock paired intervals",
                "storage_diagnostics":if diagnostic { Some("Opt-in 15-method wrapper: original guards/readers delegate unchanged. Timed calls include backend I/O; do not add them to I/O times. Per-call clock/counter overhead affects total. copied_bytes counts adapter-owned returned guards/keys and read_value_at output copies, not redb internal page/write copies. value_len reports no returned data bytes; snapshot_value reports logical pinned bytes without copying. visit elapsed_ns includes callback_ns; aggregate storage time subtracts callback_ns, leaving callback work in put_outside_storage_calls_ns except separately counted nested storage calls. visit returned_bytes counts borrowed key/value lengths, visited_records counts callback invocations, and copied_bytes is zero for traversal (copies performed inside callbacks are not measured). put_outside_storage_calls_ns includes hashing/tree bookkeeping and wrapper overhead, not pure hashing. Diagnostic results are not qualification measurements.") } else { None },
                "cpu_clock":"clock_gettime(CLOCK_THREAD_CPUTIME_ID) around the write; includes calling-thread user/system CPU and CPU-probe overhead; not inferred from wall minus sync",
                "targets":"(iteration * 7919) % keys; warmup and measured writes use distinct values",
                "allocations":"process-wide successful alloc/alloc_zeroed and realloc calls during each measured write; requested bytes includes full new realloc size, not live memory",
                "overhead":"allocator counters, backend counters and wall/CPU clock reads enabled; included in total (first CPU probe in begin, second in commit); idle pause/setup/readback/output/delta drop excluded",
                "remainder":"per-sample total minus that same sample's backend sync duration; includes non-sync I/O, scheduling and instrumentation; not pure CPU",
                "reopen":"new database handle; memory backend retains bytes in its shared InMemoryBackend; no durability claim",
                "limitations":"core storage only; no encryption/object authorization/FieldCell/AP/UI; isolated sequential baseline, not concurrency qualification"
            },
            "latency_ns":{
                "total":distribution(&samples, |s| s.total_ns),
                "begin":distribution(&samples, |s| s.begin_ns),
                "put":distribution(&samples, |s| s.put_ns),
                "commit":distribution(&samples, |s| s.commit_ns),
                "sync":distribution(&samples, |s| s.io.sync_ns),
                "thread_cpu":distribution(&samples, |s| s.thread_cpu_ns),
                "software_plus_non_sync_io":distribution(&samples, |s| s.software_plus_non_sync_io_ns)
            },
            "samples":samples
        });
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        serde_json::to_writer(&mut stdout, &output)?;
        writeln!(stdout)?;
        stdout.flush()?;
        Ok(())
    }

    pub fn run() -> Result<()> {
        if cfg!(debug_assertions) {
            return Err("build this benchmark with --release".into());
        }
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        let mut diagnostic = false;
        let mut sample_count = SAMPLES;
        let mut pause_ms = 0;
        while args.first().is_some_and(|arg| arg.starts_with("--")) {
            let option = args.remove(0);
            match option.as_str() {
                "--diagnostic-storage" => diagnostic = true,
                "--samples" | "--pause-ms" => {
                    if args.is_empty() {
                        return Err(format!("{option} requires an integer").into());
                    }
                    let value = args.remove(0).parse::<u64>()?;
                    if option == "--samples" {
                        if !(1..=10_000).contains(&value) {
                            return Err("samples must be 1..10000".into());
                        }
                        sample_count = value as usize;
                    } else {
                        if value > 1000 {
                            return Err("pause-ms must be 0..1000".into());
                        }
                        pause_ms = value;
                    }
                }
                _ => return Err(format!("unknown option: {option}").into()),
            }
        }
        if diagnostic
            && (args.is_empty()
                || args.first().is_some_and(|arg| arg == "all" || arg.starts_with("raw_redb")))
        {
            return Err("--diagnostic-storage requires one explicit \
                kvdb_plain/kvdb_syncable/kvdb_delta case"
                .into());
        }
        if args.len() > 2 {
            return Err("usage: benchmark [--diagnostic-storage] [--samples N] [--pause-ms N] \
                [CASE|all] [32|10000]; CASE is raw_redb|kvdb_plain|kvdb_syncable|kvdb_delta \
                plus _file_fullfsync|_file_fsync|_memory"
                .into());
        }
        let selected_keys = args.get(1).map(|value| value.parse::<usize>()).transpose()?;
        if selected_keys.is_some_and(|keys| keys != 32 && keys != 10_000) {
            return Err("fixture must contain 32 or 10000 keys".into());
        }
        let all = args.first().is_some_and(|arg| arg == "all");
        let directory = tempfile::tempdir()?;
        let mut matched = false;
        // Complete the raw-redb baseline before adding kvdb layers. Full-flush
        // cases require `all` or an explicit case name, never the default run.
        for layer in [Layer::Raw, Layer::Plain, Layer::Syncable, Layer::Delta] {
            for keys in [32, 10_000] {
                for medium in [Medium::Fsync, Medium::Memory, Medium::Full] {
                    let name = format!("{}_{}", layer.name(), medium.name());
                    if !all && args.first().is_some_and(|selected| selected != &name)
                        || selected_keys.is_some_and(|selected| selected != keys)
                        || args.is_empty() && matches!(medium, Medium::Full)
                    {
                        continue;
                    }
                    matched = true;
                    case(
                        directory.path(),
                        layer,
                        medium,
                        keys,
                        diagnostic,
                        sample_count,
                        pause_ms,
                    )?;
                }
            }
        }
        if !matched {
            return Err("unknown benchmark case".into());
        }
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static ALLOCATOR: native::CountingAllocator = native::CountingAllocator;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    if let Err(error) = native::run() {
        eprintln!("benchmark: {error}");
        std::process::exit(1);
    }
}
#[cfg(target_arch = "wasm32")]
fn main() {
    panic!("benchmark requires a native filesystem backend");
}
