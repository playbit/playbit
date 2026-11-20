//! Measure the production local File path with 1,000 durable 64-KiB replacements.
//! Build optimized, then run through scripts/performance-guard.py. This covers
//! path resolution, current authority, publication, and retained-version reads;
//! it excludes guest/IPC scheduling, UI, AP and remote blob hydration.
//! --known-id measures 1,000 qualified metadata/authority/readiness reads per
//! reader at 32/500 files, with one/four simultaneous readers and no writer.
#![allow(clippy::unwrap_used, reason = "Benchmark failures must stop qualification.")]
use playbit::store::{Access, Id, LocalStore, volume_index::EntryAddress};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

const SAMPLES: usize = 1000;
const FILE_BYTES: usize = 64 * 1024;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

fn report(phase: &str, mut samples: Vec<u128>, interrupted: bool) {
    let count = samples.len();
    if count == 0 {
        return;
    }
    samples.sort_unstable();
    println!(
        "{}",
        serde_json::json!({
            "phase": phase,
            "samples": count,
            "complete": count == SAMPLES && !interrupted,
            "interrupted": interrupted,
            "file_bytes": FILE_BYTES,
            "local_store": true,
            "sync": true,
            "p50_ns": samples[(count * 50).div_ceil(100) - 1],
            "p99_ns": samples[(count * 99).div_ceil(100) - 1],
            "max_ns": samples[count - 1],
        })
    );
}

struct Sample {
    open: u128,
    close: u128,
    read: u128,
    total: u128,
}

fn replace_and_read(store: &LocalStore, access: &Access, volume: Id, bytes: &[u8]) -> Sample {
    let start = Instant::now();
    let target = store.file_destination(access, volume, None, "file", false).unwrap();
    let open = start.elapsed().as_nanos();
    let phase = Instant::now();
    store.publish_file(access, &target, &mut &bytes[..], true).unwrap();
    let close = phase.elapsed().as_nanos();
    let phase = Instant::now();
    let file = store.file_entry(access, volume, None, "file").unwrap();
    let version = store
        .blob_version(access, EntryAddress { volume, entry: file.id }, Some(file.content_revision))
        .unwrap();
    let lease = store.lease_blob_version(access, &version).unwrap();
    let contents = store.read_blob_version(access, &version).unwrap();
    let read = phase.elapsed().as_nanos();
    let total = start.elapsed().as_nanos();
    assert_eq!(contents, bytes);
    drop(lease);
    Sample { open, close, read, total }
}

#[allow(
    unsafe_code,
    reason = "Installs a process-lifetime C signal handler for this operator probe."
)]
fn install_interrupt_handler() {
    // Only set a lock-free flag in the handler. All storage and reporting stay
    // outside signal context, after finishing the current durable operation.
    // SAFETY: the handler has C ABI, lives for the process, and performs no I/O.
    assert_ne!(
        unsafe { libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t) },
        libc::SIG_ERR
    );
}

fn main() {
    install_interrupt_handler();
    if std::env::args().nth(1).as_deref() == Some("--known-id") {
        for entries in [32, 500] {
            known_id_cohort(entries);
            if INTERRUPTED.load(Ordering::Relaxed) {
                break;
            }
        }
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("File publication benchmark", false).unwrap().id;
    let mut bytes = vec![42; FILE_BYTES];
    let target = store.file_destination(&access, volume, None, "file", false).unwrap();
    store.publish_file(&access, &target, &mut bytes.as_slice(), true).unwrap();
    let mut opens = Vec::with_capacity(SAMPLES);
    let mut closes = Vec::with_capacity(SAMPLES);
    let mut reads = Vec::with_capacity(SAMPLES);
    let mut totals = Vec::with_capacity(SAMPLES);
    for number in 0..SAMPLES {
        if INTERRUPTED.load(Ordering::Relaxed) {
            break;
        }
        bytes[..8].copy_from_slice(&(number as u64).to_le_bytes());
        let sample = replace_and_read(&store, &access, volume, &bytes);
        opens.push(sample.open);
        closes.push(sample.close);
        reads.push(sample.read);
        totals.push(sample.total);
        if (number + 1) % 100 == 0 {
            eprintln!("Completed {} / {SAMPLES} File replacements", number + 1);
        }
    }
    // Retain history-dependent samples and outliers without logging inside any
    // measured interval. Total ends at read completion, before verification and
    // lease retirement; it is not a full guest File lifecycle measurement.
    let interrupted = INTERRUPTED.load(Ordering::Relaxed);
    println!(
        "{}",
        serde_json::json!({
            "phase": "cohort", "samples": totals.len(), "complete": totals.len() == SAMPLES && !interrupted,
            "interrupted": interrupted,
        })
    );
    for number in 0..totals.len() {
        println!(
            "{}",
            serde_json::json!({
                "sample": number,
                "content_versions": number + 2,
                "open_ns": opens[number],
                "close_ns": closes[number],
                "read_ns": reads[number],
                "total_ns": totals[number],
            })
        );
    }
    report("file_open_write", opens, interrupted);
    report("file_close_sync", closes, interrupted);
    report("file_open_read_and_read", reads, interrupted);
    report("file_replace_and_read", totals, interrupted);
}

fn known_id_cohort(entries: usize) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Qualified File reads", false).unwrap().id;
    let bytes = vec![42; FILE_BYTES];
    let mut files = Vec::with_capacity(entries);
    for number in 0..entries {
        if INTERRUPTED.load(Ordering::Relaxed) {
            return;
        }
        files.push(
            store.create_blob(&access, volume, &format!("file-{number}"), &bytes).unwrap().id,
        );
    }
    for readers in [1, 4] {
        let barrier = std::sync::Barrier::new(readers);
        let mut samples = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..readers)
                .map(|reader| {
                    let (store, access, files, barrier) = (&store, &access, &files, &barrier);
                    scope.spawn(move || {
                        let mut samples = Vec::with_capacity(SAMPLES);
                        barrier.wait();
                        for number in 0..SAMPLES {
                            if INTERRUPTED.load(Ordering::Relaxed) {
                                break;
                            }
                            let entry = files[(number + reader) % files.len()];
                            let started = Instant::now();
                            let details = store.entry_details(access, volume, entry).unwrap();
                            samples.push(started.elapsed().as_nanos());
                            assert_eq!(details.entry.id, entry);
                            assert!(details.readable && details.writable && details.backing_ready);
                        }
                        samples
                    })
                })
                .collect();
            tasks.into_iter().flat_map(|task| task.join().unwrap()).collect::<Vec<_>>()
        });
        samples.sort_unstable();
        let count = samples.len();
        if count == 0 {
            return;
        }
        println!(
            "{}",
            serde_json::json!({
                "phase": "known_id_metadata_authority_readiness", "entries": entries,
                "readers": readers, "writers": 0, "samples": count,
                "complete": count == SAMPLES * readers && !INTERRUPTED.load(Ordering::Relaxed),
                "file_bytes": FILE_BYTES, "production_local_store": true,
                "p50_ns": samples[(count * 50).div_ceil(100) - 1],
                "p99_ns": samples[(count * 99).div_ceil(100) - 1],
                "max_ns": samples[count - 1],
            })
        );
    }
}
