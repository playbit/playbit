//! Pinned index source traversal. Canonical acceptance uses immutable history;
//! delivered heads may supply witnesses only after exact independent acceptance.
use super::*;
use crate::store::volume_index as index;
use kvdb::{LocalIndexes, ReadTxn, Storage, WriteTxn, storage::ValueRead};
use std::sync::Arc;
pub(crate) mod current;
mod retained;
mod reuse;
pub(crate) use retained::SourceRef;
pub(crate) use reuse::append_content;

// A cut precedes transfer buffers. Retained facts stay within 2 MiB; decoding
// shares the 2.5 MiB allowance with actual retained state. Large schemas can
// borrow unused fact capacity without increasing the total allocation limit.
const MATERIALIZED_BYTES: usize = 2 * 1024 * 1024;
const DECODER_BYTES: usize = 512 * 1024;
const MAX_RECORDS: usize = MAX_FACTS * 8;

/// Qualify the full canonical cut, never a selected-entry projection. Missing
/// declarations and head-only revisions supply no evidence; their dependents
/// remain Pending. Exhaustion fails the entire cut, never a truncated result.
pub fn accepted(tx: &ReadTxn<'_, impl Storage>) -> Result<BTreeMap<EntryId, Assignments>> {
    Ok(project(tx)?.admission.assignments)
}

/// Preserve the accepted exact sources of an authorized AP publication. The
/// caller must first authorize and validate the pending source transaction, and
/// abort that transaction on any error. Ordinary index repair has no authority
/// to call this boundary; receipt delivery remains a separate physical cut.
pub fn retain_accepted(local: &mut LocalIndexes<'_, '_, impl Storage>) -> Result<()> {
    retain_accepted_changes(local).map(|_| ())
}

/// Newly accepted exact events, including sources whose replicated key did not
/// change in this commit. Fanout must deliver these or declare a selected gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcceptedSource {
    pub entry: EntryId,
    pub event: EntryId,
}

pub fn retain_accepted_changes(
    local: &mut LocalIndexes<'_, '_, impl Storage>,
) -> Result<Vec<AcceptedSource>> {
    super::super::payloads::Usage::validate_local(local)?;
    let projection = project(local)?;
    projection.write_current(local)?;
    retained::retain(local, projection)
}

/// One pinned full-catalog projection. Revision IDs remain separate from event
/// IDs so effective names can report their actual immutable source revisions.
pub(crate) struct Projection {
    pub admission: Admission,
    pub sources: BTreeMap<EntryId, SourceRef>,
    pub current: BTreeMap<EntryId, current::Selection>,
    reusable: Option<reuse::Materialization>,
}

pub(crate) fn project(tx: &impl Sources) -> Result<Projection> {
    // Small cuts can retain their decoded contexts within the same allocation
    // ceiling. Larger cuts discard that attempt and use the compact two-pass
    // representation. Neither path skips source qualification or admission.
    let Materialized { facts, sources, confirmed, usage, dictionary_bytes, ids } =
        match materialize_single(tx) {
            Ok(cut) => cut,
            Err(Error::LimitExceeded) => materialize_compact(tx)?,
            Err(error) => return Err(error),
        };
    let (admission, budget) = super::super::admission::admit_budgeted(&facts)?;
    let current = current::select(tx, &facts, &admission, &sources, &confirmed)?;
    let reusable = usage.map(|usage| reuse::Materialization {
        facts,
        confirmed,
        usage,
        dictionary_bytes,
        ids,
        last_content: None,
        budget,
    });
    Ok(Projection { admission, sources, current, reusable })
}

struct Materialized {
    facts: Vec<Fact>,
    sources: BTreeMap<EntryId, SourceRef>,
    confirmed: BTreeMap<EntryId, Assignments>,
    usage: Option<reuse::Usage>,
    dictionary_bytes: usize,
    ids: usize,
}

fn materialize_single(tx: &impl Sources) -> Result<Materialized> {
    let mut facts = Vec::new();
    let mut sources = BTreeMap::new();
    let mut confirmed = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut heap = 0usize;
    let usage = visit(tx, false, 0, |decoded, assignment| {
        let capacity = if facts.len() == facts.capacity() {
            (facts.capacity() * 2).max(4)
        } else {
            facts.capacity()
        };
        let additions = std::iter::once(&decoded.fact.id)
            .chain(decoded.fact.observed.iter())
            .filter(|id| !ids.contains(*id))
            .count();
        let id_count = ids.len() + additions;
        let next_heap = heap
            .checked_add(materialized_fact_bytes(&decoded.fact, assignment.is_some()))
            .ok_or(Error::LimitExceeded)?;
        let charged = 2 * capacity * size_of::<Fact>() + next_heap + id_count * 64;
        // Charge the collector's seen set too, before any vector/map growth.
        // The doubled capacity covers growth overlap and the normalized clone.
        if capacity > MAX_FACTS
            || id_count > MAX_FACTS
            || charged + (facts.len() + 1) * 64 > MATERIALIZED_BYTES
        {
            return Err(Error::LimitExceeded);
        }
        if capacity > facts.capacity() {
            facts.try_reserve_exact(capacity - facts.len()).map_err(|_| Error::LimitExceeded)?;
        }
        let charged = 2 * facts.capacity() * size_of::<Fact>() + next_heap + id_count * 64;
        if charged + (facts.len() + 1) * 64 > MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
        let source = SourceRef::new(&decoded.witness)?;
        ids.insert(decoded.fact.id);
        ids.extend(decoded.fact.observed.iter().copied());
        if let Some(assignment) = assignment {
            confirmed.insert(decoded.fact.id, assignment);
        }
        sources.insert(decoded.fact.id, source);
        facts.push(decoded.fact);
        heap = next_heap;
        Ok(charged)
    })?;
    // The decoder already validates ordered contexts and creation-context
    // containment. The union above enforces the compact dictionary's ID limit.
    // Release it before admission creates its normalized facts and views.
    let id_count = ids.len();
    drop(ids);
    Ok(Materialized {
        facts,
        sources,
        confirmed,
        usage: Some(usage),
        dictionary_bytes: 0,
        ids: id_count,
    })
}

