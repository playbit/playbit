#![allow(clippy::unwrap_used, reason = "Tests assert successful bounded-policy fixtures.")]

use super::*;
use crate::store::object_contract::{Grant, Subject};
use kvdb::{CreateOptions, storage::mem::MemStorage};

const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;
const BUDGET: usize = 32 * 1024;

#[test]
fn policy_preflight_charges_container_nodes_before_deserialization() {
    let mut budget = Budget { used: 0, limit: MetadataPolicy::MAX_MATERIALIZED_BYTES };
    assert!(matches!(
        budget.json(vec![b'['; MetadataPolicy::MAX_MATERIALIZED_BYTES / 512 + 1].as_slice()),
        Err(Error::LimitExceeded)
    ));
    let mut budget = Budget {
        used: MetadataPolicy::MAX_MATERIALIZED_BYTES - 3,
        limit: MetadataPolicy::MAX_MATERIALIZED_BYTES,
    };
    assert!(matches!(budget.json(b"\"x\"".as_slice()), Err(Error::LimitExceeded)));
}

#[test]
fn policy_preflight_handles_escaped_strings_across_fixed_chunk_boundaries() {
    let mut bytes = vec![b' '; 4094];
    bytes.extend_from_slice(b"\"\\\"[\" ");
    let mut budget = Budget { used: 0, limit: MetadataPolicy::MAX_MATERIALIZED_BYTES };
    budget.json(bytes.as_slice()).unwrap();
    assert_eq!(budget.used, 128 + 16);

    // A long packed context spans several chunks. Budget aggregation must
    // charge every byte and reject one-byte-under admission before decoding.
    let mut bytes = vec![b'"'];
    bytes.extend(std::iter::repeat_n(b'a', 3 * 4096));
    bytes.extend_from_slice(b"\\\"[\":0");
    let expected = 128 + 4 * (3 * 4096 + 4) + 512 + 4;
    for limit in [expected - 1, expected] {
        let mut budget = Budget { used: 0, limit };
        let result = budget.json(bytes.as_slice());
        assert_eq!(result.is_ok(), limit == expected);
        if result.is_ok() {
            assert_eq!(budget.used, expected);
        }
    }
}

fn fixture() -> Db<MemStorage> {
    let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
    let mut policy = MetadataPolicy::new(
        Volume {
            id: Id::from_bytes([1; 16]),
            name: "Bounded policy".into(),
            owner: Id::from_bytes([2; 16]),
            personal: true,
            share_key_with_service: false,
        },
        Acl {
            grants: vec![Grant { subject: Subject::Public, permissions: vec![Permission::Read] }],
        },
    )
    .unwrap();
    policy.provisioned_versions.insert(crate::identifier::encode(&{
        let mut bytes = [0; 32];
        bytes[31] = 1;
        bytes
    }));
    policy.write(&db, WALL).unwrap();
    let mut tx = db.begin_write(WALL).unwrap();
    for number in 0u16..1024 {
        let mut key = vec![crate::store::volume_index::META_BYTE, 3];
        key.extend(number.to_be_bytes());
        tx.put(&key, b"0").unwrap();
    }
    // Unknown keys between valid policy records must not terminate preflight.
    tx.put(&[VOLUME_KEY[0], VOLUME_KEY[1], 0], b"ignored").unwrap();
    tx.put(&POLICY_END, b"unrelated later namespace").unwrap();
    tx.commit().unwrap();
    db
}

#[test]
fn policy_preflight_skips_retained_history_and_preserves_current_authority() {
    let db = fixture();
    POLICY_VISITS.with(|visits| visits.set(0));
    let policy = MetadataPolicy::read_bounded(&db, BUDGET).unwrap();
    assert_eq!(POLICY_VISITS.with(|visits| visits.get()), 6);
    assert_eq!(
        policy.provisioned_versions,
        BTreeSet::from([crate::identifier::encode(&{
            let mut bytes = [0; 32];
            bytes[31] = 1;
            bytes
        })])
    );
    let visitor = Id::from_bytes([3; 16]);
    assert!(policy.authorize(visitor, Permission::Read).is_ok());
    assert!(matches!(policy.authorize(visitor, Permission::Write), Err(Error::AccessDenied(_))));
    assert!(policy.authorize(policy.volume.owner, Permission::Write).is_ok());

    let tx = db.begin_write(WALL).unwrap();
    POLICY_VISITS.with(|visits| visits.set(0));
    MetadataPolicy::validate_pending(&tx, BUDGET).unwrap();
    assert_eq!(POLICY_VISITS.with(|visits| visits.get()), 6);
}

fn add_versions(tx: &mut kvdb::WriteTxn<'_, MemStorage>) {
    for version in 2..102 {
        let mut key = PROVISION_PREFIX.to_vec();
        key.extend(
            crate::identifier::encode(&{
                let mut bytes = [0; 32];
                bytes[16..].copy_from_slice(&(version as u128).to_be_bytes());
                bytes
            })
            .as_bytes(),
        );
        tx.put(&key, b"true").unwrap();
    }
}

