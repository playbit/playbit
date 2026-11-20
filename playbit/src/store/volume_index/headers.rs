//! Backing enumeration reads immutable identity and retirement only. It neither
//! assembles schema/name/content registers nor retains an independent catalog.
use super::*;
use crate::store::EntryType;
use kvdb::storage::ValueRead;

const RETAINED_BYTES: usize = 4096;
const DECODER_BASE_BYTES: usize = 16 * 1024;
const _: () = assert!(std::mem::size_of::<EntryHeader>() * MAX_ENTRY_PAGE <= RETAINED_BYTES);
const _: () = assert!(2 * MAX_ENTRY_RECORD_BYTES <= MAX_ENTRY_PAGE_BYTES);

fn decoder_charge(source: usize) -> Result<usize> {
    source
        .checked_add(RETAINED_BYTES + DECODER_BASE_BYTES)
        .filter(|charged| *charged <= MetadataPolicy::MAX_MATERIALIZED_BYTES)
        .ok_or(Error::LimitExceeded)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryHeader {
    pub id: EntryId,
    pub kind: EntryType,
    pub protocol: Option<DatabaseProtocol>,
    pub deleted: bool,
}

pub struct HeaderPage {
    pub entries: Vec<EntryHeader>,
    pub next: Option<EntryId>,
}

impl EntryHeader {
    fn from_info(id: EntryId, birth: EntryInfo, deleted: bool) -> Self {
        let kind = match birth.kind {
            EntryKind::Directory {} => EntryType::Directory,
            EntryKind::KvDb {} => EntryType::KvDb,
            EntryKind::ObjectDb { .. } => EntryType::ObjectDb,
            EntryKind::Blob { .. } => EntryType::Blob,
        };
        Self { id, kind, protocol: birth.protocol, deleted }
    }
    pub(super) fn policy(self) -> Result<crate::store::EntryPolicy> {
        if self.deleted {
            return Err(Error::NotFound("retired entry"));
        }
        Ok(crate::store::EntryPolicy { id: self.id, kind: self.kind, protocol: self.protocol })
    }
}

impl<S: Storage> IndexSnapshot<'_, S> {
    pub fn header(&self, id: EntryId) -> Result<EntryHeader> {
        let (birth, deleted) = current::identity(&self.tx, id)?;
        Ok(EntryHeader::from_info(id, birth, deleted))
    }

    /// Detached admission and rejected-source inspection only. Raw identity
    /// discovery does not qualify a birth or grant app access to its backing.
    pub fn source_header(&self, id: EntryId) -> Result<EntryHeader> {
        let reader =
            self.tx.value_reader(&entry_key(id))?.ok_or(Error::NotFound("entry source"))?;
        self.source_header_from_birth(id, &read_sync_value(&reader)?)
    }

    fn source_header_from_birth(&self, id: EntryId, bytes: &[u8]) -> Result<EntryHeader> {
        decoder_charge(crate::store::catalog::index_record_charge(bytes)?)?;
        Ok(EntryHeader::from_info(id, info(bytes)?, self.tx.has(&meta_key(RETIRED, id))?))
    }

    /// Scan at most 64 births and decode at most 1 MiB of identity sources.
    /// An empty page can have a cursor when its births are not yet qualified.
    pub fn headers(&self, after: Option<EntryId>, limit: usize) -> Result<HeaderPage> {
        self.header_page(after, limit, false)
    }

    /// Detached receipt and rejected recovery need every supplied source birth,
    /// including entries whose namespace dependencies have not arrived yet.
    pub fn source_headers(&self, after: Option<EntryId>, limit: usize) -> Result<HeaderPage> {
        self.header_page(after, limit, true)
    }

    fn header_page(
        &self,
        after: Option<EntryId>,
        limit: usize,
        source: bool,
    ) -> Result<HeaderPage> {
        if limit == 0 || limit > MAX_ENTRY_PAGE {
            return Err(Error::LimitExceeded);
        }
        let mut cursor = after.map_or_else(|| vec![ENTRY_ID_BYTE], |id| entry_key(id).to_vec());
        let mut exclusive = after.is_some();
        let mut entries = Vec::with_capacity(limit);
        let mut remaining = MAX_ENTRY_PAGE_BYTES;
        let mut scanned = 0;
        let mut last = after;
        while let Some(key) = self.tx.next_source_key(&cursor, exclusive)? {
            if key.first() != Some(&ENTRY_ID_BYTE) {
                break;
            }
            if key.len() != 16 {
                return Err(Error::corrupt("entry header key has an invalid length"));
            }
            let id = id_bytes(&key[1..])?;
            if scanned == limit {
                return Ok(HeaderPage { next: last, entries });
            }
            let Some((header, bytes)) = self.page_header(id, source, remaining)? else {
                assert!(scanned > 0, "one bounded identity fits an empty header page");
                return Ok(HeaderPage { next: last, entries });
            };
            remaining -= bytes;
            if let Some(header) = header {
                entries.push(header);
            }
            scanned += 1;
            last = Some(id);
            cursor = key;
            exclusive = true;
        }
        Ok(HeaderPage { entries, next: None })
    }

    fn page_header(
        &self,
        id: EntryId,
        source: bool,
        remaining: usize,
    ) -> Result<Option<(Option<EntryHeader>, usize)>> {
        if !source && let Some(identity) = current::Identity::read(&self.tx, id)? {
            let bytes = identity.source_bytes();
            if bytes > remaining {
                return Ok(None);
            }
            let (birth, deleted) = identity.decode(id, RETAINED_BYTES)?;
            return Ok(Some((Some(EntryHeader::from_info(id, birth, deleted)), bytes)));
        }
        let reader =
            self.tx.value_reader(&entry_key(id))?.ok_or(Error::NotFound("entry source"))?;
        if reader.len() > MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        if reader.len() > remaining {
            return Ok(None);
        }
        // Even skipped unqualified births must be canonical bounded sources.
        let header = self.source_header_from_birth(id, &read_sync_value(&reader)?)?;
        Ok(Some((source.then_some(header), reader.len())))
    }
}

#[cfg(test)]
#[path = "headers_tests.rs"]
mod tests;
