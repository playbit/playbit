#![allow(clippy::unwrap_used, reason = "Tests assert valid storage operations.")]
use super::*;
use crate::store::object_contract::{FieldDeclaration, TypeDeclaration, ValueType};
use kvdb::{CommittedDelta, CreateOptions, Db, DeltaReceiver, storage::mem::MemStorage};
use std::cell::RefCell;

const TIME: u64 = kvdb::clock::EPOCH_UNIX_SECS + 9000;

#[test]
fn optional_creation_bytes_remain_the_anchor_after_known_field_edits() {
    // Add a future optional root field before publishing the immutable birth;
    // subsequent overlays must bind the admitted bytes, not their known projection.
    let db = database(1);
    let mut tx = db.begin_write(TIME).unwrap();
    create(&mut tx, &schema(), id(10), id(1), id(9), &fields("Before")).unwrap();
    let clock = tx.clock();
    let key = creation_key(id(10));
    let mut original = tx.get(&key).unwrap().unwrap().to_vec();
    original.extend_from_slice(&99u16.to_le_bytes());
    original.extend_from_slice(&3u32.to_le_bytes());
    original.extend_from_slice(&[255, 4, 5]);
    tx.put(&key, &original).unwrap();
    validate_pending(&tx, &BTreeSet::from([id(10)])).unwrap();
    tx.commit().unwrap();
    let record = Snapshot::new(db.begin_read().unwrap()).edit_record(id(10)).unwrap();
    let mut tx = db.begin_write(TIME + 1).unwrap();
    edit(&mut tx, &schema(), record, id(9), &fields("After")).unwrap();
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(&*read.get(&key).unwrap().unwrap(), original);
    let (field_clock, raw) =
        read.get_with_clock(field_key(id(10), id(2)).as_bytes()).unwrap().unwrap();
    assert_eq!(
        parse_field(&raw, field_clock, &mut Limits::ABI.start(1, 1).unwrap()).unwrap().creation,
        anchor(&original, clock)
    );
    assert_eq!(
        Snapshot::new(db.begin_read().unwrap()).read(id(10), None).unwrap().fields[&id(2)].values,
        Some(vec![Value::Text("After".into())])
    );
}
fn id(byte: u8) -> Id {
    Id::from_bytes([byte; 16])
}
fn database(replica: u64) -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], replica)).unwrap()
}
fn schema() -> Schema {
    Schema {
        types: BTreeMap::from([(
            id(1),
            TypeDeclaration {
                name: "Note".into(),
                deprecated: false,
                fields: [2, 3]
                    .map(|byte| {
                        (
                            id(byte),
                            FieldDeclaration {
                                name: format!("field{byte}"),
                                value_type: ValueType::Text,
                                repeated: false,
                                deprecated: false,
                            },
                        )
                    })
                    .into(),
            },
        )]),
    }
}
fn fields(text: &str) -> BTreeMap<Id, Option<Vec<Value>>> {
    BTreeMap::from([(id(2), Some(vec![Value::Text(text.into())]))])
}
fn bytes(delta: CommittedDelta) -> Vec<u8> {
    match delta {
        CommittedDelta::Bytes(bytes) => bytes,
        _ => panic!("small captured fixture"),
    }
}
fn birth(db: &Db<MemStorage>, object: Id, text: &str) -> Vec<u8> {
    let mut tx = db.begin_write_with_delta(TIME, 65536).unwrap();
    create(&mut tx, &schema(), object, id(1), id(9), &fields(text)).unwrap();
    bytes(tx.commit_with_delta().unwrap())
}
fn update(
    db: &Db<MemStorage>,
    object: Id,
    time: u64,
    writer: Id,
    changes: &BTreeMap<Id, Option<Vec<Value>>>,
) -> Vec<u8> {
    let record = Snapshot::new(db.begin_read().unwrap()).read(object, None).unwrap();
    let mut tx = db.begin_write_with_delta(time, 65536).unwrap();
    edit(&mut tx, &schema(), record, writer, changes).unwrap();
    bytes(tx.commit_with_delta().unwrap())
}
fn receive(db: &Db<MemStorage>, data: &[u8], writer: Option<Id>, append: bool) -> Result<()> {
    use kvdb::{Resolver, sync::Resolution};
    let touched = RefCell::new(BTreeSet::new());
    let rejected = RefCell::new(None);
    let resolver = |delta: &kvdb::sync::Delta<'_>| match validate_source(delta, writer, append) {
        Ok(object) => {
            touched.borrow_mut().insert(object);
            kvdb::Lww.resolve(delta)
        }
        Err(error) => {
            *rejected.borrow_mut() = Some(error);
            Resolution::KeepLocal
        }
    };
    let mut receiver = DeltaReceiver::new(db, resolver, TIME + 100, 65536)?;
    receiver.process(data)?;
    if let Some(error) = rejected.borrow_mut().take() {
        return Err(error);
    }
    validate_pending(receiver.pending_transaction()?, &touched.borrow())?;
    receiver.finish()?;
    Ok(())
}

