//! Operator-only production KV stage measurements; excludes IPC and presentation.
use super::*;
use playbit::store::key_value;

fn seed(store: &LocalStore, access: &Access, volume: Id, value_bytes: usize) -> EntryAddress {
    let entry = store.create_database(access, volume, "settings", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume, entry: entry.id };
    // Untimed real backing setup. Metadata/backing publication uses LocalStore;
    // immutable test values are then seeded in bounded 64-key transactions.
    store
        .with_replica(volume, DatabaseId::Entry(entry.id), |db| {
            for start in (0u16..1064).step_by(64) {
                let mut tx = db.begin_write(1_800_000_000)?;
                for key in start..(start + 64).min(1064) {
                    let value = vec![42; if key == 0 { value_bytes } else { 128 }];
                    tx.put(&key.to_be_bytes(), &value)?;
                }
                tx.commit()?;
            }
            Ok(())
        })
        .unwrap();
    address
}
fn samples(
    store: &LocalStore,
    access: &Access,
    address: EntryAddress,
    rows: usize,
    value_bytes: usize,
) -> [Vec<u128>; 4] {
    let mut samples = std::array::from_fn(|_| Vec::with_capacity(SAMPLES));
    let mut value = vec![42; value_bytes];
    let write_key = vec![255; if value_bytes == key_value::MAX_BYTES { 511 } else { 2 }];
    for iteration in 0..SAMPLES {
        let start = Instant::now();
        let page = store.kv_keys(access, address, None, None, rows).unwrap();
        samples[0].push(start.elapsed().as_nanos());
        assert_eq!(page.keys.len(), rows);
        let start = Instant::now();
        let chunk =
            store.kv_read(access, address, &[0, 0], 0, key_value::MAX_BYTES as u32, None).unwrap();
        samples[1].push(start.elapsed().as_nanos());
        assert_eq!(chunk.bytes.len(), value_bytes);
        value[..8].copy_from_slice(&(iteration as u64).to_be_bytes());
        let start = Instant::now();
        store.kv_put(access, address, &write_key, &value).unwrap();
        samples[2].push(start.elapsed().as_nanos());
        let start = Instant::now();
        store.kv_delete(access, address, &write_key).unwrap();
        samples[3].push(start.elapsed().as_nanos());
    }
    samples
}
fn report(rows: usize, value_bytes: usize, writers: usize, phase: &str, mut values: Vec<u128>) {
    values.sort_unstable();
    println!(
        "{}",
        serde_json::json!({"phase":phase,"rows":rows,"unrelated_keys":1000,
        "value_bytes":value_bytes,"writers":writers,"samples":values.len(),
        "p50_ns":values[SAMPLES/2-1],"p99_ns":values[SAMPLES*99/100-1],
        "max_ns":values[SAMPLES-1],"production_local_store":true})
    );
}
fn cohort(rows: usize, value_bytes: usize, writers: usize) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("KV qualification", false).unwrap().id;
    let access = Access::owner(store.account().id);
    let address = seed(&store, &access, volume, value_bytes);
    let start = Barrier::new(writers + 1);
    let stop = AtomicBool::new(false);
    let samples = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for writer in 0..writers {
            let (store, access, start, stop) = (&store, &access, &start, &stop);
            workers.push(scope.spawn(move || {
                let limit = SAMPLES * 16;
                let mut value = vec![42; key_value::MAX_BYTES];
                let mut commits = 0;
                start.wait();
                for iteration in 0..limit {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    value[..8].copy_from_slice(&(iteration as u64).to_be_bytes());
                    store.kv_put(access, address, &[255, writer as u8 + 1], &value).unwrap();
                    commits += 1;
                }
                assert!(commits < limit, "concurrent writer exhausted its bound");
                commits
            }));
        }
        start.wait();
        let values = samples(&store, &access, address, rows, value_bytes);
        stop.store(true, Ordering::Release);
        let commits: Vec<_> = workers.into_iter().map(|job| job.join().unwrap()).collect();
        println!(
            "{}",
            serde_json::json!({"phase":"kv_concurrent_workers", "commits":commits,
                "limit_per_worker":SAMPLES * 16,"exhausted":false})
        );
        values
    });
    for (phase, values) in ["kv_keys", "kv_read", "kv_put", "kv_delete"].into_iter().zip(samples) {
        report(rows, value_bytes, writers, phase, values);
    }
    println!(
        "{}",
        serde_json::json!({"phase":"kv_retained_disk", "bytes":disk_bytes(directory.path())})
    );
}
fn disk_bytes(path: &std::path::Path) -> u64 {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() { disk_bytes(&path) } else { std::fs::metadata(path).unwrap().len() }
        })
        .sum()
}
pub fn run() {
    for rows in [1, 32, 64] {
        for value_bytes in [32, key_value::MAX_BYTES] {
            for writers in [0, 2] {
                cohort(rows, value_bytes, writers);
            }
        }
    }
}
