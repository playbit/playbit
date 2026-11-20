//! Exact selected-entry acceptance on the authenticated live stream. Evidence
//! qualifies only canonical payloads physically present in this event; omitted
//! dependencies remain opaque. The receiver commits evidence with source merge.
use super::{Assignments, EntryId, Error, Result, observations, payloads, source};
use crate::store::volume_index as index;
use kvdb::{LocalIndexes, ReadTxn, Storage, sync::EntState};
use source::cut::Sources;
use std::collections::BTreeMap;

// Small interactive changes share the existing 64 KiB live payload allowance.
// Larger source/evidence sets take the explicit bounded gap-recovery path.
pub const MAX_SOURCES: usize = 64;
pub const MAX_EVIDENCE_BYTES: usize = MAX_SOURCES * 16;
const MAX_DELTA_BYTES: usize = 64 * 1024;
const MATERIALIZED_BYTES: usize = 512 * 1024;

/// Sorted fixed-width event IDs and normalized masks. The transport authenticates
/// scope, epoch, sequence, policy revision, delta and these bytes together.
pub fn validate_evidence(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_EVIDENCE_BYTES || !bytes.len().is_multiple_of(16) {
        return Err(Error::InvalidInput("invalid live namespace evidence length"));
    }
    let mut previous = None;
    for row in bytes.as_chunks::<16>().0 {
        let event = EntryId::from_bytes(row[..15].try_into().expect("fixed evidence ID"))?;
        if previous.is_some_and(|previous| previous >= event) {
            return Err(Error::InvalidInput("live namespace evidence is not ordered"));
        }
        Assignments::from_bits(row[15])?;
        previous = Some(event);
    }
    Ok(())
}

/// Decode the selected namespace body without copying either bounded slice.
/// Shared by native transport and the browser Rust storage boundary.
pub fn decode_payload(payload: &[u8]) -> Result<(&[u8], &[u8])> {
    if !(5..=MAX_DELTA_BYTES).contains(&payload.len()) {
        return Err(Error::InvalidInput("invalid namespace payload length"));
    }
    let count = u32::from_le_bytes(payload[..4].try_into().expect("fixed delta length")) as usize;
    if count == 0 || count > payload.len() - 4 {
        return Err(Error::InvalidInput("invalid namespace delta length"));
    }
    let (delta, evidence) = payload[4..].split_at(count);
    validate_evidence(evidence)?;
    Ok((delta, evidence))
}

/// Build evidence outside the fanout lock from AP's already accepted exact
/// payloads. This does bounded point reads, never a full namespace projection.
pub fn evidence(tx: &ReadTxn<'_, impl Storage>, entry: EntryId, delta: &[u8]) -> Result<Vec<u8>> {
    let mut accepted = BTreeMap::new();
    visit(tx, entry, delta, |decoded, bytes| {
        if let Some(assignment) = payloads::assignment(tx, &decoded.witness, bytes)? {
            assignment.validate_source(&decoded.fact.change)?;
            accepted.insert(decoded.fact.id, assignment);
        }
        Ok(())
    })?;
    let mut bytes = Vec::with_capacity(accepted.len() * 16);
    for (id, assignment) in accepted {
        bytes.extend_from_slice(id.as_bytes());
        bytes.push(assignment.bits());
    }
    validate_evidence(&bytes)?;
    Ok(bytes)
}

/// Confirm exact physically delivered sources, including those losing LWW to
/// an unqualified local head. Never derive a receipt from post-merge winners.
/// The caller owns authenticated stream admission and aborts on any error.
pub(crate) fn apply(
    local: &mut LocalIndexes<'_, '_, impl Storage>,
    entry: EntryId,
    delta: &[u8],
    evidence: &[u8],
) -> Result<bool> {
    validate_evidence(evidence)?;
    payloads::Usage::validate_local(local)?;
    let mut accepted = BTreeMap::new();
    for row in evidence.as_chunks::<16>().0 {
        let event = EntryId::from_bytes(row[..15].try_into().expect("fixed evidence ID"))?;
        accepted.insert(event, Assignments::from_bits(row[15])?);
    }
    let mut rows = BTreeMap::new();
    let mut retained = MAX_SOURCES * 1024;
    visit(local, entry, delta, |decoded, bytes| {
        let Some(assignment) = accepted.get(&decoded.fact.id) else { return Ok(()) };
        assignment.validate_source(&decoded.fact.change)?;
        if let std::collections::btree_map::Entry::Vacant(slot) = rows.entry(decoded.fact.id) {
            retained += decoded.witness.observed.heap_bytes();
            if retained > MATERIALIZED_BYTES / 2 {
                return Err(Error::LimitExceeded);
            }
            slot.insert((
                observations::Observation {
                    source: decoded.witness,
                    status: observations::Status::Confirmed,
                    assignment: Some(*assignment),
                },
                bytes,
            ));
        }
        Ok(())
    })?;
    if rows.len() != accepted.len() {
        return Err(Error::AccessDenied("live acceptance source is absent from delivery"));
    }
    let before = observations::Ledger::load(local)?;
    let mut after = before.clone();
    let (observations, payloads): (Vec<_>, Vec<_>) = rows.into_values().unzip();
    after.apply_receipt(&observations)?;
    let mut changed = before != after;
    for (row, bytes) in observations.iter().zip(payloads) {
        changed |= payloads::retain_new(local, row, bytes, 2 * 1024 * 1024 + retained)?;
    }
    if changed {
        after.write_changes(&before, local)?;
    }
    Ok(changed)
}

fn visit<'a>(
    tx: &impl Sources,
    entry: EntryId,
    delta: &'a [u8],
    mut emit: impl FnMut(source::Decoded, &'a [u8]) -> Result<()>,
) -> Result<()> {
    let mut records = Vec::new();
    let mut error = None;
    kvdb::sync::visit_delta(delta, MAX_DELTA_BYTES, |key, state| {
        if !index::is_index_key(key) || error.is_some() {
            return;
        }
        if records.len() == MAX_SOURCES {
            error = Some(Error::LimitExceeded);
        } else if index::source_entry_id(key).is_ok_and(|id| id == entry) {
            records.push((key, state));
        } else {
            error = Some(Error::AccessDenied("live namespace source is outside selected entry"));
        }
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    if records.is_empty() {
        return Ok(());
    }
    let (clock, bytes) = tx
        .source(&index::entry_key(entry), index::MAX_ENTRY_RECORD_BYTES)?
        .ok_or(Error::NotFound("live namespace declaration"))?;
    let retained = MAX_SOURCES * 1024 + 2 * delta.len();
    admit_decoder(&bytes, retained)?;
    let declaration = source::Declaration::decode(entry, clock, &bytes)?;
    drop(bytes);
    for (key, state) in records {
        let EntState::Present { clock, value } = state else {
            return Err(Error::AccessDenied("namespace sources cannot be erased"));
        };
        admit_decoder(value, retained + declaration.heap_bytes())?;
        if let Some(decoded) = declaration.source(key, clock, value)? {
            emit(decoded, value)?;
        }
    }
    Ok(())
}

fn admit_decoder(bytes: &[u8], retained: usize) -> Result<()> {
    if bytes.len() > index::MAX_ENTRY_RECORD_BYTES
        || crate::store::catalog::index_record_charge(bytes)? + bytes.len() + 16 * 1024 + retained
            > MATERIALIZED_BYTES
    {
        return Err(Error::LimitExceeded);
    }
    Ok(())
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
