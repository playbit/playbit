//! Durable native storage backend for kvdb, built on [redb].
//!
//! redb provides an ordered byte-keyed map with MVCC snapshots, a single
//! writer, and atomic durable commits — a 1:1 match for kvdb's
//! [`Storage`] contract. Pure Rust, no C dependencies.
//!
//! Read transactions hold one snapshot table and serve zero-copy guards.
//! Write-transaction reads copy values out (redb write tables borrow the
//! transaction, so guards cannot be returned from a per-call table).

use crate::Error;
use crate::storage::{Retention, Storage, StorageProps, StorageTxn, ValueRead};
use ::redb::ReadableDatabase as _;
use ::redb::ReadableTable as _;
use ::redb::StorageBackend as _;
use std::ops::{Bound, Deref};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// Shared mutation fence for a native volume's attached database files.
/// Advances before backend writes, including writes that subsequently abort.
#[derive(Default)]
pub struct WriteEpoch {
    epoch: AtomicU64,
    active: AtomicUsize,
}
const MAX_EPOCH_WRITERS: usize = 1024;

fn next_write_epoch(current: u64) -> Option<u64> {
    current.checked_add(1)
}
fn next_epoch_writer(active: usize) -> Option<usize> {
    if active < MAX_EPOCH_WRITERS { Some(active + 1) } else { None }
}

impl WriteEpoch {
    pub fn current(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    /// Call before and after pinning a read; accept only equal returned epochs.
    /// A writer is active through backend commit/abort, including durable I/O.
    pub fn snapshot(&self) -> Result<u64> {
        let epoch = self.current();
        if self.active.load(Ordering::Acquire) != 0 || self.current() != epoch {
            return Err(Error::Busy);
        }
        Ok(epoch)
    }
    fn begin(&self) -> Result<()> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, next_epoch_writer)
            .map_err(|_| Error::Capacity)?;
        if self.epoch.try_update(Ordering::AcqRel, Ordering::Acquire, next_write_epoch).is_err() {
            self.finish();
            return Err(Error::Capacity);
        }
        Ok(())
    }
    fn finish(&self) {
        let active = self.active.fetch_sub(1, Ordering::AcqRel);
        assert!(active > 0 && active <= MAX_EPOCH_WRITERS, "a writer releases its epoch admission");
    }
}

/// Native file storage with an explicit fsync commit boundary. Rust's Apple
/// File::sync_data requests F_FULLFSYNC; that device-wide cache flush is not
/// the local commit contract. Reuse redb's file locking and positional I/O.
#[derive(Debug)]
pub struct FileBackend {
    inner: ::redb::backends::FileBackend,
    sync_file: std::fs::File,
    closed: AtomicBool,
}
impl FileBackend {
    pub fn new(file: std::fs::File) -> std::result::Result<Self, ::redb::DatabaseError> {
        let sync_file = file.try_clone()?;
        let inner = ::redb::backends::FileBackend::new(file)?;
        Ok(Self { inner, sync_file, closed: AtomicBool::new(false) })
    }
}
impl ::redb::StorageBackend for FileBackend {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(offset, bytes)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&self.sync_file)).map_err(Into::into)
    }
    fn close(&self) -> std::io::Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Err(error) = self.inner.close() {
            self.closed.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
}
impl Drop for FileBackend {
    fn drop(&mut self) {
        // Builder failure and explicit close both release the original inode's
        // lock, even if a duplicate descriptor outlives the database owner.
        let _ = self.close();
    }
}

const TABLE: ::redb::TableDefinition<'static, &'static [u8], &'static [u8]> =
    ::redb::TableDefinition::new("kvdb");

type Result<T, E = Error> = core::result::Result<T, E>;

/// One-entry full-sync staging in a caller-owned temporary file. The caller
/// must create it with private permissions and unlink it before transfer (or
/// otherwise arrange removal on drop). Clear resets the logical length and
/// subsequent appends overwrite the same file. Retained tail bytes are never
/// readable and disk use is bounded by the largest staged entry. No mmap or
/// retained value-sized buffer.
pub struct FileStaging {
    file: std::fs::File,
    len: usize,
    capacity: usize,
}
impl FileStaging {
    pub fn new(file: std::fs::File) -> Result<Self> {
        Self::with_capacity(file, crate::sync::MAX_FRAME)
    }
    /// Explicit finite storage limit for a multi-entry prepared-sync journal.
    pub fn with_capacity(file: std::fs::File, capacity: usize) -> Result<Self> {
        file.set_len(0).map_err(|_| Error::Storage(8))?;
        Ok(Self { file, len: 0, capacity })
    }
}
impl ValueRead for FileStaging {
    fn len(&self) -> usize {
        self.len
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        if offset > self.len {
            return Err(Error::InvalidArgument);
        }
        let n = out.len().min(self.len - offset);
        self.file.read_at(&mut out[..n], offset as u64).map_err(|_| Error::Storage(8))
    }
}
impl crate::sync::SyncStaging for FileStaging {
    fn clear(&mut self) -> Result<()> {
        self.len = 0;
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        if self.len.checked_add(bytes.len()).is_none_or(|n| n > self.capacity) {
            return Err(Error::Capacity);
        }
        self.file.write_all_at(bytes, self.len as u64).map_err(|_| Error::Storage(8))?;
        self.len += bytes.len();
        Ok(())
    }
}

