//! Public storage journeys over the authoritative index and named backings.
#![allow(clippy::unwrap_used, reason = "Integration fixtures assert successful operations.")]
use playbit::store::{
    Access, BuildSelector, DatabaseId, EntryId, Id, LocalStore, ReceivedVolume, Volume,
    object_contract::{Acl, EntryAcl, Grant, Permission, Principal, Subject, Value},
    volume_index::{EntryAddress, EntryKind},
};
use std::collections::BTreeMap;

type WebAuthorityRows = Vec<(Vec<u8>, Vec<u8>)>;

#[derive(Clone, Copy)]
enum WebBacking {
    Kv,
    Objects,
    Log,
}

fn backing_authority_fixture(kind: WebBacking) -> serde_json::Value {
    use playbit::store::{
        MetadataPolicy,
        volume_index::{self, DatabaseProtocol, EntryInfo},
    };
    let volume = Volume {
        id: Id::from_bytes([0xc1; 16]),
        owner: Id::from_bytes([9; 16]),
        name: "Root".into(),
        personal: true,
        share_key_with_service: true,
    };
    let account = Id::from_bytes([1; 16]);
    let pending: Option<playbit::store::bloblog::Entry> = if matches!(kind, WebBacking::Log) {
        let vectors: serde_json::Value =
            serde_json::from_slice(include_bytes!("../platform/web/tests/bloblog-vectors.json"))
                .unwrap();
        Some(serde_json::from_value(vectors["pages"][0]["entries"][0].clone()).unwrap())
    } else {
        None
    };
    let entry = pending
        .as_ref()
        .map_or_else(|| EntryId::from_bytes([0x81; 15]).unwrap(), |entry| entry.log);
    let grant =
        |permissions| Grant { subject: Subject::Principal(Principal::Human(account)), permissions };
    let acl = |permissions| {
        let mut grants = vec![
            grant(permissions),
            Grant {
                subject: Subject::Principal(Principal::Human(volume.owner)),
                permissions: vec![Permission::Read, Permission::Write],
            },
            Grant {
                subject: Subject::Principal(playbit::store::PLAYBIT_REPLICA_PRINCIPAL),
                permissions: vec![Permission::Read],
            },
        ];
        grants.retain(|grant| !grant.permissions.is_empty());
        EntryAcl::Replace(Acl { grants })
    };
    let policy: MetadataPolicy = serde_json::from_value(serde_json::json!({
        "volume": volume, "permissions": {"grants": [grant(vec![Permission::Read])]},
        "provisioned_versions": [playbit::identifier::encode(&[0x02; 32])]
    }))
    .unwrap();
    let mut index = kvdb::Db::create(
        kvdb::storage::mem::MemStorage::new(),
        &kvdb::CreateOptions::syncable(DatabaseId::Index.hash_key(&[9; 32], volume.id), 7),
    )
    .unwrap();
    policy.write(&index, 1000).unwrap();
    let schema = if matches!(kind, WebBacking::Objects) {
        let root: serde_json::Value = serde_json::to_value(
            playbit::package::VolumeInitializers::from_json(include_bytes!(
                "../../guests/todo/playbit-volumes.json"
            ))
            .unwrap()
            .for_role(playbit::package::AutomaticVolumeRole::Session),
        )
        .unwrap();
        Some(
            serde_json::from_value::<playbit::store::object_contract::Schema>(
                root["databases"][0]["kind"]["schema"].clone(),
            )
            .unwrap(),
        )
    } else {
        None
    };
    let mut tx = index.begin_write(1000).unwrap();
    volume_index::create(
        &mut tx,
        entry,
        &EntryInfo {
            namespace: Default::default(),
            origin: playbit::store::volume_index::namespace::Origin {
                installation: playbit::store::Id::from_bytes([42; 16]),
                label: None,
            },
            name: "settings".into(),
            created_revision: EntryId::from_bytes([0x82; 15]).unwrap(),
            kind: schema
                .clone()
                .map_or(EntryKind::KvDb {}, |schema| EntryKind::ObjectDb { schema }),
            protocol: Some(if matches!(kind, WebBacking::Log) {
                DatabaseProtocol::BlobLog
            } else {
                DatabaseProtocol::Kvdb
            }),
            initial_permissions: acl(vec![Permission::Read, Permission::Write]),
        },
    )
    .unwrap();
    volume_index::mark_backing_ready(&mut tx, entry).unwrap();
    tx.commit().unwrap();
    let mut snapshots = BTreeMap::new();
    for (name, permissions) in [
        ("read_write", vec![Permission::Read, Permission::Write]),
        ("append_read", vec![Permission::Read, Permission::Append]),
        ("read_only", vec![Permission::Read]),
        ("denied", vec![]),
        ("write_only", vec![Permission::Write]),
        ("read_restored", vec![Permission::Read, Permission::Write]),
    ] {
        let mut tx = index.begin_write(1000).unwrap();
        volume_index::set_permissions(&mut tx, entry, &acl(permissions)).unwrap();
        tx.commit().unwrap();
        let (next, rows) = web_authority_rows(index);
        index = next;
        snapshots.insert(name, rows);
    }
    let backing = kvdb::Db::create(
        kvdb::storage::mem::MemStorage::new(),
        &kvdb::CreateOptions::syncable(DatabaseId::Entry(entry).hash_key(&[9; 32], volume.id), 7),
    )
    .unwrap();
    let page = if let Some(entry) = pending {
        let log = playbit::store::bloblog::Log {
            database: &backing,
            volume: volume.id,
            entry: entry.log,
            encryption_key: None,
            now: 1000,
        };
        let page = log.get(None).unwrap();
        log.apply(&[entry], true).unwrap();
        Some(page)
    } else {
        let mut tx = backing.begin_write(1000).unwrap();
        if let Some(schema) = schema {
            playbit::store::object_database::create(
                &mut tx,
                &schema,
                Id::from_bytes([1; 16]),
                Id::from_bytes([1; 16]),
                account,
                &BTreeMap::from([(Id::from_bytes([3; 16]), Some(vec![Value::Boolean(false)]))]),
            )
            .unwrap();
        } else {
            tx.put(b"key", b"accepted").unwrap();
        }
        tx.commit().unwrap();
        None
    };
    let (_, rows) = web_authority_rows(backing);
    serde_json::json!({"volume": volume.id, "entry": entry, "snapshots": snapshots, "backing": rows, "page": page})
}

