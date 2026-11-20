//! Retained exact sources join the canonical cut only for their original birth.
//! Delivery scans remain physical: local retention is not evidence of delivery.
use super::*;
use crate::store::volume_index::namespace::payloads;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceRef {
    pub revision: EntryId,
    pub(super) entry: EntryId,
    pub(super) event: EntryId,
    pub(super) clock: u64,
    pub(super) birth: bool,
}
impl SourceRef {
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn event_id(self) -> EntryId {
        self.event
    }
    pub(super) fn new(source: &Witness) -> Result<Self> {
        Ok(Self {
            entry: source.entry,
            revision: source.revision,
            event: source.id()?,
            clock: source.clock,
            birth: source.birth,
        })
    }
    pub(crate) fn read(&self, tx: &impl Sources) -> Result<(kvdb::Clock, Vec<u8>)> {
        let entry = self.entry;
        let limit = index::MAX_ENTRY_RECORD_BYTES;
        let key = [payloads::PREFIX, entry.as_bytes(), self.event.as_bytes()].concat();
        let source = if self.birth {
            tx.source(&index::entry_key(entry), limit)?
        } else if let Some(bytes) = tx.local(&key, limit)? {
            Some((kvdb::Clock::from_u64(self.clock), bytes))
        } else {
            tx.source(&index::revision_key(entry, self.revision), limit)?
        };
        let (clock, bytes) = source.ok_or(Error::corrupt("accepted namespace source missing"))?;
        let digest = if self.birth {
            index::creation_digest(entry, clock, &bytes)
        } else {
            *blake3::hash(&bytes).as_bytes()
        };
        if clock.as_u64() != self.clock || event_id(entry, self.revision, digest)? != self.event {
            return Err(Error::corrupt("accepted namespace source changed"));
        }
        Ok((clock, bytes))
    }
}

/// AP has no recipient observation ledger: its authenticated pending cut is
/// the authority. Hold fixed references and assignments, then stream one source
/// at a time. Never materialize the complete retained payload collection.
pub(super) fn retain(
    local: &mut LocalIndexes<'_, '_, impl Storage>,
    projection: Projection,
) -> Result<Vec<AcceptedSource>> {
    let Projection { admission, sources, current, reusable } = projection;
    drop(reusable);
    let Admission { assignments, view, events } = admission;
    drop((view, events, current));
    // Two BTree maps and one preallocated acceptance-transition vector,
    // including minimum node occupancy and the fixed SourceRef.
    let retained =
        sources.len().checked_mul(320 + size_of::<AcceptedSource>()).ok_or(Error::LimitExceeded)?;
    if retained > MATERIALIZED_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut changed = Vec::with_capacity(assignments.len());
    for (event, assignment) in assignments {
        let source =
            sources.get(&event).ok_or(Error::corrupt("accepted source reference missing"))?;
        let (clock, bytes) = source.read(local)?;
        let charge = crate::store::catalog::index_record_charge(&bytes)?;
        if charge + bytes.capacity() + retained + 16 * 1024 > MATERIALIZED_BYTES + DECODER_BYTES {
            return Err(Error::LimitExceeded);
        }
        let witness = if source.birth {
            Declaration::decode(source.entry, clock, &bytes)?.witness()
        } else {
            let record = index::revision(&bytes)?;
            Witness {
                entry: source.entry,
                revision: source.revision,
                source_digest: *blake3::hash(&bytes).as_bytes(),
                clock: clock.as_u64(),
                birth: false,
                observed: record.observed.into(),
            }
        };
        let retained = retained + witness.observed.heap_bytes();
        let observation = observations::Observation {
            source: witness,
            status: observations::Status::Confirmed,
            assignment: Some(assignment),
        };
        if payloads::retain_new(local, &observation, &bytes, retained)? {
            changed.push(AcceptedSource { entry: observation.source.entry, event });
        }
    }
    Ok(changed)
}

