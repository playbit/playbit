//! Production LocalStore authorization, fair admission and durable name repair.
//! cargo run --locked --release -p playbit --no-default-features --features local-store --example benchmark-volume-index
//! Untimed index-size setup seeds pending declarations, never fake ready backings.
//! Measurements exclude backing creation, AP, IPC, UI and physical presentation.
#![allow(clippy::unwrap_used, reason = "Operator fixtures assert successful operations.")]
use playbit::store::{
    Access, DatabaseId, DatabasePolicy, EntryId, Id, LocalStore,
    object_contract::{
        FieldDeclaration, Permission, Principal, Schema, TypeDeclaration, ValueType,
    },
    volume_index::{self, DatabaseProtocol, EntryAddress, EntryInfo, EntryKind},
};
use std::collections::BTreeMap;
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
const SAMPLES: usize = 1000;
#[path = "volume_index_kv.rs"]
mod key_values;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Admission {
    Policy,
    Catalog,
    Summary,
    InitialSummary,
}
struct Row {
    id: EntryId,
    names: [String; 2],
}
fn rows(count: usize, name_bytes: usize) -> Vec<Row> {
    (0..count)
        .map(|number| {
            let mut bytes = [0; 15];
            bytes[7..].copy_from_slice(&(number as u64 + 1).to_be_bytes());
            let names = [0, 1].map(|version| {
                let mut name = format!("items/{number:08}/{version}");
                name.extend(std::iter::repeat_n('x', name_bytes - name.len()));
                name
            });
            Row { id: EntryId::from_bytes(bytes).unwrap(), names }
        })
        .collect()
}
fn setup(store: &LocalStore, volume: Id, rows: &[Row]) {
    store
        .with_replica(volume, DatabaseId::Index, |index| {
            for rows in rows.chunks(32) {
                let mut tx = index.begin_write(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                )?;
                for row in rows {
                    volume_index::create(
                        &mut tx,
                        row.id,
                        &EntryInfo {
                            namespace: Default::default(),
                            origin: playbit::store::volume_index::namespace::Origin {
                                installation: playbit::store::Id::from_bytes([42; 16]),
                                label: None,
                            },
                            initial_permissions: playbit::store::object_contract::EntryAcl::Inherit,
                            created_revision: EntryId::generate()?,
                            name: row.names[0].clone(),
                            protocol: Some(DatabaseProtocol::Kvdb),
                            kind: EntryKind::KvDb {},
                        },
                    )?;
                }
                tx.commit()?;
            }
            Ok(())
        })
        .unwrap();
}
fn sample(store: &LocalStore, access: &Access, volume: Id, row: &Row) -> [u128; 4] {
    let start = Instant::now();
    let current = store.entry(access, volume, row.id).unwrap();
    let read = start.elapsed().as_nanos();
    let next = if current.info.name == row.names[0] { &row.names[1] } else { &row.names[0] };
    let start = Instant::now();
    store.rename_entry(access, volume, row.id, next).unwrap();
    let rename = start.elapsed().as_nanos();
    let start = Instant::now();
    assert_eq!(store.resolve_entry(access, volume, next).unwrap().id, row.id);
    let resolve = start.elapsed().as_nanos();
    let start = Instant::now();
    let page = store.entries(access, volume, None, 64).unwrap();
    assert!(!page.entries.is_empty());
    [read, rename, resolve, start.elapsed().as_nanos()]
}
fn report(
    entries: usize,
    name_bytes: usize,
    writers: usize,
    bulk: bool,
    phase: &str,
    mut samples: Vec<u128>,
) {
    samples.sort_unstable();
    let n = samples.len();
    println!(
        "{{\"phase\":\"{phase}\",\"entries\":{entries},\"name_bytes\":{name_bytes},\"writers\":{writers},\"bulk\":{bulk},\"production_local_store\":true,\"samples\":{n},\"p50_ns\":{},\"p99_ns\":{},\"max_ns\":{}}}",
        samples[n / 2 - 1],
        samples[n * 99 / 100 - 1],
        samples[n - 1]
    );
}
fn cohort(entries: usize, name_bytes: usize, writers: usize, bulk: bool, samples: usize) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let volume = store.create_volume("Volume index qualification", false).unwrap().id;
    let access = Access::owner(store.account().id);
    let rows = rows(entries, name_bytes);
    setup(&store, volume, &rows);
    let blobs = if bulk {
        ["bulk/a", "bulk/b"]
            .map(|name| store.create_blob(&access, volume, name, &[]).unwrap().id)
            .to_vec()
    } else {
        Vec::new()
    };
    let start = Barrier::new(writers + blobs.len());
    let stop = AtomicBool::new(false);
    let elapsed = Instant::now();
    let samples = std::thread::scope(|scope| {
        let mut bulk_jobs = Vec::new();
        for entry in &blobs {
            let (store, access, start, stop) = (&store, &access, &start, &stop);
            bulk_jobs.push(scope.spawn(move || {
                let mut bytes = vec![42; 64 * 1024];
                let mut commits = 0u64;
                let mut contentions = 0u64;
                start.wait();
                let mut attempts = 0;
                while !stop.load(Ordering::Acquire) && attempts < samples * 8 {
                    attempts += 1;
                    bytes[..8].copy_from_slice(&commits.to_le_bytes());
                    match store.write_blob(access, volume, *entry, &bytes) {
                        Ok(_) => commits += 1,
                        Err(playbit::store::Error::LocalContention(_)) => contentions += 1,
                        Err(error) => panic!("bulk replacement failed: {error}"),
                    }
                }
                (commits, contentions)
            }));
        }
        let mut tasks = Vec::with_capacity(writers);
        for writer in 0..writers {
            let (store, access, start, rows) = (&store, &access, &start, &rows);
            tasks.push(scope.spawn(move || {
                start.wait();
                (0..samples)
                    .map(|iteration| {
                        sample(
                            store,
                            access,
                            volume,
                            &rows[(iteration * writers + writer) % entries],
                        )
                    })
                    .collect::<Vec<_>>()
            }));
        }
        let samples: Vec<_> = tasks.into_iter().flat_map(|task| task.join().unwrap()).collect();
        stop.store(true, Ordering::Release);
        let (commits, contentions) = bulk_jobs
            .into_iter()
            .map(|job| job.join().unwrap())
            .fold((0, 0), |(commits, contentions), next| (commits + next.0, contentions + next.1));
        if bulk {
            assert!(commits >= 2);
        }
        println!(
            "{{\"entries\":{entries},\"writers\":{writers},\"concurrent_blob_commits\":{commits},\"concurrent_blob_contentions\":{contentions},\"elapsed_ns\":{}}}",
            elapsed.elapsed().as_nanos()
        );
        samples
    });
    for (index, phase) in
        ["entry_read", "rename_durable", "resolve", "entry_page"].into_iter().enumerate()
    {
        report(
            entries,
            name_bytes,
            writers,
            bulk,
            phase,
            samples.iter().map(|sample| sample[index]).collect(),
        );
    }
}
fn admission_schema(store: &LocalStore, access: &Access, volume: Id) -> EntryAddress {
    let type_id = Id::from_bytes([0x80; 16]);
    let declaration = |fields| Schema {
        types: BTreeMap::from([(
            type_id,
            TypeDeclaration { name: "Row".into(), deprecated: false, fields },
        )]),
    };
    let entry = store
        .create_database(
            access,
            volume,
            "large-schema",
            EntryKind::ObjectDb { schema: declaration(BTreeMap::new()) },
        )
        .unwrap();
    let address = EntryAddress { volume, entry: entry.id };
    for batch in 0..32u64 {
        let fields = (batch * 32..(batch + 1) * 32)
            .map(|number| {
                let mut bytes = [0; 16];
                bytes[8..].copy_from_slice(&(number + 1).to_be_bytes());
                (
                    Id::from_bytes(bytes),
                    FieldDeclaration {
                        name: format!("f{number}"),
                        value_type: ValueType::Text,
                        repeated: false,
                        deprecated: false,
                    },
                )
            })
            .collect();
        store.establish_schema(access, address, &declaration(fields)).unwrap();
    }
    assert_eq!(store.schema(access, address).unwrap().types[&type_id].fields.len(), 1024);
    address
}

