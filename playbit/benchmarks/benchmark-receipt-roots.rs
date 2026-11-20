//! Receipt-root diagnostic. Run optimized; timings exclude fixture construction.
//! cargo run --release --locked -p playbit --example benchmark-receipt-roots -- 1000
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::store::{
    Access, DatabaseId, DatabasePolicy, Id, LocalStore, MetadataPolicy, OutboundSnapshot,
    ReplicaReader, ReplicaRoot, Result,
    object_contract::{Schema, TypeDeclaration},
    outbound_replica, projection_root,
};
use serde_json::json;
use std::{collections::BTreeMap, time::Instant};

fn measure(
    count: usize,
    expected: ReplicaRoot,
    mut read: impl FnMut() -> Result<ReplicaRoot>,
) -> Result<serde_json::Value> {
    for _ in 0..10 {
        assert_eq!(read()?, expected);
    }
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let root = read()?;
        let ns = start.elapsed().as_nanos() as u64;
        assert_eq!(root, expected);
        samples.push(ns);
    }
    let mut ordered = samples.clone();
    ordered.sort_unstable();
    let percentile = |p: usize| ordered[(count * p).div_ceil(100) - 1];
    Ok(json!({"count": count, "p50_ns": percentile(50), "p95_ns": percentile(95),
        "p99_ns": percentile(99), "max_ns": ordered[count - 1], "samples_ns": samples}))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert!(args.len() <= 1, "expected optional sample count (1..=1000)");
    let count = args.first().map(|s| s.parse::<usize>().expect("sample count")).unwrap_or(1000);
    assert!((1..=1000).contains(&count));
    let directory = tempfile::tempdir().expect("fixture directory");
    let store = LocalStore::open(directory.path())?;
    let access = Access::owner(store.account().id);
    let type_id = Id::from_bytes([1; 16]);
    let schema = Schema {
        types: BTreeMap::from([(
            type_id,
            TypeDeclaration { name: "Fixture".into(), deprecated: false, fields: BTreeMap::new() },
        )]),
    };
    for objects in [32, 500] {
        let volume = store.create_volume("Receipt fixture", true)?;
        let entry = store.create_database(
            &access,
            volume.id,
            "data",
            playbit::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
        )?;
        let address =
            playbit::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };
        for _ in 0..objects {
            store.create_object_with_fields(&access, address, type_id, &BTreeMap::new())?;
        }
        // One setup transaction; subsequent measurements are read-only. Database
        // 2 has no per-entry destination filtering. Synthetic padding exercises
        // its size without representing fabricated account revocations.
        store.with_replica(volume.id, DatabaseId::Revocations, |db| {
            let mut tx = db.begin_write(1)?;
            for index in 0u32..10_000 {
                let mut key = [0x42; 16];
                key[12..].copy_from_slice(&index.to_be_bytes());
                let mut value = [0x61; 400];
                value[..4].copy_from_slice(&index.to_be_bytes());
                tx.put(&key, &value)?;
            }
            tx.commit()?;
            Ok(())
        })?;
        for database in [DatabaseId::Index, DatabaseId::Revocations, DatabaseId::Entry(entry.id)] {
            let policy = store.with_replica(volume.id, DatabaseId::Index, |db| {
                DatabasePolicy::read(db, database)
            })?;
            let expected = store.with_replica(volume.id, database, |db| {
                outbound_replica(&std::env::temp_dir(), db, &policy, ReplicaReader::Service)?.root()
            })?;
            let admitted = store.with_replica(volume.id, database, |db| {
                measure(count, expected, || projection_root(db, &policy, ReplicaReader::Service))
            })?;
            let capture = store.with_replica(volume.id, database, |db| {
                // Time only pinning and the matching revision reads. Projection
                // and teardown stay outside the interval, as in AP admission.
                let mut samples = Vec::with_capacity(count);
                for index in 0..count + 10 {
                    let start = Instant::now();
                    let snapshot = OutboundSnapshot::new(db)?;
                    let revisions = (db.clock().as_u64(), policy.volume.revision());
                    let ns = start.elapsed().as_nanos() as u64;
                    std::hint::black_box(revisions);
                    if index == count + 9 {
                        assert_eq!(
                            snapshot
                                .project(&std::env::temp_dir(), &policy, ReplicaReader::Service)?
                                .root()?,
                            expected
                        );
                    } else {
                        drop(snapshot);
                    }
                    if index >= 10 {
                        samples.push(ns);
                    }
                }
                let mut ordered = samples.clone();
                ordered.sort_unstable();
                let at = |p: usize| ordered[(count * p).div_ceil(100) - 1];
                Ok(json!({"count": count, "p50_ns": at(50), "p95_ns": at(95),
                    "p99_ns": at(99), "max_ns": ordered[count - 1], "samples_ns": samples}))
            })?;
            let pending_capacity = if database == DatabaseId::Index {
                Some(store.with_replica(volume.id, database, |db| {
                    let tx = db.begin_write(1)?;
                    measure(count, expected, || {
                        MetadataPolicy::validate_pending(
                            &tx,
                            MetadataPolicy::MAX_MATERIALIZED_BYTES,
                        )?;
                        Ok(tx.root_hash()?)
                    })
                })?)
            } else {
                None
            };
            let current_policy = measure(count, expected, || {
                store.projection_root(volume.id, database, ReplicaReader::Service)
            })?;
            println!(
                "{}",
                json!({"objects": objects, "database": database,
                "revocation_padding_records": 10000, "padding_key_bytes": 16,
                "padding_value_bytes": 400, "root": expected,
                "admitted_snapshot_root": admitted, "outbound_snapshot_capture": capture,
                "pending_policy_capacity_and_root": pending_capacity,
                "current_policy_and_root": current_policy})
            );
        }
        store.with_replica(volume.id, DatabaseId::Revocations, |db| {
            let tx = db.begin_read()?;
            for index in 0u32..10_000 {
                let mut key = [0x42; 16];
                key[12..].copy_from_slice(&index.to_be_bytes());
                let mut expected = [0x61; 400];
                expected[..4].copy_from_slice(&index.to_be_bytes());
                assert_eq!(tx.get(&key)?.unwrap().as_ref(), expected);
            }
            Ok(())
        })?;
    }
    Ok(())
}
