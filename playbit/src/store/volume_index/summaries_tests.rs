#![allow(clippy::unwrap_used, reason = "Tests assert successful persistent summary fixtures.")]
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
fn birth(number: u64, name: String, schema: Schema) -> EntryInfo {
    EntryInfo {
        namespace: Default::default(),
        origin: crate::store::volume_index::namespace::Origin {
            installation: crate::store::Id::from_bytes([42; 16]),
            label: None,
        },
        initial_permissions: EntryAcl::Inherit,
        created_revision: id(1000 + number),
        name,
        kind: EntryKind::ObjectDb { schema },
        protocol: Some(DatabaseProtocol::Kvdb),
    }
}
fn schema(fields: BTreeMap<Id, FieldDeclaration>) -> Schema {
    Schema {
        types: BTreeMap::from([(
            Id::from_bytes([4; 16]),
            TypeDeclaration { name: "Row".into(), deprecated: false, fields },
        )]),
    }
}
fn fields(first: u64, count: u64, name_bytes: usize) -> BTreeMap<Id, FieldDeclaration> {
    (first..first + count)
        .map(|number| {
            let mut bytes = [1; 16];
            bytes[8..].copy_from_slice(&(number + 1).to_be_bytes());
            let mut name = format!("field{number:04}");
            name.extend(std::iter::repeat_n('x', name_bytes - name.len()));
            (
                Id::from_bytes(bytes),
                FieldDeclaration {
                    name,
                    value_type: ValueType::Text,
                    repeated: false,
                    deprecated: false,
                },
            )
        })
        .collect()
}

#[test]
fn large_database_summaries_never_materialize_additive_schema() {
    let database = database();
    for number in 1..=3 {
        let mut tx = database.begin_write(WALL).unwrap();
        create(
            &mut tx,
            id(number),
            &birth(number, format!("objects-{number}"), schema(BTreeMap::new())),
        )
        .unwrap();
        tx.commit().unwrap();
        for batch in 0..32 {
            let mut tx = database.begin_write(WALL + 1 + batch).unwrap();
            establish_schema(
                &mut tx,
                id(number),
                &schema(fields(batch * 32, 32, 255)),
                id(2000 + batch),
                &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
            )
            .unwrap();
            tx.commit().unwrap();
        }
    }
    schema::MATERIALIZATION_VISITS.with(|visits| visits.set(0));
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    let page = snapshot.page(None, 64).unwrap();
    assert_eq!(page.entries.len(), 3);
    assert!(page.entries.iter().all(|entry| entry.kind == EntryType::ObjectDb));
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 0);
    let listing =
        snapshot.directory_entries(DirectoryTarget::Root, None, None, 64, str::cmp).unwrap();
    assert!(matches!(listing, DirectoryListing::Page { entries, .. } if entries.len() == 3));
    assert_eq!(schema::MATERIALIZATION_VISITS.with(|visits| visits.get()), 0);
    assert!(serde_json::to_vec(&page).unwrap().len() < MAX_SERIALIZED_BYTES);
    let EntryKind::ObjectDb { schema } = snapshot.entry(id(1)).unwrap().info.kind else { panic!() };
    assert_eq!(schema.types[&Id::from_bytes([4; 16])].fields.len(), 1024);
}

