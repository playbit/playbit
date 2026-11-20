#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use playbit::store::object_contract::{
    Acl, EntryAcl, FieldEdit, Grant, ObjectFields, Permission, Schema, Subject,
};
use playbit::store::{
    DatabaseId, DatabasePolicy, EntryId, Id, Invitation, MAX_ENVELOPE_BYTES,
    MAX_REPLICA_DELTA_BYTES, MetadataPolicy, ReplicaReader, Volume, envelope_text,
    parse_envelope_text, parse_session_link, project_replica_delta, seal_blob_to, session_code,
};

fn id(byte: u8) -> Id {
    Id::from_bytes([byte; 16])
}

fn identifier<const N: usize>(data: &[u8]) {
    if data.len() <= playbit::identifier::MAX_TEXT
        && let Ok(text) = std::str::from_utf8(data)
        && let Ok(bytes) = playbit::identifier::decode::<N>(text)
    {
        assert_eq!(playbit::identifier::encode(&bytes), text);
    }
    if let Some(bytes) = data.get(..N) {
        let text = playbit::identifier::encode(bytes);
        assert_eq!(playbit::identifier::decode::<N>(&text).unwrap().as_slice(), bytes);
    }
}

fn envelope(data: &[u8]) {
    identifier::<15>(data);
    identifier::<16>(data);
    identifier::<32>(data);
    if let Ok(reference) = playbit::store::DatabaseReference::parse(data) {
        assert_eq!(reference.to_string().as_bytes(), data);
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(id) = Id::parse(text) {
            assert_eq!(id.to_string(), text);
        }
        if let Ok(bytes) = parse_envelope_text(text) {
            assert!(bytes.len() <= MAX_ENVELOPE_BYTES);
            assert_eq!(envelope_text(&bytes), text);
            let _ = Invitation::open(&[0; 16], &bytes);
        }
        if let Ok(seed) = parse_session_link(text, playbit::release_channel::ReleaseChannel::Public)
        {
            assert_eq!(
                parse_session_link(
                    &session_code(&seed),
                    playbit::release_channel::ReleaseChannel::Public
                )
                .expect("encoded seed"),
                seed
            );
        }
    }
    if let Some((seed, bytes)) = data.split_at_checked(16) {
        let seed: &[u8; 16] = seed.try_into().expect("fixed-width seed");
        let id = Id::from_bytes(*seed);
        assert_eq!(Id::parse(&id.to_string()).expect("encoded ID"), id);
        let _ = Invitation::open(seed, bytes);
    }
}

fn schemas(data: &[u8]) {
    if let Ok((mut previous, next)) = serde_json::from_slice::<(Schema, Schema)>(data) {
        let before = previous.clone();
        if previous.establish(&next).is_ok() {
            assert!(previous.validate().is_ok());
            assert!(before.validate_successor(&previous).is_ok());
            let established = previous.clone();
            previous.establish(&next).expect("repeated establishment");
            assert_eq!(previous, established);
        } else {
            assert_eq!(previous, before, "rejection leaves the schema unchanged");
        }
    }
    if let Ok(acl) = serde_json::from_slice::<Acl>(data) {
        assert!(!acl.permits(None, Permission::Write));
        assert!(!acl.permits(None, Permission::ManagePermissions));
        if acl.validate().is_err() {
            assert!(!acl.permits(None, Permission::Read));
        }
    }
}

fn fields(data: &[u8]) {
    let Ok((schema, mut fields, edit)) =
        serde_json::from_slice::<(Schema, ObjectFields, FieldEdit)>(data)
    else {
        return;
    };
    if schema.validate().is_err() {
        return;
    }
    let before = fields.clone();
    match fields.apply(&schema, &edit) {
        Ok(true) => {
            assert_eq!(fields.fields[&edit.field_id].stamp, edit.stamp);
            let updated = fields.clone();
            assert!(!fields.apply(&schema, &edit).expect("idempotent field edit"));
            assert_eq!(fields, updated);
            for (key, cell) in before.fields {
                if key != edit.field_id {
                    assert_eq!(fields.fields.get(&key), Some(&cell));
                }
            }
        }
        _ => assert_eq!(fields, before, "rejected or stale edits do not mutate"),
    }
}

fn policies() -> &'static [DatabasePolicy; 3] {
    static POLICIES: OnceLock<[DatabasePolicy; 3]> = OnceLock::new();
    POLICIES.get_or_init(|| {
        use playbit::store::volume_index::{self, DatabaseProtocol, EntryInfo, EntryKind};
        let index = kvdb::Db::create(
            kvdb::storage::mem::MemStorage::new(),
            &kvdb::CreateOptions::syncable([7; 32], 0),
        )
        .expect("valid in-memory index");
        let acl = Acl {
            grants: vec![Grant { subject: Subject::Public, permissions: vec![Permission::Read] }],
        };
        MetadataPolicy::new(
            Volume {
                id: id(1),
                name: "Fuzz".into(),
                owner: id(2),
                share_key_with_service: true,
                personal: true,
            },
            acl,
        )
        .expect("valid policy")
        .write(&index, 1)
        .expect("initial volume policy");
        let readable = EntryId::from_bytes([3; 15]).expect("nonzero entry");
        let denied = EntryId::from_bytes([4; 15]).expect("nonzero entry");
        let mut tx = index.begin_write(2).expect("fixture writer");
        for (entry, permissions) in
            [(readable, EntryAcl::Inherit), (denied, EntryAcl::Replace(Acl::default()))]
        {
            volume_index::create(
                &mut tx,
                entry,
                &EntryInfo {
                    namespace: Default::default(),
                    origin: playbit::store::volume_index::namespace::Origin {
                        installation: playbit::store::Id::from_bytes([42; 16]),
                        label: None,
                    },
                    name: entry.to_string(),
                    created_revision: entry,
                    kind: EntryKind::ObjectDb { schema: Schema::default() },
                    protocol: Some(DatabaseProtocol::Kvdb),
                    initial_permissions: permissions,
                },
            )
            .expect("valid named database");
        }
        tx.commit().expect("fixture index committed");
        [DatabaseId::Index, DatabaseId::Entry(readable), DatabaseId::Entry(denied)]
            .map(|database| DatabasePolicy::read(&index, database).expect("persisted policy"))
    })
}