fn materialize_compact(tx: &impl Sources) -> Result<Materialized> {
    let (dictionary, count) = dictionary(tx, false, 0)?;
    if count > MAX_FACTS || count * size_of::<Fact>() > MATERIALIZED_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut facts = Vec::with_capacity(count);
    let mut sources = BTreeMap::new();
    let mut confirmed = BTreeMap::new();
    // The fixed vector allocation and shared dictionary coexist with resolver
    // maps, normalized fact copies and structural views. Contexts share their
    // immutable storage; per-fact charges reserve cloned strings and map scratch.
    let dictionary_bytes = size_of_val(dictionary.as_ref()) + 32;
    let mut charged = 2 * facts.capacity() * size_of::<Fact>() + dictionary_bytes;
    let usage = visit(tx, false, charged, |mut decoded, assignment| {
        if facts.len() == count {
            return Err(Error::LimitExceeded);
        }
        decoded.fact.observed.compact(&dictionary)?;
        if let Change::Birth { creation_context: Some(context), .. } = &mut decoded.fact.change {
            context.observed.compact(&dictionary)?;
        }
        charged = charged
            .checked_add(materialized_fact_bytes(&decoded.fact, assignment.is_some()))
            .ok_or(Error::LimitExceeded)?;
        if charged + (facts.len() + 1) * 64 > MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
        if let Some(assignment) = assignment {
            confirmed.insert(decoded.fact.id, assignment);
        }
        sources.insert(decoded.fact.id, SourceRef::new(&decoded.witness)?);
        facts.push(decoded.fact);
        Ok(charged)
    })?;
    if facts.len() != count {
        return Err(Error::corrupt("pinned namespace source count changed"));
    }
    Ok(Materialized {
        facts,
        sources,
        confirmed,
        usage: Some(usage),
        dictionary_bytes,
        ids: dictionary.len(),
    })
}

fn materialized_fact_bytes(fact: &Fact, confirmed: bool) -> usize {
    let mut bytes = 736
        + usize::from(confirmed) * 64
        + fact.observed.heap_bytes()
        + 2 * fact.origin.label.as_ref().map_or(0, String::capacity);
    match &fact.change {
        Change::Birth { creation_context, name, deletions, departures, .. } => {
            if let Some(context) = creation_context {
                bytes += context.observed.heap_bytes();
            }
            // Two structural views, canonical/name maps and naming scratch,
            // including minimum BTree node occupancy.
            bytes += 1792
                + 8 * name.capacity()
                + 2 * size_of::<EntryId>() * (deletions.capacity() + departures.capacity());
        }
        Change::Edit { name: Some(name), .. } => bytes += 2 * name.capacity(),
        _ => {}
    }
    bytes
}

/// Intersect full-cut acceptance with physically delivered source payloads.
/// This compact set is transferred separately from the authenticated receipt.
pub fn delivered_ids(
    tx: &ReadTxn<'_, impl Storage>,
    accepted: &BTreeMap<EntryId, Assignments>,
    entry: Option<EntryId>,
) -> Result<BTreeMap<EntryId, Assignments>> {
    if accepted.len() > MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let mut delivered = BTreeMap::new();
    visit(tx, true, accepted.len() * 64, |decoded, _| {
        if entry.is_some_and(|entry| entry != decoded.witness.entry) {
            return Err(Error::AccessDenied("namespace source outside selected receipt scope"));
        }
        let id = decoded.witness.id()?;
        if let Some(assignment) = accepted.get(&id) {
            assignment.validate_source(&decoded.fact.change)?;
            delivered.insert(id, *assignment);
        }
        Ok((accepted.len() + delivered.len()) * 64)
    })?;
    Ok(delivered)
}

/// Extract only physically delivered identities. An opaque observed ID never
/// supplies its payload, and a selected head need not bring its ancestor payloads.
pub fn delivered(
    tx: &ReadTxn<'_, impl Storage>,
    accepted: &BTreeMap<EntryId, Assignments>,
) -> Result<Vec<Witness>> {
    if accepted.len() > MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let (dictionary, _) = dictionary(tx, true, accepted.len() * 64)?;
    let mut witnesses = BTreeMap::new();
    let mut charged = size_of_val(dictionary.as_ref()) + 32 + accepted.len() * 64;
    visit(tx, true, charged, |mut decoded, _| {
        let id = decoded.witness.id()?;
        let Some(assignment) = accepted.get(&id) else { return Ok(charged) };
        assignment.validate_source(&decoded.fact.change)?;
        if let Some(prior) = witnesses.get(&id) {
            if prior != &decoded.witness {
                return Err(Error::corrupt("namespace delivered identity changed"));
            }
            return Ok(charged);
        }
        decoded.witness.observed.compact(&dictionary)?;
        charged = charged
            .checked_add(512 + decoded.witness.observed.heap_bytes())
            .ok_or(Error::LimitExceeded)?;
        if charged > MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
        witnesses.insert(id, decoded.witness);
        Ok(charged)
    })?;
    Ok(witnesses.into_values().collect())
}

