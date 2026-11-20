#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::store::change_feed::{CommittedChangeKind, CommittedChangeReceiver, FeedClosure};

pub(super) fn new_named_object(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    name: &str,
) -> (crate::store::volume_index::EntryAddress, ObjectFields) {
    let entry = store
        .create_database(
            access,
            volume,
            name,
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
        )
        .unwrap();
    let address = crate::store::volume_index::EntryAddress { volume, entry: entry.id };
    let object =
        store.create_object_with_fields(access, address, id(1), &title("Complete")).unwrap();
    (address, object)
}

fn clear(receiver: &mut CommittedChangeReceiver) {
    while !receiver.drain().records.is_empty() {}
}

pub(super) fn pair()
-> (tempfile::TempDir, LocalStore, tempfile::TempDir, LocalStore, Volume, Access, Access) {
    let (alice_dir, alice, volume, owner) = setup();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            },
        )
        .unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    alice.provision_version(&owner, volume.id, &version).unwrap();
    let session = alice
        .create_session(
            &crate::package::AppIdentity { publisher: alice.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            volume.id,
        )
        .unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    bob.import_invitation_link(
        &alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        &alice.session_envelope(&session.id).unwrap(),
        crate::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    sync_volume(&alice, &bob, volume.id);
    sync_volume(&alice, &bob, session.root_volume);
    let participant = Access::session(bob.account().id, &session.id);
    (alice_dir, alice, bob_dir, bob, volume, owner, participant)
}

#[test]
fn pending_name_repairs_are_bounded_before_a_new_rename_commits() {
    let (_directory, store, volume, owner) = setup();
    for n in 0..256u32 {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        store.prepare_volume_name(Id::from_bytes(bytes)).unwrap();
    }
    assert_eq!(store.pending_volume_names().unwrap().len(), 256);
    assert!(matches!(store.label_volume(&owner, volume.id, "Rejected"), Err(Error::LimitExceeded)));
    assert_eq!(store.volume(volume.id).unwrap(), volume);
    assert_eq!(store.index(volume.id).unwrap().volume, volume);
    assert!(256 * (volume_name_marker(volume.id).len() + std::mem::size_of::<Id>()) <= 16 * 1024);
}

#[test]
fn interrupted_name_projection_repairs_only_marked_volumes_on_restart() {
    for committed in [false, true] {
        let (directory, store, volume, _) = setup();
        let unchanged = store.create_volume("Unrelated", false).unwrap();
        store.prepare_volume_name(volume.id).unwrap();
        if committed {
            let db = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
            let mut renamed = volume.clone();
            renamed.name = "Recovered Café".into();
            put_source(&db, &VOLUME_KEY, &renamed).unwrap();
        }
        assert_eq!(store.volume(volume.id).unwrap().name, volume.name);
        assert_eq!(store.pending_volume_names().unwrap(), vec![volume.id]);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(
            store.volume(volume.id).unwrap().name,
            if committed { "Recovered Café" } else { &volume.name }
        );
        assert_eq!(store.volume(unchanged.id).unwrap(), unchanged);
        assert!(store.pending_volume_names().unwrap().is_empty());
        // Restart opened the one interrupted name's DB1, not every known volume.
        assert_eq!(store.open.lock().unwrap().by_id.len(), 1);
    }
}

#[test]
fn shared_rename_replays_one_metadata_change_and_preserves_old_invitations() {
    let (alice_dir, alice, bob_dir, bob, volume, owner, participant) = pair();
    let session = alice.sessions().unwrap().remove(0);
    let invitation =
        alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap();
    let original_policy = alice.index(volume.id).unwrap();
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    assert!(bob.label_volume(&Access::owner(bob.account().id), volume.id, "Denied").is_err());
    assert!(alice.label_volume(&participant, volume.id, "Denied").is_err());
    for label in ["", "  ", &"é".repeat(129)] {
        assert!(alice.label_volume(&owner, volume.id, label).is_err());
    }
    assert!(feed.drain().records.is_empty());
    let renamed = alice.label_volume(&owner, volume.id, "Café 👩🏽‍💻").unwrap();
    let policy =
        DatabasePolicy::reserved(original_policy.clone(), crate::store::DatabaseId::Index).unwrap();
    let raw = record::encode(&renamed).unwrap();
    let source = kvdb::sync::Delta {
        key: &VOLUME_KEY,
        local: kvdb::sync::EntState::Missing,
        remote: kvdb::sync::EntState::Present { value: &raw, clock: kvdb::Clock::from_u64(9) },
    };
    assert!(policy.validate_source(&source, Some(participant.account)).is_err());
    for property in 0..4 {
        let mut invalid = renamed.clone();
        match property {
            0 => invalid.id = Id::generate().unwrap(),
            1 => invalid.owner = participant.account,
            2 => invalid.share_key_with_service = !invalid.share_key_with_service,
            _ => invalid.personal = !invalid.personal,
        }
        assert!(
            crate::store::catalog::validate_metadata_entry(
                &volume,
                &VOLUME_KEY,
                Some(&record::encode(&invalid).unwrap())
            )
            .is_err()
        );
    }
    assert!(renamed.same_identity(&volume));
    assert_eq!(alice.volume(volume.id).unwrap(), renamed);
    assert!(alice.volumes().unwrap().contains(&renamed));
    let batch = feed.drain();
    assert_eq!(batch.records.len(), 1);
    let record = &batch.records[0];
    assert_eq!(record.kind(), CommittedChangeKind::Database(crate::store::DatabaseId::Index));
    assert!(record.bytes().len() < 1024);
    let incoming = alice
        .outbound_delta(
            volume.id,
            crate::store::DatabaseId::Index,
            ReplicaReader::Participant(bob.account().id),
            record.bytes(),
        )
        .unwrap();
    assert_eq!(
        bob.merge_received_delta(
            &participant,
            volume.id,
            crate::store::DatabaseId::Index,
            &incoming
        )
        .unwrap()
        .applied,
        1
    );
    assert_eq!(bob.volume(volume.id).unwrap(), renamed);
    assert_eq!(
        bob.merge_received_delta(
            &participant,
            volume.id,
            crate::store::DatabaseId::Index,
            &incoming
        )
        .unwrap()
        .applied,
        0
    );
    assert_eq!(alice.index(volume.id).unwrap().permissions, original_policy.permissions);
    assert_eq!(
        alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        invitation
    );
    assert_eq!(
        bob.import_invitation_link(
            &invitation,
            &alice.session_envelope(&session.id).unwrap(),
            crate::release_channel::ReleaseChannel::Public
        )
        .unwrap(),
        session
    );
    // A newer invitation for the same volume can coexist with the old one.
    let next = alice
        .create_session(
            &session.app,
            session.selector.clone(),
            &session.resolved_build_id,
            volume.id,
        )
        .unwrap();
    bob.import_invitation_link(
        &alice.invitation(&next.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        &alice.session_envelope(&next.id).unwrap(),
        crate::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    assert_eq!(bob.volume(volume.id).unwrap(), renamed);
    alice.label_volume(&owner, volume.id, &renamed.name).unwrap();
    // Session creation can publish its own metadata; drain before testing no-op.
    clear(&mut feed);
    alice.label_volume(&owner, volume.id, &renamed.name).unwrap();
    assert!(feed.drain().records.is_empty());
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    assert_eq!(bob.volume(volume.id).unwrap(), renamed);
    assert_eq!(bob.session(&session.id).unwrap(), session);
    drop(alice);
    let alice = LocalStore::open(alice_dir.path()).unwrap();
    assert_eq!(alice.volume(volume.id).unwrap(), renamed);
    assert_eq!(
        alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        invitation
    );
}

#[test]
fn committed_birth_is_one_complete_database_delta_and_replay_is_idempotent() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let address = data(&alice, volume.id);
    let database = crate::store::DatabaseId::Entry(address.entry);
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    let object = alice.create_object_with_fields(&owner, address, id(1), &title("First")).unwrap();
    let batch = feed.drain();
    assert_eq!(batch.status.gap_generation, 0);
    assert_eq!(batch.records.len(), 1);
    let record = &batch.records[0];
    assert_eq!(record.kind(), CommittedChangeKind::Database(database));
    let projected = alice
        .outbound_delta(
            volume.id,
            database,
            ReplicaReader::Participant(bob.account().id),
            record.bytes(),
        )
        .unwrap();
    assert!(
        bob.merge_received_delta(&participant, volume.id, database, &projected).unwrap().applied
            > 0
    );
    assert_eq!(
        bob.merge_received_delta(&participant, volume.id, database, &projected).unwrap().applied,
        0
    );
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
    alice.edit_fields(&owner, address, object.object_id, &title("Edited")).unwrap();
    let batch = feed.drain();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(batch.records[0].kind(), CommittedChangeKind::Database(database));
    bob.merge_received_delta(&participant, volume.id, database, batch.records[0].bytes()).unwrap();
    assert_eq!(
        bob.read_object(&participant, address, object.object_id).unwrap(),
        alice.read_object(&owner, address, object.object_id).unwrap()
    );
}

#[test]
fn rejected_edit_emits_nothing_and_closed_receiver_never_fails_local_commit() {
    let (_dir, store, volume, owner) = setup();
    let object = store
        .create_object_with_fields(&owner, data(&store, volume.id), id(1), &title("Before"))
        .unwrap();
    let mut feed = store.take_change_receiver().unwrap();
    clear(&mut feed);
    assert!(
        store
            .edit_fields(
                &Access::owner(id(90)),
                data(&store, volume.id),
                object.object_id,
                &title("Denied")
            )
            .is_err()
    );
    assert!(
        store
            .edit_fields(
                &owner,
                data(&store, volume.id),
                object.object_id,
                &BTreeMap::from([(id(99), Some(vec![Value::Text("Invalid".into())]))])
            )
            .is_err()
    );
    assert!(feed.drain().records.is_empty());
    assert_eq!(
        store.read_object(&owner, data(&store, volume.id), object.object_id).unwrap(),
        object
    );
    drop(feed);
    store
        .edit_fields(
            &owner,
            data(&store, volume.id),
            object.object_id,
            &title("Durable after closure"),
        )
        .unwrap();
    assert_eq!(store.change_status().closed, Some(FeedClosure::ReceiverDropped));
    assert!(store.change_status().gap_generation > 0);
    let retained = store.read_object(&owner, data(&store, volume.id), object.object_id).unwrap();
    assert_ne!(retained, object);
}

#[test]
fn received_delta_rechecks_current_policy_and_rejects_truncated_payload_atomically() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let object = alice
        .create_object_with_fields(&owner, data(&alice, volume.id), id(1), &title("Before"))
        .unwrap();
    sync_volume(&alice, &bob, volume.id);
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    alice.edit_fields(&owner, data(&alice, volume.id), object.object_id, &title("After")).unwrap();
    let batch = feed.drain();
    let bytes = batch.records[0].bytes();
    let before = bob
        .replica_root(volume.id, crate::store::DatabaseId::Entry(data(&bob, volume.id).entry))
        .unwrap();
    assert!(
        bob.merge_received_delta(
            &participant,
            volume.id,
            crate::store::DatabaseId::Entry(data(&bob, volume.id).entry),
            &bytes[..bytes.len() - 1]
        )
        .is_err()
    );
    assert_eq!(
        bob.replica_root(volume.id, crate::store::DatabaseId::Entry(data(&bob, volume.id).entry))
            .unwrap(),
        before
    );
    bob.set_entry_permissions(
        &owner,
        volume.id,
        data(&bob, volume.id).entry,
        EntryAcl::Replace(Acl::default()),
    )
    .unwrap();
    assert!(
        bob.merge_received_delta(
            &participant,
            volume.id,
            crate::store::DatabaseId::Entry(data(&bob, volume.id).entry),
            bytes
        )
        .is_err()
    );
    assert_eq!(
        bob.replica_root(volume.id, crate::store::DatabaseId::Entry(data(&bob, volume.id).entry))
            .unwrap(),
        before
    );
}

#[test]
fn incoming_application_commit_progresses_while_an_independent_local_edit_is_held() {
    use std::{sync::mpsc, time::Duration};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let object = alice
        .create_object_with_fields(&owner, data(&alice, volume.id), id(1), &title("Before"))
        .unwrap();
    sync_volume(&alice, &bob, volume.id);
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    alice
        .edit_fields(&owner, data(&alice, volume.id), object.object_id, &title("Incoming"))
        .unwrap();
    let batch = feed.drain();
    let independent = bob.create_volume("Independent", true).unwrap();
    let local_owner = Access::owner(bob.account().id);
    bob.create_database(
        &local_owner,
        independent.id,
        "data",
        crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
    )
    .unwrap();
    let local = bob
        .create_object_with_fields(&local_owner, data(&bob, independent.id), id(1), &title("Local"))
        .unwrap();
    let (entered_send, entered) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    *bob.after_edit_read.lock().unwrap() = Some(Arc::new(move |id| {
        if id == independent.id {
            entered_send.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        }
    }));
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            bob.edit_fields(
                &local_owner,
                data(&bob, independent.id),
                local.object_id,
                &title("Edited"),
            )
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished_send, finished) = mpsc::sync_channel(1);
        let recipient = &bob;
        let merger = scope.spawn(move || {
            let result = recipient.merge_received_delta(
                &participant,
                volume.id,
                crate::store::DatabaseId::Entry(data(recipient, volume.id).entry),
                batch.records[0].bytes(),
            );
            finished_send.send(result).unwrap();
        });
        let progress = finished.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        merger.join().unwrap();
        assert!(
            progress.expect("incoming merge waited for an unrelated local edit").unwrap().applied
                > 0
        );
    });
}

