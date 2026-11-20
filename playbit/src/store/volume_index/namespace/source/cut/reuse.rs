//! Reuse only the exact committed predecessor of a locally authored content
//! replacement. Raw facts preserve pending/rejected intent and fresh-admission limits.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Usage {
    pub records: usize,
    pub bytes: usize,
    pub decoder: usize,
}

pub(super) struct Materialization {
    pub facts: Vec<Fact>,
    pub confirmed: BTreeMap<EntryId, Assignments>,
    pub usage: Usage,
    // Context bitsets share this dictionary; charge it once while they live.
    pub dictionary_bytes: usize,
    pub ids: usize,
    // Fixed predecessor bookkeeping, never a persisted source or authority.
    pub last_content: Option<(EntryId, usize)>,
    pub budget: super::super::super::admission::Budget,
}

/// The caller must bind prior to this writer's committed predecessor and make
/// exactly one content change before calling. No receipt, birth, or other source
/// mutation is permitted. Uncertainty/resource exhaustion uses a fresh full cut.
pub(crate) fn append_content(
    tx: &impl Sources,
    prior: Projection,
    entry: EntryId,
    revision: EntryId,
) -> Result<Option<Projection>> {
    let Projection { admission, sources, current, reusable } = prior;
    drop(current);
    let Some(raw) = reusable else { return Ok(None) };
    match append(tx, admission, sources, raw, entry, revision) {
        Ok(projection) => Ok(Some(projection)),
        Err(Error::LimitExceeded) => Ok(None),
        Err(error) => Err(error),
    }
}

fn append(
    tx: &impl Sources,
    mut admission: Admission,
    mut sources: BTreeMap<EntryId, SourceRef>,
    mut raw: Materialization,
    entry: EntryId,
    revision: EntryId,
) -> Result<Projection> {
    let prepared = match prepare(tx, &admission, &sources, &raw, entry, revision) {
        Err(Error::LimitExceeded) => {
            // No decoded append or decoder scratch survives this failed attempt.
            // Compact this exclusive predecessor once, without resetting any
            // physical-source or admission certificate, then retry admission.
            recompact(&mut raw)?;
            prepare(tx, &admission, &sources, &raw, entry, revision)?
        }
        result => result?,
    };
    let Prepared { fact, source, usage, source_bytes, capacity, ids, budget } = prepared;
    if capacity > raw.facts.capacity() {
        raw.facts
            .try_reserve_exact(capacity - raw.facts.len())
            .map_err(|_| Error::LimitExceeded)?;
    }
    if raw.facts.capacity() > capacity {
        // Allocators may grant extra capacity. Do not retain an uncharged vector.
        return Err(Error::LimitExceeded);
    }
    if let Some(budget) = budget {
        admission.events.insert(fact.id, EventState::Accepted);
        admission.assignments.insert(fact.id, Assignments::default());
        raw.budget = budget;
        raw.facts.push(fact);
    } else {
        // Release the prior view/maps before full admission allocates its own.
        drop(admission);
        raw.facts.push(fact);
        (admission, raw.budget) = super::super::super::admission::admit_budgeted(&raw.facts)?;
    }
    let facts = raw.facts;
    sources.insert(source.event, source);
    let confirmed = raw.confirmed;
    let current = current::select(tx, &facts, &admission, &sources, &confirmed)?;
    Ok(Projection {
        admission,
        sources,
        current,
        reusable: Some(Materialization {
            facts,
            confirmed,
            usage,
            dictionary_bytes: raw.dictionary_bytes,
            ids,
            last_content: Some((entry, source_bytes)),
            budget: raw.budget,
        }),
    })
}

struct Prepared {
    fact: Fact,
    source: SourceRef,
    usage: Usage,
    source_bytes: usize,
    capacity: usize,
    ids: usize,
    budget: Option<super::super::super::admission::Budget>,
}
fn prepare(
    tx: &impl Sources,
    admission: &Admission,
    sources: &BTreeMap<EntryId, SourceRef>,
    raw: &Materialization,
    entry: EntryId,
    revision: EntryId,
) -> Result<Prepared> {
    let (fact, source, usage, source_bytes) = read_content(tx, sources, raw, entry, revision)?;
    let (capacity, charged) = append_capacity(raw, &fact)?;
    let budget = super::super::super::admission::content_append_budget(
        admission, raw.budget, &raw.facts, &fact,
    )?;
    let ids = append_ids(raw, &fact, charged, usage.decoder, budget.is_some())?;
    Ok(Prepared { fact, source, usage, source_bytes, capacity, ids, budget })
}

