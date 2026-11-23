use super::*;
use crate::store::{
    Access,
    volume_index::{EntryAddress, EntryKind},
};

#[test]
fn desktop_rename_crosses_actual_management_admission_and_returns_exact_committed_cut() {
    // Goal: qualify the concrete inspector IPC. Method: use real admitted sockets and durable store data.
    let mut fixture = Fixture::new();
    let store = fixture.engine.services.store.clone();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Rename IPC", true).unwrap();
    let entry = store.create_database(&access, volume.id, "KV", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    let bytes = vec![0, 255, 0, 128];
    store.kv_put(&access, address, b"old", &bytes).unwrap();
    let expected = store.kv_read(&access, address, b"old", 0, 4, None).unwrap().root;
    let request = Request::KvRename {
        session: None,
        mutation: Box::new(crate::engine::KvRenameMutation {
            address,
            old_key: b"old".to_vec(),
            new_key: b"new".to_vec(),
            expected,
            value: bytes.clone(),
        }),
    };
    let Reply::KvRenamed { receipt } = fixture.exchange(request.clone()) else {
        panic!("actual management rename must return its typed receipt")
    };
    assert_eq!(receipt.address, address);
    assert_eq!(receipt.old_key, b"old");
    assert_eq!(receipt.new_key, b"new");
    let read = store.kv_read(&access, address, b"new", 0, 4, None).unwrap();
    assert_eq!(read.bytes, bytes);
    assert_eq!(read.root, receipt.root);
    assert_ne!(read.root, expected);
    assert!(store.kv_read(&access, address, b"old", 0, 4, None).is_err());
    assert!(
        matches!(fixture.exchange(request), Reply::Error { .. }),
        "no automatic mutation replay"
    );
    assert_eq!(store.kv_read(&access, address, b"new", 0, 4, None).unwrap(), read);
}

#[test]
fn rename_receipt_cannot_acknowledge_another_key_address_or_root_kind() {
    // Goal: bind the local receipt before normalization. Method: validate one real codec receipt against altered requests.
    let volume = crate::store::Id::from_bytes([1; 16]);
    let entry = crate::store::EntryId::from_bytes([2; 15]).unwrap();
    let address = EntryAddress { volume, entry };
    let root = crate::store::key_value::Root { level: 0, digest: [3; 32] };
    let receipt = Reply::KvRenamed {
        receipt: crate::engine::reply::KvRenameReceipt {
            address,
            old_key: b"old".to_vec(),
            new_key: b"new".to_vec(),
            root,
        },
    };
    let encoded = codec::encode(&receipt, codec::MAX_ARCHIVE).unwrap();
    let archived = codec::access::<crate::engine::reply::ArchivedReply>(&encoded).unwrap();
    let request = |address, old_key: &[u8], new_key: &[u8], expected| Request::KvRename {
        session: None,
        mutation: Box::new(crate::engine::KvRenameMutation {
            address,
            old_key: old_key.to_vec(),
            new_key: new_key.to_vec(),
            expected,
            value: vec![0, 255],
        }),
    };
    assert!(archived.matches_request(&request(address, b"old", b"new", root)));
    assert!(!archived.matches_request(&request(address, b"other", b"new", root)));
    assert!(!archived.matches_request(&request(address, b"old", b"other", root)));
    assert!(!archived.matches_request(&request(
        EntryAddress { volume: crate::store::Id::from_bytes([4; 16]), entry },
        b"old",
        b"new",
        root
    )));
    assert!(!archived.matches_request(&request(
        EntryAddress { volume, entry: crate::store::EntryId::from_bytes([5; 15]).unwrap() },
        b"old",
        b"new",
        root
    )));
    let invalid = Reply::KvRenamed {
        receipt: crate::engine::reply::KvRenameReceipt {
            address,
            old_key: b"old".to_vec(),
            new_key: b"new".to_vec(),
            root: crate::store::key_value::Root { level: kvdb::encoding::LEVEL_MAX + 1, ..root },
        },
    };
    let encoded = codec::encode(&invalid, codec::MAX_ARCHIVE).unwrap();
    let invalid = codec::access::<crate::engine::reply::ArchivedReply>(&encoded).unwrap();
    assert!(!invalid.matches_request(&request(address, b"old", b"new", root)));
}
