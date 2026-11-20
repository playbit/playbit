#![allow(clippy::unwrap_used, reason = "Tests assert successful header fixtures.")]
use super::*;
use crate::store::object_contract::{FieldDeclaration, TypeDeclaration, ValueType};
use kvdb::{CreateOptions, Db, storage::mem::MemStorage};
use std::collections::BTreeMap;

const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;

fn id(number: u64) -> EntryId {
    let mut bytes = [0; 15];
    bytes[7..].copy_from_slice(&number.to_be_bytes());
    EntryId::from_bytes(bytes).unwrap()
}

fn database() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap()
}

fn birth(number: u64, kind: EntryKind) -> EntryInfo {
    EntryInfo {
        namespace: Default::default(),
        origin: crate::store::volume_index::namespace::Origin {
            installation: crate::store::Id::from_bytes([42; 16]),
            label: None,
        },
        initial_permissions: EntryAcl::Inherit,
        created_revision: id(1000 + number),
        name: format!("entries-{number:04}"),
        protocol: (!matches!(kind, EntryKind::Blob { .. })).then_some(DatabaseProtocol::Kvdb),
        kind,
    }
}

#[test]
fn identity_pages_keep_every_birth_and_pinned_retirement_with_exclusive_cursors() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    for number in 1..=130 {
        let mut birth = birth(number, EntryKind::KvDb {});
        if number == 8 {
            birth.protocol = Some(DatabaseProtocol::BlobLog);
        }
        create(&mut tx, id(number), &birth).unwrap();
    }
    // Delayed facts with no birth must never become catalog identities.
    tx.put(&meta_key(NAME, id(999)), b"pending").unwrap();
    tx.commit().unwrap();
    let pinned = IndexSnapshot::new(database.begin_read().unwrap());
    let mut tx = database.begin_write(WALL + 1).unwrap();
    retire(
        &mut tx,
        id(3),
        id(3000),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit().unwrap();
    let mut tx = database.begin_write(WALL + 2).unwrap();
    create(&mut tx, id(131), &birth(131, EntryKind::KvDb {})).unwrap();
    tx.commit().unwrap();
    for limit in [1, 32, 64] {
        let mut after = None;
        let mut observed = Vec::new();
        loop {
            let page = pinned.headers(after, limit).unwrap();
            assert!(page.entries.len() <= limit);
            assert!(page.entries.iter().all(|header| !header.deleted));
            observed.extend(page.entries.iter().map(|header| header.id));
            let Some(next) = page.next else { break };
            assert_eq!(Some(next), observed.last().copied());
            after = Some(next);
        }
        assert_eq!(observed, (1..=130).map(id).collect::<Vec<_>>());
    }
    assert_eq!(pinned.header(id(8)).unwrap().protocol, Some(DatabaseProtocol::BlobLog));
    let current = IndexSnapshot::new(database.begin_read().unwrap());
    assert!(current.header(id(3)).unwrap().deleted);
    assert!(current.entry_policy(id(3)).is_err());
    assert_eq!(current.headers(Some(id(130)), 64).unwrap().entries[0].id, id(131));
    assert!(current.headers(None, 0).is_err());
    assert!(current.headers(None, 65).is_err());
    database.verify().unwrap();
}

fn field(number: u64, name: String) -> (Id, FieldDeclaration) {
    let mut bytes = [1; 16];
    bytes[8..].copy_from_slice(&(number + 1).to_be_bytes());
    (
        Id::from_bytes(bytes),
        FieldDeclaration { name, value_type: ValueType::Text, repeated: false, deprecated: false },
    )
}

fn schema(fields: BTreeMap<Id, FieldDeclaration>) -> Schema {
    Schema {
        types: BTreeMap::from([(
            Id::from_bytes([4; 16]),
            TypeDeclaration { name: "Row".into(), deprecated: false, fields },
        )]),
    }
}

#[test]
fn headers_and_backing_readiness_never_materialize_additive_schema() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    create(&mut tx, id(1), &birth(1, EntryKind::ObjectDb { schema: schema(BTreeMap::new()) }))
        .unwrap();
    mark_backing_ready(&mut tx, id(1)).unwrap();
    tx.commit().unwrap();
    for batch in 0..32u64 {
        let fields = (batch * 32..(batch + 1) * 32)
            .map(|number| field(number, format!("f{number}")))
            .collect();
        let mut tx = database.begin_write(WALL + batch + 1).unwrap();
        establish_schema(
            &mut tx,
            id(1),
            &schema(fields),
            id(2000 + batch),
            &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
        )
        .unwrap();
        tx.commit().unwrap();
    }
    schema::MATERIALIZATION_VISITS.with(|visits| visits.set(0));
    let index = IndexSnapshot::new(database.begin_read().unwrap());
    assert_eq!(index.headers(None, 64).unwrap().entries[0].kind, EntryType::ObjectDb);
    assert_eq!(index.entry_policy(id(1)).unwrap().kind, EntryType::ObjectDb);
    assert!(index.backing_ready(id(1)).unwrap());
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 0);
    let EntryKind::ObjectDb { schema: detailed } = index.entry(id(1)).unwrap().info.kind else {
        panic!()
    };
    assert_eq!(detailed.types[&Id::from_bytes([4; 16])].fields.len(), 1024);
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 1024);
}

