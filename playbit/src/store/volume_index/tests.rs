use super::*;
use kvdb::{CommittedDelta, CreateOptions, Db, DeltaReceiver, storage::mem::MemStorage};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;

fn id(byte: u8) -> EntryId {
    EntryId::from_bytes([byte; 15]).unwrap()
}

fn db() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap()
}

fn snapshot(db: &Db<MemStorage>) -> IndexSnapshot<'_, MemStorage> {
    IndexSnapshot::new(db.begin_read().unwrap())
}

fn record(name: &str, revision: u8) -> EntryInfo {
    EntryInfo {
        namespace: Default::default(),
        origin: crate::store::volume_index::namespace::Origin {
            installation: crate::store::Id::from_bytes([42; 16]),
            label: None,
        },
        initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
        created_revision: id(revision),
        name: name.into(),
        kind: EntryKind::KvDb {},
        protocol: Some(DatabaseProtocol::Kvdb),
    }
}

#[test]
fn session_participant_grant_is_bound_to_its_shared_volume_and_current_entry_acl() {
    use crate::package::{AppIdentity, AutomaticVolumeRole};
    use crate::store::object_contract::{Grant, Subject};
    use crate::store::{
        BuildSelector, DatabaseId, DatabasePolicy, MetadataPolicy, Session, UpdateAuthority,
    };
    let owner = Id::from_bytes([8; 16]);
    let guest = Id::from_bytes([9; 16]);
    let app = AppIdentity { publisher: owner, app_id: "test.plaza".into() };
    let session_key = [3; 32];
    let world = app.volume_id(AutomaticVolumeRole::Root, None, None, None).unwrap();
    let shared =
        app.volume_id(AutomaticVolumeRole::Session, Some(session_key), None, None).unwrap();
    let build = crate::identifier::encode(&[4; 32]);
    let session = Session {
        app,
        id: crate::identifier::encode(&session_key),
        root_volume: world,
        shared_volume: Some(shared),
        creator: owner,
        selector: BuildSelector::Exact { build_id: build.clone() },
        resolved_build_id: build,
        update_authority: UpdateAuthority::Creator,
    };
    let volume = Volume {
        id: shared,
        owner,
        name: "Room".into(),
        personal: false,
        share_key_with_service: true,
    };
    let acl = Acl {
        grants: vec![Grant {
            subject: Subject::SessionParticipants,
            permissions: vec![Permission::Read, Permission::Write],
        }],
    };
    let metadata = MetadataPolicy::new(volume.clone(), acl.clone()).unwrap();
    let actor = DataActor::session_participant(guest, &session, &volume).unwrap();
    assert!(metadata.authorize(guest, Permission::Read).is_err());
    metadata.authorize(actor, Permission::Read).unwrap();
    let reserved = DatabasePolicy::reserved(metadata.clone(), DatabaseId::Index).unwrap();
    assert!(
        reserved
            .authorize_reader(crate::store::ReplicaReader::SessionParticipant(
                crate::store::PLAYBIT_REPLICA_PRINCIPAL.into()
            ))
            .is_err()
    );
    for permission in [Permission::Append, Permission::ManagePermissions] {
        assert!(metadata.authorize(actor, permission).is_err());
    }
    let other_public_key = [5; 32];
    let other_volume = Volume {
        id: session
            .app
            .volume_id(AutomaticVolumeRole::Session, Some(other_public_key), None, None)
            .unwrap(),
        ..volume.clone()
    };
    let other_session = Session {
        id: crate::identifier::encode(&other_public_key),
        shared_volume: Some(other_volume.id),
        ..session.clone()
    };
    let other_member = DataActor::for_session(guest, &other_session, &other_volume).unwrap();
    assert!(metadata.authorize(other_member, Permission::Write).is_err());
    let world = Volume { id: session.root_volume, ..volume.clone() };
    let other = MetadataPolicy::new(world.clone(), acl).unwrap();
    assert!(other.authorize(actor, Permission::Read).is_err());
    let ordinary = DataActor::for_session(guest, &session, &world).unwrap();
    assert!(metadata.authorize(ordinary, Permission::Read).is_err());
    assert!(
        DataActor::session_participant(
            guest,
            &session,
            &Volume { personal: true, ..volume.clone() }
        )
        .is_err()
    );

    let index = db();
    metadata.write(&index, WALL).unwrap();
    put(&index, 1, record("inherited", 7), WALL + 1);
    let mut restricted = record("restricted", 8);
    restricted.initial_permissions = EntryAcl::Replace(Acl::default());
    put(&index, 2, restricted, WALL + 2);
    let mut explicit = record("participants", 9);
    explicit.initial_permissions = EntryAcl::Replace(metadata.default_permissions().clone());
    put(&index, 3, explicit, WALL + 3);
    for (entry, allowed) in [(id(1), true), (id(2), false), (id(3), true)] {
        let db = DatabasePolicy::read(&index, DatabaseId::Entry(entry)).unwrap();
        assert_eq!(db.authorize(actor, Permission::Write).is_ok(), allowed);
        assert_eq!(
            snapshot(&index)
                .authorize(
                    metadata.volume(),
                    metadata.default_permissions(),
                    actor,
                    entry,
                    Permission::Read
                )
                .is_ok(),
            allowed
        );
    }

    #[cfg(feature = "connected-engine")]
    {
        let captured = db();
        let entry = id(4);
        // Isolate this birth so both replicas share its complete causal cut.
        let local = db();
        receive(&captured, &put(&local, 4, record("temporary", 10), WALL), 128);
        delete(&local, 4, WALL + 1);
        let retirement = |actor, captured_defaults: &Acl| {
            snapshot(&local)
                .retirement_for(
                    entry,
                    &volume,
                    actor,
                    metadata.default_permissions(),
                    &snapshot(&captured),
                    captured_defaults,
                )
                .unwrap()
        };
        assert!(retirement(actor, metadata.default_permissions()).is_some());
        assert!(retirement(guest.into(), metadata.default_permissions()).is_none());
        let read_only = Acl {
            grants: vec![Grant {
                subject: Subject::SessionParticipants,
                permissions: vec![Permission::Read],
            }],
        };
        assert!(retirement(actor, &read_only).is_none());
        let mut tx = captured.begin_write(WALL + 2).unwrap();
        set_permissions(&mut tx, entry, &EntryAcl::Replace(read_only)).unwrap();
        tx.commit().unwrap();
        assert!(retirement(actor, metadata.default_permissions()).is_none());
    }
}

#[test]
fn immutable_initial_permission_replay_requires_current_authority_and_an_existing_birth() {
    use crate::store::object_contract::{Grant, Subject};
    let a = db();
    let b = db();
    let c = db();
    let writer = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Replay".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(writer)),
            permissions: vec![Permission::Write],
        }],
    };
    let mut declaration = record("notes", 7);
    declaration.initial_permissions = EntryAcl::Replace(defaults.clone());
    let CommittedDelta::Bytes(creation) = put(&a, 10, declaration, WALL) else { panic!() };
    apply_delta(&b, &creation, WALL + 1, |prior, fact| {
        prior.authorize_source(&volume, &defaults, volume.owner, fact)
    })
    .unwrap();
    let before = b.begin_read().unwrap().root_hash().unwrap();
    apply_delta(&b, &creation, WALL + 2, |prior, fact| {
        prior.authorize_source(&volume, &defaults, writer, fact)
    })
    .unwrap();
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert!(
        apply_delta(&c, &creation, WALL + 2, |prior, fact| {
            prior.authorize_source(&volume, &defaults, writer, fact)
        })
        .is_err(),
        "an incoming initial grant cannot bootstrap writer authority"
    );
    let mut tx = b.begin_write(WALL + 3).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Replace(Acl::default())).unwrap();
    tx.commit().unwrap();
    let before = b.begin_read().unwrap().root_hash().unwrap();
    assert!(
        apply_delta(&b, &creation, WALL + 4, |prior, fact| {
            prior.authorize_source(&volume, &defaults, writer, fact)
        })
        .is_err(),
        "the original grant does not override its current withdrawal"
    );
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
}

#[test]
fn policy_admission_ignores_mutable_schema_and_rechecks_acl_and_retirement() {
    use crate::store::object_contract::{
        FieldDeclaration, Grant, Subject, TypeDeclaration, ValueType,
    };
    use crate::store::{DatabaseId, DatabasePolicy, EntryType, MetadataPolicy};
    let database = db();
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        owner: Id::from_bytes([2; 16]),
        name: "Policies".into(),
        personal: true,
        share_key_with_service: true,
    };
    let reader = Id::from_bytes([3; 16]);
    MetadataPolicy::new(
        volume.clone(),
        Acl {
            grants: vec![Grant { subject: Subject::Public, permissions: vec![Permission::Read] }],
        },
    )
    .unwrap()
    .write(&database, WALL)
    .unwrap();
    let type_id = Id::from_bytes([4; 16]);
    let declaration = |fields| Schema {
        types: BTreeMap::from([(
            type_id,
            TypeDeclaration { name: "Row".into(), deprecated: false, fields },
        )]),
    };
    let mut birth = record("objects", 20);
    birth.kind = EntryKind::ObjectDb { schema: declaration(BTreeMap::new()) };
    put(&database, 10, birth, WALL);
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
        let mut tx = database.begin_write(WALL + batch + 1).unwrap();
        establish_schema(
            &mut tx,
            id(10),
            &declaration(fields),
            id(30 + batch as u8),
            &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
        )
        .unwrap();
        tx.commit().unwrap();
    }
    schema::MATERIALIZATION_VISITS.with(|visits| visits.set(0));
    let entry = snapshot(&database).live_entry(id(10)).unwrap();
    let EntryKind::ObjectDb { schema: current } = entry.info.kind else { panic!() };
    assert_eq!(current.types[&type_id].fields.len(), 1024);
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 1024);
    schema::MATERIALIZATION_VISITS.with(|visits| visits.set(0));
    let policy = DatabasePolicy::read(&database, DatabaseId::Entry(id(10))).unwrap();
    assert_eq!(policy.entry.unwrap().kind, EntryType::ObjectDb);
    policy.authorize(Principal::Human(reader), Permission::Read).unwrap();
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 0);
    let mut tx = database.begin_write(WALL + 40).unwrap();
    set_permissions(
        &mut tx,
        id(10),
        &EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(reader)),
                permissions: vec![Permission::Write],
            }],
        }),
    )
    .unwrap();
    tx.commit().unwrap();
    schema::MATERIALIZATION_VISITS.with(|visits| visits.set(0));
    let policy = DatabasePolicy::read(&database, DatabaseId::Entry(id(10))).unwrap();
    assert!(policy.authorize(Principal::Human(reader), Permission::Read).is_err());
    policy.authorize(Principal::Human(reader), Permission::Write).unwrap();
    assert!(policy.authorize(Principal::Human(volume.owner), Permission::Read).is_err());
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 0);
    let mut tx = database.begin_write(WALL + 41).unwrap();
    retire(&mut tx, id(10), id(250), &record("origin", 1).origin).unwrap();
    tx.commit().unwrap();
    schema::MATERIALIZATION_VISITS.with(|visits| visits.set(0));
    assert!(matches!(
        DatabasePolicy::read(&database, DatabaseId::Entry(id(10))),
        Err(Error::NotFound(_))
    ));
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 0);
}

fn put(db: &Db<MemStorage>, entry: u8, info: EntryInfo, wall: u64) -> CommittedDelta {
    let mut tx = db.begin_write_with_delta(wall, 64 * 1024).unwrap();
    create(&mut tx, id(entry), &info).unwrap();
    tx.commit_with_delta().unwrap()
}

