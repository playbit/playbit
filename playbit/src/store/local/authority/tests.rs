#![allow(clippy::unwrap_used, reason = "Regressions assert persistent authority boundaries.")]
use super::*;
use crate::store::{
    BuildSelector,
    volume_index::{self, EntryAddress, EntryKind},
};

fn setup() -> (tempfile::TempDir, LocalStore, Volume, Access) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Authority", true).unwrap();
    let access = Access::owner(store.account().id);
    (directory, store, volume, access)
}
fn denied<T>(result: Result<T>) {
    assert!(matches!(result, Err(Error::AccessDenied(_))));
}

#[test]
fn session_admission_cannot_erase_newer_denials_or_reuse_a_cleared_generation() {
    // Reordered and duplicate negative receipts remain fenced through reopen.
    let (directory, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let before = store.session_denial_generation(&session).unwrap();
    assert_eq!(before, 0);
    store.record_session_denial(&session, "revoked").unwrap();
    assert!(!store.clear_session_denial(&session, before).unwrap());
    let once = store.session_denial_generation(&session).unwrap();
    store.record_session_denial(&session, "revoked").unwrap();
    assert!(!store.clear_session_denial(&session, once).unwrap());
    let twice = store.session_denial_generation(&session).unwrap();
    assert_eq!(twice, once + 1);
    assert!(store.clear_session_denial(&session, twice).unwrap());
    assert_eq!(store.session_denial_generation(&session).unwrap(), twice);
    assert!(store.session_denial(&session.id).unwrap().is_none());
    store.record_session_denial(&session, "revoked again").unwrap();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(!store.clear_session_denial(&session, twice).unwrap());
    assert_eq!(store.session_denial(&session.id).unwrap().as_deref(), Some("revoked again"));
    let current = store.session_denial_generation(&session).unwrap();
    assert!(store.clear_session_denial(&session, current).unwrap());
    assert!(store.session_denial(&session.id).unwrap().is_none());
}

#[test]
fn delayed_session_admission_cannot_clear_a_denial_after_actual_build_adoption() {
    // The exact stored binding is checked again at clear publication.
    let (_directory, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let captured = store.session_denial_generation(&session).unwrap();
    let mut updated = session.clone();
    updated.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
    updated.selector = BuildSelector::Exact { build_id: updated.resolved_build_id.clone() };
    store.adopt_session(&session, &updated).unwrap();
    assert!(!store.clear_session_denial(&session, captured).unwrap());
    denied(store.session_denial_generation(&session));
    assert!(!store.record_session_denial(&session, "old transfer denied").unwrap());
    #[cfg(feature = "connected-engine")]
    assert!(
        !store
            .receive_authority_terminal(
                &Access::session_version(store.account().id, &session.id, &build),
                root.id,
                root.id,
                crate::network::ap::replication::TerminalCause::ParticipantDenied,
            )
            .unwrap()
    );
    assert_eq!(store.session_denial_generation(&updated).unwrap(), captured);
    assert!(store.session_denial(&updated.id).unwrap().is_none());
    assert!(store.record_session_denial(&updated, "new binding denied").unwrap());
    let current = store.session_denial_generation(&updated).unwrap();
    assert!(!store.clear_session_denial(&session, current).unwrap());
    assert_eq!(store.session_denial(&updated.id).unwrap().as_deref(), Some("new binding denied"));
    assert!(store.clear_session_denial(&updated, current).unwrap());
}

#[test]
fn session_denial_records_are_bounded_strict_and_fail_closed_at_exhaustion() {
    // Corrupt persistent input is refused before it can clear authority.
    let (_directory, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let oversized = "x".repeat(MAX_SESSION_DENIAL_REASON_BYTES + 1);
    assert!(matches!(store.record_session_denial(&session, &oversized), Err(Error::LimitExceeded)));
    assert_eq!(store.session_denial_generation(&session).unwrap(), 0);
    let maximum = "\0".repeat(MAX_SESSION_DENIAL_REASON_BYTES);
    store.record_session_denial(&session, &maximum).unwrap();
    assert_eq!(store.session_denial(&session.id).unwrap(), Some(maximum.clone()));
    let exhausted = SessionDenial { generation: u64::MAX, reason: Some(maximum) };
    assert!(encode(&exhausted).unwrap().len() <= MAX_SESSION_DENIAL_RECORD_BYTES);
    let key = format!("session-denial/{}", session.id);
    for bad in [
        br#"{"generation":1,"reason":"revoked","extra":true}"#.to_vec(),
        br#"{"generation":1,"generation":2,"reason":"revoked"}"#.to_vec(),
        br#"{"generation":-1,"reason":null}"#.to_vec(),
        br#"{}"#.to_vec(),
        br#"{"generation":18446744073709551615,"reason":null}"#.to_vec(),
        vec![b' '; MAX_SESSION_DENIAL_RECORD_BYTES + 1],
        encode(&SessionDenial { generation: 1, reason: Some(oversized) }).unwrap(),
    ] {
        let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
        tx.put(key.as_bytes(), &bad).unwrap();
        tx.commit().unwrap();
        assert!(matches!(store.session_denial(&session.id), Err(Error::Corrupt(_))));
        assert!(store.clear_session_denial(&session, 1).is_err());
    }
    put(&store.metadata, key.as_bytes(), &exhausted).unwrap();
    assert!(!store.clear_session_denial(&session, u64::MAX).unwrap());
    store.record_session_denial(&session, "still denied").unwrap();
    assert_eq!(store.session_denial_generation(&session).unwrap(), u64::MAX);
    assert_eq!(store.session_denial(&session.id).unwrap().as_deref(), Some("still denied"));
}

#[test]
#[ignore = "operator-only optimized local session admission metadata workload"]
fn benchmark_session_denial_admission() {
    // Measure 1,000 hot retained-binding lookup/capture/clear operations;
    // exclude setup, denial production and AP I/O; include durable clear.
    let (_directory, store, root, _) = setup();
    let build = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    for case in ["absent", "cleared", "denied"] {
        if case == "cleared" {
            store.record_session_denial(&session, "revoked").unwrap();
            let generation = store.session_denial_generation(&session).unwrap();
            assert!(store.clear_session_denial(&session, generation).unwrap());
        }
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            if case == "denied" {
                store.record_session_denial(&session, "revoked").unwrap();
            }
            let start = std::time::Instant::now();
            let retained = store.session(&session.id).unwrap();
            let generation = store.session_denial_generation(&retained).unwrap();
            assert!(store.clear_session_denial(&retained, generation).unwrap());
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        println!(
            "{}",
            serde_json::json!({
                "benchmark": "session_denial_admission", "case": case,
                "samples": samples.len(), "p99_ns": samples[989],
                "max_ns": samples[999], "optimized": !cfg!(debug_assertions),
            "boundary": "retained-binding lookup, local capture and guarded clear; excludes AP I/O and denial production"
            })
        );
        assert!(samples[989] < 2_000_000, "local admission metadata p99 exceeds 2 ms");
    }
}

fn capture(store: &LocalStore, volume: Id) -> received::ReceivedVolume {
    let stage = store.prepare_received_volume(volume).unwrap();
    for database in [DatabaseId::Index, DatabaseId::Revocations] {
        store
            .with_replica(volume, database, |source| {
                stage.with_database(database, |target| {
                    if database == DatabaseId::Index {
                        crate::store::merge_index_replica(
                            &std::env::temp_dir(),
                            source,
                            target,
                            kvdb::Lww,
                            |_| Ok(()),
                        )
                    } else {
                        crate::store::merge_replica(
                            &std::env::temp_dir(),
                            source,
                            target,
                            kvdb::Lww,
                            |_| Ok(()),
                        )
                    }
                })
            })
            .unwrap();
    }
    retain_namespace_fixture(store, volume, &stage);
    stage
}

fn entry_capture(store: &LocalStore, volume: Id, entry: EntryId) -> received::ReceivedVolume {
    let stage = store.prepare_received_entry(EntryAddress { volume, entry }).unwrap();
    let index = store.db(volume, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&index, DatabaseId::Entry(entry)).unwrap();
    for database in [DatabaseId::Index, DatabaseId::Revocations] {
        let source = store.db(volume, database).unwrap();
        let projected = super::super::scoped::ScopedSnapshot::new(
            &source,
            database,
            &policy,
            store.account().id,
            None,
        )
        .unwrap()
        .project(&std::env::temp_dir())
        .unwrap();
        stage
            .with_database(database, |target| {
                if database == DatabaseId::Index {
                    crate::store::merge_index_replica(
                        &std::env::temp_dir(),
                        projected.database(),
                        target,
                        kvdb::Lww,
                        |_| Ok(()),
                    )
                } else {
                    crate::store::merge_replica(
                        &std::env::temp_dir(),
                        projected.database(),
                        target,
                        kvdb::Lww,
                        |_| Ok(()),
                    )
                }
            })
            .unwrap();
    }
    retain_namespace_fixture(store, volume, &stage);
    stage
}

fn retain_namespace_fixture(store: &LocalStore, volume: Id, stage: &received::ReceivedVolume) {
    use volume_index::namespace::{receipt, source::cut};
    let canonical = store.db(volume, DatabaseId::Index).unwrap();
    let accepted = cut::accepted(&canonical.begin_read().unwrap()).unwrap();
    stage
        .with_database(DatabaseId::Index, |index| {
            receipt::retain_fixture(index, stage.entry_scope(), &accepted)
        })
        .unwrap();
}

#[cfg(feature = "connected-engine")]
#[test]
fn partial_publication_retains_other_facts_but_never_grants_catalog_after_restart() {
    let (directory, store, volume, access) = setup();
    let a = store.create_blob(&access, volume.id, "selected", b"A").unwrap().id;
    let b = store.create_blob(&access, volume.id, "unselected", b"B").unwrap().id;
    let root = store.replica_root(volume.id, DatabaseId::Index).unwrap();
    let mut stage = entry_capture(&store, volume.id, a);
    stage.qualify_authority();
    store.adopt_received_volume(&access, &stage).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    denied(store.read_blob_entry(&access, volume.id, b));
    denied(store.entries(&access, volume.id, None, 64));
    denied(store.resolve_entry(&access, volume.id, "selected"));
    assert_eq!(store.replica_root(volume.id, DatabaseId::Index).unwrap(), root);
    assert!(
        volume_index::IndexSnapshot::new(
            store.db(volume.id, DatabaseId::Index).unwrap().begin_read().unwrap()
        )
        .live_entry(b)
        .is_ok()
    );
    drop(stage);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    denied(store.read_blob_entry(&access, volume.id, b));
    denied(store.entries(&access, volume.id, None, 64));
    let mut full = capture(&store, volume.id);
    full.qualify_authority();
    store.adopt_received_volume(&access, &full).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, b).unwrap(), b"B");
    assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 2);
}

