//! Compute the Merkle root of an independently keyed view in sorted source
//! order. Each level retains one open branch; no source values are retained.
use super::*;
use crate::hash::BranchHasher;

pub(super) struct RootBuilder {
    key: [u8; 32],
    fanout: u8,
    levels: Vec<BranchHasher>,
}

impl RootBuilder {
    fn open(&mut self, level: usize) -> Result<()> {
        if level == self.levels.len() {
            if level >= usize::from(encoding::LEVEL_MAX) {
                return Err(Error::Capacity);
            }
            self.levels.try_reserve(1).map_err(|_| Error::Capacity)?;
            self.levels.push(BranchHasher::new(&self.key));
        }
        Ok(())
    }

    fn take(&mut self, level: usize) -> [u8; 32] {
        core::mem::replace(&mut self.levels[level], BranchHasher::new(&self.key)).finish()
    }

    fn branch(&mut self, mut level: usize, mut child: [u8; 32]) -> Result<()> {
        loop {
            let split = level < self.levels.len() && encoding::is_boundary(child[0], self.fanout);
            self.open(level)?;
            let previous = split.then(|| self.take(level));
            self.levels[level].add_branch(&child);
            let Some(previous) = previous else { return Ok(()) };
            child = previous;
            level += 1;
        }
    }

    fn leaf(&mut self, key: &[u8], entry: &EntVal<'_>) -> Result<()> {
        if !self.levels.is_empty() && encoding::is_boundary(entry.hash_byte, self.fanout) {
            let completed = self.take(0);
            self.branch(1, completed)?;
        }
        self.open(0)?;
        self.levels[0].add_leaf(key, entry);
        Ok(())
    }

    fn finish(mut self) -> Result<Option<(u8, [u8; 32])>> {
        let mut level = 0;
        while level < self.levels.len() {
            let completed = self.take(level);
            level += 1;
            if level == self.levels.len() {
                return Ok(Some((u8::try_from(level).map_err(|_| Error::Capacity)?, completed)));
            }
            self.branch(level, completed)?;
        }
        Ok(None)
    }
}

// One canonical stored leaf. The source pin, compact header and incremental
// digest let a large value yield without copying, reopening or rehashing it.
pub(super) struct Leaf<R> {
    pub(super) raw: R,
    key: Vec<u8>,
    header: [u8; encoding::ENT_HEADER_FULL],
    header_len: usize,
    offset: usize,
    hasher: blake3::Hasher,
}

impl<R: ValueRead> Leaf<R> {
    pub(super) fn new(raw: R, key: Vec<u8>, projection_key: &[u8; 32]) -> Result<Self> {
        let entry = StreamEntry::new(&raw)?;
        let offset = entry.value.start;
        let header = entry.header;
        let header_len = entry.header_len;
        let hasher = crate::hash::entry_hasher(projection_key, &key, entry.flags);
        Ok(Self { raw, key, header, header_len, offset, hasher })
    }

    pub(super) fn advance(&mut self, budget: &mut usize) -> Result<bool> {
        let mut bytes = [0; 4096];
        while self.offset < self.raw.len() && *budget > 0 {
            let count = (self.raw.len() - self.offset).min(bytes.len()).min(*budget);
            self.raw.read_exact_at(self.offset, &mut bytes[..count])?;
            self.hasher.update(&bytes[..count]);
            self.offset += count;
            *budget -= count;
        }
        Ok(self.offset == self.raw.len())
    }

    pub(super) fn digest(&self) -> [u8; 32] {
        assert_eq!(self.offset, self.raw.len(), "all source bytes precede the hash");
        *self.hasher.finalize().as_bytes()
    }
}

/// A caller-driven scan of a pinned source. The scan can yield between batches
/// without reopening its source or admitting a concurrent writer into its cut.
pub struct ProjectionScan<'a, 'db, S: Storage> {
    source: &'a ReadTxn<'db, S>,
    builder: RootBuilder,
    next: Option<Vec<u8>>,
    leaf: Option<Leaf<<S::Txn<'db> as StorageTxn>::ValueReader>>,
}

impl<'a, 'db, S: Storage> ProjectionScan<'a, 'db, S> {
    pub fn new(source: &'a ReadTxn<'db, S>, hash_key: [u8; 32], branch_factor: u8) -> Result<Self> {
        if !(2..=128).contains(&branch_factor) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            source,
            leaf: None,
            next: source.next_source_key(&[], false)?,
            builder: RootBuilder { key: hash_key, fanout: branch_factor, levels: Vec::new() },
        })
    }

    /// Hash at most 64 keys and 256 KiB of value bytes per call.
    pub fn advance(&mut self) -> Result<bool> {
        let mut budget = DEFAULT_MSG_CAP;
        for _ in 0..64 {
            if self.leaf.is_none() {
                let Some(key) = self.next.take() else { return Ok(true) };
                let mut stored = vec![0];
                stored.extend_from_slice(&key);
                let raw = self.source.raw().snapshot_value(&stored)?.ok_or(Error::Corrupt)?;
                self.leaf = Some(Leaf::new(raw, key, &self.builder.key)?);
            }
            let leaf = self.leaf.as_mut().expect("one admitted source");
            if !leaf.advance(&mut budget)? {
                return Ok(false);
            }
            let hash = leaf.digest();
            let mut decoded = EntVal::decode(&leaf.header[..leaf.header_len])?;
            decoded.hash_byte = hash[0];
            if decoded.hash.is_some() {
                decoded.hash = Some(&hash);
            }
            self.builder.leaf(&leaf.key, &decoded)?;
            self.next = self.source.next_source_key(&leaf.key, true)?;
            self.leaf = None;
        }
        Ok(self.next.is_none())
    }

    pub fn finish(self) -> Result<Option<(u8, [u8; 32])>> {
        if self.next.is_some() || self.leaf.is_some() {
            return Err(Error::Protocol);
        }
        self.builder.finish()
    }
}
