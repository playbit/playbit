//! Durable append-log storage, separate from its portable wire types.
use super::*;
use kvdb::{Clock, Db, Storage};

pub(super) fn entry_key(entry: &Entry) -> String {
    format!("e/{}/{}", entry.log, entry.id)
}
fn pending_key(entry: &Entry) -> String {
    format!("p/{}/{}", entry.log, entry.id)
}
fn cursor(db: &Db<impl Storage>, volume: Id, log: EntryId, clock: u64, id: Id) -> Cursor {
    Cursor(format!(
        "1/{volume}/{log}/{}/{clock}/{id}",
        crate::identifier::encode(&db.meta().replica_id.to_be_bytes())
    ))
}
fn cursor_position(
    db: &Db<impl Storage>,
    volume: Id,
    log: EntryId,
    after: Option<&Cursor>,
) -> Result<(u64, Id)> {
    let zero = (0, Id::from_bytes([0; 16]));
    let Some(after) = after else {
        return Ok(zero);
    };
    let (cursor_volume, cursor_log, replica, clock, id) = Cursor::fields(after.as_str())?;
    if cursor_volume != volume || cursor_log != log {
        return Err(Error::InvalidInput("cursor belongs to another volume or log"));
    }
    if replica != db.meta().replica_id {
        return Ok(zero);
    }
    Ok((clock, id))
}

