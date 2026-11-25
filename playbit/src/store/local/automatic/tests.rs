//! Real local profiles exercise scope identity, durable edits and export denial.
#![allow(clippy::unwrap_used, reason = "Tests assert isolated fixture operations.")]
use super::*;
use crate::package::{Package, VolumeInitializers};
use crate::store::{
    DatabaseId, InitialDatabase, InitialDatabaseKind,
    volume_index::{EntryAddress, EntryKind},
};

fn table() -> crate::store::EntryId {
    crate::store::EntryId::from_bytes([8; 15]).unwrap()
}

fn fixture() -> (tempfile::TempDir, LocalStore, Package) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let declaration = RootInitialization {
        permissions: Acl::default(),
        databases: vec![InitialDatabase {
            id: table(),
            name: "data".into(),
            kind: InitialDatabaseKind::KvDb {},
        }],
        blobs: Vec::new(),
    };
    let initializers = VolumeInitializers::new(
        [Role::Session, Role::User, Role::UserSession, Role::Local, Role::LocalSession]
            .into_iter()
            .map(|role| (role, declaration.clone()))
            .collect(),
    )
    .unwrap();
    let package = Package::create(
        "scope-test".into(),
        "Scope test".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::new(),
        &initializers,
    )
    .unwrap();
    let package = super::super::publisher::install_test_package(&store, package).unwrap();
    (directory, store, package)
}
fn session(store: &LocalStore, package: &Package) -> Session {
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let root = app.volume_id(Role::Root, None, None, None).unwrap();
    store
        .create_session_binding(SessionCreation {
            app,
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            root_volume: root,
            sync_shared: true,
            user_volume: None,
        })
        .unwrap()
}
fn access(store: &LocalStore, session: &Session) -> Access {
    Access::session_version(store.account.id, &session.id, &session.resolved_build_id)
}

#[cfg(all(feature = "connected-engine", feature = "app-host"))]
#[test]
fn inspector_activity_keeps_creation_dates_and_groups_roster_devices_by_account() {
    use crate::session_channel::{Participant, PresentParticipant, SOURCE_BYTES};
    let (directory, mut store, package) = fixture();
    store.set_session_activity_wake(|| {});
    let first = session(&store, &package);
    let second = session(&store, &package);
    let sessions = [first.clone(), second.clone()];
    let mut member = PresentParticipant {
        participant: Participant::decode(&[4; SOURCE_BYTES]).unwrap(),
        display_name: "Robin".into(),
    };
    let revision = store.delivery_revision();
    let provider = store.delivery_provider_revision(None, None).0;
    store.observe_session_participants(&first.id, &[member.clone()]);
    member.participant.device_id = Id::from_bytes([5; 16]).to_string();
    member.display_name = "Robin Fernández".into();
    store.observe_session_participants(&first.id, &[member]);
    for volume in [first.replica_volume(), second.replica_volume()] {
        assert_eq!(store.delivery_scope_revision(revision, &[volume]), (revision, None));
        assert_eq!(store.delivery_snapshot_revision(revision, &[volume]), (revision, None));
    }
    assert_eq!(store.delivery_provider_revision(provider, None), (provider, None));
    let activity = store.session_activity(&sessions).unwrap();
    let first_activity = &activity[&first.id];
    assert_eq!(first_activity.participants.len(), 1);
    assert_eq!(first_activity.participants[0].name, "Robin Fernández");
    assert!(first_activity.participants[0].last_seen >= first_activity.started.unwrap());
    assert!(activity[&second.id].participants.is_empty());
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    let reopened = store.session_activity(&sessions).unwrap();
    assert_eq!(reopened[&first.id].started, first_activity.started);
    assert!(reopened[&first.id].participants.is_empty());
}

#[test]
fn fresh_shared_is_lazy_then_publishes_one_original_capability() {
    // First use publishes one backing and stable existing outbox operations;
    // reading slots and reopening do not initialize or register another copy.
    let (_directory, store, package) = fixture();
    let session = session(&store, &package);
    let shared = session.shared_volume.unwrap();
    assert_ne!(session.root_volume, shared);
    assert_eq!(
        shared,
        store.automatic_volume_id(&session.app, Some(&session.id), Role::Session).unwrap()
    );
    assert!(store.volumes().unwrap().is_empty());
    assert!(store.pending_operations().unwrap().is_empty());
    store.automatic_volumes(&session.id).unwrap();
    assert!(store.volumes().unwrap().is_empty());
    let volume = store.open_automatic_volume(&session.id, Role::Session).unwrap();
    assert_eq!(volume.id, shared);
    assert!(volume.share_key_with_service);
    assert!(!volume.personal);
    assert_eq!(volume.owner, session.creator);
    store.require_replication_supported(shared).unwrap();
    let pending = store.pending_operations().unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|op| matches!(&op.operation,
        PendingKind::RegisterVolume { volume } if volume.id == shared)));
    assert!(pending.iter().any(|op| matches!(&op.operation,
        PendingKind::RegisterSession { session: registered, .. } if registered == &session)));
    let invitation = store.stored_session(&session.id).unwrap().invitation;
    assert!(invitation.root.is_none());
    assert!(invitation.root_credentials.is_none());
    assert_eq!(invitation.replica_volume().unwrap(), &volume);
    let receipt = store
        .metadata
        .begin_read()
        .unwrap()
        .get(format!("root-initialization/{shared}").as_bytes())
        .unwrap()
        .unwrap()
        .to_vec();
    store.open_automatic_volume(&session.id, Role::Session).unwrap();
    assert_eq!(store.pending_operations().unwrap(), pending);
    assert_eq!(
        store
            .metadata
            .begin_read()
            .unwrap()
            .get(format!("root-initialization/{shared}").as_bytes())
            .unwrap()
            .unwrap()
            .as_ref(),
        receipt
    );
    assert_eq!(store.sessions().unwrap(), [session]);
}

#[test]
fn acl_save_preserves_shared_build_provision_clocks_and_delta() {
    let (_directory, store, package) = fixture();
    let session = session(&store, &package);
    let volume = store.open_automatic_volume(&session.id, Role::Session).unwrap();
    let owner = Access::owner(volume.owner);
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let provision_key = |build: &str| {
        crate::store::catalog::entry_policy_keys(Some(build))
            .unwrap()
            .into_iter()
            .find(|key| key.ends_with(build.as_bytes()))
            .unwrap()
    };
    let provision_clock = |key: &[u8]| {
        let tx = index.begin_read().unwrap();
        tx.get_with_clock(key).unwrap().map(|(clock, value)| {
            assert_eq!(value.as_ref(), b"true");
            clock
        })
    };
    let original_key = provision_key(&session.resolved_build_id);
    let original_clock = provision_clock(&original_key).unwrap();
    let mut changes = store.take_change_receiver().unwrap();
    while !changes.drain().records.is_empty() {}
    let mut changed_keys = || {
        let batch = changes.drain();
        let [record] = batch.records.as_slice() else { panic!("one committed Shared Index delta") };
        assert_eq!(record.volume(), volume.id);
        assert_eq!(
            record.kind(),
            crate::store::change_feed::CommittedChangeKind::Database(DatabaseId::Index)
        );
        let mut keys = Vec::new();
        kvdb::sync::visit_delta(record.bytes(), crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
            keys.push(key.to_vec())
        })
        .unwrap();
        keys
    };
    store.set_permissions(&owner, volume.id, Acl::default()).unwrap();
    assert_eq!(changed_keys(), [crate::store::catalog::DEFAULT_PERMISSIONS_KEY.to_vec()]);
    assert_eq!(provision_clock(&original_key), Some(original_clock));

    let build = crate::identifier::encode(&[0xb8; 32]);
    let new_key = provision_key(&build);
    assert_eq!(provision_clock(&new_key), None);
    store.provision_version(&owner, volume.id, &build).unwrap();
    assert_eq!(changed_keys().as_slice(), std::slice::from_ref(&new_key));
    assert!(provision_clock(&new_key).unwrap() > original_clock);
    assert_eq!(provision_clock(&original_key), Some(original_clock));
}