fn catalog_scan(store: &LocalStore, volume: Id) {
    let mut after = None;
    let mut count = 0;
    loop {
        let page = store.replication_database_page(volume, after).unwrap();
        count += page.databases.len();
        let Some(next) = page.next else { break };
        after = Some(next);
    }
    assert_eq!(count, 1003); // Two reserved controls and 1,001 named databases.
}

fn seed_initial_schemas(store: &LocalStore, volume: Id) -> EntryAddress {
    let declaration = Schema {
        types: BTreeMap::from([(
            Id::from_bytes([0x80; 16]),
            TypeDeclaration {
                name: "Row".into(),
                deprecated: false,
                fields: (0..160_u64)
                    .map(|number| {
                        let mut bytes = [0; 16];
                        bytes[8..].copy_from_slice(&(number + 1).to_be_bytes());
                        let mut name = format!("f{number}");
                        name.extend(std::iter::repeat_n('x', 255 - name.len()));
                        (
                            Id::from_bytes(bytes),
                            FieldDeclaration {
                                name,
                                value_type: ValueType::Text,
                                repeated: false,
                                deprecated: false,
                            },
                        )
                    })
                    .collect(),
            },
        )]),
    };
    let first = EntryId::from_bytes([0x40; 15]).unwrap();
    store
        .with_replica(volume, DatabaseId::Index, |index| {
            for number in 0..64_u64 {
                let mut bytes = *first.as_bytes();
                bytes[7..].copy_from_slice(&number.to_be_bytes());
                let id = EntryId::from_bytes(bytes)?;
                let mut tx = index.begin_write(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                )?;
                volume_index::create(
                    &mut tx,
                    id,
                    &EntryInfo {
                        namespace: Default::default(),
                        origin: playbit::store::volume_index::namespace::Origin {
                            installation: playbit::store::Id::from_bytes([42; 16]),
                            label: None,
                        },
                        initial_permissions: playbit::store::object_contract::EntryAcl::Inherit,
                        created_revision: EntryId::generate()?,
                        name: format!("initial/{number:08}"),
                        protocol: Some(DatabaseProtocol::Kvdb),
                        kind: EntryKind::ObjectDb { schema: declaration.clone() },
                    },
                )?;
                tx.commit()?;
            }
            Ok(())
        })
        .unwrap();
    let mut bytes = *first.as_bytes();
    bytes[7..].fill(0);
    EntryAddress { volume, entry: EntryId::from_bytes(bytes).unwrap() }
}