#[test]
fn malformed_winning_metadata_never_commits_or_breaks_an_initialized_index() {
    let (_a, alice, _b, bob, volume, _owner, participant) = pair();
    let staged = alice
        .with_replica(volume.id, crate::store::DatabaseId::Index, |db| {
            detached_replica(&std::env::temp_dir(), db)
        })
        .unwrap();
    let before = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    for value in [None, Some(b"malformed".as_slice()), Some(b"{}".as_slice())] {
        let mut tx = staged
            .database()
            .begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)
            .unwrap();
        match value {
            Some(value) => tx.put(&DEFAULT_PERMISSIONS_KEY, value).unwrap(),
            None => {
                tx.del(&DEFAULT_PERMISSIONS_KEY).unwrap();
            }
        };
        let kvdb::CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
        assert!(
            bob.merge_received_delta(
                &participant,
                volume.id,
                crate::store::DatabaseId::Index,
                &bytes
            )
            .is_err()
        );
        assert_eq!(bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
        assert_eq!(bob.schema(&participant, data(&bob, volume.id)).unwrap(), schema());
    }
}

#[test]
fn failed_new_database_preparation_rolls_back_metadata_and_retry_finishes() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let (address, object) = new_named_object(&alice, &owner, volume.id, "new");
    let stage = super::received::receive(&alice, &bob, volume.id);
    let path = bob
        .path
        .join(crate::paths::VOLUMES)
        .join(volume.id.to_string())
        .join(crate::store::DatabaseId::Entry(address.entry).file_name());
    fs::write(&path, b"invalid existing database").unwrap();
    let before = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    assert_eq!(
        replica_root(&bob.db(volume.id, crate::store::DatabaseId::Index).unwrap()).unwrap(),
        before
    );
    assert!(
        bob.resolve_entry(&participant, volume.id, "new")
            .unwrap_err()
            .to_string()
            .contains("publication requires recovery")
    );
    fs::remove_file(&path).unwrap();
    bob.adopt_received_volume(&participant, &stage).unwrap();
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
}

#[test]
fn received_permission_removal_blocks_reads_without_erasing_retained_data() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let object = alice
        .create_object_with_fields(&owner, data(&alice, volume.id), id(1), &title("Retained"))
        .unwrap();
    sync_volume(&alice, &bob, volume.id);
    let before = bob
        .replica_root(volume.id, crate::store::DatabaseId::Entry(data(&bob, volume.id).entry))
        .unwrap();
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    alice.set_permissions(&owner, volume.id, Acl::default()).unwrap();
    let batch = feed.drain();
    assert_eq!(batch.records.len(), 1);
    assert!(
        bob.merge_received_delta(
            &participant,
            volume.id,
            crate::store::DatabaseId::Index,
            batch.records[0].bytes()
        )
        .unwrap()
        .applied
            > 0
    );
    assert!(bob.read_object(&participant, data(&bob, volume.id), object.object_id).is_err());
    assert_eq!(
        bob.replica_root(volume.id, crate::store::DatabaseId::Entry(data(&bob, volume.id).entry))
            .unwrap(),
        before
    );
}

