//! kvdb sync diagnostic. See benchmarks/sync.md for local and QUIC usage.
#![allow(
    unsafe_code,
    reason = "The benchmark reads the calling thread CPU clock through native FFI."
)]
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use kvdb::redb::{FileBackend, FileStaging, RedbStorage};
use kvdb::{
    CreateOptions, Db,
    storage::ValueRead,
    sync::{Lww, SyncReceiver, SyncSender, SyncStaging, SyncStatus},
};
#[path = "sync/metrics.rs"]
mod metrics;
#[path = "sync/quic.rs"]
mod quic;
use metrics::Wire;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{cell::Cell, path::Path, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Database = Db<RedbStorage>;
const BUFFER: usize = 256 * 1024;
const CACHE: usize = 512 * 1024;

#[derive(Clone, Copy, Default, Serialize)]
struct Calls {
    count: u64,
    total_ns: u64,
    max_ns: u64,
    at_least_1us: u64,
}

impl Calls {
    fn record(&mut self, ns: u64) {
        self.count += 1;
        self.total_ns += ns;
        self.max_ns = self.max_ns.max(ns);
        self.at_least_1us += u64::from(ns >= 1000);
    }
}

#[derive(Clone, Copy, Default, Serialize)]
struct StagingCalls {
    clear: Calls,
    append: Calls,
    read: Calls,
}

// Benchmark-only observation. With MEASURE=false the wrapper delegates without
// clock reads or counter updates. No production staging/transaction changes.
struct Staging<'a, const MEASURE: bool> {
    file: FileStaging,
    calls: &'a Cell<StagingCalls>,
}

impl<const MEASURE: bool> ValueRead for Staging<'_, MEASURE> {
    fn len(&self) -> usize {
        self.file.len()
    }

    fn read_at(&self, offset: usize, out: &mut [u8]) -> std::result::Result<usize, kvdb::Error> {
        let start = MEASURE.then(Instant::now);
        let result = self.file.read_at(offset, out);
        if let Some(start) = start {
            let mut calls = self.calls.get();
            calls.read.record(start.elapsed().as_nanos() as u64);
            self.calls.set(calls);
        }
        result
    }
}

impl<const MEASURE: bool> SyncStaging for Staging<'_, MEASURE> {
    fn clear(&mut self) -> std::result::Result<(), kvdb::Error> {
        let start = MEASURE.then(Instant::now);
        let result = self.file.clear();
        if let Some(start) = start {
            let mut calls = self.calls.get();
            calls.clear.record(start.elapsed().as_nanos() as u64);
            self.calls.set(calls);
        }
        result
    }

    fn append(&mut self, bytes: &[u8]) -> std::result::Result<(), kvdb::Error> {
        let start = MEASURE.then(Instant::now);
        let result = self.file.append(bytes);
        if let Some(start) = start {
            let mut calls = self.calls.get();
            calls.append.record(start.elapsed().as_nanos() as u64);
            self.calls.set(calls);
        }
        result
    }
}

fn create(path: &Path, memory: bool, replica: u64) -> Result<Database> {
    let mut builder = redb::Database::builder();
    builder.set_cache_size(CACHE);
    let backend = if memory {
        builder.create_with_backend(redb::backends::InMemoryBackend::new())?
    } else {
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        builder.create_with_backend(FileBackend::new(file)?)?
    };
    Ok(Db::create(
        RedbStorage::from_database(backend),
        &CreateOptions::syncable([0x42; 32], replica),
    )?)
}

fn key(index: usize) -> [u8; 16] {
    let mut key = [0x42; 16];
    key[8..].copy_from_slice(&(index as u64).to_be_bytes());
    key
}

fn value(index: usize, generation: u64, length: usize) -> Vec<u8> {
    let mut value = vec![0x61; length];
    value[..8].copy_from_slice(&(index as u64).to_be_bytes());
    value[8..16].copy_from_slice(&generation.to_be_bytes());
    value
}