#[test]
fn unfenced_peer_entry_capture_keeps_its_ceiling_and_cannot_clear_a_later_fence() {
    let (_directory, store, volume, access) = setup();
    let a = store.create_blob(&access, volume.id, "selected", b"A").unwrap().id;
    let b = store.create_blob(&access, volume.id, "unselected", b"B").unwrap().id;
    let stage = entry_capture(&store, volume.id, a);
    store.adopt_received_volume(&access, &stage).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    denied(store.read_blob_entry(&access, volume.id, b));
    denied(store.entries(&access, volume.id, None, 64));
    store.invalidate_authority(&access, volume.id).unwrap();
    let peer = entry_capture(&store, volume.id, a);
    store.adopt_received_volume(&access, &peer).unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    denied(store.entries(&access, volume.id, None, 64));
}

#[cfg(feature = "connected-engine")]
#[test]
fn interrupted_entry_publication_replays_its_scope_and_preserves_newer_negative_evidence() {
    let (directory, store, volume, access) = setup();
    let a = store.create_blob(&access, volume.id, "selected", b"A").unwrap().id;
    let b = store.create_blob(&access, volume.id, "unselected", b"B").unwrap().id;
    let mut stage = entry_capture(&store, volume.id, a);
    stage.qualify_authority();
    *store.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|checkpoint| {
        if checkpoint == received::PublicationCheckpoint::Index {
            return Err(Error::System("interrupted entry publication".into()));
        }
        Ok(())
    }));
    assert!(store.adopt_received_volume(&access, &stage).is_err());
    store.invalidate_authority(&access, volume.id).unwrap();
    drop(stage);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(!store.has_pending_publication(volume.id).unwrap());
    denied(store.read_blob_entry(&access, volume.id, a));
    denied(store.read_blob_entry(&access, volume.id, b));
    denied(store.entries(&access, volume.id, None, 64));
    let mut current = entry_capture(&store, volume.id, a);
    current.qualify_authority();
    store.adopt_received_volume(&access, &current).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    denied(store.entries(&access, volume.id, None, 64));
}