#[test]
fn interrupted_named_publication_keeps_index_hidden_and_retries_backing_and_index() {
    for stop_index in [false, true] {
        let (_a, alice, _b, bob, volume, owner, participant) = pair();
        let (address, object) = new_named_object(&alice, &owner, volume.id, "new");
        let stage = super::received::receive(&alice, &bob, volume.id);
        let stop = if stop_index {
            crate::store::DatabaseId::Index
        } else {
            crate::store::DatabaseId::Entry(address.entry)
        };
        let before = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
        *bob.before_creation_commit.lock().unwrap() = Some(Arc::new(move |database| {
            if database == stop {
                Err(Error::System("injected commit-boundary interruption".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        assert_eq!(
            replica_root(&bob.db(volume.id, crate::store::DatabaseId::Index).unwrap()).unwrap(),
            before
        );
        assert!(
            bob.resolve_entry(&participant, volume.id, "new")
                .unwrap_err()
                .to_string()
                .contains("publication requires recovery")
        );
        *bob.before_creation_commit.lock().unwrap() = None;
        bob.adopt_received_volume(&participant, &stage).unwrap();
        assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
        assert!(!bob.adopt_received_volume(&participant, &stage).unwrap().changed());
    }
}

#[test]
fn truncated_and_incomplete_birth_deltas_never_expose_an_object() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let address = data(&alice, volume.id);
    let database = crate::store::DatabaseId::Entry(address.entry);
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    let object =
        alice.create_object_with_fields(&owner, address, id(1), &title("Complete")).unwrap();
    let batch = feed.drain();
    let bytes = batch.records[0].bytes();
    let before = bob.replica_root(volume.id, database).unwrap();
    assert!(
        bob.merge_received_delta(&participant, volume.id, database, &bytes[..bytes.len() - 1])
            .is_err()
    );
    assert_eq!(bob.replica_root(volume.id, database).unwrap(), before);
    assert!(bob.read_object(&participant, address, object.object_id).is_err());
    let backing = alice.db(volume.id, database).unwrap();
    let key = crate::store::object_database::creation_key(object.object_id);
    let raw = backing.begin_read().unwrap().get(key.as_slice()).unwrap().unwrap().to_vec();
    let header =
        record::View::record(&raw).unwrap().fields(record::Mode::Write, &[1, 2, 3]).unwrap();
    let object = header.get(3).unwrap().fields(record::Mode::Write, &[1, 2, 3]).unwrap();
    let mut body = vec![record::STRUCT];
    record::put_raw(&mut body, 1, object.get(1).unwrap().0).unwrap();
    record::put_raw(&mut body, 2, object.get(2).unwrap().0).unwrap();
    let mut value = Vec::from(record::MAGIC);
    value.push(record::STRUCT);
    record::put_raw(&mut value, 1, header.get(1).unwrap().0).unwrap();
    record::put_raw(&mut value, 2, header.get(2).unwrap().0).unwrap();
    record::put_raw(&mut value, 3, &body).unwrap();
    let mut tx = backing
        .begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)
        .unwrap();
    tx.put(key.as_slice(), &value).unwrap();
    let kvdb::CommittedDelta::Bytes(incomplete) = tx.commit_with_delta().unwrap() else { panic!() };
    assert!(bob.merge_received_delta(&participant, volume.id, database, &incomplete).is_err());
    assert_eq!(bob.replica_root(volume.id, database).unwrap(), before);
    assert!(bob.query_objects(&participant, address, id(1), None, 64).unwrap().objects.is_empty());
}

#[test]
fn newer_corrupt_birth_cannot_shadow_validation_of_a_losing_source() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let address = data(&alice, volume.id);
    let database = crate::store::DatabaseId::Entry(address.entry);
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    let object =
        alice.create_object_with_fields(&owner, address, id(1), &title("Complete")).unwrap();
    let batch = feed.drain();
    let backing = bob.db(volume.id, database).unwrap();
    let mut tx = backing.begin_write(now_unix_secs() + 60).unwrap();
    tx.put(
        crate::store::object_database::creation_key(object.object_id).as_slice(),
        &record::encode(&id(99)).unwrap(),
    )
    .unwrap();
    tx.commit().unwrap();
    let before = replica_root(&backing).unwrap();
    assert!(
        bob.merge_received_delta(&participant, volume.id, database, batch.records[0].bytes())
            .is_err()
    );
    assert_eq!(replica_root(&backing).unwrap(), before);
    assert!(matches!(
        bob.read_object(&participant, address, object.object_id),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn query_cannot_enter_during_named_backing_and_index_publication() {
    use std::{sync::mpsc, time::Duration};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let (address, object) = new_named_object(&alice, &owner, volume.id, "new");
    let stage = super::received::receive(&alice, &bob, volume.id);
    let (entered_send, entered) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    *bob.before_creation_commit.lock().unwrap() = Some(Arc::new(move |database| {
        if database == crate::store::DatabaseId::Index {
            entered_send.send(()).unwrap();
            released.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Ok(())
    }));
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| bob.adopt_received_volume(&participant, &stage));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let excluded = bob.volume_operation(volume.id).unwrap().try_lock().is_err();
        let reader = scope.spawn(|| bob.read_object(&participant, address, object.object_id));
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert!(excluded, "query could enter before the publication boundary");
        assert_eq!(reader.join().unwrap().unwrap(), object);
    });
}

#[test]
fn embedded_store_wakes_its_host_and_reopens_acknowledged_edits() {
    // Two local hosts have independent readiness and identities. The host owns
    // the turn; callbacks only set a coalesced flag and never enter the store.
    use std::sync::atomic::{AtomicBool, Ordering};
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first_pending = Arc::new(AtomicBool::new(false));
    let second_pending = Arc::new(AtomicBool::new(false));
    let wake = first_pending.clone();
    let first = LocalStore::open_with_wake(first_dir.path(), move || {
        wake.store(true, Ordering::Release);
    })
    .unwrap();
    let wake = second_pending.clone();
    let second = LocalStore::open_with_wake(second_dir.path(), move || {
        wake.store(true, Ordering::Release);
    })
    .unwrap();
    assert_ne!(first.account().id, second.account().id);
    let mut receiver = first.take_change_receiver().unwrap();
    assert!(receiver.readiness().is_none());
    assert!(second.take_change_receiver().unwrap().readiness().is_none());
    let volume = first.create_volume("Embedded", false).unwrap();
    let owner = Access::owner(first.account().id);
    first
        .create_database(
            &owner,
            volume.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
        )
        .unwrap();
    let object = first
        .create_object_with_fields(&owner, data(&first, volume.id), id(1), &title("First"))
        .unwrap();
    clear(&mut receiver);
    first_pending.store(false, Ordering::Release);
    first
        .edit_fields(&owner, data(&first, volume.id), object.object_id, &title("Committed"))
        .unwrap();
    assert!(first_pending.swap(false, Ordering::AcqRel));
    assert!(!second_pending.load(Ordering::Acquire));
    let batch = receiver.drain();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(
        batch.records[0].kind(),
        CommittedChangeKind::Database(crate::store::DatabaseId::Entry(
            data(&first, volume.id).entry
        ))
    );
    assert_eq!(batch.status.gap_generation, 0);
    let saved = first.read_object(&owner, data(&first, volume.id), object.object_id).unwrap();
    assert_ne!(saved, object);
    assert!(
        first
            .edit_fields(
                &Access::owner(id(90)),
                data(&first, volume.id),
                object.object_id,
                &title("Denied")
            )
            .is_err()
    );
    assert!(!first_pending.load(Ordering::Acquire), "denied edit must not notify");
    drop(first);
    assert!(first_pending.swap(false, Ordering::AcqRel));
    assert_eq!(receiver.drain().status.closed, Some(FeedClosure::SourceDropped));
    let wake = first_pending.clone();
    let reopened = LocalStore::open_with_wake(first_dir.path(), move || {
        wake.store(true, Ordering::Release);
    })
    .unwrap();
    assert_eq!(
        reopened.read_object(&owner, data(&reopened, volume.id), object.object_id).unwrap(),
        saved
    );
    assert_ne!(reopened.change_status().epoch, batch.status.epoch);
    assert_eq!(reopened.change_status().gap_generation, 0);
}

fn receive_entry_records(
    source: &LocalStore,
    destination: &LocalStore,
    access: &Access,
    volume: Id,
    mut records: Vec<crate::store::change_feed::CommittedChange>,
) {
    records.reverse();
    for record in records {
        assert_eq!(record.kind(), CommittedChangeKind::Database(crate::store::DatabaseId::Index));
        let bytes = source
            .outbound_delta(
                volume,
                crate::store::DatabaseId::Index,
                ReplicaReader::Participant(access.account),
                record.bytes(),
            )
            .unwrap();
        destination
            .merge_received_delta(access, volume, crate::store::DatabaseId::Index, &bytes)
            .unwrap();
        assert_eq!(
            destination
                .merge_received_delta(access, volume, crate::store::DatabaseId::Index, &bytes)
                .unwrap()
                .applied,
            0
        );
    }
}

#[test]
fn named_entry_offline_creates_converge_and_conflicts_survive_restart() {
    use crate::store::volume_index::{EntryKind, IndexSnapshot};
    let (alice_dir, alice, bob_dir, bob, volume, owner, participant) = pair();
    let mut alice_feed = alice.take_change_receiver().unwrap();
    let mut bob_feed = bob.take_change_receiver().unwrap();
    clear(&mut alice_feed);
    clear(&mut bob_feed);
    let a = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    let b = bob.create_database(&participant, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    assert_ne!(a.id, b.id);
    let changes_a = alice_feed.drain().records;
    let changes_b = bob_feed.drain().records;
    receive_entry_records(&alice, &bob, &participant, volume.id, changes_a);
    receive_entry_records(&bob, &alice, &owner, volume.id, changes_b);
    let winner = a.id.max(b.id);
    let loser = a.id.min(b.id);
    for store in [&alice, &bob] {
        let backing = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        let snapshot = IndexSnapshot::new(backing.begin_read().unwrap());
        assert_eq!(snapshot.lookup("notes").unwrap().id, winner);
        assert!(snapshot.live_entry(loser).unwrap().name_conflict);
        assert_eq!(snapshot.page(None, 64).unwrap().entries.len(), 3);
    }
    alice.remove_entry(&owner, volume.id, winner).unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, alice_feed.drain().records);
    drop(alice);
    drop(bob);
    for directory in [alice_dir, bob_dir] {
        let store = LocalStore::open(directory.path()).unwrap();
        let backing = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        let snapshot = IndexSnapshot::new(backing.begin_read().unwrap());
        assert_eq!(snapshot.lookup("notes").unwrap().id, loser);
        assert!(snapshot.entry(winner).unwrap().deleted);
    }
}

#[test]
fn named_entry_delayed_rename_cannot_resurrect_deleted_id_or_retarget_new_name() {
    use crate::store::volume_index::{EntryKind, IndexSnapshot};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let mut alice_feed = alice.take_change_receiver().unwrap();
    let mut bob_feed = bob.take_change_receiver().unwrap();
    clear(&mut alice_feed);
    clear(&mut bob_feed);
    let old = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, alice_feed.drain().records);
    bob.rename_entry(&participant, volume.id, old.id, "work").unwrap();
    let delayed = bob_feed.drain().records;
    alice.remove_entry(&owner, volume.id, old.id).unwrap();
    let new = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    assert_ne!(old.id, new.id);
    receive_entry_records(&alice, &bob, &participant, volume.id, alice_feed.drain().records);
    receive_entry_records(&bob, &alice, &owner, volume.id, delayed);
    for store in [&alice, &bob] {
        let backing = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        let snapshot = IndexSnapshot::new(backing.begin_read().unwrap());
        assert_eq!(snapshot.lookup("notes").unwrap().id, new.id);
        assert!(snapshot.lookup("work").is_err());
        assert!(snapshot.entry(old.id).unwrap().deleted);
        assert!(snapshot.revisions(old.id, None, 64).unwrap().revisions.len() >= 2);
    }
    assert!(alice.rename_entry(&owner, volume.id, old.id, "revived").is_err());
}

#[test]
fn named_entry_full_sync_repairs_names_and_rename_preserves_backing_identity() {
    use crate::store::volume_index::{EntryKind, IndexSnapshot};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    let backing = alice.entry_backing(volume.id, entry.id, false).unwrap();
    let mut write = backing.begin_write(now_unix_secs()).unwrap();
    write.put(b"retained", b"data").unwrap();
    write.commit().unwrap();
    let hash_key = backing.meta().hash_key;
    alice.rename_entry(&owner, volume.id, entry.id, "work").unwrap();
    let same = alice.entry_backing(volume.id, entry.id, false).unwrap();
    assert_eq!(same.meta().hash_key, hash_key);
    assert_eq!(&*same.begin_read().unwrap().get(b"retained").unwrap().unwrap(), b"data");
    // Publish metadata through the real receipt boundary, without transferring
    // the new backing. Source repair alone does not qualify app-facing reads.
    let staged = bob.prepare_received_volume(volume.id).unwrap();
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        alice
            .with_replica(volume.id, database, |source| {
                staged.with_database(database, |target| {
                    if database == crate::store::DatabaseId::Index {
                        merge_index_replica(
                            &std::env::temp_dir(),
                            source,
                            target,
                            kvdb::Lww,
                            |tx| {
                                MetadataPolicy::validate_pending(
                                    tx,
                                    MetadataPolicy::MAX_MATERIALIZED_BYTES,
                                )
                            },
                        )
                    } else {
                        merge_replica(&std::env::temp_dir(), source, target, kvdb::Lww, |_| Ok(()))
                    }
                })
            })
            .unwrap();
    }
    bob.adopt_received_volume(&participant, &staged).unwrap();
    let index = bob.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let snapshot = IndexSnapshot::new(index.begin_read().unwrap());
    assert_eq!(snapshot.lookup("work").unwrap().id, entry.id);
    assert!(snapshot.lookup("notes").is_err());
    assert!(
        !snapshot.backing_ready(entry.id).unwrap(),
        "a remote head must not fabricate a ready empty backing"
    );
    assert!(bob.entry_backing(volume.id, entry.id, false).is_err());
}

#[test]
fn named_entry_current_override_gates_renames_and_unpublished_backings_stay_hidden() {
    use crate::store::volume_index::{EntryKind, IndexSnapshot};
    let (alice_dir, alice, _b, bob, volume, owner, participant) = pair();
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    let entry = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, feed.drain().records);
    alice
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, feed.drain().records);
    assert!(bob.rename_entry(&participant, volume.id, entry.id, "denied").is_err());
    assert!(bob.remove_entry(&participant, volume.id, entry.id).is_err());
    assert!(alice.rename_entry(&owner, volume.id, entry.id, "owner").is_err());
    assert!(matches!(alice.entry(&owner, volume.id, entry.id), Err(Error::AccessDenied(_))));
    let details = alice.entry_details(&owner, volume.id, entry.id).unwrap();
    assert!(!details.writable);
    assert!(details.manage_permissions);
    assert_eq!(details.permissions, EntryAcl::Replace(Acl::default()));
    let details = bob.entry_details(&participant, volume.id, entry.id).unwrap();
    assert!(!details.writable);
    assert!(!details.manage_permissions);
    // The owner can restore inheritance but cannot bypass content restrictions.
    alice.set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    alice.rename_entry(&owner, volume.id, entry.id, "owner").unwrap();
    let orphan = crate::store::EntryId::generate().unwrap();
    alice.entry_backing(volume.id, orphan, true).unwrap();
    super::super::initialization::sync_directory(
        &alice.path.join(crate::paths::VOLUMES).join(volume.id.to_string()),
    )
    .unwrap();
    drop(alice);
    let reopened = LocalStore::open(alice_dir.path()).unwrap();
    assert!(reopened.entry_backing(volume.id, orphan, false).is_ok());
    let database = reopened.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    assert!(snapshot.entry(orphan).is_err());
    assert_eq!(snapshot.lookup("owner").unwrap().id, entry.id);
    assert!(snapshot.backing_ready(entry.id).unwrap());
}