pub struct RedbStorage {
    db: ::redb::Database,
    writer_active: AtomicBool,
    write_epoch: OnceLock<Arc<WriteEpoch>>,
}

impl RedbStorage {
    /// Create or open a database file.
    pub fn open(path: impl AsRef<Path>) -> Result<RedbStorage> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| Error::Storage(1))?;
        let backend = FileBackend::new(file).map_err(db_err)?;
        let db = ::redb::Database::builder().create_with_backend(backend).map_err(db_err)?;
        Ok(Self::from_database(db))
    }

    pub fn from_database(db: ::redb::Database) -> RedbStorage {
        RedbStorage { db, writer_active: AtomicBool::new(false), write_epoch: OnceLock::new() }
    }

    /// Bind before exposing the handle to volume operations. A file cannot
    /// change fences while readers or writers retain the backend.
    pub(crate) fn bind_write_epoch(&self, epoch: Arc<WriteEpoch>) -> Result<()> {
        let bound = self.write_epoch.get_or_init(|| epoch.clone());
        if Arc::ptr_eq(bound, &epoch) { Ok(()) } else { Err(Error::InvalidArgument) }
    }
}

// Arbitrary but distinct backend error codes for crate::Error::Storage.
fn db_err(_: ::redb::DatabaseError) -> Error {
    Error::Storage(1)
}
fn txn_err(_: ::redb::TransactionError) -> Error {
    Error::Storage(2)
}
fn table_err(e: &::redb::TableError) -> Error {
    match e {
        ::redb::TableError::Storage(_) => Error::Storage(3),
        _ => Error::Storage(4),
    }
}
fn store_err(_: ::redb::StorageError) -> Error {
    Error::Storage(5)
}
fn commit_err(_: ::redb::CommitError) -> Error {
    Error::Storage(6)
}

type ROTable = ::redb::ReadOnlyTable<&'static [u8], &'static [u8]>;

#[allow(clippy::large_enum_variant)]
enum Inner {
    Read {
        // Kept alive for the snapshot; the table serves all reads.
        _txn: ::redb::ReadTransaction,
        // None when the table has never been created (empty database).
        table: Option<ROTable>,
    },
    Write {
        txn: Option<::redb::WriteTransaction>,
    },
}

pub struct RedbTxn<'db> {
    storage: &'db RedbStorage,
    inner: Inner,
}

impl Storage for RedbStorage {
    type Txn<'db> = RedbTxn<'db>;

    fn begin(&self, write: bool) -> Result<RedbTxn<'_>> {
        if write {
            // redb's begin_write blocks on a mutex; enforce the kvdb
            // contract (Error::Busy) before touching it.
            if self.writer_active.swap(true, Ordering::Acquire) {
                return Err(Error::Busy);
            }
            if let Some(epoch) = self.write_epoch.get()
                && let Err(error) = epoch.begin()
            {
                self.writer_active.store(false, Ordering::Release);
                return Err(error);
            }
            match self.db.begin_write() {
                Ok(txn) => Ok(RedbTxn { storage: self, inner: Inner::Write { txn: Some(txn) } }),
                Err(e) => {
                    if let Some(epoch) = self.write_epoch.get() {
                        epoch.finish();
                    }
                    self.writer_active.store(false, Ordering::Release);
                    Err(txn_err(e))
                }
            }
        } else {
            let txn = self.db.begin_read().map_err(txn_err)?;
            let table = match txn.open_table(TABLE) {
                Ok(t) => Some(t),
                Err(::redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(table_err(&e)),
            };
            Ok(RedbTxn { storage: self, inner: Inner::Read { _txn: txn, table } })
        }
    }

    fn props(&self) -> StorageProps {
        StorageProps {
            retention: Retention::Persistent,
            durable_program_crash: true,
            // Apple fsync does not promise to drain a device's volatile cache.
            durable_power_loss: !cfg!(target_vendor = "apple"),
        }
    }
}

impl Drop for RedbTxn<'_> {
    fn drop(&mut self) {
        if let Inner::Write { txn } = &mut self.inner {
            drop(txn.take()); // abort if still pending
            if let Some(epoch) = self.storage.write_epoch.get() {
                epoch.finish();
            }
            assert!(
                self.storage.writer_active.swap(false, Ordering::Release),
                "the exclusive writer owns its admission slot"
            );
        }
    }
}