#[cfg(feature = "connected-engine")]
#[test]
fn peer_capture_never_clears_and_new_terminal_survives_pending_publication_replay() {
    use received::PublicationCheckpoint;
    let (directory, store, volume, access) = setup();
    let entry = store.create_blob(&access, volume.id, "blob", b"retain").unwrap().id;
    store.invalidate_authority(&access, volume.id).unwrap();
    let peer = capture(&store, volume.id);
    store.adopt_received_volume(&access, &peer).unwrap();
    denied(store.read_blob_entry(&access, volume.id, entry));
    let mut admitted = capture(&store, volume.id);
    admitted.qualify_authority(); // Fixture substitutes authenticated, one-cut receipt qualification.
    *store.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|checkpoint| {
        if checkpoint == PublicationCheckpoint::Intent {
            return Err(Error::System("interrupted after durable intent".into()));
        }
        Ok(())
    }));
    assert!(store.adopt_received_volume(&access, &admitted).is_err());
    assert!(store.has_pending_publication(volume.id).unwrap());
    store.invalidate_authority(&access, volume.id).unwrap();
    assert_eq!(store.authority_generation(volume.id).unwrap(), 2);
    drop(admitted);
    drop(peer);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(!store.has_pending_publication(volume.id).unwrap());
    denied(store.read_blob_entry(&access, volume.id, entry));
    let mut fresh = capture(&store, volume.id);
    fresh.qualify_authority();
    store.adopt_received_volume(&access, &fresh).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, entry).unwrap(), b"retain");
    assert_eq!(store.authority_generation(volume.id).unwrap(), 3);
}

