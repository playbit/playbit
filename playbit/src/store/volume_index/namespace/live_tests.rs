#![allow(clippy::unwrap_used, reason = "Tests construct exact canonical delivery cuts.")]
use super::super::{Birth, Origin};
use super::*;
use crate::store::{Id, object_contract::EntryAcl};
use kvdb::{CommittedDelta, CreateOptions, Db, storage::mem::MemStorage};

const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;
fn id(byte: u8) -> EntryId {
    EntryId::from_bytes([byte; 15]).unwrap()
}
fn db() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap()
}
fn origin() -> Origin {
    Origin { installation: Id::from_bytes([1; 16]), label: None }
}
fn birth(revision: u8, name: &str, observed: Vec<EntryId>) -> Vec<u8> {
    index::EntryInfo {
        namespace: Birth { observed, ..Birth::default() },
        origin: origin(),
        initial_permissions: EntryAcl::Inherit,
        created_revision: id(revision),
        name: name.into(),
        kind: index::EntryKind::Directory {},
        protocol: None,
    }
    .canonical()
    .unwrap()
}
fn delta(value: CommittedDelta) -> Vec<u8> {
    let CommittedDelta::Bytes(bytes) = value else { panic!("bounded fixture delta") };
    bytes
}
fn receive(db: &Db<MemStorage>, entry: EntryId, delta: &[u8], evidence: &[u8]) -> Result<()> {
    let mut receiver = kvdb::DeltaReceiver::new(db, kvdb::Lww, WALL + 10, MAX_DELTA_BYTES)?;
    receiver.process(delta)?;
    receiver
        .qualify_local_indexes(|local| -> Result<bool> {
            let changed = apply(local, entry, delta, evidence)?;
            index::repair(local, &[entry])?;
            source::cut::project(local)?.write_current(local)?;
            Ok(changed)
        })?
        .finish()?;
    Ok(())
}

#[test]
fn selected_live_acceptance_keeps_exact_sources_when_dependencies_are_opaque() {
    // The recipient cannot read the other entry. AP acceptance qualifies the
    // exact target birth/rename, including when a higher Pending local head wins
    // raw LWW. Evidence never confirms the omitted dependency itself.
    let ap = db();
    let client = db();
    let mut tx = ap.begin_write_with_delta(WALL, MAX_DELTA_BYTES).unwrap();
    let other = birth(11, "other", vec![]);
    tx.put(&index::entry_key(id(1)), &other).unwrap();
    let other_event =
        source::Declaration::decode(id(1), tx.clock(), &other).unwrap().witness().id().unwrap();
    let target = birth(12, "target", vec![other_event]);
    tx.put(&index::entry_key(id(2)), &target).unwrap();
    let declaration = source::Declaration::decode(id(2), tx.clock(), &target).unwrap();
    let target_event = declaration.witness().id().unwrap();
    source::cut::retain_accepted(&mut tx.local_indexes()).unwrap();
    let births = delta(tx.commit_with_delta().unwrap());
    let selected = kvdb::sync::project_delta(&births, MAX_DELTA_BYTES, |key| {
        index::source_entry_id(key).is_ok_and(|entry| entry == id(2))
    })
    .unwrap();
    let proof = evidence(&ap.begin_read().unwrap(), id(2), &selected).unwrap();
    receive(&client, id(2), &selected, &proof).unwrap();
    assert_eq!(
        index::IndexSnapshot::new(client.begin_read().unwrap()).entry(id(2)).unwrap().info.name,
        "target"
    );
    assert!(!client.begin_read().unwrap().has(&index::entry_key(id(1))).unwrap());
    let mut observed = vec![other_event, target_event];
    observed.sort_unstable();
    let mut tx = ap.begin_write_with_delta(WALL + 1, MAX_DELTA_BYTES).unwrap();
    let accepted = index::EntryRevision {
        revision: id(13),
        creation: index::creation_digest(
            id(2),
            kvdb::Clock::from_u64(declaration.witness().clock),
            &target,
        ),
        origin_clock: tx.clock().as_u64(),
        origin: origin(),
        observed: observed.clone(),
        change: index::EntryChange::Name { name: "renamed".into() },
    };
    let bytes = accepted.canonical().unwrap();
    tx.put(&index::meta_key(index::NAME, id(2)), &bytes).unwrap();
    tx.put(&index::revision_key(id(2), id(13)), &bytes).unwrap();
    source::cut::retain_accepted(&mut tx.local_indexes()).unwrap();
    let changes = delta(tx.commit_with_delta().unwrap());
    // A selected write-only delivery has the current head, without history.
    let selected =
        kvdb::sync::project_delta(&changes, MAX_DELTA_BYTES, |key| !index::is_entry_history(key))
            .unwrap();
    let proof = evidence(&ap.begin_read().unwrap(), id(2), &selected).unwrap();
    assert_eq!(proof.len(), 16);
    // Missing accounting beside retained payloads is corruption, not an empty
    // baseline. Reject the complete source/evidence transaction before reset.
    let usage_key = b"namespace-confirmed-payload-usage";
    let usage = client.begin_read().unwrap().local_get(usage_key).unwrap().unwrap().to_vec();
    let mut tx = client.begin_write(WALL + 2).unwrap();
    tx.local_indexes().del(usage_key).unwrap();
    tx.commit().unwrap();
    let prior_root = client.begin_read().unwrap().root_hash().unwrap();
    assert!(receive(&client, id(2), &selected, &proof).is_err());
    assert_eq!(client.begin_read().unwrap().root_hash().unwrap(), prior_root);
    let mut tx = client.begin_write(WALL + 2).unwrap();
    tx.local_indexes().put(usage_key, &usage).unwrap();
    tx.commit().unwrap();

    let mut tx = client.begin_write(WALL + 3).unwrap();
    observed.push(id(199));
    observed.sort_unstable();
    let pending = index::EntryRevision {
        revision: id(14),
        origin_clock: tx.clock().as_u64(),
        observed,
        change: index::EntryChange::Name { name: "pending".into() },
        ..accepted.clone()
    };
    let pending_bytes = pending.canonical().unwrap();
    tx.put(&index::meta_key(index::NAME, id(2)), &pending_bytes).unwrap();
    tx.put(&index::revision_key(id(2), id(14)), &pending_bytes).unwrap();
    tx.commit().unwrap();
    receive(&client, id(2), &selected, &proof).unwrap();
    receive(&client, id(2), &selected, &proof).unwrap();
    let snapshot = index::IndexSnapshot::new(client.begin_read().unwrap());
    assert_eq!(snapshot.entry(id(2)).unwrap().info.name, "renamed");
    drop(snapshot);
    let read = client.begin_read().unwrap();
    assert_eq!(
        read.get(&index::meta_key(index::NAME, id(2))).unwrap().unwrap().as_ref(),
        pending_bytes
    );
    let accepted_witness = declaration
        .revision(kvdb::Clock::from_u64(accepted.origin_clock), &bytes)
        .unwrap()
        .unwrap()
        .witness;
    assert_eq!(
        observations::source_status(&read, &accepted_witness, 0).unwrap(),
        Some(observations::Status::Confirmed)
    );
    assert!(
        read.local_get(&[observations::PREFIX, other_event.as_bytes()].concat()).unwrap().is_none()
    );
    let root = read.root_hash().unwrap();
    drop(read);
    let mut substituted = proof.clone();
    substituted[..15].copy_from_slice(id(199).as_bytes());
    assert!(receive(&client, id(2), &selected, &substituted).is_err());
    assert_eq!(client.begin_read().unwrap().root_hash().unwrap(), root);
    let mut contradictory = proof.clone();
    contradictory[15] = 0;
    assert!(receive(&client, id(2), &selected, &contradictory).is_err());
    assert_eq!(
        index::IndexSnapshot::new(client.begin_read().unwrap()).entry(id(2)).unwrap().info.name,
        "renamed"
    );
}