#[test]
fn native_backing_authority_vectors_match_the_compiled_web_fixture() {
    for (kind, name) in [
        (WebBacking::Kv, "backing-authority-vectors.json"),
        (WebBacking::Objects, "object-authority-vectors.json"),
        (WebBacking::Log, "log-authority-vectors.json"),
    ] {
        let expected = backing_authority_fixture(kind);
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("platform/web/tests").join(name);
        if std::env::var_os("PLAYBIT_REGENERATE_BACKING_AUTHORITY_FIXTURE").is_some() {
            std::fs::write(&fixture, serde_json::to_vec(&expected).unwrap()).unwrap();
        }
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
        assert_eq!(expected, actual);
    }
}

fn web_authority_rows(
    database: kvdb::Db<kvdb::storage::mem::MemStorage>,
) -> (kvdb::Db<kvdb::storage::mem::MemStorage>, WebAuthorityRows) {
    use kvdb::storage::{Storage, StorageTxn};
    database.verify().unwrap();
    let storage = database.close();
    let mut rows = Vec::new();
    {
        let read = storage.begin(false).unwrap();
        let mut key = Vec::new();
        while let Some(next) = read.next_key(&key, !key.is_empty()).unwrap() {
            assert!(rows.len() < 512);
            rows.push((next.clone(), read.get(&next).unwrap().unwrap().to_vec()));
            key = next;
        }
    }
    (kvdb::Db::open(storage, &Default::default()).unwrap(), rows)
}