fn rename_db(
    db: &Db<MemStorage>,
    entry: u8,
    name: &str,
    revision: u8,
    wall: u64,
) -> CommittedDelta {
    let mut tx = db.begin_write_with_delta(wall, 64 * 1024).unwrap();
    rename(
        &mut tx,
        id(entry),
        name,
        id(revision),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit_with_delta().unwrap()
}

fn content_db(
    db: &Db<MemStorage>,
    entry: u8,
    kind: EntryKind,
    revision: u8,
    wall: u64,
) -> CommittedDelta {
    let mut tx = db.begin_write_with_delta(wall, 64 * 1024).unwrap();
    update_content(
        &mut tx,
        id(entry),
        kind,
        id(revision),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit_with_delta().unwrap()
}

fn revision_name(revision: &EntryRevision) -> Option<&str> {
    match &revision.change {
        EntryChange::Created { info } => Some(&info.name),
        EntryChange::Name { name } => Some(name),
        EntryChange::Move { name, .. } => name.as_deref(),
        EntryChange::Content { .. } | EntryChange::Delete {} | EntryChange::Schema { .. } => None,
    }
}

fn revision_kind(revision: &EntryRevision) -> &EntryKind {
    match &revision.change {
        EntryChange::Created { info } => &info.kind,
        EntryChange::Content { kind } => kind,
        EntryChange::Name { .. }
        | EntryChange::Move { .. }
        | EntryChange::Delete {}
        | EntryChange::Schema { .. } => {
            panic!("revision has no content metadata")
        }
    }
}

fn delete(db: &Db<MemStorage>, entry: u8, wall: u64) -> CommittedDelta {
    let mut tx = db.begin_write_with_delta(wall, 64 * 1024).unwrap();
    retire(&mut tx, id(entry), id(250), &record("origin", 1).origin).unwrap();
    tx.commit_with_delta().unwrap()
}

fn receive(db: &Db<MemStorage>, delta: &CommittedDelta, chunk_size: usize) -> usize {
    let CommittedDelta::Bytes(bytes) = delta else { panic!("fixture is bounded") };
    let changed = RefCell::new(BTreeSet::new());
    let rejected = RefCell::new(None);
    let resolver = |delta: &Delta<'_>| match resolve(delta) {
        Ok(resolution) => {
            if resolution == Resolution::UseRemote {
                changed.borrow_mut().insert(source_entry_id(delta.key).unwrap());
            }
            resolution
        }
        Err(error) => {
            *rejected.borrow_mut() = Some(error);
            Resolution::KeepLocal
        }
    };
    let mut receiver = DeltaReceiver::new(db, resolver, WALL + 100, 64 * 1024).unwrap();
    for fragment in bytes.chunks(chunk_size) {
        receiver.process(fragment).unwrap();
    }
    assert!(rejected.borrow().is_none(), "{rejected:?}");
    let ids: Vec<_> = changed.borrow().iter().copied().collect();
    receiver
        .repair_local_indexes(|local| {
            repair(local, &ids)?;
            namespace::source::cut::project(local)?.write_current(local)
        })
        .unwrap()
        .finish()
        .unwrap()
        .applied
}

fn replicate(source: &Db<MemStorage>, destination: &Db<MemStorage>, reverse: bool) {
    let read = source.begin_read().unwrap();
    let mut deltas = Vec::new();
    for entry in read.iter() {
        let (key, _) = entry.unwrap();
        deltas.push(read.delta_for_key(&key, 64 * 1024).unwrap().unwrap());
    }
    if reverse {
        deltas.reverse();
    }
    for delta in deltas {
        receive(destination, &delta, 1);
        receive(destination, &delta, 17);
    }
}

fn assert_invariants(db: &Db<MemStorage>) {
    // Navigation exposes admitted effective names, including conflicts; raw
    // register heads may be Pending or precede their immutable history.
    let index = snapshot(db);
    let entries = index.children(None).unwrap();
    let mut names = BTreeSet::new();
    for entry in &entries {
        assert!(names.insert(&entry.name));
        assert!(!entry.deleted);
        assert_eq!(index.lookup(&entry.name).unwrap().id, entry.id);
    }
    db.verify().unwrap();
}

#[test]
fn paths_are_relative_byte_compared_and_bounded() {
    for name in ["notes", "photos/猫.png", "Photos/猫.png", "é", "e\u{301}"] {
        validate_name(name).unwrap();
    }
    for name in ["", "/notes", "notes/", "a//b", "a/../b", ".", "a/./b", "x\0y", "x\ny"] {
        assert!(validate_name(name).is_err(), "{name:?}");
    }
    assert!(validate_name(&"x".repeat(MAX_ENTRY_COMPONENT_BYTES)).is_ok());
    assert!(validate_name(&"x".repeat(MAX_ENTRY_COMPONENT_BYTES + 1)).is_err());
    assert!(
        candidate_key(None, &"x".repeat(MAX_ENTRY_COMPONENT_BYTES), id(1)).len()
            <= kvdb::KEY_SIZE_MAX
    );
}

#[test]
fn local_rename_preserves_id_snapshot_and_history_and_rejects_known_collision() {
    let db = db();
    put(&db, 10, record("notes", 1), WALL);
    let before = snapshot(&db);
    rename_db(&db, 10, "work", 2, WALL + 1);
    assert_eq!(before.lookup("notes").unwrap().id, id(10));
    assert!(before.lookup("work").is_err());
    assert!(snapshot(&db).lookup("notes").is_err());
    assert_eq!(snapshot(&db).lookup("work").unwrap().id, id(10));
    assert_eq!(revision_name(&snapshot(&db).revision(id(10), id(1)).unwrap()), Some("notes"));
    let before_root = db.begin_read().unwrap().root_hash().unwrap();
    let mut tx = db.begin_write(WALL + 2).unwrap();
    assert!(matches!(create(&mut tx, id(20), &record("work", 3)), Err(Error::AlreadyExists(_))));
    drop(tx);
    assert_eq!(before_root, db.begin_read().unwrap().root_hash().unwrap());
    assert_invariants(&db);
}

#[test]
fn offline_competing_creates_preserve_both_entries_and_reveal_loser() {
    for reverse in [false, true] {
        let a = db();
        let b = db();
        put(&a, 10, record("notes", 1), WALL);
        put(&b, 20, record("notes", 2), WALL);
        replicate(&a, &b, reverse);
        replicate(&b, &a, !reverse);
        for db in [&a, &b] {
            assert_eq!(snapshot(db).lookup("notes").unwrap().id, id(20));
            assert!(snapshot(db).live_entry(id(10)).unwrap().name_conflict);
            assert_invariants(db);
        }
        let deletion = delete(&b, 20, WALL + 1);
        receive(&a, &deletion, 1);
        for db in [&a, &b] {
            assert_eq!(snapshot(db).lookup("notes").unwrap().id, id(10));
            assert!(snapshot(db).entry(id(20)).unwrap().deleted);
            assert_invariants(db);
        }
    }
}

#[test]
fn concurrent_rename_has_one_name_and_retains_equal_clock_losing_revision() {
    for equal_clock in [false, true] {
        let a = db();
        let b = db();
        let initial = put(&a, 10, record("notes", 1), WALL);
        receive(&b, &initial, 1);
        // Receiving advances B's clock; using the same explicit source clock
        // via a shared fork preserves equality for the tie-break scenario.
        let wall_a = WALL + 200;
        let wall_b = if equal_clock { wall_a } else { wall_a + 1 };
        rename_db(&a, 10, "work", 2, wall_a);
        rename_db(&b, 10, "personal", 3, wall_b);
        // Equal clocks select the hashed event ID, independently of text encoding.
        let event_rank = |db: &Db<MemStorage>, revision| {
            let record = snapshot(db).revision(id(10), id(revision)).unwrap();
            let event = namespace::event_id(
                id(10),
                id(revision),
                *blake3::hash(&record.canonical().unwrap()).as_bytes(),
            )
            .unwrap();
            (record.origin_clock, event)
        };
        let (winner, loser) = if event_rank(&a, 2) > event_rank(&b, 3) {
            ("work", "personal")
        } else {
            ("personal", "work")
        };
        replicate(&a, &b, true);
        replicate(&b, &a, false);
        for db in [&a, &b] {
            let index = snapshot(db);
            assert_eq!(index.lookup(winner).unwrap().id, id(10));
            assert!(index.lookup(loser).is_err());
            assert!(index.lookup("notes").is_err());
            assert_eq!(revision_name(&index.revision(id(10), id(2)).unwrap()), Some("work"));
            assert_invariants(db);
        }
    }
}

#[test]
fn deletion_wins_newer_rename_and_delayed_delete_cannot_retire_recreated_name() {
    for reverse in [false, true] {
        let a = db();
        let b = db();
        let initial = put(&a, 10, record("notes", 1), WALL);
        receive(&b, &initial, 1);
        let rename = rename_db(&a, 10, "work", 2, WALL + 1000);
        let deletion = delete(&b, 10, WALL + 1);
        put(&b, 20, record("notes", 3), WALL + 2);
        receive(&a, &deletion, 1);
        receive(&b, &rename, 1);
        replicate(&a, &b, reverse);
        replicate(&b, &a, !reverse);
        receive(&b, &deletion, 1);
        receive(&b, &initial, 1);
        for db in [&a, &b] {
            let index = snapshot(db);
            assert_eq!(index.lookup("notes").unwrap().id, id(20));
            assert!(index.lookup("work").is_err());
            assert!(index.live_entry(id(10)).is_err());
            assert!(index.entry(id(10)).unwrap().deleted);
            assert_eq!(revision_name(&index.revision(id(10), id(2)).unwrap()), Some("work"));
            let mut tx = db.begin_write(WALL + 2000).unwrap();
            assert!(create(&mut tx, id(10), &record("restored", 4)).is_err());
            drop(tx);
            assert_invariants(db);
        }
    }
}

#[test]
#[cfg(feature = "connected-engine")]
fn owner_retirement_repair_requires_a_qualified_delete_matching_birth_and_current_write() {
    let local = db();
    let captured = db();
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        owner: Id::from_bytes([2; 16]),
        name: "Retirement".into(),
        personal: false,
        share_key_with_service: true,
    };
    let defaults = Acl::default();
    let birth = put(&local, 10, record("temporary", 1), WALL);
    receive(&captured, &birth, 128);
    let repair = |ap: &Db<MemStorage>| {
        snapshot(&local)
            .retirement_for(
                id(10),
                &volume,
                volume.owner.into(),
                &defaults,
                &snapshot(ap),
                &defaults,
            )
            .unwrap()
    };
    assert!(repair(&captured).is_none());
    delete(&local, 10, WALL + 1);
    let retired = repair(&captured).unwrap();
    let projection = db();
    let mut tx = projection.begin_write(WALL + 2).unwrap();
    retired.copy_into(&mut tx).unwrap();
    tx.commit().unwrap();
    let source = local.begin_read().unwrap();
    let submission = projection.begin_read().unwrap();
    assert_eq!(submission.iter().count(), 2);
    for key in [meta_key(RETIRED, id(10)).to_vec(), revision_key(id(10), id(250)).to_vec()] {
        let (clock, bytes) = source.get_with_clock(&key).unwrap().unwrap();
        let (sent_clock, sent_bytes) = submission.get_with_clock(&key).unwrap().unwrap();
        assert_eq!((sent_clock, sent_bytes.as_ref()), (clock, bytes.as_ref()));
    }

    // A physically received retirement without local authorship or qualification
    // cannot enter the selected writer's recovery path.
    let mut tx = captured.begin_write(WALL + 2).unwrap();
    retired.copy_into(&mut tx).unwrap();
    tx.commit().unwrap();
    assert!(!snapshot(&captured).header(id(10)).unwrap().deleted);
    assert!(snapshot(&captured).selected_retirement(id(10)).unwrap().is_none());
    let different = db();
    put(&different, 10, record("different incarnation", 2), WALL);
    assert!(repair(&different).is_none());
    let mut tx = captured.begin_write(WALL + 2).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Replace(Acl::default())).unwrap();
    tx.commit().unwrap();
    assert!(repair(&captured).is_none());
}

#[test]
fn reordered_retirement_before_creation_never_temporarily_publishes_old_name() {
    let a = db();
    let b = db();
    put(&a, 10, record("notes", 1), WALL);
    let deletion = delete(&a, 10, WALL + 1);
    receive(&b, &deletion, 1);
    replicate(&a, &b, true);
    assert!(snapshot(&b).lookup("notes").is_err());
    assert!(snapshot(&b).entry(id(10)).unwrap().deleted);
    assert_invariants(&b);
}

