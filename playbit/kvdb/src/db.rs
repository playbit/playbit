//! Database handle and transactions.
//!
//! [`Db`] wraps a [`Storage`] backend and implements the Core v1 store
//! contract: snapshot readers, one writer, read-your-writes, atomic commit.
//! A database is either *plain* (raw ordered key-value entries, no
//! overhead) or *syncable* (prolly tree + HLC clocks, enabling replica
//! synchronization), chosen at creation.
//!
//! kvdb is sans-io: wall-clock time is passed in by the caller at
//! [`Db::begin_write`], and randomness (hash key, replica id) at creation.
//!
//! On targets with 64-bit atomics (every native target and wasm32) a [`Db`]
//! is `Sync` whenever its storage is, so one handle can be shared across
//! threads: readers run concurrently with each other and with the single
//! writer, and committed clocks stay strictly increasing.

use crate::KEY_SIZE_MAX;
use crate::clock::Clock;
use crate::encoding::{self, ENT_FLAG_TOMBSTONE, EntVal, META_KEY, Metadata};
use crate::error::{Error, Result};
use crate::storage::{Storage, StorageProps, StorageTxn};
use crate::tree::{self, Tree};
use alloc::vec::Vec;
use core::ops::Deref;

/// Options for opening existing storage. Verification streams the entire
/// syncable tree; enable it for diagnostics, not on each release hot-path open.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    pub verify_on_open: bool,
}

/// Options for [`Db::create`].
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Maintain the prolly tree, hashes, and clocks needed for sync.
    pub syncable: bool,
    /// Tree fan-out (2..=128); ignored for plain databases. A value that
    /// divides 256 is used as given; any other comes out coarser, because
    /// one hash byte decides each boundary (see
    /// [`crate::encoding::is_boundary`]).
    pub branch_factor: u8,
    /// Keyed-BLAKE3 key. Every replica of one database must share it; it
    /// should be generated randomly for the replica set and kept with it.
    pub hash_key: [u8; 32],
    /// Random identity of this replica (local diagnostics only).
    pub replica_id: u64,
}

impl CreateOptions {
    /// A plain transactional store with no sync machinery.
    pub fn plain() -> CreateOptions {
        CreateOptions { syncable: false, branch_factor: 0, hash_key: [0; 32], replica_id: 0 }
    }

    /// A syncable store with the default branch factor.
    pub fn syncable(hash_key: [u8; 32], replica_id: u64) -> CreateOptions {
        CreateOptions { syncable: true, branch_factor: 32, hash_key, replica_id }
    }
}

pub struct Db<S: Storage> {
    storage: S,
    meta: Metadata,
    /// Clock of the most recently committed write transaction.
    clock: CommittedClock,
}

#[cfg(feature = "redb")]
impl Db<crate::redb::RedbStorage> {
    /// Attach this backend to its volume's native snapshot mutation fence.
    pub fn bind_write_epoch(&self, epoch: std::sync::Arc<crate::redb::WriteEpoch>) -> Result<()> {
        self.storage.bind_write_epoch(epoch)
    }
}

/// The latest committed clock, packed as in [`Clock::as_u64`]. Atomic where the
/// target has 64-bit atomics, so a shared `Db` can serve concurrent readers;
/// a plain cell elsewhere (thumbv7em), where `Db` stays single-threaded.
#[cfg(target_has_atomic = "64")]
struct CommittedClock(core::sync::atomic::AtomicU64);

#[cfg(target_has_atomic = "64")]
impl CommittedClock {
    const fn new(clock: Clock) -> CommittedClock {
        CommittedClock(core::sync::atomic::AtomicU64::new(clock.as_u64()))
    }

    fn get(&self) -> Clock {
        Clock::from_u64(self.0.load(core::sync::atomic::Ordering::Acquire))
    }

