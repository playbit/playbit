#![allow(clippy::unwrap_used, reason = "Tests assert fixture setup and expected outcomes.")]
use super::*;
use kvdb::{CreateOptions, storage::mem::MemStorage};
const OWNER: Id = Id::from_bytes([9; 16]);
const TITLE: Id = Id::from_bytes([2; 16]);

fn declaration() -> RootInitialization {
    crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session)
}
fn database() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 1)).unwrap()
}
fn objects(entry: &mut InitialDatabase) -> &mut Vec<InitialObject> {
    let InitialDatabaseKind::ObjectDb { objects, .. } = &mut entry.kind else {
        panic!("fixture is an object database");
    };
    objects
}

#[test]
fn named_database_preserves_exact_seed_ids_and_complete_initial_fields() {
    for count in [0, 1, MAX_ROOT_INITIAL_OBJECTS] {
        let mut root = declaration();
        let entry = &mut root.databases[0];
        let seed = objects(entry)[0].clone();
        *objects(entry) = (0..count)
            .map(|n| InitialObject { id: Id::from_bytes([32 + n as u8; 16]), ..seed.clone() })
            .collect();
        let database = database();
        entry.write_objects(&database, OWNER, 1).unwrap();
        let snapshot = object_database::Snapshot::new(database.begin_read().unwrap());
        for seed in objects(entry) {
            let record = snapshot.read(seed.id, None).unwrap();
            assert_eq!(record.object_id, seed.id);
            assert_eq!(record.type_id, seed.type_id);
            for (id, cell) in record.fields {
                assert_eq!(cell.values.as_ref(), seed.fields.get(&id));
                assert_eq!(cell.stamp.device_id, OWNER);
            }
        }
    }
}

#[test]
fn invalid_later_seed_aborts_all_earlier_births() {
    let mut root = declaration();
    let entry = &mut root.databases[0];
    let mut invalid = objects(entry)[0].clone();
    invalid.id = Id::from_bytes([88; 16]);
    invalid.fields.insert(TITLE, vec![Value::Boolean(false)]);
    objects(entry).push(invalid);
    let database = database();
    let clock = database.clock();
    let hash = database.begin_read().unwrap().root_hash().unwrap();
    assert!(entry.write_objects(&database, OWNER, 1).is_err());
    assert_eq!(database.clock(), clock);
    assert_eq!(database.begin_read().unwrap().root_hash().unwrap(), hash);
    assert!(database.begin_read().unwrap().next_key(b"", false).unwrap().is_none());
}

#[test]
fn declaration_admission_rejects_seed_field_types_and_shapes_without_storage() {
    // JSON and typed declaration admission share ordinary object creation's
    // field validation, before any backing database or root intent can exist.
    let invalid = |root: &RootInitialization| {
        assert!(root.fingerprint().is_err());
        assert!(RootInitialization::from_json(&serde_json::to_vec(root).unwrap()).is_err());
    };
    let mut root = declaration();
    objects(&mut root.databases[0])[0].type_id = Id::from_bytes([90; 16]);
    invalid(&root);
    root = declaration();
    objects(&mut root.databases[0])[0]
        .fields
        .insert(Id::from_bytes([91; 16]), vec![Value::Text("Unknown field".into())]);
    invalid(&root);
    for values in [
        vec![Value::Boolean(false)],
        Vec::new(),
        vec![Value::Text("One".into()), Value::Text("Two".into())],
    ] {
        root = declaration();
        objects(&mut root.databases[0])[0].fields.insert(TITLE, values);
        invalid(&root);
    }
    let InitialDatabaseKind::ObjectDb { schema, objects: seeds } = &mut root.databases[0].kind
    else {
        panic!("fixture is an object database");
    };
    schema.types.get_mut(&seeds[0].type_id).unwrap().fields.get_mut(&TITLE).unwrap().repeated =
        true;
    root.fingerprint().unwrap();
    objects(&mut root.databases[0])[0].fields.insert(TITLE, Vec::new());
    root.fingerprint().unwrap();
}

#[test]
fn initializer_never_overwrites_an_existing_backing() {
    let entry = declaration().databases.remove(0);
    let database = database();
    entry.write_objects(&database, OWNER, 1).unwrap();
    let hash = database.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(entry.write_objects(&database, OWNER, 2), Err(Error::AlreadyExists(_))));
    assert_eq!(database.begin_read().unwrap().root_hash().unwrap(), hash);
}

#[test]
fn declaration_rejects_duplicate_ids_names_and_unbounded_content() {
    let mut root = declaration();
    let entry = root.databases[0].clone();
    root.databases.push(entry);
    assert!(matches!(root.fingerprint(), Err(Error::InvalidInput(_))));
    root.databases[1].id = EntryId::from_bytes([12; 15]).unwrap();
    assert!(matches!(root.fingerprint(), Err(Error::InvalidInput(_))));
    root.databases[1].name = "other".into();
    root.fingerprint().unwrap();
    let duplicate = objects(&mut root.databases[0])[0].clone();
    objects(&mut root.databases[0]).push(duplicate);
    assert!(matches!(root.fingerprint(), Err(Error::InvalidInput(_))));
    assert!(matches!(
        RootInitialization::from_json(&vec![b' '; MAX_ROOT_INITIALIZATION_BYTES + 1]),
        Err(Error::LimitExceeded)
    ));
    let mut root = declaration();
    objects(&mut root.databases[0])[0]
        .fields
        .insert(TITLE, vec![Value::Text("x".repeat(MAX_ROOT_INITIALIZATION_BYTES))]);
    assert!(matches!(root.fingerprint(), Err(Error::LimitExceeded)));
    assert!(
        RootInitialization::from_json(
            br#"{"permissions":{"grants":[]},"schema":{"types":{}},"objects":[]}"#
        )
        .is_err()
    );
}

#[test]
fn independent_blob_seeds_share_entry_identity_name_and_size_bounds() {
    let mut root = declaration();
    let blob = InitialBlob {
        id: root.databases[0].id,
        name: "note.txt".into(),
        bytes: b"Independent seed".to_vec(),
    };
    root.blobs.push(blob);
    assert!(matches!(root.fingerprint(), Err(Error::InvalidInput(_))));
    root.blobs[0].id = EntryId::from_bytes([31; 15]).unwrap();
    root.blobs[0].name = root.databases[0].name.clone();
    assert!(matches!(root.fingerprint(), Err(Error::InvalidInput(_))));
    root.blobs[0].name = "note.txt".into();
    root.fingerprint().unwrap();
    root.blobs[0].bytes = vec![0; MAX_ROOT_INITIALIZATION_BYTES];
    assert!(matches!(root.fingerprint(), Err(Error::LimitExceeded)));
}

#[test]
fn prepared_backing_receipt_binds_seed_owner_and_source_root() {
    let entry = declaration().databases.remove(0);
    let database = database();
    assert!(!entry.prepared_objects(&database, OWNER).unwrap());
    entry.write_objects(&database, OWNER, 1).unwrap();
    assert!(entry.prepared_objects(&database, OWNER).unwrap());
    assert!(entry.prepared_objects(&database, Id::from_bytes([8; 16])).is_err());
    let mut changed = entry.clone();
    changed.name = "other".into();
    assert!(changed.prepared_objects(&database, OWNER).is_err());
    let mut tx = database.begin_write(2).unwrap();
    tx.put(b"unexpected", b"changed after preparation").unwrap();
    tx.commit().unwrap();
    assert!(entry.prepared_objects(&database, OWNER).is_err());
}