#[test]
fn projected_world_reopens_without_keys_and_accepts_a_later_credentialed_invite() {
    let (_owner_dir, owner, package) = fixture();
    let room = session(&owner, &package);
    let world = owner.open_automatic_volume(&room.id, Role::Root).unwrap();
    owner.open_automatic_volume(&room.id, Role::Session).unwrap();
    let (guest_dir, guest, _) = fixture();
    let channel = crate::release_channel::ReleaseChannel::Public;
    let import = |guest: &LocalStore, session: &Session| {
        guest
            .import_invitation_link(
                &owner.invitation(&session.id, channel).unwrap(),
                &owner.session_envelope(&session.id).unwrap(),
                channel,
            )
            .unwrap();
    };
    import(&guest, &room);
    guest.prepare_world_replica(&room, &world).unwrap();
    assert!(matches!(guest.credentials(world.id), Err(Error::NotFound(_))));
    let catalog: serde_json::Value =
        serde_json::from_slice(&guest.recovery_catalog().unwrap()).unwrap();
    assert!(
        catalog["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["volume"]["id"] != serde_json::to_value(world.id).unwrap())
    );
    guest
        .save_recovery_state(&RecoveryState {
            secret: [7; 16],
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        })
        .unwrap();
    guest.seed_account_records().unwrap();
    guest.set_world_denied(&room, true).unwrap();
    let projected_key =
        guest.replica_db(world.id, DatabaseId::Index, None).unwrap().meta().hash_key;
    // The same projected profile recovers databases before and after a second invite
    // supplies the private World key. The publisher's policy permits the guest to write.
    let public = Acl {
        grants: vec![crate::store::object_contract::Grant {
            subject: crate::store::object_contract::Subject::Public,
            permissions: vec![Permission::Read, Permission::Write],
        }],
    };
    owner.set_permissions(&Access::owner(owner.account.id), world.id, public).unwrap();
    let index = guest.db(world.id, DatabaseId::Index).unwrap();
    owner.index(world.id).unwrap().write(&index, now_unix_secs()).unwrap();
    drop(index);
    let recover = |guest: &LocalStore| {
        let access = Access::owner(guest.account.id);
        let entry = guest.create_database(&access, world.id, "source", EntryKind::KvDb {}).unwrap();
        let address = EntryAddress { volume: world.id, entry: entry.id };
        guest.kv_put(&access, address, b"key", b"saved").unwrap();
        guest.remove_entry(&access, world.id, entry.id).unwrap();
        let recovered = guest.recover_entry(&access, address, "source", None).unwrap();
        let address = EntryAddress { volume: world.id, entry: recovered.id };
        assert_eq!(guest.kv_read(&access, address, b"key", 0, 64, None).unwrap().bytes, b"saved");
        let database = DatabaseId::Entry(recovered.id);
        assert_eq!(
            guest.db(world.id, database).unwrap().meta().hash_key,
            database.hash_key(&DatabaseId::world_projection_key(world.id), world.id)
        );
        guest.remove_entry(&access, world.id, recovered.id).unwrap();
    };
    recover(&guest);
    drop(guest);

    let guest = LocalStore::open(guest_dir.path()).unwrap();
    assert!(matches!(guest.check_world_admission(&room), Err(Error::AccessDenied(_))));
    guest.set_world_denied(&room, false).unwrap();
    guest.check_world_admission(&room).unwrap();
    let root_session = owner
        .create_session(&room.app, room.selector.clone(), &room.resolved_build_id, world.id)
        .unwrap();
    import(&guest, &root_session);
    #[cfg(feature = "connected-engine")]
    assert_eq!(
        guest.world_replica_route(world.id, Some(&root_session)).unwrap(),
        WorldReplicaRoute::CredentialedRoot
    );
    assert!(guest.credentials(world.id).unwrap() == owner.credentials(world.id).unwrap());
    recover(&guest);
    assert_eq!(
        guest.replica_db(world.id, DatabaseId::Index, None).unwrap().meta().hash_key,
        projected_key
    );
    assert_ne!(
        owner.replica_db(world.id, DatabaseId::Index, None).unwrap().meta().hash_key,
        projected_key
    );
}

#[test]
fn recovered_same_account_shared_never_reseeds_missing_backing() {
    // Account recovery transfers the exact capability, not the creator's local
    // initializer authority. A missing replica remains unavailable on device 2.
    let (_directory, creator, package) = fixture();
    let session = session(&creator, &package);
    let shared = creator.open_automatic_volume(&session.id, Role::Session).unwrap();
    let catalog = creator.recovery_catalog().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut recipient = LocalStore::open(directory.path()).unwrap();
    recipient.bind_authenticated_account(creator.account.id).unwrap();
    recipient.import_recovery_catalog(&catalog, || true).unwrap().unwrap();
    assert!(recipient.local_session(&session.id).unwrap().is_none());
    assert_eq!(recipient.session(&session.id).unwrap(), session);
    assert_eq!(recipient.credentials(shared.id).unwrap(), creator.credentials(shared.id).unwrap());
    assert!(recipient.pending_operations().unwrap().is_empty());
    assert!(
        recipient
            .metadata
            .begin_read()
            .unwrap()
            .get(format!("root-initialization/{}", shared.id).as_bytes())
            .unwrap()
            .is_none()
    );
    drop(recipient);
    let backing = directory
        .path()
        .join(crate::paths::VOLUMES)
        .join(shared.id.to_string())
        .join(crate::store::DatabaseId::Index.file_name());
    std::fs::remove_file(&backing).unwrap();
    let recipient = LocalStore::open(directory.path()).unwrap();
    assert!(recipient.open_automatic_volume(&session.id, Role::Session).is_err());
    assert!(!backing.exists());
    assert!(
        recipient
            .metadata
            .begin_read()
            .unwrap()
            .get(format!("root-initialization/{}", shared.id).as_bytes())
            .unwrap()
            .is_none()
    );
}

#[test]
fn shared_initialization_restarts_at_each_existing_durable_boundary() {
    for step in 1..=6 {
        let (directory, store, package) = fixture();
        let session = session(&store, &package);
        let shared = session.shared_volume.unwrap();
        super::super::initialization::INTERRUPT.set(step);
        assert!(store.open_automatic_volume(&session.id, Role::Session).is_err());
        super::super::initialization::INTERRUPT.set(0);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.session(&session.id).unwrap(), session);
        store.open_automatic_volume(&session.id, Role::Session).unwrap();
        let credentials = store.credentials(shared).unwrap();
        let pending = store.pending_operations().unwrap();
        let receipt = store
            .metadata
            .begin_read()
            .unwrap()
            .get(format!("root-initialization/{shared}").as_bytes())
            .unwrap()
            .unwrap()
            .to_vec();
        let entry = EntryAddress { volume: shared, entry: table() };
        store.kv_put(&access(&store, &session), entry, b"edit", b"keep after retry").unwrap();
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        store.open_automatic_volume(&session.id, Role::Session).unwrap();
        assert_eq!(store.credentials(shared).unwrap(), credentials);
        assert_eq!(store.pending_operations().unwrap(), pending);
        assert_eq!(pending.len(), 2);
        assert_eq!(
            store
                .metadata
                .begin_read()
                .unwrap()
                .get(format!("root-initialization/{shared}").as_bytes())
                .unwrap()
                .unwrap()
                .as_ref(),
            receipt
        );
        assert_eq!(
            store.kv_read(&access(&store, &session), entry, b"edit", 0, 64, None).unwrap().bytes,
            b"keep after retry"
        );
    }
}

