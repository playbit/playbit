//! Full-sync reception without holding the live database writer during transport.
use super::*;
use crate::storage::ValueRead;

/// Receives differences against a pinned read snapshot into caller-owned,
/// discardable storage. No live write transaction exists until `merge`.
/// The caller must bound journal disk and destroy its storage on drop.
pub struct PreparedSync<'db, S: Storage, B: SyncStaging, J: SyncStaging> {
    receiver: SyncReceiver<'db, S, Journal<J>, B>,
    hash_key: [u8; 32],
    branch_factor: u8,
}
impl<'db, S: Storage, B: SyncStaging, J: SyncStaging> PreparedSync<'db, S, B, J> {
    pub fn new(
        db: &'db Db<S>,
        entry_staging: B,
        mut journal: J,
        max_journal_bytes: usize,
        out: &mut Vec<u8>,
    ) -> Result<Self> {
        journal.clear()?;
        let resolver = Journal { storage: journal, capacity: max_journal_bytes };
        Ok(Self {
            receiver: SyncReceiver::start(db, resolver, entry_staging, None, None, out)?,
            hash_key: db.meta().hash_key,
            branch_factor: db.meta().branch_factor,
        })
    }
    /// Request the independently keyed sender's actual leaf range. Compare
    /// values against the pinned local source and stage only logical differences.
    /// No local canonical branch or hash is exposed to the sender.
    pub fn new_projection(
        db: &'db Db<S>,
        projection_key: [u8; 32],
        entry_staging: B,
        mut journal: J,
        max_journal_bytes: usize,
        out: &mut Vec<u8>,
    ) -> Result<Self> {
        if !db.meta().syncable {
            return Err(Error::Mode);
        }
        journal.clear()?;
        let resolver = Journal { storage: journal, capacity: max_journal_bytes };
        Ok(Self {
            receiver: SyncReceiver::start(
                db,
                resolver,
                entry_staging,
                None,
                Some(projection_key),
                out,
            )?,
            hash_key: projection_key,
            branch_factor: db.meta().branch_factor,
        })
    }
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<SyncStatus> {
        self.receiver.process(input, out)
    }
    /// Releases the read snapshot and single-entry scratch. Incomplete or failed
    /// transfers cannot produce changes eligible for installation.
    pub fn finish(self) -> Result<PreparedChanges<J>> {
        let RState::Done(end) = self.receiver.state else {
            return Err(Error::Protocol);
        };
        Ok(PreparedChanges {
            journal: self.receiver.resolver.storage,
            hash_key: self.hash_key,
            branch_factor: self.branch_factor,
            observed_clock: self.receiver.observed_clock,
            end,
        })
    }
}

struct Journal<J> {
    storage: J,
    capacity: usize,
}
// key length, value length, origin clock, state (missing/deleted/present).
const HEADER: usize = 15;
impl<J: SyncStaging> SyncResolver for Journal<J> {
    fn resolve(&mut self, delta: &SyncDelta<'_>) -> Result<Resolution> {
        let (kind, clock, value) = match delta.remote {
            SyncEntState::Missing => (0, Clock::ZERO, None),
            SyncEntState::Deleted { clock } => (1, clock, None),
            SyncEntState::Present { clock, value } => (2, clock, Some(value)),
        };
        let length = value.map_or(0, ValueRead::len);
        let record_len = HEADER
            .checked_add(delta.key.len())
            .and_then(|n| n.checked_add(length))
            .ok_or(Error::Capacity)?;
        if self.storage.len().checked_add(record_len).is_none_or(|n| n > self.capacity) {
            return Err(Error::Capacity);
        }
        let mut header = [0; HEADER];
        header[..2].copy_from_slice(&(delta.key.len() as u16).to_le_bytes());
        header[2..6].copy_from_slice(&(length as u32).to_le_bytes());
        header[6..14].copy_from_slice(&clock.encode());
        header[14] = kind;
        self.storage.append(&header)?;
        self.storage.append(delta.key)?;
        if let Some(value) = value {
            let mut copy = [0; 4096];
            let mut offset = 0;
            while offset < length {
                let n = copy.len().min(length - offset);
                value.read_exact_at(offset, &mut copy[..n])?;
                self.storage.append(&copy[..n])?;
                offset += n;
            }
        }
        Ok(Resolution::KeepLocal)
    }
}