#[test]
fn named_entry_concurrent_renames_have_one_current_name_and_retained_revisions() {
    use crate::store::volume_index::{EntryKind, IndexSnapshot};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let mut alice_feed = alice.take_change_receiver().unwrap();
    let mut bob_feed = bob.take_change_receiver().unwrap();
    clear(&mut alice_feed);
    clear(&mut bob_feed);
    let entry = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, alice_feed.drain().records);
    alice.rename_entry(&owner, volume.id, entry.id, "work").unwrap();
    bob.rename_entry(&participant, volume.id, entry.id, "personal").unwrap();
    let a = alice_feed.drain().records;
    let b = bob_feed.drain().records;
    receive_entry_records(&alice, &bob, &participant, volume.id, a);
    receive_entry_records(&bob, &alice, &owner, volume.id, b);
    let a = alice.entry(&owner, volume.id, entry.id).unwrap();
    let b = bob.entry(&participant, volume.id, entry.id).unwrap();
    assert_eq!(a, b);
    for store in [&alice, &bob] {
        let database = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        let index = IndexSnapshot::new(database.begin_read().unwrap());
        assert!(index.lookup("notes").is_err());
        assert_eq!(index.lookup(&a.info.name).unwrap().id, entry.id);
        let losing_name = if a.info.name == "work" { "personal" } else { "work" };
        assert!(index.lookup(losing_name).is_err());
        let revisions = index.revisions(entry.id, None, 64).unwrap().revisions;
        assert_eq!(revisions.len(), 3);
        assert!(revisions.iter().any(|r| matches!(
            &r.change, crate::store::volume_index::EntryChange::Name { name } if name == losing_name
        )));
    }
}

#[test]
fn independent_blob_rename_retirement_and_shared_content_survive_eviction_and_restart() {
    // Both entries share immutable content; entry lifetime never depends on an
    // object field. Retirement and cache eviction retain prepared versions.
    use crate::store::volume_index::{EntryKind, IndexSnapshot};
    let (directory, store, volume, owner) = setup();
    let first = store.create_blob(&owner, volume.id, "photos/cat", b"shared").unwrap();
    let second = store.create_blob(&owner, volume.id, "photos/other", b"shared").unwrap();
    assert_ne!(first.id, second.id);
    let EntryKind::Blob { digest, encrypted_key, published_at_micros, .. } =
        first.info.kind.clone()
    else {
        panic!()
    };
    assert_ne!(digest, encrypted_key);
    assert!(published_at_micros > 0);
    store.rename_entry(&owner, volume.id, first.id, "photos/kitten").unwrap();
    assert_eq!(store.entry(&owner, volume.id, first.id).unwrap().info.kind, first.info.kind);
    assert_eq!(store.read_blob_entry(&owner, volume.id, first.id).unwrap(), b"shared");
    store.remove_entry(&owner, volume.id, first.id).unwrap();
    assert!(store.read_blob_entry(&owner, volume.id, first.id).is_err());
    store.evict_blobs(0).unwrap();
    assert_eq!(store.read_blob_entry(&owner, volume.id, second.id).unwrap(), b"shared");
    let updated = store.write_blob(&owner, volume.id, second.id, b"new").unwrap();
    assert_eq!(updated.info.name, second.info.name);
    assert_eq!(updated.id, second.id);
    assert_eq!(updated.name_revision, second.name_revision);
    assert_ne!(updated.content_revision, second.content_revision);
    assert_eq!(fs::read(store.blob_path(&digest, false)).unwrap(), b"shared");
    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    assert_eq!(reopened.read_blob_entry(&owner, volume.id, second.id).unwrap(), b"new");
    assert!(reopened.read_blob_entry(&owner, volume.id, first.id).is_err());
    let database = reopened.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let index = IndexSnapshot::new(database.begin_read().unwrap());
    assert!(index.backing_ready(second.id).unwrap());
    assert!(index.entry(first.id).unwrap().deleted);
    assert_eq!(index.revisions(second.id, None, 64).unwrap().revisions.len(), 2);
}

#[test]
fn removing_an_object_file_reference_keeps_the_independent_named_blob() {
    // Exercise the shared plaintext pool while the object API is being replaced.
    // Releasing its last reference must respect a named entry's retention pin.
    let (_directory, store, volume, owner) = setup();
    let named = store.create_blob(&owner, volume.id, "keep", b"shared").unwrap();
    let object = store.create_object(&owner, data(&store, volume.id), id(1)).unwrap();
    store
        .edit_fields(
            &owner,
            data(&store, volume.id),
            object.object_id,
            &BTreeMap::from([(
                id(4),
                Some(vec![Value::File { volume_id: volume.id, entry_id: named.id }]),
            )]),
        )
        .unwrap();
    store
        .edit_fields(
            &owner,
            data(&store, volume.id),
            object.object_id,
            &BTreeMap::from([(id(4), None)]),
        )
        .unwrap();
    store.evict_blobs(0).unwrap();
    assert_eq!(store.read_blob_entry(&owner, volume.id, named.id).unwrap(), b"shared");
}

#[test]
fn saturated_blob_preparations_reject_without_waiting_for_content_io() {
    // Two real preparations stop before publication. A third request receives
    // explicit admission failure, then both admitted publications can finish.
    let (_directory, store, volume, owner) = setup();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            let (ready, changed) = &*self.0;
            *ready.lock().unwrap() = true;
            changed.notify_all();
        }
    }
    let (arrived, arrivals) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
    let failed = arrived.clone();
    let unblock = release.clone();
    *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
        arrived.send(Ok(())).unwrap();
        let (state, changed) = &*unblock;
        let mut ready = state.lock().unwrap();
        while !*ready {
            ready = changed.wait(ready).unwrap();
        }
        Ok(())
    }));
    std::thread::scope(|scope| {
        // Release paused workers before scope joins, including on watchdog failure.
        let unblock = Release(release.clone());
        let worker = |name, content| {
            let result = store.create_blob(&owner, volume.id, name, content);
            if let Err(error) = &result {
                failed.send(Err(error.to_string())).unwrap();
            }
            result
        };
        let a = scope.spawn(move || worker("a", b"first"));
        let b = scope.spawn(move || worker("b", b"second"));
        arrivals.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();
        arrivals.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();
        assert!(matches!(
            store.create_blob(&owner, volume.id, "c", b"third"),
            Err(Error::LocalContention(_))
        ));
        drop(unblock);
        assert!(a.join().unwrap().is_ok());
        assert!(b.join().unwrap().is_ok());
    });
    assert_eq!(store.entries(&owner, volume.id, None, 64).unwrap().entries.len(), 3);
    assert_eq!(store.entry_blobs.active.load(std::sync::atomic::Ordering::Acquire), 0);
}

#[test]
fn independent_blob_publication_failure_retains_bytes_without_exposing_a_name() {
    // Stop after durable preparation and pinning, before the authoritative
    // declaration. Reopening exposes no entry with missing or partial content.
    use crate::store::volume_index::IndexSnapshot;
    let (directory, store, volume, owner) = setup();
    *store.entry_blobs.before_publish.lock().unwrap() =
        Some(Arc::new(|| Err(Error::System("interrupted before blob publication".into()))));
    let before = store.entries(&owner, volume.id, None, 64).unwrap();
    assert!(store.create_blob(&owner, volume.id, "pending", b"retained").is_err());
    assert_eq!(before, store.entries(&owner, volume.id, None, 64).unwrap());
    let digest = *blake3::hash(b"retained").as_bytes();
    assert_eq!(fs::read(store.blob_path(&digest, false)).unwrap(), b"retained");
    store.evict_blobs(0).unwrap();
    assert_eq!(fs::read(store.blob_path(&digest, false)).unwrap(), b"retained");
    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    let database = reopened.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    assert!(IndexSnapshot::new(database.begin_read().unwrap()).lookup("pending").is_err());
    let published = reopened.create_blob(&owner, volume.id, "pending", b"retained").unwrap();
    assert_eq!(reopened.read_blob_entry(&owner, volume.id, published.id).unwrap(), b"retained");
}