    /// Advance to `clock` unless the stored clock is already newer.
    fn raise(&self, clock: Clock) {
        self.0.fetch_max(clock.as_u64(), core::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(not(target_has_atomic = "64"))]
struct CommittedClock(core::cell::Cell<u64>);

#[cfg(not(target_has_atomic = "64"))]
impl CommittedClock {
    const fn new(clock: Clock) -> CommittedClock {
        CommittedClock(core::cell::Cell::new(clock.as_u64()))
    }

    fn get(&self) -> Clock {
        Clock::from_u64(self.0.get())
    }

    /// Advance to `clock` unless the stored clock is already newer.
    fn raise(&self, clock: Clock) {
        self.0.set(self.0.get().max(clock.as_u64()));
    }
}

impl<S: Storage> Db<S> {
    /// Create a new database in empty storage.
    pub fn create(storage: S, opts: &CreateOptions) -> Result<Db<S>> {
        if opts.syncable && !(2..=128).contains(&opts.branch_factor) {
            return Err(Error::InvalidArgument);
        }
        let meta = Metadata {
            syncable: opts.syncable,
            branch_factor: opts.branch_factor,
            replica_id: opts.replica_id,
            clock: Clock::ZERO,
            hash_key: opts.hash_key,
        };
        {
            let mut txn = storage.begin(true)?;
            if txn.get(&META_KEY)?.is_some() {
                return Err(Error::Exists);
            }
            txn.put(&META_KEY, &meta.encode())?;
            txn.commit()?;
        }
        Ok(Db { storage, meta, clock: CommittedClock::new(Clock::ZERO) })
    }

    /// Open an existing database.
    pub fn open(storage: S, opts: &OpenOptions) -> Result<Db<S>> {
        let meta = {
            let txn = storage.begin(false)?;
            let g = txn.get(&META_KEY)?.ok_or(Error::NotFound)?;
            Metadata::decode(&g)?
        };
        let clock = meta.clock;
        let db = Db { storage, meta, clock: CommittedClock::new(clock) };
        if opts.verify_on_open && meta.syncable {
            db.verify()?;
        }
        Ok(db)
    }

    pub fn open_or_create(storage: S, opts: &CreateOptions, open: &OpenOptions) -> Result<Db<S>> {
        let exists = {
            let txn = storage.begin(false)?;
            txn.get(&META_KEY)?.is_some()
        };
        if exists { Db::open(storage, open) } else { Db::create(storage, opts) }
    }

    /// Verify one consistent committed snapshot without mutating storage.
    /// Plain databases have no Merkle structure and return `Error::Mode`.
    pub fn verify(&self) -> Result<()> {
        if !self.meta.syncable {
            return Err(Error::Mode);
        }
        let mut txn = self.storage.begin(false)?;
        Tree { txn: &mut txn, hash_key: self.meta.hash_key, branch_factor: self.meta.branch_factor }
            .verify()
    }

    pub fn meta(&self) -> &Metadata {
        &self.meta
    }

    /// Clock of the most recent committed write transaction.
    pub fn clock(&self) -> Clock {
        self.clock.get()
    }

    pub fn props(&self) -> StorageProps {
        self.storage.props()
    }

    /// Release the database, returning the underlying storage.
    pub fn close(self) -> S {
        self.storage
    }

    /// Begin a read snapshot. Multiple may exist concurrently.
    pub fn begin_read(&self) -> Result<ReadTxn<'_, S>> {
        Ok(ReadTxn { inner: TxnInner { db: self, txn: self.storage.begin(false)? } })
    }

    /// Begin the (single) write transaction. `wall_unix_secs` is the current
    /// wall-clock time in seconds since the Unix epoch, used to advance the
    /// database's hybrid logical clock; an unset or wrong host clock is
    /// tolerated (the HLC only ever moves forward).
    pub fn begin_write(&self, wall_unix_secs: u64) -> Result<WriteTxn<'_, S>> {
        let txn = self.storage.begin(true)?;
        let (committed_clock, clock) = if self.meta.syncable {
            // The exclusive storage writer sees exactly the last committed
            // metadata, so the committed clock is read from there. The
            // in-memory copy is published only after a commit returns and can
            // still lag behind it at this point.
            let committed = {
                let stored = txn.get(&META_KEY)?.ok_or(Error::Corrupt)?;
                Metadata::decode(&stored)?.clock
            };
            let next = committed.incr(wall_unix_secs)?;
            assert!(next > committed, "a writer advances the committed replica clock");
            // Never commit the terminal clock: a subsequent writer must have
            // a representable successor, including after a remote merge.
            next.incr(0)?;
            (committed, next)
        } else {
            (Clock::ZERO, Clock::ZERO)
        };
        Ok(WriteTxn { inner: TxnInner { db: self, txn }, clock, committed_clock, delta: None })
    }

    /// Begin a write that can publish a bounded delta after commit. The cap
    /// includes wire headers. Oversized transactions still commit, returning
    /// `CommittedDelta::PullRequired`; ordinary writes do not capture deltas.
    pub fn begin_write_with_delta(
        &self,
        wall_unix_secs: u64,
        max_bytes: usize,
    ) -> Result<WriteTxn<'_, S>> {
        if !self.meta.syncable {
            return Err(Error::Mode);
        }
        let mut txn = self.begin_write(wall_unix_secs)?;
        txn.delta = Some(crate::sync::DeltaCapture::new(max_bytes));
        Ok(txn)
    }
}

/// A borrowed key or value, valid while its transaction is (and, in a write
/// transaction, until the next mutation).
pub struct Bytes<G: Deref<Target = [u8]>> {
    guard: G,
    start: usize,
}

impl<G: Deref<Target = [u8]>> Deref for Bytes<G> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.guard[self.start..]
    }
}

impl<G: Deref<Target = [u8]>> AsRef<[u8]> for Bytes<G> {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl<G: Deref<Target = [u8]>> core::fmt::Debug for Bytes<G> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Bytes({} bytes)", self.len())
    }
}

/// Borrowed bytes tied to a transaction: shorthand for the backend guard.
pub type ValueRef<'t, 'db, S> = Bytes<<<S as Storage>::Txn<'db> as StorageTxn>::Guard<'t>>;

struct TxnInner<'db, S: Storage> {
    db: &'db Db<S>,
    txn: S::Txn<'db>,
}

