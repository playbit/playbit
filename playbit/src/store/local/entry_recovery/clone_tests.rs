#![allow(clippy::unwrap_used, reason = "Tests assert database-copy behavior.")]
use super::*;
use crate::store::object_contract::{Acl, EntryAcl};

#[test]
fn copies_objects_and_append_logs_into_differently_keyed_volume() {
    // Preserve typed data, inherit destination permissions, and keep logs in service-backed volumes.
    use crate::store::object_contract::{Grant, Subject, Value};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account.id);
    let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    let source =
        store.initialize_root(Id::generate().unwrap(), "Source", true, &declaration).unwrap();
    let destination = store.create_volume("Destination", true).unwrap();
    let database = &declaration.databases[0];
    let crate::store::InitialDatabaseKind::ObjectDb { schema, .. } = &database.kind else {
        panic!("object fixture")
    };
    let (ty, definition) = schema.types.iter().next().unwrap();
    let field = *definition.fields.iter().find(|(_, f)| f.name == "title").unwrap().0;
    let address = EntryAddress { volume: source.id, entry: database.id };
    let record = store
        .create_object_with_fields(
            &access,
            address,
            *ty,
            &BTreeMap::from([(field, Some(vec![Value::Text("copied".into())]))]),
        )
        .unwrap();
    let target = EntryAddress { volume: destination.id, entry: EntryId::generate().unwrap() };
    store
        .set_entry_permissions(
            &access,
            source.id,
            database.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(store.account.id)),
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    // An interrupted pre-birth link must not reserve an otherwise unpublished identity forever.
    let orphan = store
        .path
        .join(crate::paths::VOLUMES)
        .join(destination.id.to_string())
        .join(DatabaseId::Entry(target.entry).file_name());
    fs::write(orphan, b"interrupted prepared backing").unwrap();
    store.clone_database(&access, address, target, None, "Objects").unwrap();
    assert_eq!(
        store.entry_details(&access, target.volume, target.entry).unwrap().permissions,
        EntryAcl::Inherit
    );
    assert_eq!(store.read_object(&access, target, record.object_id).unwrap(), record);
    store
        .edit_fields(
            &access,
            target,
            record.object_id,
            &BTreeMap::from([(field, Some(vec![Value::Text("changed".into())]))]),
        )
        .unwrap();
    assert_eq!(store.read_object(&access, address, record.object_id).unwrap(), record);
    let log = store.create_bloblog(&access, source.id, "Log").unwrap();
    store.bloblog_put(&access, source.id, log.id, &[b"one".to_vec(), b"two".to_vec()]).unwrap();
    let target = EntryAddress { volume: destination.id, entry: EntryId::generate().unwrap() };
    store
        .clone_database(
            &access,
            EntryAddress { volume: source.id, entry: log.id },
            target,
            None,
            "Log copy",
        )
        .unwrap();
    assert!(
        store.bloblog_get(&access, destination.id, target.entry, None).unwrap().entries.is_empty()
    );
    let mut copied = store.bloblog_pending(&access, destination.id, target.entry).unwrap();
    // Pending entries are indexed by identity, so compare their payloads without that order.
    copied.sort_unstable_by(|a, b| a.data.cmp(&b.data));
    assert_eq!(
        copied.iter().map(|e| e.data.as_slice()).collect::<Vec<_>>(),
        [b"one".as_slice(), b"two".as_slice()]
    );
    assert!(copied.iter().all(|entry| entry.log == target.entry));
    let private = store.create_volume("Private", false).unwrap();
    let forbidden = EntryAddress { volume: private.id, entry: EntryId::generate().unwrap() };
    assert!(matches!(
        store.clone_database(
            &access,
            EntryAddress { volume: source.id, entry: log.id },
            forbidden,
            None,
            "Not allowed"
        ),
        Err(Error::AccessDenied(_))
    ));
    assert!(store.entry(&access, private.id, forbidden.entry).is_err());
}

#[test]
fn copies_current_kv_data_with_independent_identity_and_durable_retry_receipt() {
    // Copy between differently keyed volumes, reopen, and repeat the same copy operation.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account.id);
    let volume = store.create_volume("Source", false).unwrap();
    let target = store.create_volume("Destination", false).unwrap();
    let entry = store.create_database(&access, volume.id, "source", EntryKind::KvDb {}).unwrap();
    let source = EntryAddress { volume: volume.id, entry: entry.id };
    store.kv_put(&access, source, b"key", b"before").unwrap();
    let destination = EntryAddress { volume: target.id, entry: EntryId::generate().unwrap() };
    let copy = store.clone_database(&access, source, destination, None, "copy").unwrap();
    assert_eq!(copy.id, destination.entry);
    assert_ne!(copy.info.created_revision, entry.info.created_revision);
    assert_eq!(copy.info.initial_permissions, EntryAcl::Inherit);
    assert_eq!(store.kv_read(&access, destination, b"key", 0, 64, None).unwrap().bytes, b"before");
    store.kv_put(&access, source, b"key", b"after").unwrap();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(
        store.clone_database(&access, source, destination, None, "copy").unwrap().id,
        copy.id
    );
    assert_eq!(store.kv_read(&access, destination, b"key", 0, 64, None).unwrap().bytes, b"before");
    assert!(store.clone_database(&access, source, destination, None, "different").is_err());
    store
        .set_entry_permissions(&access, volume.id, source.entry, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(
        store.clone_database(&access, source, destination, None, "copy"),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
fn copy_rejects_non_database_and_occupied_destination_without_publishing_entry() {
    // Bad sources and occupied names must not create a namespace birth.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account.id);
    let volume = store.create_volume("Personal", false).unwrap();
    let blob = store.create_blob(&access, volume.id, "blob", b"data").unwrap();
    let source = EntryAddress { volume: volume.id, entry: blob.id };
    let destination = EntryAddress { volume: volume.id, entry: EntryId::generate().unwrap() };
    assert!(store.clone_database(&access, source, destination, None, "copy").is_err());
    let database =
        store.create_database(&access, volume.id, "database", EntryKind::KvDb {}).unwrap();
    let source = EntryAddress { volume: volume.id, entry: database.id };
    assert!(matches!(
        store.clone_database(&access, source, destination, None, "blob"),
        Err(Error::AlreadyExists(_))
    ));
    assert!(store.entry(&access, volume.id, destination.entry).is_err());
    store.clone_database(&access, source, destination, None, "copy").unwrap();
    assert_eq!(store.resolve_entry(&access, volume.id, "copy").unwrap().id, destination.entry);
}