#[test]
fn live_evidence_requires_ordered_unique_bounded_ids_and_valid_masks() {
    // Wire rejection happens before ledger materialization or source mutation.
    let row = [id(1).as_bytes().as_slice(), &[1]].concat();
    validate_evidence(&row).unwrap();
    validate_evidence(&[]).unwrap();
    assert!(validate_evidence(&row[..15]).is_err());
    assert!(validate_evidence(&[row.as_slice(), row.as_slice()].concat()).is_err());
    let mut invalid = row;
    invalid[15] = 4;
    assert!(validate_evidence(&invalid).is_err());
    assert!(validate_evidence(&vec![0; MAX_EVIDENCE_BYTES + 16]).is_err());
}

#[test]
fn dependency_arrival_reports_acceptance_of_unchanged_selected_sources_once() {
    // A selected birth can become accepted when only another entry changes.
    // Fanout needs this transition even though its raw delta omits that birth.
    let ap = db();
    let mut tx = ap.begin_write(WALL).unwrap();
    let clock = tx.clock();
    let dependency = birth(11, "other", vec![]);
    let dependency_event =
        source::Declaration::decode(id(1), clock, &dependency).unwrap().witness().id().unwrap();
    let selected = birth(12, "selected", vec![dependency_event]);
    tx.put(&index::entry_key(id(2)), &selected).unwrap();
    let selected_event =
        source::Declaration::decode(id(2), clock, &selected).unwrap().witness().id().unwrap();
    assert!(source::cut::retain_accepted_changes(&mut tx.local_indexes()).unwrap().is_empty());
    tx.commit().unwrap();
    let mut tx = ap.begin_write_with_delta(WALL + 1, MAX_DELTA_BYTES).unwrap();
    tx.put_with_clock(&index::entry_key(id(1)), &dependency, clock).unwrap();
    let changes = source::cut::retain_accepted_changes(&mut tx.local_indexes()).unwrap();
    assert!(changes.contains(&source::cut::AcceptedSource { entry: id(2), event: selected_event }));
    assert_eq!(changes.len(), 2);
    let bytes = delta(tx.commit_with_delta().unwrap());
    kvdb::sync::visit_delta(&bytes, MAX_DELTA_BYTES, |key, _| {
        assert_ne!(key, index::entry_key(id(2)));
    })
    .unwrap();
    let mut tx = ap.begin_write(WALL + 2).unwrap();
    assert!(source::cut::retain_accepted_changes(&mut tx.local_indexes()).unwrap().is_empty());
    tx.commit().unwrap();
}