// First pass retains only unique IDs. The second pass can compact one decoded
// source at a time; the quadratic closed contexts are never held together.
fn dictionary(tx: &impl Sources, heads: bool, retained: usize) -> Result<(Arc<[EntryId]>, usize)> {
    let mut ids = BTreeSet::new();
    let mut count = 0usize;
    visit(tx, heads, retained, |decoded, _| {
        count += 1;
        for id in std::iter::once(&decoded.fact.id).chain(decoded.fact.observed.iter()) {
            ids.insert(*id);
            if ids.len() > MAX_FACTS {
                return Err(Error::LimitExceeded);
            }
        }
        Ok(retained + ids.len() * 64)
    })?;
    Ok((ids.into_iter().collect::<Vec<_>>().into(), count))
}

/// The same bounded source scan serves pinned reads and a writer's pending
/// tree. No projection may ignore earlier births in its own transaction.
pub(crate) trait Sources {
    fn next_source_key(&self, cursor: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>>;
    fn source(&self, key: &[u8], limit: usize) -> Result<Option<(kvdb::Clock, Vec<u8>)>>;
    fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>>;
    fn local(&self, key: &[u8], limit: usize) -> Result<Option<Vec<u8>>>;
}
impl<S: Storage> Sources for ReadTxn<'_, S> {
    fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(ReadTxn::next_local_key(self, key, exclusive)?)
    }
    fn local(&self, key: &[u8], limit: usize) -> Result<Option<Vec<u8>>> {
        let Some(bytes) = self.local_get(key)? else { return Ok(None) };
        if bytes.len() > limit {
            return Err(Error::LimitExceeded);
        }
        Ok(Some(bytes.to_vec()))
    }

    fn next_source_key(&self, cursor: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(ReadTxn::next_source_key(self, cursor, exclusive)?)
    }
    fn source(&self, key: &[u8], limit: usize) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
        let Some(value) = self.value_reader(key)? else { return Ok(None) };
        if value.len() > limit {
            return Err(Error::LimitExceeded);
        }
        let clock = value.clock().ok_or(Error::InvalidInput("namespace requires source clocks"))?;
        Ok(Some((clock, index::read_sync_value(&value)?)))
    }
}
impl<S: Storage> Sources for WriteTxn<'_, S> {
    fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(WriteTxn::next_local_key(self, key, exclusive)?)
    }
    fn local(&self, key: &[u8], limit: usize) -> Result<Option<Vec<u8>>> {
        let Some(bytes) = self.local_get(key)? else { return Ok(None) };
        if bytes.len() > limit {
            return Err(Error::LimitExceeded);
        }
        Ok(Some(bytes.to_vec()))
    }

    fn next_source_key(&self, cursor: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(WriteTxn::next_source_key(self, cursor, exclusive)?)
    }
    fn source(&self, key: &[u8], limit: usize) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
        let mut source = None;
        self.visit_value(key, |clock, value| -> Result<()> {
            let clock = clock.ok_or(Error::InvalidInput("namespace requires source clocks"))?;
            source = Some((clock, index::read_sync_value_bounded(value, limit)?));
            Ok(())
        })?;
        Ok(source)
    }
}

impl<S: Storage> Sources for LocalIndexes<'_, '_, S> {
    fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(LocalIndexes::next_local_key(self, key, exclusive)?)
    }
    fn local(&self, key: &[u8], limit: usize) -> Result<Option<Vec<u8>>> {
        let Some(bytes) = self.get(key)? else { return Ok(None) };
        if bytes.len() > limit {
            return Err(Error::LimitExceeded);
        }
        Ok(Some(bytes.to_vec()))
    }
    fn next_source_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        Ok(LocalIndexes::next_source_key(self, key, exclusive)?)
    }
    fn source(&self, key: &[u8], limit: usize) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
        let Some((clock, bytes)) = self.source_get_with_clock(key)? else { return Ok(None) };
        if bytes.len() > limit {
            return Err(Error::LimitExceeded);
        }
        Ok(Some((clock, bytes.to_vec())))
    }
}

struct SourceRecord {
    clock: kvdb::Clock,
    bytes: Vec<u8>,
    revision: Option<index::EntryRevision>,
}

pub(crate) struct Scan {
    records: usize,
    bytes: usize,
    pub(crate) retained: usize,
    materialized: usize,
    limit: usize,
    decoder: usize,
}
impl Default for Scan {
    fn default() -> Self {
        Self {
            records: 0,
            bytes: 0,
            retained: 0,
            materialized: 0,
            limit: DECODER_BYTES,
            decoder: 0,
        }
    }
}
impl Scan {
    pub(crate) fn materializing(bytes: usize) -> Result<Self> {
        if bytes > MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(Self {
            materialized: bytes,
            limit: MATERIALIZED_BYTES + DECODER_BYTES,
            ..Self::default()
        })
    }

    pub(crate) fn retain_materialized(&mut self, bytes: usize) -> Result<()> {
        if bytes > MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
        self.materialized = bytes;
        Ok(())
    }

