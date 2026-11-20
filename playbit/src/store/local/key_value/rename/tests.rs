#![allow(clippy::unwrap_used, reason = "Isolated profiles assert complete rename transactions.")]
use super::*;
use crate::store::{
    object_contract::{Acl, EntryAcl, Grant, Principal, Subject},
    volume_index::EntryKind,
};

#[test]
fn ordinary_rename_is_atomic_conditional_and_collision_safe() {
    // Goal: move exact bytes once. Method: collide, race the cut, then commit and reopen a real backing.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account.id);
    let volume = store.create_volume("Rename", true).unwrap();
    let entry = store.create_database(&access, volume.id, "KV", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    let bytes = [0, 255, 0, 128];
    store.kv_put(&access, address, b"old", &bytes).unwrap();
    store.kv_put(&access, address, b"occupied", b"keep").unwrap();
    let original = store.kv_read(&access, address, b"old", 0, 4, None).unwrap();
    assert!(matches!(
        store.kv_rename(&access, address, b"absent", b"new", original.root, &bytes),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        store.kv_rename(&access, address, b"old", b"occupied", original.root, &bytes),
        Err(Error::AlreadyExists(_))
    ));
    assert_eq!(store.kv_read(&access, address, b"old", 0, 4, None).unwrap(), original);
    store.kv_put(&access, address, b"racing", b"change").unwrap();
    assert!(matches!(
        store.kv_rename(&access, address, b"old", b"new", original.root, &bytes),
        Err(Error::LocalContention(_))
    ));
    let current = store.kv_read(&access, address, b"old", 0, 4, None).unwrap();
    let mut receiver = store.changes.take_receiver().unwrap();
    drop(receiver.drain());
    let next = store.kv_rename(&access, address, b"old", b"new", current.root, &bytes).unwrap();
    let published = receiver.drain();
    assert_eq!(published.records.len(), 1, "one committed rename produces one publication");
    assert_eq!(published.records[0].volume(), volume.id);
    assert_eq!(
        published.records[0].kind(),
        crate::store::change_feed::CommittedChangeKind::Database(DatabaseId::Entry(entry.id))
    );
    drop(published);
    drop(receiver);
    assert!(store.kv_read(&access, address, b"old", 0, 4, None).is_err());
    let moved = store.kv_read(&access, address, b"new", 0, 4, None).unwrap();
    assert_eq!(moved.bytes, bytes);
    assert_eq!(moved.root, next);
    assert_ne!(next, current.root);
    assert_eq!(store.kv_read(&access, address, b"occupied", 0, 4, None).unwrap().bytes, b"keep");
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.kv_read(&access, address, b"new", 0, 4, None).unwrap().bytes, bytes);
}

#[test]
fn ordinary_rename_requires_current_read_and_write_without_changing_the_source() {
    // Goal: deny rename with incomplete rights. Method: replace the entry ACL at the real store boundary.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account.id);
    let volume = store.create_volume("Rename rights", true).unwrap();
    let entry = store.create_database(&access, volume.id, "KV", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    store.kv_put(&access, address, b"old", b"keep").unwrap();
    let original = store.kv_read(&access, address, b"old", 0, 4, None).unwrap();
    for permissions in [vec![Permission::Read], vec![Permission::Write], vec![Permission::Append]] {
        store
            .set_entry_permissions(
                &access,
                volume.id,
                entry.id,
                EntryAcl::Replace(Acl {
                    grants: vec![Grant {
                        subject: Subject::Principal(Principal::Human(access.account)),
                        permissions,
                    }],
                }),
            )
            .unwrap();
        assert!(matches!(
            store.kv_rename(&access, address, b"old", b"new", original.root, b"keep"),
            Err(Error::AccessDenied(_))
        ));
        let database = store.entry_backing(volume.id, entry.id, false).unwrap();
        let read = database.begin_read().unwrap();
        assert_eq!(read.root_hash().unwrap().map(key_value::Root::from), Some(original.root));
        assert!(read.get(b"new").unwrap().is_none());
    }
}
