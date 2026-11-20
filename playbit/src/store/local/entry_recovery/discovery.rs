//! Background-only, bounded discovery of data absent from the accepted catalog.
use super::*;
use crate::store::volume_index::{RecoveryCursor, RejectedEntry, RejectedEntryPage};

const MAX_SCANNED: usize = 128;

impl LocalStore {
    /// Catalog Read permits enumeration; each retained entry still needs current
    /// Read. A scan cursor advances past denied rows without exposing their IDs.
    pub fn rejected_entries(
        &self,
        access: &Access,
        volume: Id,
        after: Option<RecoveryCursor>,
        limit: usize,
    ) -> Result<RejectedEntryPage> {
        if limit == 0
            || limit > volume_index::MAX_ENTRY_PAGE
            || after.is_some_and(|cursor| usize::from(cursor.offset) >= MAX_SCANNED)
        {
            return Err(Error::LimitExceeded);
        }
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.authorize(access, &self.index(volume)?, Permission::Read)?;
        let index = self.db(volume, DatabaseId::Index)?;
        let current = IndexSnapshot::new(index.begin_read()?);
        let mut page = RejectedEntryPage { entries: Vec::with_capacity(limit), next: after };
        let mut scanned = 0;
        for cut in received::rejected::cuts(&index)? {
            if cut.volume != volume {
                return Err(Error::corrupt("rejected cut belongs to a different volume"));
            }
            if after.is_some_and(|after| cut.operation < after.cut) {
                continue;
            }
            let path = self.path.join(crate::paths::RECEIVED).join(cut.operation.to_string());
            let original = open_replica(&path.join("rejected/index.kvdb"), 512 * 1024)?;
            cut.verify_index(&original)?;
            let retained = IndexSnapshot::new(original.begin_read()?);
            for (offset, entry) in cut.entries.into_iter().enumerate() {
                let cursor = RecoveryCursor {
                    cut: cut.operation,
                    offset: u16::try_from(offset)
                        .map_err(|_| Error::corrupt("recovery cursor offset exceeds its limit"))?,
                };
                if after.is_some_and(|after| cursor <= after) {
                    continue;
                }
                if scanned == MAX_SCANNED || page.entries.len() == limit {
                    return Ok(page);
                }
                scanned += 1;
                page.next = Some(cursor);
                let address = EntryAddress { volume, entry };
                match self.authorize_rejected_read(access, address, &current, &retained) {
                    Ok(()) => {
                        page.entries.push(RejectedEntry { cursor, entry: retained.summary(entry)? })
                    }
                    Err(Error::AccessDenied(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        page.next = None;
        Ok(page)
    }
}