#[test]
fn blob_conflict_versions_and_mtime_survive_rename_and_retirement() {
    let a = db();
    let b = db();
    let blob = |name: &str, revision: u8, digest: u8, published_at_micros: i64| EntryInfo {
        namespace: Default::default(),
        origin: crate::store::volume_index::namespace::Origin {
            installation: crate::store::Id::from_bytes([42; 16]),
            label: None,
        },
        initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
        created_revision: id(revision),
        name: name.into(),
        kind: EntryKind::Blob {
            len_bytes: 10,
            digest: [digest; 32],
            encrypted_key: [digest + 1; 32],
            published_at_micros,
        },
        protocol: None,
    };
    let initial = put(&a, 10, blob("cat", 1, 10, 1000), WALL);
    receive(&b, &initial, 1);
    content_db(&a, 10, blob("cat", 2, 20, 2000).kind, 2, WALL + 200);
    content_db(&b, 10, blob("cat", 3, 30, 3000).kind, 3, WALL + 201);
    replicate(&a, &b, true);
    replicate(&b, &a, false);
    rename_db(&a, 10, "kitten", 4, WALL + 300);
    delete(&a, 10, WALL + 301);
    let index = snapshot(&a);
    assert_eq!(
        revision_kind(&index.revision(id(10), id(2)).unwrap()),
        &blob("cat", 2, 20, 2000).kind
    );
    assert_eq!(
        revision_kind(&index.revision(id(10), id(3)).unwrap()),
        &blob("cat", 3, 30, 3000).kind
    );
    assert_eq!(index.entry(id(10)).unwrap().info.kind, blob("cat", 4, 30, 3000).kind);
    assert_invariants(&a);
}

#[test]
fn rename_preserves_concurrent_blob_content_in_both_delivery_orders() {
    for reverse in [false, true] {
        let a = db();
        let b = db();
        let mut initial = record("cat", 1);
        initial.kind = EntryKind::Blob {
            len_bytes: 10,
            digest: [10; 32],
            encrypted_key: [11; 32],
            published_at_micros: 1000,
        };
        initial.protocol = None;
        let creation = put(&a, 10, initial.clone(), WALL);
        receive(&b, &creation, 1);
        rename_db(&a, 10, "kitten", 2, WALL + 1000);
        let mut updated = initial;
        updated.kind = EntryKind::Blob {
            len_bytes: 20,
            digest: [20; 32],
            encrypted_key: [21; 32],
            published_at_micros: 2000,
        };
        content_db(&b, 10, updated.kind.clone(), 3, WALL + 1);
        replicate(&a, &b, reverse);
        replicate(&b, &a, !reverse);
        replicate(&a, &b, !reverse);
        for database in [&a, &b] {
            let current = snapshot(database).live_entry(id(10)).unwrap();
            assert_eq!(current.info.name, "kitten");
            assert_eq!(current.info.kind, updated.kind);
            assert_invariants(database);
        }
    }
}

#[test]
fn rename_preserves_concurrent_schema_update_without_rewriting_declaration() {
    use crate::store::object_contract::TypeDeclaration;
    for reverse in [false, true] {
        let a = db();
        let b = db();
        let mut initial = record("notes", 7);
        initial.kind = EntryKind::ObjectDb { schema: Schema::default() };
        let creation = put(&a, 10, initial, WALL);
        receive(&b, &creation, 1);
        let before = a
            .begin_read()
            .unwrap()
            .get_with_clock(&entry_key(id(10)))
            .map(|record| record.map(|(clock, bytes)| (clock, bytes.to_vec())))
            .unwrap();
        rename_db(&a, 10, "work", 2, WALL + 1000);
        let mut schema = Schema::default();
        schema.types.insert(
            Id::from_bytes([8; 16]),
            TypeDeclaration { name: "Note".into(), deprecated: false, fields: Default::default() },
        );
        let mut tx = b.begin_write_with_delta(WALL + 1, 64 * 1024).unwrap();
        establish_schema(
            &mut tx,
            id(10),
            &schema,
            id(3),
            &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
        )
        .unwrap();
        tx.commit().unwrap();
        replicate(&a, &b, reverse);
        replicate(&b, &a, !reverse);
        for database in [&a, &b] {
            let current = snapshot(database).live_entry(id(10)).unwrap();
            assert_eq!(current.info.name, "work");
            assert_eq!(current.info.kind, EntryKind::ObjectDb { schema: schema.clone() });
            assert_eq!(current.name_revision, id(2));
            assert_eq!(current.content_revision, id(7));
            assert_eq!(current.info.created_revision, id(7));
            let after = database
                .begin_read()
                .unwrap()
                .get_with_clock(&entry_key(id(10)))
                .map(|record| record.map(|(clock, bytes)| (clock, bytes.to_vec())))
                .unwrap();
            assert_eq!(before, after);
            assert_invariants(database);
        }
    }
}

fn object_schema(fields: &[(u8, &str, bool)]) -> Schema {
    use crate::store::object_contract::{FieldDeclaration, TypeDeclaration, ValueType};
    Schema {
        types: [(
            Id::from_bytes([1; 16]),
            TypeDeclaration {
                name: "Note".into(),
                deprecated: false,
                fields: fields
                    .iter()
                    .map(|(field, name, deprecated)| {
                        (
                            Id::from_bytes([*field; 16]),
                            FieldDeclaration {
                                name: (*name).into(),
                                value_type: ValueType::Text,
                                repeated: false,
                                deprecated: *deprecated,
                            },
                        )
                    })
                    .collect(),
            },
        )]
        .into(),
    }
}

fn schema_db(
    database: &Db<MemStorage>,
    schema: &Schema,
    revision: u8,
    wall: u64,
) -> CommittedDelta {
    let mut tx = database.begin_write_with_delta(wall, 64 * 1024).unwrap();
    establish_schema(
        &mut tx,
        id(10),
        schema,
        id(revision),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit_with_delta().unwrap()
}

#[test]
fn offline_schema_additions_and_deprecations_converge_without_losing_name_conflicts() {
    for reverse in [false, true] {
        let a = db();
        let b = db();
        let mut initial = record("notes", 7);
        initial.kind = EntryKind::ObjectDb { schema: object_schema(&[(2, "title", false)]) };
        receive(&b, &put(&a, 10, initial, WALL), 1);
        schema_db(&a, &object_schema(&[(2, "title", true), (3, "body", false)]), 8, WALL + 1);
        schema_db(&b, &object_schema(&[(4, "body", false)]), 9, WALL + 2);
        replicate(&a, &b, reverse);
        replicate(&b, &a, !reverse);
        for database in [&a, &b] {
            let EntryKind::ObjectDb { schema } =
                snapshot(database).live_entry(id(10)).unwrap().info.kind
            else {
                panic!()
            };
            let type_id = Id::from_bytes([1; 16]);
            let fields = &schema.types[&type_id].fields;
            assert_eq!(fields.len(), 3);
            assert!(fields[&Id::from_bytes([2; 16])].deprecated);
            assert!(matches!(schema.field_by_name(type_id, "body"), Err(Error::AlreadyExists(_))));
            assert!(fields.contains_key(&Id::from_bytes([3; 16])));
            assert!(fields.contains_key(&Id::from_bytes([4; 16])));
            assert!(!snapshot(database).schema_pending(id(10)).unwrap());
            assert_eq!(snapshot(database).revisions(id(10), None, 64).unwrap().revisions.len(), 3);
            assert_invariants(database);
        }
        assert_eq!(
            a.begin_read().unwrap().root_hash().unwrap(),
            b.begin_read().unwrap().root_hash().unwrap()
        );
    }
}

#[test]
fn duplicate_schema_establishment_and_concurrent_identical_ids_preserve_meaning() {
    let a = db();
    let b = db();
    let mut initial = record("notes", 7);
    initial.kind = EntryKind::ObjectDb { schema: Schema::default() };
    receive(&b, &put(&a, 10, initial, WALL), 1);
    let addition = object_schema(&[(2, "title", false)]);
    let first = schema_db(&a, &addition, 8, WALL + 1);
    schema_db(&b, &addition, 9, WALL + 2);
    receive(&b, &first, 1);
    receive(&b, &first, 1);
    replicate(&b, &a, true);
    assert_eq!(
        snapshot(&a).live_entry(id(10)).unwrap().info.kind,
        EntryKind::ObjectDb { schema: addition.clone() }
    );
    let before = a.begin_read().unwrap().root_hash().unwrap();
    let mut changed = addition;
    changed
        .types
        .get_mut(&Id::from_bytes([1; 16]))
        .unwrap()
        .fields
        .get_mut(&Id::from_bytes([2; 16]))
        .unwrap()
        .name = "changed".into();
    let mut tx = a.begin_write(WALL + 3).unwrap();
    assert!(
        establish_schema(
            &mut tx,
            id(10),
            &changed,
            id(11),
            &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None }
        )
        .is_err()
    );
    drop(tx);
    assert_eq!(before, a.begin_read().unwrap().root_hash().unwrap());
}

#[test]
fn schema_additions_preserve_optional_facts_and_their_origin_clocks() {
    // An older replica reasserts type/field deprecation while adding a field.
    // Every unfamiliar optional fact byte and original clock must survive.
    let source = db();
    let target = db();
    let mut initial = record("notes", 7);
    initial.kind = EntryKind::ObjectDb { schema: Schema::default() };
    put(&source, 10, initial, WALL);
    let mut declaration = object_schema(&[(2, "title", true)]);
    let type_id = Id::from_bytes([1; 16]);
    declaration.types.get_mut(&type_id).unwrap().deprecated = true;
    schema_db(&source, &declaration, 8, WALL + 1);
    let original = source.begin_read().unwrap();
    let mut tx = source.begin_write(WALL + 2).unwrap();
    let mut saved = BTreeMap::new();
    for item in original.iter() {
        let (key, _) = item.unwrap();
        if !schema::is_key(&key) {
            continue;
        }
        let (clock, value) = original.get_with_clock(&key).unwrap().unwrap();
        let mut bytes = value.to_vec();
        bytes.extend_from_slice(&99u16.to_le_bytes());
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.push(record::TEXT);
        bytes.extend_from_slice(b"future");
        schema::validate_value(&key, &bytes).unwrap();
        for requirement in [record::READ_REQUIRED, record::WRITE_REQUIRED] {
            let mut required = bytes.clone();
            let offset = value.len();
            required[offset..offset + 2].copy_from_slice(&(99 | requirement).to_le_bytes());
            assert!(schema::validate_value(&key, &required).is_err());
        }
        tx.put_with_clock(&key, &bytes, clock).unwrap();
        saved.insert(key.to_vec(), (clock, bytes));
    }
    tx.commit().unwrap();
    drop(original);
    assert_eq!(saved.len(), 4, "type, field and both deprecations");
    replicate(&source, &target, false);
    let creation = {
        let snapshot = target.begin_read().unwrap();
        let (clock, bytes) = snapshot.get_with_clock(&entry_key(id(10))).unwrap().unwrap();
        (clock, bytes.to_vec())
    };
    let mut addition = object_schema(&[(2, "title", true), (3, "body", false)]);
    addition.types.get_mut(&type_id).unwrap().deprecated = true;
    schema_db(&target, &addition, 9, WALL + 3);
    let EntryKind::ObjectDb { schema } = snapshot(&target).live_entry(id(10)).unwrap().info.kind
    else {
        panic!()
    };
    assert_eq!(schema, addition);
    let current = target.begin_read().unwrap();
    for (key, (clock, bytes)) in &saved {
        let (current_clock, current_bytes) = current.get_with_clock(key).unwrap().unwrap();
        assert_eq!((current_clock, current_bytes.as_ref()), (*clock, bytes.as_slice()));
    }
    let (clock, bytes) = current.get_with_clock(&entry_key(id(10))).unwrap().unwrap();
    assert_eq!((clock, bytes.as_ref()), (creation.0, creation.1.as_slice()));
    let before = current.root_hash().unwrap();
    drop(current);
    schema_db(&target, &addition, 10, WALL + 4);
    assert_eq!(target.begin_read().unwrap().root_hash().unwrap(), before);
    let mut changed = addition;
    changed
        .types
        .get_mut(&type_id)
        .unwrap()
        .fields
        .get_mut(&Id::from_bytes([2; 16]))
        .unwrap()
        .name = "renamed".into();
    let mut tx = target.begin_write(WALL + 5).unwrap();
    assert!(
        establish_schema(&mut tx, id(10), &changed, id(11), &record("origin", 1).origin).is_err()
    );
    drop(tx);
    assert_eq!(target.begin_read().unwrap().root_hash().unwrap(), before);
    assert_invariants(&target);
}