/// Change only the representation of an exclusively owned predecessor. The
/// caller discards it on failure; source/admission certificates stay unchanged.
pub(super) fn recompact(raw: &mut Materialization) -> Result<()> {
    let mut retained = raw.dictionary_bytes + 2 * raw.facts.capacity() * size_of::<Fact>();
    for fact in &raw.facts {
        retained += materialized_fact_bytes(fact, raw.confirmed.contains_key(&fact.id));
    }
    if retained > MATERIALIZED_BYTES || raw.ids > MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let dictionary = compact_dictionary(raw, retained)?;
    let dictionary_bytes = 32 + size_of_val(dictionary.as_ref());
    retained += dictionary_bytes; // Old and new dictionaries coexist until the last replacement.
    for fact in &mut raw.facts {
        compact_context(&mut fact.observed, &dictionary, &mut retained)?;
        if let Change::Birth { creation_context: Some(context), .. } = &mut fact.change {
            compact_context(&mut context.observed, &dictionary, &mut retained)?;
        }
    }
    retained -= raw.dictionary_bytes;
    if retained > MATERIALIZED_BYTES {
        return Err(Error::LimitExceeded);
    }
    raw.dictionary_bytes = dictionary_bytes;
    Ok(())
}
fn compact_dictionary(raw: &Materialization, retained: usize) -> Result<Arc<[EntryId]>> {
    // BTree nodes and simultaneous Vec/Arc dictionary storage, before allocation.
    let scratch = 64 * raw.ids + 2 * (32 + raw.ids * size_of::<EntryId>());
    if retained + scratch > MATERIALIZED_BYTES + DECODER_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut ids = BTreeSet::new();
    for fact in &raw.facts {
        if ids.len() == raw.ids || !ids.insert(fact.id) {
            return Err(Error::corrupt("namespace dictionary count changed"));
        }
    }
    // The retained exact union count proves there are no opaque IDs when
    // every dictionary member is already one of these unique fact identities.
    if ids.len() < raw.ids {
        for fact in &raw.facts {
            for id in &fact.observed {
                if !ids.contains(id) {
                    if ids.len() == raw.ids {
                        return Err(Error::corrupt("namespace dictionary count changed"));
                    }
                    ids.insert(*id);
                }
            }
        }
    }
    if ids.len() != raw.ids {
        return Err(Error::corrupt("namespace dictionary count changed"));
    }
    Ok(ids.into_iter().collect::<Vec<_>>().into())
}
fn compact_context(
    context: &mut super::super::super::Context,
    dictionary: &Arc<[EntryId]>,
    retained: &mut usize,
) -> Result<()> {
    let words = 32 + dictionary.len().div_ceil(64) * size_of::<u64>();
    // The old context and both Vec/Arc bitmap allocations coexist briefly.
    if *retained + 2 * words > MATERIALIZED_BYTES + DECODER_BYTES {
        return Err(Error::LimitExceeded);
    }
    let old = context.heap_bytes();
    context.compact(dictionary)?;
    *retained = *retained - old + context.heap_bytes();
    Ok(())
}

fn read_content(
    tx: &impl Sources,
    sources: &BTreeMap<EntryId, SourceRef>,
    raw: &Materialization,
    entry: EntryId,
    revision: EntryId,
) -> Result<(Fact, SourceRef, Usage, usize)> {
    // Existing materialization remains live while reading the new source.
    let retained = raw.dictionary_bytes
        + 2 * raw.facts.capacity() * size_of::<Fact>()
        + raw
            .facts
            .iter()
            .map(|fact| materialized_fact_bytes(fact, raw.confirmed.contains_key(&fact.id)))
            .sum::<usize>();
    let mut scan = Scan::materializing(retained)?;
    let (clock, bytes) = scan
        .read(tx, &index::entry_key(entry))?
        .ok_or(Error::corrupt("content declaration missing"))?;
    let declaration = Declaration::decode(entry, clock, &bytes)?;
    drop(bytes);
    scan.retained = declaration.heap_bytes();
    let record = scan
        .read_record(tx, &index::revision_key(entry, revision))?
        .ok_or(Error::corrupt("content revision missing"))?;
    let source_bytes = record.bytes.len();
    let mut usage = content_usage(raw, entry, source_bytes)?;
    let decoded = declaration
        .revision_record(
            record.clock,
            &record.bytes,
            record.revision.ok_or(Error::corrupt("content source has no revision"))?,
        )?
        .ok_or(Error::corrupt("content source has no namespace fact"))?;
    if !matches!(decoded.fact.change,
        Change::Edit { entry: id, parent: None, name: None, content: true } if id == entry)
        || sources.contains_key(&decoded.fact.id)
    {
        return Err(Error::corrupt("content append changed an existing source"));
    }
    let source = SourceRef::new(&decoded.witness)?;
    let scratch = declaration.heap_bytes() + record.bytes.capacity();
    let qualification =
        retained::qualification(tx, &decoded, retained + scratch, &mut usage.decoder, scratch)?;
    // Authored content is unconfirmed. An existing receipt for the newly
    // generated identity is not the narrow local transition supported here.
    if !matches!(qualification, retained::Qualification::Unconfirmed) {
        return Err(Error::LimitExceeded);
    }
    // A full scan also holds the CONTENT head while decoding its identical
    // immutable anchor. Preserve that larger scratch requirement.
    usage.decoder = usage.decoder.max(scan.decoder + record.bytes.capacity());
    drop((declaration, record.bytes));
    let fact = decoded.fact;
    drop(decoded.witness);
    Ok((fact, source, usage, source_bytes))
}