#[test]
fn independent_blob_write_rechecks_permissions_after_preparation() {
    // Revocation during file I/O must reject the subsequent index publication.
    let (_directory, store, volume, owner) = setup();
    let store = Arc::new(store);
    let entry = store.create_blob(&owner, volume.id, "file", b"before").unwrap();
    let weak = Arc::downgrade(&store);
    let writer = owner.clone();
    *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
        weak.upgrade().unwrap().set_entry_permissions(
            &writer,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl::default()),
        )
    }));
    assert!(store.write_blob(&owner, volume.id, entry.id, b"rejected").is_err());
    assert!(store.read_blob_entry(&owner, volume.id, entry.id).is_err());
    assert_eq!(
        store.entry_details(&owner, volume.id, entry.id).unwrap().entry.info.kind,
        entry.info.kind
    );
    assert_eq!(store.entry_blobs.active.load(std::sync::atomic::Ordering::Acquire), 0);
}

#[test]
fn received_new_blob_metadata_invalidates_old_content_readiness() {
    // Replicate current lengths without lazy blob bodies, then change content
    // and deny Read. Size uses admitted metadata, never stale bodies or partial sums.
    use crate::store::volume_index::IndexSnapshot;
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let size = |entry| bob.payload_size(&participant, volume.id, entry, &cancelled);
    let base_size = size(None).unwrap();
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    let entry = alice.create_blob(&owner, volume.id, "folder/file", b"first").unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, feed.drain().records);
    let folder = bob.resolve_entry(&participant, volume.id, "folder").unwrap();
    let database = bob.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    assert!(!IndexSnapshot::new(database.begin_read().unwrap()).backing_ready(entry.id).unwrap());
    assert_eq!(size(Some(entry.id)).unwrap(), 5);
    assert_eq!(size(Some(folder.id)).unwrap(), 5);
    assert_eq!(size(None).unwrap(), base_size + 5);
    assert!(bob.read_blob_entry(&participant, volume.id, entry.id).is_err());
    let mut tx = database.begin_write(now_unix_secs()).unwrap();
    crate::store::volume_index::mark_backing_ready(&mut tx, entry.id).unwrap();
    tx.commit().unwrap();
    assert!(IndexSnapshot::new(database.begin_read().unwrap()).backing_ready(entry.id).unwrap());
    alice.write_blob(&owner, volume.id, entry.id, b"second").unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, feed.drain().records);
    assert!(!IndexSnapshot::new(database.begin_read().unwrap()).backing_ready(entry.id).unwrap());
    assert_eq!(size(Some(entry.id)).unwrap(), 6);
    assert_eq!(size(Some(folder.id)).unwrap(), 6);
    assert_eq!(size(None).unwrap(), base_size + 6);
    assert!(bob.read_blob_entry(&participant, volume.id, entry.id).is_err());
    alice
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, feed.drain().records);
    for target in [Some(entry.id), Some(folder.id), None] {
        assert!(matches!(size(target), Err(Error::AccessDenied(_))));
    }
}

#[test]
fn payload_size_uses_projected_directory_visibility() {
    // Merge an unseen child with its parent's deletion. The visible surviving
    // directory uses namespace admission while directly retired leaves remain denied.
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let base = alice.payload_size(&owner, volume.id, None, &cancelled).unwrap();
    let mut alice_feed = alice.take_change_receiver().unwrap();
    let mut bob_feed = bob.take_change_receiver().unwrap();
    clear(&mut alice_feed);
    clear(&mut bob_feed);
    let old = alice.create_blob(&owner, volume.id, "folder/old", b"retired").unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, alice_feed.drain().records);
    let folder = alice.resolve_entry(&owner, volume.id, "folder").unwrap();
    let current = bob.create_blob(&participant, volume.id, "folder/new", b"current").unwrap();
    alice.remove_entry(&owner, volume.id, old.id).unwrap();
    alice.remove_entry(&owner, volume.id, folder.id).unwrap();
    let removed = alice_feed.drain().records;
    receive_entry_records(&bob, &alice, &owner, volume.id, bob_feed.drain().records);
    receive_entry_records(&alice, &bob, &participant, volume.id, removed);
    for (store, access) in [(&alice, &owner), (&bob, &participant)] {
        let details = store.entry_details(access, volume.id, folder.id).unwrap();
        assert!(!details.entry.deleted && details.readable);
        assert_eq!(store.payload_size(access, volume.id, Some(folder.id), &cancelled).unwrap(), 7);
        assert_eq!(store.payload_size(access, volume.id, None, &cancelled).unwrap(), base + 7);
        assert_eq!(store.payload_size(access, volume.id, Some(current.id), &cancelled).unwrap(), 7);
        assert!(store.payload_size(access, volume.id, Some(old.id), &cancelled).is_err());
    }
    alice
        .set_entry_permissions(&owner, volume.id, folder.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    for target in [None, Some(folder.id)] {
        assert!(matches!(
            alice.payload_size(&owner, volume.id, target, &cancelled),
            Err(Error::AccessDenied(_))
        ));
    }
}

#[test]
fn blob_body_delivery_reorders_without_changing_the_current_version() {
    use crate::store::volume_index::EntryAddress;
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let mut feed = alice.take_change_receiver().unwrap();
    clear(&mut feed);
    let entry = alice.create_blob(&owner, volume.id, "file", b"first").unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    let first = alice.blob_version(&owner, address, None).unwrap();
    let first_body = encrypted_bytes(&alice, &owner, &first).unwrap();
    alice.write_blob(&owner, volume.id, entry.id, b"second").unwrap();
    let second = alice.blob_version(&owner, address, None).unwrap();
    let second_body = encrypted_bytes(&alice, &owner, &second).unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, feed.drain().records);
    bob.receive_blob_version(&participant, &first, &mut &first_body[..], true).unwrap();
    assert_eq!(bob.read_blob_version(&participant, &first).unwrap(), b"first");
    assert!(bob.read_blob_entry(&participant, volume.id, entry.id).is_err());
    assert!(!bob.entry_details(&participant, volume.id, entry.id).unwrap().backing_ready);
    let mut corrupt = second_body.clone();
    corrupt[15] ^= 1;
    assert!(bob.receive_blob_version(&participant, &second, &mut &corrupt[..], true).is_err());
    bob.receive_blob_version(&participant, &second, &mut &second_body[..], true).unwrap();
    bob.receive_blob_version(&participant, &first, &mut &first_body[..], false).unwrap();
    assert_eq!(bob.read_blob_entry(&participant, volume.id, entry.id).unwrap(), b"second");
    assert!(bob.entry_details(&participant, volume.id, entry.id).unwrap().backing_ready);
    assert!(bob.blob_version_durable(&participant, &first).unwrap());
    assert!(bob.pending_entry_blobs(&participant, volume.id, None).unwrap().versions.is_empty());
    bob.remove_entry(&participant, volume.id, entry.id).unwrap();
    // Namespace deletion does not revoke a previously captured revision.
    assert_eq!(bob.read_blob_version(&participant, &first).unwrap(), b"first");
    bob.receive_blob_version(&participant, &second, &mut &second_body[..], true).unwrap();
    assert!(bob.blob_version(&participant, address, None).is_err());
}

#[test]
fn blob_upload_acknowledgment_is_body_specific_and_survives_restart() {
    use crate::store::volume_index::EntryAddress;
    let (directory, store, volume, owner) = setup();
    let first = store.create_blob(&owner, volume.id, "one", b"shared").unwrap();
    let second = store.create_blob(&owner, volume.id, "two", b"shared").unwrap();
    let version = store
        .blob_version(&owner, EntryAddress { volume: volume.id, entry: first.id }, None)
        .unwrap();
    assert_eq!(store.pending_entry_blobs(&owner, volume.id, None).unwrap().versions.len(), 2);
    assert!(store.has_pending_blob_uploads().unwrap());
    let mut forged = version.clone();
    forged.encrypted_key[0] ^= 1;
    assert!(store.acknowledge_blob_version(&owner, &forged).is_err());
    assert!(!store.blob_version_durable(&owner, &version).unwrap());
    store.acknowledge_blob_version(&owner, &version).unwrap();
    let cleanup = store.pending_entry_blobs(&owner, volume.id, None).unwrap().versions;
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].address.entry, second.id);
    assert!(store.blob_version_durable(&owner, &cleanup[0]).unwrap());
    store.acknowledge_blob_version(&owner, &cleanup[0]).unwrap();
    assert!(!store.has_pending_blob_uploads().unwrap());
    store.write_blob(&owner, volume.id, second.id, b"new").unwrap();
    assert_eq!(store.pending_entry_blobs(&owner, volume.id, None).unwrap().versions.len(), 1);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(store.blob_version_durable(&owner, &version).unwrap());
    assert_eq!(store.read_blob_version(&owner, &version).unwrap(), b"shared");
    assert_eq!(store.pending_entry_blobs(&owner, volume.id, None).unwrap().versions.len(), 1);
}