#[test]
fn reordered_schema_dependencies_remain_pending_without_hiding_the_entry_name() {
    let a = db();
    let b = db();
    let mut initial = record("notes", 7);
    initial.kind = EntryKind::ObjectDb { schema: Schema::default() };
    let creation = put(&a, 10, initial, WALL);
    schema_db(&a, &object_schema(&[(2, "title", true)]), 8, WALL + 1);
    let tx = a.begin_read().unwrap();
    let mut by_kind = BTreeMap::new();
    for item in tx.iter() {
        let (key, _) = item.unwrap();
        if schema::is_key(&key) {
            by_kind.insert(key[1], tx.delta_for_key(&key, 64 * 1024).unwrap().unwrap());
        }
    }
    receive(&b, &by_kind[&schema::DEPRECATED], 1);
    receive(&b, &by_kind[&schema::FIELD], 1);
    assert!(snapshot(&b).page(None, 64).unwrap().entries.is_empty());
    receive(&b, &creation, 1);
    assert_eq!(snapshot(&b).lookup("notes").unwrap().id, id(10));
    assert!(snapshot(&b).schema_pending(id(10)).unwrap());
    receive(&b, &by_kind[&schema::TYPE], 1);
    assert!(!snapshot(&b).schema_pending(id(10)).unwrap());
    let EntryKind::ObjectDb { schema } = snapshot(&b).live_entry(id(10)).unwrap().info.kind else {
        panic!()
    };
    assert!(schema.types[&Id::from_bytes([1; 16])].fields[&Id::from_bytes([2; 16])].deprecated);
}

#[test]
fn wrong_schema_creation_binding_aborts_source_and_name_publication() {
    let a = db();
    let b = db();
    let mut initial = record("notes", 7);
    initial.kind = EntryKind::ObjectDb { schema: Schema::default() };
    receive(&b, &put(&a, 10, initial, WALL), 1);
    schema_db(&a, &object_schema(&[(2, "title", false)]), 8, WALL + 1);
    let key = {
        let tx = a.begin_read().unwrap();
        tx.iter()
            .map(|item| item.unwrap().0.to_vec())
            .find(|key| key.len() == 49 && key[1] == schema::FIELD)
            .unwrap()
    };
    let mut tx = a.begin_write_with_delta(WALL + 2, 64 * 1024).unwrap();
    let mut value = tx.get(&key).unwrap().unwrap().to_vec();
    // PBR header + struct + first field header + binary kind precede the anchor.
    assert_eq!(
        &value[12..44],
        record::View::record(&value)
            .unwrap()
            .fields(record::Mode::Write, &[1, 2])
            .unwrap()
            .get(1)
            .unwrap()
            .body(record::BYTES)
            .unwrap()
    );
    value[12..44].fill(0x99);
    tx.put(&key, &value).unwrap();
    let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
    let before = b.begin_read().unwrap().root_hash().unwrap();
    assert!(apply_delta(&b, &delta, WALL + 3, |_, _| Ok(())).is_err());
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(snapshot(&b).lookup("notes").unwrap().id, id(10));
}

#[test]
fn schema_materialization_matches_regression_cases_under_reordered_delivery() {
    let source = db();
    let mut initial = record("notes", 7);
    initial.kind = EntryKind::ObjectDb { schema: Schema::default() };
    let creation = put(&source, 10, initial, WALL);
    schema_db(&source, &object_schema(&[(2, "title", true)]), 8, WALL + 1);
    let retirement = delete(&source, 10, WALL + 2);
    let mut facts = BTreeMap::new();
    let tx = source.begin_read().unwrap();
    for item in tx.iter() {
        let (key, _) = item.unwrap();
        if schema::is_key(&key) {
            facts.insert(key[1], tx.delta_for_key(&key, 64 * 1024).unwrap().unwrap());
        }
    }
    for row in include_str!("../../../tests/fixtures/schema-index.txt").lines() {
        let word: Vec<_> = row.split_whitespace().collect();
        let flag = |index: usize| word[index].parse::<bool>().unwrap();
        let replica = db();
        for (kind, selected) in
            [(schema::DEPRECATED, flag(3)), (schema::FIELD, flag(2)), (schema::TYPE, flag(1))]
        {
            if selected {
                receive(&replica, &facts[&kind], 1);
                receive(&replica, &facts[&kind], 17);
            }
        }
        if flag(4) {
            receive(&replica, &retirement, 1);
        }
        if flag(0) {
            receive(&replica, &creation, 1);
        }
        let index = snapshot(&replica);
        let mut type_present = false;
        let mut field_present = false;
        let mut deprecated = false;
        let mut pending = false;
        if let Ok(entry) = index.entry(id(10)) {
            let EntryKind::ObjectDb { schema } = entry.info.kind else { panic!() };
            if let Some(declaration) = schema.types.get(&Id::from_bytes([1; 16])) {
                type_present = true;
                if let Some(field) = declaration.fields.get(&Id::from_bytes([2; 16])) {
                    field_present = true;
                    deprecated = field.deprecated;
                }
            }
            pending = index.schema_pending(id(10)).unwrap();
        }
        assert_eq!(
            [
                type_present,
                field_present,
                deprecated,
                field_present && index.live_entry(id(10)).is_ok(),
                pending
            ],
            [flag(5), flag(6), flag(7), flag(8), flag(9)],
            "{row}"
        );
        assert_invariants(&replica);
    }
}

#[test]
fn reordered_register_and_history_wait_for_creation_and_never_publish_initial_name() {
    let a = db();
    let b = db();
    let creation = put(&a, 10, record("notes", 7), WALL);
    rename_db(&a, 10, "work", 2, WALL + 1000);
    let read = a.begin_read().unwrap();
    for key in [meta_key(NAME, id(10)).to_vec(), revision_key(id(10), id(2)).to_vec()] {
        let delta = read.delta_for_key(&key, 64 * 1024).unwrap().unwrap();
        receive(&b, &delta, 1);
        receive(&b, &delta, 17);
        assert!(snapshot(&b).page(None, 64).unwrap().entries.is_empty());
        assert!(snapshot(&b).lookup("work").is_err());
    }
    receive(&b, &creation, 1);
    assert_eq!(snapshot(&b).lookup("work").unwrap().id, id(10));
    assert!(snapshot(&b).lookup("notes").is_err());
    receive(&b, &creation, 17);
    replicate(&a, &b, true);
    assert_eq!(snapshot(&b).revisions(id(10), None, 64).unwrap().revisions.len(), 2);
    assert_invariants(&b);
}

#[test]
fn append_cannot_admit_name_or_history_before_creation_arrives() {
    use crate::store::object_contract::{Grant, Subject};
    let a = db();
    let b = db();
    let appender = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Append".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(appender)),
            permissions: vec![Permission::Append],
        }],
    };
    let CommittedDelta::Bytes(creation) = put(&a, 10, record("notes", 7), WALL) else { panic!() };
    rename_db(&a, 10, "work", 2, WALL + 1000);
    let read = a.begin_read().unwrap();
    for key in [meta_key(NAME, id(10)).to_vec(), revision_key(id(10), id(2)).to_vec()] {
        let CommittedDelta::Bytes(delta) = read.delta_for_key(&key, 64 * 1024).unwrap().unwrap()
        else {
            panic!()
        };
        let before = b.begin_read().unwrap().root_hash().unwrap();
        assert!(
            apply_delta(&b, &delta, WALL + 2000, |prior, delta| prior
                .authorize_source(&volume, &defaults, appender, delta))
            .is_err()
        );
        assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    }
    apply_delta(&b, &creation, WALL + 2001, |prior, delta| {
        prior.authorize_source(&volume, &defaults, appender, delta)
    })
    .unwrap();
    assert_eq!(snapshot(&b).lookup("notes").unwrap().id, id(10));
    assert_invariants(&b);
}

#[test]
fn creation_bytes_and_origin_clock_are_immutable_even_without_history() {
    let clock = kvdb::Clock { time: 100, count: 1 };
    let bytes = record("notes", 7).canonical().unwrap();
    assert!(
        validate_source(&Delta {
            key: &entry_key(id(10)),
            local: EntState::Missing,
            remote: EntState::Present { value: &bytes, clock: kvdb::Clock::ZERO }
        })
        .is_err()
    );
    let mut zero_origin = record("notes", 7);
    zero_origin.origin.installation = Id::from_bytes([0; 16]);
    assert!(
        validate_record(&entry_key(id(10)), Some(&record::encode(&zero_origin).unwrap())).is_err()
    );
    let fields = record::View::record(&bytes)
        .unwrap()
        .fields(record::Mode::Write, &[1, 2, 3, 4, 5, 6, 7])
        .unwrap();
    let mut missing_origin = Vec::from(record::MAGIC);
    missing_origin.push(record::STRUCT);
    for tag in [1, 3, 4, 5, 6, 7] {
        record::put_raw(&mut missing_origin, tag, fields.get(tag).unwrap().0).unwrap();
    }
    assert!(validate_record(&entry_key(id(10)), Some(&missing_origin)).is_err());
    let local = EntState::Present { value: &bytes, clock };
    let changed = record("work", 7).canonical().unwrap();
    for remote in [
        EntState::Present { value: &bytes, clock: kvdb::Clock { time: 100, count: 2 } },
        EntState::Present { value: &changed, clock },
    ] {
        assert!(validate_source(&Delta { key: &entry_key(id(10)), local, remote }).is_err());
    }
    assert!(validate_source(&Delta { key: &entry_key(id(10)), local, remote: local }).is_ok());
}

#[test]
fn unsupported_required_record_semantics_abort_the_entire_delta() {
    for requirement in [record::READ_REQUIRED, record::WRITE_REQUIRED] {
        let source = db();
        let recipient = db();
        let creation = put(&source, 10, record("notes", 7), WALL);
        receive(&recipient, &creation, 1);
        let mut tx = source.begin_write_with_delta(WALL + 1, 64 * 1024).unwrap();
        let mut bytes = record("future", 8).canonical().unwrap();
        bytes.extend_from_slice(&(99 | requirement).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(255);
        tx.put(&entry_key(id(11)), &bytes).unwrap();
        let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
        let before = (recipient.clock(), recipient.begin_read().unwrap().root_hash().unwrap());
        assert!(apply_delta(&recipient, &delta, WALL + 2, |_, _| Ok(())).is_err());
        assert_eq!(
            (recipient.clock(), recipient.begin_read().unwrap().root_hash().unwrap()),
            before
        );
        assert_eq!(snapshot(&recipient).lookup("notes").unwrap().id, id(10));
        assert!(snapshot(&recipient).lookup("future").is_err());
    }
}

#[test]
fn wrong_creation_binding_aborts_source_and_derived_publication() {
    let a = db();
    let b = db();
    let creation = put(&a, 10, record("notes", 7), WALL);
    receive(&b, &creation, 1);
    rename_db(&a, 10, "work", 2, WALL + 1000);
    let mut tx = a.begin_write_with_delta(WALL + 1001, 64 * 1024).unwrap();
    let mut event = revision(&tx.get(&meta_key(NAME, id(10))).unwrap().unwrap()).unwrap();
    event.creation = [9; 32];
    event.origin_clock = tx.clock().as_u64();
    tx.put(&meta_key(NAME, id(10)), &event.canonical().unwrap()).unwrap();
    let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
    let before = b.begin_read().unwrap().root_hash().unwrap();
    assert!(apply_delta(&b, &delta, WALL + 2000, |_, _| Ok(())).is_err());
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(snapshot(&b).lookup("notes").unwrap().id, id(10));
    assert!(snapshot(&b).lookup("work").is_err());
    assert_invariants(&b);
}

#[test]
fn deletion_binding_is_checked_when_creation_arrives_last() {
    let source = db();
    let recipient = db();
    let birth = put(&source, 10, record("file", 7), WALL);
    let mut tx = source.begin_write_with_delta(WALL + 1, 64 * 1024).unwrap();
    let deletion = EntryRevision {
        revision: id(8),
        creation: [9; 32],
        origin_clock: tx.clock().as_u64(),
        origin: record("file", 7).origin,
        observed: Vec::new(),
        change: EntryChange::Delete {},
    };
    tx.put(&meta_key(RETIRED, id(10)), &deletion.canonical().unwrap()).unwrap();
    let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
    apply_delta(&recipient, &delta, WALL + 2, |_, _| Ok(())).unwrap();
    let before = recipient.begin_read().unwrap().root_hash().unwrap();
    let CommittedDelta::Bytes(birth) = birth else { panic!() };
    assert!(apply_delta(&recipient, &birth, WALL + 3, |_, _| Ok(())).is_err());
    assert_eq!(before, recipient.begin_read().unwrap().root_hash().unwrap());
    assert!(snapshot(&recipient).entry(id(10)).is_err());
}

#[test]
fn deletion_cannot_reuse_a_namespace_revision_id() {
    let database = db();
    put(&database, 10, record("file", 7), WALL);
    rename_db(&database, 10, "renamed", 8, WALL + 1);
    let read = database.begin_read().unwrap();
    let birth = read.get_with_clock(&entry_key(id(10))).unwrap().unwrap();
    let name = read.get_with_clock(&meta_key(NAME, id(10))).unwrap().unwrap();
    let mut deletion = revision(&name.1).unwrap();
    deletion.change = EntryChange::Delete {};
    let bytes = deletion.canonical().unwrap();
    assert!(matches!(
        validate_deletion(
            id(10),
            (birth.0, &birth.1),
            [Some((name.0, &name.1)), None, None],
            Some((name.0, &bytes)),
        ),
        Err(Error::AccessDenied("entry revision ID reused across registers"))
    ));
}

#[test]
fn deletion_head_cannot_reuse_a_losing_history_revision() {
    let source = db();
    let recipient = db();
    receive(&recipient, &put(&source, 10, record("file", 7), WALL), 1);
    receive(&recipient, &rename_db(&source, 10, "first", 8, WALL + 1), 2);
    receive(&recipient, &rename_db(&source, 10, "second", 9, WALL + 2), 3);
    let mut tx = source.begin_write_with_delta(WALL + 3, 64 * 1024).unwrap();
    let mut deletion = revision(&tx.get(&revision_key(id(10), id(8))).unwrap().unwrap()).unwrap();
    deletion.origin_clock = tx.clock().as_u64();
    deletion.change = EntryChange::Delete {};
    tx.put(&meta_key(RETIRED, id(10)), &deletion.canonical().unwrap()).unwrap();
    let CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else { panic!() };
    let before = recipient.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        apply_delta(&recipient, &delta, WALL + 4, |_, _| Ok(())),
        Err(Error::AccessDenied("entry head differs from immutable history"))
    ));
    assert_eq!(before, recipient.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(snapshot(&recipient).lookup("second").unwrap().id, id(10));
}

