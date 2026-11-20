//! Compare receipt-root lookups with the actual detached projection protocol.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::store::object_contract::{Grant, Principal, Subject};

const CACHE_BYTES: usize = 512 * 1024;

fn fixture(service: bool) -> (tempfile::TempDir, LocalStore, Volume, Id) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Projection roots", service).unwrap();
    let participant = Id::from_bytes([77; 16]);
    store
        .set_permissions(
            &Access::owner(store.account().id),
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(participant)),
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    (directory, store, volume, participant)
}

fn key(database: crate::store::DatabaseId, byte: u8) -> Vec<u8> {
    match database {
        crate::store::DatabaseId::Index => {
            format!("provision/{}", crate::identifier::encode(&[byte; 32])).into_bytes()
        }
        crate::store::DatabaseId::Revocations => {
            Id::from_bytes([byte; 16]).to_string().into_bytes()
        }
        _ => unreachable!(),
    }
}

fn write(source: &Database, key: &[u8], value: Option<&[u8]>) {
    let mut transaction = source.begin_write(now_unix_secs()).unwrap();
    match value {
        Some(value) => transaction.put(key, value).unwrap(),
        None => {
            transaction.del(key).unwrap();
        }
    };
    transaction.commit().unwrap();
}

fn assert_free_matches_projection(
    source: &Database,
    policy: &MetadataPolicy,
    database: crate::store::DatabaseId,
    readers: &[ReplicaReader],
) -> ReplicaRoot {
    let original = replica_root(source).unwrap();
    for &reader in readers {
        let admission = DatabasePolicy::reserved(policy.clone(), database).unwrap();
        let projected =
            outbound_replica(&std::env::temp_dir(), source, &admission, reader).unwrap();
        assert_eq!(projected.root().unwrap(), original);
        assert_eq!(projection_root(source, &admission, reader).unwrap(), projected.root().unwrap());
    }
    assert_eq!(replica_root(source).unwrap(), original, "projection must not mutate its source");
    original
}

fn assert_local_matches_projection(
    store: &LocalStore,
    volume: Id,
    database: crate::store::DatabaseId,
    readers: &[ReplicaReader],
) -> ReplicaRoot {
    let policy = store.index(volume).unwrap();
    let source = store.db(volume, database).unwrap();
    let original = assert_free_matches_projection(&source, &policy, database, readers);
    for &reader in readers {
        assert_eq!(store.projection_root(volume, database, reader).unwrap(), original);
    }
    original
}

#[test]
fn unfiltered_roots_match_real_projection_for_empty_live_deleted_reinserted_and_reopened_data() {
    let (_directory, store, volume, participant) = fixture(true);
    let policy = store.index(volume.id).unwrap();
    let readers = [
        ReplicaReader::Service,
        ReplicaReader::Participant(volume.owner),
        ReplicaReader::Participant(participant),
    ];
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        // Free DB1 projection also accepts an empty snapshot. A LocalStore DB1
        // cannot be empty because its current admission policy must exist there.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.kvdb");
        let source = create_replica(&path, [42; 32], CACHE_BYTES).unwrap();
        assert_eq!(assert_free_matches_projection(&source, &policy, database, &readers), None);
        let first = key(database, 11);
        let second = key(database, 12);
        write(&source, &first, Some(b"true"));
        write(&source, &second, Some(b"true"));
        let live = assert_free_matches_projection(&source, &policy, database, &readers);
        write(&source, &first, None);
        assert!(source.begin_read().unwrap().is_del(&first).unwrap().is_some());
        let mixed = assert_free_matches_projection(&source, &policy, database, &readers);
        assert_ne!(mixed, live);
        write(&source, &second, None);
        let deleted = assert_free_matches_projection(&source, &policy, database, &readers);
        assert!(deleted.is_some(), "tombstones are retained in the canonical root");
        write(&source, &first, Some(b"true"));
        assert!(source.begin_read().unwrap().is_del(&first).unwrap().is_none());
        let reinserted = assert_free_matches_projection(&source, &policy, database, &readers);
        assert_ne!(reinserted, deleted);
        assert_ne!(reinserted, live, "original entry clocks must participate in the root");
        drop(source);
        let reopened = open_replica(&path, CACHE_BYTES).unwrap();
        assert_eq!(
            assert_free_matches_projection(&reopened, &policy, database, &readers),
            reinserted
        );
    }
}

#[test]
fn local_unfiltered_roots_match_real_projection_through_tombstones_and_restart() {
    let (directory, store, volume, participant) = fixture(true);
    let readers = [
        ReplicaReader::Service,
        ReplicaReader::Participant(volume.owner),
        ReplicaReader::Participant(participant),
    ];
    assert_eq!(
        assert_local_matches_projection(
            &store,
            volume.id,
            crate::store::DatabaseId::Revocations,
            &readers
        ),
        None
    );
    let mut final_roots = [None, None];
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        let source = store.db(volume.id, database).unwrap();
        let first = key(database, 21);
        let second = key(database, 22);
        assert_local_matches_projection(&store, volume.id, database, &readers);
        for (key, value) in [
            (&first, Some(b"true".as_slice())),
            (&second, Some(b"true".as_slice())),
            (&first, None),
            (&second, None),
            (&first, Some(b"true".as_slice())),
        ] {
            write(&source, key, value);
            assert_local_matches_projection(&store, volume.id, database, &readers);
        }
        final_roots[usize::from(database == crate::store::DatabaseId::Revocations)] =
            replica_root(&source).unwrap();
    }
    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        assert_eq!(
            assert_local_matches_projection(&reopened, volume.id, database, &readers),
            final_roots[usize::from(database == crate::store::DatabaseId::Revocations)]
        );
    }
}

