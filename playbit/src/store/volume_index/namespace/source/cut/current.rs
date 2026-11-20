//! Original-entry current controls. These are qualified source references, not
//! canonical directory aliases, generated names, or cycle-repair assignments.
use super::*;

const PREFIX: &[u8] = b"namespace-current/";
const MAGIC: &[u8; 4] = b"NC01";
const SOURCE_BYTES: usize = 15 + 15 + 8;
const MAX_BYTES: usize = 5 + 5 * SOURCE_BYTES;

/// Event IDs share the projection's source table. Reserve 256 bytes per birth
/// for this map, including minimum BTree node occupancy, in project().
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub birth: EntryId,
    pub name: Option<EntryId>,
    pub parent: Option<EntryId>,
    pub content: Option<EntryId>,
    pub deletion: Option<EntryId>,
}

pub(super) fn select(
    tx: &impl Sources,
    facts: &[Fact],
    admission: &Admission,
    sources: &BTreeMap<EntryId, SourceRef>,
    confirmed: &BTreeMap<EntryId, Assignments>,
) -> Result<BTreeMap<EntryId, Selection>> {
    let mut current = BTreeMap::new();
    // An accepted edit alone cannot expose an unqualified original birth.
    for fact in facts {
        if let Change::Birth { entry, .. } = fact.change
            && assignment(fact, admission, confirmed)?.is_some()
        {
            current.insert(
                entry,
                Selection {
                    birth: fact.id,
                    name: None,
                    parent: None,
                    content: None,
                    deletion: None,
                },
            );
        }
    }
    for fact in facts {
        let Some(assignment) = assignment(fact, admission, confirmed)? else { continue };
        let Some(entry) = current.get_mut(&fact.change.entry()) else { continue };
        let next = sources[&fact.id];
        match fact.change {
            Change::Edit { content, .. } => {
                if assignment.name && newer(next, sources[&entry.name.unwrap_or(entry.birth)]) {
                    entry.name = Some(fact.id);
                }
                if assignment.parent && newer(next, sources[&entry.parent.unwrap_or(entry.birth)]) {
                    entry.parent = Some(fact.id);
                }
                if content {
                    let prior = sources[&entry.content.unwrap_or(entry.birth)];
                    // Content retains the register's canonical-byte tie break;
                    // location rank is clock/event ID instead.
                    if next.clock > prior.clock
                        || next.clock == prior.clock && next.read(tx)?.1 > prior.read(tx)?.1
                    {
                        entry.content = Some(fact.id);
                    }
                }
            }
            Change::Delete { .. } => {
                if entry.deletion.is_none_or(|prior| newer(next, sources[&prior])) {
                    entry.deletion = Some(fact.id);
                }
            }
            Change::Birth { .. } => {}
        }
    }
    Ok(current)
}

fn assignment(
    fact: &Fact,
    admission: &Admission,
    confirmed: &BTreeMap<EntryId, Assignments>,
) -> Result<Option<Assignments>> {
    let local = admission.assignments.get(&fact.id).copied();
    let Some(confirmed) = confirmed.get(&fact.id).copied() else { return Ok(local) };
    confirmed.validate_source(&fact.change)?;
    if admission.events[&fact.id] == EventState::Rejected
        || local.is_some_and(|local| local != confirmed)
    {
        return Err(Error::corrupt("confirmed namespace assignment contradicts canonical source"));
    }
    // A selected receipt may omit dependencies. Its exact authenticated
    // assignment remains current without fabricating acceptance of those IDs.
    Ok(Some(confirmed))
}

fn newer(next: SourceRef, prior: SourceRef) -> bool {
    (next.clock, next.event) > (prior.clock, prior.event)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub birth: SourceRef,
    pub name: Option<SourceRef>,
    pub parent: Option<SourceRef>,
    pub content: Option<SourceRef>,
    pub deletion: Option<SourceRef>,
    pub name_conflict: bool,
}
impl Record {
    /// Exact original-entry controls used by selected recipient snapshots.
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn fields(self) -> [(u8, Option<SourceRef>); 4] {
        [
            (index::NAME, self.name),
            (index::CONTENT, self.content),
            (index::PARENT, self.parent),
            (index::RETIRED, self.deletion),
        ]
    }
    /// One bounded point read; reading a control never recomputes a full cut.
    /// Callers validate the referenced source bytes before using their contents.
    pub(crate) fn read(tx: &impl Sources, entry: EntryId) -> Result<Option<Self>> {
        let key = [PREFIX, entry.as_bytes()].concat();
        tx.local(&key, MAX_BYTES)?.map(|bytes| Self::decode(entry, &bytes)).transpose()
    }