#[test]
fn history_pages_merge_creation_with_lower_and_higher_revision_ids() {
    let database = db();
    put(&database, 10, record("notes", 5), WALL);
    for (name, revision) in [("work", 1), ("personal", 7), ("archive", 9)] {
        rename_db(&database, 10, name, revision, WALL + u64::from(revision) + 1000);
    }
    let index = snapshot(&database);
    let mut after = None;
    let mut actual = Vec::new();
    loop {
        let page = index.revisions(id(10), after, 1).unwrap();
        actual.extend(page.revisions.into_iter().map(|event| event.revision));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(actual, vec![id(1), id(5), id(7), id(9)]);
    assert_eq!(index.revisions(id(10), Some(id(9)), 64).unwrap().revisions.len(), 0);
}

#[test]
fn timestamp_history_pages_use_origin_clock_and_revision_ties() {
    let database = db();
    put(&database, 10, record("notes", 5), WALL);
    rename_db(&database, 10, "first", 9, WALL + 1);
    // Concurrent changes from the same cut have the same originating clock,
    // despite arriving in a different order.
    let left = db();
    let right = db();
    receive(&left, &put(&db(), 10, record("notes", 5), WALL), 17);
    receive(&right, &put(&db(), 10, record("notes", 5), WALL), 17);
    let low = rename_db(&left, 10, "low", 1, WALL + 2);
    let high = rename_db(&right, 10, "high", 7, WALL + 2);
    receive(&database, &high, 23);
    receive(&database, &low, 13);
    let index = snapshot(&database);
    for limit in [1, 2, 3, 64] {
        let mut after = None;
        let mut actual = Vec::new();
        loop {
            let page = index.history(id(10), after, limit).unwrap();
            assert!(page.revisions.len() <= limit);
            if let Some(next) = page.next {
                assert_eq!(Some(next), page.revisions.last().map(HistoryCursor::from));
            }
            actual.extend(page.revisions.iter().map(HistoryCursor::from));
            after = page.next;
            if after.is_none() {
                break;
            }
            assert!(actual.len() <= 4, "cursor must advance");
        }
        assert_eq!(
            actual.iter().map(|cursor| cursor.revision).collect::<Vec<_>>(),
            vec![id(7), id(1), id(9), id(5)]
        );
        assert_eq!(actual[0].origin_clock, actual[1].origin_clock);
        assert!(actual.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(index.history(id(10), actual.last().copied(), limit).unwrap().revisions.is_empty());
    }
    assert!(matches!(index.history(id(10), None, 0), Err(Error::LimitExceeded)));
    assert!(matches!(index.history(id(10), None, 65), Err(Error::LimitExceeded)));
}

#[test]
fn source_rejects_derived_forgery_erasure_noncanonical_and_mutable_revisions() {
    let clock = kvdb::Clock { time: 100, count: 1 };
    let bytes = record("notes", 1).canonical().unwrap();
    let present = EntState::Present { value: &bytes, clock };
    assert!(
        validate_source(&Delta {
            key: &name_key(None, "notes"),
            local: EntState::Missing,
            remote: present
        })
        .is_err()
    );
    for key in [
        entry_key(id(10)).to_vec(),
        meta_key(RETIRED, id(10)).to_vec(),
        revision_key(id(10), id(1)).to_vec(),
    ] {
        assert!(
            validate_source(&Delta {
                key: &key,
                local: present,
                remote: EntState::Deleted { clock }
            })
            .is_err()
        );
    }
    let mut padded = bytes.clone();
    padded.push(b' ');
    assert!(
        validate_source(&Delta {
            key: &entry_key(id(10)),
            local: present,
            remote: EntState::Present { value: &padded, clock }
        })
        .is_err()
    );
    let changed = record("work", 1).canonical().unwrap();
    assert!(
        validate_source(&Delta {
            key: &revision_key(id(10), id(1)),
            local: present,
            remote: EntState::Present { value: &changed, clock }
        })
        .is_err()
    );
}

#[test]
fn rename_never_replaces_current_permissions_and_owner_can_inspect_metadata() {
    use crate::store::object_contract::{Grant, Subject};
    let db = db();
    put(&db, 10, record("notes", 1), WALL);
    let owner = Id::from_bytes([9; 16]);
    let reader = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Test".into(),
        owner,
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(reader)),
            permissions: vec![Permission::Read],
        }],
    };
    snapshot(&db)
        .authorize(&volume, &defaults, Principal::Human(reader), id(10), Permission::Read)
        .unwrap();
    let mut tx = db.begin_write(WALL + 1).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Replace(Acl::default())).unwrap();
    tx.commit().unwrap();
    rename_db(&db, 10, "work", 2, WALL + 2);
    let index = snapshot(&db);
    assert!(
        index
            .authorize(&volume, &defaults, Principal::Human(reader), id(10), Permission::Read)
            .is_err()
    );
    assert!(
        index
            .authorize(&volume, &defaults, Principal::Human(owner), id(10), Permission::Read)
            .is_err()
    );
    assert_eq!(index.entry(id(10)).unwrap().info.name, "work");
}

#[test]
fn source_admission_uses_prior_permissions_and_read_only_recipient_can_repair() {
    use crate::store::object_contract::{Grant, Subject};
    let a = db();
    let b = db();
    let owner = Id::from_bytes([9; 16]);
    let writer = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Test".into(),
        owner,
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(writer)),
            permissions: vec![Permission::Read, Permission::Write],
        }],
    };
    let delta = put(&a, 10, record("notes", 1), WALL);
    let CommittedDelta::Bytes(bytes) = delta else { panic!() };
    apply_delta(&b, &bytes, WALL + 1, |prior, delta| {
        prior.authorize_source(&volume, &defaults, writer, delta)
    })
    .unwrap();

    let mut tx = a.begin_write_with_delta(WALL + 2, 64 * 1024).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Replace(Acl::default())).unwrap();
    let CommittedDelta::Bytes(revoke) = tx.commit_with_delta().unwrap() else { panic!() };
    apply_delta(&b, &revoke, WALL + 3, |prior, delta| {
        prior.authorize_source(&volume, &defaults, owner, delta)
    })
    .unwrap();
    let delta = rename_db(&a, 10, "work", 2, WALL + 4);
    let CommittedDelta::Bytes(rename_bytes) = delta else { panic!() };
    let before = b.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        apply_delta(&b, &rename_bytes, WALL + 5, |prior, delta| prior
            .authorize_source(&volume, &defaults, writer, delta)),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(snapshot(&b).lookup("notes").unwrap().id, id(10));

    // Receipt is authenticated and its source was admitted by the originating
    // authority. Repair needs no write grant for the receiving account.
    apply_delta(&b, &rename_bytes, WALL + 6, |_, _| Ok(())).unwrap();
    assert_eq!(snapshot(&b).lookup("work").unwrap().id, id(10));
    assert!(
        snapshot(&b)
            .authorize(&volume, &defaults, Principal::Human(writer), id(10), Permission::Read)
            .is_err()
    );

    // A source changeset cannot grant itself authority, even if its ACL appears
    // before its head on a transport with a different ordering.
    let mut tx = a.begin_write_with_delta(WALL + 7, 64 * 1024).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Inherit).unwrap();
    rename(
        &mut tx,
        id(10),
        "private",
        id(3),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    let CommittedDelta::Bytes(self_grant) = tx.commit_with_delta().unwrap() else { panic!() };
    let before = b.begin_read().unwrap().root_hash().unwrap();
    assert!(
        apply_delta(&b, &self_grant, WALL + 8, |prior, delta| prior
            .authorize_source(&volume, &defaults, writer, delta))
        .is_err()
    );
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert_invariants(&b);
}

#[test]
fn delegated_permission_sources_require_current_management_and_cannot_authorize_content() {
    // Pin the receiver's current policy: a manager can change only ACLs, never bootstrap or replay authority.
    use crate::store::object_contract::{Grant, Subject};
    let source = db();
    let target = db();
    let manager = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Delegated management".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(manager)),
            permissions: vec![Permission::Write],
        }],
    };
    for (entry, name) in [(10, "managed"), (11, "other")] {
        let CommittedDelta::Bytes(bytes) = put(&source, entry, record(name, entry), WALL) else {
            panic!()
        };
        apply_delta(&target, &bytes, WALL + 1, |prior, fact| {
            prior.authorize_source(&volume, &defaults, volume.owner, fact)
        })
        .unwrap();
    }
    let delegated = EntryAcl::Replace(Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(manager)),
            permissions: vec![Permission::ManagePermissions],
        }],
    });
    let permission_change = |entry, acl: &EntryAcl, wall| {
        let mut tx = source.begin_write_with_delta(wall, 64 * 1024).unwrap();
        set_permissions(&mut tx, id(entry), acl).unwrap();
        let CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else { panic!() };
        bytes
    };
    let grant = permission_change(10, &delegated, WALL + 2);
    let before = target.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        apply_delta(&target, &grant, WALL + 3, |prior, fact| {
            prior.authorize_source(&volume, &defaults, manager, fact)
        }),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, target.begin_read().unwrap().root_hash().unwrap());
    apply_delta(&target, &grant, WALL + 3, |prior, fact| {
        prior.authorize_source(&volume, &defaults, volume.owner, fact)
    })
    .unwrap();
    snapshot(&target).authorize_source_writer(&volume, &defaults, manager, id(10)).unwrap();

    let CommittedDelta::Bytes(rename) = rename_db(&source, 10, "unauthorized", 12, WALL + 4) else {
        panic!()
    };
    let before = target.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        apply_delta(&target, &rename, WALL + 5, |prior, fact| {
            prior.authorize_source(&volume, &defaults, manager, fact)
        }),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, target.begin_read().unwrap().root_hash().unwrap());

    let sibling = permission_change(11, &delegated, WALL + 6);
    assert!(matches!(
        apply_delta(&target, &sibling, WALL + 7, |prior, fact| {
            prior.authorize_source(&volume, &defaults, manager, fact)
        }),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, target.begin_read().unwrap().root_hash().unwrap());

    let mut edited = delegated.clone();
    let EntryAcl::Replace(acl) = &mut edited else { panic!() };
    acl.grants.push(Grant { subject: Subject::Public, permissions: vec![Permission::Read] });
    let edit = permission_change(10, &edited, WALL + 8);
    apply_delta(&target, &edit, WALL + 9, |prior, fact| {
        prior.authorize_source(&volume, &defaults, manager, fact)
    })
    .unwrap();
    assert_eq!(snapshot(&target).permissions(id(10)).unwrap(), edited);

    let revoke = permission_change(10, &EntryAcl::Replace(Acl::default()), WALL + 10);
    apply_delta(&target, &revoke, WALL + 11, |prior, fact| {
        prior.authorize_source(&volume, &defaults, volume.owner, fact)
    })
    .unwrap();
    assert!(
        snapshot(&target).authorize_source_writer(&volume, &defaults, manager, id(10)).is_err()
    );
    let stale = permission_change(10, &delegated, WALL + 12);
    let before = target.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        apply_delta(&target, &stale, WALL + 13, |prior, fact| {
            prior.authorize_source(&volume, &defaults, manager, fact)
        }),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, target.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(snapshot(&target).permissions(id(10)).unwrap(), EntryAcl::Replace(Acl::default()));
}