#[test]
fn pages_keep_binary_id_order_across_base58_digit_boundaries() {
    let db = database(1);
    let mut first = [0; 16];
    first[15] = 57;
    let first = Id::from_bytes(first);
    let mut second = [0; 16];
    second[15] = 58;
    let second = Id::from_bytes(second);
    assert!(first < second && first.to_string() > second.to_string());
    birth(&db, second, "second");
    birth(&db, first, "first");
    let snapshot = Snapshot::new(db.begin_read().unwrap());
    let page = snapshot.page(id(1), None, 1, None).unwrap();
    assert_eq!(page.objects[0].object_id, first);
    assert_eq!(page.next, Some(first));
    let page = snapshot.page(id(1), page.next, 1, None).unwrap();
    assert_eq!(page.objects[0].object_id, second);
    assert!(page.next.is_none());
}

#[test]
fn projected_edits_validate_the_complete_pending_record_before_commit() {
    let db = database(1);
    let mut tx = db.begin_write(TIME).unwrap();
    create(&mut tx, &schema(), id(4), id(1), id(9), &fields(&"a".repeat(600_000))).unwrap();
    tx.commit().unwrap();
    let before = db.begin_read().unwrap().root_hash().unwrap();
    let record = Snapshot::new(db.begin_read().unwrap()).edit_record(id(4)).unwrap();
    assert!(record.fields.is_empty());
    let mut tx = db.begin_write(TIME + 1).unwrap();
    let large = BTreeMap::from([(id(3), Some(vec![Value::Text("b".repeat(600_000))]))]);
    assert!(matches!(edit(&mut tx, &schema(), record, id(9), &large), Err(Error::LimitExceeded)));
    drop(tx);
    assert_eq!(db.begin_read().unwrap().root_hash().unwrap(), before);
    assert!(Snapshot::new(db.begin_read().unwrap()).read(id(4), None).is_ok());

    // Replacing the old large cell in the same transaction frees its budget.
    let record = Snapshot::new(db.begin_read().unwrap()).edit_record(id(4)).unwrap();
    let mut changes = large;
    changes.insert(id(2), Some(vec![Value::Text("small".into())]));
    let mut tx = db.begin_write(TIME + 2).unwrap();
    edit(&mut tx, &schema(), record, id(9), &changes).unwrap();
    tx.commit().unwrap();
    assert!(Snapshot::new(db.begin_read().unwrap()).read(id(4), None).is_ok());
    validate_pending(&db.begin_write(TIME + 3).unwrap(), &BTreeSet::from([id(4)])).unwrap();
}

#[test]
#[cfg(feature = "local-store")]
fn local_edit_rejects_aggregate_overflow_without_publishing_or_breaking_reads() {
    use crate::store::{
        Access, LocalStore,
        volume_index::{EntryAddress, EntryKind},
    };
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let volume = store.create_volume("Bounded objects", false).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "notes", EntryKind::ObjectDb { schema: schema() })
        .unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    let record = store
        .create_object_with_fields(&owner, address, id(1), &fields(&"a".repeat(600_000)))
        .unwrap();
    let mut feed = store.take_change_receiver().unwrap();
    while !feed.drain().records.is_empty() {}
    let changes = BTreeMap::from([(id(3), Some(vec![Value::Text("b".repeat(600_000))]))]);
    assert!(matches!(
        store.edit_fields(&owner, address, record.object_id, &changes),
        Err(Error::LimitExceeded)
    ));
    assert!(feed.drain().records.is_empty());
    assert_eq!(store.read_object(&owner, address, record.object_id).unwrap(), record);
}