#[test]
fn invalidation_survives_reopen_and_never_enters_source_or_name_repair() {
    let (directory, store, volume, access) = setup();
    let entry = store.create_blob(&access, volume.id, "a", b"retain").unwrap();
    let root = store.replica_root(volume.id, DatabaseId::Index).unwrap();
    store.invalidate_authority(&access, volume.id).unwrap();
    denied(store.read_blob_entry(&access, volume.id, entry.id));
    assert_eq!(store.replica_root(volume.id, DatabaseId::Index).unwrap(), root);
    store.set_permissions(&access, volume.id, Acl::default()).unwrap();
    store.set_entry_permissions(&access, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    let root = store.replica_root(volume.id, DatabaseId::Index).unwrap();
    store
        .with_replica(volume.id, DatabaseId::Index, |database| {
            let mut tx = database.begin_write(now_unix_secs())?;
            crate::store::volume_index::repair_all(&mut tx.local_indexes())?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    assert_eq!(store.replica_root(volume.id, DatabaseId::Index).unwrap(), root);
    assert_eq!(store.authority_generation(volume.id).unwrap(), 1);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    denied(store.read_blob_entry(&access, volume.id, entry.id));
    assert_eq!(store.authority_generation(volume.id).unwrap(), 1);
}

#[test]
fn entry_capture_restores_only_its_entry_and_newer_invalidation_survives_old_clear() {
    let (_directory, store, volume, access) = setup();
    let a = store.create_blob(&access, volume.id, "a", b"A").unwrap().id;
    let b = store.create_blob(&access, volume.id, "b", b"B").unwrap().id;
    store.invalidate_authority(&access, volume.id).unwrap();
    let generation = store.authority_generation(volume.id).unwrap();
    store.clear_authority_locked(volume.id, generation, Some((a, 0))).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    denied(store.read_blob_entry(&access, volume.id, b));
    denied(store.entries(&access, volume.id, None, 64));
    store.invalidate_authority(&access, volume.id).unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    store.clear_authority_locked(volume.id, generation, None).unwrap();
    store.clear_authority_locked(volume.id, generation, Some((a, 0))).unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    denied(store.entries(&access, volume.id, None, 64));
    store.clear_authority_locked(volume.id, generation + 1, None).unwrap();
    assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 2);
    assert_eq!(store.authority_generation(volume.id).unwrap(), generation + 2);
}

#[test]
fn content_paths_stop_but_owner_can_manage_permissions() {
    let (_directory, store, volume, access) = setup();
    let blob = store.create_blob(&access, volume.id, "blob", b"data").unwrap().id;
    let log = store.create_bloblog(&access, volume.id, "log").unwrap().id;
    let object = store
        .create_database(
            &access,
            volume.id,
            "objects",
            EntryKind::ObjectDb { schema: Schema::default() },
        )
        .unwrap()
        .id;
    let address = EntryAddress { volume: volume.id, entry: object };
    store.invalidate_authority(&access, volume.id).unwrap();
    denied(store.schema(&access, address));
    denied(store.read_object(&access, address, Id::from_bytes([8; 16])));
    denied(store.establish_schema(&access, address, &Schema::default()));
    denied(store.bloblog_get(&access, volume.id, log, None));
    denied(store.bloblog_put(&access, volume.id, log, &[b"blocked".to_vec()]));
    denied(store.write_blob(&access, volume.id, blob, b"blocked"));
    denied(store.rename_entry(&access, volume.id, blob, "renamed"));
    denied(store.remove_entry(&access, volume.id, blob));
    denied(store.entry_revisions(
        &access,
        EntryAddress { volume: volume.id, entry: blob },
        None,
        64,
    ));
    denied(store.recover_entry(
        &access,
        EntryAddress { volume: volume.id, entry: blob },
        "copy",
        None,
    ));
    store.set_permissions(&access, volume.id, Acl::default()).unwrap();
    store.set_entry_permissions(&access, volume.id, blob, EntryAcl::Inherit).unwrap();
    let details = store.entry_details(&access, volume.id, blob).unwrap();
    assert!(details.manage_permissions);
    assert!(
        !details.readable
            && !details.retained_readable
            && !details.writable
            && !details.recoverable
    );
    denied(store.read_blob_entry(&access, volume.id, blob));
}

#[cfg(feature = "connected-engine")]
#[test]
fn entry_withdrawal_preserves_a_sibling_and_only_exact_captures_clear_it() {
    use crate::network::ap::replication::TerminalCause;
    let (directory, store, volume, access) = setup();
    let a = store.create_blob(&access, volume.id, "a", b"A").unwrap().id;
    let b = store.create_blob(&access, volume.id, "b", b"B").unwrap().id;
    let address = EntryAddress { volume: volume.id, entry: a };
    for entry in [b, a] {
        let mut stage = entry_capture(&store, volume.id, entry);
        stage.qualify_authority();
        store.adopt_received_volume(&access, &stage).unwrap();
    }
    let mut old = entry_capture(&store, volume.id, a);
    old.qualify_authority();
    assert!(
        store
            .receive_authority_terminal(
                &access,
                volume.id,
                volume.id,
                TerminalCause::EntryAuthorityChanged
            )
            .is_err()
    );
    assert!(
        store
            .receive_entry_authority_terminal(
                &access,
                address,
                Id::from_bytes([9; 16]),
                TerminalCause::EntryAuthorityChanged
            )
            .is_err()
    );
    store
        .receive_entry_authority_terminal(
            &access,
            address,
            volume.id,
            TerminalCause::EntryAuthorityChanged,
        )
        .unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    assert_eq!(store.read_blob_entry(&access, volume.id, b).unwrap(), b"B");
    store.adopt_received_volume(&access, &old).unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    assert_eq!(store.read_blob_entry(&access, volume.id, b).unwrap(), b"B");
    assert_eq!(store.authority_generation(volume.id).unwrap(), 0);
    drop(old);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    let mut current = entry_capture(&store, volume.id, a);
    current.qualify_authority();
    store.adopt_received_volume(&access, &current).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    store
        .receive_entry_authority_terminal(
            &access,
            address,
            volume.id,
            TerminalCause::ServiceUnavailable,
        )
        .unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    store
        .receive_entry_authority_terminal(
            &access,
            address,
            volume.id,
            TerminalCause::EntryAuthorityChanged,
        )
        .unwrap();
    let mut full = capture(&store, volume.id);
    full.qualify_authority();
    store.adopt_received_volume(&access, &full).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, a).unwrap(), b"A");
    assert_eq!(store.read_blob_entry(&access, volume.id, b).unwrap(), b"B");
    assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 2);
    store
        .receive_entry_authority_terminal(
            &access,
            address,
            volume.id,
            TerminalCause::AuthorityChanged,
        )
        .unwrap();
    denied(store.read_blob_entry(&access, volume.id, a));
    denied(store.read_blob_entry(&access, volume.id, b));
}