impl<'db, S: Storage> TxnInner<'db, S> {
    fn visit<E: From<Error>>(
        &self,
        start: &[u8],
        exclusive: bool,
        mut visitor: impl FnMut(&[u8], &dyn crate::storage::ValueRead) -> core::result::Result<bool, E>,
    ) -> core::result::Result<(), E> {
        if start.len() > KEY_SIZE_MAX {
            return Err(Error::KeySize.into());
        }
        let mut bound = [0; KEY_SIZE_MAX + 1];
        bound[1..1 + start.len()].copy_from_slice(start);
        let mut callback_error = None;
        self.txn
            .visit(&bound[..1 + start.len()], exclusive, false, &mut |key, raw| {
                if key.first() != Some(&0) {
                    return Ok(false);
                }
                TxnInner::<S>::check_key(&key[1..])?;
                let mut start = 0;
                if self.db.meta.syncable {
                    let mut header = [0; encoding::ENT_HEADER_FULL];
                    let n = raw.len().min(header.len());
                    raw.read_exact_at(0, &mut header[..n])?;
                    let ent = EntVal::decode(&header[..n])?;
                    if ent.is_tombstone() {
                        return Ok(true);
                    }
                    start = if raw.len() >= encoding::ENT_HEADER_FULL {
                        encoding::ENT_HEADER_FULL
                    } else {
                        encoding::ENT_HEADER_COMPACT
                    };
                }
                match visitor(&key[1..], &UserValue { raw, start }) {
                    Ok(proceed) => Ok(proceed),
                    Err(error) => {
                        callback_error = Some(error);
                        Ok(false)
                    }
                }
            })
            .map_err(E::from)?;
        match callback_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn check_key(key: &[u8]) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidKey);
        }
        if key.len() > KEY_SIZE_MAX {
            return Err(Error::KeySize);
        }
        Ok(())
    }

    fn skey(key: &[u8]) -> Vec<u8> {
        let mut skey = Vec::with_capacity(1 + key.len());
        skey.push(0);
        skey.extend_from_slice(key);
        skey
    }

    fn get<'t>(&'t self, key: &[u8]) -> Result<Option<ValueRef<'t, 'db, S>>> {
        Self::check_key(key)?;
        let skey = Self::skey(key);
        let Some(guard) = self.txn.get(&skey)? else {
            return Ok(None);
        };
        if !self.db.meta.syncable {
            return Ok(Some(Bytes { guard, start: 0 }));
        }
        let (tomb, start) = {
            let ent = EntVal::decode(&guard)?;
            (ent.is_tombstone(), encoding::header_size(&guard))
        };
        if tomb {
            return Ok(None);
        }
        Ok(Some(Bytes { guard, start }))
    }

    fn get_with_clock<'t>(&'t self, key: &[u8]) -> Result<Option<(Clock, ValueRef<'t, 'db, S>)>> {
        if !self.db.meta.syncable {
            return Err(Error::Mode);
        }
        Self::check_key(key)?;
        let Some(guard) = self.txn.get(&Self::skey(key))? else {
            return Ok(None);
        };
        let (clock, tombstone, start) = {
            let entry = EntVal::decode(&guard)?;
            (entry.clock, entry.is_tombstone(), encoding::header_size(&guard))
        };
        Ok((!tombstone).then_some((clock, Bytes { guard, start })))
    }

    /// If `key` currently holds a tombstone, its deletion clock.
    fn is_del(&self, key: &[u8]) -> Result<Option<Clock>> {
        if !self.db.meta.syncable {
            return Err(Error::Mode);
        }
        Self::check_key(key)?;
        match self.txn.get(&Self::skey(key))? {
            Some(g) => {
                let ent = EntVal::decode(&g)?;
                Ok(ent.is_tombstone().then_some(ent.clock))
            }
            None => Ok(None),
        }
    }

    fn root_hash(&self) -> Result<Option<(u8, [u8; 32])>> {
        if !self.db.meta.syncable {
            return Err(Error::Mode);
        }
        tree::root(&self.txn)
    }

    fn iter_from<'t>(&'t self, start: &[u8]) -> Iter<'t, S::Txn<'db>> {
        let from = Self::skey(start);
        Iter {
            txn: &self.txn,
            syncable: self.db.meta.syncable,
            keyspace: 0,
            from,
            excl: false,
            done: false,
        }
    }

    fn local_get<'t>(&'t self, key: &[u8]) -> Result<Option<ValueRef<'t, 'db, S>>> {
        let stored = encoding::local_key(key, false)?;
        if self.txn.value_len(&stored)?.is_some_and(|len| len > crate::VALUE_SIZE_MAX) {
            return Err(Error::Corrupt);
        }
        Ok(self.txn.get(&stored)?.map(|guard| Bytes { guard, start: 0 }))
    }

    fn next_source_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        if key.len() > KEY_SIZE_MAX {
            return Err(Error::KeySize);
        }
        let Some(next) = self.txn.next_key(&Self::skey(key), exclusive)? else {
            return Ok(None);
        };
        if next.first() != Some(&0) {
            return Ok(None);
        }
        Self::check_key(&next[1..])?;
        Ok(Some(next[1..].to_vec()))
    }

    fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        let bound = encoding::local_key(key, true)?;
        let Some(next) = self.txn.next_key(&bound, exclusive)? else { return Ok(None) };
        if next.first() != Some(&encoding::LOCAL_KEY_BYTE) {
            return Ok(None);
        }
        Self::check_key(&next[1..])?;
        Ok(Some(next[1..].to_vec()))
    }

    fn local_iter_from<'t>(&'t self, start: &[u8]) -> Result<Iter<'t, S::Txn<'db>>> {
        let from = encoding::local_key(start, true)?;
        Ok(Iter {
            txn: &self.txn,
            syncable: false,
            keyspace: encoding::LOCAL_KEY_BYTE,
            from,
            excl: false,
            done: false,
        })
    }
}

/// Forward iterator over live entries in key order (tombstones skipped).
pub struct Iter<'t, T: StorageTxn> {
    txn: &'t T,
    syncable: bool,
    keyspace: u8,
    from: Vec<u8>,
    excl: bool,
    done: bool,
}