#[test]
fn policy_window_still_rejects_excess_allocation_before_read_or_commit() {
    let db = fixture();
    let mut tx = db.begin_write(WALL).unwrap();
    add_versions(&mut tx);
    assert!(matches!(MetadataPolicy::validate_pending(&tx, BUDGET), Err(Error::LimitExceeded)));
    drop(tx);
    assert_eq!(MetadataPolicy::read_bounded(&db, BUDGET).unwrap().provisioned_versions.len(), 1);

    // A low-level malformed/oversized bootstrap cannot bypass read admission.
    let mut tx = db.begin_write(WALL).unwrap();
    add_versions(&mut tx);
    tx.commit().unwrap();
    assert!(matches!(MetadataPolicy::read_bounded(&db, BUDGET), Err(Error::LimitExceeded)));
}

#[test]
fn pending_capacity_counts_effective_policy_values_and_preserves_rollback() {
    let db = fixture();
    let before = (db.clock(), db.begin_read().unwrap().root_hash().unwrap());
    let mut tx = db.begin_write(WALL).unwrap();
    add_versions(&mut tx);
    assert!(matches!(MetadataPolicy::validate_pending(&tx, BUDGET), Err(Error::LimitExceeded)));
    drop(tx);
    assert_eq!((db.clock(), db.begin_read().unwrap().root_hash().unwrap()), before);

    let mut tx = db.begin_write(WALL).unwrap();
    add_versions(&mut tx);
    for version in 2..102 {
        let mut key = PROVISION_PREFIX.to_vec();
        key.extend(
            crate::identifier::encode(&{
                let mut bytes = [0; 32];
                bytes[16..].copy_from_slice(&(version as u128).to_be_bytes());
                bytes
            })
            .as_bytes(),
        );
        tx.del(&key).unwrap();
    }
    MetadataPolicy::validate_pending(&tx, BUDGET).unwrap();
    tx.commit().unwrap();
    assert_eq!(MetadataPolicy::read_bounded(&db, BUDGET).unwrap().provisioned_versions.len(), 1);
}

#[test]
fn incoming_policy_values_are_charged_even_before_conflict_resolution() {
    let source = fixture();
    let mut tx = source.begin_write_with_delta(WALL, 64 * 1024).unwrap();
    let mut value = record::encode(&Acl::default()).unwrap();
    value.extend_from_slice(&99u16.to_le_bytes());
    value.extend_from_slice(&64u32.to_le_bytes());
    value.extend_from_slice(&[255; 64]);
    tx.put(&DEFAULT_PERMISSIONS_KEY, &value).unwrap();
    let kvdb::CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
    assert!(matches!(
        MetadataPolicy::validate_delta_materialization(&bytes, BUDGET),
        Err(Error::LimitExceeded)
    ));
}

#[test]
fn invalid_host_ceilings_fail_before_catalog_reads_or_writes() {
    let db = Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap();
    for limit in [0, BASE_BYTES - 1, MetadataPolicy::MAX_MATERIALIZED_BYTES + 1, usize::MAX] {
        assert!(matches!(MetadataPolicy::read_bounded(&db, limit), Err(Error::LimitExceeded)));
        let tx = db.begin_write(1).unwrap();
        assert!(matches!(MetadataPolicy::validate_pending(&tx, limit), Err(Error::LimitExceeded)));
    }
    let tx = db.begin_write(1).unwrap();
    assert!(matches!(
        MetadataPolicy::validate_pending(&tx, BASE_BYTES),
        Err(Error::UnsupportedVolumeFormat)
    ));
}

#[test]
fn unsupported_volume_formats_are_rejected_before_entry_decoding_or_policy_changes() {
    // A missing/unknown marker must never be interpreted as the current schema,
    // even if the rest of the volume is readable. Policy edits cannot stamp it.
    for marker in [None, Some(b"0".as_slice()), Some(b"2".as_slice()), Some(b"255".as_slice())] {
        let db = fixture();
        let policy = MetadataPolicy::read(&db).unwrap();
        let mut tx = db.begin_write(WALL).unwrap();
        match marker {
            Some(value) => tx.put(&FORMAT_KEY, value).unwrap(),
            None => {
                tx.del(&FORMAT_KEY).unwrap();
            }
        }
        tx.commit().unwrap();
        let before = (db.clock(), db.begin_read().unwrap().root_hash().unwrap());
        assert!(matches!(MetadataPolicy::read(&db), Err(Error::UnsupportedVolumeFormat)));
        assert!(matches!(policy.write(&db, WALL), Err(Error::UnsupportedVolumeFormat)));
        assert_eq!((db.clock(), db.begin_read().unwrap().root_hash().unwrap()), before);
        let tx = db.begin_write(WALL).unwrap();
        assert!(matches!(
            MetadataPolicy::validate_pending(&tx, BUDGET),
            Err(Error::UnsupportedVolumeFormat)
        ));
        assert!(matches!(
            validate_metadata_entry(policy.volume(), &FORMAT_KEY, marker),
            Err(Error::UnsupportedVolumeFormat)
        ));
    }
}