#[test]
fn volume_defaults_cannot_authorize_an_acl_override_before_its_restricted_birth() {
    // Deliver a forged override before the owner birth: defaults must never bootstrap entry management.
    use crate::store::object_contract::{Grant, Subject};
    let source = db();
    let target = db();
    let manager = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Delayed restricted birth".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(manager)),
            permissions: vec![
                Permission::Read,
                Permission::Write,
                Permission::Append,
                Permission::ManagePermissions,
            ],
        }],
    };
    let mut initial = record("restricted", 1);
    initial.initial_permissions = EntryAcl::Replace(Acl::default());
    let CommittedDelta::Bytes(birth) = put(&source, 10, initial, WALL) else { panic!() };
    let mut tx = source.begin_write_with_delta(WALL + 1, 64 * 1024).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Replace(defaults.clone())).unwrap();
    let CommittedDelta::Bytes(forged) = tx.commit_with_delta().unwrap() else { panic!() };
    let before = target.begin_read().unwrap().root_hash().unwrap();
    assert!(
        apply_delta(&target, &forged, WALL + 2, |prior, fact| {
            prior.authorize_source(&volume, &defaults, manager, fact)
        })
        .is_err()
    );
    assert_eq!(before, target.begin_read().unwrap().root_hash().unwrap());
    apply_delta(&target, &birth, WALL + 3, |prior, fact| {
        prior.authorize_source(&volume, &defaults, volume.owner, fact)
    })
    .unwrap();
    assert_eq!(snapshot(&target).permissions(id(10)).unwrap(), EntryAcl::Replace(Acl::default()));
    assert!(
        snapshot(&target)
            .authorize(&volume, &defaults, Principal::Human(manager), id(10), Permission::Read)
            .is_err()
    );
    let before = target.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        apply_delta(&target, &forged, WALL + 4, |prior, fact| {
            prior.authorize_source(&volume, &defaults, manager, fact)
        }),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, target.begin_read().unwrap().root_hash().unwrap());
}

#[test]
fn permission_publication_receipts_bind_only_one_exact_local_register() {
    // Vary every receipt binding, then copy source facts to prove receipt authority never arrives remotely.
    let source = db();
    let reflected = db();
    let manager = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Exact ACL publication".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl::default();
    put(&source, 10, record("selected", 1), WALL);
    put(&source, 11, record("sibling", 2), WALL);
    let permissions = EntryAcl::Replace(Acl::default());
    let mut tx = source.begin_write(WALL + 1).unwrap();
    set_permissions(&mut tx, id(10), &permissions).unwrap();
    retain_permission_publication(&mut tx, id(10), manager).unwrap();
    set_permissions(&mut tx, id(11), &permissions).unwrap();
    tx.commit().unwrap();
    let key = permission_source_key(id(10));
    let read = source.begin_read().unwrap();
    let (clock, value) = read.get_with_clock(&key).unwrap().unwrap();
    let bytes = value.to_vec();
    let remote = EntState::Present { clock, value: &bytes };
    let receipt = read.local_get(&permission_publication_key(id(10))).unwrap().unwrap().to_vec();
    assert_eq!(receipt.len(), PERMISSION_PUBLICATION_BYTES);
    assert!(read.iter().all(|row| !row.unwrap().0.starts_with(PERMISSION_PUBLICATION_PREFIX)));
    drop(read);
    snapshot(&source)
        .authorize_committed_source(&volume, &defaults, manager, &key, remote)
        .unwrap();
    snapshot(&source)
        .authorize_source(&volume, &defaults, manager, &Delta { key: &key, local: remote, remote })
        .unwrap();
    assert!(
        snapshot(&source).authorize_source_writer(&volume, &defaults, manager, id(10)).is_err()
    );
    snapshot(&source).authorize_source_writer(&volume, &defaults, volume.owner, id(10)).unwrap();
    assert!(
        snapshot(&source)
            .authorize_committed_source(&volume, &defaults, Id::from_bytes([7; 16]), &key, remote)
            .is_err()
    );
    for other_key in [permission_source_key(id(11)).to_vec(), entry_key(id(10)).to_vec()] {
        let read = source.begin_read().unwrap();
        let (clock, bytes) = read.get_with_clock(&other_key).unwrap().unwrap();
        assert!(
            snapshot(&source)
                .authorize_committed_source(
                    &volume,
                    &defaults,
                    manager,
                    &other_key,
                    EntState::Present { clock, value: &bytes },
                )
                .is_err()
        );
    }
    #[cfg(feature = "connected-engine")]
    {
        let local = snapshot(&source);
        let target = snapshot(&reflected);
        assert!(local.permission_restoration_delta(manager, id(10), &target).unwrap().is_some());
        assert!(
            local.permission_restoration_delta(volume.owner, id(10), &target).unwrap().is_none()
        );
        assert!(local.permission_restoration_delta(manager, id(11), &target).unwrap().is_none());
        assert!(local.permission_restoration_delta(manager, id(10), &local).unwrap().is_none());
        drop(target);
        put(&reflected, 10, record("selected", 1), WALL);
        let mut newer = reflected.begin_write(WALL + 2).unwrap();
        set_permissions(&mut newer, id(10), &permissions).unwrap();
        newer.commit().unwrap();
        assert!(
            local
                .permission_restoration_delta(manager, id(10), &snapshot(&reflected))
                .unwrap()
                .is_none()
        );
    }
    let changed = encode(&EntryAcl::Inherit).unwrap();
    assert!(
        snapshot(&source)
            .authorize_source(
                &volume,
                &defaults,
                manager,
                &Delta {
                    key: &key,
                    local: remote,
                    remote: EntState::Present { clock, value: &changed },
                },
            )
            .is_err(),
        "an identical ACL replay admits no new grant"
    );
    assert!(
        snapshot(&source)
            .authorize_committed_source(
                &volume,
                &defaults,
                manager,
                &key,
                EntState::Present { clock, value: &changed },
            )
            .is_err()
    );
    for offset in [4, 20, 28] {
        let mut altered = receipt.clone();
        altered[offset] ^= 1;
        let mut tx = source.begin_write(WALL + 2).unwrap();
        tx.local_indexes().put(&permission_publication_key(id(10)), &altered).unwrap();
        tx.commit().unwrap();
        assert!(
            snapshot(&source)
                .authorize_committed_source(&volume, &defaults, manager, &key, remote)
                .is_err()
        );
    }
    for malformed in [Vec::new(), vec![0; PERMISSION_PUBLICATION_BYTES]] {
        let mut tx = source.begin_write(WALL + 2).unwrap();
        tx.local_indexes().put(&permission_publication_key(id(10)), &malformed).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            snapshot(&source).permission_publication(manager, id(10)),
            Err(Error::Corrupt(_))
        ));
    }
    let mut tx = source.begin_write(WALL + 3).unwrap();
    tx.local_indexes().put(&permission_publication_key(id(10)), &receipt).unwrap();
    tx.commit().unwrap();
    replicate(&source, &reflected, false);
    assert!(!snapshot(&reflected).permission_publication(manager, id(10)).unwrap());
    assert!(
        snapshot(&reflected)
            .authorize_committed_source(&volume, &defaults, manager, &key, remote)
            .is_err()
    );
    let mut tx = source.begin_write(WALL + 4).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Inherit).unwrap();
    tx.commit().unwrap();
    assert!(!snapshot(&source).permission_publication(manager, id(10)).unwrap());
    assert!(
        snapshot(&source)
            .authorize_committed_source(&volume, &defaults, manager, &key, remote)
            .is_err()
    );
}

#[test]
fn initial_override_is_authoritative_in_every_partial_replica_snapshot() {
    use crate::store::object_contract::{Grant, Subject};
    let a = db();
    let b = db();
    let owner = Id::from_bytes([9; 16]);
    let writer = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Test".into(),
        owner,
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Public,
            permissions: vec![Permission::Read, Permission::Write, Permission::Append],
        }],
    };
    let mut initial = record("private-recovery", 1);
    initial.initial_permissions = EntryAcl::Replace(Acl::default());
    let CommittedDelta::Bytes(birth) = put(&a, 10, initial, WALL) else { panic!() };
    assert!(matches!(
        apply_delta(&b, &birth, WALL + 1, |prior, delta| prior
            .authorize_source(&volume, &defaults, writer, delta)),
        Err(Error::AccessDenied(_))
    ));
    apply_delta(&b, &birth, WALL + 2, |prior, delta| {
        prior.authorize_source(&volume, &defaults, owner, delta)
    })
    .unwrap();
    let before = b.begin_read().unwrap().root_hash().unwrap();
    let clock = b.clock();
    assert!(matches!(
        apply_delta(&b, &birth, WALL + 2, |prior, delta| prior
            .authorize_source(&volume, &defaults, owner, delta)),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(clock, b.clock());
    // Authenticated catalog receipt retains the admitted declaration; it does
    // not grant its recipient content Write or reauthorize its originating writer.
    for _ in 0..2 {
        apply_delta(&b, &birth, WALL + 2, |_, _| Ok(())).unwrap();
        for account in [writer, owner] {
            assert!(
                snapshot(&b)
                    .authorize(
                        &volume,
                        &defaults,
                        Principal::Human(account),
                        id(10),
                        Permission::Read
                    )
                    .is_err()
            );
        }
        assert_eq!(snapshot(&b).lookup("private-recovery").unwrap().id, id(10));
    }
    let mut tx = a.begin_write_with_delta(WALL + 3, 65536).unwrap();
    set_permissions(&mut tx, id(10), &EntryAcl::Inherit).unwrap();
    let CommittedDelta::Bytes(grant) = tx.commit_with_delta().unwrap() else { panic!() };
    apply_delta(&b, &grant, WALL + 4, |prior, delta| {
        prior.authorize_source(&volume, &defaults, owner, delta)
    })
    .unwrap();
    snapshot(&b)
        .authorize(&volume, &defaults, Principal::Human(writer), id(10), Permission::Read)
        .unwrap();
    // Duplicate/delayed birth cannot replace the later explicit override.
    apply_delta(&b, &birth, WALL + 5, |prior, delta| {
        prior.authorize_source(&volume, &defaults, owner, delta)
    })
    .unwrap();
    snapshot(&b)
        .authorize(&volume, &defaults, Principal::Human(writer), id(10), Permission::Read)
        .unwrap();
}

#[test]
fn append_authority_cannot_rename_delete_or_add_conflicting_history() {
    use crate::store::object_contract::{Grant, Subject};
    let a = db();
    let b = db();
    let appender = Id::from_bytes([8; 16]);
    let volume = Volume {
        id: Id::from_bytes([1; 16]),
        name: "Append".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let defaults = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(appender)),
            permissions: vec![Permission::Append],
        }],
    };
    let CommittedDelta::Bytes(create) = put(&a, 10, record("notes", 1), WALL) else { panic!() };
    apply_delta(&b, &create, WALL + 1, |prior, delta| {
        prior.authorize_source(&volume, &defaults, appender, delta)
    })
    .unwrap();
    let before = b.begin_read().unwrap().root_hash().unwrap();
    let clock = b.clock();
    let (replayed, _) = apply_delta(&b, &create, WALL + 1, |prior, delta| {
        prior.authorize_source(&volume, &defaults, appender, delta)
    })
    .unwrap();
    assert_eq!(replayed.applied, 0);
    assert_eq!(before, b.begin_read().unwrap().root_hash().unwrap());
    assert_eq!(clock, b.clock());
    assert!(matches!(
        apply_delta(&b, &create, WALL + 1, |prior, delta| prior.authorize_source(
            &volume,
            &Acl::default(),
            appender,
            delta
        )),
        Err(Error::AccessDenied(_))
    ));
    let CommittedDelta::Bytes(rename) = rename_db(&a, 10, "work", 2, WALL + 2) else { panic!() };
    assert!(
        apply_delta(&b, &rename, WALL + 3, |prior, delta| prior
            .authorize_source(&volume, &defaults, appender, delta))
        .is_err()
    );
    let CommittedDelta::Bytes(deletion) = delete(&a, 10, WALL + 4) else { panic!() };
    assert!(
        apply_delta(&b, &deletion, WALL + 5, |prior, delta| prior
            .authorize_source(&volume, &defaults, appender, delta))
        .is_err()
    );
    assert_eq!(snapshot(&b).lookup("notes").unwrap().id, id(10));
    assert!(snapshot(&b).revision(id(10), id(2)).is_err());
    apply_delta(&b, &deletion, WALL + 6, |_, _| Ok(())).unwrap();
    assert!(matches!(
        apply_delta(&b, &create, WALL + 7, |prior, delta| prior
            .authorize_source(&volume, &defaults, appender, delta)),
        Err(Error::AccessDenied(_))
    ));
    assert_invariants(&b);
}