/// Completed, bounded journal. It contains remote differences, not a copied
/// replica or a transcript to replay against a changed tree.
pub struct PreparedChanges<J: SyncStaging> {
    journal: J,
    hash_key: [u8; 32],
    branch_factor: u8,
    observed_clock: Clock,
    end: SyncEnd,
}
struct JournalValue<'a, J> {
    journal: &'a J,
    offset: usize,
    len: usize,
}
impl<J: ValueRead> ValueRead for JournalValue<'_, J> {
    fn len(&self) -> usize {
        self.len
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        if offset > self.len {
            return Err(Error::InvalidArgument);
        }
        let n = out.len().min(self.len - offset);
        self.journal.read_at(self.offset + offset, &mut out[..n])
    }
}
impl<J: SyncStaging> PreparedChanges<J> {
    /// A completed exchange with no differences can be acknowledged under
    /// current caller authorization without opening a live writer. This does
    /// not certify a current root: the receive snapshot may have been replaced.
    pub fn is_empty(&self) -> bool {
        self.journal.len() == 0
    }

    /// Inspect completed remote differences without a live writer. This owns
    /// the journal, so a caller can suspend between records (for example while
    /// authenticating ciphertext). The inspector must finish successfully
    /// before this interface returns the changes for a current-state merge.
    pub fn inspect(self) -> PreparedInspection<J> {
        PreparedInspection {
            changes: self,
            cursor: JournalCursor::new(),
            finished: false,
            failed: false,
        }
    }

    /// Re-evaluates every received difference against CURRENT committed data.
    /// The caller supplies current authorization, holds its ordering boundary,
    /// and validates `pending_transaction` before the explicit durable finish.
    /// Any error or dropping the returned merge rolls back all pending changes.
    /// The journal may be reused for a detached comparison. A remote-wins mirror
    /// must start at the same root as the snapshot used to receive this journal;
    /// unchanged subtrees are absent from the journal, not complete remote data.
    pub fn merge<'db, S: Storage, R: SyncResolver>(
        &self,
        db: &'db Db<S>,
        resolver: R,
        wall_secs: u64,
        max_delta_bytes: usize,
    ) -> Result<PreparedMerge<'db, S>> {
        self.merge_projection(db, &db.meta().hash_key, resolver, wall_secs, max_delta_bytes)
    }

    /// Install differences received against an explicitly authorized projection
    /// of this database. The caller supplies its expected transport identity
    /// and resolves incoming logical facts against the current canonical data.
    pub fn merge_projection<'db, S: Storage, R: SyncResolver>(
        &self,
        db: &'db Db<S>,
        projection_key: &[u8; 32],
        mut resolver: R,
        wall_secs: u64,
        max_delta_bytes: usize,
    ) -> Result<PreparedMerge<'db, S>> {
        if !db.meta().syncable {
            return Err(Error::Mode);
        }
        if projection_key != &self.hash_key {
            return Err(Error::HashKeyMismatch);
        }
        if db.meta().branch_factor != self.branch_factor {
            return Err(Error::BranchFactorMismatch);
        }
        let mut wtxn = db.begin_write(wall_secs)?;
        wtxn.capture_changes(max_delta_bytes);
        let snapshot = db.begin_read()?;
        if self.observed_clock >= wtxn.clock() {
            wtxn.set_clock(wtxn.clock().recv(self.observed_clock, 0)?)?;
        }
        let mut repair = PendingRepair::default();
        let mut cursor = JournalCursor::new();
        let mut applied = 0;
        let mut deltas = 0;
        while cursor.next(self, |stored_key, remote| {
            let source = snapshot.raw().snapshot_value(stored_key)?;
            let local = source.as_ref().map(|value| StreamEntry::new(value)).transpose()?;
            let key = &stored_key[1..];
            let delta = SyncDelta {
                key,
                local: local.as_ref().map_or(SyncEntState::Missing, StreamEntry::state),
                remote,
            };
            if resolver.resolve(&delta)? == Resolution::UseRemote {
                repair.prepare(&mut wtxn, key)?;
                let changed = match delta.remote {
                    SyncEntState::Missing => wtxn.sync_forget_deferred(key)?.then_some(false),
                    SyncEntState::Deleted { clock } => wtxn.sync_put_from_deferred(
                        key,
                        &crate::storage::SliceValue(&[]),
                        ENT_FLAG_TOMBSTONE,
                        clock,
                    )?,
                    SyncEntState::Present { clock, value } => {
                        wtxn.sync_put_from_deferred(key, value, 0, clock)?
                    }
                };
                if let Some(stable) = changed {
                    repair.changed(key, stable);
                    applied += 1;
                }
            }
            Ok(())
        })? {
            deltas += 1;
        }
        repair.flush(&mut wtxn)?;
        Ok(PreparedMerge { wtxn, end: self.end, deltas, applied })
    }
}

