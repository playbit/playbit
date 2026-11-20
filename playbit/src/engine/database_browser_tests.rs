#![allow(clippy::unwrap_used, reason = "Tests assert native editor operations.")]
use super::database_browser::*;
use crate::store::{Access, DatabaseReference, LocalStore, volume_index::EntryKind};
use serde_json::{Value, json};

fn request(store: &LocalStore, value: Value) -> crate::store::Result<Value> {
    serde_json::from_slice(&call(store, &serde_json::to_vec(&value).unwrap()).1?)
        .map_err(crate::store::Error::decode::<Value>)
}

#[test]
fn native_editor_resolves_references_and_creates_edits_deletes_binary_keys_offline() {
    // Use native request bytes and real store authorization, including insert's no-replacement contract.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Personal", false).unwrap();
    let access = Access::owner(store.account().id);
    let database =
        store.create_database(&access, volume.id, "folder/Settings", EntryKind::KvDb {}).unwrap();
    let address = json!({ "volume": volume.id, "entry": database.id });
    let reference = DatabaseReference { volume: volume.id, entry: database.id }.to_string();
    let opened = request(&store, json!({ "op": "open", "reference": reference })).unwrap();
    assert_eq!(opened["details"]["entry"]["id"], json!(database.id));
    assert!(opened["parent"].is_string());
    let insert =
        json!({ "op": "insert", "address": address, "key": [0, 255, 42], "value": [0, 17, 255] });
    assert_eq!(request(&store, insert.clone()).unwrap()["kind"], "saved");
    assert!(request(&store, insert).is_err());
    let read = json!({ "op": "read", "address": address, "key": [0, 255, 42] });
    assert_eq!(request(&store, read.clone()).unwrap()["value"]["bytes"], json!([0, 17, 255]));
    request(&store, json!({ "op": "put", "address": address, "key": [0, 255, 42], "value": [42] }))
        .unwrap();
    assert_eq!(request(&store, read.clone()).unwrap()["value"]["bytes"], json!([42]));
    request(&store, json!({ "op": "delete_key", "address": address, "key": [0, 255, 42] }))
        .unwrap();
    assert!(request(&store, read).is_err());
}

#[test]
fn recovered_reference_waits_for_its_initial_namespace() {
    let directory = tempfile::tempdir().unwrap();
    let source = LocalStore::open(directory.path().join("source")).unwrap();
    let volume = source.create_volume("Personal", true).unwrap();
    let access = Access::owner(source.account().id);
    let entry = source.create_database(&access, volume.id, "Settings", EntryKind::KvDb {}).unwrap();
    let mut recovered = LocalStore::open(directory.path().join("recovered")).unwrap();
    recovered.bind_authenticated_account(source.account().id).unwrap();
    recovered.import_recovery_catalog(&source.recovery_catalog().unwrap(), || true).unwrap();
    let reference = DatabaseReference { volume: volume.id, entry: entry.id }.to_string();
    let error = request(&recovered, json!({ "op": "open", "reference": reference })).unwrap_err();
    assert!(matches!(error, crate::store::Error::System(ref text)
        if text == "This volume is waiting for initial synchronization."));
    assert!(recovered.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap().is_none());
}

#[test]
fn native_reference_open_rejects_files_deleted_databases_and_revoked_access() {
    // A correctly formed pointer does not confer Read authority or select a blob as a database.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Personal", false).unwrap();
    let access = Access::owner(store.account().id);
    let blob = store.create_blob(&access, volume.id, "file", b"data").unwrap();
    let open = |entry| json!({ "op": "open", "reference": DatabaseReference { volume: volume.id, entry }.to_string() });
    assert!(request(&store, open(blob.id)).is_err());
    let db = store.create_database(&access, volume.id, "Settings", EntryKind::KvDb {}).unwrap();
    store
        .set_entry_permissions(
            &access,
            volume.id,
            db.id,
            crate::store::object_contract::EntryAcl::Replace(Default::default()),
        )
        .unwrap();
    assert!(request(&store, open(db.id)).is_err());
    store
        .set_entry_permissions(
            &access,
            volume.id,
            db.id,
            crate::store::object_contract::EntryAcl::Inherit,
        )
        .unwrap();
    store.remove_entry(&access, volume.id, db.id).unwrap();
    assert!(request(&store, open(db.id)).is_err());
    assert!(call(&store, &vec![b' '; MAX_REQUEST_BYTES + 1]).1.is_err());
    assert!(request(&store, json!({ "op": "volumes", "unknown": true })).is_err());
}