impl<'t, T: StorageTxn> Iterator for Iter<'t, T> {
    type Item = Result<(Bytes<T::Guard<'t>>, Bytes<T::Guard<'t>>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let txn: &'t T = self.txn;
            let (k, v) = match txn.next_from(&self.from, self.excl) {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Ok(Some(kv)) => kv,
            };
            if k.first() != Some(&self.keyspace) {
                self.done = true;
                return None;
            }
            if self.keyspace == encoding::LOCAL_KEY_BYTE
                && (k.len() < 2
                    || k.len() > crate::KEY_SIZE_MAX + 1
                    || v.len() > crate::VALUE_SIZE_MAX)
            {
                self.done = true;
                return Some(Err(Error::Corrupt));
            }
            self.from.clear();
            self.from.extend_from_slice(&k);
            self.excl = true;
            if self.syncable {
                let (tomb, start) = match EntVal::decode(&v) {
                    Ok(ent) => (ent.is_tombstone(), encoding::header_size(&v)),
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                };
                if tomb {
                    continue;
                }
                return Some(Ok((Bytes { guard: k, start: 1 }, Bytes { guard: v, start })));
            }
            return Some(Ok((Bytes { guard: k, start: 1 }, Bytes { guard: v, start: 0 })));
        }
    }
}

/// Owned live-value reader retaining its backend read snapshot/page.
pub struct ValueReader<V: crate::storage::ValueRead> {
    raw: V,
    start: usize,
    clock: Option<Clock>,
}
impl<V: crate::storage::ValueRead> ValueReader<V> {
    /// Origin clock captured from the same immutable value as this reader.
    /// Plain databases have no per-entry replication clock.
    pub fn clock(&self) -> Option<Clock> {
        self.clock
    }
}
impl<V: crate::storage::ValueRead> crate::storage::ValueRead for ValueReader<V> {
    fn len(&self) -> usize {
        self.raw.len() - self.start
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        if offset > self.len() {
            return Err(Error::InvalidArgument);
        }
        self.raw.read_at(self.start + offset, out)
    }
}

/// A read snapshot.
pub struct ReadTxn<'db, S: Storage> {
    inner: TxnInner<'db, S>,
}

impl<'db, S: Storage> ReadTxn<'db, S> {
    /// Committed clock from this exact pinned backend snapshot. The database's
    /// latest in-memory clock may already describe a newer transaction.
    pub fn clock(&self) -> Result<Clock> {
        let stored = self.inner.txn.get(&META_KEY)?.ok_or(Error::Corrupt)?;
        Ok(Metadata::decode(&stored)?.clock)
    }