pub enum Guard<'g> {
    Access(::redb::AccessGuard<'g, &'static [u8]>),
    Owned(Vec<u8>),
}

impl Deref for Guard<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Guard::Access(g) => g.value(),
            Guard::Owned(v) => v,
        }
    }
}

/// The table of a pending write transaction. `open_table` on a write
/// transaction creates the table, so a missing table is not a possible
/// outcome; the transaction is only absent after commit consumed it.
fn pending_table(
    txn: &Option<::redb::WriteTransaction>,
) -> Result<::redb::Table<'_, &'static [u8], &'static [u8]>> {
    let txn = txn.as_ref().ok_or(Error::Storage(7))?;
    txn.open_table(TABLE).map_err(|e| table_err(&e))
}

impl RedbTxn<'_> {
    fn write_table(&mut self) -> Result<::redb::Table<'_, &'static [u8], &'static [u8]>> {
        match &self.inner {
            Inner::Write { txn } => pending_table(txn),
            Inner::Read { .. } => Err(Error::ReadOnly),
        }
    }

    /// Range step shared by next_from/prev_from, copying results out for
    /// write transactions.
    fn seek_owned(
        table: &impl ::redb::ReadableTable<&'static [u8], &'static [u8]>,
        key: &[u8],
        excl: bool,
        forward: bool,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut range = Self::range(table, key, excl, forward)?;
        let item = if forward { range.next() } else { range.next_back() };
        match item {
            None => Ok(None),
            Some(Err(e)) => Err(store_err(e)),
            Some(Ok((k, v))) => Ok(Some((k.value().to_vec(), v.value().to_vec()))),
        }
    }

    #[allow(clippy::type_complexity)]
    fn range<'t, T: ::redb::ReadableTable<&'static [u8], &'static [u8]>>(
        table: &'t T,
        key: &[u8],
        excl: bool,
        forward: bool,
    ) -> Result<::redb::Range<'t, &'static [u8], &'static [u8]>> {
        let bound = if excl { Bound::Excluded(key) } else { Bound::Included(key) };
        let r = if forward {
            table.range::<&[u8]>((bound, Bound::Unbounded))
        } else {
            table.range::<&[u8]>((Bound::Unbounded, bound))
        };
        r.map_err(store_err)
    }
}

pub struct RedbValueReader(::redb::AccessGuard<'static, &'static [u8]>);
impl ValueRead for RedbValueReader {
    fn len(&self) -> usize {
        self.0.value().len()
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        crate::storage::SliceValue(self.0.value()).read_at(offset, out)
    }
}