fn deltas(data: &[u8]) {
    let [index, readable, denied] = policies();
    // Entry Read admits the whole backing; no object-level filtering remains.
    for policy in [index, readable] {
        if let Ok(projected) = project_replica_delta(policy, ReplicaReader::Service, data) {
            assert!(projected.len() <= MAX_REPLICA_DELTA_BYTES);
            assert_eq!(projected, data);
            assert_eq!(
                project_replica_delta(policy, ReplicaReader::Service, &projected)
                    .expect("projected delta is valid"),
                projected,
            );
        }
    }
    assert!(project_replica_delta(denied, ReplicaReader::Service, data).is_err());
}

fn blobs(data: &[u8]) {
    let Some((key, plaintext)) = data.split_at_checked(32) else { return };
    let key = key.try_into().expect("fixed-width blob key");
    let digest = *blake3::hash(plaintext).as_bytes();
    let seal = |volume| {
        let mut bytes = Vec::new();
        seal_blob_to(key, volume, &digest, plaintext.len() as u64, &mut &plaintext[..], &mut bytes)
            .expect("bounded blob");
        bytes
    };
    let first = seal(id(1));
    assert_eq!(
        first.len() as u64,
        playbit::store::blob_envelope_len(plaintext.len() as u64).unwrap()
    );
    assert_eq!(seal(id(1)), first);
    assert_ne!(seal(id(2)), first);
}

fn root_initialization(data: &[u8]) {
    use playbit::store::{
        InitialDatabaseKind, MAX_ROOT_INITIAL_OBJECTS, MAX_ROOT_INITIALIZATION_BYTES,
        RootInitialization,
    };
    if let Ok(declaration) = RootInitialization::from_json(data) {
        assert!(data.len() <= MAX_ROOT_INITIALIZATION_BYTES);
        assert!(declaration.databases.len() + declaration.blobs.len() <= 64);
        let mut object_count = 0;
        for database in &declaration.databases {
            if let InitialDatabaseKind::ObjectDb { schema, objects } = &database.kind {
                assert!(schema.validate().is_ok());
                object_count += objects.len();
            }
        }
        assert!(object_count <= MAX_ROOT_INITIAL_OBJECTS);
        assert!(declaration.permissions.validate().is_ok());
        let canonical = serde_json::to_vec(&declaration).expect("serializable declaration");
        assert!(canonical.len() <= MAX_ROOT_INITIALIZATION_BYTES);
        let repeated =
            RootInitialization::from_json(&canonical).expect("canonical declaration is valid");
        assert_eq!(serde_json::to_vec(&repeated).expect("serializable declaration"), canonical);
    }
}

fn object_database_sources(data: &[u8]) {
    use kvdb::{
        Clock,
        sync::{Delta, EntState},
    };
    let Some((key_length, rest)) = data.split_at_checked(1) else { return };
    let Some((key, value)) = rest.split_at_checked(key_length[0] as usize) else { return };
    let delta = Delta {
        key,
        local: EntState::Missing,
        remote: EntState::Present { value, clock: Clock { time: 100, count: 1 } },
    };
    let _ = playbit::store::object_database::validate_source(&delta, None, false);
    let _ = playbit::store::object_database::validate_source(&delta, Some(id(9)), true);
}

fn client_records(data: &[u8]) {
    use playbit::store::catalog::record;
    let _ = record::charge(data);
    if let Ok(value) = record::decode::<Volume>(data) {
        assert_eq!(record::rewrite(data, &value).unwrap(), data);
        assert_eq!(record::decode::<Volume>(&record::encode(&value).unwrap()).unwrap(), value);
    }
    let _ = record::read::<Volume>(data);
    let _ = record::decode::<playbit::store::volume_index::EntryInfo>(data);
    let _ = record::decode::<playbit::store::volume_index::EntryRevision>(data);
    let _ = record::decode::<playbit::store::object_contract::FieldCell>(data);
    let _ = playbit::store::account::Record::decode(data);
    let _ = playbit::store::account::Record::browser_plaintext(data);
}

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = playbit::store::account_key::parse(text);
    }
    let Some((&mode, data)) = data.split_first() else { return };
    // Bound serde allocation and per-iteration hashing; production admission
    // retains its own limits. Oversized replica deltas still reach rejection.
    if data.len() > MAX_REPLICA_DELTA_BYTES + 1 {
        return;
    }
    match mode % 8 {
        0 => envelope(data),
        1 => schemas(data),
        2 => fields(data),
        3 => deltas(data),
        4 => blobs(data),
        5 => root_initialization(data),
        6 => object_database_sources(data),
        _ => client_records(data),
    }
});