#[test]
fn authorized_recipient_imports_shared_and_replicates_offline_edits_without_seeding() {
    let (_creator_directory, creator, package) = fixture();
    let session = session(&creator, &package);
    let shared = creator.open_automatic_volume(&session.id, Role::Session).unwrap();
    let entry = EntryAddress { volume: shared.id, entry: table() };
    let creator_access = access(&creator, &session);
    creator.kv_put(&creator_access, entry, b"creator", b"first edit").unwrap();
    let recipient_directory = tempfile::tempdir().unwrap();
    let recipient = LocalStore::open(recipient_directory.path()).unwrap();
    assert_ne!(creator.account.id, recipient.account.id);
    assert_eq!(
        creator.volume_details(&creator_access, shared.id).unwrap().permissions.grants,
        [crate::store::object_contract::Grant {
            subject: crate::store::object_contract::Subject::SessionParticipants,
            permissions: vec![Permission::Read, Permission::Write],
        }]
    );
    let channel = crate::release_channel::ReleaseChannel::Public;
    let link = creator.invitation(&session.id, channel).unwrap();
    let envelope = creator.session_envelope(&session.id).unwrap();
    #[cfg(feature = "connected-engine")]
    {
        recipient.preview_invitation(&link, channel).unwrap();
        let header = fs::read(recipient.path.join(profile_format::FILE)).unwrap();
        let snapshot = || {
            let tx = recipient.metadata.begin_read().unwrap();
            let records: Vec<(Vec<u8>, Vec<u8>)> = tx
                .iter()
                .take(65)
                .map(|record| {
                    let (key, value) = record.unwrap();
                    assert!(key.len() + value.len() <= 16 * 1024);
                    (key.to_vec(), value.to_vec())
                })
                .collect();
            assert!(records.len() <= 64);
            records
        };
        let metadata = snapshot();
        let mut mismatched = session.clone();
        mismatched.creator = recipient.account.id;
        assert!(
            recipient.check_invitation_registration(&session.id, &envelope, &mismatched).is_err()
        );
        assert_eq!(fs::read(recipient.path.join(profile_format::FILE)).unwrap(), header);
        assert_eq!(snapshot(), metadata);
    }
    for _ in 0..2 {
        assert_eq!(recipient.import_invitation_link(&link, &envelope, channel).unwrap(), session);
    }
    let outer = fs::read(recipient.path.join(profile_format::FILE)).unwrap();
    let inner = recipient
        .metadata
        .begin_read()
        .unwrap()
        .get(profile::format::KEY)
        .unwrap()
        .unwrap()
        .to_vec();
    for descriptor in [outer, inner] {
        let descriptor: serde_json::Value = serde_json::from_slice(&descriptor).unwrap();
        for access in ["required_read_features", "required_write_features"] {
            assert!(
                descriptor[access]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|feature| feature == profile::format::SHARED_SESSION)
            );
        }
    }
    assert!(recipient.local_session(&session.id).unwrap().is_none());
    assert!(recipient.automatic_binding(shared.id).unwrap().is_none());
    assert_eq!(recipient.credentials(shared.id).unwrap(), creator.credentials(shared.id).unwrap());
    assert!(recipient.pending_operations().unwrap().is_empty());
    assert!(
        recipient
            .metadata
            .begin_read()
            .unwrap()
            .get(format!("root-initialization/{}", shared.id).as_bytes())
            .unwrap()
            .is_none()
    );
    let stage = super::super::tests::received::receive(&creator, &recipient, shared.id);
    recipient.adopt_received_volume(&access(&recipient, &session), &stage).unwrap();
    drop(stage);
    assert_eq!(recipient.open_automatic_volume(&session.id, Role::Session).unwrap(), shared);
    let recipient_access = access(&recipient, &session);
    assert_eq!(
        recipient.kv_read(&recipient_access, entry, b"creator", 0, 64, None).unwrap().bytes,
        b"first edit"
    );
    // Both profiles edit their encrypted local backing while disconnected.
    recipient.kv_put(&recipient_access, entry, b"recipient", b"offline recipient edit").unwrap();
    creator.kv_put(&creator_access, entry, b"creator", b"offline creator edit").unwrap();
    drop(recipient);
    let recipient = LocalStore::open(recipient_directory.path()).unwrap();
    assert_eq!(recipient.session(&session.id).unwrap(), session);
    let stage = super::super::tests::received::receive(&recipient, &creator, shared.id);
    creator.adopt_received_volume(&creator_access, &stage).unwrap();
    drop(stage);
    let stage = super::super::tests::received::receive(&creator, &recipient, shared.id);
    recipient.adopt_received_volume(&recipient_access, &stage).unwrap();
    drop(stage);
    assert_eq!(
        creator.kv_read(&creator_access, entry, b"recipient", 0, 64, None).unwrap().bytes,
        b"offline recipient edit"
    );
    assert_eq!(
        recipient.kv_read(&recipient_access, entry, b"creator", 0, 64, None).unwrap().bytes,
        b"offline creator edit"
    );
    assert!(creator.volume(session.root_volume).is_err());
    assert!(recipient.volume(session.root_volume).is_err());
    creator.revoke(&creator_access, shared.id, recipient.account.id, "removed").unwrap();
    let stage = super::super::tests::received::receive(&creator, &recipient, shared.id);
    assert!(recipient.adopt_received_revocations(&recipient_access, &stage).is_err());
    drop(stage);
    assert!(matches!(recipient.kv_put(&recipient_access, entry, b"denied", b"value"),
        Err(Error::ParticipantRevoked { volume, .. }) if volume == shared.id));
    assert!(recipient.pending_operations().unwrap().is_empty());
}

#[test]
fn shared_candidate_policy_admits_updates_without_broadening_running_build_access() {
    for same_account in [false, true] {
        let (_directory, creator, package) = fixture();
        let original = session(&creator, &package);
        let shared = creator.open_automatic_volume(&original.id, Role::Session).unwrap();
        let recipient_directory = tempfile::tempdir().unwrap();
        let mut recipient = LocalStore::open(recipient_directory.path()).unwrap();
        if same_account {
            recipient.bind_authenticated_account(creator.account.id).unwrap();
            recipient
                .import_recovery_catalog(&creator.recovery_catalog().unwrap(), || true)
                .unwrap()
                .unwrap();
        } else {
            creator
                .grant_volume(
                    &Access::owner(creator.account.id),
                    shared.id,
                    recipient.account.id,
                    true,
                )
                .unwrap();
            let channel = crate::release_channel::ReleaseChannel::Public;
            recipient
                .import_invitation_link(
                    &creator.invitation(&original.id, channel).unwrap(),
                    &creator.session_envelope(&original.id).unwrap(),
                    channel,
                )
                .unwrap();
        }
        let stage = super::super::tests::received::receive(&creator, &recipient, shared.id);
        recipient.adopt_received_volume(&access(&recipient, &original), &stage).unwrap();
        drop(stage);
        let build = crate::identifier::encode(&[0xb7; 32]);
        let mut candidate = original.clone();
        candidate.selector = BuildSelector::Exact { build_id: build.clone() };
        candidate.resolved_build_id = build.clone();
        if !same_account {
            assert!(recipient.authorize_session_candidate(&original, &candidate).is_err());
            assert!(recipient.adopt_session(&original, &candidate).is_err());
        }
        creator.provision_version(&Access::owner(creator.account.id), shared.id, &build).unwrap();
        let stage = super::super::tests::received::receive(&creator, &recipient, shared.id);
        recipient.adopt_received_volume(&access(&recipient, &original), &stage).unwrap();
        drop(stage);
        for store in [&creator, &recipient] {
            let next = access(store, &candidate);
            // Even a provisioned future build cannot operate before adoption.
            assert!(store.authorize_replication(&next, shared.id, Permission::Read).is_err());
            store.authorize_session_candidate(&original, &candidate).unwrap();
            store.adopt_session(&original, &candidate).unwrap();
            store.authorize_replication(&next, shared.id, Permission::Read).unwrap();
            assert!(
                store
                    .authorize_replication(&access(store, &original), shared.id, Permission::Read)
                    .is_err()
            );
            assert_eq!(store.session(&original.id).unwrap(), candidate);
            assert!(store.volume(original.root_volume).is_err());
        }
    }
}