    fn encode(self) -> Vec<u8> {
        let fields = [self.name, self.parent, self.content, self.deletion];
        let flags = fields
            .iter()
            .enumerate()
            .fold(0, |bits, (i, field)| bits | (u8::from(field.is_some()) << i));
        let mut bytes = Vec::with_capacity(5 + (1 + flags.count_ones() as usize) * SOURCE_BYTES);
        bytes.extend_from_slice(MAGIC);
        bytes.push(flags | (u8::from(self.name_conflict) << 4));
        for source in std::iter::once(self.birth).chain(fields.into_iter().flatten()) {
            bytes.extend_from_slice(source.revision.as_bytes());
            bytes.extend_from_slice(source.event.as_bytes());
            bytes.extend_from_slice(&source.clock.to_le_bytes());
        }
        assert!(bytes.len() <= MAX_BYTES);
        bytes
    }

    fn decode(entry: EntryId, bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 5 || &bytes[..4] != MAGIC || bytes[4] & !31 != 0 {
            return Err(Error::corrupt("invalid current namespace record"));
        }
        let flags = bytes[4] & 15;
        if bytes.len() != 5 + (1 + flags.count_ones() as usize) * SOURCE_BYTES {
            return Err(Error::corrupt("invalid current namespace record length"));
        }
        let mut cursor = 5;
        let mut read = |birth: bool| -> Result<SourceRef> {
            let revision = EntryId::from_bytes(
                bytes[cursor..cursor + 15].try_into().expect("checked source length"),
            )?;
            let event = EntryId::from_bytes(
                bytes[cursor + 15..cursor + 30].try_into().expect("checked source length"),
            )?;
            let clock = u64::from_le_bytes(
                bytes[cursor + 30..cursor + SOURCE_BYTES]
                    .try_into()
                    .expect("checked source length"),
            );
            cursor += SOURCE_BYTES;
            if clock == 0 {
                return Err(Error::corrupt("current namespace source has zero clock"));
            }
            Ok(SourceRef { entry, revision, event, clock, birth })
        };
        let birth = read(true)?;
        let mut fields = [None; 4];
        for (i, field) in fields.iter_mut().enumerate() {
            if flags & (1 << i) != 0 {
                let source = read(false)?;
                if source.event == birth.event || source.clock <= birth.clock {
                    return Err(Error::corrupt("current namespace edit does not follow birth"));
                }
                *field = Some(source);
            }
        }
        let [name, parent, content, deletion] = fields;
        Ok(Self { birth, name, parent, content, deletion, name_conflict: bytes[4] & 16 != 0 })
    }
}