pub(super) fn visit(
    tx: &impl Sources,
    declaration: &Declaration,
    scan: &mut Scan,
    collector: &mut Collector<impl FnMut(Decoded, Option<Assignments>) -> Result<usize>>,
) -> Result<()> {
    let prefix = [payloads::PREFIX, declaration.entry.as_bytes()].concat();
    let mut cursor = prefix.clone();
    let mut exclusive = false;
    while let Some(key) = tx.next_local_key(&cursor, exclusive)? {
        if !key.starts_with(&prefix) {
            break;
        }
        if key.len() != prefix.len() + 15 {
            return Err(Error::corrupt("retained source key"));
        }
        scan.records += 1;
        if scan.records > MAX_RECORDS {
            return Err(Error::LimitExceeded);
        }
        let event = EntryId::from_bytes(
            key[prefix.len()..].try_into().expect("checked retained event key"),
        )?;
        if collector.seen.contains(&event) {
            // The exact event already has independently decoded canonical bytes.
            // Count this physical key visit, without decoding the same fact twice.
            cursor = key;
            exclusive = true;
            continue;
        }
        let remaining = MAX_SOURCE_BYTES.checked_sub(scan.bytes).ok_or(Error::LimitExceeded)?;
        let bytes = tx
            .local(&key, remaining.min(index::MAX_ENTRY_RECORD_BYTES))?
            .ok_or(Error::corrupt("retained source disappeared"))?;
        scan.bytes += bytes.len();
        let charge = crate::store::catalog::index_record_charge(&bytes)?;
        scan.admit_decoder(charge + bytes.capacity() + scan.retained + 16 * 1024)?;
        if let Some(decoded) = decode(declaration, &key[prefix.len()..], &bytes)? {
            scan.materialized =
                collector.accept(tx, false, decoded, scan.retained + bytes.capacity())?;
        }
        cursor = key;
        exclusive = true;
    }
    Ok(())
}

fn decode(declaration: &Declaration, event: &[u8], bytes: &[u8]) -> Result<Option<Decoded>> {
    let record = match index::revision(bytes) {
        Ok(record) => record,
        Err(error) => {
            // Births are read only from the current immutable declaration. Old
            // retained births neither replace it nor poison its new source cut.
            if index::info(bytes).is_ok() {
                return Ok(None);
            }
            return Err(error);
        }
    };
    let expected = event_id(declaration.entry, record.revision, *blake3::hash(bytes).as_bytes())?;
    if expected.as_bytes() != event {
        return Err(Error::corrupt("retained event identity changed"));
    }
    if record.creation != declaration.digest {
        return Ok(None);
    }
    let clock = kvdb::Clock::from_u64(record.origin_clock);
    drop(record);
    declaration.revision(clock, bytes)
}

fn decoded_bytes(decoded: &Decoded) -> usize {
    let mut bytes = decoded.fact.observed.heap_bytes()
        + decoded.witness.observed.heap_bytes()
        + decoded.fact.origin.label.as_ref().map_or(0, String::capacity);
    match &decoded.fact.change {
        Change::Birth { name, creation_context, deletions, departures, .. } => {
            bytes += name.capacity()
                + size_of::<EntryId>() * (deletions.capacity() + departures.capacity());
            bytes += creation_context.as_ref().map_or(0, |context| context.observed.heap_bytes());
        }
        Change::Edit { name: Some(name), .. } => bytes += name.capacity(),
        _ => {}
    }
    bytes
}

/// Semantic admission never revives an explicitly excluded source. Confirmation
/// can reverse Excluded only through the separate authenticated receipt boundary.
pub(super) enum Qualification {
    Unconfirmed,
    Confirmed(Assignments),
    Excluded,
}