fn admission_samples(
    store: &LocalStore,
    volume: Id,
    address: EntryAddress,
    mode: Admission,
    after: EntryId,
) -> Vec<u128> {
    (0..SAMPLES)
        .map(|_| {
            let started = Instant::now();
            match mode {
                Admission::Catalog => catalog_scan(store, volume),
                Admission::Summary | Admission::InitialSummary => {
                    let page = store
                        .entries(&Access::owner(store.account().id), volume, Some(after), 64)
                        .unwrap();
                    if mode == Admission::Summary {
                        assert_eq!(page.entries.len(), 64);
                    } else {
                        assert!(!page.entries.is_empty() && page.entries.len() < 64);
                        assert!(page.next.is_some());
                    }
                    assert!(page.entries.iter().any(|entry| entry.id == address.entry));
                }
                Admission::Policy => store
                    .with_replica(volume, DatabaseId::Index, |index| {
                        let policy = DatabasePolicy::read(index, DatabaseId::Entry(address.entry))?;
                        policy.authorize(Principal::Human(store.account().id), Permission::Read)
                    })
                    .unwrap(),
            }
            started.elapsed().as_nanos()
        })
        .collect()
}

fn admission_cohort(concurrent: bool, mode: Admission) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Admission", true).unwrap().id;
    let access = Access::owner(store.account().id);
    let rows = rows(1000, 32);
    setup(&store, volume, &rows);
    let address = admission_schema(&store, &access, volume);
    let (address, after) = if mode == Admission::InitialSummary {
        (seed_initial_schemas(&store, volume), rows[999].id)
    } else {
        (address, rows[936].id)
    };
    let start = Barrier::new(1 + 2 * usize::from(concurrent));
    let stop = AtomicBool::new(false);
    let samples = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for row in rows.iter().take(2 * usize::from(concurrent)) {
            let (store, access, start, stop) = (&store, &access, &start, &stop);
            workers.push(scope.spawn(move || {
                start.wait();
                let limit = SAMPLES * 128;
                let mut commits = 0;
                for number in 0..limit {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    store.rename_entry(access, volume, row.id, &row.names[number % 2]).unwrap();
                    commits += 1;
                }
                (commits, commits == limit)
            }));
        }
        start.wait();
        let samples = admission_samples(&store, volume, address, mode, after);
        stop.store(true, Ordering::Release);
        let commits: Vec<_> = workers
            .into_iter()
            .map(|worker| {
                let (commits, exhausted) = worker.join().unwrap();
                assert!(!exhausted, "admission timing exhausted its concurrent worker bound");
                commits
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"phase":"admission_concurrent_workers", "commits":commits,
            "limit_per_worker":SAMPLES * 128, "exhausted":false})
        );
        samples
    });
    report(
        1000,
        32,
        1 + 2 * usize::from(concurrent),
        concurrent,
        match mode {
            Admission::Catalog => "catalog_scan_1001_databases_1024_fields",
            Admission::Policy => "policy_admission_1024_fields",
            Admission::Summary => "summary_64_rows_including_1024_fields",
            Admission::InitialSummary => "summary_160_initial_fields_255_byte_names",
        },
        samples,
    );
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--kv") {
        key_values::run();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--admission") {
        admission_cohort(false, Admission::Policy);
        admission_cohort(true, Admission::Policy);
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--catalog") {
        admission_cohort(false, Admission::Catalog);
        admission_cohort(true, Admission::Catalog);
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--summaries") {
        admission_cohort(false, Admission::Summary);
        admission_cohort(true, Admission::Summary);
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--initial-summaries") {
        admission_cohort(false, Admission::InitialSummary);
        admission_cohort(true, Admission::InitialSummary);
        return;
    }
    for (entries, name_bytes, writers, bulk) in [
        (1, 32, 1, false),
        (32, 32, 1, false),
        (64, 32, 1, false),
        (500, 32, 1, false),
        (10000, 32, 1, false),
        (32, 480, 1, false),
        (500, 32, 4, false),
        (64, 32, 4, true),
    ] {
        cohort(entries, name_bytes, writers, bulk, SAMPLES);
    }
}