#[test]
fn existing_local_binding_and_data_are_not_promoted_on_creation_retry() {
    // Retain the old record shape (no shared_volume field) and real old Shared
    // data. A new writer must keep that exact binding local and retain its bytes.
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let seed = [0x72; SEED_BYTES];
    let id = SessionKeys::derive(&seed).unwrap().session_id();
    let root = app.volume_id(Role::Root, None, None, None).unwrap();
    let old = Session {
        app: app.clone(),
        id: id.clone(),
        selector: BuildSelector::Exact { build_id: build.clone() },
        resolved_build_id: build.clone(),
        root_volume: root,
        shared_volume: None,
        creator: store.account.id,
        update_authority: UpdateAuthority::Creator,
    };
    let retained = LocalSession {
        session: old.clone(),
        seed,
        creation_build: build.clone(),
        user_volume: None,
    };
    {
        let _profile = store.lock_profile().unwrap();
        profile_format::enable_local_app_scopes(&store).unwrap();
        put(&store.metadata, format!("local-session/{id}").as_bytes(), &retained).unwrap();
    }
    let shared = store.open_automatic_volume(&id, Role::Session).unwrap();
    let entry = EntryAddress { volume: shared.id, entry: table() };
    store.kv_put(&access(&store, &old), entry, b"kept", b"old local value").unwrap();
    let request = SessionCreation {
        app,
        selector: old.selector.clone(),
        resolved_build_id: build,
        root_volume: root,
        sync_shared: true,
        user_volume: None,
    };
    assert_eq!(store.bind_automatic_session(request, Some(seed), None).unwrap(), old);
    assert!(store.require_replication_supported(shared.id).is_err());
    assert!(store.pending_operations().unwrap().is_empty());
    assert_eq!(
        store.kv_read(&access(&store, &old), entry, b"kept", 0, 64, None).unwrap().bytes,
        b"old local value"
    );
}

#[test]
fn logical_slots_allocate_nothing_and_session_codes_round_trip() {
    // Two sessions share app scopes but have independent durable session scopes.
    let (_directory, store, package) = fixture();
    let first = session(&store, &package);
    let second = session(&store, &package);
    assert_eq!(store.session_app_names().unwrap()[&first.id], package.name());
    let seed = store.session_seed(&first.id).unwrap();
    let mut retry = SessionCreation {
        app: first.app.clone(),
        selector: first.selector.clone(),
        resolved_build_id: first.resolved_build_id.clone(),
        root_volume: first.root_volume,
        sync_shared: true,
        user_volume: None,
    };
    assert_eq!(store.bind_automatic_session(retry.clone(), Some(seed), None).unwrap(), first);
    retry.resolved_build_id = crate::identifier::encode(&[0x55; 32]);
    retry.selector = BuildSelector::Exact { build_id: retry.resolved_build_id.clone() };
    assert!(store.bind_automatic_session(retry, Some(seed), None).is_err());
    assert_eq!(store.session(&first.id).unwrap(), first);
    let a = store.automatic_volumes(&first.id).unwrap();
    let b = store.automatic_volumes(&second.id).unwrap();
    assert_eq!(a.len(), 6);
    for (a, b) in a.iter().zip(&b) {
        assert_eq!(a.state, AutomaticVolumeState::Lazy);
        assert_eq!(a.volume == b.volume, !a.role.is_session());
    }
    assert!(store.volumes().unwrap().is_empty());
    assert!(store.pending_operations().unwrap().is_empty());
    let reference = store.session_reference(&first.id).unwrap();
    assert_eq!(
        crate::store::parse_session_code(&reference.code()).unwrap(),
        store.session_seed(&first.id).unwrap()
    );
    assert_eq!(
        crate::store::session_id(
            &crate::store::parse_session_link(
                &reference.link(crate::release_channel::ReleaseChannel::Public),
                crate::release_channel::ReleaseChannel::Public
            )
            .unwrap()
        )
        .unwrap(),
        first.id
    );
    std::fs::remove_file(store.path.join(crate::paths::BUILDS).join(package.id().unwrap()))
        .unwrap();
    assert_eq!(store.session_app_names().unwrap()[&first.id], package.name());
}

#[test]
fn app_scope_readiness_preserves_ordinary_roots_and_materialized_data() {
    // Model a session-only catalog, then import and remove its local initializer;
    // listing must preserve ordinary bindings and already materialized scope data.
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let build = package.id().unwrap();
    let owner = Access::owner(store.account.id);
    let root = store
        .initialize_root(
            Id::generate().unwrap(),
            "Ordinary",
            false,
            &package.initializer(Role::User).unwrap(),
        )
        .unwrap();
    let root_entry = EntryAddress { volume: root.id, entry: table() };
    store.kv_put(&owner, root_entry, b"a", b"ordinary retained").unwrap();
    let session = store
        .create_session(app, BuildSelector::Exact { build_id: build.clone() }, &build, root.id)
        .unwrap();
    let key = format!("imported-app/{}/{}", app.publisher, app.app_id);
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    assert!(tx.del(key.as_bytes()).unwrap());
    tx.commit().unwrap();

    assert!(store.imported_apps().unwrap().is_empty());
    assert_eq!(store.retained_sessions().unwrap().as_slice(), std::slice::from_ref(&session));
    let unavailable = store.app_volumes(app).unwrap();
    assert_eq!(unavailable.len(), 3);
    assert!(unavailable.iter().all(|slot| slot.state == AutomaticVolumeState::Unavailable));
    let root_slot = store.automatic_volumes(&session.id).unwrap().remove(0);
    assert_eq!(root_slot.role, Role::Root);
    assert_eq!(root_slot.volume, root.id);
    assert!(root_slot.overridden);
    assert_eq!(root_slot.state, AutomaticVolumeState::Ready);
    assert_eq!(store.volumes().unwrap().as_slice(), std::slice::from_ref(&root));

    store.remember_imported_app(&package).unwrap();
    let lazy = store.app_volumes(app).unwrap();
    assert_eq!(lazy.len(), unavailable.len());
    assert!(lazy.iter().all(|slot| slot.state == AutomaticVolumeState::Lazy));
    assert_eq!(store.volumes().unwrap().as_slice(), std::slice::from_ref(&root));
    let world = store.open_app_volume(app, Role::Root).unwrap();
    let user = store.open_app_volume(app, Role::User).unwrap();
    let user_entry = EntryAddress { volume: user.id, entry: table() };
    store.kv_put(&owner, user_entry, b"a", b"user retained").unwrap();
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    assert!(tx.del(key.as_bytes()).unwrap());
    tx.commit().unwrap();

    let materialized = store.app_volumes(app).unwrap();
    assert_eq!(materialized.len(), unavailable.len());
    for ((before, imported), after) in unavailable.iter().zip(&lazy).zip(&materialized) {
        assert_eq!(
            (before.role, before.volume, before.overridden),
            (imported.role, imported.volume, imported.overridden)
        );
        assert_eq!(
            (before.role, before.volume, before.overridden),
            (after.role, after.volume, after.overridden)
        );
        assert_eq!(
            after.state,
            if after.role == Role::Local {
                AutomaticVolumeState::Unavailable
            } else {
                AutomaticVolumeState::Ready
            }
        );
    }
    assert_eq!(store.volume(world.id).unwrap(), world);
    assert_eq!(store.volume(user.id).unwrap(), user);
    assert_eq!(store.session(&session.id).unwrap(), session);
    assert_eq!(
        store.kv_read(&owner, root_entry, b"a", 0, 64, None).unwrap().bytes,
        b"ordinary retained"
    );
    assert_eq!(
        store.kv_read(&owner, user_entry, b"a", 0, 64, None).unwrap().bytes,
        b"user retained"
    );
}

