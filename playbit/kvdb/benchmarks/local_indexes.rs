//! Operator-only paired storage benchmark; never run as a timing assertion.
//! cargo run --locked --release -p kvdb --example local_indexes
//! Measures source read/update and durable commit, with/without two local name
//! mutations. Uses the native fsync backend and 512 KiB cache per database.
//! Excludes fixture creation, AP, SDK/IPC, UI and physical presentation.
#![allow(clippy::unwrap_used, reason = "Operator fixtures assert successful operations.")]

use kvdb::{
    CreateOptions, Db,
    redb::{FileBackend, RedbStorage},
};
use std::{
    fs::OpenOptions,
    path::Path,
    sync::{Arc, Barrier},
    time::Instant,
};

const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 7000;
const SAMPLES: usize = 1000;

struct Row {
    key: [u8; 16],
    names: [Vec<u8>; 2],
}

fn rows(count: usize, name_bytes: usize) -> Vec<Row> {
    (0..count)
        .map(|number| {
            let mut key = [0; 16];
            key[0] = 1;
            key[1..9].copy_from_slice(&(number as u64).to_be_bytes());
            let names = [0, 1].map(|version| {
                let mut name = format!("name/{number:08}/{version}").into_bytes();
                name.resize(name_bytes, b'x');
                name
            });
            Row { key, names }
        })
        .collect()
}

fn open(path: &Path) -> Db<RedbStorage> {
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
    let backend = FileBackend::new(file).unwrap();
    let mut builder = redb::Database::builder();
    builder.set_cache_size(512 * 1024);
    let database = builder.create_with_backend(backend).unwrap();
    Db::create(RedbStorage::from_database(database), &CreateOptions::syncable([7; 32], 1)).unwrap()
}

fn cohort(
    path: &Path,
    count: usize,
    name_bytes: usize,
    derived: bool,
    start: Arc<Barrier>,
) -> Vec<u128> {
    let rows = rows(count, name_bytes);
    let db = open(path);
    let mut tx = db.begin_write(WALL).unwrap();
    for row in &rows {
        tx.put(&row.key, &row.names[0]).unwrap();
        if derived {
            tx.local_indexes().put(&row.names[0], &row.key[1..]).unwrap();
        }
    }
    tx.commit().unwrap();
    start.wait();
    let mut samples = Vec::with_capacity(SAMPLES);
    for iteration in 0..SAMPLES {
        let row = &rows[iteration % count];
        let next = &row.names[1 - (iteration / count) % 2];
        let begin = Instant::now();
        let mut tx = db.begin_write(WALL + 1 + iteration as u64).unwrap();
        let previous = tx.get(&row.key).unwrap().unwrap().to_vec();
        tx.put(&row.key, next).unwrap();
        if derived {
            let mut index = tx.local_indexes();
            assert!(index.del(&previous).unwrap());
            index.put(next, &row.key[1..]).unwrap();
        }
        tx.commit().unwrap();
        samples.push(begin.elapsed().as_nanos());
    }
    db.verify().unwrap();
    samples
}

fn report(count: usize, name_bytes: usize, writers: usize, derived: bool, mut samples: Vec<u128>) {
    samples.sort_unstable();
    let n = samples.len();
    println!(
        "{{\"entries\":{count},\"name_bytes\":{name_bytes},\"writers\":{writers},\"derived\":{derived},\"samples\":{n},\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"max_ns\":{}}}",
        samples[n / 2 - 1],
        samples[n * 90 / 100 - 1],
        samples[n * 99 / 100 - 1],
        samples[n - 1]
    );
}

fn main() {
    // Each cohort has finite fixture/sample memory, one or four cache-bounded
    // backends, and no extra scheduling or replication queue.
    for (count, name_bytes, writers) in
        [(32, 32, 1), (500, 32, 1), (10000, 32, 1), (32, 480, 1), (500, 32, 4)]
    {
        for derived in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let barrier = Arc::new(Barrier::new(writers));
            let samples = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(writers);
                for writer in 0..writers {
                    let path = directory.path().join(format!("{writer}.kvdb"));
                    let barrier = barrier.clone();
                    handles.push(
                        scope.spawn(move || cohort(&path, count, name_bytes, derived, barrier)),
                    );
                }
                handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect()
            });
            report(count, name_bytes, writers, derived, samples);
        }
    }
}