#[test]
fn native_blob_authority_vectors_match_the_compiled_web_fixture() {
    use playbit::store::{
        EntryId, MetadataPolicy, Volume,
        object_contract::{Acl, EntryAcl, Grant, Permission, Principal, Subject},
        volume_index::{self, BlobVersion, EntryInfo, EntryKind},
    };
    let volume = Volume {
        id: Id::from_bytes([0xa1; 16]),
        owner: Id::from_bytes([9; 16]),
        name: "Root".into(),
        personal: true,
        share_key_with_service: true,
    };
    let account = Id::from_bytes([1; 16]);
    let entry = EntryId::from_bytes([0x71; 15]).unwrap();
    let revision = EntryId::from_bytes([0x72; 15]).unwrap();
    let acl = |permission, service| {
        let mut grants = vec![
            Grant {
                subject: Subject::Principal(Principal::Human(account)),
                permissions: vec![permission],
            },
            Grant {
                subject: Subject::Principal(Principal::Human(volume.owner)),
                permissions: vec![Permission::Read, Permission::Write],
            },
        ];
        if service {
            grants.push(Grant {
                subject: Subject::Principal(playbit::store::PLAYBIT_REPLICA_PRINCIPAL),
                permissions: vec![Permission::Read],
            });
        }
        EntryAcl::Replace(Acl { grants })
    };
    let policy: MetadataPolicy = serde_json::from_value(serde_json::json!({
        "volume": volume, "permissions": {"grants": []},
        "provisioned_versions": [playbit::identifier::encode(&[0x02; 32])]
    }))
    .unwrap();
    let mut database = kvdb::Db::create(
        kvdb::storage::mem::MemStorage::new(),
        &kvdb::CreateOptions::syncable(DatabaseId::Index.hash_key(&[9; 32], volume.id), 7),
    )
    .unwrap();
    policy.write(&database, 1000).unwrap();
    let version = BlobVersion {
        address: EntryAddress { volume: volume.id, entry },
        revision,
        len_bytes: 3,
        digest: [0x73; 32],
        encrypted_key: [0x74; 32],
    };
    let mut tx = database.begin_write(1000).unwrap();
    volume_index::create(
        &mut tx,
        entry,
        &EntryInfo {
            namespace: Default::default(),
            origin: playbit::store::volume_index::namespace::Origin {
                installation: playbit::store::Id::from_bytes([42; 16]),
                label: None,
            },
            name: "supplied.txt".into(),
            created_revision: revision,
            protocol: None,
            initial_permissions: acl(Permission::Write, true),
            kind: EntryKind::Blob {
                len_bytes: version.len_bytes,
                digest: version.digest,
                encrypted_key: version.encrypted_key,
                published_at_micros: 1,
            },
        },
    )
    .unwrap();
    let mut pending = b"blob-pending/".to_vec();
    pending.extend_from_slice(entry.as_bytes());
    pending.extend_from_slice(revision.as_bytes());
    tx.local_indexes().put(&pending, &serde_json::to_vec(&version).unwrap()).unwrap();
    tx.commit().unwrap();
    let mut snapshots = BTreeMap::new();
    let mut receipts = BTreeMap::new();
    for (name, permission, service) in [
        ("write_only", Permission::Write, true),
        ("append_only", Permission::Append, true),
        ("no_service", Permission::Write, false),
        ("read_only", Permission::Read, true),
    ] {
        let mut tx = database.begin_write(1000).unwrap();
        volume_index::set_permissions(&mut tx, entry, &acl(permission, service)).unwrap();
        tx.commit().unwrap();
        let current =
            playbit::store::DatabasePolicy::read(&database, DatabaseId::Entry(entry)).unwrap();
        assert!(current.volume.authorize(account, Permission::Read).is_err());
        assert!(current.authorize(Principal::Human(account), permission).is_ok());
        let (next, rows) = web_authority_rows(database);
        database = next;
        snapshots.insert(name, rows);
        let read = database.begin_read().unwrap();
        let accepted = volume_index::namespace::source::cut::accepted(&read).unwrap();
        let (namespace, evidence) =
            volume_index::namespace::receipt::Receipt::new(None, &accepted).unwrap();
        receipts.insert(
            name,
            serde_json::json!({
                "namespace": namespace, "root": read.root_hash().unwrap(), "evidence": evidence,
            }),
        );
    }
    let mut tx = database.begin_write(1000).unwrap();
    volume_index::retire(
        &mut tx,
        entry,
        EntryId::from_bytes([250; 15]).unwrap(),
        &volume_index::namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit().unwrap();
    let (database, rows) = web_authority_rows(database);
    snapshots.insert("retired", rows);
    let mut tx = database.begin_write(1000).unwrap();
    volume_index::set_permissions(&mut tx, entry, &acl(Permission::Write, true)).unwrap();
    tx.commit().unwrap();
    let (_, rows) = web_authority_rows(database);
    snapshots.insert("retired_write_only", rows);
    let mut revocations = BTreeMap::new();
    for (name, id) in [("root", volume.id), ("session", Id::from_bytes([0xa2; 16]))] {
        let database = kvdb::Db::create(
            kvdb::storage::mem::MemStorage::new(),
            &kvdb::CreateOptions::syncable(DatabaseId::Revocations.hash_key(&[9; 32], id), 7),
        )
        .unwrap();
        let mut tx = database.begin_write(1000).unwrap();
        tx.put(account.to_string().as_bytes(), br#""removed""#).unwrap();
        tx.commit().unwrap();
        let (_, rows) = web_authority_rows(database);
        revocations.insert(name, rows);
    }
    // A later accepted content source exercises aggregate retained-payload
    // accounting in the compiled browser, independently of its local heads.
    {
        use kvdb::storage::{Storage, StorageTxn};
        let storage = kvdb::storage::mem::MemStorage::new();
        let mut raw = storage.begin(true).unwrap();
        for (key, value) in &snapshots["read_only"] {
            raw.put(key, value).unwrap();
        }
        raw.commit().unwrap();
        let database = kvdb::Db::open(storage, &Default::default()).unwrap();
        let mut tx = database.begin_write(1000).unwrap();
        volume_index::namespace::source::cut::retain_accepted(&mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let (database, rows) = web_authority_rows(database);
        snapshots.insert("confirmed", rows);
        let mut tx = database.begin_write(1001).unwrap();
        volume_index::update_content(
            &mut tx,
            entry,
            EntryKind::Blob {
                len_bytes: 4,
                digest: [0x75; 32],
                encrypted_key: [0x76; 32],
                published_at_micros: 2,
            },
            EntryId::from_bytes([0x77; 15]).unwrap(),
            &volume_index::namespace::Origin {
                installation: Id::from_bytes([42; 16]),
                label: None,
            },
        )
        .unwrap();
        volume_index::namespace::source::cut::retain_accepted(&mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let (database, rows) = web_authority_rows(database);
        snapshots.insert("replacement", rows);
        let read = database.begin_read().unwrap();
        let accepted = volume_index::namespace::source::cut::accepted(&read).unwrap();
        let (namespace, evidence) =
            volume_index::namespace::receipt::Receipt::new(None, &accepted).unwrap();
        receipts.insert(
            "replacement",
            serde_json::json!({
                "namespace": namespace, "root": read.root_hash().unwrap(), "evidence": evidence,
            }),
        );
    }
    // File path operations require directory Read independently of the leaf
    // override. Keep the existing known-ID upload vectors and add path grants.
    for (name, source) in [("file_write", "write_only"), ("file_read", "read_only")] {
        use kvdb::storage::{Storage, StorageTxn};
        let storage = kvdb::storage::mem::MemStorage::new();
        let mut raw = storage.begin(true).unwrap();
        for (key, value) in &snapshots[source] {
            raw.put(key, value).unwrap();
        }
        raw.commit().unwrap();
        let database = kvdb::Db::open(storage, &Default::default()).unwrap();
        let directory_policy: MetadataPolicy = serde_json::from_value(serde_json::json!({
            "volume": volume,
            "permissions": {"grants": [{
                "subject": Subject::Principal(Principal::Human(account)),
                "permissions": [Permission::Read]
            }]},
            "provisioned_versions": [playbit::identifier::encode(&[0x02; 32])]
        }))
        .unwrap();
        directory_policy.write(&database, 1000).unwrap();
        let (database, rows) = web_authority_rows(database);
        snapshots.insert(name, rows);
        let read = database.begin_read().unwrap();
        let accepted = volume_index::namespace::source::cut::accepted(&read).unwrap();
        let (namespace, evidence) =
            volume_index::namespace::receipt::Receipt::new(None, &accepted).unwrap();
        receipts.insert(
            name,
            serde_json::json!({
                "namespace": namespace, "root": read.root_hash().unwrap(), "evidence": evidence,
            }),
        );
    }
    let expected = serde_json::json!({"snapshots": snapshots, "receipts": receipts, "revocations": revocations, "reference": {
        "volume": volume.id, "entry": entry, "revision": revision, "len_bytes": 3,
        "digest": "73".repeat(32), "encrypted_key": "74".repeat(32), "offset": 0, "limit": 0
    }});
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("platform/web/tests/blob-authority-vectors.json");
    if std::env::var_os("PLAYBIT_REGENERATE_BLOB_AUTHORITY_FIXTURE").is_some() {
        std::fs::write(&fixture, serde_json::to_vec(&expected).unwrap()).unwrap();
    }
    let actual: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
    assert_eq!(expected, actual);
}

#[test]
fn native_log_index_matches_the_compiled_web_fixture() {
    use playbit::store::{
        MetadataPolicy,
        volume_index::{self, DatabaseProtocol, EntryInfo},
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("platform/web/tests/bloblog-vectors.json");
    let mut fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let volume = Volume {
        id: serde_json::from_value(fixture["volume"].clone()).unwrap(),
        owner: Id::from_bytes([1; 16]),
        name: "Root".into(),
        personal: true,
        share_key_with_service: true,
    };
    let entry: EntryId = serde_json::from_value(fixture["entry"].clone()).unwrap();
    let policy: MetadataPolicy = serde_json::from_value(serde_json::json!({
        "volume": volume, "permissions": {"grants": []}, "provisioned_versions": []
    }))
    .unwrap();
    let database = kvdb::Db::create(
        kvdb::storage::mem::MemStorage::new(),
        &kvdb::CreateOptions::syncable(DatabaseId::Index.hash_key(&[9; 32], volume.id), 42),
    )
    .unwrap();
    policy.write(&database, 1000).unwrap();
    let mut tx = database.begin_write(1000).unwrap();
    // Fixed parent identities keep the native/browser protocol vector reproducible.
    let parent = EntryId::from_bytes([0x76; 15]).unwrap();
    volume_index::create(
        &mut tx,
        parent,
        &EntryInfo {
            namespace: Default::default(),
            origin: volume_index::namespace::Origin {
                installation: Id::from_bytes([42; 16]),
                label: None,
            },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::from_bytes([0x77; 15]).unwrap(),
            name: "logs".into(),
            kind: EntryKind::Directory {},
            protocol: None,
        },
    )
    .unwrap();
    volume_index::create(
        &mut tx,
        entry,
        &EntryInfo {
            namespace: volume_index::namespace::Birth {
                parent: Some(parent),
                ..Default::default()
            },
            origin: volume_index::namespace::Origin {
                installation: Id::from_bytes([42; 16]),
                label: None,
            },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::from_bytes([0x78; 15]).unwrap(),
            name: "activity".into(),
            kind: EntryKind::KvDb {},
            protocol: Some(DatabaseProtocol::BlobLog),
        },
    )
    .unwrap();
    volume_index::mark_backing_ready(&mut tx, entry).unwrap();
    tx.commit().unwrap();
    let (_, rows) = web_authority_rows(database);
    let expected = serde_json::to_value(rows).unwrap();
    if std::env::var_os("PLAYBIT_REGENERATE_BACKING_AUTHORITY_FIXTURE").is_some() {
        fixture["index"] = expected.clone();
        let entries = fixture["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|page| page["entries"].as_array().unwrap().iter())
            .map(|entry| {
                serde_json::from_value::<playbit::store::bloblog::Entry>(entry.clone()).unwrap()
            })
            .collect::<Vec<_>>();
        let hash = DatabaseId::Entry(entry).hash_key(&[9; 32], volume.id);
        let local = kvdb::Db::create(
            kvdb::storage::mem::MemStorage::new(),
            &kvdb::CreateOptions::syncable(hash, 44),
        )
        .unwrap();
        let log = playbit::store::bloblog::Log {
            database: &local,
            volume: volume.id,
            entry,
            encryption_key: None,
            now: 1000,
        };
        log.apply(&entries[..1], true).unwrap();
        fixture["local_backing"] = serde_json::to_value(web_authority_rows(local).1).unwrap();
        let remote = kvdb::Db::create(
            kvdb::storage::mem::MemStorage::new(),
            &kvdb::CreateOptions::syncable(hash, 43),
        )
        .unwrap();
        let log = playbit::store::bloblog::Log {
            database: &remote,
            volume: volume.id,
            entry,
            encryption_key: None,
            now: 1000,
        };
        for entries in entries.chunks(32) {
            log.apply(entries, false).unwrap();
        }
        let mut pages = Vec::new();
        let mut cursor = None;
        loop {
            let page = log.get(cursor.as_ref()).unwrap();
            cursor = Some(page.cursor.clone());
            let more = page.more;
            pages.push(page);
            if !more {
                break;
            }
        }
        fixture["pages"] = serde_json::to_value(pages).unwrap();
        std::fs::write(&path, serde_json::to_vec(&fixture).unwrap()).unwrap();
    }
    assert_eq!(fixture["index"], expected);
}

fn fixture() -> (tempfile::TempDir, LocalStore, Volume, Access, EntryAddress) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let declaration = playbit::package::VolumeInitializers::from_json(include_bytes!(
        "../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(playbit::package::AutomaticVolumeRole::Session);
    let volume =
        store.initialize_root(Id::generate().unwrap(), "Tasks", false, &declaration).unwrap();
    let access = Access::owner(store.account().id);
    let address = EntryAddress { volume: volume.id, entry: declaration.databases[0].id };
    (directory, store, volume, access, address)
}
fn title(text: &str) -> BTreeMap<Id, Option<Vec<Value>>> {
    BTreeMap::from([(Id::from_bytes([2; 16]), Some(vec![Value::Text(text.into())]))])
}

#[test]
fn ordinary_kv_is_independent_by_id_and_append_never_replaces_or_revives() {
    let (directory, store, volume, owner, object) = fixture();
    store.set_permissions(&owner, volume.id, Acl::default()).unwrap();
    let entry = store.create_database(&owner, volume.id, "settings", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    let writer = Access::owner(Id::from_bytes([0x41; 16]));
    let reader = Access::owner(Id::from_bytes([0x42; 16]));
    let append = Access::owner(Id::from_bytes([0x43; 16]));
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(Principal::Human(Id::from_bytes([0x41; 16]))),
                        permissions: vec![Permission::Write],
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(Id::from_bytes([0x42; 16]))),
                        permissions: vec![Permission::Read],
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(Id::from_bytes([0x43; 16]))),
                        permissions: vec![Permission::Append],
                    },
                ],
            }),
        )
        .unwrap();
    assert!(store.entries(&reader, volume.id, None, 64).is_err());
    assert!(store.resolve_entry(&reader, volume.id, "settings").is_err());
    store.kv_put(&writer, address, &[0, 0xff], b"first").unwrap();
    assert!(store.kv_read(&writer, address, &[0, 0xff], 0, 10, None).is_err());
    assert_eq!(store.kv_read(&reader, address, &[0, 0xff], 0, 10, None).unwrap().bytes, b"first");
    store.kv_put(&append, address, b"new", b"once").unwrap();
    let root = store.kv_keys(&reader, address, None, None, 64).unwrap().root;
    store.kv_put(&append, address, b"new", b"once").unwrap();
    assert_eq!(store.kv_keys(&reader, address, None, None, 64).unwrap().root, root);
    assert!(store.kv_put(&append, address, b"new", b"twice").is_err());
    assert!(store.kv_delete(&append, address, b"new").is_err());
    store.kv_delete(&writer, address, b"new").unwrap();
    assert!(store.kv_put(&append, address, b"new", b"once").is_err());
    assert!(store.kv_put(&writer, object, b"bad", b"value").is_err());
    let log = store.create_bloblog(&owner, volume.id, "log").unwrap();
    assert!(
        store
            .kv_put(&owner, EntryAddress { volume: volume.id, entry: log.id }, b"bad", b"value")
            .is_err()
    );
    assert!(
        store
            .kv_put(&writer, address, b"large", &vec![0; playbit::store::key_value::MAX_BYTES + 1])
            .is_err()
    );
    store
        .kv_put(&writer, address, b"maximum", &vec![0; playbit::store::key_value::MAX_BYTES])
        .unwrap();
    store.set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    store.rename_entry(&owner, volume.id, entry.id, "renamed").unwrap();
    assert_eq!(store.kv_read(&owner, address, &[0, 0xff], 0, 10, None).unwrap().bytes, b"first");
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.kv_read(&owner, address, &[0, 0xff], 0, 10, None).unwrap().bytes, b"first");
    store.remove_entry(&owner, volume.id, entry.id).unwrap();
    let replacement =
        store.create_database(&owner, volume.id, "renamed", EntryKind::KvDb {}).unwrap();
    assert_ne!(replacement.id, entry.id);
    assert!(store.kv_read(&owner, address, &[0, 0xff], 0, 10, None).is_err());
    assert!(
        store
            .kv_keys(
                &owner,
                EntryAddress { volume: volume.id, entry: replacement.id },
                None,
                None,
                64
            )
            .unwrap()
            .keys
            .is_empty()
    );
}
fn stage(source: &LocalStore, target: &LocalStore, volume: Id) -> ReceivedVolume {
    let received = target.prepare_received_volume(volume).unwrap();
    let mut after = None;
    loop {
        let page = source.replication_database_page(volume, after).unwrap();
        for database in page.databases {
            source
                .with_replica(volume, database, |source| {
                    received.with_database(database, |target| {
                        if database == DatabaseId::Index {
                            playbit::store::merge_index_replica(
                                &std::env::temp_dir(),
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            playbit::store::merge_replica(
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
            received.finish_database(database).unwrap();
        }
        let Some(next) = page.next else {
            return received;
        };
        after = Some(next);
    }
}
fn pair() -> (tempfile::TempDir, LocalStore, tempfile::TempDir, LocalStore, Volume, Access, Access)
{
    let (a_dir, alice, volume, owner, _) = fixture();
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
    let build = playbit::identifier::encode(&[0xab; 32]);
    alice.provision_version(&owner, volume.id, &build).unwrap();
    let session = alice
        .create_session(
            &playbit::package::AppIdentity { publisher: alice.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            volume.id,
        )
        .unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let bob = LocalStore::open(b_dir.path()).unwrap();
    bob.import_invitation_link(
        &alice.invitation(&session.id, playbit::release_channel::ReleaseChannel::Public).unwrap(),
        &alice.session_envelope(&session.id).unwrap(),
        playbit::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    let participant = Access::session(bob.account().id, &session.id);
    let root = stage(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &root).unwrap();
    (a_dir, alice, b_dir, bob, volume, owner, participant)
}

#[test]
fn rename_preserves_schema_objects_backing_and_complete_references() {
    let (directory, store, volume, owner, address) = fixture();
    let record = store
        .query_objects(&owner, address, Id::from_bytes([1; 16]), None, 64)
        .unwrap()
        .objects
        .remove(0);
    let schema = store.schema(&owner, address).unwrap();
    let before = store.replica_root(volume.id, DatabaseId::Entry(address.entry)).unwrap();
    store.rename_entry(&owner, volume.id, address.entry, "work/tasks").unwrap();
    assert_eq!(store.resolve_entry(&owner, volume.id, "work/tasks").unwrap().id, address.entry);
    assert!(store.resolve_entry(&owner, volume.id, "data").is_err());
    assert_eq!(store.read_object(&owner, address, record.object_id).unwrap(), record);
    assert_eq!(store.schema(&owner, address).unwrap(), schema);
    assert_eq!(store.replica_root(volume.id, DatabaseId::Entry(address.entry)).unwrap(), before);
    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    assert_eq!(reopened.read_object(&owner, address, record.object_id).unwrap(), record);
}

#[test]
fn databases_own_separate_content_even_with_equal_type_ids() {
    let (_directory, store, volume, owner, first) = fixture();
    let second = store
        .create_database(
            &owner,
            volume.id,
            "second",
            EntryKind::ObjectDb { schema: store.schema(&owner, first).unwrap() },
        )
        .unwrap();
    let second = EntryAddress { volume: volume.id, entry: second.id };
    let a = store
        .create_object_with_fields(&owner, first, Id::from_bytes([1; 16]), &title("first"))
        .unwrap();
    let b = store
        .create_object_with_fields(&owner, second, Id::from_bytes([1; 16]), &title("second"))
        .unwrap();
    assert!(store.read_object(&owner, first, b.object_id).is_err());
    assert!(store.read_object(&owner, second, a.object_id).is_err());
    assert_eq!(
        store.query_objects(&owner, second, Id::from_bytes([1; 16]), None, 64).unwrap().objects,
        [b]
    );
}

#[test]
fn offline_competing_names_converge_without_losing_entries() {
    let (_a, alice, b_dir, bob, volume, owner, participant) = pair();
    let a = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    let b = bob.create_database(&participant, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    let to_alice = stage(&bob, &alice, volume.id);
    let to_bob = stage(&alice, &bob, volume.id);
    alice.adopt_received_volume(&owner, &to_alice).unwrap();
    bob.adopt_received_volume(&participant, &to_bob).unwrap();
    for (store, access) in [(&alice, &owner), (&bob, &participant)] {
        assert_eq!(store.resolve_entry(access, volume.id, "notes").unwrap().id, a.id.max(b.id));
        assert!(
            store.entry_details(access, volume.id, a.id.min(b.id)).unwrap().entry.name_conflict
        );
        assert_eq!(store.entries(access, volume.id, None, 64).unwrap().entries.len(), 3);
    }
    drop(to_bob);
    drop(bob);
    let bob = LocalStore::open(b_dir.path()).unwrap();
    assert_eq!(bob.resolve_entry(&participant, volume.id, "notes").unwrap().id, a.id.max(b.id));
}

#[test]
fn concurrent_rename_delete_and_recreate_keep_ids_distinct() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    bob.adopt_received_volume(&participant, &stage(&alice, &bob, volume.id)).unwrap();
    let mut feed = bob.take_change_receiver().unwrap();
    feed.drain();
    bob.rename_entry(&participant, volume.id, entry.id, "delayed").unwrap();
    let delayed = feed.drain().records.remove(0);
    alice.remove_entry(&owner, volume.id, entry.id).unwrap();
    let fresh = alice.create_database(&owner, volume.id, "notes", EntryKind::KvDb {}).unwrap();
    for _ in 0..2 {
        alice.merge_received_delta(&owner, volume.id, DatabaseId::Index, delayed.bytes()).unwrap();
    }
    bob.adopt_received_volume(&participant, &stage(&alice, &bob, volume.id)).unwrap();
    for (store, access) in [(&alice, &owner), (&bob, &participant)] {
        assert_eq!(store.resolve_entry(access, volume.id, "notes").unwrap().id, fresh.id);
        assert!(store.resolve_entry(access, volume.id, "delayed").is_err());
        assert!(store.entry_details(access, volume.id, entry.id).unwrap().entry.deleted);
        let history = store
            .entry_revisions(access, EntryAddress { volume: volume.id, entry: entry.id }, None, 64)
            .unwrap();
        assert_ne!(alice.installation(), bob.installation());
        assert_eq!(history.revisions.len(), 3);
        for revision in history.revisions {
            let expected = match revision.change {
                playbit::store::volume_index::EntryChange::Created { .. }
                | playbit::store::volume_index::EntryChange::Delete {} => alice.installation(),
                playbit::store::volume_index::EntryChange::Name { .. } => bob.installation(),
                _ => panic!("unexpected fixture revision"),
            };
            assert_eq!(
                revision.origin.installation, expected,
                "replication retains the originating installation"
            );
        }
    }
}

#[test]
fn overrides_replace_owner_content_access_without_hiding_conflicts() {
    let (_directory, store, volume, owner, address) = fixture();
    store
        .set_entry_permissions(&owner, volume.id, address.entry, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(store.schema(&owner, address).is_err());
    assert!(store.resolve_entry(&owner, volume.id, "data").is_err());
    let details = store.entry_details(&owner, volume.id, address.entry).unwrap();
    assert!(details.manage_permissions);
    assert!(!details.readable);
    assert!(!details.writable);
    assert_eq!(store.entries(&owner, volume.id, None, 64).unwrap().entries.len(), 1);
    store.set_entry_permissions(&owner, volume.id, address.entry, EntryAcl::Inherit).unwrap();
    store.schema(&owner, address).unwrap();
}

#[test]
fn independent_blob_content_history_and_shared_pins_survive_restart() {
    let (directory, store, volume, owner, _) = fixture();
    let first = store.create_blob(&owner, volume.id, "photo", b"shared").unwrap();
    let second = store.create_blob(&owner, volume.id, "copy", b"shared").unwrap();
    let address = EntryAddress { volume: volume.id, entry: first.id };
    let old = store.blob_version(&owner, address, None).unwrap();
    store.rename_entry(&owner, volume.id, first.id, "photos/photo").unwrap();
    assert_eq!(store.entry(&owner, volume.id, first.id).unwrap().info.kind, first.info.kind);
    store.write_blob(&owner, volume.id, first.id, b"changed").unwrap();
    assert_eq!(store.read_blob_version(&owner, &old).unwrap(), b"shared");
    store.remove_entry(&owner, volume.id, first.id).unwrap();
    assert_eq!(store.read_blob_version(&owner, &old).unwrap(), b"shared");
    store.evict_blobs(0).unwrap();
    assert_eq!(store.read_blob_entry(&owner, volume.id, second.id).unwrap(), b"shared");
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.read_blob_entry(&owner, volume.id, second.id).unwrap(), b"shared");
    assert!(store.entry_details(&owner, volume.id, first.id).unwrap().entry.deleted);
}

#[test]
fn write_only_entry_edits_return_only_changed_fields() {
    let (_directory, store, volume, owner, address) = fixture();
    let writer = Id::from_bytes([8; 16]);
    store
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(writer)),
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    let record = store
        .query_objects(&owner, address, Id::from_bytes([1; 16]), None, 64)
        .unwrap()
        .objects
        .remove(0);
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            address.entry,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(writer)),
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let writer = Access::owner(writer);
    let edited = store.edit_fields(&writer, address, record.object_id, &title("mine")).unwrap();
    assert_eq!(edited.fields.len(), 1);
    assert_eq!(
        edited.fields[&Id::from_bytes([2; 16])].values,
        title("mine")[&Id::from_bytes([2; 16])]
    );
    assert!(store.read_object(&writer, address, record.object_id).is_err());
}

#[test]
fn zero_or_noncanonical_entry_ids_are_rejected() {
    assert!(EntryId::from_bytes([0; 15]).is_err());
    for text in [
        "1",
        "AB".repeat(15).as_str(),
        playbit::identifier::encode(&[0x00; 15]).as_str(),
        playbit::identifier::encode(&[0xab; 16]).as_str(),
    ] {
        assert!(EntryId::parse(text).is_err());
    }
}