pub(super) fn qualification(
    tx: &impl Sources,
    decoded: &Decoded,
    materialized: usize,
    decoder: &mut usize,
    source_retained: usize,
) -> Result<Qualification> {
    let key = [observations::PREFIX, decoded.fact.id.as_bytes()].concat();
    let Some(bytes) = tx.local(&key, index::MAX_ENTRY_RECORD_BYTES)? else {
        return Ok(Qualification::Unconfirmed);
    };
    let charge = crate::store::catalog::json_projection_charge(&bytes)?;
    *decoder = (*decoder)
        .max(charge + bytes.capacity() + decoded_bytes(decoded) + 16 * 1024 + source_retained);
    if charge
        .checked_add(bytes.capacity() + decoded_bytes(decoded) + 16 * 1024)
        .and_then(|bytes| bytes.checked_add(materialized))
        .is_none_or(|bytes| bytes > MATERIALIZED_BYTES + DECODER_BYTES)
    {
        return Err(Error::LimitExceeded);
    }
    let observation = observations::decode_observation(&bytes, &decoded.witness)?;
    Ok(match observation.status {
        observations::Status::Excluded | observations::Status::Rejected => Qualification::Excluded,
        observations::Status::Confirmed => Qualification::Confirmed(
            observation.assignment.expect("validated confirmed observation has assignments"),
        ),
        observations::Status::Provisional | observations::Status::Authored => {
            Qualification::Unconfirmed
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests exercise exact source and receipt transitions.")]
mod tests {
    use super::*;
    use crate::store::{
        object_contract::EntryAcl,
        volume_index::{EntryChange, EntryInfo, EntryRevision, IndexSnapshot},
    };
    use kvdb::{CreateOptions, Db, storage::mem::MemStorage};
    const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;
    fn id(byte: u8) -> EntryId {
        EntryId::from_bytes([byte; 15]).unwrap()
    }
    fn body(byte: u8) -> EntryKind {
        EntryKind::Blob {
            len_bytes: 8,
            digest: [byte; 32],
            encrypted_key: [byte + 1; 32],
            published_at_micros: 0,
        }
    }
    fn origin() -> Origin {
        Origin { installation: Id::from_bytes([1; 16]), label: None }
    }
    fn witness(db: &Db<MemStorage>, record: &EntryRevision) -> Witness {
        let read = db.begin_read().unwrap();
        let (clock, bytes) = read.get_with_clock(&index::entry_key(id(1))).unwrap().unwrap();
        Declaration::decode(id(1), clock, &bytes)
            .unwrap()
            .revision(kvdb::Clock::from_u64(record.origin_clock), &record.canonical().unwrap())
            .unwrap()
            .unwrap()
            .witness
    }
    fn qualify(db: &Db<MemStorage>, record: &EntryRevision, status: observations::Status) {
        let source = witness(db, record);
        let row = observations::Observation {
            source,
            status,
            assignment: (status == observations::Status::Confirmed)
                .then_some(Assignments::default()),
        };
        let mut tx = db.begin_write(WALL + 20).unwrap();
        let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
        let mut after = before.clone();
        after.apply_receipt(std::slice::from_ref(&row)).unwrap();
        after.write_changes(&before, &mut tx.local_indexes()).unwrap();
        if status == observations::Status::Confirmed {
            payloads::retain(&mut tx.local_indexes(), &row, &record.canonical().unwrap(), 0)
                .unwrap();
        }
        project(&tx).unwrap().write_current(&mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
    }
    fn replace(db: &Db<MemStorage>, record: &EntryRevision) {
        let mut tx = db.begin_write(WALL + 30).unwrap();
        let bytes = record.canonical().unwrap();
        let clock = kvdb::Clock::from_u64(record.origin_clock);
        for key in [
            index::revision_key(id(1), record.revision).to_vec(),
            index::meta_key(index::CONTENT, id(1)).to_vec(),
        ] {
            tx.put_with_clock(&key, &bytes, clock).unwrap();
        }
        // Pending transaction and committed snapshot must see the same union.
        let count = project(&tx).unwrap().admission.events.len();
        tx.commit().unwrap();
        assert_eq!(count, project(&db.begin_read().unwrap()).unwrap().admission.events.len());
    }

    #[cfg(feature = "local-store")]
    #[test]
    fn retained_cut_ap_publication_revisits_pending_and_survives_restart_atomically() {
        use kvdb::{OpenOptions, redb::RedbStorage};
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = Db::create(
            RedbStorage::open(file.path()).unwrap(),
            &CreateOptions::syncable([9; 32], 0),
        )
        .unwrap();
        let mut tx = db.begin_write(WALL).unwrap();
        let info = EntryInfo {
            namespace: Birth::default(),
            origin: origin(),
            initial_permissions: EntryAcl::Inherit,
            created_revision: id(2),
            name: "file".into(),
            kind: body(1),
            protocol: None,
        };
        let bytes = info.canonical().unwrap();
        tx.put(&index::entry_key(id(1)), &bytes).unwrap();
        let (clock, _) =
            tx.local_indexes().source_get_with_clock(&index::entry_key(id(1))).unwrap().unwrap();
        let declaration = Declaration::decode(id(1), clock, &bytes).unwrap();
        let birth = declaration.witness().id().unwrap();
        retain_accepted(&mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let first = EntryRevision {
            revision: id(3),
            creation: declaration.digest,
            origin_clock: clock.as_u64() + 1,
            origin: origin(),
            observed: vec![birth],
            change: EntryChange::Content { kind: body(2) },
        };
        let source = |record: &EntryRevision| {
            declaration
                .revision(kvdb::Clock::from_u64(record.origin_clock), &record.canonical().unwrap())
                .unwrap()
                .unwrap()
                .witness
        };
        let first_source = source(&first);
        let publish = |record: &EntryRevision| {
            let mut tx = db.begin_write(WALL + 1).unwrap();
            tx.put_with_clock(
                &index::revision_key(id(1), record.revision),
                &record.canonical().unwrap(),
                kvdb::Clock::from_u64(record.origin_clock),
            )
            .unwrap();
            retain_accepted(&mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
        };
        publish(&first);
        let mut dependency = first.clone();
        dependency.revision = id(4);
        dependency.origin_clock += 1;
        dependency.change = EntryChange::Name { name: "renamed".into() };
        dependency.observed.push(first_source.id().unwrap());
        dependency.observed.sort_unstable();
        let dependency_source = source(&dependency);
        let mut next = dependency.clone();
        next.revision = first.revision;
        next.origin_clock += 1;
        next.change = EntryChange::Content { kind: body(3) };
        next.observed.push(dependency_source.id().unwrap());
        next.observed.sort_unstable();
        let next_source = source(&next);
        let payload_key = |source: &Witness| {
            [payloads::PREFIX, source.entry.as_bytes(), source.id().unwrap().as_bytes()].concat()
        };
        publish(&next);
        let blob_version = |record: &EntryRevision| {
            let EntryChange::Content {
                kind: EntryKind::Blob { len_bytes, digest, encrypted_key, .. },
            } = record.change
            else {
                panic!("content fixture");
            };
            index::BlobVersion {
                address: index::EntryAddress { volume: Id::from_bytes([9; 16]), entry: id(1) },
                revision: record.revision,
                len_bytes,
                digest,
                encrypted_key,
            }
        };
        {
            let snapshot = IndexSnapshot::new(db.begin_read().unwrap());
            assert_eq!(
                snapshot.accepted_blob_creation(&blob_version(&first)).unwrap(),
                declaration.digest
            );
            assert!(snapshot.accepted_blob_creation(&blob_version(&next)).is_err());
        }
        {
            let read = db.begin_read().unwrap();
            assert!(read.local_get(&payload_key(&first_source)).unwrap().is_some());
            assert!(read.local_get(&payload_key(&next_source)).unwrap().is_none());
            assert_eq!(
                project(&read).unwrap().admission.events[&next_source.id().unwrap()],
                EventState::Pending
            );
        }
        // Arrival of a different source admits the existing Pending payload.
        publish(&dependency);
        {
            let read = db.begin_read().unwrap();
            assert!(read.local_get(&payload_key(&next_source)).unwrap().is_some());
            let assignment = accepted(&read).unwrap()[&dependency_source.id().unwrap()];
            assert_eq!(assignment, Assignments { name: true, parent: false });
            assert!(
                read.next_local_key(observations::PREFIX, false)
                    .unwrap()
                    .is_none_or(|key| !key.starts_with(observations::PREFIX))
            );
        }
        // A corrupt retained predecessor aborts the source update too.
        let mut tx = db.begin_write(WALL + 2).unwrap();
        tx.put(b"rollback-marker", b"uncommitted").unwrap();
        tx.local_indexes().put(&payload_key(&first_source), b"invalid").unwrap();
        assert!(retain_accepted(&mut tx.local_indexes()).is_err());
        drop(tx);
        let mut tx = db.begin_write(WALL + 3).unwrap();
        tx.put(b"rollback-marker", b"uncommitted").unwrap();
        tx.local_indexes().del(b"namespace-confirmed-payload-usage").unwrap();
        assert!(matches!(retain_accepted(&mut tx.local_indexes()), Err(Error::Corrupt(_))));
        drop(tx);
        drop(db);
        let db =
            Db::open(RedbStorage::open(file.path()).unwrap(), &OpenOptions::default()).unwrap();
        let read = db.begin_read().unwrap();
        assert!(read.get(b"rollback-marker").unwrap().is_none());
        for record in [&first, &next] {
            assert_eq!(
                read.local_get(&payload_key(&source(record))).unwrap().unwrap().as_ref(),
                record.canonical().unwrap()
            );
        }
        for record in [&first, &next] {
            assert_eq!(
                IndexSnapshot::new(db.begin_read().unwrap())
                    .accepted_blob_creation(&blob_version(record))
                    .unwrap(),
                declaration.digest
            );
        }
        assert_eq!(accepted(&read).unwrap().len(), 4);
        assert_eq!(
            payloads::revision(&read, id(1), first.revision).unwrap().unwrap().1,
            next.canonical().unwrap()
        );
    }

    #[test]
    fn retained_cut_preserves_exact_content_and_dependencies_without_forging_delivery() {
        // Synthetic storage/reconciliation robustness: authorized AP ingress
        // rejects revision-key replacement. Exact retained facts must nonetheless
        // survive a differing raw slot without inventing receipt delivery.
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let mut tx = db.begin_write(WALL).unwrap();
        index::create(
            &mut tx,
            id(1),
            &EntryInfo {
                namespace: Birth::default(),
                origin: origin(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: id(2),
                name: "file".into(),
                kind: body(1),
                protocol: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let mut tx = db.begin_write(WALL + 1).unwrap();
        index::update_content(&mut tx, id(1), body(2), id(3), &origin()).unwrap();
        tx.commit().unwrap();
        let first = IndexSnapshot::new(db.begin_read().unwrap()).revision(id(1), id(3)).unwrap();
        qualify(&db, &first, observations::Status::Confirmed);
        assert_eq!(project(&db.begin_read().unwrap()).unwrap().admission.events.len(), 2);
        let first_event = witness(&db, &first).id().unwrap();
        let mut next = first.clone();
        next.change = EntryChange::Content { kind: body(3) };
        next.origin_clock += 1;
        next.observed.push(first_event);
        next.observed.push(id(99));
        next.observed.sort_unstable();
        replace(&db, &next);
        let index = IndexSnapshot::new(db.begin_read().unwrap());
        assert_eq!(index.lookup("file").unwrap().info.kind, body(2));
        drop(index);
        next.observed.retain(|event| *event != id(99));
        replace(&db, &next);
        let read = db.begin_read().unwrap();
        let accepted = accepted(&read).unwrap();
        assert_eq!(accepted.len(), 3);
        let delivered = delivered_ids(&read, &accepted, None).unwrap();
        assert_eq!(delivered.len(), 2);
        assert!(!delivered.contains_key(&first_event));
        let selected = Db::create(MemStorage::new(), &CreateOptions::syncable([8; 32], 0)).unwrap();
        let mut tx = selected.begin_write(WALL + 30).unwrap();
        for key in
            [index::entry_key(id(1)).to_vec(), index::meta_key(index::CONTENT, id(1)).to_vec()]
        {
            let (clock, bytes) = read.get_with_clock(&key).unwrap().unwrap();
            tx.put_with_clock(&key, &bytes, clock).unwrap();
        }
        tx.commit().unwrap();
        qualify(&selected, &next, observations::Status::Confirmed);
        let next_event = witness(&selected, &next).id().unwrap();
        let projected = project(&selected.begin_read().unwrap()).unwrap();
        assert_eq!(projected.admission.events[&next_event], EventState::Pending);
        assert_eq!(projected.admission.assignments.len(), 1);
        // Membership still comes from the locally admitted namespace. Exact AP
        // confirmation independently qualifies the visible file's newer body.
        assert_eq!(
            IndexSnapshot::new(selected.begin_read().unwrap()).lookup("file").unwrap().info.kind,
            body(3)
        );
        drop(read);
        assert_eq!(
            IndexSnapshot::new(db.begin_read().unwrap()).lookup("file").unwrap().info.kind,
            body(3)
        );
        // Canonical reconciliation can exclude the raw next source. Re-running
        // semantic admission cannot itself requalify the same exact bytes.
        qualify(&db, &next, observations::Status::Excluded);
        assert_eq!(
            IndexSnapshot::new(db.begin_read().unwrap()).lookup("file").unwrap().info.kind,
            body(2)
        );
        qualify(&db, &next, observations::Status::Confirmed);
        assert_eq!(
            IndexSnapshot::new(db.begin_read().unwrap()).lookup("file").unwrap().info.kind,
            body(3)
        );
        // Replacing the original declaration leaves unrelated retained history,
        // which cannot poison or contribute to the new birth's source cut.
        let mut tx = db.begin_write(WALL + 40).unwrap();
        let mut birth = index::info(&tx.get(&index::entry_key(id(1))).unwrap().unwrap()).unwrap();
        birth.origin.label = Some("replacement birth".into());
        tx.put(&index::entry_key(id(1)), &birth.canonical().unwrap()).unwrap();
        tx.del(&index::revision_key(id(1), id(3))).unwrap();
        tx.del(&index::meta_key(index::CONTENT, id(1))).unwrap();
        assert_eq!(project(&tx).unwrap().admission.events.len(), 1);
        tx.commit().unwrap();
        assert_eq!(project(&db.begin_read().unwrap()).unwrap().admission.events.len(), 1);
    }
}
