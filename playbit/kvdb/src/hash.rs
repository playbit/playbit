//! Keyed BLAKE3 hashing of entries and branches.
//!
//! Every replica of one database shares a 32-byte `hash_key`
//! ([`crate::encoding::Metadata::hash_key`]). Keying the hash prevents
//! adversarial key/value choices from degenerating the tree shape, and the
//! shared key is what makes tree shapes and hashes comparable across
//! replicas.
//!
//! All hash inputs are framed unambiguously and use little-endian length
//! prefixes, so hashes are identical on every architecture.
//! BranchHasher is data plane. Release checks preserve the fixed-width
//! compact-child framing even when an internal caller bypasses validation.

use crate::encoding::{ENT_FLAG_TOMBSTONE, EntVal};

/// Hash of one leaf entry:
/// `keyed_blake3(hash_key, le32(key.len) ++ key ++ flags ++ userval)`.
///
/// `flags` participates so a tombstone hashes differently from a live entry
/// with an empty value. Only the tombstone bit is hashed; other (future)
/// flags must not affect identity.
pub fn entry_hash(hash_key: &[u8; 32], key: &[u8], flags: u8, userval: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(hash_key);
    h.update(&(key.len() as u32).to_le_bytes());
    h.update(key);
    h.update(&[flags & ENT_FLAG_TOMBSTONE]);
    h.update(userval);
    *h.finalize().as_bytes()
}

/// Canonical entry hash read in bounded chunks.
pub fn entry_hash_from(
    hash_key: &[u8; 32],
    key: &[u8],
    flags: u8,
    value: &dyn crate::storage::ValueRead,
) -> crate::error::Result<[u8; 32]> {
    let mut h = blake3::Hasher::new_keyed(hash_key);
    h.update(&(key.len() as u32).to_le_bytes());
    h.update(key);
    h.update(&[flags & ENT_FLAG_TOMBSTONE]);
    let mut buf = [0; 4096];
    let mut offset = 0;
    while offset < value.len() {
        let n = buf.len().min(value.len() - offset);
        value.read_exact_at(offset, &mut buf[..n])?;
        h.update(&buf[..n]);
        offset += n;
    }
    Ok(*h.finalize().as_bytes())
}

/// Incremental hash of a branch node over its children, in key order.
///
/// Each child's contribution is framed with a type byte so concatenated
/// contributions are unambiguous:
///
/// ```text
/// branch child:       0x01 ++ child_branch_hash[32]
/// full leaf child:    0x02 ++ clock[8] ++ entry_hash[32]
/// compact leaf child: 0x03 ++ clock[8] ++ le16(key.len) ++ key ++ flags ++ le16(userval.len) ++ userval
/// ```
///
/// Compact leaves contribute their raw key and value instead of a stored
/// hash (only one hash byte is stored for them); the length-prefixed raw
/// material is exactly as collision-resistant here, without nested hashing.
/// Leaf clocks participate so equal subtree hashes also prove equal LWW
/// versions. Equal values with different clocks must still synchronize.
pub struct BranchHasher {
    inner: blake3::Hasher,
}

const CHILD_BRANCH: u8 = 0x01;
const CHILD_LEAF_FULL: u8 = 0x02;
const CHILD_LEAF_COMPACT: u8 = 0x03;

impl BranchHasher {
    pub fn new(hash_key: &[u8; 32]) -> BranchHasher {
        BranchHasher { inner: blake3::Hasher::new_keyed(hash_key) }
    }

    pub fn add_branch(&mut self, child_hash: &[u8; 32]) {
        self.inner.update(&[CHILD_BRANCH]);
        self.inner.update(child_hash);
    }

    /// Add a leaf child. `key` is the user key (no level prefix).
    pub fn add_leaf(&mut self, key: &[u8], ent: &EntVal<'_>) {
        assert!(key.len() <= crate::KEY_SIZE_MAX, "leaf key fits the canonical length field");
        match ent.hash {
            Some(hash) => {
                self.inner.update(&[CHILD_LEAF_FULL]);
                self.inner.update(&ent.clock.encode());
                self.inner.update(hash);
            }
            None => {
                assert!(
                    ent.userval.len() <= crate::encoding::ENT_USERVAL_COMPACT_MAX,
                    "compact leaf payload fits its canonical layout"
                );
                self.inner.update(&[CHILD_LEAF_COMPACT]);
                self.inner.update(&ent.clock.encode());
                self.inner.update(&(key.len() as u16).to_le_bytes());
                self.inner.update(key);
                self.inner.update(&[ent.flags & ENT_FLAG_TOMBSTONE]);
                self.inner.update(&(ent.userval.len() as u16).to_le_bytes());
                self.inner.update(ent.userval);
            }
        }
    }

    pub fn finish(self) -> [u8; 32] {
        *self.inner.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests;