/// Durable append entries with optional client/peer encryption of stored values.
/// AP always uses `None` and preserves the supplied payload bytes unchanged.
/// Entry wire encoding is independent of this local storage encryption.
pub struct Log<'a, S: Storage> {
    pub database: &'a Db<S>,
    pub volume: Id,
    pub entry: EntryId,
    /// Client/peer storage key. Never supply the volume database key on AP.
    pub encryption_key: Option<[u8; 32]>,
    pub now: u64,
}
impl<S: Storage> Log<'_, S> {
    pub fn validate_cursor(&self, after: &Cursor) -> Result<()> {
        cursor_position(self.database, self.volume, self.entry, Some(after))?;
        Ok(())
    }

    /// Caller admits this page against its current policy or detached source cut.
    pub fn apply_page(&self, page: &Page) -> Result<()> {
        self.validate_cursor(&page.cursor)?;
        self.apply(&page.entries, false)?;
        let encoded = crate::store::catalog::encode(&page.cursor)?;
        if self.database.begin_read()?.get(b"remote-cursor")?.as_deref() == Some(encoded.as_slice())
        {
            return Ok(());
        }
        let mut tx = self.database.begin_write(self.now)?;
        tx.put(b"remote-cursor", &encoded)?;
        tx.commit()?;
        Ok(())
    }

    /// Check every live persistent record without a history-sized collection.
    /// Entry identities, arrival indexes and pending pointers must agree.
    pub fn validate(&self) -> Result<()> {
        let tx = self.database.begin_read()?;
        for row in tx.iter_from(b"") {
            let (key, value) = row?;
            self.validate_record(&tx, &key, &value)?;
        }
        Ok(())
    }
    /// Continue over one immutable, caller-fenced preparation in bounded pages.
    pub fn validate_page(&self, after: Option<&[u8]>, limit: usize) -> Result<Option<Vec<u8>>> {
        if limit == 0 || limit > 64 {
            return Err(Error::LimitExceeded);
        }
        let tx = self.database.begin_read()?;
        let mut last = None;
        let mut count = 0;
        for row in tx.iter_from(after.unwrap_or_default()) {
            let (key, value) = row?;
            if Some(key.as_ref()) == after {
                continue;
            }
            if count == limit {
                return Ok(last);
            }
            self.validate_record(&tx, &key, &value)?;
            last = Some(key.to_vec());
            count += 1;
        }
        Ok(None)
    }
    fn validate_record(&self, tx: &kvdb::ReadTxn<'_, S>, key: &[u8], value: &[u8]) -> Result<()> {
        if key == b"ap-bloblog-commit-v1" {
            return if value.len() == 8 {
                Ok(())
            } else {
                Err(Error::corrupt("invalid AP log checkpoint"))
            };
        }
        if key == b"remote-cursor" {
            return self.validate_cursor(&crate::store::catalog::decode(value)?);
        }
        let key = std::str::from_utf8(key)
            .map_err(|_| Error::corrupt("bloblog record key is not UTF-8"))?;
        if key.starts_with("e/") {
            let entry = self.open(key, value)?;
            let (arrival, _) = tx
                .get_with_clock(key.as_bytes())?
                .ok_or(Error::corrupt("bloblog entry clock is missing"))?;
            let index = format!("l/{}/{:016x}/{}", self.entry, arrival.as_u64(), entry.id);
            if tx.get(index.as_bytes())?.as_deref() != Some(key.as_bytes()) {
                return Err(Error::corrupt("bloblog arrival index does not reference its entry"));
            }
        } else if key.starts_with("l/") || key.starts_with("p/") {
            let reference = std::str::from_utf8(value)
                .map_err(|_| Error::corrupt("bloblog reference is not UTF-8"))?;
            let (arrival, stored) = tx
                .get_with_clock(value)?
                .ok_or(Error::corrupt("referenced bloblog entry is missing"))?;
            let entry = self.open(reference, &stored)?;
            let expected = if key.starts_with("l/") {
                format!("l/{}/{:016x}/{}", self.entry, arrival.as_u64(), entry.id)
            } else {
                pending_key(&entry)
            };
            if key != expected {
                return Err(Error::corrupt("bloblog index key does not match its entry"));
            }
        } else {
            return Err(Error::corrupt("unknown bloblog record key"));
        }
        Ok(())
    }

    pub fn seal(&self, entry: &Entry) -> Result<Vec<u8>> {
        if entry.log != self.entry {
            return Err(Error::InvalidInput("entry belongs to another log"));
        }
        #[cfg(not(target_arch = "wasm32"))]
        let mut bytes = encode(std::slice::from_ref(entry))?;
        #[cfg(target_arch = "wasm32")]
        let bytes = encode(std::slice::from_ref(entry))?;
        if let Some(key) = self.encryption_key {
            #[cfg(target_arch = "wasm32")]
            {
                let _ = key;
                Err(Error::InvalidInput("encrypted log storage is unavailable on Web"))
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let key = blake3::derive_key("app.playbit.bloblog.encryption.v1", &key);
                let mut hash = blake3::Hasher::new_keyed(&blake3::derive_key(
                    "app.playbit.bloblog.nonce.v1",
                    &key,
                ));
                hash.update(self.volume.as_bytes());
                hash.update(&bytes);
                let nonce: [u8; 12] = hash.finalize().as_bytes()[..12]
                    .try_into()
                    .expect("checked fixed-width field has the destination width");
                let key = ring::aead::LessSafeKey::new(
                    ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key).map_err(|_| {
                        Error::Internal("could not construct the bloblog encryption key")
                    })?,
                );
                let aad = format!("playbit/bloblog/1/{}/{}", self.volume, entry_key(entry));
                key.seal_in_place_append_tag(
                    ring::aead::Nonce::assume_unique_for_key(nonce),
                    ring::aead::Aad::from(aad.as_bytes()),
                    &mut bytes,
                )
                .map_err(|_| Error::Internal("bloblog encryption failed"))?;
                let mut sealed = nonce.to_vec();
                sealed.extend_from_slice(&bytes);
                Ok(sealed)
            }
        } else {
            Ok(bytes)
        }
    }
    pub fn open(&self, entry_key: &str, bytes: &[u8]) -> Result<Entry> {
        #[cfg(not(target_arch = "wasm32"))]
        let mut bytes = bytes.to_vec();
        #[cfg(target_arch = "wasm32")]
        let bytes = bytes.to_vec();
        let plain = if let Some(key) = self.encryption_key {
            #[cfg(target_arch = "wasm32")]
            {
                let _ = key;
                return Err(Error::InvalidInput("encrypted log storage is unavailable on Web"));
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let key = blake3::derive_key("app.playbit.bloblog.encryption.v1", &key);
                if bytes.len() < 28 {
                    return Err(Error::corrupt("encrypted bloblog entry is truncated"));
                }
                let nonce = bytes[..12]
                    .try_into()
                    .expect("checked fixed-width field has the destination width");
                let key = ring::aead::LessSafeKey::new(
                    ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key).map_err(|_| {
                        Error::Internal("could not construct the bloblog encryption key")
                    })?,
                );
                let aad = format!("playbit/bloblog/1/{}/{entry_key}", self.volume);
                key.open_in_place(
                    ring::aead::Nonce::assume_unique_for_key(nonce),
                    ring::aead::Aad::from(aad.as_bytes()),
                    &mut bytes[12..],
                )
                .map_err(|_| Error::corrupt("bloblog entry authentication failed"))?
            }
        } else {
            bytes.as_slice()
        };
        let mut entries = decode(plain)?;
        if entries.len() != 1 {
            return Err(Error::corrupt("bloblog record must contain exactly one entry"));
        }
        let entry = entries.pop().expect("the decoded entry count was checked as one");
        if entry.log != self.entry || entry_key != self::entry_key(&entry) {
            return Err(Error::corrupt("bloblog entry identity differs from its record key"));
        }
        Ok(entry)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn put(&self, blobs: &[Vec<u8>]) -> Result<(Cursor, Vec<Entry>)> {
        self.put_with_nonce(blobs, Id::generate)
    }

    /// Platform entropy stays outside portable storage and never comes from the guest.
    pub fn put_with_nonce(
        &self,
        blobs: &[Vec<u8>],
        mut nonce: impl FnMut() -> Result<Id>,
    ) -> Result<(Cursor, Vec<Entry>)> {
        validate_blobs(blobs)?;
        let log = self.entry;
        let before = self.database.clock().as_u64();
        let clock = self.database.clock().incr(self.now)?.as_u64();
        let entries = blobs
            .iter()
            .map(|data| {
                let nonce = nonce()?;
                Ok(Entry {
                    log,
                    id: identity(log, clock, nonce, data),
                    clock,
                    nonce,
                    data: data.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.apply(&entries, true)?;
        // A put receipt starts before this batch. Returning our own entries is
        // permitted, and concurrent arrivals must not disappear behind the put.
        Ok((cursor(self.database, self.volume, log, before, Id::from_bytes([255; 16])), entries))
    }
    pub fn apply(&self, entries: &[Entry], pending: bool) -> Result<bool> {
        encode(entries)?;
        let sealed = entries.iter().map(|e| self.seal(e)).collect::<Result<Vec<_>>>()?;
        let mut tx = self.database.begin_write(self.now)?;
        let mut arrival = tx.clock();
        let mut changed = false;
        for (entry, sealed) in entries.iter().zip(sealed) {
            let key = entry_key(entry);
            if let Some(existing) = tx.get(key.as_bytes())? {
                if self.open(&key, &existing)? != *entry {
                    return Err(Error::InvalidInput("bloblog entry identity collision"));
                }
                continue;
            }
            arrival = arrival.recv(Clock::from_u64(entry.clock), self.now)?;
            // Persist the local arrival clock as well as the originating clock.
            // The next transaction cannot assign an earlier arrival after reopen.
            tx.put_with_clock(key.as_bytes(), &sealed, arrival)?;
            let index = format!("{:016x}/{}", arrival.as_u64(), entry.id);
            tx.put(format!("l/{}/{index}", entry.log).as_bytes(), key.as_bytes())?;
            if pending {
                tx.put(pending_key(entry).as_bytes(), key.as_bytes())?;
            }
            changed = true;
        }
        if changed {
            tx.commit()?;
        }
        Ok(changed)
    }
    /// Replay from the start of this local replica.
    pub fn start_cursor(&self) -> Cursor {
        cursor(self.database, self.volume, self.entry, 0, Id::from_bytes([0; 16]))
    }

    /// Boundary after all currently committed arrivals on this replica.
    pub fn cursor(&self) -> Cursor {
        cursor(
            self.database,
            self.volume,
            self.entry,
            self.database.clock().as_u64(),
            Id::from_bytes([255; 16]),
        )
    }
    pub fn get(&self, after: Option<&Cursor>) -> Result<Page> {
        self.get_page(after, false)
    }

    /// Legacy pending entries are retained for explicit recovery, never confirmed reads.
    pub fn get_committed(&self, after: Option<&Cursor>) -> Result<Page> {
        self.get_page(after, true)
    }

    fn get_page(&self, after: Option<&Cursor>, committed: bool) -> Result<Page> {
        let log = self.entry;
        let (clock, id) = cursor_position(self.database, self.volume, log, after)?;
        let prefix = format!("l/{log}/");
        let start = format!("{prefix}{clock:016x}/{id}");
        let tx = self.database.begin_read()?;
        let mut entries = Vec::new();
        let mut size = 4;
        let mut next = cursor(self.database, self.volume, log, clock, id);
        let mut more = false;
        for row in tx.iter_from(start.as_bytes()) {
            let (key, value) = row?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            if &*key == start.as_bytes() {
                continue;
            }
            let reference = std::str::from_utf8(&value)
                .map_err(|_| Error::corrupt("bloblog reference is not UTF-8"))?;
            let stored =
                tx.get(&value)?.ok_or(Error::corrupt("referenced bloblog entry is missing"))?;
            let entry = self.open(reference, &stored)?;
            if committed && tx.get(pending_key(&entry).as_bytes())?.is_some() {
                continue;
            }
            let length = 59 + entry.data.len();
            if entries.len() == MAX_BLOBS || size + length > MAX_BYTES {
                more = true;
                break;
            }
            let suffix = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| Error::corrupt("bloblog arrival index key is not UTF-8"))?;
            let arrival_hex = suffix
                .get(..16)
                .ok_or(Error::corrupt("bloblog arrival index clock is truncated"))?;
            let arrival = u64::from_str_radix(arrival_hex, 16)
                .map_err(|_| Error::corrupt("bloblog arrival index clock is not hexadecimal"))?;
            next = cursor(self.database, self.volume, log, arrival, entry.id);
            size += length;
            entries.push(entry);
        }
        Ok(Page { entries, cursor: next, more })
    }
    pub fn pending(&self) -> Result<Vec<Entry>> {
        let tx = self.database.begin_read()?;
        let mut entries = Vec::new();
        let mut size = 4;
        for row in tx.iter_from(b"p/") {
            let (key, value) = row?;
            if !key.starts_with(b"p/") {
                break;
            }
            let reference = std::str::from_utf8(&value)
                .map_err(|_| Error::corrupt("bloblog reference is not UTF-8"))?;
            let entry = self.open(
                reference,
                &tx.get(&value)?.ok_or(Error::corrupt("referenced bloblog entry is missing"))?,
            )?;
            let length = 59 + entry.data.len();
            if entries.len() == MAX_BLOBS || size + length > MAX_BYTES {
                break;
            }
            size += length;
            entries.push(entry);
        }
        Ok(entries)
    }
    pub fn acknowledge(&self, entries: &[Entry]) -> Result<()> {
        self.acknowledge_changed(entries).map(|_| ())
    }

    /// Report receipt bookkeeping changes only after their durable commit.
    pub(crate) fn acknowledge_changed(&self, entries: &[Entry]) -> Result<bool> {
        if entries.iter().any(|entry| entry.log != self.entry) {
            return Err(Error::InvalidInput("entry belongs to another log"));
        }
        let mut tx = self.database.begin_write(self.now)?;
        let mut changed = false;
        for entry in entries {
            changed |= tx.del(pending_key(entry).as_bytes())?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Remove one bounded page of unaccepted appends from a detached copy.
    /// The caller must retain the original before publishing this copy and
    /// replay the qualified AP pages, including any append whose ack was lost.
    pub fn discard_pending_page(&self) -> Result<(usize, bool)> {
        let mut tx = self.database.begin_write(self.now)?;
        let mut keys = Vec::with_capacity(64);
        let mut complete = true;
        for row in tx.iter_from(b"p/") {
            let (key, _) = row?;
            if !key.starts_with(b"p/") {
                break;
            }
            if keys.len() == 64 {
                complete = false;
                break;
            }
            keys.push(key.to_vec());
        }
        for key in &keys {
            let reference = tx
                .get(key)?
                .ok_or(Error::corrupt("pending bloblog reference is missing"))?
                .to_vec();
            let text = std::str::from_utf8(&reference)
                .map_err(|_| Error::corrupt("bloblog reference is not UTF-8"))?;
            let (arrival, value) = tx
                .get_with_clock(&reference)?
                .ok_or(Error::corrupt("referenced bloblog entry is missing"))?;
            let entry = self.open(text, &value)?;
            if entry.log != self.entry || key.as_slice() != pending_key(&entry).as_bytes() {
                return Err(Error::corrupt("pending bloblog entry does not match its index"));
            }
            let index = format!("l/{}/{:016x}/{}", self.entry, arrival.as_u64(), entry.id);
            if tx.get(index.as_bytes())?.as_deref() != Some(reference.as_ref()) {
                return Err(Error::corrupt("bloblog arrival index does not reference its entry"));
            }
            drop(value);
            tx.del(key)?;
            tx.del(&reference)?;
            tx.del(index.as_bytes())?;
        }
        let count = keys.len();
        if count > 0 {
            tx.commit()?;
        }
        Ok((count, complete))
    }
}
