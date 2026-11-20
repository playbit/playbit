//! Prolly-tree maintenance.
//!
//! The tree is defined canonically, as a pure function of the leaf set:
//!
//! - Level 0 nodes are the leaf entries (tombstones included).
//! - The *chunk heads* of a level are its first node plus every node whose
//!   value byte 0 satisfies [`encoding::is_boundary`]. A chunk spans from
//!   its head up to (excluding) the next head.
//! - Each chunk has one parent node at the level above: key = the chunk
//!   head's user key, except the first chunk of a level, whose parent is the
//!   level *anchor* (empty user key, sorts first). The parent's value is the
//!   [`BranchHasher`] hash of the chunk's members.
//! - Level 1 exists whenever there is at least one leaf. Level `L+1`
//!   (`L >= 1`) exists iff level `L` has at least two nodes. The highest
//!   level therefore always has exactly one node — its anchor — which is
//!   the tree root.
//!
//! Because the shape depends only on the data (and the shared hash key),
//! any two replicas holding the same entries store byte-identical trees.
//!
//! Updates are *windowed re-chunks*: after a leaf changes, each level is
//! re-chunked over the smallest window that could have been affected — from
//! one chunk head before the changed range to the first head after it — and
//! the computed parents are diffed against the stored ones. Changes ripple
//! upward until a level reports no differences. Bulk builds use the same
//! re-chunk code over a whole-level window, so the incremental and bulk paths
//! agree by construction. Verification independently checks the stored result.
//!
//! The re-chunk scan is data plane. Its cursor/window assertions remain in
//! release: a bad window can silently omit parents. Verification independently
//! streams whole levels without rebuilding or modifying storage, retaining only
//! fixed-size headers, hash state and key bounds (not a copy of the tree).

use crate::clock::Clock;
#[cfg(test)]
use crate::encoding::ENT_FLAG_TOMBSTONE;
use crate::encoding::{self, ENT_USERVAL_COMPACT_MAX, EntVal, META_KEY_BYTE};
use crate::error::{Error, Result};
use crate::hash::BranchHasher;
use crate::storage::{SliceValue, StorageTxn, ValueRead};
use alloc::vec;
use alloc::vec::Vec;

/// (storage key, stored value), copied out of the backend.
#[cfg(test)]
type Node = (Vec<u8>, Vec<u8>);

pub(crate) struct Tree<'a, T: StorageTxn> {
    pub txn: &'a mut T,
    pub hash_key: [u8; 32],
    pub branch_factor: u8,
}

