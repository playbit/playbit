//! The storage seam: an ordered byte-keyed map with snapshot transactions.
//!
//! kvdb's tree, transaction, and sync layers are generic over this trait.
//! Backends decide durability and platform integration: an in-memory map
//! (provided here as [`mem::MemStorage`]), redb or LMDB on native targets,
//! OPFS on the web, a flash-friendly store on embedded.
//!
//! ## Contract
//!
//! - Keys are byte strings ordered by unsigned bytewise lexicographic order.
//! - [`Storage::begin`] with `write = false` returns a snapshot: it observes
//!   one consistent committed state, unaffected by later writers.
//! - `write = true` returns the single writer (a second concurrent write
//!   transaction fails with [`Error::Busy`]); it reads its own writes, and
//!   [`StorageTxn::commit`] makes all its changes visible atomically.
//!   Dropping a transaction without committing discards its changes.
//! - Read guards borrow from the transaction. Mutating methods take
//!   `&mut self`, so the borrow checker enforces that no guard outlives a
//!   write — backends may invalidate read buffers on mutation.

use crate::error::{Error, Result};
use alloc::vec::Vec;
use core::ops::Deref;

/// Storage retention class, mirroring Playbit Core v1 store properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Cleared when the program session ends.
    Ephemeral,
    /// Normally survives restarts, but the host may evict it.
    BestEffort,
    /// Not automatically evicted by normal host policy.
    Persistent,
}

/// Properties a backend reports about itself; the runtime surfaces these
/// through Core's storage-property introspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageProps {
    pub retention: Retention,
    /// A committed transaction survives program/runtime failure.
    pub durable_program_crash: bool,
    /// A committed transaction survives OS failure or sudden power loss.
    pub durable_power_loss: bool,
}

pub trait Storage {
    type Txn<'db>: StorageTxn
    where
        Self: 'db;

    /// Begin a transaction. `write = false` gives a read snapshot;
    /// `write = true` the (single) writer.
    fn begin(&self, write: bool) -> Result<Self::Txn<'_>>;

    fn props(&self) -> StorageProps;
}

/// A stable, random-access byte source. Implementations must not allocate a
/// buffer proportional to the value merely to service a bounded read.
pub trait ValueRead {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Return zero only at EOF; `offset > len()` is invalid.
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize>;
    fn read_exact_at(&self, mut offset: usize, mut out: &mut [u8]) -> Result<()> {
        if offset.checked_add(out.len()).is_none_or(|end| end > self.len()) {
            return Err(Error::Corrupt);
        }
        while !out.is_empty() {
            let n = self.read_at(offset, out)?;
            if n == 0 || n > out.len() {
                return Err(Error::Corrupt);
            }
            offset += n;
            out = &mut out[n..];
        }
        Ok(())
    }
}

/// Borrow existing bytes without allocating a streamed source.
pub struct SliceValue<'a>(pub &'a [u8]);
impl ValueRead for SliceValue<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        let tail = self.0.get(offset..).ok_or(Error::InvalidArgument)?;
        let n = tail.len().min(out.len());
        out[..n].copy_from_slice(&tail[..n]);
        Ok(n)
    }
}

/// Borrowed-entry callback. True continues the scan; false stops it.
pub type ValueVisitor<'a> = dyn FnMut(&[u8], &dyn ValueRead) -> Result<bool> + 'a;

pub trait StorageTxn {
    /// An owned pin of one value and its read snapshot. It must hold the
    /// backend's actual page/resource rather than copy a complete value.
    type ValueReader: ValueRead;
    /// Read snapshots only; write transactions return `Error::Mode`.
    fn snapshot_value(&self, key: &[u8]) -> Result<Option<Self::ValueReader>>;

    /// Borrowed view of a stored key or value.
    type Guard<'g>: Deref<Target = [u8]>
    where
        Self: 'g;