fn content_usage(raw: &Materialization, entry: EntryId, source_bytes: usize) -> Result<Usage> {
    let mut usage = raw.usage;
    // Keep probe counts conservative. A prior narrow append to this same entry
    // proves the exact physical head and its present anchor. Replace those two
    // scan charges, while retaining the old immutable history's own charge.
    let replaced = match raw.last_content {
        Some((prior, bytes)) if prior == entry => {
            bytes.checked_mul(2).ok_or(Error::LimitExceeded)?
        }
        _ => 0,
    };
    usage.records = usage.records.checked_add(2).ok_or(Error::LimitExceeded)?;
    usage.bytes = usage
        .bytes
        .checked_sub(replaced)
        .and_then(|bytes| bytes.checked_add(source_bytes.checked_mul(3)?))
        .ok_or(Error::LimitExceeded)?;
    if usage.records > MAX_RECORDS || usage.bytes > MAX_SOURCE_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(usage)
}

fn append_capacity(raw: &Materialization, fact: &Fact) -> Result<(usize, usize)> {
    let count = raw.facts.len() + 1;
    if count > MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let capacity = if count > raw.facts.capacity() {
        if raw.dictionary_bytes > 0 { count } else { (raw.facts.capacity() * 2).max(4) }
    } else {
        raw.facts.capacity()
    };
    if capacity > MAX_FACTS {
        return Err(Error::LimitExceeded);
    }
    let mut charged = raw.dictionary_bytes + 2 * capacity * size_of::<Fact>();
    for fact in raw.facts.iter().chain(std::iter::once(fact)) {
        charged = charged
            .checked_add(materialized_fact_bytes(fact, raw.confirmed.contains_key(&fact.id)))
            .ok_or(Error::LimitExceeded)?;
    }
    Ok((capacity, charged))
}

fn append_ids(
    raw: &Materialization,
    fact: &Fact,
    charged: usize,
    decoder: usize,
    eligible: bool,
) -> Result<usize> {
    let count = raw.facts.len() + 1;
    let ids = if eligible {
        // The complete Accepted context adds no IDs. Eligibility proves the
        // new identity occurs in neither old facts nor their observed IDs.
        let ids = raw.ids.checked_add(1).ok_or(Error::LimitExceeded)?;
        let total = charged + (ids + count) * 64;
        if ids > MAX_FACTS
            || total > MATERIALIZED_BYTES
            || total + decoder > MATERIALIZED_BYTES + DECODER_BYTES
        {
            return Err(Error::LimitExceeded);
        }
        ids
    } else {
        let mut ids = BTreeSet::new();
        for fact in raw.facts.iter().chain(std::iter::once(fact)) {
            for id in std::iter::once(&fact.id).chain(fact.observed.iter()) {
                if !ids.contains(id) {
                    let total = charged + (ids.len() + 1 + count) * 64;
                    if ids.len() == MAX_FACTS
                        || total > MATERIALIZED_BYTES
                        || total + decoder > MATERIALIZED_BYTES + DECODER_BYTES
                    {
                        return Err(Error::LimitExceeded);
                    }
                    ids.insert(*id);
                }
            }
        }
        ids.len()
    };
    Ok(ids)
}
