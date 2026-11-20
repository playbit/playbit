//! Automatic directory births record the exact previously observed incarnation.
use super::*;

pub(super) fn set_incarnation(
    tx: &mut WriteTxn<'_, impl Storage>,
    id: EntryId,
    entry: &mut EntryInfo,
    publication: &namespace::observations::Publication,
) -> Result<()> {
    let dictionary: std::sync::Arc<[EntryId]> = entry.namespace.observed.clone().into();
    // Decode the candidate before retaining history. Reserve its source EntryInfo
    // and decoded form (including the caller's cloned observation vector), plus
    // the publication head that remains live throughout this operation.
    let encoded = entry.canonical()?;
    let candidate_charge =
        2 * crate::store::catalog::index_record_charge(&encoded)? + encoded.capacity();
    let mut candidate =
        namespace::source::Declaration::decode(id, tx.clock(), &encoded)?.birth()?.fact;
    candidate.observed.compact(&dictionary)?;
    drop(encoded);
    let mut facts = Vec::with_capacity(entry.namespace.observed.len());
    let mut retained = 2 * facts.capacity() * size_of::<namespace::Fact>()
        + size_of_val(dictionary.as_ref())
        + 32
        + publication.heap_bytes()
        + candidate_charge;
    let mut scan = namespace::source::cut::Scan::materializing(retained)?;
    for event in &entry.namespace.observed {
        let witness = publication.witness(*event, &tx.local_indexes())?;
        scan.retained = witness.observed.heap_bytes();
        let (clock, birth) = scan
            .read(tx, &entry_key(witness.entry))?
            .ok_or(Error::NotFound("observed namespace birth"))?;
        let declaration = namespace::source::Declaration::decode(witness.entry, clock, &birth)?;
        scan.retained += birth.capacity() + declaration.heap_bytes();
        let mut decoded = if witness.birth {
            declaration.birth()?
        } else {
            let (clock, revision) = scan
                .read(tx, &revision_key(witness.entry, witness.revision))?
                .ok_or(Error::NotFound("observed namespace revision"))?;
            declaration
                .revision(clock, &revision)?
                .ok_or(Error::corrupt("observed revision is not a namespace event"))?
        };
        if decoded.witness != witness {
            return Err(Error::AccessDenied("observed namespace source changed"));
        }
        // Contexts retain exact IDs through the one shared observed dictionary.
        // Reserve both source and normalized facts, including copied birth
        // vectors and resolver scratch, before retaining another decoded fact.
        let bytes = compact_fact(&mut decoded.fact, &dictionary)?;
        retained = retained.checked_add(bytes).ok_or(Error::LimitExceeded)?;
        scan.retain_materialized(retained)?;
        facts.push(decoded.fact);
    }
    let (deletions, departures) = namespace::automatic_context(&candidate, &facts)?;
    entry.namespace.deletions = deletions;
    entry.namespace.departures = departures;
    Ok(())
}

fn compact_fact(
    fact: &mut namespace::Fact,
    dictionary: &std::sync::Arc<[EntryId]>,
) -> Result<usize> {
    fact.observed.compact(dictionary)?;
    let mut bytes = 512
        + fact.observed.heap_bytes()
        + 2 * fact.origin.label.as_ref().map_or(0, String::capacity);
    match &mut fact.change {
        namespace::Change::Birth { creation_context, name, deletions, departures, .. } => {
            if let Some(context) = creation_context {
                context.observed.compact(dictionary)?;
                bytes += context.observed.heap_bytes();
            }
            bytes += 1536
                + 8 * name.capacity()
                + 2 * size_of::<EntryId>() * (deletions.capacity() + departures.capacity());
        }
        namespace::Change::Edit { name: Some(name), .. } => bytes += 2 * name.capacity(),
        _ => {}
    }
    Ok(bytes)
}