    /// Read a persistent replica-local secondary record from this snapshot.
    /// These records are disjoint from ordinary keys and absent from replication.
    pub fn local_get<'t>(&'t self, key: &[u8]) -> Result<Option<ValueRef<'t, 'db, S>>> {
        self.inner.local_get(key)
    }

    /// Seek replica-local secondary records without scanning source entries.
    /// An empty bound starts at the first local record; values have no HLC header.
    pub fn local_iter_from(&self, start: &[u8]) -> Result<Iter<'_, S::Txn<'db>>> {
        self.inner.local_iter_from(start)
    }

    /// Seek local metadata keys without reading their values.
    pub fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_local_key(key, exclusive)
    }

    /// Raw storage transaction (crate-internal; used by sync).
    pub(crate) fn raw(&self) -> &S::Txn<'db> {
        &self.inner.txn
    }

    pub fn get<'t>(&'t self, key: &[u8]) -> Result<Option<ValueRef<'t, 'db, S>>> {
        self.inner.get(key)
    }

    /// One live value and its originating entry clock from this transaction.
    /// Missing/deleted keys return None; plain databases return Error::Mode.
    /// The bytes borrow the same backend guard that supplied the clock.
    pub fn get_with_clock<'t>(
        &'t self,
        key: &[u8],
    ) -> Result<Option<(Clock, ValueRef<'t, 'db, S>)>> {
        self.inner.get_with_clock(key)
    }

    /// Capture one already-committed entry for replay, including a tombstone.
    /// This does not write, advance clocks or scan other keys. None means the
    /// key has no stored entry; oversized entries return PullRequired.
    pub fn delta_for_key(
        &self,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<crate::sync::CommittedDelta>> {
        if !self.inner.db.meta.syncable {
            return Err(Error::Mode);
        }
        TxnInner::<S>::check_key(key)?;
        let storage_key = TxnInner::<S>::skey(key);
        let Some(len) = self.inner.txn.value_len(&storage_key)? else {
            return Ok(None);
        };
        let mut capture = crate::sync::DeltaCapture::new(max_bytes);
        if capture.admits(key, len) {
            let stored = self.inner.txn.get(&storage_key)?.ok_or(Error::Corrupt)?;
            capture.record(key, &stored);
        }
        let metadata = self.inner.txn.get(&META_KEY)?.ok_or(Error::Corrupt)?;
        let metadata = Metadata::decode(&metadata)?;
        Ok(Some(capture.encode(&metadata, metadata.clock)))
    }

    /// Pin one live value in this snapshot. Repeated bounded reads reuse the
    /// backend's actual page; drop the reader before advancing to another key.
    pub fn value_reader(
        &self,
        key: &[u8],
    ) -> Result<Option<ValueReader<<S::Txn<'db> as StorageTxn>::ValueReader>>> {
        use crate::storage::ValueRead;
        TxnInner::<S>::check_key(key)?;
        let Some(raw) = self.raw().snapshot_value(&TxnInner::<S>::skey(key))? else {
            return Ok(None);
        };
        let mut start = 0;
        let mut clock = None;
        if self.inner.db.meta.syncable {
            let mut header = [0; encoding::ENT_HEADER_FULL];
            let n = raw.len().min(header.len());
            raw.read_exact_at(0, &mut header[..n])?;
            let ent = EntVal::decode(&header[..n])?;
            if ent.is_tombstone() {
                return Ok(None);
            }
            clock = Some(ent.clock);
            start = if raw.len() >= encoding::ENT_HEADER_FULL {
                encoding::ENT_HEADER_FULL
            } else {
                encoding::ENT_HEADER_COMPACT
            };
        }
        Ok(Some(ValueReader { raw, start, clock }))
    }

    /// Length of the live user value, without copying its bytes.
    pub fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
        Ok(self.value_range(key)?.map(|(_, len)| len))
    }

    /// Read a bounded slice of the live user value from this snapshot.
    pub fn read_value_at(
        &self,
        key: &[u8],
        offset: usize,
        out: &mut [u8],
    ) -> Result<Option<usize>> {
        let Some((start, len)) = self.value_range(key)? else {
            return Ok(None);
        };
        if offset > len {
            return Err(Error::InvalidArgument);
        }
        let skey = TxnInner::<S>::skey(key);
        self.raw().read_value_at(&skey, start + offset, out)
    }

    /// First source key at or after `key`, including tombstones. This only
    /// copies the bounded key; it never reads or materializes its value.
    /// Replica-local secondary keys remain outside this namespace.
    pub fn next_source_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_source_key(key, exclusive)
    }

    /// First live user key at or after `key` (strictly after if exclusive).
    /// The empty key is permitted as the beginning-of-keyspace sentinel.
    pub fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        if key.len() > KEY_SIZE_MAX {
            return Err(Error::KeySize);
        }
        let mut cursor = TxnInner::<S>::skey(key);
        let mut excl = exclusive;
        while let Some(next) = self.raw().next_key(&cursor, excl)? {
            if next.first() != Some(&0) {
                return Ok(None);
            }
            if self.value_len(&next[1..])?.is_some() {
                return Ok(Some(next[1..].to_vec()));
            }
            cursor = next;
            excl = true;
        }
        Ok(None)
    }

    /// Visit live user entries at or after `start` (strictly after when
    /// exclusive). Empty `start` is the beginning-of-keyspace sentinel.
    /// Keys and bounded value readers are borrowed only during the callback.
    /// True continues; false or an error stops immediately. Both database
    /// modes are supported, with syncable headers hidden and tombstones skipped.
    pub fn visit<E: From<Error>>(
        &self,
        start: &[u8],
        exclusive: bool,
        visitor: impl FnMut(&[u8], &dyn crate::storage::ValueRead) -> core::result::Result<bool, E>,
    ) -> core::result::Result<(), E> {
        self.inner.visit(start, exclusive, visitor)
    }

    fn value_range(&self, key: &[u8]) -> Result<Option<(usize, usize)>> {
        TxnInner::<S>::check_key(key)?;
        let skey = TxnInner::<S>::skey(key);
        let Some(len) = self.raw().value_len(&skey)? else {
            return Ok(None);
        };
        if !self.inner.db.meta.syncable {
            return Ok(Some((0, len)));
        }
        let n = len.min(encoding::ENT_HEADER_FULL);
        let mut header = [0; encoding::ENT_HEADER_FULL];
        if self.raw().read_value_at(&skey, 0, &mut header[..n])? != Some(n) {
            return Err(Error::Corrupt);
        }
        let ent = EntVal::decode(&header[..n])?;
        if ent.is_tombstone() {
            return Ok(None);
        }
        let start = if len >= encoding::ENT_HEADER_FULL {
            encoding::ENT_HEADER_FULL
        } else {
            encoding::ENT_HEADER_COMPACT
        };
        Ok(Some((start, len - start)))
    }

    pub fn has(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// If `key` currently holds a tombstone, its deletion clock
    /// (syncable databases only).
    pub fn is_del(&self, key: &[u8]) -> Result<Option<Clock>> {
        self.inner.is_del(key)
    }

    /// The Merkle root `(level, hash)`; None for an empty database
    /// (syncable databases only).
    pub fn root_hash(&self) -> Result<Option<(u8, [u8; 32])>> {
        self.inner.root_hash()
    }

    pub fn iter(&self) -> Iter<'_, S::Txn<'db>> {
        self.inner.iter_from(&[])
    }

    /// Iterate live entries with key `>= start`.
    pub fn iter_from(&self, start: &[u8]) -> Iter<'_, S::Txn<'db>> {
        self.inner.iter_from(start)
    }
}

struct UserValue<'a> {
    raw: &'a dyn crate::storage::ValueRead,
    start: usize,
}
impl crate::storage::ValueRead for UserValue<'_> {
    fn len(&self) -> usize {
        self.raw.len() - self.start
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        if offset > self.len() {
            return Err(Error::InvalidArgument);
        }
        self.raw.read_at(self.start + offset, out)
    }
}

/// The write transaction: reads its own writes; commit is atomic.
pub struct WriteTxn<'db, S: Storage> {
    inner: TxnInner<'db, S>,
    clock: Clock,
    /// Immutable clock read under the backend's exclusive writer admission.
    /// Checking against it at commit needs no additional storage access.
    committed_clock: Clock,
    delta: Option<crate::sync::DeltaCapture>,
}

