#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::engine::{Request, ipc, reply::ArchivedReply};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

struct EngineProcess(Child);

#[test]
fn retained_history_and_recovery_replies_bind_address_cursor_and_selected_revision() {
    use playbit::engine::reply::{DiagnosticReply, Reply};
    use playbit::store::{Access, EntryId, Id, LocalStore, volume_index::EntryAddress};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Recovery", false).unwrap().id;
    let access = Access::owner(store.account().id);
    let entry = store.create_blob(&access, volume, "file", b"retained").unwrap();
    let address = EntryAddress { volume, entry: entry.id };
    let revision = entry.info.created_revision;
    store.remove_entry(&access, volume, entry.id).unwrap();
    let page = store.entry_revisions(&access, address, None, 64).unwrap();
    let encoded = ipc::codec::encode(
        &Reply::EntryRevisions { address, after: None, page: Box::new(page) },
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let reply = ipc::codec::access::<ArchivedReply>(&encoded).unwrap();
    let wrong = EntryAddress { volume: Id::from_bytes([3; 16]), ..address };
    for (target, cursor, valid) in [
        (address, None, true),
        (wrong, None, false),
        (
            address,
            Some(playbit::store::volume_index::HistoryCursor { origin_clock: 1, revision }),
            false,
        ),
    ] {
        assert_eq!(
            reply.matches_request(&Request::EntryRevisions {
                session: None,
                address: target,
                after: cursor,
                limit: 64
            }),
            valid
        );
    }
    let page = store.entry_revisions(&access, address, None, 1).unwrap();
    let cursor = page.next.expect("deletion precedes creation");
    let page = store.entry_revisions(&access, address, Some(cursor), 1).unwrap();
    let encoded = ipc::codec::encode(
        &Reply::EntryRevisions { address, after: Some(cursor), page: Box::new(page) },
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let reply = ipc::codec::access::<ArchivedReply>(&encoded).unwrap();
    for (after, valid) in [
        (cursor, true),
        (
            playbit::store::volume_index::HistoryCursor {
                origin_clock: cursor.origin_clock + 1,
                ..cursor
            },
            false,
        ),
        (
            playbit::store::volume_index::HistoryCursor {
                revision: EntryId::from_bytes([9; 15]).unwrap(),
                ..cursor
            },
            false,
        ),
    ] {
        assert_eq!(
            reply.matches_request(&Request::EntryRevisions {
                session: None,
                address,
                after: Some(after),
                limit: 1
            }),
            valid
        );
    }
    let recovered =
        store.recover_entry(&access, address, "folder/recovered", Some(revision)).unwrap();
    let encoded = ipc::codec::encode(
        &Reply::Diagnostic(DiagnosticReply::EntryRecovered {
            name: "folder/recovered".into(),
            source: address,
            revision: Some(revision),
            rejected_cut: None,
            entry: recovered,
        }),
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let reply = ipc::codec::access::<ArchivedReply>(&encoded).unwrap();
    for (target, selected, name, valid) in [
        (address, Some(revision), "folder/recovered", true),
        (wrong, Some(revision), "folder/recovered", false),
        (address, None, "folder/recovered", false),
        (address, Some(EntryId::from_bytes([9; 15]).unwrap()), "folder/recovered", false),
        (address, Some(revision), "other/recovered", false),
    ] {
        assert_eq!(
            reply.matches_request(&Request::RecoverEntry {
                address: target,
                name: name.into(),
                revision: selected,
                rejected_cut: None,
            }),
            valid
        );
    }
}

// File acknowledgments identify the volume, stable entry and requested name.
// A same-shaped result from another entry must not acknowledge this operation.
#[test]
fn file_replies_bind_the_complete_entry_address() {
    use playbit::engine::reply::{DiagnosticReply, Reply};
    use playbit::store::{Access, Id, LocalStore, volume_index::EntryAddress};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Files", false).unwrap().id;
    let entry = store
        .create_blob(&Access::owner(store.account().id), volume, "Files/readme.txt", b"test")
        .unwrap();
    let address = EntryAddress { volume, entry: entry.id };
    let path = "output.txt".into();
    let encode = |reply| ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap();
    let bytes =
        encode(Reply::Diagnostic(DiagnosticReply::FileWritten { address, entry: entry.clone() }));
    let reply = ipc::codec::access::<ArchivedReply>(&bytes).unwrap();
    assert!(reply.matches_request(&Request::FileWrite { address, path }));
    let wrong = EntryAddress { volume: Id::from_bytes([3; 16]), ..address };
    assert!(
        !reply.matches_request(&Request::FileWrite { address: wrong, path: "input.txt".into() })
    );
    let bytes = encode(Reply::Diagnostic(DiagnosticReply::FileCreated {
        address,
        name: "Files/readme.txt".into(),
        entry,
    }));
    let reply = ipc::codec::access::<ArchivedReply>(&bytes).unwrap();
    assert!(reply.matches_request(&Request::FileCreate {
        session: None,
        volume,
        name: "Files/readme.txt".into(),
        path: "input.txt".into()
    }));
    assert!(!reply.matches_request(&Request::FileCreate {
        session: None,
        volume,
        name: "Other/readme.txt".into(),
        path: "input.txt".into()
    }));
    assert!(!reply.matches_request(&Request::FileWrite { address, path: "input.txt".into() }));
    let bytes = encode(Reply::Diagnostic(DiagnosticReply::FileRead { address, bytes: 4 }));
    let reply = ipc::codec::access::<ArchivedReply>(&bytes).unwrap();
    assert!(reply.matches_request(&Request::FileRead {
        address,
        session: None,
        path: "output.txt".into()
    }));
    assert!(!reply.matches_request(&Request::FileRead {
        address: wrong,
        session: None,
        path: "output.txt".into()
    }));
}

#[test]
fn object_replies_bind_database_object_type_and_page_cursor() {
    use playbit::engine::reply::{ObjectPageResult, ObjectResult, Reply};
    use playbit::store::{EntryId, Id, object_contract::ObjectFields, volume_index::EntryAddress};
    let database = EntryAddress {
        volume: Id::from_bytes([1; 16]),
        entry: EntryId::from_bytes([2; 15]).unwrap(),
    };
    let object = Id::from_bytes([3; 16]);
    let type_id = Id::from_bytes([4; 16]);
    let result = ObjectFields::new(object, type_id);
    let encoded = ipc::codec::encode(
        &Reply::Object(ObjectResult { database, object: result }),
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let reply = ipc::codec::access::<ArchivedReply>(&encoded).unwrap();
    assert!(reply.matches_request(&Request::ReadObject { session: None, database, object }));
    let wrong_database = EntryAddress { entry: EntryId::from_bytes([5; 15]).unwrap(), ..database };
    assert!(!reply.matches_request(&Request::ReadObject {
        session: None,
        database: wrong_database,
        object
    }));
    let wrong_volume = EntryAddress { volume: Id::from_bytes([6; 16]), ..database };
    assert!(!reply.matches_request(&Request::ReadObject {
        session: None,
        database: wrong_volume,
        object
    }));
    assert!(!reply.matches_request(&Request::ReadObject {
        session: None,
        database,
        object: type_id
    }));
    assert!(!reply.matches_request(&Request::CreateObject {
        session: None,
        database,
        type_id: object,
        fields: Default::default()
    }));
    assert!(!reply.matches_request(&Request::CreateObject {
        session: None,
        database,
        type_id,
        fields: Default::default()
    }));
    let encoded = ipc::codec::encode(
        &Reply::ObjectCreated(ObjectResult {
            database,
            object: ObjectFields::new(object, type_id),
        }),
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let reply = ipc::codec::access::<ArchivedReply>(&encoded).unwrap();
    assert!(reply.matches_request(&Request::CreateObject {
        session: None,
        database,
        type_id,
        fields: Default::default()
    }));
    assert!(!reply.matches_request(&Request::ReadObject { session: None, database, object }));
    let encoded = ipc::codec::encode(
        &Reply::Objects(ObjectPageResult {
            database,
            type_id,
            after: Some(object),
            page: playbit::store::ObjectPage { objects: vec![], next: None },
        }),
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let reply = ipc::codec::access::<ArchivedReply>(&encoded).unwrap();
    assert!(reply.matches_request(&Request::Objects {
        session: None,
        database,
        type_id,
        after: Some(object),
        limit: 64
    }));
    for request in [
        Request::Objects {
            session: None,
            database: wrong_database,
            type_id,
            after: Some(object),
            limit: 64,
        },
        Request::Objects {
            session: None,
            database,
            type_id: object,
            after: Some(object),
            limit: 64,
        },
        Request::Objects { session: None, database, type_id, after: None, limit: 64 },
    ] {
        assert!(!reply.matches_request(&request));
    }
}

#[test]
fn repeated_owned_references_cannot_amplify_archive_materialization() {
    // Point two owned vectors at one byte range. Validation must reject sharing
    // before a caller could reconstruct multiple allocations from the same bytes.
    #[derive(rkyv::Archive, rkyv::Serialize)]
    struct Payload {
        first: Vec<u8>,
        second: Vec<u8>,
    }
    let mut bytes = ipc::codec::encode(
        &Payload { first: vec![1; 512], second: vec![2; 512] },
        ipc::codec::MAX_ARCHIVE,
    )
    .unwrap();
    let archived = ipc::codec::access::<ArchivedPayload>(&bytes).unwrap();
    let base = bytes.as_ptr() as usize;
    let target = archived.first.as_ptr() as usize - base;
    let field = std::ptr::from_ref(&archived.second) as usize - base;
    // ArchivedVec's first C-layout field is its 32-bit relative pointer.
    let offset = i32::try_from(target as i64 - field as i64).unwrap();
    bytes[field..field + 4].copy_from_slice(&offset.to_le_bytes());
    assert!(ipc::codec::access::<ArchivedPayload>(&bytes).is_err());
}
impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn abandoned_queued_clients_do_not_terminate_the_engine() {
    let profile = tempfile::tempdir().unwrap();
    let (mut ready, child_output) = UnixStream::pair().unwrap();
    ready.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let output: std::os::fd::OwnedFd = child_output.into();
    let mut engine = EngineProcess(
        Command::new(env!("CARGO_BIN_EXE_playbit-engine"))
            .env("PLAYBIT_PROFILE", profile.path())
            .stdout(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let socket = profile.path().join("control/engine.sock");
    let mut signal = [0; ipc::ENGINE_READY.len()];
    ready.read_exact(&mut signal).unwrap();
    assert_eq!(signal, ipc::ENGINE_READY);
    let mut blocked = UnixStream::connect(&socket).unwrap();
    ipc::verify_peer(&blocked).unwrap();
    // Hold dispatch in a partial frame while subsequent callers abandon their
    // queued sockets. On macOS, configuring an already-closed accepted socket
    // can return EINVAL; that error belongs to its client, not the host loop.
    blocked.write_all(&[0]).unwrap();
    thread::sleep(Duration::from_millis(100));
    for _ in 0..16 {
        let mut client = UnixStream::connect(&socket).unwrap();
        ipc::write(
            &mut client,
            &ipc::codec::encode(&Request::Status, ipc::codec::MAX_ARCHIVE).unwrap(),
        )
        .unwrap();
        client.shutdown(Shutdown::Both).unwrap();
    }
    drop(blocked);
    let status = ipc::call_archive(profile.path(), &Request::Status).unwrap();
    assert!(matches!(status.archived(), ArchivedReply::Status(_)));
    assert!(engine.0.try_wait().unwrap().is_none());
}