impl<T: StorageTxn> Tree<'_, T> {
    /// Validate stored leaves and every canonical parent, including the sole
    /// root anchor. Disk corruption is an error, never an assertion failure.
    pub(crate) fn verify(&mut self) -> Result<()> {
        if !(2..=128).contains(&self.branch_factor) {
            return Err(Error::Corrupt);
        }
        let mut count = self.verify_leaves()?;
        for level in 0..=encoding::LEVEL_MAX {
            if count == 0 || (level > 0 && count == 1) {
                return self.verify_end(level + 1);
            }
            if level == encoding::LEVEL_MAX {
                return Err(Error::Corrupt);
            }
            let expected = self.verify_parents(level)?;
            let mut actual = 0u64;
            self.txn.visit(&[level + 1], false, false, &mut |key, _| {
                if !node_at_level(key, level + 1) {
                    return Ok(false);
                }
                actual = actual.checked_add(1).ok_or(Error::Capacity)?;
                Ok(true)
            })?;
            if actual != expected {
                return Err(Error::Corrupt);
            }
            count = actual;
        }
        unreachable!("the last representable level either ends the tree or returns corruption")
    }

    fn verify_leaves(&self) -> Result<u64> {
        let mut count = 0u64;
        self.txn.visit(&[], false, false, &mut |key, value| {
            if key.is_empty() {
                return Err(Error::Corrupt);
            }
            if key[0] != 0 {
                return Ok(false);
            }
            verify_leaf(&self.hash_key, key, value)?;
            count = count.checked_add(1).ok_or(Error::Capacity)?;
            Ok(true)
        })?;
        Ok(count)
    }

    fn verify_parents(&self, level: u8) -> Result<u64> {
        let mut count = 0u64;
        let mut cursor = vec![level];
        loop {
            let (hash, next) = self.verify_chunk(level, &cursor)?;
            let mut parent = cursor.clone();
            parent[0] = level + 1;
            // End the backend visit before another table access. redb permits
            // only one live write-table handle, even for read-only inspection.
            self.verify_parent(&parent, hash)?;
            count = count.checked_add(1).ok_or(Error::Capacity)?;
            match next {
                None => return Ok(count),
                Some(next) => cursor = next,
            }
        }
    }

    fn verify_chunk(&self, level: u8, cursor: &[u8]) -> Result<([u8; 32], Option<Vec<u8>>)> {
        let mut hasher = BranchHasher::new(&self.hash_key);
        let mut first = true;
        let mut next = None;
        let mut header = [0; encoding::ENT_HEADER_FULL];
        self.txn.visit(cursor, false, false, &mut |key, value| {
            if !node_at_level(key, level) {
                return Ok(false);
            }
            if first && level > 0 && cursor.len() == 1 && key.len() != 1 {
                return Err(Error::Corrupt);
            }
            let value = read_node_header(key, value, &mut header)?;
            if !first && self.is_boundary(value)? {
                next = Some(key.to_vec());
                return Ok(false);
            }
            first = false;
            if level == 0 {
                hasher.add_leaf(&key[1..], &EntVal::decode(value)?);
            } else {
                hasher.add_branch(value.try_into().map_err(|_| Error::Corrupt)?);
            }
            Ok(true)
        })?;
        if first {
            return Err(Error::Corrupt);
        }
        Ok((hasher.finish(), next))
    }

    fn verify_parent(&self, key: &[u8], hash: [u8; 32]) -> Result<()> {
        // Length is checked before get: a corrupt branch must not allocate an
        // arbitrarily large value in a backend that copies write-table reads.
        if self.txn.value_len(key)? != Some(hash.len()) {
            return Err(Error::Corrupt);
        }
        if self.txn.get(key)?.is_none_or(|stored| *stored != hash) {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn verify_end(&self, from_level: u8) -> Result<()> {
        // Inspect borrowed keys: seeking an owned key could copy an arbitrarily
        // large corrupt tail key before its length or level is validated.
        self.txn.visit(&[from_level], false, false, &mut |key, value| {
            if key.first() == Some(&encoding::LOCAL_KEY_BYTE) {
                if key.len() < 2
                    || key.len() > crate::KEY_SIZE_MAX + 1
                    || value.len() > crate::VALUE_SIZE_MAX
                {
                    return Err(Error::Corrupt);
                }
                return Ok(true);
            }
            if key != encoding::META_KEY {
                return Err(Error::Corrupt);
            }
            Ok(true) // Only metadata and local secondary records follow the tree.
        })
    }

    /// Insert or replace the leaf `ukey`. Returns false (and writes nothing)
    /// when an equivalent entry already exists; equivalence includes the originating clock.
    pub fn put_leaf(
        &mut self,
        ukey: &[u8],
        userval: &[u8],
        flags: u8,
        clock: Clock,
    ) -> Result<bool> {
        self.put_leaf_from(ukey, &SliceValue(userval), flags, clock)
    }

    pub fn put_leaf_from(
        &mut self,
        ukey: &[u8],
        value: &dyn ValueRead,
        flags: u8,
        clock: Clock,
    ) -> Result<bool> {
        let Some(stable) = self.write_leaf_from(ukey, value, flags, clock)? else {
            return Ok(false);
        };
        self.leaf_changed(ukey, stable)?;
        Ok(true)
    }

    /// Store one leaf without updating branches. None means unchanged; Some
    /// records whether an existing leaf retained its boundary status. Only the
    /// full receiver defers repair, inside its private atomic transaction.
    pub(crate) fn write_leaf_from(
        &mut self,
        ukey: &[u8],
        value: &dyn ValueRead,
        flags: u8,
        clock: Clock,
    ) -> Result<Option<bool>> {
        if value.len() > crate::VALUE_SIZE_MAX {
            return Err(Error::ValueSize);
        }
        let hash = crate::hash::entry_hash_from(&self.hash_key, ukey, flags, value)?;
        let skey = leaf_skey(ukey);
        let mut header = Vec::with_capacity(encoding::ENT_HEADER_FULL);
        if value.len() <= ENT_USERVAL_COMPACT_MAX {
            let mut small = [0; ENT_USERVAL_COMPACT_MAX];
            value.read_exact_at(0, &mut small[..value.len()])?;
            encoding::encode_ent_val(&mut header, &hash, flags, clock, &small[..value.len()]);
        } else {
            header.extend_from_slice(&hash);
            header.push(flags);
            header.extend_from_slice(&clock.encode());
        }
        let stable_leaf_boundary = if let Some(old_header) = node_value_header(self.txn, &skey)? {
            let old = EntVal::decode(&old_header)?;
            let new = EntVal::decode(&header)?;
            if old.content_eq(&new) && old.clock == clock {
                return Ok(None);
            }
            self.is_boundary(&old_header)? == self.is_boundary(&header)?
        } else {
            false
        };
        if value.len() <= ENT_USERVAL_COMPACT_MAX {
            self.txn.put(&skey, &header)?;
        } else {
            struct Encoded<'a> {
                header: &'a [u8],
                value: &'a dyn ValueRead,
            }
            impl ValueRead for Encoded<'_> {
                fn len(&self) -> usize {
                    self.header.len() + self.value.len()
                }
                fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
                    if offset < self.header.len() {
                        return SliceValue(self.header).read_at(offset, out);
                    }
                    self.value.read_at(offset - self.header.len(), out)
                }
            }
            self.txn.put_from(&skey, &Encoded { header: &header, value })?;
        }
        Ok(Some(stable_leaf_boundary))
    }

    /// Read the leaf at `ukey` (tombstones included), copied out.
    pub fn get_leaf_raw(&self, ukey: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.txn.get(&leaf_skey(ukey))?.map(|g| g.to_vec()))
    }

    /// Erase the leaf entirely ("delete and forget"): to other replicas the
    /// key becomes *missing*, not *deleted*.
    pub fn remove_leaf(&mut self, ukey: &[u8]) -> Result<bool> {
        let changed = self.erase_leaf(ukey)?;
        if changed {
            self.leaf_changed(ukey, false)?;
        }
        Ok(changed)
    }

    /// Forget a leaf without updating branches; see write_leaf_from.
    pub(crate) fn erase_leaf(&mut self, ukey: &[u8]) -> Result<bool> {
        let skey = leaf_skey(ukey);
        if self.txn.get(&skey)?.is_none() {
            return Ok(false);
        }
        self.txn.del(&skey)?;
        Ok(true)
    }

    /// The current level-1 parent interval containing this leaf key. Branches
    /// must be consistent when called: a receiver queries only before a batch
    /// starts or after its preceding batch was repaired. Empty bounds cover
    /// an empty tree; the anchor's empty key covers the first chunk.
    pub(crate) fn leaf_interval(&self, key: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut seek = Vec::with_capacity(key.len() + 1);
        seek.push(1);
        seek.extend_from_slice(key);
        let mut lower = Vec::new();
        let mut upper = None;
        self.txn.visit(&seek, false, true, &mut |key, _| {
            if node_at_level(key, 1) {
                if key.len() > crate::KEY_SIZE_MAX + 1 {
                    return Err(Error::Corrupt);
                }
                lower = key[1..].to_vec();
            }
            Ok(false)
        })?;
        self.txn.visit(&seek, true, false, &mut |key, _| {
            if node_at_level(key, 1) {
                if key.len() > crate::KEY_SIZE_MAX + 1 {
                    return Err(Error::Corrupt);
                }
                upper = Some(key[1..].to_vec());
            }
            Ok(false)
        })?;
        Ok((lower, upper))
    }

    /// Re-derive every branch level from the leaves (bulk build / repair).
    pub fn build_all(&mut self) -> Result<()> {
        self.delete_levels_from(1)?;
        // A window covering every possible user key (keys are <= KEY_SIZE_MAX).
        let lo = Vec::new();
        let hi = vec![0xffu8; crate::KEY_SIZE_MAX + 1];
        self.propagate(&lo, &hi, false)
    }

    /// Ripple a leaf change upward.
    fn leaf_changed(&mut self, ukey: &[u8], stable_leaf_boundary: bool) -> Result<()> {
        self.propagate(ukey, ukey, stable_leaf_boundary)
    }

    pub(crate) fn propagate(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        stable_leaf_boundary: bool,
    ) -> Result<()> {
        assert!(lo <= hi, "changed tree window is ordered");
        assert!((2..=128).contains(&self.branch_factor), "tree fan-out was validated at open");
        let mut lo = lo.to_vec();
        let mut hi = hi.to_vec();
        let mut level = 0u8;
        // Hash-selected boundaries can have fan-out one: log(key_count)
        // is not a worst-case depth bound. The format's 127 levels bound
        // work even for adversarial hash distributions; rechunk asserts
        // that every computed parent stays inside this bound.
        while level < encoding::LEVEL_MAX {
            match self.rechunk(level, &lo, &hi, stable_leaf_boundary && level == 0)? {
                None => return Ok(()),
                Some((l, h)) => {
                    lo = l;
                    hi = h;
                }
            }
            level += 1;
        }
        Err(Error::Capacity)
    }

    /// Re-chunk level `level` over a window covering the changed user-key
    /// range `[lo, hi]` and write the resulting parent entries at
    /// `level + 1`. Returns the changed parent user-key range, or None when
    /// the parents were already correct (or the level above was removed).
    fn rechunk(
        &mut self,
        level: u8,
        lo: &[u8],
        hi: &[u8],
        stable_leaf_boundary: bool,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        assert!(lo <= hi, "re-chunk window is ordered");
        assert!(level < encoding::LEVEL_MAX, "parent has a representable level");
        assert!(!stable_leaf_boundary || level == 0, "only leaves have a stable boundary shortcut");
        assert!(!stable_leaf_boundary || lo == hi, "stable boundary covers exactly one leaf");
        let parent_level = level + 1;

        let mut first_key = None;
        let mut has_two = false;
        let mut has_higher_level = false;
        let mut header = [0; encoding::ENT_HEADER_FULL];
        self.txn.visit(&[level], false, false, &mut |key, value| {
            if !node_at_level(key, level) {
                // When fewer than two nodes were found, this visit exhausted
                // the level. Metadata/EOF proves there is nothing to prune;
                // an intervening branch still requires ordinary cleanup.
                has_higher_level = key.first().is_some_and(|byte| *byte < META_KEY_BYTE);
                return Ok(false);
            }
            read_node_header(key, value, &mut header)?;
            if first_key.is_none() {
                first_key = Some(key.to_vec());
                Ok(true)
            } else {
                has_two = true;
                Ok(false)
            }
        })?;
        let Some(first_key) = first_key else {
            // Level 0 became empty: the whole tree above it goes away.
            if has_higher_level {
                self.delete_levels_from(1.max(level))?;
            }
            return Ok(None);
        };

        if level > 0 && !has_two {
            // A single-node level is the root; nothing may exist above it.
            if has_higher_level {
                self.delete_levels_from(parent_level)?;
            }
            return Ok(None);
        }

        // Window start: one chunk head before the head covering `lo` — an
        // inserted head at `lo` splits its predecessor chunk, whose hash
        // then changes too.
        // An existing leaf overwrite with unchanged boundary status keeps
        // its chunk membership. Higher levels always need the predecessor.
        let h0 = self.window_head(level, lo, &first_key, stable_leaf_boundary)?;
        assert!(h0 >= first_key, "window head cannot precede the level's first entry");
        let window_is_first = h0 == first_key;

        // Read one complete chunk at a time. Parent writes cannot change this
        // level, and every visit ends before mutation. The next boundary is
        // inspected again when resuming, but no child value or backend guard
        // survives the visit. Scratch is independent of chunk size and count.
        let mut cursor = h0;
        let mut first_chunk = true;
        let mut extended = false;
        let mut parent_cursor = None;
        let mut changed = None;
        loop {
            let mut next_head = None;
            let mut end_window = false;
            let mut hasher = BranchHasher::new(&self.hash_key);
            let mut first_child = true;
            let mut header = [0; encoding::ENT_HEADER_FULL];
            self.txn.visit(&cursor, false, false, &mut |key, value| {
                if !node_at_level(key, level) {
                    return Ok(false);
                }
                let value = read_node_header(key, value, &mut header)?;
                let ukey = &key[1..];
                if !first_child && self.is_boundary(value)? {
                    // Preserve the extra chunk needed when the first node's
                    // anchor membership changes. This flag spans visits.
                    if ukey > hi {
                        if stable_leaf_boundary || !window_is_first || extended {
                            end_window = true;
                        } else {
                            extended = true;
                        }
                    }
                    next_head = Some(key.to_vec());
                    return Ok(false);
                }
                first_child = false;
                if level == 0 {
                    hasher.add_leaf(ukey, &EntVal::decode(value)?);
                } else {
                    let child = value.try_into().map_err(|_| Error::Corrupt)?;
                    hasher.add_branch(child);
                }
                Ok(true)
            })?;
            assert!(!first_child, "each chunk includes its selected head");
            let hash = hasher.finish();
            let mut parent = cursor.clone();
            parent[0] = parent_level;
            if window_is_first && first_chunk {
                parent.truncate(1);
            }
            // Merge computed parents in order. At most one stored parent is
            // copied, and only its fixed-size hash is read. Insertions behind
            // the exclusive cursor cannot be mistaken for old parents later.
            if parent_cursor.is_none() {
                parent_cursor = Some(parent.clone());
            }
            self.merge_parent(
                parent_level,
                parent_cursor.as_mut().expect("the first parent initialized the cursor"),
                !first_chunk,
                &parent,
                Some(&hash),
                &mut changed,
            )?;
            first_chunk = false;
            match next_head {
                Some(mut next) if end_window => {
                    next[0] = parent_level;
                    self.merge_parent(
                        parent_level,
                        parent_cursor.as_mut().expect("the first parent initialized the cursor"),
                        true,
                        &next,
                        None,
                        &mut changed,
                    )?;
                    break;
                }
                None => {
                    // The next level prefix is an exclusive bound beyond all
                    // parents at this level, including their empty anchor.
                    self.merge_parent(
                        parent_level,
                        parent_cursor.as_mut().expect("the first parent initialized the cursor"),
                        true,
                        &[parent_level + 1],
                        None,
                        &mut changed,
                    )?;
                    break;
                }
                Some(next) => {
                    assert!(next > cursor, "chunk traversal advances strictly");
                    cursor = next;
                }
            }
        }
        Ok(changed)
    }

    /// Delete old parents preceding `parent`, then optionally upsert that
    /// computed parent. With no hash, `parent` is the exclusive window end.
    fn merge_parent(
        &mut self,
        level: u8,
        cursor: &mut Vec<u8>,
        mut exclusive: bool,
        parent: &[u8],
        hash: Option<&[u8; 32]>,
        changed: &mut Option<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        assert!(cursor.as_slice() <= parent, "parent diff advances toward its ordered bound");
        assert_eq!(cursor[0], level, "parent diff stays within its selected level");
        loop {
            let mut old = None;
            let mut header = [0; encoding::ENT_HEADER_FULL];
            self.txn.visit(cursor, exclusive, false, &mut |key, value| {
                if node_at_level(key, level) {
                    let len = read_node_header(key, value, &mut header)?.len();
                    if key <= parent {
                        old = Some((key.to_vec(), header, len));
                    }
                }
                Ok(false)
            })?;
            let Some((key, value, len)) = old else {
                if let Some(hash) = hash {
                    self.txn.put(parent, hash)?;
                    note_parent_change(changed, parent);
                    *cursor = parent.to_vec();
                }
                return Ok(());
            };
            if key == parent {
                if let Some(hash) = hash {
                    if value[..len] != hash[..] {
                        self.txn.put(parent, hash)?;
                        note_parent_change(changed, parent);
                    }
                    *cursor = key;
                }
                return Ok(());
            }
            self.txn.del(&key)?;
            note_parent_change(changed, &key);
            *cursor = key;
            exclusive = true;
        }
    }

    // ---- bounded header access through retained backend ranges ----

    fn is_boundary(&self, val: &[u8]) -> Result<bool> {
        let b = *val.first().ok_or(Error::Corrupt)?;
        Ok(encoding::is_boundary(b, self.branch_factor))
    }

    /// Locate the covering chunk and, unless its membership is stable, its
    /// predecessor in one reverse visit. Clamp to the first node even when
    /// `lo` precedes it; that node is a head regardless of its boundary hash.
    fn window_head(
        &self,
        level: u8,
        lo: &[u8],
        first_key: &[u8],
        covering_only: bool,
    ) -> Result<Vec<u8>> {
        let mut skey = Vec::with_capacity(1 + lo.len());
        skey.push(level);
        skey.extend_from_slice(lo);
        let mut found = first_key.to_vec();
        let mut heads_left = if covering_only { 1 } else { 2 };
        let mut header = [0; encoding::ENT_HEADER_FULL];
        self.txn.visit(&skey, false, true, &mut |key, value| {
            if !node_at_level(key, level) {
                return Ok(false);
            }
            let value = read_node_header(key, value, &mut header)?;
            if key == first_key {
                found = key.to_vec();
                return Ok(false);
            }
            if self.is_boundary(value)? {
                heads_left -= 1;
                if heads_left == 0 {
                    found = key.to_vec();
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        Ok(found)
    }

    /// Delete every branch entry at `level` and above.
    fn delete_levels_from(&mut self, level: u8) -> Result<()> {
        assert!(level >= 1, "branch pruning cannot erase leaves");
        loop {
            let mut key = None;
            let mut header = [0; encoding::ENT_HEADER_FULL];
            self.txn.visit(&[level], false, false, &mut |k, value| {
                if !k.is_empty() && k[0] < META_KEY_BYTE {
                    read_node_header(k, value, &mut header)?;
                    key = Some(k.to_vec());
                }
                Ok(false)
            })?;
            let Some(k) = key else { return Ok(()) };
            self.txn.del(&k)?;
        }
    }
}

/// Parent changes arrive in key order. Retain only their inclusive bounds.
fn note_parent_change(changed: &mut Option<(Vec<u8>, Vec<u8>)>, key: &[u8]) {
    let key = &key[1..];
    match changed {
        None => *changed = Some((key.to_vec(), key.to_vec())),
        Some((_, hi)) => {
            assert!(key >= hi.as_slice(), "parent changes arrive in ascending key order");
            if key > hi.as_slice() {
                *hi = key.to_vec();
            }
        }
    }
}

/// Branch maintenance needs at most a full leaf header; compact leaves
/// include their entire (at most 30-byte) value in that same bound.
pub(crate) fn node_value_header<T: StorageTxn>(txn: &T, key: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut found = None;
    let mut header = [0; encoding::ENT_HEADER_FULL];
    txn.visit(key, false, false, &mut |stored_key, value| {
        if stored_key == key {
            found = Some(read_node_header(key, value, &mut header)?.to_vec());
        }
        Ok(false)
    })?;
    Ok(found)
}

fn read_node_header<'a>(
    key: &[u8],
    value: &dyn ValueRead,
    header: &'a mut [u8; encoding::ENT_HEADER_FULL],
) -> Result<&'a [u8]> {
    if key.len() > crate::KEY_SIZE_MAX + 1 {
        return Err(Error::Corrupt);
    }
    let n = if key.first() == Some(&0) {
        value.len().min(encoding::ENT_HEADER_FULL)
    } else {
        value.len()
    };
    if n > header.len() {
        return Err(Error::Corrupt);
    }
    value.read_exact_at(0, &mut header[..n])?;
    Ok(&header[..n])
}

fn verify_leaf(hash_key: &[u8; 32], key: &[u8], value: &dyn ValueRead) -> Result<()> {
    if key.len() < 2 || key.len() > crate::KEY_SIZE_MAX + 1 {
        return Err(Error::Corrupt);
    }
    let mut header = [0; encoding::ENT_HEADER_FULL];
    let bytes = read_node_header(key, value, &mut header)?;
    let ent = EntVal::decode(bytes)?;
    let start = encoding::header_size(bytes);
    let source = LeafValue { raw: value, start };
    if source.len() > crate::VALUE_SIZE_MAX
        || ent.clock.is_zero()
        || (ent.hash.is_some() && source.len() <= ENT_USERVAL_COMPACT_MAX)
        || (ent.is_tombstone() && !source.is_empty())
    {
        return Err(Error::Corrupt);
    }
    let hash = crate::hash::entry_hash_from(hash_key, &key[1..], ent.flags, &source)?;
    if ent.hash_byte != hash[0] || ent.hash.is_some_and(|stored| stored != &hash) {
        return Err(Error::Corrupt);
    }
    Ok(())
}

/// Borrow a leaf's payload during verification without copying the value.
struct LeafValue<'a> {
    raw: &'a dyn ValueRead,
    start: usize,
}

impl ValueRead for LeafValue<'_> {
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

/// The tree root: `(level, branch hash)`, or None for an empty tree.
/// The root is the single node of the highest branch level, found by
/// stepping back once from the metadata key.
pub(crate) fn root<T: StorageTxn>(txn: &T) -> Result<Option<(u8, [u8; 32])>> {
    match txn.prev_from(&encoding::META_KEY, true)? {
        Some((k, v)) => {
            let (k, v): (&[u8], &[u8]) = (&k, &v);
            if k.is_empty() || k[0] == 0 || k[0] >= META_KEY_BYTE {
                return Ok(None); // clock entry (or nothing): empty tree
            }
            let hash: [u8; 32] = v.try_into().map_err(|_| Error::Corrupt)?;
            Ok(Some((k[0], hash)))
        }
        None => Ok(None),
    }
}

fn leaf_skey(ukey: &[u8]) -> Vec<u8> {
    let mut skey = Vec::with_capacity(1 + ukey.len());
    skey.push(0);
    skey.extend_from_slice(ukey);
    skey
}

/// Is `skey` a node of `level`?
fn node_at_level(skey: &[u8], level: u8) -> bool {
    skey.first() == Some(&level)
}

#[cfg(test)]
mod tests;