#[test]
fn candidate_projection_matches_regression_cases() {
    let name = |value| match value {
        1 => "notes",
        2 => "work",
        _ => panic!("missing head"),
    };
    for line in include_str!("../../../tests/fixtures/volume-index.txt").lines() {
        let values: Vec<u8> = line.split_whitespace().map(|word| word.parse().unwrap()).collect();
        assert_eq!(values.len(), 8);
        let db = db();
        let mut tx = db.begin_write(WALL).unwrap();
        let birth_clock = tx.clock();
        let mut ids = Vec::new();
        for fields in [&values[..3], &values[3..6]] {
            let entry = id(fields[0]);
            ids.push(entry);
            if fields[1] != 0 {
                tx.put(&entry_key(entry), &record(name(fields[1]), fields[0]).canonical().unwrap())
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        let mut tx = db.begin_write(WALL + 1).unwrap();
        for fields in [&values[..3], &values[3..6]] {
            if fields[2] != 0 {
                let entry = id(fields[0]);
                let bytes = record(name(fields[1].max(1)), fields[0]).canonical().unwrap();
                let birth =
                    namespace::source::Declaration::decode(entry, birth_clock, &bytes).unwrap();
                let deletion = EntryRevision {
                    revision: id(250),
                    creation: creation_digest(entry, birth_clock, &bytes),
                    origin_clock: tx.clock().as_u64(),
                    origin: record("origin", 1).origin,
                    observed: vec![birth.witness().id().unwrap()],
                    change: EntryChange::Delete {},
                };
                let bytes = deletion.canonical().unwrap();
                tx.put(&meta_key(RETIRED, entry), &bytes).unwrap();
                tx.put(&revision_key(entry, deletion.revision), &bytes).unwrap();
            }
        }
        repair(&mut tx.local_indexes(), &ids).unwrap();
        namespace::source::cut::project(&tx)
            .unwrap()
            .write_current(&mut tx.local_indexes())
            .unwrap();
        tx.commit().unwrap();
        let actual = snapshot(&db).lookup(name(values[6]));
        if values[7] == 0 {
            assert!(matches!(actual, Err(Error::NotFound(_))), "{line}");
        } else {
            assert_eq!(actual.unwrap().id, id(values[7]), "{line}");
        }
    }
}

#[test]
fn independent_fields_match_regression_cases() {
    for line in include_str!("../../../tests/fixtures/volume-fields.txt").lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 4);
        let declared = fields[0] == "1";
        let base: u8 = fields[1].parse().unwrap();
        let later: Option<u8> =
            if fields[2] == "-" { None } else { Some(fields[2].parse().unwrap()) };
        let expected: u8 = fields[3].parse().unwrap();
        for change_name in [false, true] {
            let a = db();
            let b = db();
            let blob = |value| EntryKind::Blob {
                len_bytes: u64::from(value),
                digest: [value; 32],
                encrypted_key: [value; 32],
                published_at_micros: i64::from(value),
            };
            let initial = EntryInfo {
                namespace: Default::default(),
                origin: crate::store::volume_index::namespace::Origin {
                    installation: crate::store::Id::from_bytes([42; 16]),
                    label: None,
                },
                initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                created_revision: id(9),
                name: format!("name{base}"),
                kind: blob(base),
                protocol: None,
            };
            let creation = put(&a, 10, initial, WALL);
            let mut history = None;
            if let Some(later) = later {
                let source_key = if change_name {
                    rename_db(&a, 10, &format!("name{later}"), 1, WALL + 1000);
                    meta_key(NAME, id(10))
                } else {
                    content_db(&a, 10, blob(later), 2, WALL + 1000);
                    meta_key(CONTENT, id(10))
                };
                let delta =
                    a.begin_read().unwrap().delta_for_key(&source_key, 64 * 1024).unwrap().unwrap();
                receive(&b, &delta, 1);
                history = a
                    .begin_read()
                    .unwrap()
                    .delta_for_key(
                        &revision_key(id(10), id(if change_name { 1 } else { 2 })),
                        64 * 1024,
                    )
                    .unwrap();
            }
            if declared {
                receive(&b, &creation, 17);
            }
            // A raw head is staged data until its immutable history is present.
            if declared {
                let current = snapshot(&b).entry(id(10)).unwrap();
                assert_eq!(current.info.name, format!("name{base}"), "{line}");
                assert_eq!(current.info.kind, blob(base), "{line}");
            }
            if let Some(history) = history {
                receive(&b, &history, 1);
            }
            let actual = snapshot(&b).entry(id(10));
            if expected == 0 {
                assert!(matches!(actual, Err(Error::NotFound(_))), "{line}");
            } else {
                let actual = actual.unwrap();
                let (name, content) = if change_name { (expected, base) } else { (base, expected) };
                assert_eq!(actual.info.name, format!("name{name}"), "{line}");
                assert_eq!(actual.info.kind, blob(content), "{line}");
            }
            assert_invariants(&b);
        }
    }
}

#[test]
fn database_protocol_is_explicit_and_immutable() {
    let database = db();
    let mut log = record("events", 1);
    log.protocol = Some(DatabaseProtocol::BlobLog);
    put(&database, 10, log.clone(), WALL);
    let mut changed = log.clone();
    changed.protocol = Some(DatabaseProtocol::Kvdb);
    changed.created_revision = id(2);
    let mut tx = database.begin_write(WALL + 1).unwrap();
    assert!(matches!(create(&mut tx, id(10), &changed), Err(Error::AlreadyExists(_))));
    drop(tx);
    log.protocol = None;
    assert!(log.validate().is_err());
    log.kind = EntryKind::ObjectDb { schema: Schema::default() };
    log.protocol = Some(DatabaseProtocol::BlobLog);
    assert!(log.validate().is_err());
}

#[test]
fn local_namespace_sources_and_observations_share_the_source_transaction() {
    // Exercise actual birth/rename publication, including rollback: source IDs
    // and causal knowledge must commit together without confirming local work.
    use namespace::{EventState, observations::Ledger, source::Declaration};
    let database = db();
    let mut tx = database.begin_write(WALL).unwrap();
    create(&mut tx, id(10), &record("first", 1)).unwrap();
    create(&mut tx, id(11), &record("second", 2)).unwrap();
    tx.commit().unwrap();
    let read = database.begin_read().unwrap();
    let first = {
        let (clock, bytes) = read.get_with_clock(&entry_key(id(10))).unwrap().unwrap();
        Declaration::decode(id(10), clock, &bytes).unwrap()
    };
    let second = {
        let (clock, bytes) = read.get_with_clock(&entry_key(id(11))).unwrap().unwrap();
        Declaration::decode(id(11), clock, &bytes).unwrap().birth().unwrap()
    };
    let first_birth = first.birth().unwrap();
    assert_eq!(second.fact.observed.iter().copied().collect::<Vec<_>>(), [first_birth.fact.id]);
    drop(read);
    let root = database.begin_read().unwrap().root_hash().unwrap();
    {
        let mut tx = database.begin_write(WALL + 1).unwrap();
        rename(&mut tx, id(10), "aborted", id(3), &record("unused", 4).origin).unwrap();
        assert_eq!(Ledger::load(&tx.local_indexes()).unwrap().context().unwrap().len(), 3);
    }
    assert_eq!(database.begin_read().unwrap().root_hash().unwrap(), root);
    let mut tx = database.begin_write(WALL + 2).unwrap();
    assert_eq!(Ledger::load(&tx.local_indexes()).unwrap().context().unwrap().len(), 2);
    rename(&mut tx, id(10), "renamed", id(5), &record("unused", 4).origin).unwrap();
    tx.commit().unwrap();
    let read = database.begin_read().unwrap();
    let (clock, bytes) = read.get_with_clock(&meta_key(NAME, id(10))).unwrap().unwrap();
    let edited = first.revision(clock, &bytes).unwrap().unwrap();
    assert_eq!(&*bytes, &*read.get(&revision_key(id(10), id(5))).unwrap().unwrap());
    let admitted = namespace::admit(&[first_birth.fact, second.fact, edited.fact]).unwrap();
    assert!(admitted.events.values().all(|state| *state == EventState::Accepted));
    assert_eq!(admitted.view.entries[&id(10)].name, "renamed");
}

#[test]
fn namespace_edit_clock_rejection_precedes_source_writes() {
    // One transaction can create a chain of births. A following edit must use
    // a later clock; rejecting it must leave existing staged records unchanged.
    let database = db();
    let mut tx = database.begin_write(WALL).unwrap();
    create(&mut tx, id(10), &record("first", 1)).unwrap();
    tx.commit().unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    rename(&mut tx, id(10), "accepted", id(2), &record("unused", 3).origin).unwrap();
    assert!(matches!(
        rename(&mut tx, id(10), "rejected", id(4), &record("unused", 3).origin),
        Err(Error::InvalidInput(_))
    ));
    assert!(tx.get(&revision_key(id(10), id(4))).unwrap().is_none());
    assert!(matches!(
        create(&mut tx, id(11), &record("too early", 5)),
        Err(Error::InvalidInput(_))
    ));
    assert!(tx.get(&entry_key(id(11))).unwrap().is_none());
    tx.commit().unwrap();
    assert_eq!(snapshot(&database).entry(id(10)).unwrap().info.name, "accepted");
    let mut tx = database.begin_write(WALL + 2).unwrap();
    rename(&mut tx, id(10), "next transaction", id(6), &record("unused", 3).origin).unwrap();
    tx.commit().unwrap();
}

#[test]
fn one_thousand_publications_fit_persisted_causal_source_budgets() {
    // Use real source transactions and a final full-ledger reload. Compact
    // source bytes and local bookkeeping must both support the required history.
    let database = db();
    let entry = id(10);
    let mut tx = database.begin_write(WALL).unwrap();
    create(&mut tx, entry, &record("first", 1)).unwrap();
    tx.commit().unwrap();
    let origin = record("unused", 2).origin;
    for number in 1..=1000u64 {
        let mut bytes = [9; 15];
        bytes[7..].copy_from_slice(&number.to_be_bytes());
        let revision = EntryId::from_bytes(bytes).unwrap();
        let mut tx = database.begin_write(WALL + number).unwrap();
        rename(&mut tx, entry, &format!("file-{number}"), revision, &origin).unwrap();
        tx.commit().unwrap();
    }
    let mut tx = database.begin_write(WALL + 1001).unwrap();
    let ledger = namespace::observations::Ledger::load(&tx.local_indexes()).unwrap();
    assert_eq!(ledger.context().unwrap().len(), 1001);
    drop(tx);
    let read = database.begin_read().unwrap();
    let (clock, bytes) = read.get_with_clock(&entry_key(entry)).unwrap().unwrap();
    let declaration = namespace::source::Declaration::decode(entry, clock, &bytes).unwrap();
    let mut facts = vec![declaration.birth().unwrap().fact];
    let mut source_bytes = bytes.len();
    let prefix = meta_key(REVISION, entry);
    for item in read.iter_from(&prefix) {
        let (key, bytes) = item.unwrap();
        if !key.starts_with(&prefix) {
            break;
        }
        source_bytes += bytes.len();
        let (clock, _) = read.get_with_clock(&key).unwrap().unwrap();
        facts.push(declaration.revision(clock, &bytes).unwrap().unwrap().fact);
    }
    assert!(source_bytes < 16 * 1024 * 1024);
    let admitted = namespace::admit(&facts).unwrap();
    assert_eq!(admitted.events.len(), 1001);
    assert!(admitted.events.values().all(|state| *state == namespace::EventState::Accepted));
    assert_eq!(admitted.view.entries[&entry].name, "file-1000");
    drop(read);
    let mut tx = database.begin_write(WALL + 1002).unwrap();
    create_at_path(&mut tx, id(11), "new/child", &record("child", 2)).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&database).lookup("new/child").unwrap().id, id(11));
    admit_sources(&database);
}