#[test]
fn reordered_and_duplicate_birth_and_fields_preserve_complete_initial_records() {
    let left = database(1);
    let right = database(2);
    let initial = birth(&left, id(4), "initial");
    let changed = update(
        &left,
        id(4),
        TIME + 1,
        id(9),
        &BTreeMap::from([(id(3), Some(vec![Value::Text("body".into())]))]),
    );
    receive(&right, &changed, None, false).unwrap();
    assert!(matches!(
        Snapshot::new(right.begin_read().unwrap()).read(id(4), None),
        Err(Error::NotFound(_))
    ));
    for delta in [&initial, &changed, &initial] {
        receive(&right, delta, None, false).unwrap();
    }
    let record = Snapshot::new(right.begin_read().unwrap()).read(id(4), None).unwrap();
    assert_eq!(record.fields[&id(2)].values, fields("initial")[&id(2)]);
    assert_eq!(record.fields[&id(3)].values, Some(vec![Value::Text("body".into())]));
    assert_eq!(
        left.begin_read().unwrap().root_hash().unwrap(),
        right.begin_read().unwrap().root_hash().unwrap()
    );
}

#[test]
fn concurrent_fields_converge_and_retirement_wins_delayed_edits() {
    let left = database(1);
    let right = database(2);
    let initial = birth(&left, id(4), "initial");
    receive(&right, &initial, None, false).unwrap();
    let first = update(&left, id(4), TIME + 1, id(8), &fields("first"));
    let second = update(&right, id(4), TIME + 1, id(9), &fields("second"));
    receive(&left, &second, None, false).unwrap();
    receive(&right, &first, None, false).unwrap();
    assert_eq!(
        Snapshot::new(left.begin_read().unwrap()).read(id(4), None).unwrap(),
        Snapshot::new(right.begin_read().unwrap()).read(id(4), None).unwrap()
    );
    let mut tx = left.begin_write_with_delta(TIME + 2, 65536).unwrap();
    retire(&mut tx, id(4)).unwrap();
    let deletion = bytes(tx.commit_with_delta().unwrap());
    receive(&right, &deletion, None, false).unwrap();
    receive(&right, &first, None, false).unwrap();
    assert!(matches!(
        Snapshot::new(right.begin_read().unwrap()).read(id(4), None),
        Err(Error::NotFound(_))
    ));
    assert!(right.begin_read().unwrap().get(creation_key(id(4)).as_slice()).unwrap().is_some());
}

#[test]
fn immutable_birth_and_creation_bound_fields_abort_invalid_replication() {
    let left = database(1);
    let right = database(2);
    let other = database(3);
    let initial = birth(&left, id(4), "initial");
    receive(&right, &initial, None, false).unwrap();
    let counterfeit = birth(&other, id(4), "other");
    let before = right.begin_read().unwrap().root_hash().unwrap();
    assert!(receive(&right, &counterfeit, None, false).is_err());
    assert_eq!(right.begin_read().unwrap().root_hash().unwrap(), before);
    let edit = update(&other, id(4), TIME + 1, id(9), &fields("delayed"));
    assert!(receive(&right, &edit, None, false).is_err());
    assert_eq!(right.begin_read().unwrap().root_hash().unwrap(), before);
    let pending = database(4);
    receive(&pending, &edit, None, false).unwrap();
    assert!(receive(&pending, &initial, None, false).is_err());
    assert!(pending.begin_read().unwrap().get(creation_key(id(4)).as_slice()).unwrap().is_none());
}

#[test]
fn append_admits_only_new_birth_from_authenticated_creator() {
    let left = database(1);
    let right = database(2);
    let initial = birth(&left, id(4), "initial");
    assert!(receive(&right, &initial, Some(id(8)), true).is_err());
    receive(&right, &initial, Some(id(9)), true).unwrap();
    let before = right.begin_read().unwrap().root_hash().unwrap();
    receive(&right, &initial, Some(id(9)), true).unwrap();
    assert_eq!(right.begin_read().unwrap().root_hash().unwrap(), before);
    let change = update(&left, id(4), TIME + 1, id(9), &fields("changed"));
    assert!(receive(&right, &change, Some(id(9)), true).is_err());
    assert!(receive(&right, &change, Some(id(8)), false).is_err());
}