#[test]
fn summary_pages_keep_conflicts_retirement_and_the_pinned_name() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    for number in 1..=130 {
        let name = if number <= 2 { "same".into() } else { format!("objects-{number}") };
        let entry = birth(number, name, Schema::default());
        if number == 2 {
            // A separately admitted offline birth bypasses local name rejection;
            // source and derived repair still share the publication transaction.
            tx.put(&entry_key(id(number)), &entry.canonical().unwrap()).unwrap();
            repair(&mut tx.local_indexes(), &[id(number)]).unwrap();
        } else {
            create(&mut tx, id(number), &entry).unwrap();
        }
    }
    tx.commit().unwrap();
    let pinned = IndexSnapshot::new(database.begin_read().unwrap());
    let mut tx = database.begin_write(WALL + 1).unwrap();
    retire(
        &mut tx,
        id(2),
        id(3000),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit().unwrap();
    let mut tx = database.begin_write(WALL + 2).unwrap();
    rename(
        &mut tx,
        id(3),
        "renamed",
        id(2000),
        &namespace::Origin { installation: Id::from_bytes([42; 16]), label: None },
    )
    .unwrap();
    tx.commit().unwrap();
    for limit in [1, 32, 64] {
        let mut after = None;
        let mut observed = Vec::new();
        loop {
            let page = pinned.page(after, limit).unwrap();
            assert!(page.entries.len() <= limit);
            observed.extend(page.entries);
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        assert_eq!(
            observed.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            (1..=130).map(id).collect::<Vec<_>>()
        );
        assert!(observed[0].name_conflict);
        assert!(!observed[1].name_conflict);
        assert!(!observed[1].deleted);
        assert_eq!(observed[2].name, "objects-3");
    }
    let current = IndexSnapshot::new(database.begin_read().unwrap());
    let page = current.page(None, 64).unwrap();
    assert!(!page.entries[0].name_conflict);
    assert!(page.entries[1].deleted);
    assert_eq!(page.entries[2].name, "renamed");
    assert!(current.page(None, 0).is_err());
    assert!(current.page(None, 65).is_err());
}

#[test]
fn summary_wire_bound_includes_maximum_escaped_names() {
    let database = database();
    let mut tx = database.begin_write(WALL).unwrap();
    for number in 1..=64 {
        let name = format!("{number:08}{}", "\\".repeat(MAX_ENTRY_COMPONENT_BYTES - 8));
        create(&mut tx, id(number), &birth(number, name, Schema::default())).unwrap();
    }
    tx.commit().unwrap();
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    let mut after = None;
    let mut observed = Vec::new();
    loop {
        let page = snapshot.page(after, 64).unwrap();
        // Source-byte credit can end a page before its row limit. Every page
        // still fits its wire bound and the exact cursor loses no entry.
        assert!(!page.entries.is_empty() && page.entries.len() <= 64);
        assert!(
            page.entries.iter().all(|entry| entry.name.capacity() == MAX_ENTRY_COMPONENT_BYTES)
        );
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_SERIALIZED_BYTES);
        observed.extend(page.entries.iter().map(|entry| entry.id));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(observed, (1..=64).map(id).collect::<Vec<_>>());
}

#[test]
fn summary_byte_boundary_skips_no_large_birth_and_rejects_oversized_name_values() {
    let database = database();
    let initial = schema(fields(0, 100, 255));
    let row_bytes = birth(1, "objects-1".into(), initial.clone()).canonical().unwrap().len();
    assert!(row_bytes > MAX_ENTRY_PAGE_BYTES / 64);
    let mut tx = database.begin_write(WALL).unwrap();
    for number in 1..=65 {
        create(&mut tx, id(number), &birth(number, format!("objects-{number}"), initial.clone()))
            .unwrap();
    }
    tx.commit().unwrap();
    let snapshot = IndexSnapshot::new(database.begin_read().unwrap());
    let mut after = None;
    let mut observed = Vec::new();
    loop {
        let page = snapshot.page(after, 64).unwrap();
        assert!(!page.entries.is_empty());
        assert!(page.entries.len() < 64);
        observed.extend(page.entries.into_iter().map(|entry| entry.id));
        let Some(next) = page.next else { break };
        after = Some(next);
    }
    assert_eq!(observed, (1..=65).map(id).collect::<Vec<_>>());
    let (mut after, mut cut) = (None, None);
    let mut discovered = Vec::new();
    loop {
        let DirectoryListing::Page { cut: got, entries, next, .. } =
            snapshot.directory_entries(DirectoryTarget::Root, cut, after, 64, str::cmp).unwrap()
        else {
            panic!("pinned directory page")
        };
        assert!(!entries.is_empty() && entries.len() < 64);
        discovered.extend(entries.into_iter().map(|entry| entry.id));
        cut = Some(got);
        after = next;
        if after.is_none() {
            break;
        }
    }
    discovered.sort_unstable();
    assert_eq!(discovered, (1..=65).map(id).collect::<Vec<_>>());
    let mut tx = database.begin_write(WALL + 1).unwrap();
    tx.put(&meta_key(NAME, id(1)), &vec![b'x'; MAX_ENTRY_RECORD_BYTES + 1]).unwrap();
    tx.commit().unwrap();
    let current = IndexSnapshot::new(database.begin_read().unwrap());
    assert!(matches!(current.page(None, 64), Err(Error::LimitExceeded)));
    assert!(snapshot.page(None, 64).is_ok());
}