impl<'db, S: Storage> WriteTxn<'db, S> {
    /// Read replica-local metadata from the same pending transaction as sources.
    pub fn local_get<'t>(&'t self, key: &[u8]) -> Result<Option<ValueRef<'t, 'db, S>>> {
        self.inner.local_get(key)
    }

    /// Seek local metadata keys without reading their values.
    pub fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_local_key(key, exclusive)
    }

    /// First pending source key, including tombstones, without copying a value.
    pub fn next_source_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_source_key(key, exclusive)
    }

    /// Borrow one pending live value and its source clock. The callback runs
    /// at most once, never for a missing key or tombstone, and may reject the
    /// length before reading bytes. No value-sized copy precedes the callback.
    pub fn visit_value<E: From<Error>>(
        &self,
        key: &[u8],
        mut visitor: impl FnMut(
            Option<Clock>,
            &dyn crate::storage::ValueRead,
        ) -> core::result::Result<(), E>,
    ) -> core::result::Result<bool, E> {
        TxnInner::<S>::check_key(key)?;
        let bound = TxnInner::<S>::skey(key);
        let mut found = false;
        let mut callback_error = None;
        self.inner
            .txn
            .visit(&bound, false, false, &mut |key, raw| {
                if key != bound {
                    return Ok(false);
                }
                let mut start = 0;
                let mut clock = None;
                if self.inner.db.meta.syncable {
                    let mut header = [0; encoding::ENT_HEADER_FULL];
                    let n = raw.len().min(header.len());
                    raw.read_exact_at(0, &mut header[..n])?;
                    let ent = EntVal::decode(&header[..n])?;
                    if ent.is_tombstone() {
                        return Ok(false);
                    }
                    clock = Some(ent.clock);
                    start = if raw.len() >= encoding::ENT_HEADER_FULL {
                        encoding::ENT_HEADER_FULL
                    } else {
                        encoding::ENT_HEADER_COMPACT
                    };
                }
                found = true;
                if let Err(error) = visitor(clock, &UserValue { raw, start }) {
                    callback_error = Some(error);
                }
                Ok(false)
            })
            .map_err(E::from)?;
        match callback_error {
            Some(error) => Err(error),
            None => Ok(found),
        }
    }

    /// Update secondary records atomically with this transaction's source writes.
    /// The view cannot mutate source records, tree hashes, or captured deltas.
    pub fn local_indexes(&mut self) -> LocalIndexes<'_, 'db, S> {
        LocalIndexes { inner: &mut self.inner }
    }

    /// Verify the effective pending tree, without committing it. Full-sync
    /// receivers call this only after flushing their deferred branch repair.
    /// This diagnostic scans the entire tree. Tests and fuzz targets opt in
    /// explicitly; ordinary commits do not call it, including in debug builds.
    pub fn verify(&mut self) -> Result<()> {
        if !self.inner.db.meta.syncable {
            return Err(Error::Mode);
        }
        self.tree().verify()
    }
    /// The transaction's HLC clock. Ordinary writes use this clock;
    /// `put_with_clock` preserves the supplied originating clock instead.
    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Committed predecessor captured under exclusive writer admission. This
    /// does not describe pending writes made through this transaction.
    pub fn committed_clock(&self) -> Clock {
        self.committed_clock
    }

    pub fn get<'t>(&'t self, key: &[u8]) -> Result<Option<ValueRef<'t, 'db, S>>> {
        self.inner.get(key)
    }

    /// One live value and its originating entry clock from this transaction.
    /// Missing/deleted keys return None; plain databases return Error::Mode.
    /// The bytes borrow the same backend guard that supplied the clock.
    pub fn get_with_clock<'t>(
        &'t self,
        key: &[u8],
    ) -> Result<Option<(Clock, ValueRef<'t, 'db, S>)>> {
        self.inner.get_with_clock(key)
    }

    /// Borrow live values from the effective pending transaction, including
    /// this transaction's writes. Same bounds and callback rules as ReadTxn::visit.
    pub fn visit<E: From<Error>>(
        &self,
        start: &[u8],
        exclusive: bool,
        visitor: impl FnMut(&[u8], &dyn crate::storage::ValueRead) -> core::result::Result<bool, E>,
    ) -> core::result::Result<(), E> {
        self.inner.visit(start, exclusive, visitor)
    }

    pub fn has(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    pub fn is_del(&self, key: &[u8]) -> Result<Option<Clock>> {
        self.inner.is_del(key)
    }

    pub fn root_hash(&self) -> Result<Option<(u8, [u8; 32])>> {
        self.inner.root_hash()
    }

    pub fn iter(&self) -> Iter<'_, S::Txn<'db>> {
        self.inner.iter_from(&[])
    }

    pub fn iter_from(&self, start: &[u8]) -> Iter<'_, S::Txn<'db>> {
        self.inner.iter_from(start)
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        TxnInner::<S>::check_key(key)?;
        if value.len() > crate::VALUE_SIZE_MAX {
            return Err(Error::ValueSize);
        }
        if self.inner.db.meta.syncable {
            let clock = self.clock;
            self.tree().put_leaf(key, value, 0, clock)?;
            self.capture_delta(key)
        } else {
            self.inner.txn.put(&TxnInner::<S>::skey(key), value)
        }
    }

    /// Stream a stable source into final storage with the same semantics as
    /// `put`. A source/storage error requires dropping this transaction.
    pub fn put_from(&mut self, key: &[u8], value: &dyn crate::storage::ValueRead) -> Result<()> {
        TxnInner::<S>::check_key(key)?;
        if value.len() > crate::VALUE_SIZE_MAX {
            return Err(Error::ValueSize);
        }
        if self.inner.db.meta.syncable {
            let clock = self.clock;
            self.tree().put_leaf_from(key, value, 0, clock)?;
            self.capture_delta(key)
        } else {
            self.inner.txn.put_from(&TxnInner::<S>::skey(key), value)
        }
    }

    /// Insert already validated state while preserving its originating clock.
    /// Requires a syncable database and a nonzero clock with enough remaining
    /// range for this receive event and the next local write.
    ///
    /// Like `put`, this overwrites the entry regardless of its previous clock:
    /// the caller must already have authenticated and resolved the imported
    /// state. Use sync receivers when conflict resolution is still required.
    /// Identical value/clock pairs leave the entry unchanged. Delta capture
    /// includes changed entries with their supplied clocks, only after commit.
    /// Invalid arguments and clock exhaustion leave this transaction unchanged.
    pub fn put_with_clock(&mut self, key: &[u8], value: &[u8], origin: Clock) -> Result<()> {
        if !self.inner.db.meta.syncable {
            return Err(Error::Mode);
        }
        TxnInner::<S>::check_key(key)?;
        if value.len() > crate::VALUE_SIZE_MAX {
            return Err(Error::ValueSize);
        }
        if origin.is_zero() {
            return Err(Error::InvalidArgument);
        }
        let next = if origin >= self.clock { self.clock.recv(origin, 0)? } else { self.clock };
        // begin_write also requires a successor for its own transaction clock.
        // Reserve both increments so one subsequent local write can commit.
        next.incr(0)?.incr(0)?;
        self.sync_put(key, value, 0, origin)?;
        self.clock = next;
        Ok(())
    }

    /// Stream validated state with its originating clock into a fresh backing.
    /// The caller resolves admission before copying and drops this transaction
    /// on a source/storage error. Scratch is independent of the value length.
    pub fn put_from_with_clock(
        &mut self,
        key: &[u8],
        value: &dyn crate::storage::ValueRead,
        origin: Clock,
    ) -> Result<()> {
        if !self.inner.db.meta.syncable {
            return Err(Error::Mode);
        }
        TxnInner::<S>::check_key(key)?;
        if value.len() > crate::VALUE_SIZE_MAX {
            return Err(Error::ValueSize);
        }
        if origin.is_zero() {
            return Err(Error::InvalidArgument);
        }
        let next = if origin >= self.clock { self.clock.recv(origin, 0)? } else { self.clock };
        next.incr(0)?.incr(0)?;
        self.tree().put_leaf_from(key, value, 0, origin)?;
        self.capture_delta(key)?;
        self.clock = next;
        Ok(())
    }

    /// Delete `key`. In a syncable database this writes a tombstone so the
    /// deletion propagates to other replicas. Returns whether a live entry
    /// was present.
    pub fn del(&mut self, key: &[u8]) -> Result<bool> {
        TxnInner::<S>::check_key(key)?;
        if !self.inner.db.meta.syncable {
            return self.inner.txn.del(&TxnInner::<S>::skey(key));
        }
        let clock = self.clock;
        let mut tree = self.tree();
        let live = match tree.get_leaf_raw(key)? {
            None => false,
            Some(v) => !EntVal::decode(&v)?.is_tombstone(),
        };
        if !live {
            return Ok(false);
        }
        tree.put_leaf(key, &[], ENT_FLAG_TOMBSTONE, clock)?;
        self.capture_delta(key)?;
        Ok(true)
    }

    /// Delete `key` leaving no trace ("forget"): other replicas will see the
    /// key as missing rather than deleted, which can resurrect it during
    /// sync. Use [`WriteTxn::del`] unless local erasure is the goal.
    pub fn del_forget(&mut self, key: &[u8]) -> Result<bool> {
        TxnInner::<S>::check_key(key)?;
        if self.inner.db.meta.syncable {
            let changed = self.tree().remove_leaf(key)?;
            if changed && let Some(delta) = &mut self.delta {
                delta.require_pull();
            }
            Ok(changed)
        } else {
            self.inner.txn.del(&TxnInner::<S>::skey(key))
        }
    }

    /// Re-derive the whole tree from the leaves (syncable databases only).
    /// A maintenance/repair operation; normal writes keep the tree correct
    /// incrementally.
    pub fn rebuild_tree(&mut self) -> Result<()> {
        if !self.inner.db.meta.syncable {
            return Err(Error::Mode);
        }
        self.tree().build_all()
    }

    pub fn commit(self) -> Result<()> {
        // Integrity is maintained by immediate branch repair (or a receiver's
        // bounded repair flush), paired assertions and the exclusive writer.
        // Diagnostics call verify explicitly before commit; scanning the whole
        // tree here makes growing sequences of debug commits quadratic work.
        let WriteTxn { inner, clock, committed_clock, .. } = self;
        let TxnInner { db, mut txn } = inner;
        if db.meta.syncable {
            assert!(clock > committed_clock, "the exclusive writer commits a newer replica clock");
            let mut meta = db.meta;
            meta.clock = clock;
            txn.put(&META_KEY, &meta.encode())?;
        }
        txn.commit()?;
        if db.meta.syncable {
            // Published only once the storage commit succeeded: `clock()` is
            // the latest committed clock, never one a reader could observe
            // from a transaction that is still open or that failed to commit.
            // The next writer reads the committed clock from storage, so the
            // gap between the commit and this publication cannot make it
            // start stale.
            db.clock.raise(clock);
        }
        Ok(())
    }

    /// Commit, then return bytes safe to publish. No bytes escape if commit
    /// fails. Requires `begin_write_with_delta`. A forgotten key cannot be
    /// represented by a delta and also returns `PullRequired`.
    pub fn commit_with_delta(mut self) -> Result<crate::sync::CommittedDelta> {
        let capture = self.delta.take().ok_or(Error::Mode)?;
        let delta = capture.encode(self.inner.db.meta(), self.clock);
        self.commit()?;
        Ok(delta)
    }

    fn capture_delta(&mut self, key: &[u8]) -> Result<()> {
        if let Some(delta) = &mut self.delta {
            let len = self.inner.txn.value_len(&TxnInner::<S>::skey(key))?.ok_or(Error::Corrupt)?;
            if !delta.admits(key, len) {
                return Ok(());
            }
            let stored = self.inner.txn.get(&TxnInner::<S>::skey(key))?.ok_or(Error::Corrupt)?;
            delta.record(key, &stored);
        }
        Ok(())
    }

    fn tree(&mut self) -> Tree<'_, S::Txn<'db>> {
        Tree {
            txn: &mut self.inner.txn,
            hash_key: self.inner.db.meta.hash_key,
            branch_factor: self.inner.db.meta.branch_factor,
        }
    }

    // ---- crate-internal hooks for sync ----

    /// Override the transaction clock (sync merge transactions use an HLC
    /// receive-event clock derived from the remote peer).
    pub(crate) fn set_clock(&mut self, clock: Clock) -> Result<()> {
        // The next begin_write needs its transaction clock and a successor.
        // Reject before storing a receive clock that would prevent that write.
        clock.incr(0)?.incr(0)?;
        assert!(clock >= self.clock, "receive events never move the transaction clock backward");
        self.clock = clock;
        Ok(())
    }

    /// Write an entry with an explicit clock and flags, preserving the
    /// originating transaction's clock during merges.
    pub(crate) fn sync_put(
        &mut self,
        key: &[u8],
        userval: &[u8],
        flags: u8,
        clock: Clock,
    ) -> Result<bool> {
        TxnInner::<S>::check_key(key)?;
        let changed = self.tree().put_leaf(key, userval, flags, clock)?;
        if changed {
            self.capture_delta(key)?;
        }
        Ok(changed)
    }

    /// Full receiver only: store/capture a leaf and defer branch repair.
    pub(crate) fn sync_put_from_deferred(
        &mut self,
        key: &[u8],
        value: &dyn crate::storage::ValueRead,
        flags: u8,
        clock: Clock,
    ) -> Result<Option<bool>> {
        TxnInner::<S>::check_key(key)?;
        let changed = self.tree().write_leaf_from(key, value, flags, clock)?;
        if changed.is_some() {
            self.capture_delta(key)?;
        }
        Ok(changed)
    }

    /// Full receiver only: forget/capture a leaf and defer branch repair.
    pub(crate) fn sync_forget_deferred(&mut self, key: &[u8]) -> Result<bool> {
        TxnInner::<S>::check_key(key)?;
        let changed = self.tree().erase_leaf(key)?;
        if changed && let Some(delta) = &mut self.delta {
            delta.require_pull();
        }
        Ok(changed)
    }

    pub(crate) fn sync_leaf_interval(&mut self, key: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        TxnInner::<S>::check_key(key)?;
        self.tree().leaf_interval(key)
    }

    pub(crate) fn sync_repair(&mut self, lo: &[u8], hi: &[u8], stable: bool) -> Result<()> {
        self.tree().propagate(lo, hi, stable)
    }

    pub(crate) fn capture_changes(&mut self, max_bytes: usize) {
        self.delta = Some(crate::sync::DeltaCapture::new(max_bytes));
    }
}