#[test]
fn projected_pages_bound_sparse_scans_and_do_not_decode_unrequested_cells() {
    let db = database(1);
    for byte in 10..80 {
        birth(&db, id(byte), "initial");
    }
    let snapshot = Snapshot::new(db.begin_read().unwrap());
    let page = snapshot.page(id(1), None, 64, Some(&[])).unwrap();
    assert_eq!(page.objects.len(), 64);
    assert!(page.objects.iter().all(|record| record.fields.is_empty()));
    assert_eq!(page.next, Some(id(73)));
    let rest = snapshot.page(id(1), page.next, 64, Some(&[id(2)])).unwrap();
    assert_eq!(rest.objects.len(), 6);
    assert_eq!(rest.next, None);
    let sparse = snapshot.page(id(99), None, 64, Some(&[])).unwrap();
    assert!(sparse.objects.is_empty());
    assert_eq!(sparse.next, Some(id(73)));
}

#[test]
fn oversized_initial_records_fail_before_publication() {
    let db = database(1);
    let mut tx = db.begin_write(TIME).unwrap();
    assert!(matches!(
        create(&mut tx, &schema(), id(4), id(1), id(9), &fields(&"x".repeat(1_048_576))),
        Err(Error::LimitExceeded)
    ));
    assert!(tx.get(creation_key(id(4)).as_slice()).unwrap().is_none());
    let mut oversized = fields("initial");
    oversized.insert(id(3), Some(vec![Value::Text(String::new()); 4097]));
    assert!(matches!(
        create(&mut tx, &schema(), id(4), id(1), id(9), &oversized),
        Err(Error::LimitExceeded)
    ));
}

#[test]
fn retirement_received_before_birth_prevents_local_identity_reuse() {
    let source = database(1);
    let initial = birth(&source, id(4), "initial");
    let mut tx = source.begin_write_with_delta(TIME + 1, 65536).unwrap();
    retire(&mut tx, id(4)).unwrap();
    let deletion = bytes(tx.commit_with_delta().unwrap());
    let destination = database(2);
    receive(&destination, &deletion, None, false).unwrap();
    let mut tx = destination.begin_write(TIME + 2).unwrap();
    assert!(matches!(
        create(&mut tx, &schema(), id(4), id(1), id(9), &fields("reuse")),
        Err(Error::AlreadyExists(_))
    ));
    assert!(tx.get(creation_key(id(4)).as_slice()).unwrap().is_none());
    drop(tx);
    receive(&destination, &initial, None, false).unwrap();
    assert!(matches!(
        Snapshot::new(destination.begin_read().unwrap()).read(id(4), None),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn creation_bound_fields_match_regression_cases_under_reordered_delivery() {
    let mut cases = 0;
    for row in include_str!("../../../tests/fixtures/object-database.txt").lines() {
        let columns: Vec<_> = row.split_whitespace().collect();
        assert_eq!(columns.len(), 5);
        let declared = columns[0] == "true";
        let retired = columns[1] == "true";
        let ranks: [u64; 2] = [columns[2].parse().unwrap(), columns[3].parse().unwrap()];
        let sources = [database(1), database(2)];
        let initial = birth(&sources[0], id(4), "rank1");
        birth(&sources[1], id(4), "rank1");
        let mut deltas = Vec::new();
        for (source, rank) in sources.iter().zip(ranks) {
            if rank != 0 {
                // Rank 1 is the fixed initial field; zero means no update.
                deltas.push(update(
                    source,
                    id(4),
                    TIME + rank - 1,
                    id(9),
                    &fields(&format!("rank{rank}")),
                ));
            }
        }
        let mut tx = sources[0].begin_write_with_delta(TIME + 10, 65536).unwrap();
        retire(&mut tx, id(4)).unwrap();
        let deletion = bytes(tx.commit_with_delta().unwrap());
        let destination = database(3);
        for delta in deltas.iter().rev() {
            receive(&destination, delta, None, false).unwrap();
            receive(&destination, delta, None, false).unwrap();
        }
        if retired {
            receive(&destination, &deletion, None, false).unwrap();
        }
        if declared {
            receive(&destination, &initial, None, false).unwrap();
        }
        let read = Snapshot::new(destination.begin_read().unwrap()).read(id(4), None);
        if columns[4] == "-" {
            assert!(matches!(read, Err(Error::NotFound(_))));
        } else {
            let expected = columns[4].parse::<u64>().unwrap();
            let cell = &read.as_ref().unwrap().fields[&id(2)];
            assert_eq!(cell.values, Some(vec![Value::Text(format!("rank{expected}"))]));
            assert_eq!(
                cell.stamp.clock,
                kvdb::Clock { time: (9000 + expected - 1) as u32, count: 0 }.as_u64()
            );
        }
        cases += 1;
    }
    assert_eq!(cases, 64);
}