#[test]
fn corrupt_markers_fail_closed_and_saturation_cannot_clear() {
    let (_directory, store, volume, access) = setup();
    let database = store.db(volume.id, DatabaseId::Index).unwrap();
    for bytes in [vec![1], vec![0; 10], {
        let mut bytes = vec![0; 9];
        bytes[8] = 2;
        bytes
    }] {
        let mut tx = database.begin_write(now_unix_secs()).unwrap();
        tx.local_indexes().put(&barrier_key(access.account), &bytes).unwrap();
        tx.commit().unwrap();
        assert!(matches!(store.entries(&access, volume.id, None, 64), Err(Error::Corrupt(_))));
    }
    let mut tx = database.begin_write(now_unix_secs()).unwrap();
    tx.local_indexes()
        .put(&barrier_key(access.account), &Barrier { generation: u64::MAX, fenced: true }.encode())
        .unwrap();
    tx.commit().unwrap();
    store.clear_authority_locked(volume.id, u64::MAX, None).unwrap();
    denied(store.entries(&access, volume.id, None, 64));
    store.invalidate_authority(&access, volume.id).unwrap();
    assert_eq!(store.authority_generation(volume.id).unwrap(), u64::MAX);
    denied(store.invalidate_authority(&Access::owner(Id::from_bytes([9; 16])), volume.id));
}