#[test]
fn blob_versions_are_bound_to_volume_history_and_current_permissions() {
    use crate::store::volume_index::EntryAddress;
    let (_directory, store, volume, owner) = setup();
    let other = store.create_volume("other", false).unwrap();
    let first = store.create_blob(&owner, volume.id, "file", b"shared").unwrap();
    let second = store.create_blob(&owner, other.id, "file", b"shared").unwrap();
    let a = store
        .blob_version(&owner, EntryAddress { volume: volume.id, entry: first.id }, None)
        .unwrap();
    let b = store
        .blob_version(&owner, EntryAddress { volume: other.id, entry: second.id }, None)
        .unwrap();
    assert_eq!(a.digest, b.digest);
    assert_ne!(a.encrypted_key, b.encrypted_key);
    let body = encrypted_bytes(&store, &owner, &a).unwrap();
    assert!(store.receive_blob_version(&owner, &b, &mut &body[..], true).is_err());
    let mut forged = a.clone();
    forged.revision = crate::store::EntryId::generate().unwrap();
    assert!(store.read_blob_version(&owner, &forged).is_err());
    store
        .set_entry_permissions(&owner, volume.id, first.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(store.read_blob_version(&owner, &a).is_err());
    assert!(store.receive_blob_version(&owner, &a, &mut &body[..], true).is_err());
    assert!(store.acknowledge_blob_version(&owner, &a).is_err());
}

#[test]
fn pending_blob_pages_bound_current_and_historical_versions() {
    let (_directory, store, volume, owner) = setup();
    let entry = store.create_blob(&owner, volume.id, "file", b"zero").unwrap();
    for n in 0..66u8 {
        store.write_blob(&owner, volume.id, entry.id, &[n]).unwrap();
    }
    let first = store.pending_entry_blobs(&owner, volume.id, None).unwrap();
    assert_eq!(first.versions.len(), 64);
    let second = store.pending_entry_blobs(&owner, volume.id, first.next).unwrap();
    assert_eq!(second.versions.len(), 3);
    assert!(second.next.is_none());
    let revisions: BTreeSet<_> =
        first.versions.iter().chain(&second.versions).map(|version| version.revision).collect();
    assert_eq!(revisions.len(), 67);
    for version in first.versions.iter().chain(&second.versions) {
        assert!(encrypted_bytes(&store, &owner, version).is_ok());
    }
}

fn write_only_blob_acl(account: Id, service: bool) -> EntryAcl {
    let mut grants = vec![Grant {
        subject: Subject::Principal(Principal::Human(account)),
        permissions: vec![Permission::Write],
    }];
    if service {
        grants.push(Grant {
            subject: Subject::Principal(crate::store::PLAYBIT_REPLICA_PRINCIPAL),
            permissions: vec![Permission::Read],
        });
    }
    EntryAcl::Replace(Acl { grants })
}

#[cfg(feature = "connected-engine")]
#[test]
fn current_entry_write_admits_publication_without_catalog_read() {
    let (_directory, store, _, owner) = setup();
    let volume = store.create_volume("Publish", true).unwrap();
    let writer = Access::owner(Id::generate().unwrap());
    let entry = store.create_blob(&owner, volume.id, "file", b"existing").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            write_only_blob_acl(writer.account, true),
        )
        .unwrap();
    let mut feed = store.take_change_receiver().unwrap();
    clear(&mut feed);
    store.rename_entry(&writer, volume.id, entry.id, "renamed").unwrap();
    let batch = feed.drain();
    assert_eq!(batch.records.len(), 1);
    let bytes = batch.records[0].bytes();
    assert_eq!(
        store
            .publication_delta(&writer, volume.id, crate::store::DatabaseId::Index, bytes)
            .unwrap(),
        bytes
    );
    assert!(store.entries(&writer, volume.id, None, 64).is_err());
    assert!(
        store
            .directory_entries(
                &writer,
                volume.id,
                crate::store::volume_index::DirectoryTarget::Root,
                None,
                None,
                64,
                str::cmp,
            )
            .is_err()
    );
    assert!(store.read_blob_entry(&writer, volume.id, entry.id).is_err());
    // Local publication advice cannot survive a current ACL withdrawal.
    store
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(
        store
            .publication_delta(&writer, volume.id, crate::store::DatabaseId::Index, bytes)
            .is_err()
    );
    // Deletion publication is allowed under the retained ACL, without reviving
    // the ID or admitting content access to a deleted entry.
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            write_only_blob_acl(writer.account, true),
        )
        .unwrap();
    clear(&mut feed);
    store.remove_entry(&writer, volume.id, entry.id).unwrap();
    let batch = feed.drain();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(
        store
            .publication_delta(
                &writer,
                volume.id,
                crate::store::DatabaseId::Index,
                batch.records[0].bytes()
            )
            .unwrap(),
        batch.records[0].bytes()
    );
    assert!(store.read_blob_entry(&writer, volume.id, entry.id).is_err());
}

#[test]
fn entry_read_by_id_does_not_grant_catalog_or_name_lookup() {
    let (_directory, store, volume, owner) = setup();
    let reader = Access::owner(Id::generate().unwrap());
    let entry = store.create_blob(&owner, volume.id, "visible-by-id", b"content").unwrap();
    let other = store.create_blob(&owner, volume.id, "another", b"private").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(Principal::Human(reader.account)),
                        permissions: vec![Permission::Read],
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(owner.account)),
                        permissions: vec![Permission::Write],
                    },
                ],
            }),
        )
        .unwrap();
    assert_eq!(store.read_blob_entry(&reader, volume.id, entry.id).unwrap(), b"content");
    assert!(store.entry_details(&reader, volume.id, entry.id).unwrap().readable);
    assert!(store.entries(&reader, volume.id, None, 64).is_err());
    assert!(store.volume_details(&reader, volume.id).is_err());
    assert!(
        store
            .directory_entries(
                &reader,
                volume.id,
                crate::store::volume_index::DirectoryTarget::Entry(entry.id),
                None,
                None,
                64,
                str::cmp,
            )
            .is_err()
    );
    assert!(store.resolve_entry(&reader, volume.id, "visible-by-id").is_err());
    assert!(store.resolve_entry(&reader, volume.id, "missing").is_err());
    assert!(store.entry(&reader, volume.id, other.id).is_err());
    assert!(store.entry_details(&reader, volume.id, other.id).is_err());
    store.rename_entry(&owner, volume.id, entry.id, "renamed").unwrap();
    assert_eq!(store.entry(&reader, volume.id, entry.id).unwrap().info.name, "renamed");
    assert_eq!(store.read_blob_entry(&reader, volume.id, entry.id).unwrap(), b"content");
    assert!(store.resolve_entry(&reader, volume.id, "renamed").is_err());
    store
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(store.entry(&reader, volume.id, entry.id).is_err());
    assert!(store.entry_details(&reader, volume.id, entry.id).is_err());
    assert!(store.read_blob_entry(&reader, volume.id, entry.id).is_err());
}

#[test]
fn volume_details_keep_origin_clocks_across_rename_and_replica_import() {
    let (_alice_dir, alice, _bob_dir, bob, volume, owner, participant) = pair();
    let created = alice.volume_details(&owner, volume.id).unwrap();
    assert_eq!(created.volume, volume);
    assert_eq!(created.created_clock, created.name_clock);
    assert_ne!(created.created_clock, 0);
    assert_eq!(bob.volume_details(&participant, volume.id).unwrap(), created);
    alice.label_volume(&owner, volume.id, "Renamed volume").unwrap();
    let renamed = alice.volume_details(&owner, volume.id).unwrap();
    assert_eq!(renamed.created_clock, created.created_clock);
    assert!(renamed.name_clock > created.name_clock);
    assert_eq!(renamed.volume.name, "Renamed volume");
    assert_eq!(renamed.permissions, created.permissions);
    sync_volume(&alice, &bob, volume.id);
    assert_eq!(bob.volume_details(&participant, volume.id).unwrap(), renamed);
    // An empty default ACL does not strand the owner repair UI.
    alice.set_permissions(&owner, volume.id, Acl::default()).unwrap();
    let repaired = alice.volume_details(&owner, volume.id).unwrap();
    assert_eq!(repaired.created_clock, created.created_clock);
    assert_eq!(repaired.name_clock, renamed.name_clock);
    assert_eq!(repaired.permissions, Acl::default());
    sync_volume(&alice, &bob, volume.id);
    assert!(bob.volume_details(&participant, volume.id).is_err());
}

#[test]
fn directory_identity_creation_follows_renamed_parent_instead_of_replacement_path() {
    use crate::store::volume_index::EntryKind;
    let (_directory, store, volume, owner) = setup();
    let store = Arc::new(store);
    store.create_database(&owner, volume.id, "Folder/seed", EntryKind::KvDb {}).unwrap();
    let parent = store.resolve_entry(&owner, volume.id, "Folder").unwrap();
    let weak = Arc::downgrade(&store);
    let actor = owner.clone();
    let volume_id = volume.id;
    *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
        let store = weak.upgrade().ok_or(Error::Internal("fixture store dropped"))?;
        store.rename_entry_component(&actor, volume_id, parent.id, "Moved")?;
        store.create_database(&actor, volume_id, "Folder/replacement-seed", EntryKind::KvDb {})?;
        Ok(())
    }));
    let blob = store
        .create_blob_from_reader_in(&owner, volume.id, parent.id, "photo", &mut &b"image bytes"[..])
        .unwrap();
    *store.entry_blobs.before_publish.lock().unwrap() = None;
    let kv = store
        .create_database_in(&owner, volume.id, parent.id, "settings", EntryKind::KvDb {})
        .unwrap();
    let log = store.create_bloblog_in(&owner, volume.id, parent.id, "events").unwrap();
    for entry in [blob, kv, log] {
        assert_eq!(entry.info.namespace.parent, Some(parent.id));
        let path = format!("Moved/{}", entry.info.name);
        assert_eq!(store.resolve_entry(&owner, volume.id, &path).unwrap().id, entry.id);
        assert!(
            store.resolve_entry(&owner, volume.id, &format!("Folder/{}", entry.info.name)).is_err()
        );
    }
    let replaced = store.resolve_entry(&owner, volume.id, "Folder").unwrap();
    assert_ne!(replaced.id, parent.id);
}