impl StorageTxn for RedbTxn<'_> {
    type ValueReader = RedbValueReader;
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<RedbValueReader>> {
        match &self.inner {
            Inner::Read { table: None, .. } => Ok(None),
            Inner::Read { table: Some(table), .. } => {
                // Use the inherent ReadOnlyTable::get: its 'static guard owns
                // the real page and read-snapshot lifetime, without copying.
                Ok(table.get(key).map_err(store_err)?.map(RedbValueReader))
            }
            Inner::Write { .. } => Err(Error::Mode),
        }
    }
    type Guard<'g>
        = Guard<'g>
    where
        Self: 'g;

    fn get<'g>(&'g self, key: &[u8]) -> Result<Option<Guard<'g>>> {
        match &self.inner {
            Inner::Read { table: None, .. } => Ok(None),
            Inner::Read { table: Some(t), .. } => {
                Ok(t.get(key).map_err(store_err)?.map(Guard::Access))
            }
            Inner::Write { txn } => Ok(pending_table(txn)?
                .get(key)
                .map_err(store_err)?
                .map(|g| Guard::Owned(g.value().to_vec()))),
        }
    }

    fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
        self.with_value(key, |value| Ok(value.len()))
    }
    fn read_value_at(&self, key: &[u8], offset: usize, out: &mut [u8]) -> Result<Option<usize>> {
        self.with_value(key, |value| {
            let tail = value.get(offset..).ok_or(Error::InvalidArgument)?;
            let n = tail.len().min(out.len());
            out[..n].copy_from_slice(&tail[..n]);
            Ok(n)
        })
    }
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.seek_key(key, exclusive, true)
    }
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
        self.seek_key(key, exclusive, false)
    }
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut crate::storage::ValueVisitor<'_>,
    ) -> Result<()> {
        fn visit_table<T: ::redb::ReadableTable<&'static [u8], &'static [u8]>>(
            table: &T,
            key: &[u8],
            exclusive: bool,
            reverse: bool,
            visitor: &mut crate::storage::ValueVisitor<'_>,
        ) -> Result<()> {
            let mut range = RedbTxn::range(table, key, exclusive, !reverse)?;
            loop {
                let entry = if reverse { range.next_back() } else { range.next() };
                let Some(entry) = entry else {
                    return Ok(());
                };
                let (key, value) = entry.map_err(store_err)?;
                if !visitor(key.value(), &crate::storage::SliceValue(value.value()))? {
                    return Ok(());
                }
            }
        }
        match &self.inner {
            Inner::Read { table: None, .. } => Ok(()),
            Inner::Read { table: Some(table), .. } => {
                visit_table(table, key, exclusive, reverse, visitor)
            }
            Inner::Write { txn } => {
                visit_table(&pending_table(txn)?, key, exclusive, reverse, visitor)
            }
        }
    }
    fn put_from(&mut self, key: &[u8], value: &dyn ValueRead) -> Result<()> {
        let mut table = self.write_table()?;
        {
            // redb 3 requires this guard to be dropped before transaction
            // commit. This lexical scope guarantees that, including errors.
            // The reservation is final backend-owned pages, counted in RSS.
            #[allow(deprecated)]
            let mut reserved = table.insert_reserve(key, value.len()).map_err(store_err)?;
            for (i, chunk) in reserved.as_mut().chunks_mut(4096).enumerate() {
                value.read_exact_at(i * 4096, chunk)?;
            }
        }
        Ok(())
    }

    fn next_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(Guard<'g>, Guard<'g>)>> {
        self.seek(key, excl, true)
    }

    fn prev_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(Guard<'g>, Guard<'g>)>> {
        self.seek(key, excl, false)
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        let mut table = self.write_table()?;
        table.insert(key, val).map_err(store_err)?;
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<bool> {
        let mut table = self.write_table()?;
        Ok(table.remove(key).map_err(store_err)?.is_some())
    }

    fn commit(mut self) -> Result<()> {
        match &mut self.inner {
            Inner::Write { txn } => {
                let txn = txn.take().ok_or(Error::Storage(7))?;
                // Ensure the table exists even if the transaction wrote
                // nothing, so later reads see a consistent (empty) table.
                txn.open_table(TABLE).map_err(|e| table_err(&e))?;
                txn.commit().map_err(commit_err)
                // Drop releases writer_active.
            }
            Inner::Read { .. } => Err(Error::ReadOnly),
        }
    }
}

impl RedbTxn<'_> {
    fn with_value<T>(&self, key: &[u8], f: impl FnOnce(&[u8]) -> Result<T>) -> Result<Option<T>> {
        match &self.inner {
            Inner::Read { table: None, .. } => Ok(None),
            Inner::Read { table: Some(table), .. } => {
                table.get(key).map_err(store_err)?.map(|v| f(v.value())).transpose()
            }
            Inner::Write { txn } => {
                pending_table(txn)?.get(key).map_err(store_err)?.map(|v| f(v.value())).transpose()
            }
        }
    }
    fn seek_key(&self, key: &[u8], exclusive: bool, forward: bool) -> Result<Option<Vec<u8>>> {
        fn seek<T: ::redb::ReadableTable<&'static [u8], &'static [u8]>>(
            table: &T,
            key: &[u8],
            exclusive: bool,
            forward: bool,
        ) -> Result<Option<Vec<u8>>> {
            let mut range = RedbTxn::range(table, key, exclusive, forward)?;
            let item = if forward { range.next() } else { range.next_back() };
            item.map(|entry| entry.map(|(k, _)| k.value().to_vec()).map_err(store_err)).transpose()
        }
        match &self.inner {
            Inner::Read { table: None, .. } => Ok(None),
            Inner::Read { table: Some(table), .. } => seek(table, key, exclusive, forward),
            Inner::Write { txn } => seek(&pending_table(txn)?, key, exclusive, forward),
        }
    }

    fn seek<'g>(
        &'g self,
        key: &[u8],
        excl: bool,
        forward: bool,
    ) -> Result<Option<(Guard<'g>, Guard<'g>)>> {
        match &self.inner {
            Inner::Read { table: None, .. } => Ok(None),
            Inner::Read { table: Some(t), .. } => {
                let mut range = Self::range(t, key, excl, forward)?;
                let item = if forward { range.next() } else { range.next_back() };
                match item {
                    None => Ok(None),
                    Some(Err(e)) => Err(store_err(e)),
                    Some(Ok((k, v))) => Ok(Some((Guard::Access(k), Guard::Access(v)))),
                }
            }
            Inner::Write { txn } => Ok(Self::seek_owned(&pending_table(txn)?, key, excl, forward)?
                .map(|(k, v)| (Guard::Owned(k), Guard::Owned(v)))),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[allow(unsafe_code)]
mod prepared_bench;