#[test]
fn dependency_catalog_ceilings_do_not_grant_catalog_or_deny_other_entry_reads() {
    // Partial dependency catalogs affect their own listing; negative evidence
    // and participant denial still stop every dependent session read.
    let (_directory, store, root, owner) = setup();
    let version = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let personal = store.create_volume("Selected personal", true).unwrap();
    store
        .select_personal_volumes(
            &session.id,
            vec![crate::store::PersonalVolumeGrant { volume: personal.id, write: true }],
        )
        .unwrap();
    let access = Access::session(owner.account, &session.id);
    let a = store.create_blob(&owner, root.id, "root", b"A").unwrap().id;
    let b = store.create_blob(&owner, personal.id, "selected", b"B").unwrap().id;
    store.restrict_catalog_locked(root.id, a).unwrap();
    assert_eq!(store.read_blob_entry(&access, personal.id, b).unwrap(), b"B");
    store.restrict_catalog_locked(personal.id, b).unwrap();
    for (volume, entry, expected) in [(root.id, a, b"A"), (personal.id, b, b"B")] {
        assert_eq!(store.read_blob_entry(&access, volume, entry).unwrap(), expected);
        denied(store.entries(&access, volume, None, 64));
    }
    for volume in [root.id, personal.id] {
        store.invalidate_authority(&owner, volume).unwrap();
        if volume == root.id {
            denied(store.read_blob_entry(&access, root.id, a));
        } else {
            assert_eq!(store.read_blob_entry(&access, root.id, a).unwrap(), b"A");
        }
        denied(store.read_blob_entry(&access, personal.id, b));
        let generation = store.authority_generation(volume).unwrap();
        store.clear_authority_locked(volume, generation, None).unwrap();
    }
    assert_eq!(store.read_blob_entry(&access, root.id, a).unwrap(), b"A");
    assert_eq!(store.read_blob_entry(&access, personal.id, b).unwrap(), b"B");
    store.record_session_denial(&session, "participant denied").unwrap();
    denied(store.read_blob_entry(&access, root.id, a));
    denied(store.read_blob_entry(&access, personal.id, b));
}