fn cpu_ns() -> Result<u64> {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes into this valid local timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

#[derive(Serialize)]
struct Sample {
    total_ns: u64,
    protocol_and_apply_ns: u64,
    finish_ns: u64,
    thread_cpu_ns: u64,
    wire: Wire,
    #[serde(skip_serializing_if = "Option::is_none")]
    quic: Option<serde_json::Value>,
    applied: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    staging: Option<StagingCalls>,
}

fn merge<const MEASURE: bool>(source: &Database, target: &Database) -> Result<Sample> {
    // File creation and fixed wire buffers are setup; measured scope starts at
    // sender snapshot/receiver transaction acquisition and ends after commit.
    let calls = Cell::new(StagingCalls::default());
    let staging =
        Staging::<MEASURE> { file: FileStaging::new(tempfile::tempfile()?)?, calls: &calls };
    let mut request = Vec::with_capacity(BUFFER);
    let mut fragment = vec![0; BUFFER];
    let start = Instant::now();
    let cpu = cpu_ns()?;
    let mut sender = SyncSender::new(source)?;
    let mut receiver = SyncReceiver::new(target, Lww, staging, 1, &mut request)?;
    let mut wire = Wire::default();
    loop {
        wire.requests.feed(&request)?;
        sender.process(&request)?;
        request.clear();
        let mut done = false;
        loop {
            let count = sender.write_output(&mut fragment)?;
            if count == 0 {
                break;
            }
            wire.responses.feed(&fragment[..count])?;
            done |= receiver.process(&fragment[..count], &mut request)? == SyncStatus::Done;
        }
        if done {
            wire.requests.feed(&request)?;
            sender.process(&request)?;
            let finishing = start.elapsed().as_nanos() as u64;
            let outcome = receiver.finish()?;
            let cpu = cpu_ns()? - cpu;
            let total = start.elapsed().as_nanos() as u64;
            return Ok(Sample {
                total_ns: total,
                protocol_and_apply_ns: finishing,
                finish_ns: total - finishing,
                thread_cpu_ns: cpu,
                wire,
                quic: None,
                applied: outcome.applied,
                staging: MEASURE.then(|| calls.get()),
            });
        }
        assert!(!request.is_empty(), "protocol stopped before completion");
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Workload {
    keys: usize,
    changes: usize,
    samples: usize,
}

impl Workload {
    fn validate(self) -> Result<Self> {
        if self.keys > 100_000 || self.changes > self.keys || !(1..=1000).contains(&self.samples) {
            return Err("expected keys <= 100000, changes <= keys, samples in 1..=1000".into());
        }
        Ok(self)
    }

    fn seed(self, source: &Database) -> Result<()> {
        let mut tx = source.begin_write(1)?;
        for index in 0..self.keys {
            tx.put(&key(index), &value(index, 0, 300))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn change(self, source: &Database, generation: usize) -> Result<()> {
        let mut tx = source.begin_write(1)?;
        for change in 0..self.changes {
            // Unique, spread across the keyspace for every legal keys/changes pair.
            let index = change * self.keys / self.changes;
            tx.put(&key(index), &value(index, generation as u64, 1024))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn verify(
        self,
        target: &Database,
        root: Option<(u8, [u8; 32])>,
        generation: usize,
    ) -> Result<()> {
        let read = target.begin_read()?;
        if read.root_hash()? != root {
            return Err("received root differs from pinned sender root".into());
        }
        let mut changed = vec![false; self.keys];
        if generation > 0 {
            for change in 0..self.changes {
                changed[change * self.keys / self.changes] = true;
            }
        }
        let mut count = 0;
        for entry in read.iter() {
            let (actual_key, actual_value) = entry?;
            if count >= self.keys || actual_key.as_ref() != key(count) {
                return Err("received keys differ from fixture".into());
            }
            let generation = if changed[count] { generation as u64 } else { 0 };
            let expected = value(count, generation, if generation == 0 { 300 } else { 1024 });
            if actual_value.as_ref() != expected {
                return Err("received value differs from fixture".into());
            }
            count += 1;
        }
        if count != self.keys {
            return Err("missing fixture records".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Case {
    Cold,
    Equal,
    Catchup,
}
const CASES: [Case; 3] = [Case::Cold, Case::Equal, Case::Catchup];

struct Options {
    workload: Workload,
    memory: bool,
    diagnostic: bool,
    quic: bool,
    bind: std::net::SocketAddr,
    peer: Option<String>,
    cert: Option<std::path::PathBuf>,
}

const HELP: &str = "kvdb sync benchmark (synthetic data only)
  sync [--medium memory|fsync] [--keys N] [--changes N] [--samples N]
  sync --quic [--bind IP:PORT] [--medium memory|fsync]
  sync --quic --pull-from HOST:PORT --cert SERVER_CERT.der [workload options]
  --diagnostic-staging  time individual staging operations (adds overhead)
Defaults: memory, 10000 keys, min(1000, keys) changes, 10 samples per case.
Cases: cold empty receiver, equal roots, scattered overwrites; one warmup each.
QUIC server prints a temporary certificate path; copy that certificate to the client.
See playbit/kvdb/benchmarks/sync.md for measurement boundaries and limits.";

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut result = Self {
            workload: Workload { keys: 10_000, changes: 1000, samples: 10 },
            memory: true,
            diagnostic: false,
            quic: false,
            bind: "0.0.0.0:0".parse()?,
            peer: None,
            cert: None,
        };
        let mut changes = None;
        let mut explicit_bind = false;
        let mut explicit_workload = false;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            explicit_workload |= matches!(
                arg.as_str(),
                "--keys" | "--changes" | "--samples" | "--diagnostic-staging"
            );
            match arg.as_str() {
                "--quic" => result.quic = true,
                "--diagnostic-staging" => result.diagnostic = true,
                "--keys" => result.workload.keys = args.next().ok_or("missing keys")?.parse()?,
                "--changes" => changes = Some(args.next().ok_or("missing changes")?.parse()?),
                "--samples" => {
                    result.workload.samples = args.next().ok_or("missing samples")?.parse()?
                }
                "--bind" => {
                    explicit_bind = true;
                    result.bind = args.next().ok_or("missing bind address")?.parse()?;
                }
                "--pull-from" => result.peer = Some(args.next().ok_or("missing peer address")?),
                "--cert" => {
                    result.cert = Some(args.next().ok_or("missing certificate path")?.into())
                }
                "--medium" => {
                    result.memory = match args.next().as_deref() {
                        Some("memory") => true,
                        Some("fsync") => false,
                        _ => return Err("medium must be memory or fsync".into()),
                    }
                }
                _ => return Err(format!("unknown option: {arg}\n{HELP}").into()),
            }
        }
        result.workload.changes = changes.unwrap_or(1000.min(result.workload.keys));
        result.workload.validate()?;
        if result.peer.is_some() != result.cert.is_some() || (!result.quic && result.peer.is_some())
        {
            return Err("--pull-from requires --quic and --cert".into());
        }
        let server = result.quic && result.peer.is_none();
        if explicit_bind && !server {
            return Err("--bind is only valid for the QUIC server".into());
        }
        if server && explicit_workload {
            return Err("workload and staging options belong on the client".into());
        }
        Ok(result)
    }
}

fn report(options: &Options, case: Case, samples: &[Sample]) -> Result<()> {
    use std::io::Write;
    let mut ordered: Vec<_> = samples.iter().map(|s| s.total_ns).collect();
    ordered.sort_unstable();
    let p = |percent: usize| ordered[(ordered.len() * percent).div_ceil(100) - 1];
    let value = json!({
        "transport":if options.quic { "quic" } else { "memory" },
        "medium":if options.memory { "memory" } else { "fsync" },
        "case":case, "workload":options.workload, "warmups":1,
        "diagnostic_staging":options.diagnostic, "cache_bytes_per_database":CACHE,
        "protocol_version":kvdb::sync::PROTOCOL_VERSION,
        "small_range_bytes":kvdb::sync::SMALL_RANGE_BYTES,
        "request_batch_max":kvdb::sync::REQUEST_BATCH_MAX, "samples":samples,
        "total":{"p50_ns":p(50),"p95_ns":p(95),"p99_ns":p(99),"max_ns":ordered.last()},
        "verified":"exact pinned source root and every fixture key/value",
        "boundary":"receiver acquisition through durable finish; local mode also includes sender acquisition/processing; QUIC setup, source-ready control, final peer receipt and verification are separate",
        "limitations":"synthetic one-way merge; no AP framing/auth/engine/GUI; memory backend is nondurable; both backends use file staging; few samples do not qualify p99; thread CPU includes any same-thread QUIC work"
    });
    println!("{value}");
    std::io::stdout().flush()?;
    Ok(())
}

fn run_local<const MEASURE: bool>(options: &Options) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = create(&directory.path().join("source.kvdb"), options.memory, 1)?;
    options.workload.seed(&source)?;
    let mut target_directory = tempfile::tempdir()?;
    let mut target = create(&target_directory.path().join("target.kvdb"), options.memory, 2)?;
    for case in CASES {
        let mut samples = Vec::with_capacity(options.workload.samples);
        for iteration in 0..=options.workload.samples {
            if case == Case::Cold {
                drop(target);
                target_directory = tempfile::tempdir()?;
                target = create(&target_directory.path().join("target.kvdb"), options.memory, 2)?;
            }
            let generation = if case == Case::Catchup { iteration + 1 } else { 0 };
            if generation > 0 {
                options.workload.change(&source, generation)?;
            }
            let sample = merge::<MEASURE>(&source, &target)?;
            check_sample(options.workload, case, &sample)?;
            options.workload.verify(&target, source.begin_read()?.root_hash()?, generation)?;
            if iteration > 0 {
                samples.push(sample);
            }
        }
        report(options, case, &samples)?;
    }
    Ok(())
}

fn check_sample(workload: Workload, case: Case, sample: &Sample) -> Result<()> {
    let expected = match case {
        Case::Cold => workload.keys,
        Case::Equal => 0,
        Case::Catchup => workload.changes,
    };
    if sample.applied != expected
        || !sample.wire.requests.complete()
        || !sample.wire.responses.complete()
    {
        return Err("incomplete transfer or incorrect applied count".into());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let options = Options::parse(args)?;
    if options.quic {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        if options.diagnostic {
            runtime.block_on(quic::run::<true>(&options))
        } else {
            runtime.block_on(quic::run::<false>(&options))
        }
    } else if options.diagnostic {
        run_local::<true>(&options)
    } else {
        run_local::<false>(&options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_cold_equal_and_scattered_catchup() {
        for keys in [0, 1, 32, 257] {
            let options =
                Options::parse(["--keys".into(), keys.to_string(), "--samples".into(), "1".into()])
                    .unwrap();
            run_local::<false>(&options).unwrap();
        }
    }

    #[test]
    fn invalid_workloads_and_missing_trust_are_rejected() {
        for args in [
            vec!["--samples", "0"],
            vec!["--bind", "127.0.0.1:0"],
            vec!["--quic", "--keys", "1"],
            vec!["--keys", "100001"],
            vec!["--keys", "1", "--changes", "2"],
            vec!["--quic", "--pull-from", "127.0.0.1:1"],
        ] {
            assert!(Options::parse(args.into_iter().map(String::from)).is_err());
        }
    }
}