#[test]
fn paths_create_durable_parents_and_rename_keeps_descendant_identity() {
    let db = db();
    let mut tx = db.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), "work/notes", &record("notes", 1)).unwrap();
    create_at_path(&mut tx, id(11), "work/tasks", &record("tasks", 2)).unwrap();
    create_at_path(&mut tx, id(12), "personal/notes", &record("notes", 3)).unwrap();
    tx.commit().unwrap();
    let before = snapshot(&db);
    let directory = before.lookup("work").unwrap();
    assert!(matches!(directory.info.kind, EntryKind::Directory {}));
    assert!(directory.info.namespace.automatic);
    let notes = before.lookup("work/notes").unwrap();
    assert_eq!(notes.info.name, "notes");
    assert_eq!(notes.info.namespace.parent, Some(directory.id));
    assert_eq!(before.lookup("work/tasks").unwrap().info.namespace.parent, Some(directory.id));
    assert_eq!(before.lookup("personal/notes").unwrap().id, id(12));
    let mut tx = db.begin_write(WALL + 1).unwrap();
    rename(&mut tx, directory.id, "archive", id(4), &directory.info.origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&db).lookup("archive/notes").unwrap().id, id(10));
    assert_eq!(before.lookup("work/notes").unwrap().id, id(10));
    assert!(snapshot(&db).lookup("work/notes").is_err());
    let mut tx = db.begin_write(WALL + 2).unwrap();
    assert!(matches!(
        create_at_path(&mut tx, id(13), "archive/notes/child", &record("child", 5)),
        Err(Error::InvalidInput(_))
    ));
    drop(tx);
    let mut tx = db.begin_write(WALL + 3).unwrap();
    create_at_path(&mut tx, id(14), "discarded/deep/leaf", &record("leaf", 6)).unwrap();
    drop(tx);
    assert!(snapshot(&db).lookup("discarded").is_err());
    db.verify().unwrap();
}

#[test]
fn automatic_directory_recreation_records_observed_departure() {
    let db = db();
    let mut tx = db.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), "work/a", &record("a", 1)).unwrap();
    tx.commit().unwrap();
    let directory = snapshot(&db).lookup("work").unwrap();
    let mut tx = db.begin_write(WALL + 1).unwrap();
    rename(&mut tx, directory.id, "archive", id(2), &directory.info.origin).unwrap();
    tx.commit().unwrap();
    let mut tx = db.begin_write(WALL + 2).unwrap();
    create_at_path(&mut tx, id(11), "work/b", &record("b", 3)).unwrap();
    tx.commit().unwrap();
    let current = snapshot(&db);
    let fresh = current.lookup("work").unwrap();
    assert_ne!(fresh.id, directory.id);
    assert_eq!(fresh.info.namespace.departures.len(), 1);
    assert_eq!(current.lookup("archive/a").unwrap().id, id(10));
    assert_eq!(current.lookup("work/b").unwrap().id, id(11));
    admit_sources(&db);
}

#[test]
fn moves_preserve_independent_names_and_omit_unchanged_assignments() {
    let a = db();
    let b = db();
    let mut tx = a.begin_write_with_delta(WALL, 1024 * 1024).unwrap();
    create_at_path(&mut tx, id(10), "work/note", &record("note", 1)).unwrap();
    let mut directory = record("archive", 2);
    directory.kind = EntryKind::Directory {};
    directory.protocol = None;
    create(&mut tx, id(11), &directory).unwrap();
    let initial = tx.commit_with_delta().unwrap();
    receive(&b, &initial, 1);
    let origin = directory.origin;
    let mut tx = a.begin_write(WALL + 10).unwrap();
    move_entry(&mut tx, id(10), Some(id(11)), Some("note"), id(3), &origin).unwrap();
    tx.commit().unwrap();
    rename_db(&b, 10, "renamed", 4, WALL + 11);
    replicate(&a, &b, false);
    replicate(&b, &a, true);
    for database in [&a, &b] {
        assert_eq!(snapshot(database).lookup("archive/renamed").unwrap().id, id(10));
    }
    let mut tx = a.begin_write(WALL + 20).unwrap();
    move_entry(&mut tx, id(10), Some(id(11)), Some("final"), id(5), &origin).unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        snapshot(&a).revision(id(10), id(5)).unwrap().change,
        EntryChange::Name { .. }
    ));
    let mut tx = a.begin_write(WALL + 21).unwrap();
    move_entry(&mut tx, id(10), Some(id(11)), Some("final"), id(6), &origin).unwrap();
    tx.commit().unwrap();
    assert!(snapshot(&a).revision(id(10), id(6)).is_err());
    let mut tx = a.begin_write(WALL + 22).unwrap();
    move_entry(&mut tx, id(10), None, None, id(7), &origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&a).lookup("final").unwrap().id, id(10));
    admit_sources(&a);
}

#[test]
fn directory_move_cannot_exceed_descendant_depth() {
    let database = db();
    let path = vec!["a"; namespace::MAX_DEPTH].join("/");
    let mut tx = database.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), &path, &record("a", 1)).unwrap();
    let mut directory = record("other", 2);
    directory.kind = EntryKind::Directory {};
    directory.protocol = None;
    create(&mut tx, id(11), &directory).unwrap();
    tx.commit().unwrap();
    let root = snapshot(&database).lookup("a").unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    assert!(matches!(
        move_entry(&mut tx, root.id, Some(id(11)), None, id(3), &directory.origin),
        Err(Error::LimitExceeded)
    ));
    drop(tx);
    assert_eq!(snapshot(&database).lookup(&path).unwrap().id, id(10));
}

fn admit_sources(database: &Db<MemStorage>) {
    let tx = database.begin_read().unwrap();
    let mut facts = Vec::new();
    for item in tx.iter_from(&[ENTRY_ID_BYTE]) {
        let (key, bytes) = item.unwrap();
        if key.first() != Some(&ENTRY_ID_BYTE) {
            break;
        }
        let entry = id_bytes(&key[1..]).unwrap();
        let (clock, _) = tx.get_with_clock(&key).unwrap().unwrap();
        let source = namespace::source::Declaration::decode(entry, clock, &bytes).unwrap();
        facts.push(source.birth().unwrap().fact);
        let prefix = meta_key(REVISION, entry);
        for item in tx.iter_from(&prefix) {
            let (key, bytes) = item.unwrap();
            if !key.starts_with(&prefix) {
                break;
            }
            let (clock, _) = tx.get_with_clock(&key).unwrap().unwrap();
            if let Some(decoded) = source.revision(clock, &bytes).unwrap() {
                facts.push(decoded.fact);
            }
        }
    }
    let admitted = namespace::admit(&facts).unwrap();
    assert!(admitted.events.values().all(|state| *state == namespace::EventState::Accepted));
}

#[test]
fn unrelated_retired_ancestry_does_not_block_directory_rename() {
    let database = db();
    let mut tx = database.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), "A/a", &record("a", 1)).unwrap();
    create_at_path(&mut tx, id(11), "B/b", &record("b", 2)).unwrap();
    tx.commit().unwrap();
    let a = snapshot(&database).lookup("A").unwrap();
    let b = snapshot(&database).lookup("B").unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    retire(&mut tx, a.id, id(250), &a.info.origin).unwrap();
    tx.commit().unwrap();
    let mut tx = database.begin_write(WALL + 2).unwrap();
    rename(&mut tx, b.id, "C", id(3), &b.info.origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&database).lookup("C/b").unwrap().id, id(11));
}

#[test]
fn parent_register_allows_only_identical_shared_move_revisions() {
    let database = db();
    let mut tx = database.begin_write(WALL).unwrap();
    let mut directory = record("folder", 1);
    directory.kind = EntryKind::Directory {};
    directory.protocol = None;
    create(&mut tx, id(10), &directory).unwrap();
    let mut file = record("file", 2);
    file.kind = EntryKind::Blob {
        len_bytes: 0,
        digest: [1; 32],
        encrypted_key: [2; 32],
        published_at_micros: 1,
    };
    file.protocol = None;
    create(&mut tx, id(11), &file).unwrap();
    tx.commit().unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    move_entry(&mut tx, id(11), Some(id(10)), Some("moved"), id(3), &file.origin).unwrap();
    tx.commit().unwrap();
    assert_eq!(snapshot(&database).lookup("folder/moved").unwrap().id, id(11));
    let tx = database.begin_read().unwrap();
    let (birth_clock, birth) = tx.get_with_clock(&entry_key(id(11))).unwrap().unwrap();
    let (clock, parent) = tx.get_with_clock(&meta_key(PARENT, id(11))).unwrap().unwrap();
    let mut update = revision(&parent).unwrap();
    update.change = EntryChange::Name { name: "forged".into() };
    let name = update.canonical().unwrap();
    assert!(matches!(
        assemble(id(11), (birth_clock, &birth), Some((clock, &name)), None, Some((clock, &parent))),
        Err(Error::AccessDenied("entry revision ID reused across registers"))
    ));
    update.change = EntryChange::Content { kind: file.kind };
    let content = update.canonical().unwrap();
    assert!(matches!(
        assemble(
            id(11),
            (birth_clock, &birth),
            None,
            Some((clock, &content)),
            Some((clock, &parent))
        ),
        Err(Error::AccessDenied("entry revision ID reused across registers"))
    ));
}

#[test]
fn directory_at_maximum_depth_remains_openable() {
    let database = db();
    let path = vec!["a"; namespace::MAX_DEPTH].join("/");
    let mut directory = record("a", 1);
    directory.kind = EntryKind::Directory {};
    directory.protocol = None;
    let mut tx = database.begin_write(WALL).unwrap();
    create_at_path(&mut tx, id(10), &path, &directory).unwrap();
    tx.commit().unwrap();
    let snapshot = snapshot(&database);
    assert_eq!(snapshot.lookup(&path).unwrap().id, id(10));
    snapshot.directory(Some(id(10))).unwrap();
    assert!(snapshot.children(Some(id(10))).unwrap().is_empty());
}

#[path = "projection_tests.rs"]
mod projection_tests;

#[test]
fn exact_blob_validation_accepts_delivered_content_without_history() {
    // Selected Write replicas retain a delivered content head without history.
    // Exact validation must preserve upload admission without granting Read.
    use crate::store::object_contract::{Grant, Subject};
    let db = db();
    let mut initial = record("file", 1);
    initial.protocol = None;
    initial.kind = EntryKind::Blob {
        len_bytes: 1,
        digest: [1; 32],
        encrypted_key: [2; 32],
        published_at_micros: 0,
    };
    put(&db, 10, initial, WALL);
    let mut tx = db.begin_write(WALL + 1).unwrap();
    let kind = EntryKind::Blob {
        len_bytes: 2,
        digest: [3; 32],
        encrypted_key: [4; 32],
        published_at_micros: 0,
    };
    let (record, _) = change_event(
        &tx,
        id(10),
        id(2),
        EntryChange::Content { kind },
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.put(&meta_key(CONTENT, id(10)), &record.canonical().unwrap()).unwrap();
    tx.commit().unwrap();
    let index = snapshot(&db);
    let volume = Volume {
        id: Id::from_bytes([8; 16]),
        name: "test".into(),
        owner: Id::from_bytes([9; 16]),
        personal: true,
        share_key_with_service: true,
    };
    let account = Id::from_bytes([7; 16]);
    let defaults = Acl {
        grants: vec![Grant { subject: Subject::Public, permissions: vec![Permission::Write] }],
    };
    let version = BlobVersion {
        address: EntryAddress { volume: volume.id, entry: id(10) },
        revision: id(2),
        len_bytes: 2,
        digest: [3; 32],
        encrypted_key: [4; 32],
    };
    index.authorize_blob_upload(&volume, &defaults, account, id(10), id(2)).unwrap();
    index.validate_blob_version(&version).unwrap();
    assert!(
        index
            .authorize_retained_read(&volume, &defaults, Principal::Human(account), id(10))
            .is_err()
    );
    assert!(index.revision(id(10), id(2)).is_err());
    let mut wrong = version;
    wrong.encrypted_key[0] ^= 1;
    assert!(index.validate_blob_version(&wrong).is_err());
}