#[test]
fn root_lookup_keeps_reader_admission_even_when_database_is_empty() {
    for service in [false, true] {
        let (_directory, store, volume, participant) = fixture(service);
        let policy = store.index(volume.id).unwrap();
        let denied = ReplicaReader::Participant(Id::from_bytes([78; 16]));
        for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
            let source = store.db(volume.id, database).unwrap();
            for reader in
                [ReplicaReader::Participant(volume.owner), ReplicaReader::Participant(participant)]
            {
                assert_free_matches_projection(&source, &policy, database, &[reader]);
                assert_eq!(
                    store.projection_root(volume.id, database, reader).unwrap(),
                    replica_root(&source).unwrap()
                );
            }
            assert!(matches!(
                projection_root(
                    &source,
                    &DatabasePolicy::reserved(policy.clone(), database).unwrap(),
                    denied
                ),
                Err(Error::AccessDenied(_))
            ));
            assert!(matches!(
                store.projection_root(volume.id, database, denied),
                Err(Error::AccessDenied(_))
            ));
            if !service {
                assert!(matches!(
                    projection_root(
                        &source,
                        &DatabasePolicy::reserved(policy.clone(), database).unwrap(),
                        ReplicaReader::Service
                    ),
                    Err(Error::AccessDenied("waiting for an authorized peer"))
                ));
                assert!(matches!(
                    store.projection_root(volume.id, database, ReplicaReader::Service),
                    Err(Error::AccessDenied("waiting for an authorized peer"))
                ));
            }
        }
    }
}

#[test]
fn root_lookup_preserves_plain_mode_and_unknown_database_errors() {
    let (directory, store, volume, _) = fixture(true);
    let policy = store.index(volume.id).unwrap();
    let plain = create_db(&directory.path().join("plain.kvdb"), None, CACHE_BYTES).unwrap();
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        for reader in [ReplicaReader::Service, ReplicaReader::Participant(volume.owner)] {
            assert!(matches!(
                projection_root(
                    &plain,
                    &DatabasePolicy::reserved(policy.clone(), database).unwrap(),
                    reader
                ),
                Err(Error::Store(kvdb::Error::Mode))
            ));
        }
    }
    let unknown =
        crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([78; 15]).unwrap());
    assert!(store.projection_root(volume.id, unknown, ReplicaReader::Service).is_err());
    assert!(
        !directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(volume.id.to_string())
            .join(unknown.file_name())
            .exists()
    );
    // A plain replacement is invalid for a registered replica, not an empty
    // root that may be acknowledged. This is an isolated fault fixture only.
    store.close_db(volume.id, crate::store::DatabaseId::Revocations).unwrap();
    let path = directory
        .path()
        .join(crate::paths::VOLUMES)
        .join(volume.id.to_string())
        .join(crate::store::DatabaseId::Revocations.file_name());
    fs::remove_file(&path).unwrap();
    drop(create_db(&path, None, CACHE_BYTES).unwrap());
    assert!(matches!(
        store.projection_root(
            volume.id,
            crate::store::DatabaseId::Revocations,
            ReplicaReader::Service
        ),
        Err(Error::Store(kvdb::Error::Mode))
    ));
}

#[test]
fn current_root_rejects_old_receipt_after_edit_while_pinned_snapshot_is_unchanged() {
    let (_directory, store, volume, _) = fixture(true);
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        let source = store.db(volume.id, database).unwrap();
        let entry = key(database, 31);
        write(&source, &entry, Some(b"true"));
        let policy = store.index(volume.id).unwrap();
        let pinned = source.begin_read().unwrap();
        let old_root = pinned.root_hash().unwrap();
        let snapshot = OutboundSnapshot::new(&source).unwrap();
        let receipt =
            DurableRevision { revision: 1, policy_revision: policy.revision(), root: old_root };
        store.acknowledge_replica(volume.id, database, &receipt).unwrap();
        write(&source, &entry, None);
        assert_eq!(pinned.root_hash().unwrap(), old_root);
        let detached_old = snapshot
            .project(
                &std::env::temp_dir(),
                &DatabasePolicy::reserved(policy.clone(), database).unwrap(),
                ReplicaReader::Service,
            )
            .unwrap();
        assert_eq!(detached_old.root().unwrap(), old_root);
        let current =
            assert_local_matches_projection(&store, volume.id, database, &[ReplicaReader::Service]);
        assert_ne!(
            current, receipt.root,
            "the current-root receipt comparison must notice the later edit"
        );
        assert_eq!(
            store.durable_revision(volume.id, database).unwrap(),
            Some(receipt),
            "a current-root read must not rewrite the exact older AP acknowledgment"
        );
    }
}