/// Persistent derived records within one backend transaction. Source reads
/// include pending merged writes; mutations affect only the local keyspace.
/// Callers own authorization, index invariants, and bounds on repair work.
pub struct LocalIndexes<'t, 'db, S: Storage> {
    inner: &'t mut TxnInner<'db, S>,
}

impl<'db, S: Storage> LocalIndexes<'_, 'db, S> {
    /// Seek pending source keys without materializing their values.
    pub fn next_source_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_source_key(key, exclusive)
    }

    /// Seek local metadata keys without materializing their values.
    pub fn next_local_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.inner.next_local_key(key, exclusive)
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<ValueRef<'_, 'db, S>>> {
        self.inner.local_get(key)
    }

    pub fn iter_from(&self, start: &[u8]) -> Result<Iter<'_, S::Txn<'db>>> {
        self.inner.local_iter_from(start)
    }

    pub fn source_get(&self, key: &[u8]) -> Result<Option<ValueRef<'_, 'db, S>>> {
        self.inner.get(key)
    }

    pub fn source_get_with_clock(
        &self,
        key: &[u8],
    ) -> Result<Option<(Clock, ValueRef<'_, 'db, S>)>> {
        self.inner.get_with_clock(key)
    }

    pub fn source_iter_from(&self, start: &[u8]) -> Iter<'_, S::Txn<'db>> {
        self.inner.iter_from(start)
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        TxnInner::<S>::check_key(key)?;
        if value.len() > crate::VALUE_SIZE_MAX {
            return Err(Error::ValueSize);
        }
        let stored = encoding::local_key(key, false)?;
        self.inner.txn.put(&stored, value)
    }

    /// Remove a derived record without a replicated tombstone.
    pub fn del(&mut self, key: &[u8]) -> Result<bool> {
        let stored = encoding::local_key(key, false)?;
        self.inner.txn.del(&stored)
    }
}

#[cfg(test)]
mod tests;