fn large_initial_schema() -> Schema {
    let mut fields = BTreeMap::new();
    let mut prior = schema(fields.clone());
    for number in 0..1024 {
        let (field_id, declaration) = field(number, "x".repeat(255));
        fields.insert(field_id, declaration);
        let next = schema(fields.clone());
        let mut value = birth(1, EntryKind::ObjectDb { schema: next.clone() });
        // The last of 65 local births also retains the preceding birth IDs.
        // Leave room for that required context when sizing this page fixture.
        value.namespace.observed = (1..=64).map(id).collect();
        match value.canonical() {
            Ok(bytes) => {
                assert!(bytes.len() <= MAX_ENTRY_RECORD_BYTES);
                prior = next;
            }
            Err(Error::LimitExceeded) => return prior,
            Err(error) => panic!("large-schema fixture failed: {error}"),
        }
    }
    panic!("fixture must reach the existing birth byte ceiling");
}

#[test]
fn byte_bounded_header_pages_do_not_skip_large_births() {
    let schema = large_initial_schema();
    let mut largest = birth(1, EntryKind::ObjectDb { schema: schema.clone() });
    let bytes = largest.canonical().unwrap();
    largest.namespace.observed = (1..=64).map(id).collect();
    assert!(largest.canonical().unwrap().len() > MAX_ENTRY_RECORD_BYTES - 1024);
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    for number in 1..=65 {
        create(&mut tx, id(number), &birth(number, EntryKind::ObjectDb { schema: schema.clone() }))
            .unwrap();
    }
    tx.commit().unwrap();
    let index = IndexSnapshot::new(database.begin_read().unwrap());
    let mut after = None;
    let mut observed = Vec::new();
    loop {
        let page = index.headers(after, 64).unwrap();
        assert!(!page.entries.is_empty());
        assert!(page.entries.len() < 64);
        assert!(page.entries.len() * bytes.len() <= MAX_ENTRY_PAGE_BYTES);
        observed.extend(page.entries.into_iter().map(|header| header.id));
        let Some(next) = page.next else { break };
        after = Some(next);
    }
    assert_eq!(observed, (1..=65).map(id).collect::<Vec<_>>());
}

#[test]
fn malformed_and_oversized_births_fail_header_queries() {
    for bytes in [
        b"{}".to_vec(),
        vec![b'x'; MAX_ENTRY_RECORD_BYTES + 1],
        [b" ".as_slice(), &birth(1, EntryKind::KvDb {}).canonical().unwrap()].concat(),
    ] {
        let database = database();
        let mut tx = database.begin_write(WALL).unwrap();
        tx.put(&entry_key(id(1)), &bytes).unwrap();
        tx.commit().unwrap();
        let index = IndexSnapshot::new(database.begin_read().unwrap());
        assert!(index.header(id(1)).is_err());
        assert!(index.headers(None, 64).is_err());
    }
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    tx.put(&[ENTRY_ID_BYTE, 7], b"{}").unwrap();
    tx.commit().unwrap();
    assert!(IndexSnapshot::new(database.begin_read().unwrap()).headers(None, 64).is_err());
}

#[test]
fn pending_births_advance_header_pages_without_granting_app_access() {
    use crate::store::{DatabaseId, DatabasePolicy, ReplicaReader};
    let database = database();
    let owner = Id::from_bytes([2; 16]);
    MetadataPolicy::new(
        Volume {
            id: Id::from_bytes([1; 16]),
            owner,
            name: "Catalog".into(),
            personal: true,
            share_key_with_service: true,
        },
        Acl::default(),
    )
    .unwrap()
    .write(&database, WALL)
    .unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    create(&mut tx, id(65), &birth(65, EntryKind::KvDb {})).unwrap();
    for number in 1..=64 {
        let mut pending = birth(number, EntryKind::KvDb {});
        pending.namespace.observed = vec![id(9000)];
        tx.put(&entry_key(id(number)), &pending.canonical().unwrap()).unwrap();
    }
    namespace::source::cut::project(&tx).unwrap().write_current(&mut tx.local_indexes()).unwrap();
    tx.commit().unwrap();
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    let page = snapshot.headers(None, 64).unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.next, Some(id(64)));
    let next = snapshot.headers(page.next, 64).unwrap();
    assert_eq!(next.entries.iter().map(|entry| entry.id).collect::<Vec<_>>(), [id(65)]);
    assert_eq!(next.next, None);
    let sources = snapshot.source_headers(None, 64).unwrap();
    assert_eq!(sources.entries.len(), 64);
    assert_eq!(sources.next, Some(id(64)));
    assert!(matches!(snapshot.header(id(1)), Err(Error::NotFound(_))));
    assert!(matches!(
        DatabasePolicy::read(&database, DatabaseId::Entry(id(1))),
        Err(Error::NotFound(_))
    ));
    let incoming = DatabasePolicy::read_source(&database, DatabaseId::Entry(id(1))).unwrap();
    incoming.authorize_reader(ReplicaReader::Participant(owner)).unwrap();
    assert!(
        incoming.authorize_reader(ReplicaReader::Participant(Id::from_bytes([3; 16]))).is_err()
    );
}

#[test]
fn header_page_byte_budget_includes_qualified_deletion_sources() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    create(&mut tx, id(1), &birth(1, EntryKind::KvDb {})).unwrap();
    tx.commit().unwrap();
    let mut tx = database.begin_write(WALL + 1).unwrap();
    retire(&mut tx, id(1), id(2000), &birth(1, EntryKind::KvDb {}).origin).unwrap();
    tx.commit().unwrap();
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    let birth_bytes = snapshot.tx.get(&entry_key(id(1))).unwrap().unwrap().len();
    let identity = current::Identity::read(&snapshot.tx, id(1)).unwrap().unwrap();
    let bytes = identity.source_bytes();
    assert!(bytes > birth_bytes);
    assert!(snapshot.page_header(id(1), false, bytes - 1).unwrap().is_none());
    let (header, charged) = snapshot.page_header(id(1), false, bytes).unwrap().unwrap();
    assert!(header.unwrap().deleted);
    assert_eq!(charged, bytes);
}