#[test]
fn private_user_edits_share_app_scope_and_survive_cold_reopen() {
    use crate::network::ap::publisher::{KeyStatus, KeyStatusClaims};
    use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};

    // Import another publisher's app, then open User before World and retain edits.
    let (_publisher_directory, publisher, package) = fixture();
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    assert_ne!(store.account.id, publisher.account.id);
    let issuer = SigningKey::from_bytes(&[7; 32]);
    let now = now_unix_secs();
    let claims = &package.publisher_certificate().unwrap().claims;
    let status = KeyStatus::issue(
        KeyStatusClaims {
            issuer: issuer.verifying_key().to_bytes(),
            publisher: claims.publisher,
            signer: claims.signer,
            epoch: 1,
            checked_at: now,
            revoked: false,
        },
        issuer.to_pkcs8_der().unwrap().as_bytes(),
    )
    .unwrap();
    store
        .cache_publisher_status(issuer.verifying_key().to_bytes(), &status.to_bytes(), now)
        .unwrap();
    let build = package.id().unwrap();
    let builds = store.path.join(crate::paths::BUILDS);
    fs::create_dir_all(&builds).unwrap();
    fs::copy(publisher.path.join(crate::paths::BUILDS).join(&build), builds.join(&build)).unwrap();
    let package = store
        .authenticate_package(
            Package::from_bytes(&fs::read(builds.join(&build)).unwrap()).unwrap(),
            now,
        )
        .unwrap();
    store.remember_imported_app(&package).unwrap();
    assert_eq!(package.identity().unwrap().publisher, publisher.account.id);
    let first = session(&store, &package);
    let second = session(&store, &package);
    let user = store.open_automatic_volume(&first.id, Role::User).unwrap();
    assert_eq!(user.owner, store.account.id);
    assert_ne!(user.owner, publisher.account.id);
    assert_eq!(store.volumes().unwrap().len(), 1);
    let address = EntryAddress { volume: user.id, entry: table() };
    store.kv_put(&access(&store, &first), address, b"a", b"retained").unwrap();
    assert_eq!(
        store.kv_read(&access(&store, &second), address, b"a", 0, 64, None).unwrap().bytes,
        b"retained"
    );
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    let reopened = store.open_automatic_volume(&first.id, Role::User).unwrap();
    assert_eq!(reopened.id, user.id);
    assert_eq!(reopened.owner, store.account.id);
    assert_ne!(reopened.owner, publisher.account.id);
    assert_eq!(
        store.kv_read(&access(&store, &first), address, b"a", 0, 64, None).unwrap().bytes,
        b"retained"
    );
    let shared_a = store.open_automatic_volume(&first.id, Role::Session).unwrap();
    let shared_b = store.open_automatic_volume(&second.id, Role::Session).unwrap();
    assert_eq!(shared_a.owner, store.account.id);
    assert!(shared_a.share_key_with_service);
    assert_eq!(shared_a.id, first.shared_volume.unwrap());
    let invitation = store.stored_session(&first.id).unwrap().invitation;
    invitation.verify().unwrap();
    assert_eq!(invitation.session.creator, store.account.id);
    assert_eq!(invitation.session.app.publisher, publisher.account.id);
    assert_ne!(invitation.session.creator, invitation.session.app.publisher);
    assert!(
        matches!(&invitation.session.selector, BuildSelector::Exact { build_id } if build_id == &build)
    );
    assert_eq!(invitation.replica_volume().unwrap(), &shared_a);
    store
        .kv_put(
            &access(&store, &first),
            EntryAddress { volume: shared_a.id, entry: table() },
            b"a",
            b"private session",
        )
        .unwrap();
    assert!(
        store
            .kv_keys(
                &access(&store, &second),
                EntryAddress { volume: shared_b.id, entry: table() },
                None,
                None,
                32
            )
            .unwrap()
            .keys
            .is_empty()
    );
}

#[test]
fn deferred_scopes_stay_local_while_supported_shared_recovers() {
    // Compare a new automatic scope with an ordinary volume in one real profile.
    let (_directory, store, package) = fixture();
    let session = session(&store, &package);
    let user = store.open_automatic_volume(&session.id, Role::User).unwrap();
    let ordinary = store.create_volume("Ordinary", true).unwrap();
    assert!(
        store.authorize_replication(&access(&store, &session), user.id, Permission::Read).is_err()
    );
    assert!(store.service_provisioning(user.id).is_err());
    assert!(store.replication_database_page(user.id, None).is_err());
    assert!(
        store
            .create_session(
                &session.app,
                session.selector.clone(),
                &session.resolved_build_id,
                user.id
            )
            .is_err()
    );
    assert!(store.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).is_ok());
    assert!(store.service_provisioning(ordinary.id).is_ok());
    let catalog: serde_json::Value =
        serde_json::from_slice(&store.recovery_catalog().unwrap()).unwrap();
    assert_eq!(catalog["volumes"].as_array().unwrap().len(), 2);
    assert!(
        catalog["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["volume"]["id"] == ordinary.id.to_string())
    );
    assert_eq!(catalog["invitations"].as_array().unwrap().len(), 1);
    assert!(store.pending_operations().unwrap().iter().all(
        |p| !matches!(&p.operation, PendingKind::RegisterVolume { volume } if volume.id == user.id)
    ));

    #[cfg(feature = "app-host")]
    {
        let declaration = package.initializer(Role::User).unwrap();
        let explicit_root = store
            .initialize_root(Id::generate().unwrap(), &user.name, false, &declaration)
            .unwrap();
        assert!(!explicit_root.personal);
        let mut expected = vec![user.id, session.shared_volume.unwrap()];
        expected.sort_unstable();
        assert_eq!(store.automatic_volume_ids(&store.volumes().unwrap()).unwrap(), expected);
    }

    store.select_personal_volumes(&session.id, Vec::new()).unwrap();
    store
        .save_recovery_state(&RecoveryState {
            secret: [7; 16],
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        })
        .unwrap();
    store.delete_volume(user.id).unwrap();
    assert!(store.pending_account_record(None).unwrap().is_none());
    let cipher = crate::store::account::Cipher::new(store.account.id, &[7; 16]).unwrap();
    let decode = |record: &super::super::PendingAccountRecord| {
        crate::store::account::Record::decode(
            &cipher.open(&record.key, record.clock, &record.ciphertext).unwrap(),
        )
        .unwrap()
    };
    let shared = session.shared_volume.unwrap();
    store.delete_volume(shared).unwrap();
    let deletion = store.pending_account_record(None).unwrap().unwrap();
    assert!(matches!(decode(&deletion),
        crate::store::account::Record::VolumeDeleted { volume, .. } if volume == shared));
    assert_eq!(store.account_record_dependencies(&deletion).unwrap(), [None, None]);
    // Seeding a deleted creator scope must retain its tombstone and the saved
    // empty consent without calling the now-unavailable session accessor.
    store.seed_account_records().unwrap();
    let mut retained = [false; 3];
    while let Some(record) = store.pending_account_record(None).unwrap() {
        match decode(&record) {
            crate::store::account::Record::Volume { volume, .. } => {
                assert_ne!(volume.id, user.id);
                assert_ne!(Some(volume.id), session.shared_volume);
            }
            crate::store::account::Record::HostedInvitation { invitation, scope, .. } => {
                assert_eq!(invitation.session, session);
                assert!(matches!(scope, crate::store::AppScope::Session { id, .. }
                    if id == session.id));
                retained[0] = true;
            }
            crate::store::account::Record::VolumeDeleted { volume, .. } => {
                assert_eq!(volume, shared);
                retained[1] = true;
            }
            crate::store::account::Record::PersonalVolumes { session: id, grants } => {
                assert_eq!(id, session.id);
                assert!(grants.is_empty());
                retained[2] = true;
            }
            _ => panic!("this fixture has no other account records"),
        }
        assert!(store.acknowledge_account_record(&record).unwrap());
    }
    assert_eq!(retained, [true; 3]);
}