    /// Exact lookup.
    fn get<'g>(&'g self, key: &[u8]) -> Result<Option<Self::Guard<'g>>>;

    /// Length/range reads must not materialize the rest of the value.
    fn value_len(&self, key: &[u8]) -> Result<Option<usize>>;
    fn read_value_at(&self, key: &[u8], offset: usize, out: &mut [u8]) -> Result<Option<usize>>;
    /// Ordered key-only seeks must not copy the corresponding value.
    fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>>;
    fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>>;
    /// Visit entries from the inclusive/exclusive key bound, in ascending order
    /// (descending when `reverse`). Return false to stop or true to continue.
    /// Values are borrowed only for the callback and support bounded reads;
    /// never copy a complete value just to traverse it. Errors stop immediately.
    /// Native backends retain one range iterator for the entire visit.
    fn visit(
        &self,
        key: &[u8],
        exclusive: bool,
        reverse: bool,
        visitor: &mut ValueVisitor<'_>,
    ) -> Result<()>;
    /// Fill final backend storage from a stable source. On any error the
    /// caller must discard the write transaction; partially filled data must
    /// never be committed. No whole-value intermediate buffer is permitted.
    fn put_from(&mut self, key: &[u8], value: &dyn ValueRead) -> Result<()>;

    /// First entry with key `>= key` (or `> key` when `excl`).
    #[allow(clippy::type_complexity)]
    fn next_from<'g>(
        &'g self,
        key: &[u8],
        excl: bool,
    ) -> Result<Option<(Self::Guard<'g>, Self::Guard<'g>)>>;

    /// Last entry with key `<= key` (or `< key` when `excl`).
    #[allow(clippy::type_complexity)]
    fn prev_from<'g>(
        &'g self,
        key: &[u8],
        excl: bool,
    ) -> Result<Option<(Self::Guard<'g>, Self::Guard<'g>)>>;

    /// Insert or replace. Fails with [`Error::ReadOnly`] on a snapshot.
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()>;

    /// Remove `key`; returns whether it was present.
    fn del(&mut self, key: &[u8]) -> Result<bool>;

    /// Atomically publish this transaction's changes.
    fn commit(self) -> Result<()>;
}

pub mod mem {
    //! In-memory backend: ephemeral storage for tests, wasm sessions, and
    //! RAM-only devices.
    //!
    //! Snapshots are O(1) (shared immutable map); each write transaction
    //! clones the map (O(n)) and swaps it back in on commit. Simple and
    //! obviously correct; not intended for large persistent workloads.

    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::rc::Rc;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use core::ops::Bound;

    type Map = BTreeMap<Vec<u8>, Vec<u8>>;

    #[derive(Default)]
    pub struct MemStorage {
        committed: RefCell<Rc<Map>>,
        writer_active: Cell<bool>,
    }

    impl MemStorage {
        pub fn new() -> MemStorage {
            MemStorage::default()
        }
    }

