//! Navigation pages decode births/current names, never additive schemas or data.
use super::*;
use crate::store::EntryType;
use kvdb::storage::ValueRead;

pub const MAX_RETAINED_BYTES: usize = 128 * 1024;
pub const MAX_SERIALIZED_BYTES: usize = 96 * 1024;
pub(super) const MAX_SOURCE_BYTES: usize = 192 * 1024;
pub(super) const TARGET_SOURCE_BYTES: usize = 64 * 1024;
const _: () = assert!(3 * MAX_ENTRY_RECORD_BYTES <= MAX_SOURCE_BYTES);
const _: () =
    assert!(MAX_ENTRY_PAGE * (2 * MAX_ENTRY_COMPONENT_BYTES + 192) + 80 <= MAX_SERIALIZED_BYTES);
const _: () = assert!(
    MAX_ENTRY_PAGE * (std::mem::size_of::<EntrySummary>() + 4 * MAX_ENTRY_COMPONENT_BYTES)
        <= MAX_RETAINED_BYTES
);

fn decoder_charge(source: usize) -> Result<usize> {
    source
        .checked_add(MAX_RETAINED_BYTES)
        .filter(|charged| *charged <= MetadataPolicy::MAX_MATERIALIZED_BYTES)
        .ok_or(Error::LimitExceeded)
}

fn source_bytes(birth: usize, name: usize, parent: usize) -> Result<usize> {
    if [birth, name, parent].into_iter().any(|bytes| bytes > MAX_ENTRY_RECORD_BYTES) {
        return Err(Error::LimitExceeded);
    }
    birth.checked_add(name).and_then(|bytes| bytes.checked_add(parent)).ok_or(Error::LimitExceeded)
}

impl<S: Storage> IndexSnapshot<'_, S> {
    pub(super) fn summary_source_bytes(&self, id: EntryId) -> Result<usize> {
        let birth = self
            .tx
            .value_reader(&entry_key(id))?
            .ok_or(Error::corrupt("entry creation record is missing"))?;
        let name = self.tx.value_reader(&meta_key(NAME, id))?;
        let name = name.as_ref().map_or(0, ValueRead::len);
        let parent = self.tx.value_reader(&meta_key(PARENT, id))?;
        source_bytes(birth.len(), name, parent.as_ref().map_or(0, ValueRead::len))
    }

    pub(crate) fn summary(&self, id: EntryId) -> Result<EntrySummary> {
        // Also used independently by retained-cut discovery. Check both lengths
        // before allocation; this transaction pins their values and clocks.
        self.summary_source_bytes(id)?;
        let birth = self
            .tx
            .get_with_clock(&entry_key(id))?
            .ok_or(Error::corrupt("entry creation record is missing"))?;
        let name = self.tx.get_with_clock(&meta_key(NAME, id))?;
        let parent = self.tx.get_with_clock(&meta_key(PARENT, id))?;
        let values = [
            Some((birth.0, &birth.1[..])),
            name.as_ref().map(|(clock, bytes)| (*clock, &bytes[..])),
            parent.as_ref().map(|(clock, bytes)| (*clock, &bytes[..])),
        ];
        decoder_charge(source_charge(&values)?.1)?;
        let (record, _, _) =
            assemble(id, values[0].expect("birth exists"), values[1], None, values[2])?;
        let deleted = self.tx.has(&meta_key(RETIRED, id))?;
        let winner = self.tx.local_get(&name_key(record.namespace.parent, &record.name))?;
        let name_conflict =
            !deleted && winner.as_ref().map(|bytes| id_bytes(bytes)).transpose()? != Some(id);
        let kind = match record.kind {
            EntryKind::Directory {} => EntryType::Directory,
            EntryKind::KvDb {} => EntryType::KvDb,
            EntryKind::ObjectDb { .. } => EntryType::ObjectDb,
            EntryKind::Blob { .. } => EntryType::Blob,
        };
        Ok(EntrySummary {
            id,
            parent: record.namespace.parent,
            name: record.name.into_boxed_str().into_string(),
            kind,
            protocol: record.protocol,
            deleted,
            name_conflict,
        })
    }

    /// Includes conflicts and hidden canonical identities from one admitted
    /// namespace cut. Coalesced aliases and pending births add no catalog row.
    pub fn page(&self, after: Option<EntryId>, limit: usize) -> Result<EntryPage> {
        if limit == 0 || limit > MAX_ENTRY_PAGE {
            return Err(Error::LimitExceeded);
        }
        let view = &self.namespace_projection()?.admission.view;
        let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let mut entries: Vec<EntrySummary> = Vec::with_capacity(limit);
        let mut source_bytes = 0usize;
        for (id, entry) in view.entries.range((start, std::ops::Bound::Unbounded)) {
            let id = *id;
            if entries.len() == limit {
                return Ok(EntryPage { next: entries.last().map(|entry| entry.id), entries });
            }
            source_bytes = source_bytes
                .checked_add(self.summary_source_bytes(id)?)
                .ok_or(Error::LimitExceeded)?;
            if source_bytes > TARGET_SOURCE_BYTES && !entries.is_empty() {
                return Ok(EntryPage { next: entries.last().map(|entry| entry.id), entries });
            }
            assert!(
                source_bytes <= MAX_SOURCE_BYTES,
                "one bounded birth/name pair fits the hard ceiling"
            );
            entries.push(self.namespace_summary(entry)?);
        }
        Ok(EntryPage { entries, next: None })
    }
}

#[cfg(test)]
#[path = "summaries_tests.rs"]
mod tests;