impl Projection {
    /// Called only at an authorized publication boundary. Generic index repair
    /// cannot qualify source records. All writes share the caller's transaction.
    pub(crate) fn write_current(
        &self,
        local: &mut LocalIndexes<'_, '_, impl Storage>,
    ) -> Result<()> {
        if self.current.len() > MAX_ENTRIES {
            return Err(Error::LimitExceeded);
        }
        let mut cursor = PREFIX.to_vec();
        let mut exclusive = false;
        let mut count = 0;
        while let Some(key) = local.next_local_key(&cursor, exclusive)? {
            if !key.starts_with(PREFIX) {
                break;
            }
            count += 1;
            if count > MAX_ENTRIES {
                return Err(Error::LimitExceeded);
            }
            if key.len() != PREFIX.len() + 15 {
                return Err(Error::corrupt("invalid current namespace key"));
            }
            let entry =
                EntryId::from_bytes(key[PREFIX.len()..].try_into().expect("checked entry key"))?;
            if !self.current.contains_key(&entry) {
                local.del(&key)?;
            }
            cursor = key;
            exclusive = true;
        }
        for (&entry, selected) in &self.current {
            let record = Record {
                birth: self.sources[&selected.birth],
                name: selected.name.map(|event| self.sources[&event]),
                parent: selected.parent.map(|event| self.sources[&event]),
                content: selected.content.map(|event| self.sources[&event]),
                deletion: selected.deletion.map(|event| self.sources[&event]),
                name_conflict: self
                    .admission
                    .view
                    .canonical
                    .get(&entry)
                    .and_then(|canonical| self.admission.view.entries.get(canonical))
                    .is_some_and(|entry| entry.visible && entry.name != entry.requested_name),
            };
            if Record::read(local, entry)? != Some(record) {
                local.put(&[PREFIX, entry.as_bytes()].concat(), &record.encode())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests qualify exact selected sources and inspect durable controls."
)]
mod tests {
    use super::*;
    use crate::store::object_contract::EntryAcl;
    use crate::store::volume_index::{EntryChange, EntryInfo, EntryRevision};
    use kvdb::{CreateOptions, Db, storage::mem::MemStorage};
    const WALL: u64 = kvdb::clock::EPOCH_UNIX_SECS + 1000;
    fn id(byte: u8) -> EntryId {
        EntryId::from_bytes([byte; 15]).unwrap()
    }
    fn origin() -> Origin {
        Origin { installation: Id::from_bytes([1; 16]), label: None }
    }
    fn body(byte: u8) -> EntryKind {
        EntryKind::Blob {
            len_bytes: 8,
            digest: [byte; 32],
            encrypted_key: [byte; 32],
            published_at_micros: 0,
        }
    }
    fn confirm(
        tx: &mut WriteTxn<'_, MemStorage>,
        source: Witness,
        assignment: Assignments,
        bytes: &[u8],
    ) {
        let row = observations::Observation {
            source,
            assignment: Some(assignment),
            status: observations::Status::Confirmed,
        };
        let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
        let mut after = before.clone();
        after.apply_receipt(std::slice::from_ref(&row)).unwrap();
        after.write_changes(&before, &mut tx.local_indexes()).unwrap();
        crate::store::volume_index::namespace::payloads::retain(
            &mut tx.local_indexes(),
            &row,
            bytes,
            0,
        )
        .unwrap();
    }
    fn materialize(tx: &mut WriteTxn<'_, MemStorage>) -> Projection {
        let projection = project(tx).unwrap();
        projection.write_current(&mut tx.local_indexes()).unwrap();
        projection
    }

    #[test]
    fn current_selected_sources_require_birth_and_preserve_normalized_assignments() {
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let mut tx = db.begin_write(WALL).unwrap();
        let birth = EntryInfo {
            namespace: Birth { observed: vec![id(200)], ..Birth::default() },
            origin: origin(),
            initial_permissions: EntryAcl::Inherit,
            created_revision: id(2),
            name: "file".into(),
            kind: body(1),
            protocol: None,
        }
        .canonical()
        .unwrap();
        tx.put(&index::entry_key(id(1)), &birth).unwrap();
        let declaration = Declaration::decode(id(1), tx.clock(), &birth).unwrap();
        let witness = declaration.witness();
        let mut observed = vec![witness.id().unwrap(), id(200)];
        observed.sort_unstable();
        let edit = |revision, delta, change| EntryRevision {
            revision: id(revision),
            creation: declaration.digest,
            origin_clock: witness.clock + delta,
            origin: origin(),
            observed: observed.clone(),
            change,
        };
        let content = edit(3, 1, EntryChange::Content { kind: body(2) });
        let content_bytes = content.canonical().unwrap();
        let source = |record: &EntryRevision| {
            declaration
                .revision(kvdb::Clock::from_u64(record.origin_clock), &record.canonical().unwrap())
                .unwrap()
                .unwrap()
                .witness
        };
        confirm(&mut tx, source(&content), Assignments::default(), &content_bytes);
        assert!(materialize(&mut tx).current.is_empty());
        assert!(Record::read(&tx, id(1)).unwrap().is_none());
        assert!(matches!(index::mark_backing_ready(&mut tx, id(1)), Err(Error::NotFound(_))));
        index::mark_source_backing_ready(&mut tx, id(1)).unwrap();
        // Confirming the edit never confirmed its opaque birth dependency.
        confirm(&mut tx, witness.clone(), Assignments::default(), &birth);
        let rename = edit(4, 2, EntryChange::Name { name: "renamed".into() });
        confirm(
            &mut tx,
            source(&rename),
            Assignments { name: true, parent: false },
            &rename.canonical().unwrap(),
        );
        let moved =
            edit(5, 3, EntryChange::Move { parent: Some(id(201)), name: Some("redundant".into()) });
        confirm(
            &mut tx,
            source(&moved),
            Assignments { name: false, parent: true },
            &moved.canonical().unwrap(),
        );
        let projected = materialize(&mut tx);
        assert!(projected.admission.assignments.is_empty());
        let current = Record::read(&tx, id(1)).unwrap().unwrap();
        assert_eq!(current.name.unwrap().revision, rename.revision);
        assert_eq!(current.parent.unwrap().revision, moved.revision);
        assert_eq!(current.content.unwrap().read(&tx).unwrap().1, content_bytes);
        assert!(current.deletion.is_none());
        // New pending raw heads cannot displace retained exact sources.
        let next = edit(6, 4, EntryChange::Content { kind: body(3) });
        let deletion = edit(7, 5, EntryChange::Delete {});
        for (kind, record) in [(index::CONTENT, &next), (index::RETIRED, &deletion)] {
            for key in [
                index::meta_key(kind, id(1)).to_vec(),
                index::revision_key(id(1), record.revision).to_vec(),
            ] {
                tx.put_with_clock(
                    &key,
                    &record.canonical().unwrap(),
                    kvdb::Clock::from_u64(record.origin_clock),
                )
                .unwrap();
            }
        }
        materialize(&mut tx);
        assert_eq!(Record::read(&tx, id(1)).unwrap().unwrap(), current);
        // Verifying the newer Pending body cannot certify the current body.
        index::mark_published_blob_ready(&mut tx, id(1), &body(3)).unwrap();
        tx.commit().unwrap();
        assert!(!index::IndexSnapshot::new(db.begin_read().unwrap()).backing_ready(id(1)).unwrap());
        let mut tx = db.begin_write(WALL + 1).unwrap();
        index::mark_published_blob_ready(&mut tx, id(1), &body(2)).unwrap();
        tx.commit().unwrap();
        let volume = crate::store::Volume {
            id: Id::from_bytes([1; 16]),
            owner: Id::from_bytes([2; 16]),
            name: "Current reads".into(),
            personal: true,
            share_key_with_service: true,
        };
        crate::store::MetadataPolicy::new(
            volume.clone(),
            crate::store::object_contract::Acl::default(),
        )
        .unwrap()
        .write(&db, WALL + 1)
        .unwrap();
        let principal = crate::store::object_contract::Principal::Human(volume.owner);
        let defaults = crate::store::object_contract::Acl::default();
        let permission = crate::store::object_contract::Permission::Read;
        let snapshot = index::IndexSnapshot::new(db.begin_read().unwrap());
        snapshot.authorize(&volume, &defaults, principal, id(1), permission).unwrap();
        assert!(snapshot.backing_ready(id(1)).unwrap());
        assert!(!snapshot.source_backing_ready(id(1)).unwrap());
        let entry = snapshot.live_entry(id(1)).unwrap();
        assert_eq!(entry.info.name, "renamed");
        assert!(!entry.name_conflict);
        assert_eq!(entry.info.namespace.parent, Some(id(201)));
        assert_eq!(entry.info.kind, body(2));
        assert_eq!(entry.content_revision, content.revision);
        assert!(snapshot.source_entry(id(1)).unwrap().deleted);
        assert!(snapshot.source_header(id(1)).unwrap().deleted);
        assert!(!snapshot.header(id(1)).unwrap().deleted);
        assert!(snapshot.entry_policy(id(1)).is_ok());
        crate::store::DatabasePolicy::read(&db, crate::store::DatabaseId::Entry(id(1)))
            .unwrap()
            .authorize(principal, permission)
            .unwrap();
        assert!(matches!(
            crate::store::DatabasePolicy::read_source(&db, crate::store::DatabaseId::Entry(id(1))),
            Err(Error::NotFound(_))
        ));
        drop(snapshot);
        let mut tx = db.begin_write(WALL + 1).unwrap();
        confirm(&mut tx, source(&deletion), Assignments::default(), &deletion.canonical().unwrap());
        materialize(&mut tx);
        assert_eq!(
            Record::read(&tx, id(1)).unwrap().unwrap().deletion.unwrap().revision,
            deletion.revision
        );
        tx.commit().unwrap();
        let stored = Record::read(&db.begin_read().unwrap(), id(1)).unwrap().unwrap();
        assert_eq!(stored.name, current.name);
        assert_eq!(stored.content, current.content);
        assert!(stored.deletion.is_some());
        let snapshot = index::IndexSnapshot::new(db.begin_read().unwrap());
        assert!(matches!(
            snapshot.authorize(&volume, &defaults, principal, id(1), permission),
            Err(Error::NotFound(_))
        ));
        assert!(snapshot.backing_ready(id(1)).unwrap());
        assert!(snapshot.entry(id(1)).unwrap().deleted);
        assert!(snapshot.header(id(1)).unwrap().deleted);
        assert!(matches!(snapshot.entry_policy(id(1)), Err(Error::NotFound(_))));
        assert!(matches!(
            crate::store::DatabasePolicy::read(&db, crate::store::DatabaseId::Entry(id(1))),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(snapshot.live_entry(id(1)), Err(Error::NotFound(_))));
        drop(snapshot);
        // Corruption fails before committing either controls or new sources.
        let mut tx = db.begin_write(WALL + 1).unwrap();
        assert!(matches!(index::mark_backing_ready(&mut tx, id(1)), Err(Error::NotFound(_))));
        tx.local_indexes().put(&[PREFIX, id(1).as_bytes()].concat(), b"invalid").unwrap();
        assert!(Record::read(&tx, id(1)).is_err());
        assert!(project(&tx).unwrap().write_current(&mut tx.local_indexes()).is_err());
        drop(tx);
        assert_eq!(Record::read(&db.begin_read().unwrap(), id(1)).unwrap(), Some(stored));
    }

    #[test]
    fn current_exclusion_restores_prior_content_and_removes_unqualified_birth() {
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
        let original = Record::read(&db.begin_read().unwrap(), id(1)).unwrap().unwrap();
        let mut tx = db.begin_write(WALL + 1).unwrap();
        index::update_content(&mut tx, id(1), body(2), id(3), &origin()).unwrap();
        let updated = Record::read(&tx, id(1)).unwrap().unwrap();
        assert_eq!(updated.content.unwrap().revision, id(3));
        let (clock, bytes) = original.birth.read(&tx).unwrap();
        let declaration = Declaration::decode(id(1), clock, &bytes).unwrap();
        let (clock, bytes) = updated.content.unwrap().read(&tx).unwrap();
        let edit = declaration.revision(clock, &bytes).unwrap().unwrap().witness;
        let exclude = |tx: &mut WriteTxn<'_, MemStorage>, source| {
            let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
            let mut after = before.clone();
            after
                .apply_receipt(&[observations::Observation {
                    source,
                    status: observations::Status::Excluded,
                    assignment: None,
                }])
                .unwrap();
            after.write_changes(&before, &mut tx.local_indexes()).unwrap();
            materialize(tx);
        };
        exclude(&mut tx, edit);
        assert_eq!(Record::read(&tx, id(1)).unwrap(), Some(original));
        assert!(tx.has(&index::meta_key(index::CONTENT, id(1))).unwrap());
        tx.commit().unwrap();
        assert_eq!(Record::read(&db.begin_read().unwrap(), id(1)).unwrap(), Some(original));
        let snapshot = index::IndexSnapshot::new(db.begin_read().unwrap());
        assert_eq!(snapshot.entry(id(1)).unwrap().info.kind, body(1));
        assert_eq!(snapshot.source_entry(id(1)).unwrap().info.kind, body(2));
        drop(snapshot);
        let mut tx = db.begin_write(WALL + 2).unwrap();
        exclude(&mut tx, declaration.witness());
        assert!(Record::read(&tx, id(1)).unwrap().is_none());
        assert!(tx.has(&index::entry_key(id(1))).unwrap());
        tx.commit().unwrap();
        assert!(Record::read(&db.begin_read().unwrap(), id(1)).unwrap().is_none());
        let snapshot = index::IndexSnapshot::new(db.begin_read().unwrap());
        assert!(matches!(snapshot.entry(id(1)), Err(Error::NotFound(_))));
        assert!(matches!(snapshot.backing_ready(id(1)), Err(Error::NotFound(_))));
        assert!(snapshot.source_entry(id(1)).is_ok());
    }

    #[test]
    fn current_equal_clock_content_uses_bytes_and_confirmation_cannot_change_normalization() {
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
        let original = Record::read(&tx, id(1)).unwrap().unwrap();
        let (clock, bytes) = original.birth.read(&tx).unwrap();
        let declaration = Declaration::decode(id(1), clock, &bytes).unwrap();
        let record = |number, change| EntryRevision {
            revision: id(number),
            creation: declaration.digest,
            origin_clock: clock.as_u64() + 1,
            origin: origin(),
            observed: vec![original.birth.event],
            change,
        };
        // Pick two deterministic sources whose byte and event-ID ranks differ.
        let mut sources = Vec::new();
        for number in 3..19 {
            let edit = record(number, EntryChange::Content { kind: body(number) });
            let bytes = edit.canonical().unwrap();
            let decoded = declaration
                .revision(kvdb::Clock::from_u64(edit.origin_clock), &bytes)
                .unwrap()
                .unwrap();
            sources.push((bytes, decoded.witness, edit.revision));
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        let pair = sources
            .windows(2)
            .find(|pair| pair[0].1.id().unwrap() > pair[1].1.id().unwrap())
            .unwrap();
        for (bytes, source, revision) in pair {
            tx.put_with_clock(
                &index::revision_key(id(1), *revision),
                bytes,
                kvdb::Clock::from_u64(source.clock),
            )
            .unwrap();
        }
        materialize(&mut tx);
        assert_eq!(Record::read(&tx, id(1)).unwrap().unwrap().content.unwrap().revision, pair[1].2);
        let moved = record(50, EntryChange::Move { parent: None, name: Some("renamed".into()) });
        let bytes = moved.canonical().unwrap();
        tx.put_with_clock(
            &index::revision_key(id(1), moved.revision),
            &bytes,
            kvdb::Clock::from_u64(moved.origin_clock),
        )
        .unwrap();
        let source = declaration
            .revision(kvdb::Clock::from_u64(moved.origin_clock), &bytes)
            .unwrap()
            .unwrap()
            .witness;
        let projected = materialize(&mut tx);
        assert_eq!(
            projected.admission.assignments[&source.id().unwrap()],
            Assignments { name: true, parent: false }
        );
        // A complete local context proves this field is meaningful. A receipt
        // cannot claim otherwise for the same exact event.
        confirm(&mut tx, source, Assignments::default(), &bytes);
        assert!(matches!(project(&tx), Err(Error::Corrupt(_))));
        drop(tx);
        assert!(Record::read(&db.begin_read().unwrap(), id(1)).unwrap().is_none());
    }

    #[test]
    fn pending_raw_name_repair_does_not_mark_a_qualified_entry_conflicted() {
        // Repair can move the raw candidate before its causal dependencies arrive.
        // The qualified read must retain both its prior name and conflict status.
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
        let (mut event, _) = index::change_event(
            &tx,
            id(1),
            id(3),
            EntryChange::Name { name: "ghost".into() },
            &origin(),
        )
        .unwrap();
        event.observed.push(id(200));
        event.observed.sort_unstable();
        let bytes = event.canonical().unwrap();
        tx.put(&index::meta_key(index::NAME, id(1)), &bytes).unwrap();
        tx.put(&index::revision_key(id(1), id(3)), &bytes).unwrap();
        index::repair(&mut tx.local_indexes(), &[id(1)]).unwrap();
        materialize(&mut tx);
        tx.commit().unwrap();
        let snapshot = index::IndexSnapshot::new(db.begin_read().unwrap());
        let current = snapshot.entry(id(1)).unwrap();
        assert_eq!(current.info.name, "file");
        assert!(!current.name_conflict);
        assert_eq!(snapshot.source_entry(id(1)).unwrap().info.name, "ghost");
        assert_eq!(snapshot.lookup("file").unwrap().id, id(1));
        assert!(snapshot.lookup("ghost").is_err());
    }

    #[test]
    fn current_encoding_rejects_invalid_flags_clocks_and_lengths() {
        let birth =
            SourceRef { entry: id(1), revision: id(2), event: id(3), clock: 5, birth: true };
        let edit = SourceRef { revision: id(4), event: id(5), clock: 6, birth: false, ..birth };
        let record = Record {
            birth,
            name: Some(edit),
            parent: Some(edit),
            content: Some(edit),
            deletion: Some(edit),
            name_conflict: true,
        };
        let bytes = record.encode();
        assert_eq!(bytes.len(), MAX_BYTES);
        assert_eq!(Record::decode(id(1), &bytes).unwrap(), record);
        for length in 0..bytes.len() {
            assert!(Record::decode(id(1), &bytes[..length]).is_err());
        }
        let mut bad = bytes.clone();
        bad[4] |= 32;
        assert!(Record::decode(id(1), &bad).is_err());
        let mut bad = bytes.clone();
        bad[35..43].fill(0);
        assert!(Record::decode(id(1), &bad).is_err());
        let mut bad = bytes;
        bad[73..81].copy_from_slice(&5u64.to_le_bytes());
        assert!(Record::decode(id(1), &bad).is_err());
    }
}