    pub struct MemTxn<'db> {
        store: &'db MemStorage,
        /// Snapshot for readers; the writer's working copy lives in `write`.
        snapshot: Rc<Map>,
        write: Option<Map>,
    }

    impl Storage for MemStorage {
        type Txn<'db> = MemTxn<'db>;

        fn begin(&self, write: bool) -> Result<MemTxn<'_>> {
            let snapshot = self.committed.borrow().clone();
            let write = if write {
                if self.writer_active.replace(true) {
                    return Err(Error::Busy);
                }
                Some((*snapshot).clone())
            } else {
                None
            };
            Ok(MemTxn { store: self, snapshot, write })
        }

        fn props(&self) -> StorageProps {
            StorageProps {
                retention: Retention::Ephemeral,
                durable_program_crash: false,
                durable_power_loss: false,
            }
        }
    }

    impl MemTxn<'_> {
        fn map(&self) -> &Map {
            self.write.as_ref().unwrap_or(&self.snapshot)
        }
    }

    impl Drop for MemTxn<'_> {
        fn drop(&mut self) {
            if self.write.is_some() {
                assert!(
                    self.store.writer_active.replace(false),
                    "the exclusive writer owns its admission slot"
                );
            }
        }
    }

    pub struct MemValueReader {
        snapshot: Rc<Map>,
        key: Vec<u8>,
    }
    impl ValueRead for MemValueReader {
        fn len(&self) -> usize {
            self.snapshot[&self.key].len()
        }
        fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
            SliceValue(&self.snapshot[&self.key]).read_at(offset, out)
        }
    }

    impl StorageTxn for MemTxn<'_> {
        type ValueReader = MemValueReader;
        fn snapshot_value(&self, key: &[u8]) -> Result<Option<MemValueReader>> {
            if self.write.is_some() {
                return Err(Error::Mode);
            }
            Ok(self
                .snapshot
                .contains_key(key)
                .then(|| MemValueReader { snapshot: self.snapshot.clone(), key: key.to_vec() }))
        }
        type Guard<'g>
            = &'g [u8]
        where
            Self: 'g;

        fn get<'g>(&'g self, key: &[u8]) -> Result<Option<&'g [u8]>> {
            Ok(self.map().get(key).map(Vec::as_slice))
        }

        fn value_len(&self, key: &[u8]) -> Result<Option<usize>> {
            Ok(self.map().get(key).map(Vec::len))
        }
        fn read_value_at(
            &self,
            key: &[u8],
            offset: usize,
            out: &mut [u8],
        ) -> Result<Option<usize>> {
            self.map().get(key).map(|v| SliceValue(v).read_at(offset, out)).transpose()
        }
        fn next_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
            let bound = if exclusive { Bound::Excluded(key) } else { Bound::Included(key) };
            Ok(self
                .map()
                .range::<[u8], _>((bound, Bound::Unbounded))
                .next()
                .map(|(k, _)| k.clone()))
        }
        fn prev_key(&self, key: &[u8], exclusive: bool) -> Result<Option<Vec<u8>>> {
            let bound = if exclusive { Bound::Excluded(key) } else { Bound::Included(key) };
            Ok(self
                .map()
                .range::<[u8], _>((Bound::Unbounded, bound))
                .next_back()
                .map(|(k, _)| k.clone()))
        }
        fn visit(
            &self,
            key: &[u8],
            exclusive: bool,
            reverse: bool,
            visitor: &mut ValueVisitor<'_>,
        ) -> Result<()> {
            let bound = if exclusive { Bound::Excluded(key) } else { Bound::Included(key) };
            let bounds =
                if reverse { (Bound::Unbounded, bound) } else { (bound, Bound::Unbounded) };
            let mut range = self.map().range::<[u8], _>(bounds);
            loop {
                let entry = if reverse { range.next_back() } else { range.next() };
                let Some((key, value)) = entry else {
                    return Ok(());
                };
                if !visitor(key, &SliceValue(value))? {
                    return Ok(());
                }
            }
        }
        fn put_from(&mut self, key: &[u8], value: &dyn ValueRead) -> Result<()> {
            let map = self.write.as_mut().ok_or(Error::ReadOnly)?;
            // This is the RAM backend's final stored value, not transfer scratch.
            let mut bytes = alloc::vec![0; value.len()];
            for (i, chunk) in bytes.chunks_mut(4096).enumerate() {
                value.read_exact_at(i * 4096, chunk)?;
            }
            map.insert(key.to_vec(), bytes);
            Ok(())
        }

        fn next_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
            let lo = if excl { Bound::Excluded(key) } else { Bound::Included(key) };
            Ok(self
                .map()
                .range::<[u8], _>((lo, Bound::Unbounded))
                .next()
                .map(|(k, v)| (k.as_slice(), v.as_slice())))
        }

        fn prev_from<'g>(&'g self, key: &[u8], excl: bool) -> Result<Option<(&'g [u8], &'g [u8])>> {
            let hi = if excl { Bound::Excluded(key) } else { Bound::Included(key) };
            Ok(self
                .map()
                .range::<[u8], _>((Bound::Unbounded, hi))
                .next_back()
                .map(|(k, v)| (k.as_slice(), v.as_slice())))
        }

        fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
            let map = self.write.as_mut().ok_or(Error::ReadOnly)?;
            map.insert(key.to_vec(), val.to_vec());
            Ok(())
        }

        fn del(&mut self, key: &[u8]) -> Result<bool> {
            let map = self.write.as_mut().ok_or(Error::ReadOnly)?;
            Ok(map.remove(key).is_some())
        }

        fn commit(mut self) -> Result<()> {
            let map = self.write.take().ok_or(Error::ReadOnly)?;
            *self.store.committed.borrow_mut() = Rc::new(map);
            assert!(
                self.store.writer_active.replace(false),
                "the exclusive writer owns its admission slot"
            );
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests;
}