#[cfg(feature = "connected-engine")]
#[test]
fn binding_qualification_survives_restart_without_granting_catalog_or_siblings() {
    let (directory, store, root, owner) = setup();
    let version = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let access = Access::session_version(owner.account, &session.id, &version);
    let a = store.create_blob(&owner, root.id, "selected", b"A").unwrap().id;
    let sibling = store.create_blob(&owner, root.id, "sibling", b"B").unwrap().id;
    store.invalidate_authority(&owner, root.id).unwrap();
    denied(store.admit_binding(&access, root.id));
    let peer = entry_capture(&store, root.id, a);
    store.adopt_received_volume(&access, &peer).unwrap();
    denied(store.admit_binding(&access, root.id));
    let mut current = entry_capture(&store, root.id, a);
    current.qualify_authority(); // Substitute the trusted AP receipt qualification.
    store.adopt_received_volume(&access, &current).unwrap();
    store.admit_binding(&access, root.id).unwrap();
    store.admit_binding(&access, session.root_volume).unwrap();
    assert_eq!(store.read_blob_entry(&access, root.id, a).unwrap(), b"A");
    denied(store.read_blob_entry(&access, root.id, sibling));
    denied(store.resolve_entry(&access, root.id, "selected"));
    denied(store.entries(&access, root.id, None, 64));
    drop(current);
    drop(peer);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    store.admit_binding(&access, root.id).unwrap();
    store.admit_binding(&access, session.root_volume).unwrap();
    denied(store.entries(&access, root.id, None, 64));
    let mut stale = entry_capture(&store, root.id, a);
    stale.qualify_authority();
    store.invalidate_authority(&owner, root.id).unwrap();
    store.adopt_received_volume(&access, &stale).unwrap();
    denied(store.admit_binding(&access, root.id));
    denied(store.admit_binding(&access, session.root_volume));
    let mut current = entry_capture(&store, root.id, a);
    current.qualify_authority();
    store.adopt_received_volume(&access, &current).unwrap();
    store.admit_binding(&access, root.id).unwrap();
    store.record_session_denial(&session, "participant denied").unwrap();
    denied(store.admit_binding(&access, root.id));
    denied(store.read_blob_entry(&access, root.id, a));
}

#[test]
fn malformed_and_future_binding_witnesses_never_admit_fenced_metadata() {
    let (_directory, store, volume, owner) = setup();
    store.invalidate_authority(&owner, volume.id).unwrap();
    let database = store.db(volume.id, DatabaseId::Index).unwrap();
    let mut tx = database.begin_write(now_unix_secs()).unwrap();
    tx.local_indexes().put(&binding_key(owner.account), &[0; 7]).unwrap();
    tx.commit().unwrap();
    assert!(matches!(store.admit_binding(&owner, volume.id), Err(Error::Corrupt(_))));
    for generation in [0_u64, 2, u64::MAX] {
        let mut tx = database.begin_write(now_unix_secs()).unwrap();
        tx.local_indexes().put(&binding_key(owner.account), &generation.to_le_bytes()).unwrap();
        tx.commit().unwrap();
        denied(store.admit_binding(&owner, volume.id));
    }
}

