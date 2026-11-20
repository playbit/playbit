//! Replica-local causal knowledge. Receipt qualification supplies exact source
//! witnesses; observed IDs are edges, never proof that their payloads were read.
//! These records survive catalog narrowing and never enter replicated roots.
use super::*;
use crate::store::catalog::{decode, encode};
use kvdb::{LocalIndexes, Storage};
use std::sync::Arc;

pub(crate) const PREFIX: &[u8] = b"namespace-observation/";
mod publication;
pub use publication::Publication;

fn check_materialization(events: usize, ids: usize) -> Result<()> {
    if events > MAX_FACTS || ids >= MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let charged = ids * size_of::<EntryId>() + 32 + events * (1024 + 32 + ids.div_ceil(64) * 8);
    if charged > 2 * 1024 * 1024 {
        return Err(Error::LimitExceeded);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Witness {
    pub entry: EntryId,
    pub revision: EntryId,
    pub source_digest: [u8; 32],
    pub clock: u64,
    pub birth: bool,
    /// Closed context from the canonical source, sorted and unique. Missing
    /// payloads do not erase these edges or become individually confirmed rows.
    pub observed: Context,
}
impl Witness {
    pub fn id(&self) -> Result<EntryId> {
        event_id(self.entry, self.revision, self.source_digest)
    }
    fn validate(&self) -> Result<usize> {
        if self.clock == 0
            || self.observed.len() >= MAX_FACTS
            || !self.observed.ordered()
            || self.observed.contains(&self.id()?)
        {
            return Err(Error::InvalidInput("invalid namespace observation context"));
        }
        // Packed 15-byte IDs use exactly 20 base64 characters each. The fixed
        // allowance covers keys, identity, digest, clock, status and row key.
        let charge = 512 + self.observed.len() * 20;
        if charge > super::super::MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(charge)
    }
}
/// A denied source can exclude only the exact locally provisional payload.
/// Missing rows, confirmed knowledge and independently excluded history do not
/// become new exclusions merely because a received projection omits them.
pub(crate) fn is_provisional(
    tx: &kvdb::ReadTxn<'_, impl Storage>,
    source: &Witness,
    retained_bytes: usize,
) -> Result<bool> {
    Ok(matches!(
        source_status(tx, source, retained_bytes)?,
        Some(Status::Provisional | Status::Authored)
    ))
}

/// Return the exact locally recorded qualification, never an inferred status
/// from the presence of a raw source or an opaque dependency ID.
pub fn source_status(
    tx: &kvdb::ReadTxn<'_, impl Storage>,
    source: &Witness,
    retained_bytes: usize,
) -> Result<Option<Status>> {
    let Some(bytes) = tx.local_get(&[PREFIX, source.id()?.as_bytes()].concat())? else {
        return Ok(None);
    };
    if bytes.len() > super::super::MAX_ENTRY_RECORD_BYTES
        || crate::store::catalog::json_projection_charge(&bytes)?
            .checked_add(bytes.len() + source.observed.len() * size_of::<EntryId>() + 16 * 1024)
            .and_then(|bytes| bytes.checked_add(retained_bytes))
            .is_none_or(|bytes| bytes > 512 * 1024)
    {
        return Err(Error::LimitExceeded);
    }
    decode_status(&bytes, source).map(Some)
}

/// Callers charge the bounded row and its decoder before entering this helper.
pub(crate) fn decode_status(bytes: &[u8], source: &Witness) -> Result<Status> {
    Ok(decode_observation(bytes, source)?.status)
}

pub(crate) fn decode_observation(bytes: &[u8], source: &Witness) -> Result<Observation> {
    let observation: Observation = decode(bytes)?;
    observation.validate()?;
    if observation.source != *source {
        return Err(Error::corrupt("local namespace observation differs from its source"));
    }
    Ok(observation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Observed payload, without proof that this replica authored it.
    Provisional,
    /// Source authored in this replica's checked publication transaction.
    /// Pending intent is not independent full-cut acceptance.
    Authored,
    Confirmed,
    /// An exact unconfirmed source excluded by canonical reconciliation. Current
    /// permission denial is not permanent: later exact acceptance may confirm it.
    Excluded,
    /// Intrinsically invalid under immutable causal evidence, not merely absent
    /// from an AP cut or denied by its current permission policy.
    Rejected,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub source: Witness,
    pub status: Status,
    /// Only an exact full-cut confirmation can certify normalized assignments.
    pub assignment: Option<Assignments>,
}
impl Observation {
    fn validate(&self) -> Result<usize> {
        if (self.status == Status::Confirmed) != self.assignment.is_some()
            || self.source.birth
                && self.assignment.is_some_and(|value| value != Assignments::default())
        {
            return Err(Error::InvalidInput("namespace qualification assignment mismatch"));
        }
        self.source.validate()
    }
}
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    events: BTreeMap<EntryId, Arc<Observation>>,
}
impl Ledger {
    pub fn load(local: &LocalIndexes<'_, '_, impl Storage>) -> Result<Self> {
        Self::load_from(|| local.iter_from(PREFIX), publication::Head::load(local)?)
    }
    pub fn load_read(tx: &kvdb::ReadTxn<'_, impl Storage>) -> Result<Self> {
        Self::load_from(|| tx.local_iter_from(PREFIX), publication::Head::load_read(tx)?)
    }
    fn load_from<'a, T: kvdb::storage::StorageTxn + 'a>(
        rows: impl Fn() -> std::result::Result<kvdb::db::Iter<'a, T>, kvdb::Error>,
        head: publication::Head,
    ) -> Result<Self> {
        let mut ids = BTreeSet::new();
        Self::visit(rows()?, |id, record| {
            for id in std::iter::once(&id).chain(record.source.observed.iter()) {
                ids.insert(*id);
                if ids.len() > MAX_FACTS {
                    return Err(Error::LimitExceeded);
                }
            }
            Ok(())
        })?;
        let dictionary: Arc<[EntryId]> = ids.into_iter().collect::<Vec<_>>().into();
        let mut ledger = Self::default();
        Self::visit(rows()?, |id, mut record| {
            check_materialization(ledger.events.len() + 1, dictionary.len())?;
            record.source.observed.compact(&dictionary)?;
            ledger.events.insert(id, Arc::new(record));
            Ok(())
        })?;
        ledger.validate()?;
        if head != publication::Head::from_ledger(&ledger)? {
            return Err(Error::corrupt("namespace observation head differs from its records"));
        }
        Ok(ledger)
    }
    fn visit<'a, T: kvdb::storage::StorageTxn + 'a>(
        rows: kvdb::db::Iter<'a, T>,
        mut visit: impl FnMut(EntryId, Observation) -> Result<()>,
    ) -> Result<()> {
        let mut bytes = 0usize;
        let mut count = 0usize;
        for item in rows {
            let (key, value) = item?;
            if !key.starts_with(PREFIX) {
                break;
            }
            if key.len() != PREFIX.len() + 15 || value.len() > super::super::MAX_ENTRY_RECORD_BYTES
            {
                return Err(Error::corrupt("invalid namespace observation record"));
            }
            bytes = bytes.checked_add(value.len()).ok_or(Error::LimitExceeded)?;
            count += 1;
            if bytes > MAX_SOURCE_BYTES || count > MAX_FACTS {
                return Err(Error::LimitExceeded);
            }
            if crate::store::catalog::json_projection_charge(&value)? + value.len() + 16 * 1024
                > 512 * 1024
            {
                return Err(Error::LimitExceeded);
            }
            let record: Observation = decode(&value)?;
            record.validate()?;
            let id = record.source.id()?;
            if &key[PREFIX.len()..] != id.as_bytes() {
                return Err(Error::corrupt("namespace observation source binding"));
            }
            visit(id, record)?;
        }
        Ok(())
    }
    /// Preflight all durable encoding before committing a publication intent.
    pub fn validate_persistence(&self) -> Result<()> {
        self.validate()?;
        for event in self.events.values() {
            if encode(event.as_ref())?.len() > super::super::MAX_ENTRY_RECORD_BYTES {
                return Err(Error::LimitExceeded);
            }
        }
        publication::Head::from_ledger(self)?.encoded()?;
        Ok(())
    }
    /// Validate a complete candidate before writing any local records. The
    /// caller commits these changes with its source/reconciliation transaction.
    pub fn write_changes(
        &self,
        previous: &Self,
        local: &mut LocalIndexes<'_, '_, impl Storage>,
    ) -> Result<()> {
        self.validate()?;
        if previous.events.keys().any(|id| !self.events.contains_key(id)) {
            return Err(Error::InvalidInput("namespace observations cannot be forgotten"));
        }
        for (id, prior) in &previous.events {
            let next = &self.events[id];
            if prior.source != next.source
                || prior.status == Status::Confirmed && prior.assignment != next.assignment
                || prior.status == Status::Confirmed && next.status != Status::Confirmed
                || prior.status == Status::Rejected && next.status != Status::Rejected
                || prior.status == Status::Excluded
                    && matches!(next.status, Status::Provisional | Status::Authored)
                || prior.status == Status::Authored && next.status == Status::Provisional
            {
                return Err(Error::InvalidInput("namespace observation status cannot regress"));
            }
        }
        for (id, event) in &self.events {
            if previous.events.get(id) == Some(event) {
                continue;
            }
            let value = encode(event.as_ref())?;
            if value.len() > super::super::MAX_ENTRY_RECORD_BYTES {
                return Err(Error::LimitExceeded);
            }
        }
        let head = publication::Head::from_ledger(self)?;
        head.write(local)?;
        // Encodability was checked before writing. Stream one bounded row at a
        // time; retaining every serialized closed context would be quadratic.
        for (id, event) in &self.events {
            if previous.events.get(id) != Some(event) {
                local.put(&[PREFIX, id.as_bytes()].concat(), &encode(event.as_ref())?)?;
            }
        }
        Ok(())
    }
    /// Apply one complete receipt plan atomically in memory. Each source has one
    /// final qualification; exact duplicates are harmless and replay is idempotent.
    /// Local provisional publication belongs to `Publication`, never a receipt.
    /// Callers qualify closed source contexts before constructing these witnesses.
    pub fn apply_receipt(&mut self, observations: &[Observation]) -> Result<()> {
        let mut work = Work(0);
        let plan = Self::receipt_plan(observations, &mut work)?;
        let mut candidate = self.clone();
        let mut excluded = BTreeSet::new();
        for (id, event) in plan {
            if candidate.qualify(id, event)? {
                excluded.insert(id);
            }
        }
        // A later exact acceptance promotes only that event. Persist the status
        // of each unconfirmed descendant now, so accepting an ancestor cannot
        // silently resurrect its old, independently unaccepted descendants.
        for event in candidate.events.values_mut() {
            work.charge(1 + event.source.observed.len())?;
            if matches!(event.status, Status::Provisional | Status::Authored)
                && event.source.observed.iter().any(|id| excluded.contains(id))
            {
                Arc::make_mut(event).status = Status::Excluded;
            }
        }
        candidate.validate_with_work(&mut work)?;
        *self = candidate;
        Ok(())
    }
    fn receipt_plan<'a>(
        observations: &'a [Observation],
        work: &mut Work,
    ) -> Result<BTreeMap<EntryId, &'a Observation>> {
        if observations.len() > MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        let mut plan = BTreeMap::new();
        for event in observations {
            work.charge(1 + event.source.observed.len())?;
            event.validate()?;
            if matches!(event.status, Status::Provisional | Status::Authored) {
                return Err(Error::InvalidInput("receipt cannot publish provisional observations"));
            }
            if let Some(prior) = plan.insert(event.source.id()?, event)
                && prior != event
            {
                return Err(Error::InvalidInput("conflicting namespace receipt observations"));
            }
        }
        Ok(plan)
    }
    /// Return whether this receipt newly excludes the source. Replaying an old
    /// exclusion must not sweep later work authored from confirmed opaque edges.
    fn qualify(&mut self, id: EntryId, event: &Observation) -> Result<bool> {
        if let Some(prior) = self.events.get(&id) {
            if prior.source != event.source
                || prior.status == Status::Confirmed
                    && event.status == Status::Confirmed
                    && prior.assignment != event.assignment
            {
                return Err(Error::corrupt("namespace observation identity reused"));
            }
            if matches!(
                (prior.status, event.status),
                (Status::Confirmed, Status::Rejected) | (Status::Rejected, Status::Confirmed)
            ) {
                return Err(Error::corrupt("contradictory namespace qualification"));
            }
            if matches!(prior.status, Status::Confirmed | Status::Rejected) || **prior == *event {
                return Ok(false);
            }
        } else if self.events.len() == MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        self.events.insert(id, Arc::new(event.clone()));
        Ok(event.status == Status::Excluded)
    }
    fn validate(&self) -> Result<()> {
        self.validate_with_work(&mut Work(0))
    }
    fn validate_with_work(&self, work: &mut Work) -> Result<()> {
        if self.events.len() > MAX_FACTS {
            return Err(Error::LimitExceeded);
        }
        let mut size = 0usize;
        let mut dictionary = BTreeSet::new();
        for observation in self.events.values() {
            work.charge(1 + 3 * observation.source.observed.len())?;
            size = size.checked_add(observation.validate()?).ok_or(Error::LimitExceeded)?;
            if size > MAX_SOURCE_BYTES {
                return Err(Error::LimitExceeded);
            }
            for id in std::iter::once(observation.source.id()?)
                .chain(observation.source.observed.iter().copied())
            {
                dictionary.insert(id);
                if dictionary.len() > MAX_FACTS {
                    return Err(Error::LimitExceeded);
                }
            }
            if observation.status == Status::Confirmed
                && observation.source.observed.iter().any(|id| {
                    self.events.get(id).is_some_and(|event| event.status == Status::Rejected)
                })
            {
                // Dropping this edge would falsely manufacture concurrency.
                return Err(Error::corrupt("contradictory qualified namespace context"));
            }
        }
        // Match the exact dictionary and compact-row admission used by load.
        // A durable receipt must never create a ledger that cannot be reopened.
        check_materialization(self.events.len(), dictionary.len())?;
        Ok(())
    }
    /// Use qualified and provisional closed contexts, including unknown payload
    /// IDs. Exclusions are sticky on individual rows; they never erase a confirmed
    /// head's independently qualified causal edges, even after catalog narrowing.
    pub fn context(&self) -> Result<Vec<EntryId>> {
        self.validate()?;
        let rejected: BTreeSet<_> = self
            .events
            .iter()
            .filter_map(|(id, event)| (event.status == Status::Rejected).then_some(*id))
            .collect();
        let mut context = BTreeSet::new();
        for (id, event) in &self.events {
            if matches!(event.status, Status::Rejected | Status::Excluded)
                || event.source.observed.iter().any(|id| rejected.contains(id))
            {
                continue;
            }
            context.insert(*id);
            context.extend(event.source.observed.iter().copied());
            if context.len() >= MAX_FACTS {
                return Err(Error::LimitExceeded);
            }
        }
        Ok(context.into_iter().collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests check exact evidence persistence and rejection.")]
mod tests {
    use super::*;
    use kvdb::{CreateOptions, Db, storage::mem::MemStorage};
    fn record(ledger: &mut Ledger, source: Witness, status: Status) -> Result<()> {
        if status != Status::Provisional {
            return ledger.apply_receipt(&[Observation {
                source,
                status,
                assignment: (status == Status::Confirmed).then(Assignments::default),
            }]);
        }
        // Exercise the actual local publication boundary when preparing fixtures.
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0))?;
        let mut tx = db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000)?;
        ledger.write_changes(&Ledger::default(), &mut tx.local_indexes())?;
        let mut publication = Publication::load(&tx.local_indexes())?;
        publication.observe(source, &tx.local_indexes())?;
        publication.write(&mut tx.local_indexes())?;
        *ledger = Ledger::load(&tx.local_indexes())?;
        Ok(())
    }
    fn witness(byte: u8, observed: Vec<EntryId>) -> Witness {
        Witness {
            entry: EntryId::from_bytes([byte; 15]).unwrap(),
            revision: EntryId::from_bytes([byte + 1; 15]).unwrap(),
            source_digest: [byte; 32],
            clock: u64::from(byte),
            birth: true,
            observed: observed.into(),
        }
    }
    fn numbered_witness(number: u16, opaque: bool) -> Witness {
        let mut bytes = [0; 15];
        bytes[13..].copy_from_slice(&number.to_be_bytes());
        let entry = EntryId::from_bytes(bytes).unwrap();
        let observed = if opaque {
            (0..4)
                .map(|offset| {
                    let mut bytes = [0; 15];
                    bytes[13..].copy_from_slice(&(10000 + number * 4 + offset).to_be_bytes());
                    EntryId::from_bytes(bytes).unwrap()
                })
                .collect()
        } else {
            Vec::new()
        };
        Witness {
            entry,
            revision: entry,
            source_digest: [3; 32],
            clock: u64::from(number),
            birth: true,
            observed: observed.into(),
        }
    }
    #[test]
    fn authoring_provenance_survives_reload_without_promoting_observed_payloads() {
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let observed = witness(1, vec![]);
        let authored = witness(3, vec![observed.id().unwrap()]);
        let mut tx = db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        let mut publication = Publication::load(&tx.local_indexes()).unwrap();
        publication.observe(observed.clone(), &tx.local_indexes()).unwrap();
        publication.publish(observed.clone(), &tx.local_indexes()).unwrap();
        publication.publish(authored.clone(), &tx.local_indexes()).unwrap();
        publication.write(&mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(source_status(&read, &observed, 0).unwrap(), Some(Status::Provisional));
        assert_eq!(source_status(&read, &authored, 0).unwrap(), Some(Status::Authored));
        assert_eq!(source_status(&read, &witness(5, vec![]), 0).unwrap(), None);
        drop(read);
        let mut tx = db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1001).unwrap();
        let mut ledger = Ledger::load(&tx.local_indexes()).unwrap();
        let before = ledger.clone();
        for status in [Status::Provisional, Status::Authored] {
            assert!(
                ledger
                    .apply_receipt(&[Observation {
                        source: authored.clone(),
                        status,
                        assignment: None,
                    }])
                    .is_err()
            );
            assert_eq!(ledger.events, before.events);
        }
        ledger
            .apply_receipt(&[Observation {
                source: observed.clone(),
                status: Status::Excluded,
                assignment: None,
            }])
            .unwrap();
        assert_eq!(ledger.events[&authored.id().unwrap()].status, Status::Excluded);
        ledger.write_changes(&before, &mut tx.local_indexes()).unwrap();
        let mut publication = Publication::load(&tx.local_indexes()).unwrap();
        assert!(publication.publish(authored.clone(), &tx.local_indexes()).is_err());
        assert!(publication.observe(authored, &tx.local_indexes()).is_err());
        assert!(publication.context().is_empty());
    }

    #[test]
    fn confirmed_normalized_assignments_cannot_change_on_replay() {
        let source = Witness { birth: false, ..witness(1, vec![]) };
        let accepted = Observation {
            source,
            status: Status::Confirmed,
            assignment: Some(Assignments { name: false, parent: true }),
        };
        let mut ledger = Ledger::default();
        ledger.apply_receipt(std::slice::from_ref(&accepted)).unwrap();
        let before = ledger.events.clone();
        let contradictory = Observation {
            assignment: Some(Assignments { name: true, parent: true }),
            ..accepted.clone()
        };
        assert!(ledger.apply_receipt(&[contradictory]).is_err());
        assert_eq!(ledger.events, before);
        ledger.apply_receipt(&[accepted]).unwrap();
        assert_eq!(ledger.events, before);
    }

    #[test]
    fn receipt_capacity_rejection_preserves_prior_reloadable_knowledge() {
        // Both a large opaque dictionary and many tiny rows can exceed reload
        // memory while their wire bytes remain small. Neither may be committed.
        for (count, opaque) in [(1025, true), (2000, false)] {
            let mut ledger = Ledger::default();
            let observations: Vec<_> = (1..=count)
                .map(|number| Observation {
                    source: numbered_witness(number, opaque),
                    status: Status::Excluded,
                    assignment: None,
                })
                .collect();
            assert!(matches!(ledger.apply_receipt(&observations), Err(Error::LimitExceeded)));
            assert!(ledger.events.is_empty());
        }
    }
    #[test]
    fn publish_exclude_cycles_keep_retained_ids_in_the_incremental_head() {
        // Excluded rows no longer contribute usable context, but still consume
        // dictionary and row capacity. Exercise actual authoring and reload.
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        for number in 1..=100 {
            let source = numbered_witness(number, true);
            let mut tx =
                db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000 + u64::from(number)).unwrap();
            let mut publication = Publication::load(&tx.local_indexes()).unwrap();
            publication.publish(source.clone(), &tx.local_indexes()).unwrap();
            publication.write(&mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
            let mut tx =
                db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 2000 + u64::from(number)).unwrap();
            let before = Ledger::load(&tx.local_indexes()).unwrap();
            let mut after = before.clone();
            after
                .apply_receipt(&[Observation {
                    source,
                    status: Status::Excluded,
                    assignment: None,
                }])
                .unwrap();
            after.write_changes(&before, &mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
            let mut tx =
                db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 3000 + u64::from(number)).unwrap();
            let loaded = Ledger::load(&tx.local_indexes()).unwrap();
            assert!(loaded.context().unwrap().is_empty());
            assert_eq!(loaded.events.len(), usize::from(number));
        }
    }
    #[test]
    fn selected_head_keeps_unknown_ancestor_edges_without_confirming_payloads() {
        let deletion = witness(1, vec![]);
        let head = witness(3, vec![deletion.id().unwrap()]);
        let mut ledger = Ledger::default();
        record(&mut ledger, head.clone(), Status::Confirmed).unwrap();
        assert_eq!(ledger.events.len(), 1);
        assert!(ledger.context().unwrap().contains(&deletion.id().unwrap()));
        let before = ledger.context().unwrap();
        assert!(record(&mut ledger, deletion, Status::Rejected).is_err());
        assert_eq!(ledger.context().unwrap(), before);
        record(&mut ledger, head, Status::Provisional).unwrap();
        assert_eq!(ledger.context().unwrap(), before);
    }
    #[test]
    fn exact_provisional_rejection_excludes_descendants_but_preserves_confirmed_deletions() {
        let confirmed = witness(1, vec![]);
        let provisional = witness(3, vec![]);
        let child = witness(5, vec![provisional.id().unwrap()]);
        let mut ledger = Ledger::default();
        record(&mut ledger, confirmed.clone(), Status::Confirmed).unwrap();
        record(&mut ledger, provisional.clone(), Status::Provisional).unwrap();
        record(&mut ledger, child, Status::Provisional).unwrap();
        record(&mut ledger, provisional.clone(), Status::Rejected).unwrap();
        assert_eq!(ledger.context().unwrap(), [confirmed.id().unwrap()]);
        // Same entry/revision with a different canonical payload is a new event.
        let accepted = Witness { source_digest: [9; 32], ..provisional };
        record(&mut ledger, accepted.clone(), Status::Confirmed).unwrap();
        assert!(ledger.context().unwrap().contains(&accepted.id().unwrap()));
    }
    #[test]
    fn later_exact_acceptance_does_not_resurrect_excluded_descendants() {
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let parent = witness(1, vec![]);
        let child = witness(3, vec![parent.id().unwrap()]);
        let mut ledger = Ledger::default();
        record(&mut ledger, parent.clone(), Status::Provisional).unwrap();
        record(&mut ledger, child.clone(), Status::Provisional).unwrap();
        record(&mut ledger, parent.clone(), Status::Excluded).unwrap();
        assert_eq!(ledger.events[&child.id().unwrap()].status, Status::Excluded);
        assert!(ledger.context().unwrap().is_empty());
        let later = witness(5, vec![parent.id().unwrap()]);
        assert!(record(&mut ledger, later, Status::Provisional).is_err());
        let mut tx = db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        ledger.write_changes(&Ledger::default(), &mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let mut tx = db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1001).unwrap();
        let before = Ledger::load(&tx.local_indexes()).unwrap();
        let mut accepted = before.clone();
        record(&mut accepted, parent.clone(), Status::Confirmed).unwrap();
        assert_eq!(accepted.context().unwrap(), [parent.id().unwrap()]);
        assert_eq!(accepted.events[&child.id().unwrap()].status, Status::Excluded);
        assert!(record(&mut accepted, child.clone(), Status::Provisional).is_err());
        accepted.write_changes(&before, &mut tx.local_indexes()).unwrap();
        let before = accepted.clone();
        record(&mut accepted, child.clone(), Status::Confirmed).unwrap();
        accepted.write_changes(&before, &mut tx.local_indexes()).unwrap();
        assert!(
            Ledger::load(&tx.local_indexes())
                .unwrap()
                .context()
                .unwrap()
                .contains(&child.id().unwrap())
        );
    }
    #[test]
    fn exclusion_preserves_confirmed_opaque_edges_and_exact_rejection_stays_terminal() {
        let ancestor = witness(1, vec![]);
        let confirmed = witness(3, vec![ancestor.id().unwrap()]);
        let mut ledger = Ledger::default();
        record(&mut ledger, ancestor.clone(), Status::Provisional).unwrap();
        record(&mut ledger, confirmed.clone(), Status::Confirmed).unwrap();
        record(&mut ledger, ancestor.clone(), Status::Excluded).unwrap();
        let context = ledger.context().unwrap();
        assert!(context.contains(&ancestor.id().unwrap()));
        assert!(context.contains(&confirmed.id().unwrap()));
        // Current-policy exclusion must not contradict an independently qualified
        // selected head. Intrinsic invalidity of the same edge would contradict it.
        assert!(record(&mut ledger, ancestor.clone(), Status::Rejected).is_err());
        assert_eq!(ledger.context().unwrap(), context);
        record(&mut ledger, confirmed.clone(), Status::Excluded).unwrap();
        assert_eq!(ledger.events[&confirmed.id().unwrap()].status, Status::Confirmed);
        // Later work may rely on the independently confirmed head's opaque edge.
        // Replaying the old exclusion cannot retract that newly authored work.
        let mut observed = vec![ancestor.id().unwrap(), confirmed.id().unwrap()];
        observed.sort();
        let child = witness(7, observed);
        record(&mut ledger, child.clone(), Status::Provisional).unwrap();
        let before = ledger.events.clone();
        record(&mut ledger, ancestor, Status::Excluded).unwrap();
        assert_eq!(ledger.events, before);
        assert!(ledger.context().unwrap().contains(&child.id().unwrap()));
        let rejected = witness(5, vec![]);
        record(&mut ledger, rejected.clone(), Status::Rejected).unwrap();
        record(&mut ledger, rejected.clone(), Status::Excluded).unwrap();
        assert_eq!(ledger.events[&rejected.id().unwrap()].status, Status::Rejected);
        assert!(record(&mut ledger, rejected, Status::Confirmed).is_err());
    }
    #[test]
    fn receipt_batch_is_atomic_on_contradiction_and_capacity_failure() {
        // A later contradictory row must not leave earlier promotions applied.
        let rejected = witness(1, vec![]);
        let pending = witness(3, vec![]);
        let mut ledger = Ledger::default();
        record(&mut ledger, rejected.clone(), Status::Rejected).unwrap();
        record(&mut ledger, pending.clone(), Status::Provisional).unwrap();
        assert!(
            ledger
                .apply_receipt(&[
                    Observation {
                        source: pending.clone(),
                        status: Status::Confirmed,
                        assignment: Some(Assignments::default())
                    },
                    Observation {
                        source: rejected.clone(),
                        status: Status::Confirmed,
                        assignment: Some(Assignments::default())
                    },
                ])
                .is_err()
        );
        assert_eq!(ledger.events[&pending.id().unwrap()].status, Status::Provisional);
        assert_eq!(ledger.events[&rejected.id().unwrap()].status, Status::Rejected);
        let oversized = vec![
            Observation {
                source: pending.clone(),
                status: Status::Confirmed,
                assignment: Some(Assignments::default())
            };
            MAX_FACTS + 1
        ];
        assert!(matches!(ledger.apply_receipt(&oversized), Err(Error::LimitExceeded)));
        assert_eq!(ledger.events[&pending.id().unwrap()].status, Status::Provisional);
    }
    #[test]
    fn receipt_replay_and_duplicate_qualification_are_order_independent() {
        // A receipt cannot publish local work or hide an exclusion behind a
        // second status for the same source. Replays retain sticky descendants.
        let parent = witness(1, vec![]);
        let child = witness(3, vec![parent.id().unwrap()]);
        let mut ledger = Ledger::default();
        record(&mut ledger, parent.clone(), Status::Provisional).unwrap();
        record(&mut ledger, child.clone(), Status::Provisional).unwrap();
        let excluded =
            Observation { source: parent.clone(), status: Status::Excluded, assignment: None };
        let confirmed = Observation {
            source: parent.clone(),
            status: Status::Confirmed,
            assignment: Some(Assignments::default()),
        };
        for plan in [
            vec![excluded.clone(), confirmed.clone()],
            vec![confirmed.clone(), excluded.clone()],
            vec![
                excluded.clone(),
                Observation {
                    source: child.clone(),
                    status: Status::Provisional,
                    assignment: None,
                },
            ],
        ] {
            assert!(ledger.apply_receipt(&plan).is_err());
            assert_eq!(ledger.events[&parent.id().unwrap()].status, Status::Provisional);
            assert_eq!(ledger.events[&child.id().unwrap()].status, Status::Provisional);
        }
        let plan = [excluded.clone(), excluded];
        ledger.apply_receipt(&plan).unwrap();
        let before = ledger.events.clone();
        ledger.apply_receipt(&plan).unwrap();
        assert_eq!(ledger.events, before);
        ledger.apply_receipt(&[confirmed]).unwrap();
        assert_eq!(ledger.events[&child.id().unwrap()].status, Status::Excluded);
    }
    #[test]
    fn local_authoring_cannot_restore_an_excluded_provisional_branch() {
        // Reconciliation removes excluded provisional chains from usable knowledge.
        // Reobserving their stored births must not put them back into the head.
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let parent = witness(1, vec![]);
        let child = witness(3, vec![parent.id().unwrap()]);
        let mut ledger = Ledger::default();
        record(&mut ledger, parent.clone(), Status::Provisional).unwrap();
        record(&mut ledger, child.clone(), Status::Provisional).unwrap();
        record(&mut ledger, parent.clone(), Status::Excluded).unwrap();
        let mut tx = db.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        ledger.write_changes(&Ledger::default(), &mut tx.local_indexes()).unwrap();
        let mut publication = Publication::load(&tx.local_indexes()).unwrap();
        assert!(matches!(
            publication.observe(child, &tx.local_indexes()),
            Err(Error::AccessDenied(_))
        ));
        assert!(matches!(
            publication.observe(parent, &tx.local_indexes()),
            Err(Error::AccessDenied(_))
        ));
        assert!(publication.context().is_empty());
        publication.write(&mut tx.local_indexes()).unwrap();
        assert!(Ledger::load(&tx.local_indexes()).unwrap().context().unwrap().is_empty());
    }
    #[test]
    fn knowledge_commits_atomically_outside_replication_and_survives_narrowing() {
        let db = Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap();
        let before = db.begin_read().unwrap().root_hash().unwrap();
        let mut ledger = Ledger::default();
        let event = witness(1, vec![]);
        record(&mut ledger, event.clone(), Status::Confirmed).unwrap();
        let wall = kvdb::clock::EPOCH_UNIX_SECS + 1000;
        {
            let mut tx = db.begin_write(wall).unwrap();
            ledger.write_changes(&Ledger::default(), &mut tx.local_indexes()).unwrap();
            // An aborted receipt cannot persist causal knowledge.
        }
        let mut tx = db.begin_write(wall).unwrap();
        assert!(Ledger::load(&tx.local_indexes()).unwrap().context().unwrap().is_empty());
        ledger.write_changes(&Ledger::default(), &mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        assert_eq!(db.begin_read().unwrap().root_hash().unwrap(), before);
        let mut tx = db.begin_write(wall + 1).unwrap();
        let restored = Ledger::load(&tx.local_indexes()).unwrap();
        assert_eq!(restored.context().unwrap(), [event.id().unwrap()]);
        // A narrower receipt with no event witnesses makes no deletions.
        restored.write_changes(&restored, &mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
    }
}