    pub(crate) fn admit_decoder(&mut self, bytes: usize) -> Result<()> {
        self.decoder = self.decoder.max(bytes);
        if bytes.checked_add(self.materialized).is_none_or(|bytes| bytes > self.limit) {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }

    pub(crate) fn read(
        &mut self,
        tx: &impl Sources,
        key: &[u8],
    ) -> Result<Option<(kvdb::Clock, Vec<u8>)>> {
        // Drop the decoded record before a caller retains bytes while scanning
        // another source (notably a head and its immutable history anchor).
        Ok(self.read_record(tx, key)?.map(|record| (record.clock, record.bytes)))
    }

    fn read_record(&mut self, tx: &impl Sources, key: &[u8]) -> Result<Option<SourceRecord>> {
        self.records += 1;
        if self.records > MAX_RECORDS {
            return Err(Error::LimitExceeded);
        }
        let remaining = MAX_SOURCE_BYTES.checked_sub(self.bytes).ok_or(Error::LimitExceeded)?;
        let Some((clock, bytes)) = tx.source(key, remaining.min(index::MAX_ENTRY_RECORD_BYTES))?
        else {
            return Ok(None);
        };
        self.bytes += bytes.len();
        let charge = crate::store::catalog::index_record_charge(&bytes)?;
        self.admit_decoder(charge + bytes.capacity() + self.retained + 16 * 1024)?;
        let (_, record) = index::validate_source_record(&kvdb::sync::Delta {
            key,
            local: kvdb::sync::EntState::Missing,
            remote: kvdb::sync::EntState::Present { value: &bytes, clock },
        })?;
        Ok(Some(SourceRecord { clock, bytes, revision: record }))
    }
}
struct Collector<F> {
    emit: F,
    seen: BTreeSet<EntryId>,
    materialized: usize,
    decoder: usize,
}
impl<F: FnMut(Decoded, Option<Assignments>) -> Result<usize>> Collector<F> {
    fn accept(
        &mut self,
        tx: &impl Sources,
        delivered: bool,
        decoded: Decoded,
        source_retained: usize,
    ) -> Result<usize> {
        let qualification = if delivered {
            retained::Qualification::Unconfirmed
        } else {
            retained::qualification(
                tx,
                &decoded,
                self.materialized + self.seen.len() * 64 + source_retained,
                &mut self.decoder,
                source_retained,
            )?
        };
        let assignment = match qualification {
            retained::Qualification::Excluded => {
                return Ok(self.materialized + self.seen.len() * 64);
            }
            retained::Qualification::Confirmed(assignment) => Some(assignment),
            retained::Qualification::Unconfirmed => None,
        };
        let event = decoded.fact.id;
        let fresh = !self.seen.contains(&event);
        if fresh {
            if self.seen.len() == MAX_FACTS {
                return Err(Error::LimitExceeded);
            }
            self.materialized = (self.emit)(decoded, assignment)?;
        }
        let bytes = self
            .materialized
            .checked_add((self.seen.len() + usize::from(fresh)) * 64)
            .ok_or(Error::LimitExceeded)?;
        if bytes > MATERIALIZED_BYTES {
            return Err(Error::LimitExceeded);
        }
        if fresh {
            self.seen.insert(event);
        }
        Ok(bytes)
    }
}

fn visit(
    tx: &impl Sources,
    heads: bool,
    materialized: usize,
    emit: impl FnMut(Decoded, Option<Assignments>) -> Result<usize>,
) -> Result<reuse::Usage> {
    let mut collector = Collector { emit, seen: BTreeSet::new(), materialized, decoder: 0 };
    let mut scan = Scan::materializing(materialized)?;
    let mut cursor = vec![index::ENTRY_ID_BYTE];
    let mut exclusive = false;
    let mut entries = 0usize;
    while let Some(key) = tx.next_source_key(&cursor, exclusive)? {
        if key.first() != Some(&index::ENTRY_ID_BYTE) {
            break;
        }
        let id = index::source_entry_id(&key)?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(Error::LimitExceeded);
        }
        if let Some((clock, bytes)) = scan.read(tx, &key)? {
            let declaration = Declaration::decode(id, clock, &bytes)?;
            scan.materialized = collector.accept(
                tx,
                heads,
                declaration.birth()?,
                bytes.capacity() + declaration.heap_bytes(),
            )?;
            drop(bytes);
            scan.retained = declaration.heap_bytes();
            visit_revisions(
                tx,
                &declaration,
                heads,
                &mut scan,
                &mut |decoded, source_retained| {
                    collector.accept(tx, heads, decoded, source_retained)
                },
            )?;
            if !heads {
                retained::visit(tx, &declaration, &mut scan, &mut collector)?;
            }
            scan.retained = 0;
        }
        cursor = key;
        exclusive = true;
    }
    Ok(reuse::Usage {
        records: scan.records,
        bytes: scan.bytes,
        decoder: scan.decoder.max(collector.decoder),
    })
}
fn visit_revisions(
    tx: &impl Sources,
    declaration: &Declaration,
    heads: bool,
    scan: &mut Scan,
    emit: &mut impl FnMut(Decoded, usize) -> Result<usize>,
) -> Result<()> {
    let prefix = index::meta_key(index::REVISION, declaration.entry);
    let mut cursor = prefix.to_vec();
    let mut exclusive = false;
    while let Some(key) = tx.next_source_key(&cursor, exclusive)? {
        if !key.starts_with(&prefix) {
            break;
        }
        if let Some(SourceRecord { clock, bytes, revision: record }) = scan.read_record(tx, &key)? {
            let record = record.ok_or(Error::corrupt("history source has no revision"))?;
            if let Some(decoded) = declaration.revision_record(clock, &bytes, record)? {
                scan.materialized = emit(decoded, scan.retained + bytes.capacity())?;
            }
        }
        cursor = key;
        exclusive = true;
    }
    for kind in [index::NAME, index::CONTENT, index::PARENT, index::RETIRED] {
        let key = index::meta_key(kind, declaration.entry);
        let Some((clock, bytes)) = scan.read(tx, &key)? else { continue };
        let revision = index::revision(&bytes)?.revision;
        let anchor = index::revision_key(declaration.entry, revision);
        scan.retained += bytes.capacity();
        if let Some((anchored_clock, anchored_bytes)) = scan.read(tx, &anchor)?
            && (clock != anchored_clock || bytes != anchored_bytes)
        {
            return Err(Error::corrupt("namespace head differs from immutable history"));
        }
        scan.retained -= bytes.capacity();
        if heads && let Some(decoded) = declaration.revision(clock, &bytes)? {
            scan.materialized = emit(decoded, scan.retained + bytes.capacity())?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests build exact source cuts and compare their evidence.")]
mod tests {
    use super::*;
    use crate::store::object_contract::EntryAcl;
    use crate::store::volume_index::EntryInfo;
    use kvdb::{CreateOptions, Db, storage::mem::MemStorage};

    fn db() -> Db<MemStorage> {
        Db::create(MemStorage::new(), &CreateOptions::syncable([7; 32], 0)).unwrap()
    }
    fn id(byte: u8) -> EntryId {
        EntryId::from_bytes([byte; 15]).unwrap()
    }
    fn origin() -> Origin {
        Origin { installation: Id::from_bytes([1; 16]), label: None }
    }
    fn copy(source: &Db<MemStorage>, target: &Db<MemStorage>, keys: &[Vec<u8>]) {
        let read = source.begin_read().unwrap();
        let mut write = target.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        for key in keys {
            let (clock, bytes) = read.get_with_clock(key).unwrap().unwrap();
            write.put_with_clock(key, &bytes, clock).unwrap();
        }
        write.commit().unwrap();
    }
    fn numbered(number: u16) -> EntryId {
        let mut bytes = [0; 15];
        bytes[13..].copy_from_slice(&number.to_be_bytes());
        EntryId::from_bytes(bytes).unwrap()
    }
    fn assert_materializations_equal(tx: &impl Sources) {
        let single = materialize_single(tx).unwrap();
        let compact = materialize_compact(tx).unwrap();
        assert_eq!(single.facts, compact.facts);
        assert_eq!(single.sources, compact.sources);
        assert_eq!(single.confirmed, compact.confirmed);
    }
    fn qualify_and_reload(source: &Db<MemStorage>, count: usize) {
        let read = source.begin_read().unwrap();
        // Both supported large cohorts exceed the raw-context allocation, so
        // these also exercise release of the speculative cut before compaction.
        assert!(matches!(materialize_single(&read), Err(Error::LimitExceeded)));
        let ids = accepted(&read).expect("complete cohort fits the acceptance budget");
        assert_eq!(ids.len(), count);
        let witnesses = delivered(&read, &ids).expect("complete receipt fits the witness budget");
        assert_eq!(witnesses.len(), count);
        let observations: Vec<_> = witnesses
            .into_iter()
            .map(|source| observations::Observation {
                assignment: Some(ids[&source.id().unwrap()]),
                source,
                status: observations::Status::Confirmed,
            })
            .collect();
        let mut ledger = observations::Ledger::default();
        ledger.apply_receipt(&observations).unwrap();
        let target = db();
        let mut tx = target.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 2000).unwrap();
        ledger.write_changes(&observations::Ledger::default(), &mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let mut tx = target.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 2001).unwrap();
        let loaded = observations::Ledger::load(&tx.local_indexes()).unwrap();
        assert_eq!(loaded.context().unwrap(), ids.into_keys().collect::<Vec<_>>());
    }
    #[test]
    fn content_append_matches_full_cut_with_pending_rejected_and_confirmed_sources() {
        let source = db();
        let body = EntryKind::Blob {
            len_bytes: 8,
            digest: [1; 32],
            encrypted_key: [2; 32],
            published_at_micros: 0,
        };
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        index::create(
            &mut tx,
            id(1),
            &EntryInfo {
                namespace: Birth::default(),
                origin: origin(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: id(2),
                name: "file".into(),
                kind: body.clone(),
                protocol: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let assignments = accepted(&source.begin_read().unwrap()).unwrap();
        receipt::retain_fixture(&source, None, &assignments).unwrap();
        let rows = receipt::capture(&source.begin_read().unwrap(), None).unwrap();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
        let mut after = before.clone();
        after.apply_receipt(&rows).unwrap();
        after.write_changes(&before, &mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        // Physical but unresolved/rejected history must survive cache reuse.
        // Neither fact contributes to the local authoring context.
        for (revision, observed) in [(id(3), vec![id(250)]), (id(4), vec![])] {
            let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1001).unwrap();
            let (mut event, _) = index::change_event(
                &tx,
                id(1),
                revision,
                EntryChange::Content { kind: body.clone() },
                &origin(),
            )
            .unwrap();
            event.observed = observed;
            tx.put(&index::revision_key(id(1), revision), &event.canonical().unwrap()).unwrap();
            tx.commit().unwrap();
        }
        let mut prior = project(&source.begin_read().unwrap()).unwrap();
        assert!(prior.admission.events.values().any(|state| *state == EventState::Pending));
        assert!(prior.admission.events.values().any(|state| *state == EventState::Rejected));
        assert_eq!(prior.reusable.as_ref().unwrap().confirmed.len(), 1);
        for number in 10..42 {
            if [18, 34].contains(&number) {
                let raw = prior.reusable.as_mut().unwrap();
                let facts = raw.facts.clone();
                let usage = (raw.usage.records, raw.usage.bytes, raw.usage.decoder);
                let last_content = raw.last_content;
                let budget = (raw.budget.work, raw.budget.projection, raw.budget.source_bytes);
                reuse::recompact(raw).unwrap();
                assert_eq!(raw.facts, facts, "pending/rejected and opaque dependencies survive");
                assert_eq!((raw.usage.records, raw.usage.bytes, raw.usage.decoder), usage);
                assert_eq!(
                    (raw.budget.work, raw.budget.projection, raw.budget.source_bytes),
                    budget
                );
                assert_eq!(raw.last_content, last_content);
                assert!(raw.dictionary_bytes > 0);
            }
            if number == 26 {
                // Reuse the compact exact cut with its shared dictionary too.
                // Later appended contexts may use ordinary IDs alongside bits.
                let compact = materialize_compact(&source.begin_read().unwrap()).unwrap();
                assert!(compact.dictionary_bytes > 0);
                prior.reusable = Some(reuse::Materialization {
                    budget: super::super::super::admission::admit_budgeted(&compact.facts)
                        .unwrap()
                        .1,
                    facts: compact.facts,
                    confirmed: compact.confirmed,
                    usage: compact.usage.unwrap(),
                    dictionary_bytes: compact.dictionary_bytes,
                    ids: compact.ids,
                    last_content: None,
                });
            }
            let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000 + number).unwrap();
            let revision = id(number as u8);
            index::change(
                &mut tx,
                id(1),
                revision,
                EntryChange::Content { kind: body.clone() },
                &origin(),
            )
            .unwrap();
            if number == 10 {
                for field in 0..5 {
                    let mut exhausted = project(&source.begin_read().unwrap()).unwrap();
                    let raw = exhausted.reusable.as_mut().unwrap();
                    let usage = &mut raw.usage;
                    match field {
                        0 => usage.records = MAX_RECORDS,
                        1 => usage.bytes = MAX_SOURCE_BYTES,
                        2 => usage.decoder = MATERIALIZED_BYTES + DECODER_BYTES,
                        3 => raw.dictionary_bytes = MATERIALIZED_BYTES,
                        _ => raw.ids = MAX_FACTS,
                    }
                    let result = append_content(&tx, exhausted, id(1), revision);
                    if field == 4 {
                        // This is an inconsistent union certificate, not real exhaustion.
                        assert!(matches!(result, Err(Error::Corrupt(_))));
                    } else {
                        assert!(result.unwrap().is_none());
                    }
                }
            }
            let next = append_content(&tx, prior, id(1), revision).unwrap().unwrap();
            let full = project(&tx).unwrap();
            assert_eq!(next.admission, full.admission);
            assert_eq!(next.current, full.current);
            assert_eq!(next.sources, full.sources);
            let raw = next.reusable.as_ref().unwrap();
            let ids: BTreeSet<_> = raw
                .facts
                .iter()
                .flat_map(|fact| std::iter::once(&fact.id).chain(fact.observed.iter()))
                .collect();
            assert_eq!(raw.ids, ids.len());
            assert_eq!(raw.ids, full.reusable.as_ref().unwrap().ids);
            assert!(
                next.reusable.as_ref().unwrap().usage.bytes
                    >= full.reusable.as_ref().unwrap().usage.bytes
            );
            assert!(
                next.reusable.as_ref().unwrap().usage.records
                    >= full.reusable.as_ref().unwrap().usage.records
            );
            assert!(
                next.reusable.as_ref().unwrap().usage.decoder
                    >= full.reusable.as_ref().unwrap().usage.decoder
            );
            next.write_current(&mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
            prior = next;
        }
    }

    #[test]
    fn content_scan_accounting_survives_alternation_compaction_and_reload() {
        let source = db();
        let body = EntryKind::Blob {
            len_bytes: 8,
            digest: [1; 32],
            encrypted_key: [2; 32],
            published_at_micros: 0,
        };
        for number in [1, 3] {
            let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + number).unwrap();
            index::create(
                &mut tx,
                numbered(number as u16),
                &EntryInfo {
                    namespace: Birth::default(),
                    origin: origin(),
                    initial_permissions: EntryAcl::Inherit,
                    created_revision: numbered(number as u16 + 1),
                    name: format!("file-{number}"),
                    kind: body.clone(),
                    protocol: None,
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let mut prior = project(&source.begin_read().unwrap()).unwrap();
        for (step, entry) in [1, 1, 3, 3, 1, 1, 3, 3, 1, 1, 3, 3].into_iter().enumerate() {
            if step == 4 {
                prior = project(&source.begin_read().unwrap()).unwrap();
                assert!(prior.reusable.as_ref().unwrap().last_content.is_none());
            }
            if step == 8 {
                let raw = prior.reusable.as_mut().unwrap();
                let head = raw.last_content;
                reuse::recompact(raw).unwrap();
                assert_eq!(raw.last_content, head);
            }
            let entry = numbered(entry);
            let raw = prior.reusable.as_ref().unwrap();
            let previous_bytes = raw.usage.bytes;
            let same_head = raw.last_content.is_some_and(|(id, _)| id == entry);
            let revision = numbered(100 + step as u16);
            let mut tx =
                source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 100 + step as u64).unwrap();
            index::change(
                &mut tx,
                entry,
                revision,
                EntryChange::Content { kind: body.clone() },
                &origin(),
            )
            .unwrap();
            let added = tx.get(&index::revision_key(entry, revision)).unwrap().unwrap().len();
            let next = append_content(&tx, prior, entry, revision).unwrap().unwrap();
            let full = project(&tx).unwrap();
            assert_eq!(next.admission, full.admission);
            assert_eq!(next.current, full.current);
            assert_eq!(next.sources, full.sources);
            let incremental = next.reusable.as_ref().unwrap().usage;
            let fresh = full.reusable.as_ref().unwrap().usage;
            assert!(incremental.bytes >= fresh.bytes);
            assert!(incremental.records >= fresh.records);
            assert!(incremental.decoder >= fresh.decoder);
            if same_head {
                assert!(incremental.bytes < previous_bytes + 3 * added);
            }
            next.write_current(&mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
            prior = next;
        }
    }

    #[test]
    fn content_append_compacts_before_rebuilding_the_source_cut() {
        let source = db();
        let body = EntryKind::Blob {
            len_bytes: 8,
            digest: [1; 32],
            encrypted_key: [2; 32],
            published_at_micros: 0,
        };
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        index::create(
            &mut tx,
            numbered(1),
            &EntryInfo {
                namespace: Birth {
                    creation_context: Some(super::super::super::CreationContext {
                        observed: Vec::new(),
                    }),
                    ..Birth::default()
                },
                origin: origin(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: numbered(2),
                name: "file".into(),
                kind: body.clone(),
                protocol: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let mut prior = project(&source.begin_read().unwrap()).unwrap();
        assert_eq!(prior.reusable.as_ref().unwrap().dictionary_bytes, 0);
        for number in 3..=600u16 {
            let mut tx = source
                .begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000 + u64::from(number))
                .unwrap();
            index::change(
                &mut tx,
                numbered(1),
                numbered(number),
                EntryChange::Content { kind: body.clone() },
                &origin(),
            )
            .unwrap();
            let next = append_content(&tx, prior, numbered(1), numbered(number))
                .unwrap()
                .expect("bounded compaction should preserve incremental publication");
            if next.reusable.as_ref().unwrap().dictionary_bytes > 0 {
                let full = project(&tx).unwrap();
                assert_eq!(next.admission, full.admission);
                assert_eq!(next.current, full.current);
                assert_eq!(next.sources, full.sources);
                assert!(number > 65, "exercise multiple context words");
                return;
            }
            next.write_current(&mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
            prior = next;
        }
        panic!("workload did not reach automatic compaction");
    }

    #[test]
    fn five_hundred_sequential_births_fit_acceptance_and_durable_receipt() {
        // The supported catalog cohort includes closed contexts acquired from
        // actual publication; it is not a collection of independent empty births.
        let source = db();
        for number in 1..=500u16 {
            let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
            index::create(
                &mut tx,
                numbered(number),
                &EntryInfo {
                    namespace: Birth::default(),
                    origin: origin(),
                    initial_permissions: EntryAcl::Inherit,
                    created_revision: numbered(number + 1000),
                    name: format!("object-{number}"),
                    kind: EntryKind::ObjectDb { schema: Default::default() },
                    protocol: Some(index::DatabaseProtocol::Kvdb),
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }
        qualify_and_reload(&source, 500);
    }
    #[test]
    fn thousand_sequential_edits_fit_acceptance_and_durable_receipt() {
        // Exact losing history and every closed dependency survive AP admission,
        // delivered witness capture, local receipt persistence and ledger reload.
        let source = db();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        index::create(
            &mut tx,
            numbered(1),
            &EntryInfo {
                namespace: Birth::default(),
                origin: origin(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: numbered(2),
                name: "directory".into(),
                kind: EntryKind::Directory {},
                protocol: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        for number in 3..=1002u16 {
            let mut tx = source
                .begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000 + u64::from(number))
                .unwrap();
            index::rename(
                &mut tx,
                numbered(1),
                &format!("directory-{number}"),
                numbered(number),
                &origin(),
            )
            .unwrap();
            tx.commit().unwrap();
            if [3, 33, 129].contains(&number) {
                assert_materializations_equal(&source.begin_read().unwrap());
            }
        }
        qualify_and_reload(&source, 1001);
    }
    #[test]
    fn selected_heads_need_full_cut_acceptance_and_losing_history_is_retained() {
        // A selected head conveys its exact payload but cannot certify omitted
        // dependencies. Full immutable history can qualify that same payload.
        let source = db();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        index::create(
            &mut tx,
            id(1),
            &EntryInfo {
                namespace: Birth::default(),
                origin: origin(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: id(2),
                name: "original".into(),
                kind: EntryKind::Directory {},
                protocol: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        for (revision, name) in [(id(3), "first"), (id(4), "second")] {
            let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1001).unwrap();
            index::rename(&mut tx, id(1), name, revision, &origin()).unwrap();
            tx.commit().unwrap();
        }
        assert_materializations_equal(&source.begin_read().unwrap());
        let canonical = accepted(&source.begin_read().unwrap()).unwrap();
        assert_eq!(canonical.len(), 3);
        let selected = db();
        copy(
            &source,
            &selected,
            &[index::entry_key(id(1)).to_vec(), index::meta_key(index::NAME, id(1)).to_vec()],
        );
        assert_eq!(accepted(&selected.begin_read().unwrap()).unwrap().len(), 1);
        let witnesses = delivered(&selected.begin_read().unwrap(), &canonical).unwrap();
        assert_eq!(witnesses.len(), 2);
        let head = witnesses.iter().find(|witness| !witness.birth).unwrap();
        assert_eq!(head.revision, id(4));
        assert_eq!(head.observed.len(), 2);
        let orphan = db();
        copy(&source, &orphan, &[index::revision_key(id(1), id(4)).to_vec()]);
        assert!(accepted(&orphan.begin_read().unwrap()).unwrap().is_empty());
        assert!(delivered(&orphan.begin_read().unwrap(), &canonical).unwrap().is_empty());
        assert_materializations_equal(&selected.begin_read().unwrap());
        assert_materializations_equal(&orphan.begin_read().unwrap());

        let excluded = delivered(&source.begin_read().unwrap(), &canonical)
            .unwrap()
            .into_iter()
            .find(|source| source.revision == id(3))
            .unwrap();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1002).unwrap();
        let mut ledger = observations::Ledger::load(&tx.local_indexes()).unwrap();
        let before = ledger.clone();
        ledger
            .apply_receipt(&[observations::Observation {
                source: excluded,
                status: observations::Status::Excluded,
                assignment: None,
            }])
            .unwrap();
        ledger.write_changes(&before, &mut tx.local_indexes()).unwrap();
        tx.commit().unwrap();
        let read = source.begin_read().unwrap();
        assert_materializations_equal(&read);
        let projected = project(&read).unwrap();
        assert_eq!(projected.admission.events.len(), 1);
        assert_eq!(projected.admission.view.entries[&id(1)].name, "original");
    }
    #[test]
    fn selected_receipt_keeps_normalized_assignments_without_ancestor_payloads() {
        let source = db();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        for (entry, revision, parent, name) in
            [(id(1), id(11), None, "D"), (id(2), id(12), Some(id(1)), "photos")]
        {
            index::create(
                &mut tx,
                entry,
                &EntryInfo {
                    namespace: Birth { parent, ..Birth::default() },
                    origin: origin(),
                    initial_permissions: EntryAcl::Inherit,
                    created_revision: revision,
                    name: name.into(),
                    kind: EntryKind::Directory {},
                    protocol: None,
                },
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let observed =
            observations::Publication::capture(&source.begin_read().unwrap()).unwrap().observed;
        // Both writers observed the same births. The newer move's raw NAME
        // wins storage ordering, but its unchanged name contributes no assignment.
        for (number, change) in [
            (20, EntryChange::Name { name: "pictures".into() }),
            (30, EntryChange::Move { parent: None, name: Some("photos".into()) }),
        ] {
            let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000 + number).unwrap();
            let (mut event, _) =
                index::change_event(&tx, id(2), id(number as u8), change, &origin()).unwrap();
            event.observed = observed.clone();
            let bytes = event.canonical().unwrap();
            tx.put(&index::meta_key(index::NAME, id(2)), &bytes).unwrap();
            if number == 30 {
                tx.put(&index::meta_key(index::PARENT, id(2)), &bytes).unwrap();
            }
            tx.put(&index::revision_key(id(2), event.revision), &bytes).unwrap();
            tx.commit().unwrap();
        }
        let read = source.begin_read().unwrap();
        let projected = project(&read).unwrap();
        assert_eq!(projected.admission.view.entries[&id(2)].name, "pictures");
        assert_eq!(projected.admission.view.entries[&id(2)].parent, None);
        let canonical = projected.admission.assignments;
        let selected = db();
        copy(
            &source,
            &selected,
            &[
                index::entry_key(id(2)).to_vec(),
                index::meta_key(index::NAME, id(2)).to_vec(),
                index::meta_key(index::PARENT, id(2)).to_vec(),
                index::revision_key(id(2), id(20)).to_vec(),
            ],
        );
        assert!(accepted(&selected.begin_read().unwrap()).unwrap().is_empty());
        receipt::retain_fixture(&selected, Some(id(2)), &canonical).unwrap();
        assert_materializations_equal(&selected.begin_read().unwrap());
        let witnesses = receipt::capture(&selected.begin_read().unwrap(), Some(id(2))).unwrap();
        assert_eq!(witnesses.len(), 3);
        for witness in witnesses {
            assert_eq!(witness.source.entry, id(2));
            let expected = match witness.source.revision {
                revision if revision == id(20) => Assignments { name: true, parent: false },
                revision if revision == id(30) => Assignments { name: false, parent: true },
                _ => Assignments::default(),
            };
            assert_eq!(witness.assignment, Some(expected));
        }
    }

    #[test]
    fn source_keys_and_exact_anchor_bytes_and_clocks_are_checked() {
        // Key purpose/suffix and clock are part of source admission, even when
        // decoding the same revision bytes alone would otherwise succeed.
        let source = db();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1000).unwrap();
        index::create(
            &mut tx,
            id(1),
            &EntryInfo {
                namespace: Birth::default(),
                origin: origin(),
                initial_permissions: EntryAcl::Inherit,
                created_revision: id(2),
                name: "original".into(),
                kind: EntryKind::Directory {},
                protocol: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let mut tx = source.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1001).unwrap();
        index::rename(&mut tx, id(1), "renamed", id(3), &origin()).unwrap();
        tx.commit().unwrap();
        let read = source.begin_read().unwrap();
        let (clock, bytes) =
            read.get_with_clock(&index::meta_key(index::NAME, id(1))).unwrap().unwrap();
        for key in [
            index::meta_key(index::CONTENT, id(1)).to_vec(),
            index::revision_key(id(1), id(4)).to_vec(),
        ] {
            let invalid = db();
            copy(&source, &invalid, &[index::entry_key(id(1)).to_vec()]);
            let mut tx = invalid.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1002).unwrap();
            tx.put_with_clock(&key, &bytes, clock).unwrap();
            tx.commit().unwrap();
            assert!(accepted(&invalid.begin_read().unwrap()).is_err());
        }
        for change_clock in [false, true] {
            let invalid = db();
            copy(
                &source,
                &invalid,
                &[index::entry_key(id(1)).to_vec(), index::meta_key(index::NAME, id(1)).to_vec()],
            );
            let mut revision = index::revision(&bytes).unwrap();
            let anchor_clock = if change_clock {
                kvdb::Clock::from_u64(clock.as_u64() + 1)
            } else {
                revision.change = EntryChange::Name { name: "different".into() };
                clock
            };
            revision.origin_clock = anchor_clock.as_u64();
            let mut tx = invalid.begin_write(kvdb::clock::EPOCH_UNIX_SECS + 1002).unwrap();
            tx.put_with_clock(
                &index::revision_key(id(1), id(3)),
                &revision.canonical().unwrap(),
                anchor_clock,
            )
            .unwrap();
            tx.commit().unwrap();
            assert!(accepted(&invalid.begin_read().unwrap()).is_err());
        }
    }
}