/// Bounded read-only inspection of a completed journal. Each callback receives
/// a borrowed streamed value, not an allocated copy. No database is borrowed.
pub struct PreparedInspection<J: SyncStaging> {
    changes: PreparedChanges<J>,
    cursor: JournalCursor,
    finished: bool,
    failed: bool,
}
impl<J: SyncStaging> PreparedInspection<J> {
    /// Visit one difference. Returns false only after the complete journal has
    /// been visited. A callback/read error permanently prevents `finish`.
    pub fn next(
        &mut self,
        inspect: impl FnOnce(&[u8], SyncEntState<'_>) -> Result<()>,
    ) -> Result<bool> {
        if self.failed {
            return Err(Error::Protocol);
        }
        if self.finished {
            return Ok(false);
        }
        self.failed = true;
        let found = self.cursor.next(&self.changes, |key, remote| inspect(&key[1..], remote))?;
        self.finished = !found;
        self.failed = false;
        Ok(found)
    }

    pub fn finish(self) -> Result<PreparedChanges<J>> {
        if !self.finished || self.failed {
            return Err(Error::Protocol);
        }
        Ok(self.changes)
    }
}

/// Both inspection and merge consume the identical validated record stream.
struct JournalCursor {
    offset: usize,
    previous: [u8; crate::KEY_SIZE_MAX],
    previous_len: usize,
}
impl JournalCursor {
    fn new() -> Self {
        Self { offset: 0, previous: [0; crate::KEY_SIZE_MAX], previous_len: 0 }
    }
    /// The callback gets the level-zero storage key, including its prefix.
    fn next<J: SyncStaging>(
        &mut self,
        changes: &PreparedChanges<J>,
        visit: impl FnOnce(&[u8], SyncEntState<'_>) -> Result<()>,
    ) -> Result<bool> {
        if self.offset == changes.journal.len() {
            return Ok(false);
        }
        let mut header = [0; HEADER];
        changes.journal.read_exact_at(self.offset, &mut header)?;
        let mut offset = self.offset.checked_add(HEADER).ok_or(Error::Corrupt)?;
        let key_len = u16::from_le_bytes(
            header[..2].try_into().expect("checked fixed-width field has the destination width"),
        ) as usize;
        let value_len = u32::from_le_bytes(
            header[2..6].try_into().expect("checked fixed-width field has the destination width"),
        ) as usize;
        let clock = Clock::decode(&header[6..14]).ok_or(Error::Corrupt)?;
        let kind = header[14];
        if key_len == 0
            || key_len > crate::KEY_SIZE_MAX
            || value_len > crate::VALUE_SIZE_MAX
            || kind > 2
            || (kind != 2 && value_len != 0)
            || (kind == 0) != clock.is_zero()
            || clock > changes.observed_clock
        {
            return Err(Error::Corrupt);
        }
        let mut key = [0; crate::KEY_SIZE_MAX + 1];
        changes.journal.read_exact_at(offset, &mut key[1..=key_len])?;
        if self.previous_len > 0 && key[1..=key_len] <= self.previous[..self.previous_len] {
            return Err(Error::Corrupt);
        }
        offset = offset.checked_add(key_len).ok_or(Error::Corrupt)?;
        let end = offset.checked_add(value_len).ok_or(Error::Corrupt)?;
        if end > changes.journal.len() {
            return Err(Error::Corrupt);
        }
        let value = JournalValue { journal: &changes.journal, offset, len: value_len };
        let remote = match kind {
            0 => SyncEntState::Missing,
            1 => SyncEntState::Deleted { clock },
            _ => SyncEntState::Present { clock, value: &value },
        };
        visit(&key[..=key_len], remote)?;
        self.previous[..key_len].copy_from_slice(&key[1..=key_len]);
        self.previous_len = key_len;
        self.offset = end;
        Ok(true)
    }
}

/// One pending live transaction, owned until validation and durable commit.
pub struct PreparedMerge<'db, S: Storage> {
    wtxn: WriteTxn<'db, S>,
    end: SyncEnd,
    deltas: usize,
    applied: usize,
}
impl<'db, S: Storage> PreparedMerge<'db, S> {
    pub fn pending_transaction(&self) -> Result<&WriteTxn<'db, S>> {
        Ok(&self.wtxn)
    }
    /// Update local secondary records against the resolved current destination,
    /// never the older receive snapshot. Failure rolls back the whole merge;
    /// duplicate/no-change merges do not invoke the callback or commit.
    pub fn repair_local_indexes<E: From<Error>>(
        mut self,
        update: impl FnOnce(&mut crate::LocalIndexes<'_, 'db, S>) -> core::result::Result<(), E>,
    ) -> core::result::Result<Self, E> {
        if self.applied > 0 {
            update(&mut self.wtxn.local_indexes())?;
        }
        Ok(self)
    }
    pub fn finish_with_delta(self) -> Result<(SyncOutcome, Option<CommittedDelta>)> {
        let committed = self.applied > 0;
        let delta = if committed { Some(self.wtxn.commit_with_delta()?) } else { None };
        Ok((
            SyncOutcome { end: self.end, deltas: self.deltas, applied: self.applied, committed },
            delta,
        ))
    }
}

#[cfg(test)]
mod tests;