#[test]
fn directory_identity_blob_publication_rechecks_deletion_and_parent_read() {
    use crate::store::volume_index::EntryKind;
    for deleted in [false, true] {
        let (_directory, store, volume, owner) = setup();
        let store = Arc::new(store);
        store.create_database(&owner, volume.id, "Folder/seed", EntryKind::KvDb {}).unwrap();
        let parent = store.resolve_entry(&owner, volume.id, "Folder").unwrap();
        let weak = Arc::downgrade(&store);
        let actor = owner.clone();
        let volume_id = volume.id;
        *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
            let store = weak.upgrade().ok_or(Error::Internal("fixture store dropped"))?;
            if deleted {
                store.remove_entry(&actor, volume_id, parent.id)?;
                store.create_database(
                    &actor,
                    volume_id,
                    "Folder/replacement-seed",
                    EntryKind::KvDb {},
                )?;
            } else {
                store.set_entry_permissions(
                    &actor,
                    volume_id,
                    parent.id,
                    EntryAcl::Replace(Acl::default()),
                )?;
            }
            Ok(())
        }));
        assert!(
            store
                .create_blob_from_reader_in(
                    &owner,
                    volume.id,
                    parent.id,
                    "photo",
                    &mut &b"image bytes"[..],
                )
                .is_err()
        );
        *store.entry_blobs.before_publish.lock().unwrap() = None;
        assert!(
            store
                .create_database_in(&owner, volume.id, parent.id, "settings", EntryKind::KvDb {})
                .is_err()
        );
        assert!(store.create_bloblog_in(&owner, volume.id, parent.id, "events").is_err());
        let entries = store.entries(&owner, volume.id, None, 64).unwrap().entries;
        assert!(!entries.iter().any(|entry| entry.name == "photo"));
    }
}

#[test]
fn directory_identity_creation_requires_one_component_and_live_directory() {
    use crate::store::volume_index::EntryKind;
    let (_directory, store, volume, owner) = setup();
    store.create_database(&owner, volume.id, "Folder/seed", EntryKind::KvDb {}).unwrap();
    let parent = store.resolve_entry(&owner, volume.id, "Folder").unwrap();
    for name in [String::new(), "a/b".into(), ".".into(), "..".into(), "x".repeat(256)] {
        assert!(
            store
                .create_database_in(&owner, volume.id, parent.id, &name, EntryKind::KvDb {})
                .is_err()
        );
        assert!(store.create_bloblog_in(&owner, volume.id, parent.id, &name).is_err());
        assert!(
            store
                .create_blob_from_reader_in(
                    &owner,
                    volume.id,
                    parent.id,
                    &name,
                    &mut std::io::empty()
                )
                .is_err()
        );
    }
    let file = store.create_database(&owner, volume.id, "Settings", EntryKind::KvDb {}).unwrap();
    assert!(
        store
            .create_database_in(&owner, volume.id, file.id, "invalid", EntryKind::KvDb {})
            .is_err()
    );
    store.remove_entry(&owner, volume.id, parent.id).unwrap();
    assert!(
        store
            .create_database_in(&owner, volume.id, parent.id, "invalid", EntryKind::KvDb {})
            .is_err()
    );
}

#[test]
fn directory_catalog_metadata_preserves_owner_repair_without_content_read() {
    use crate::store::volume_index::{DirectoryListing, DirectoryTarget};
    let (_directory, store, volume, owner) = setup();
    let existing = data(&store, volume.id).entry;
    let entry = store.create_blob(&owner, volume.id, "private", b"content").unwrap();
    store
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(!store.entry_details(&owner, volume.id, entry.id).unwrap().readable);
    assert!(store.read_blob_entry(&owner, volume.id, entry.id).is_err());
    let DirectoryListing::Page { entries, parent, ancestors, .. } = store
        .directory_entries(
            &owner,
            volume.id,
            DirectoryTarget::Entry(entry.id),
            None,
            None,
            64,
            str::cmp,
        )
        .unwrap()
    else {
        panic!("catalog metadata")
    };
    assert_eq!(parent, None);
    assert!(ancestors.is_empty());
    assert_eq!(
        entries.iter().map(|entry| entry.id).collect::<BTreeSet<_>>(),
        BTreeSet::from([existing, entry.id]),
    );
    // Grant repair still targets the original identity; discovery adds no Read.
    store.set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    assert_eq!(store.read_blob_entry(&owner, volume.id, entry.id).unwrap(), b"content");
    store.remove_entry(&owner, volume.id, entry.id).unwrap();
    let DirectoryListing::Page { entries, .. } = store
        .directory_entries(&owner, volume.id, DirectoryTarget::Root, None, None, 64, str::cmp)
        .unwrap()
    else {
        panic!("live catalog")
    };
    assert_eq!(entries.iter().map(|entry| entry.id).collect::<Vec<_>>(), [existing]);
}

#[cfg(feature = "connected-engine")]
#[test]
fn selected_blob_outbox_never_visits_sibling_entries() {
    let (_directory, store, volume, owner) = setup();
    let first = store.create_blob(&owner, volume.id, "first", b"one").unwrap();
    let second = store.create_blob(&owner, volume.id, "second", b"two").unwrap();
    let address = crate::store::volume_index::EntryAddress { volume: volume.id, entry: first.id };
    let page = store.pending_selected_entry_blobs(&owner, address, None).unwrap();
    assert_eq!(page.versions.len(), 1);
    assert_eq!(page.versions[0].address, address);
    assert!(page.next.is_none());
    assert!(matches!(
        store.pending_selected_entry_blobs(
            &owner,
            address,
            Some((second.id, second.content_revision))
        ),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(store.pending_entry_blobs(&owner, volume.id, None).unwrap().versions.len(), 2);
}

#[test]
fn entry_revocation_without_session_denies_cached_grants_after_reopen() {
    let (directory, store, volume, owner) = setup();
    let reader = Access::owner(Id::generate().unwrap());
    let entry = store.create_blob(&owner, volume.id, "by-id", b"retained").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(reader.account)),
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    assert_eq!(store.read_blob_entry(&reader, volume.id, entry.id).unwrap(), b"retained");
    store
        .with_replica(volume.id, crate::store::DatabaseId::Revocations, |replica| {
            put(replica, reader.account.to_string().as_bytes(), &"removed")
        })
        .unwrap();
    let check = |store: &LocalStore| {
        assert!(
            matches!(store.entry(&reader, volume.id, entry.id), Err(Error::ParticipantRevoked { volume: denied_volume, account })
            if denied_volume == volume.id && account == reader.account)
        );
        assert!(matches!(
            store.read_blob_entry(&reader, volume.id, entry.id),
            Err(Error::ParticipantRevoked { volume: denied_volume, account })
            if denied_volume == volume.id && account == reader.account
        ));
        assert!(store.entry_details(&reader, volume.id, entry.id).is_err());
        let database = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        assert!(
            crate::store::volume_index::IndexSnapshot::new(database.begin_read().unwrap())
                .live_entry(entry.id)
                .is_ok(),
            "revocation retains indexed data"
        );
    };
    check(&store);
    drop(store);
    check(&LocalStore::open(directory.path()).unwrap());
}

#[test]
fn session_entry_admission_checks_each_dependency_without_fencing_unrelated_access() {
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
    let target = store.create_volume("Personal", false).unwrap();
    store
        .select_personal_volumes(
            &session.id,
            vec![PersonalVolumeGrant { volume: target.id, write: true }],
        )
        .unwrap();
    let entry = store.create_blob(&owner, target.id, "content", b"retained").unwrap();
    let access = Access::session(owner.account, &session.id);
    for dependency in [root.id, target.id] {
        assert_eq!(store.read_blob_entry(&access, target.id, entry.id).unwrap(), b"retained");
        store
            .with_replica(dependency, crate::store::DatabaseId::Revocations, |replica| {
                put(replica, owner.account.to_string().as_bytes(), &"removed")
            })
            .unwrap();
        assert!(matches!(
            store.read_blob_entry(&access, target.id, entry.id),
            Err(Error::ParticipantRevoked { volume, account })
            if volume == dependency && account == access.account
        ));
        assert!(store.write_blob(&access, target.id, entry.id, b"denied").is_err());
        if dependency != target.id {
            assert_eq!(store.read_blob_entry(&owner, target.id, entry.id).unwrap(), b"retained");
        } else {
            assert!(store.read_blob_entry(&owner, target.id, entry.id).is_err());
        }
        store
            .with_replica(dependency, crate::store::DatabaseId::Revocations, |replica| {
                let mut transaction = replica.begin_write(now_unix_secs())?;
                transaction.del(owner.account.to_string().as_bytes())?;
                transaction.commit()?;
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(store.read_blob_entry(&access, target.id, entry.id).unwrap(), b"retained");
}

#[test]
fn write_only_upload_uses_entry_authority_without_catalog_or_plaintext_read() {
    use crate::store::volume_index::{EntryAddress, IndexSnapshot};
    let (_directory, store, volume, owner) = setup();
    let writer = Access::owner(Id::generate().unwrap());
    let entry = store.create_blob(&owner, volume.id, "file", b"existing").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            write_only_blob_acl(writer.account, true),
        )
        .unwrap();
    store.write_blob(&writer, volume.id, entry.id, b"supplied").unwrap();
    assert!(store.entries(&writer, volume.id, None, 64).is_err());
    assert!(store.read_blob_entry(&writer, volume.id, entry.id).is_err());
    let index = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let version = IndexSnapshot::new(index.begin_read().unwrap())
        .blob_version(EntryAddress { volume: volume.id, entry: entry.id }, None)
        .unwrap();
    assert!(
        store.pending_entry_blobs(&writer, volume.id, None).unwrap().versions.contains(&version)
    );
    assert!(store.verify_blob_version(&writer, &version).is_err());
    assert!(store.blob_version_durable(&writer, &version).is_err());
    let encrypted = encrypted_bytes(&store, &writer, &version).unwrap();
    let mut plaintext = Vec::new();
    crate::store::open_blob_to(
        &store.credentials(volume.id).unwrap().blob_key,
        &version,
        &mut &encrypted[..],
        &mut plaintext,
    )
    .unwrap();
    assert_eq!(plaintext, b"supplied");
    assert!(!store.blob_upload_durable(&writer, &version).unwrap());
    let mut forged = version.clone();
    forged.digest[0] ^= 1;
    assert!(store.encrypt_blob_version(&writer, &forged).is_err());
    assert!(store.acknowledge_blob_upload(&writer, &forged).is_err());
    store.acknowledge_blob_upload(&writer, &version).unwrap();
    assert!(store.blob_upload_durable(&writer, &version).unwrap());
    assert!(store.read_blob_version(&writer, &version).is_err());
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            write_only_blob_acl(writer.account, false),
        )
        .unwrap();
    assert!(store.blob_upload_durable(&writer, &version).is_err());
    assert!(store.acknowledge_blob_upload(&writer, &version).is_err());
    store
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(store.encrypt_blob_version(&writer, &version).is_err());
    assert!(store.pending_entry_blobs(&writer, volume.id, None).unwrap().versions.is_empty());
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            write_only_blob_acl(writer.account, true),
        )
        .unwrap();
    store.remove_entry(&writer, volume.id, entry.id).unwrap();
    assert!(store.encrypt_blob_version(&writer, &version).is_err());
    assert!(store.acknowledge_blob_upload(&writer, &version).is_err());
}

#[test]
fn pending_blob_pages_bound_denied_scans_and_reach_later_eligible_entries() {
    let (_directory, store, volume, owner) = setup();
    let writer = Access::owner(Id::generate().unwrap());
    let denied = store.create_blob(&owner, volume.id, "denied", b"zero").unwrap();
    for value in 0..66u8 {
        store.write_blob(&owner, volume.id, denied.id, &[value]).unwrap();
    }
    let eligible = store.create_blob(&owner, volume.id, "eligible", b"eligible").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            eligible.id,
            write_only_blob_acl(writer.account, true),
        )
        .unwrap();
    let mut after = None;
    let mut versions = Vec::new();
    let mut pages = 0;
    loop {
        let page = store.pending_entry_blobs(&writer, volume.id, after).unwrap();
        pages += 1;
        assert!(pages <= 2);
        versions.extend(page.versions);
        let Some(next) = page.next else { break };
        assert_ne!(after, Some(next));
        after = Some(next);
    }
    assert_eq!(pages, 2);
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].address.entry, eligible.id);
}