#[test]
fn world_publication_preserves_private_scopes_and_recovers_its_original_binding() {
    // A package without Session keeps World hosting and its empty local Shared
    // scope, including after publication retires the creator's local record.
    let (directory, store, package) = fixture();
    let initializers = VolumeInitializers::new(
        [Role::User, Role::UserSession, Role::Local, Role::LocalSession]
            .into_iter()
            .map(|role| (role, package.initializer(role).unwrap()))
            .collect(),
    )
    .unwrap();
    let package = Package::create(
        package.app_id().into(),
        package.name().into(),
        package.wasm().unwrap(),
        BTreeMap::new(),
        &initializers,
    )
    .unwrap();
    let package = super::super::publisher::install_test_package(&store, package).unwrap();
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let first = store
        .create_session_binding(SessionCreation {
            root_volume: app.volume_id(Role::Root, None, None, None).unwrap(),
            app,
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            sync_shared: false,
            user_volume: None,
        })
        .unwrap();
    let seed = store.session_seed(&first.id).unwrap();
    let secret = [0x42; 16];
    store
        .save_recovery_state(&recovery_catalog::RecoveryState {
            secret,
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        })
        .unwrap();
    let private = store.open_automatic_volume(&first.id, Role::User).unwrap();
    let address = EntryAddress { volume: private.id, entry: table() };
    store.kv_put(&access(&store, &first), address, b"draft", b"private").unwrap();

    let world = store.open_automatic_volume(&first.id, Role::Root).unwrap();
    assert!(world.share_key_with_service);
    assert!(store.replication_supported(world.id).unwrap());
    assert!(!store.replication_supported(private.id).unwrap());
    assert!(!store.is_local_session(&first.id).unwrap());
    assert!(store.has_automatic_scopes(&first.id).unwrap());
    assert_eq!(store.session_seed(&first.id).unwrap(), seed);
    assert_eq!(store.session(&first.id).unwrap(), first);
    assert_eq!(store.automatic_volumes(&first.id).unwrap().len(), 6);
    assert_eq!(
        store
            .automatic_volumes(&first.id)
            .unwrap()
            .into_iter()
            .find(|slot| slot.role == Role::Session)
            .unwrap()
            .state,
        AutomaticVolumeState::Lazy
    );
    assert_eq!(
        store.kv_read(&access(&store, &first), address, b"draft", 0, 64, None).unwrap().bytes,
        b"private"
    );
    let pending = store.pending_operations().unwrap();
    assert!(pending.iter().any(|pending| matches!(
        &pending.operation, PendingKind::RegisterSession { session, .. } if session == &first
    )));
    assert_eq!(store.open_automatic_volume(&first.id, Role::Root).unwrap(), world);
    let local_shared = store.open_automatic_volume(&first.id, Role::Session).unwrap();
    assert!(!local_shared.share_key_with_service);
    assert!(!store.replication_supported(local_shared.id).unwrap());
    assert!(
        store
            .entries(&access(&store, &first), local_shared.id, None, 64)
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(store.pending_operations().unwrap(), pending);

    // An ordinary invitation carries World, not the creator's private Shared.
    // The recipient snapshot must not offer initialization that opening denies.
    let recipient_directory = tempfile::tempdir().unwrap();
    let recipient = LocalStore::open(recipient_directory.path()).unwrap();
    store
        .grant_volume(&Access::owner(store.account.id), world.id, recipient.account.id, false)
        .unwrap();
    let channel = crate::release_channel::ReleaseChannel::Public;
    recipient
        .import_invitation_link(
            &store.invitation(&first.id, channel).unwrap(),
            &store.session_envelope(&first.id).unwrap(),
            channel,
        )
        .unwrap();
    let slots = recipient.automatic_volumes(&first.id).unwrap();
    let shared = slots.iter().find(|slot| slot.role == Role::Session).unwrap();
    assert_eq!(shared.volume, local_shared.id);
    assert_eq!(shared.state, AutomaticVolumeState::Unavailable);
    assert!(
        slots
            .iter()
            .filter(|slot| matches!(
                slot.role,
                Role::User | Role::UserSession | Role::Local | Role::LocalSession
            ))
            .all(|slot| slot.state == AutomaticVolumeState::Lazy)
    );
    assert!(matches!(
        recipient.open_automatic_volume(&first.id, Role::Session),
        Err(Error::AccessDenied(_))
    ));
    assert!(recipient.volume(local_shared.id).is_err());
    assert!(recipient.pending_operations().unwrap().is_empty());
    assert_eq!(
        store
            .automatic_volumes(&first.id)
            .unwrap()
            .into_iter()
            .find(|slot| slot.role == Role::Session)
            .unwrap()
            .state,
        AutomaticVolumeState::Ready
    );

    let catalog = store.recovery_catalog().unwrap();
    let cipher = crate::store::account::Cipher::new(store.account.id, &secret).unwrap();
    let replica_directory = tempfile::tempdir().unwrap();
    let mut replica = LocalStore::open(replica_directory.path()).unwrap();
    replica.bind_authenticated_account(store.account.id).unwrap();
    replica.save_recovery_state(&store.recovery_state().unwrap().unwrap()).unwrap();
    let mut after = None;
    let mut scopes = Vec::new();
    while let Some(record) = store.pending_account_record(after).unwrap() {
        let bytes = cipher.open(&record.key, record.clock, &record.ciphertext).unwrap();
        if let crate::store::account::Record::HostedVolume { scope, .. }
        | crate::store::account::Record::HostedInvitation { scope, .. } =
            crate::store::account::Record::decode(&bytes).unwrap()
        {
            scopes.push(scope);
        }
        let delta = store.stage_account_record(&record).unwrap().unwrap();
        replica.receive_account_delta(&delta).unwrap();
        if replica.session(&first.id).is_ok() {
            assert!(replica.has_automatic_scopes(&first.id).unwrap());
        }
        after = Some(record.key);
    }
    assert_eq!(scopes.len(), 2);
    assert_eq!(replica.hosted_app_scopes().unwrap(), store.hosted_app_scopes().unwrap());
    assert!(replica.volume(private.id).is_err());
    assert!(replica.volume(local_shared.id).is_err());
    let recovered_directory = tempfile::tempdir().unwrap();
    let mut recovered = LocalStore::open(recovered_directory.path()).unwrap();
    recovered.bind_authenticated_account(store.account.id).unwrap();
    recovered.import_recovery_catalog(&catalog, || true).unwrap();
    assert_eq!(recovered.session(&first.id).unwrap(), first);
    assert_eq!(recovered.session_seed(&first.id).unwrap(), seed);
    assert!(recovered.has_automatic_scopes(&first.id).unwrap());
    assert_eq!(
        recovered.automatic_binding(world.id).unwrap(),
        store.automatic_binding(world.id).unwrap()
    );
    assert_eq!(recovered.hosted_app_scopes().unwrap(), store.hosted_app_scopes().unwrap());
    assert!(recovered.volume(private.id).is_err());
    assert!(recovered.volume(local_shared.id).is_err());

    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    assert_eq!(reopened.open_automatic_volume(&first.id, Role::Root).unwrap(), world);
    assert_eq!(reopened.open_automatic_volume(&first.id, Role::Session).unwrap(), local_shared);
    assert_eq!(reopened.session_seed(&first.id).unwrap(), seed);
    assert_eq!(
        reopened.kv_read(&access(&reopened, &first), address, b"draft", 0, 64, None).unwrap().bytes,
        b"private"
    );
    reopened.delete_volume(world.id).unwrap();
    reopened.reconcile_account_records().unwrap();
    reopened.seed_account_records().unwrap();
    let catalog: serde_json::Value =
        serde_json::from_slice(&reopened.recovery_catalog().unwrap()).unwrap();
    assert!(catalog.get("scopes").is_none());
}

#[test]
fn explicit_world_selection_keeps_root_backing_and_shared_rejects_another_root() {
    let (_directory, store, package) = fixture();
    let default = session(&store, &package);
    let world = store.open_automatic_volume(&default.id, Role::Root).unwrap();
    let request = SessionCreation {
        app: default.app.clone(),
        selector: default.selector.clone(),
        resolved_build_id: default.resolved_build_id.clone(),
        root_volume: world.id,
        sync_shared: false,
        user_volume: None,
    };
    let explicit = store.create_session_binding(request.clone()).unwrap();
    assert_eq!(explicit.root_volume, world.id);
    assert_eq!(explicit.shared_volume, None);
    assert!(default.shared_volume.is_some());
    let other = store.create_volume("Explicit root", false).unwrap();
    let mut invalid = request;
    invalid.root_volume = other.id;
    invalid.sync_shared = true;
    assert!(store.create_session_binding(invalid).is_err());
}

#[test]
fn ordinary_address_collision_and_invalid_user_override_do_not_initialize_scopes() {
    // Existing ordinary identity must never become an implicit app capability.
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap().clone();
    let user = store.automatic_volume_id(&app, None, Role::User).unwrap();
    let declaration = package.initializer(Role::User).unwrap();
    let ordinary = store.initialize_root(user, "Existing", false, &declaration).unwrap();
    assert!(store.open_app_volume(&app, Role::User).is_err());
    assert_eq!(store.volume(user).unwrap(), ordinary);
    let build = package.id().unwrap();
    let request = SessionCreation {
        app: app.clone(),
        selector: BuildSelector::Exact { build_id: build.clone() },
        resolved_build_id: build,
        root_volume: app.volume_id(Role::Root, None, None, None).unwrap(),
        sync_shared: true,
        user_volume: Some(Id::from_bytes([3; 16])),
    };
    assert!(store.create_session_binding(request).is_err());
    assert!(store.sessions().unwrap().is_empty());
}

#[test]
fn full_import_catalog_rejects_new_identity_without_poisoning_existing_apps() {
    // Fill the real signed import path, then verify rejection preserves listing.
    let (_directory, store, package) = fixture();
    for index in 1..256 {
        let candidate = Package::new(
            format!("catalog-{index}"),
            "Catalog".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::new(),
        )
        .unwrap();
        super::super::publisher::install_test_package(&store, candidate).unwrap();
    }
    let extra = Package::new(
        "catalog-extra".into(),
        "Extra".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::new(),
    )
    .unwrap();
    let extra = crate::package::signature::testing::signed(&store, extra).unwrap();
    assert!(matches!(store.remember_imported_app(&extra), Err(Error::LimitExceeded)));
    assert_eq!(store.imported_apps().unwrap().len(), 256);
    store.remember_imported_app(&package).unwrap();
    assert_eq!(store.imported_apps().unwrap().len(), 256);
    assert_eq!(store.app_volumes(package.identity().unwrap()).unwrap().len(), 3);
}

#[test]
fn app_removal_rechecks_new_data_and_preserves_unchecked_and_external_volumes() {
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let session = session(&store, &package);
    assert_eq!(store.app_removal_impact(app).unwrap(), AppRemovalImpact::default());
    let user = store.open_automatic_volume(&session.id, Role::UserSession).unwrap();
    let local = store.open_automatic_volume(&session.id, Role::Local).unwrap();
    let access = Access::owner(store.account.id);
    let private = store.create_blob(&access, local.id, "keep", b"saved locally").unwrap();
    let expected = AppRemovalImpact { local_data: true, user_volumes: true };
    assert_eq!(
        store.remove_app(app, false, false).unwrap(),
        AppRemovalResult::Confirmation(expected)
    );
    assert!(!store.app_removed(app).unwrap());
    assert!(store.remove_app(app, false, true).is_err());
    store.remove_app(app, true, false).unwrap();
    store.remove_app(app, true, true).unwrap();
    #[cfg(feature = "app-host")]
    {
        let channel = crate::release_channel::ReleaseChannel::Public;
        let link = store.session_reference(&session.id).unwrap().link(channel);
        assert!(store.preview_desktop_invitation(&link, channel).unwrap().retained);
        assert!(store.app_removed(app).unwrap());
        store.retain_session(&session.id).unwrap();
        assert!(!store.app_removed(app).unwrap());
        assert!(store.transient_desktop_session().unwrap().is_none());
        assert!(store.pending_invitations().unwrap().is_empty());
    }
    store.remember_imported_app(&package).unwrap();
    assert_eq!(store.read_blob_entry(&access, local.id, private.id).unwrap(), b"saved locally");

    // An ordinary explicit override, even one pointing at an automatic Local
    // address, retains that volume's separate use and excludes it from erasure.
    let build = package.id().unwrap();
    store
        .create_session_binding(SessionCreation {
            app: app.clone(),
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            root_volume: session.root_volume,
            user_volume: Some(local.id),
            sync_shared: false,
        })
        .unwrap();
    let protected = store.app_removal_impact(app).unwrap();
    assert!(protected.user_volumes);
    assert!(!protected.local_data);
    store.remove_app(app, true, true).unwrap();
    assert_eq!(store.volume(user.id).unwrap(), user);
    assert_eq!(store.read_blob_entry(&access, local.id, private.id).unwrap(), b"saved locally");
}

#[test]
fn app_removal_resumes_private_erasure_and_preserves_other_blob_references() {
    let (directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let session = session(&store, &package);
    let owner = Access::owner(store.account.id);
    let local = store.open_automatic_volume(&session.id, Role::LocalSession).unwrap();
    let user = store.open_automatic_volume(&session.id, Role::User).unwrap();
    use crate::store::browser_drafts::{
        BrowserDraft, BrowserDraftBody, BrowserDraftOperation, BrowserDraftResult, DraftEncoding,
        KvDraft,
    };
    let draft = |volume| BrowserDraft {
        id: Id::generate().unwrap(),
        revision: 1,
        volume,
        entry: Some(table()),
        ancestors: vec![],
        body: BrowserDraftBody::Kv(KvDraft {
            creating: true,
            original_key: None,
            key_text: "key".into(),
            value_text: "unpublished edit".into(),
            key_encoding: DraftEncoding::Text,
            value_encoding: DraftEncoding::Text,
            base: None,
            changed_during_edit: false,
            uncertain: false,
            read_unavailable: false,
            complete_put: false,
        }),
    };
    let discarded = draft(local.id);
    let kept = draft(user.id);
    let save = |store: &LocalStore, draft| {
        store.browser_drafts(
            store.account.id,
            BrowserDraftOperation::Upsert { expected_revision: None, draft: Box::new(draft) },
        )
    };
    save(&store, discarded.clone()).unwrap();
    save(&store, kept.clone()).unwrap();
    let other = store.create_blob(&owner, user.id, "same", b"shared bytes").unwrap();
    store.create_blob(&owner, local.id, "same", b"shared bytes").unwrap();
    store.create_blob(&owner, local.id, "only", b"local bytes").unwrap();
    store.forget_session(&session.id).unwrap();
    assert!(store.app_removal_impact(app).unwrap().local_data, "includes hidden sessions");
    // The owner of an open backing must release it before files can be erased.
    let held = store.db(local.id, DatabaseId::Index).unwrap();
    assert!(matches!(store.remove_app(app, true, true).unwrap(),
        AppRemovalResult::Removed { erased_volumes, cleanup_error: Some(error) }
        if erased_volumes == [local.id] && error.contains("cleanup will resume")));
    assert!(store.app_removed(app).unwrap());
    assert!(
        !store.has_pending_blob_uploads().unwrap(),
        "deferred Local cleanup permits idle shutdown"
    );
    drop(held);
    drop(store);
    let mut services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    services.await_app_data_recovery();
    let store = services.store.clone();
    assert!(store.app_removed(app).unwrap());
    assert!(store.volume(local.id).is_err());
    assert!(!store.path.join(crate::paths::VOLUMES).join(local.id.to_string()).exists());
    assert!(!store.blob_path(blake3::hash(b"local bytes").as_bytes(), false).exists());
    assert_eq!(store.read_blob_entry(&owner, user.id, other.id).unwrap(), b"shared bytes");
    let BrowserDraftResult::List(drafts) =
        store.browser_drafts(store.account.id, BrowserDraftOperation::List).unwrap()
    else {
        panic!("expected summaries")
    };
    assert_eq!(drafts, vec![kept.summary()]);
    assert!(save(&store, discarded).is_err(), "late insert cannot restore a discarded draft");
    assert!(
        store.open_automatic_volume(&session.id, Role::LocalSession).is_err(),
        "a queued open cannot reinitialize erased local data before re-add"
    );
    store.remember_imported_app(&package).unwrap();
    let fresh = store.open_automatic_volume(&session.id, Role::LocalSession).unwrap();
    assert_eq!(fresh.id, local.id);
    assert!(store.resolve_entry(&owner, fresh.id, "only").is_err());
}

#[test]
fn app_removal_can_finish_during_an_independent_blob_import() {
    struct Input<F>(Option<F>, &'static [u8]);
    impl<F: FnOnce()> std::io::Read for Input<F> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if let Some(during_import) = self.0.take() {
                during_import();
            }
            self.1.read(bytes)
        }
    }
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let local = store.open_app_volume(app, Role::Local).unwrap().id;
    let retained = store.create_volume("Retained", true).unwrap().id;
    let owner = Access::owner(store.account.id);
    let body = b"shared content";
    store.create_blob(&owner, local, "only pin", body).unwrap();
    let remove = || {
        store.remove_app(app, true, true).unwrap();
        assert!(!store.blob_path(blake3::hash(body).as_bytes(), false).exists());
    };
    let copy = store
        .create_blob_from_reader(&owner, retained, "copy", &mut Input(Some(remove), body))
        .unwrap();
    assert_eq!(store.read_blob_entry(&owner, retained, copy.id).unwrap(), body);
    store.remember_imported_app(&package).unwrap();
    store.open_app_volume(app, Role::Local).unwrap();
    let erase_target = || {
        store.remove_app(app, true, true).unwrap();
    };
    let late = b"late private content";
    assert!(
        store
            .create_blob_from_reader(&owner, local, "late", &mut Input(Some(erase_target), late))
            .is_err()
    );
    assert!(!store.blob_path(blake3::hash(late).as_bytes(), false).exists());
}

#[test]
fn app_removal_retries_acknowledged_versions_that_share_one_durability_receipt() {
    let (directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let volume = store.open_app_volume(app, Role::Local).unwrap().id;
    let owner = Access::owner(store.account.id);
    let make_version = |name| {
        let entry = store.create_blob(&owner, volume, name, b"one body, two pins").unwrap();
        store.blob_version(&owner, EntryAddress { volume, entry: entry.id }, None).unwrap()
    };
    let first = make_version("first");
    let second = make_version("second");
    assert_eq!(first.encrypted_key, second.encrypted_key);
    for version in [&first, &second] {
        store.acknowledge_blob_version(&owner, version).unwrap();
    }
    let receipts = format!("entry-durable/{volume}/");
    let pins = format!("entry-content/{}/", hex(&first.digest));
    let count_pins = |store: &LocalStore| {
        scan::<crate::store::volume_index::BlobVersion>(&store.metadata, &pins).unwrap().len()
    };
    assert_eq!(scan::<bool>(&store.metadata, &receipts).unwrap().len(), 1);
    assert_eq!(count_pins(&store), 2);

    // The first pin can retire, but the open reader prevents unlinking the
    // last copy. Reopening must find the remaining acknowledged pin with an
    // empty upload queue and finish reclaiming the recoverable plaintext.
    let lease = store.lease_blob_version(&owner, &first).unwrap();
    assert!(matches!(store.remove_app(app, true, true).unwrap(),
        AppRemovalResult::Removed { erased_volumes, cleanup_error: Some(_) }
        if erased_volumes == [volume]));
    assert_eq!(count_pins(&store), 1);
    assert_eq!(scan::<bool>(&store.metadata, &receipts).unwrap().len(), 1);
    assert!(store.blob_path(&first.digest, true).exists());
    drop(lease);
    drop(store);
    let mut services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    services.await_app_data_recovery();
    let reopened = &services.store;
    assert_eq!(count_pins(reopened), 0);
    assert!(scan::<bool>(&reopened.metadata, &receipts).unwrap().is_empty());
    assert!(!reopened.blob_path(&first.digest, true).exists());
    assert!(!reopened.blob_path(&first.digest, false).exists());
}

#[test]
fn app_removal_stuck_on_one_volume_allows_other_apps_and_cleanup_after_restart() {
    let (directory, store, first) = fixture();
    let other = Package::new(
        "another-app".into(),
        "Another app".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::new(),
    )
    .unwrap();
    let second = super::super::publisher::install_test_package(&store, other).unwrap();
    let mut targets = [first, second].map(|package| {
        let app = package.identity().unwrap();
        let id = store.open_app_volume(app, Role::Local).unwrap().id;
        let _held = store.db(id, DatabaseId::Index).unwrap();
        assert!(matches!(
            store.remove_app(app, true, true).unwrap(),
            AppRemovalResult::Removed { cleanup_error: Some(_), .. }
        ));
        (id, package)
    });
    // Fail the first target in the worker's traversal. The second must still finish.
    targets.sort_by_key(|(id, _)| id.to_string());
    let [(stuck, first), (healthy, _)] = targets;
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    tx.del(super::super::initialization::receipt_key(stuck).as_bytes()).unwrap();
    tx.put(&super::super::volume_name_marker(stuck), &[]).unwrap();
    tx.commit().unwrap();
    drop(store);

    let mut services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let unrelated = services.store.create_volume("Unrelated", false).unwrap();
    assert_eq!(services.store.volume(unrelated.id).unwrap(), unrelated);
    services.await_app_data_recovery();
    assert!(services.store.app_data_erasure_pending(stuck).unwrap());
    assert!(!services.store.app_data_erasure_pending(healthy).unwrap());
    assert!(!directory.path().join(crate::paths::VOLUMES).join(healthy.to_string()).exists());
    let log = crate::paths::Paths::for_profile(directory.path()).unwrap().engine_log;
    assert!(
        std::fs::read_to_string(log)
            .unwrap()
            .contains("app cleanup conflicts with retained volume")
    );
    services.store.remember_imported_app(&first).unwrap();
    assert!(matches!(
        services.store.open_app_volume(first.identity().unwrap(), Role::Local),
        Err(Error::LocalContention(_))
    ));
}

#[test]
fn app_removal_accepts_a_data_free_app_with_a_large_unrelated_catalog() {
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let mut other = app.clone();
    other.app_id = "another-app".into();
    // A profile can accumulate hidden local sessions independently of the
    // visible sidebar limit. Their existence cannot restrict this import.
    let mut record = store.local_session(&session(&store, &package).id).unwrap().unwrap();
    record.session.app = other.clone();
    record.session.root_volume = store.automatic_volume_id(&other, None, Role::Root).unwrap();
    record.session.shared_volume = None;
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    for number in 0..4097u32 {
        record.seed[..4].copy_from_slice(&number.to_le_bytes());
        record.session.id = crate::store::session_id(&record.seed).unwrap();
        tx.put(
            format!("local-session/{}", record.session.id).as_bytes(),
            &encode(&record).unwrap(),
        )
        .unwrap();
        tx.put(super::super::session_list::hidden(&record.session.id).as_bytes(), &[1]).unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(store.app_removal_impact(app).unwrap(), AppRemovalImpact::default());
    assert_eq!(
        store.remove_app(app, false, false).unwrap(),
        AppRemovalResult::Removed { erased_volumes: vec![], cleanup_error: None }
    );
}

#[test]
fn removing_an_app_can_keep_more_local_volumes_than_one_erasure_can_stage() {
    let (_directory, store, package) = fixture();
    let app = package.identity().unwrap();
    let session = session(&store, &package);
    let saved = store.open_automatic_volume(&session.id, Role::LocalSession).unwrap();
    let owner = Access::owner(store.account.id);
    let file = store.create_blob(&owner, saved.id, "keep", b"private data").unwrap();
    let mut binding = store.automatic_binding(saved.id).unwrap().unwrap();
    let mut volume = saved.clone();
    // The impact scan uses these same two catalog records for each initialized
    // scope. Only the saved scope needs a physical backing for this check.
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    for n in 1..=4096u32 {
        let mut seed = [0; SEED_BYTES];
        seed[..4].copy_from_slice(&n.to_le_bytes());
        binding.session = Some(crate::store::session_id(&seed).unwrap());
        volume.id =
            store.automatic_volume_id(app, binding.session.as_deref(), binding.role).unwrap();
        tx.put(binding_key(volume.id).as_bytes(), &encode(&binding).unwrap()).unwrap();
        tx.put(format!("volume/{}", volume.id).as_bytes(), &encode(&volume).unwrap()).unwrap();
    }
    tx.commit().unwrap();

    let impact = AppRemovalImpact { user_volumes: false, local_data: true };
    assert_eq!(store.app_removal_impact(app).unwrap(), impact);
    assert_eq!(
        store.remove_app(app, false, false).unwrap(),
        AppRemovalResult::Confirmation(impact)
    );
    assert!(matches!(store.remove_app(app, true, true), Err(Error::LimitExceeded)));
    assert!(!store.app_removed(app).unwrap());
    assert_eq!(
        store.remove_app(app, true, false).unwrap(),
        AppRemovalResult::Removed { erased_volumes: vec![], cleanup_error: None }
    );
    assert!(store.app_removed(app).unwrap());
    assert_eq!(store.read_blob_entry(&owner, saved.id, file.id).unwrap(), b"private data");

    // Explicit selections protect these scopes. A checked removal now has
    // just one target, even though the same large catalog is still present.
    store.remember_imported_app(&package).unwrap();
    let mut record = store.local_session(&session.id).unwrap().unwrap();
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    for n in 1..=4096u32 {
        record.seed = [0; SEED_BYTES];
        record.seed[..4].copy_from_slice(&n.to_le_bytes());
        record.session.id = crate::store::session_id(&record.seed).unwrap();
        record.user_volume = Some(
            store.automatic_volume_id(app, Some(&record.session.id), Role::LocalSession).unwrap(),
        );
        tx.put(
            format!("local-session/{}", record.session.id).as_bytes(),
            &encode(&record).unwrap(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(
        store.remove_app(app, true, true).unwrap(),
        AppRemovalResult::Removed { erased_volumes: vec![saved.id], cleanup_error: None }
    );
    assert!(store.volume(record.user_volume.unwrap()).is_ok());
}
