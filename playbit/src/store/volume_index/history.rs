//! Newest-first inspector history. Export/recovery traversal retains ID order.
use super::*;
use std::{cmp::Reverse, collections::BinaryHeap};

/// Exclusive position in descending (origin clock, revision ID) order.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct HistoryCursor {
    pub origin_clock: u64,
    pub revision: EntryId,
}

impl From<&EntryRevision> for HistoryCursor {
    fn from(record: &EntryRevision) -> Self {
        Self { origin_clock: record.origin_clock, revision: record.revision }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct HistoryPage {
    pub revisions: Vec<EntryRevision>,
    pub next: Option<HistoryCursor>,
}

impl<S: Storage> IndexSnapshot<'_, S> {
    /// Scan on the background reader, retaining at most limit + 1 small keys.
    /// Re-read selected payloads in the same pinned transaction. No persisted
    /// index or full-history payload allocation is needed.
    pub fn history(
        &self,
        id: EntryId,
        after: Option<HistoryCursor>,
        limit: usize,
    ) -> Result<HistoryPage> {
        if limit == 0 || limit > MAX_ENTRY_PAGE {
            return Err(Error::LimitExceeded);
        }
        let (clock, bytes) =
            self.tx.get_with_clock(&entry_key(id))?.ok_or(Error::NotFound("entry"))?;
        let mut declaration = RevisionDeclaration::new(id, clock, &bytes, None)?;
        // Keep only declaration identity while scanning; large birth schemas
        // must not consume the next revision's decoding/page allowance.
        declaration.initial = None;
        drop(bytes);
        let candidates = self.history_candidates(id, &declaration, after, limit + 1)?;
        let mut revisions = Vec::with_capacity(limit);
        let mut used = 0;
        let mut charged = 16 * 1024;
        let mut more = false;
        for cursor in candidates {
            if revisions.len() == limit {
                more = true;
                break;
            }
            let key = revision_key(id, cursor.revision);
            let value = if cursor.revision == declaration.revision {
                None
            } else {
                Some(self.tx.get(&key)?.expect("selected history in pinned snapshot"))
            };
            let (size, charge) = match &value {
                Some(value) => (value.len(), crate::store::catalog::index_record_charge(value)?),
                None => (declaration.bytes, declaration.charge),
            };
            if used + size + 256 > MAX_ENTRY_PAGE_BYTES
                || charged + charge > crate::store::MetadataPolicy::MAX_MATERIALIZED_BYTES
            {
                more = true;
                break;
            }
            used += size + 256;
            charged += charge;
            let record = match value {
                Some(value) => declaration.history(&key, &value)?,
                None => {
                    let bytes = self.tx.get(&entry_key(id))?.expect("pinned entry declaration");
                    RevisionDeclaration::new(id, clock, &bytes, None)?
                        .initial
                        .expect("creation requested without a cursor")
                }
            };
            revisions.push(record);
        }
        if revisions.is_empty() && more {
            return Err(Error::LimitExceeded);
        }
        let next = if more { revisions.last().map(HistoryCursor::from) } else { None };
        Ok(HistoryPage { revisions, next })
    }

    fn history_candidates(
        &self,
        id: EntryId,
        declaration: &RevisionDeclaration,
        after: Option<HistoryCursor>,
        capacity: usize,
    ) -> Result<Vec<HistoryCursor>> {
        assert!(capacity <= MAX_ENTRY_PAGE + 1);
        let mut newest = BinaryHeap::with_capacity(capacity);
        let mut consider = |cursor| {
            if after.is_some_and(|after| cursor >= after) {
                return;
            }
            if newest.len() < capacity {
                newest.push(Reverse(cursor));
            } else if let Some(mut oldest) = newest.peek_mut()
                && cursor > oldest.0
            {
                *oldest = Reverse(cursor);
            }
        };
        consider(HistoryCursor {
            origin_clock: declaration.clock.as_u64(),
            revision: declaration.revision,
        });
        let prefix = meta_key(REVISION, id);
        let mut source_bytes = declaration.bytes;
        for (count, row) in self.tx.iter_from(&prefix).enumerate() {
            let (key, value) = row?;
            if !key.starts_with(&prefix) {
                break;
            }
            // Same physical-record ceiling as canonical namespace admission.
            source_bytes = source_bytes.checked_add(value.len()).ok_or(Error::LimitExceeded)?;
            if count + 1 >= namespace::MAX_FACTS * 8 || source_bytes > namespace::MAX_SOURCE_BYTES {
                return Err(Error::LimitExceeded);
            }
            if key.len() != 32 {
                return Err(Error::corrupt("entry history key must be 32 bytes"));
            }
            let charge = crate::store::catalog::index_record_charge(&value)?;
            if 16 * 1024 + charge > crate::store::MetadataPolicy::MAX_MATERIALIZED_BYTES {
                return Err(Error::LimitExceeded);
            }
            consider(HistoryCursor::from(&declaration.history(&key, &value)?));
        }
        Ok(newest.into_sorted_vec().into_iter().map(|Reverse(cursor)| cursor).collect())
    }
}