fn named_schema(field: u8) -> Schema {
    Schema {
        types: [(
            id(20),
            TypeDeclaration {
                name: "Note".into(),
                deprecated: false,
                fields: [(
                    id(field),
                    FieldDeclaration {
                        name: "body".into(),
                        value_type: ValueType::Text,
                        repeated: false,
                        deprecated: false,
                    },
                )]
                .into(),
            },
        )]
        .into(),
    }
}

#[test]
fn database_schemas_are_independent_and_noop_establishment_emits_nothing() {
    use crate::store::volume_index::EntryKind;
    let (_directory, store, volume, owner) = setup();
    let first = store
        .create_database(
            &owner,
            volume.id,
            "notes",
            EntryKind::ObjectDb { schema: Schema::default() },
        )
        .unwrap();
    let second = store
        .create_database(
            &owner,
            volume.id,
            "archive",
            EntryKind::ObjectDb { schema: Schema::default() },
        )
        .unwrap();
    let mut feed = store.take_change_receiver().unwrap();
    clear(&mut feed);
    let schema = named_schema(21);
    store
        .establish_schema(
            &owner,
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: first.id },
            &schema,
        )
        .unwrap();
    assert_eq!(
        store.entry(&owner, volume.id, first.id).unwrap().info.kind,
        EntryKind::ObjectDb { schema: schema.clone() }
    );
    assert_eq!(
        store.entry(&owner, volume.id, second.id).unwrap().info.kind,
        EntryKind::ObjectDb { schema: Schema::default() }
    );
    clear(&mut feed);
    let before = store.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    store
        .establish_schema(
            &owner,
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: first.id },
            &schema,
        )
        .unwrap();
    assert_eq!(store.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
    assert!(feed.drain().records.is_empty());
    store
        .set_entry_permissions(&owner, volume.id, first.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(
        store
            .establish_schema(
                &owner,
                crate::store::volume_index::EntryAddress { volume: volume.id, entry: first.id },
                &named_schema(22)
            )
            .is_err()
    );
    assert_eq!(
        store.entry_details(&owner, volume.id, first.id).unwrap().entry.info.kind,
        EntryKind::ObjectDb { schema }
    );
}

#[test]
fn named_database_offline_schema_changes_converge_after_rename_and_restart() {
    use crate::store::volume_index::EntryKind;
    let (alice_dir, alice, bob_dir, bob, volume, owner, participant) = pair();
    let mut a_feed = alice.take_change_receiver().unwrap();
    let mut b_feed = bob.take_change_receiver().unwrap();
    clear(&mut a_feed);
    clear(&mut b_feed);
    let entry = alice
        .create_database(
            &owner,
            volume.id,
            "notes",
            EntryKind::ObjectDb { schema: Schema::default() },
        )
        .unwrap();
    receive_entry_records(&alice, &bob, &participant, volume.id, a_feed.drain().records);
    alice
        .establish_schema(
            &owner,
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id },
            &named_schema(21),
        )
        .unwrap();
    bob.establish_schema(
        &participant,
        crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id },
        &named_schema(22),
    )
    .unwrap();
    alice.rename_entry(&owner, volume.id, entry.id, "work/notes").unwrap();
    let a = a_feed.drain().records;
    let b = b_feed.drain().records;
    receive_entry_records(&alice, &bob, &participant, volume.id, a);
    receive_entry_records(&bob, &alice, &owner, volume.id, b);
    let expected = alice.entry(&owner, volume.id, entry.id).unwrap();
    assert_eq!(bob.entry(&participant, volume.id, entry.id).unwrap(), expected);
    let EntryKind::ObjectDb { schema } = &expected.info.kind else { panic!() };
    assert_eq!(schema.types[&id(20)].fields.len(), 2);
    assert!(matches!(schema.field_by_name(id(20), "body"), Err(Error::AlreadyExists(_))));
    drop(alice);
    drop(bob);
    let alice = LocalStore::open(alice_dir.path()).unwrap();
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    assert_eq!(alice.entry(&owner, volume.id, entry.id).unwrap(), expected);
    assert_eq!(bob.entry(&participant, volume.id, entry.id).unwrap(), expected);
}

#[test]
fn named_entry_recovery_rejects_swapped_or_missing_published_backing() {
    use crate::store::volume_index::EntryKind;
    for missing in [false, true] {
        let (directory, store, volume, owner) = setup();
        let first = store.create_database(&owner, volume.id, "first", EntryKind::KvDb {}).unwrap();
        let second =
            store.create_database(&owner, volume.id, "second", EntryKind::KvDb {}).unwrap();
        drop(store);
        let path = directory.path().join(crate::paths::VOLUMES).join(volume.id.to_string());
        std::fs::remove_file(path.join(format!("{}.kvdb", first.id))).unwrap();
        if !missing {
            std::fs::rename(
                path.join(format!("{}.kvdb", second.id)),
                path.join(format!("{}.kvdb", first.id)),
            )
            .unwrap();
        }
        let store = LocalStore::open(directory.path()).unwrap();
        let result = store.entry_backing(volume.id, first.id, false);
        assert!(
            result.is_err(),
            "published data cannot turn into an empty or another entry's database"
        );
        assert_eq!(store.entry(&owner, volume.id, first.id).unwrap().info.name, "first");
    }
}
#[test]
fn entry_name_resolution_tracks_rename_and_rechecks_current_permissions() {
    use crate::store::volume_index::EntryKind;
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Resolve", false).unwrap();
    let access = Access::owner(store.account().id);
    let entry = store.create_database(&access, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    assert_eq!(store.resolve_entry(&access, volume.id, "notes").unwrap().id, entry.id);
    store.rename_entry(&access, volume.id, entry.id, "work/notes").unwrap();
    assert!(matches!(store.resolve_entry(&access, volume.id, "notes"), Err(Error::NotFound(_))));
    assert_eq!(store.resolve_entry(&access, volume.id, "work/notes").unwrap().id, entry.id);
    store
        .set_entry_permissions(&access, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(
        store.resolve_entry(&access, volume.id, "work/notes"),
        Err(Error::AccessDenied(_))
    ));
    assert!(
        store
            .entries(&access, volume.id, None, 64)
            .unwrap()
            .entries
            .iter()
            .any(|row| row.id == entry.id)
    );
    store.set_entry_permissions(&access, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    let replacement =
        store.create_database(&access, volume.id, "work/notes", EntryKind::KvDb {}).unwrap();
    assert_ne!(replacement.id, entry.id);
    assert_eq!(store.resolve_entry(&access, volume.id, "work/notes").unwrap().id, replacement.id);
    drop(store);
}

#[test]
fn peer_publication_qualifies_birth_rename_and_deletion_across_restart() {
    use crate::store::{
        DatabaseId, DatabasePolicy,
        volume_index::{EntryKind, IndexSnapshot},
    };
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "peer-born", EntryKind::KvDb {}).unwrap();
    let source = alice.db(volume.id, DatabaseId::Index).unwrap();
    bob.merge_authorized_replica(&participant, volume.id, DatabaseId::Index, &source).unwrap();
    let target = bob.db(volume.id, DatabaseId::Index).unwrap();
    let pinned = IndexSnapshot::new(target.begin_read().unwrap());
    assert_eq!(pinned.entry(entry.id).unwrap().info.name, "peer-born");
    assert!(!pinned.header(entry.id).unwrap().deleted);
    DatabasePolicy::read(&target, DatabaseId::Entry(entry.id))
        .unwrap()
        .authorize_reader(ReplicaReader::Participant(bob.account().id))
        .unwrap();
    alice.rename_entry(&owner, volume.id, entry.id, "peer-renamed").unwrap();
    bob.merge_authorized_replica(&participant, volume.id, DatabaseId::Index, &source).unwrap();
    assert_eq!(
        IndexSnapshot::new(target.begin_read().unwrap()).entry(entry.id).unwrap().info.name,
        "peer-renamed"
    );
    assert_eq!(pinned.entry(entry.id).unwrap().info.name, "peer-born");
    drop(pinned);
    drop(target);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    let target = bob.db(volume.id, DatabaseId::Index).unwrap();
    assert_eq!(
        IndexSnapshot::new(target.begin_read().unwrap()).entry(entry.id).unwrap().info.name,
        "peer-renamed"
    );
    alice.remove_entry(&owner, volume.id, entry.id).unwrap();
    bob.merge_authorized_replica(&participant, volume.id, DatabaseId::Index, &source).unwrap();
    assert!(IndexSnapshot::new(target.begin_read().unwrap()).header(entry.id).unwrap().deleted);
    assert!(matches!(
        DatabasePolicy::read(&target, DatabaseId::Entry(entry.id)),
        Err(Error::NotFound(_))
    ));
}