#[test]
fn a_future_or_malformed_entry_epoch_cannot_grant_local_read_or_qualification() {
    let (_directory, store, volume, access) = setup();
    let entry = store.create_blob(&access, volume.id, "selected", b"A").unwrap().id;
    let database = store.db(volume.id, DatabaseId::Index).unwrap();
    for bytes in [
        vec![0; 16],
        EntryBarrier { volume_generation: 1, barrier: Barrier::default() }.encode().to_vec(),
    ] {
        let mut tx = database.begin_write(now_unix_secs()).unwrap();
        tx.local_indexes().put(&entry_barrier_key(access.account, entry), &bytes).unwrap();
        tx.commit().unwrap();
        assert!(matches!(store.read_blob_entry(&access, volume.id, entry), Err(Error::Corrupt(_))));
        assert!(matches!(
            store.prepare_received_entry(EntryAddress { volume: volume.id, entry }),
            Err(Error::Corrupt(_))
        ));
    }
}

#[cfg(feature = "connected-engine")]
#[test]
fn terminals_validate_dependencies_and_distinguish_service_loss_from_denial() {
    use crate::network::ap::replication::TerminalCause;
    let (directory, store, volume, owner) = setup();
    let version = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            volume.id,
        )
        .unwrap();
    let access = Access::session(owner.account, &session.id);
    let unrelated = store.create_volume("Unrelated", true).unwrap().id;
    denied(store.receive_authority_terminal(
        &access,
        volume.id,
        unrelated,
        TerminalCause::AuthorityChanged,
    ));
    assert_eq!(store.authority_generation(unrelated).unwrap(), 0);
    assert!(
        !store
            .receive_authority_terminal(
                &access,
                volume.id,
                volume.id,
                TerminalCause::ServiceUnavailable
            )
            .unwrap()
    );
    store.entries(&owner, volume.id, None, 64).unwrap();
    store
        .receive_authority_terminal(
            &access,
            volume.id,
            session.root_volume,
            TerminalCause::AuthorityChanged,
        )
        .unwrap();
    denied(store.entries(&access, volume.id, None, 64));
    denied(store.entries(&owner, volume.id, None, 64));
    store
        .receive_authority_terminal(&access, volume.id, volume.id, TerminalCause::ParticipantDenied)
        .unwrap();
    assert!(store.session_denial(&session.id).unwrap().is_some());
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(store.session_denial(&session.id).unwrap().is_some());
    denied(store.entries(&access, volume.id, None, 64));
}

#[test]
fn guest_owned_session_records_share_root_storage_and_persist_with_root_permissions() {
    let (directory, store, root, owner) = setup();
    let build = crate::identifier::encode(&[0xab; 32]);
    let first = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let second = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    // The guest chooses both this database and its keys. The host creates no session namespace.
    let entry = store.create_database(&owner, root.id, "notes", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: root.id, entry: entry.id };
    store.kv_put(&owner, address, first.id.as_bytes(), b"first note").unwrap();
    store.kv_put(&owner, address, second.id.as_bytes(), b"second note").unwrap();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    for (session, value) in
        [(&first, b"first note".as_slice()), (&second, b"second note".as_slice())]
    {
        assert_eq!(
            store.kv_read(&owner, address, session.id.as_bytes(), 0, 1024, None).unwrap().bytes,
            value
        );
    }
    // Possessing the session descriptor does not grant this account root access.
    let outsider = Access::session_version(Id::from_bytes([0x45; 16]), &first.id, &build);
    denied(store.kv_read(&outsider, address, first.id.as_bytes(), 0, 1024, None));
    denied(store.kv_put(&outsider, address, first.id.as_bytes(), b"denied"));
    assert_eq!(
        store.kv_read(&owner, address, first.id.as_bytes(), 0, 1024, None).unwrap().bytes,
        b"first note"
    );
}
