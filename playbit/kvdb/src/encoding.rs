//! Storage format version 1 for plain and syncable databases.
//!
//! The whole database lives in one flat ordered byte-keyspace, partitioned by
//! a leading level byte:
//!
//! ```text
//! [0x00] ++ user_key  leaf entry
//! [0x0N] ++ key       branch node at level N (1..=0x7F); bare [0x0N] = anchor
//! [0x80]              metadata record (also the root-seek sentinel)
//! [0x81] ++ user_key  replica-local secondary record (not replicated)
//! ```
//!
//! Leaf values in a *syncable* database carry a header; branch values are a
//! bare 32-byte hash; entries in a *plain* database are raw user bytes with
//! no header at all.
//!
//! ```text
//! full    (userval > 30 bytes):  hash[32] flags[1] clock[8] userval…
//! compact (userval ≤ 30 bytes):  hash_byte[1] flags[1] clock[8] userval…
//! ```
//!
//! The two layouts are discriminated by total length: compact totals are at
//! most 40 bytes, full totals at least 41. A tombstone is a compact value
//! with an empty userval (total length exactly 10) and the TOMBSTONE flag.
//!
//! All multi-byte integers are little-endian, except clocks, which are
//! big-endian so byte order equals clock order.

use crate::clock::Clock;
use crate::error::{Error, Result};
use alloc::vec::Vec;

/// Highest valid branch level byte.
pub const LEVEL_MAX: u8 = 0x7F;
/// Key byte of the metadata record; sorts after every tree key.
pub const META_KEY_BYTE: u8 = 0x80;
/// The metadata record's full storage key.
pub const META_KEY: [u8; 1] = [META_KEY_BYTE];
/// Secondary records share the backend transaction but never enter the tree.
pub const LOCAL_KEY_BYTE: u8 = 0x81;

/// Validate before allocation; empty keys are seek bounds, never records.
pub(crate) fn local_key(key: &[u8], seek: bool) -> Result<Vec<u8>> {
    if key.is_empty() && !seek {
        return Err(Error::InvalidKey);
    }
    if key.len() > crate::KEY_SIZE_MAX {
        return Err(Error::KeySize);
    }
    let mut stored = Vec::with_capacity(1 + key.len());
    stored.push(LOCAL_KEY_BYTE);
    stored.extend_from_slice(key);
    Ok(stored)
}

/// Size of a full entry-value header: hash[32] + flags[1] + clock[8].
pub const ENT_HEADER_FULL: usize = 41;
/// Size of a compact entry-value header: hash_byte[1] + flags[1] + clock[8].
pub const ENT_HEADER_COMPACT: usize = 10;
/// Largest userval encodable in the compact layout.
/// (`ENT_HEADER_COMPACT + 30 == 40 < ENT_HEADER_FULL`.)
pub const ENT_USERVAL_COMPACT_MAX: usize = 30;

const _: () = assert!(ENT_HEADER_COMPACT + ENT_USERVAL_COMPACT_MAX + 1 == ENT_HEADER_FULL);
const _: () = assert!(LEVEL_MAX + 1 == META_KEY_BYTE);
const _: () = assert!(META_KEY_BYTE + 1 == LOCAL_KEY_BYTE);

/// Entry flag: the entry is a tombstone (deleted key).
pub const ENT_FLAG_TOMBSTONE: u8 = 1 << 0;
const ENT_FLAGS_KNOWN: u8 = ENT_FLAG_TOMBSTONE;

/// A node's hash decides whether it starts a new parent branch ("boundary").
///
/// One hash byte decides, so a whole number of the 256 byte values are
/// boundaries: `256 / branch_factor` rounded down. The fan-out that
/// produces is `256` over that count, which equals `branch_factor` only
/// when `branch_factor` divides 256 and is larger otherwise — 33 averages
/// 36, and every value from 86 up shares the count 2 and averages 128.
#[inline]
pub fn is_boundary(hash_byte: u8, branch_factor: u8) -> bool {
    // Data plane: callers validate metadata before entering a scan; this
    // release check catches internal callers that bypass that boundary.
    assert!((2..=128).contains(&branch_factor), "validated tree fan-out is 2..=128");
    (hash_byte as u16) < 256 / branch_factor as u16
}

/// Header size of a stored leaf value (discriminated by total length).
#[inline]
pub fn header_size(stored: &[u8]) -> usize {
    if stored.len() >= ENT_HEADER_FULL { ENT_HEADER_FULL } else { ENT_HEADER_COMPACT }
}

/// Decoded view of a syncable leaf entry value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntVal<'a> {
    /// First byte of the entry hash (drives the boundary decision).
    pub hash_byte: u8,
    /// Full 32-byte hash; present only in the full layout.
    pub hash: Option<&'a [u8; 32]>,
    pub flags: u8,
    pub clock: Clock,
    pub userval: &'a [u8],
}

impl<'a> EntVal<'a> {
    pub fn is_tombstone(&self) -> bool {
        self.flags & ENT_FLAG_TOMBSTONE != 0
    }

    /// Decode a stored leaf value.
    pub fn decode(stored: &'a [u8]) -> Result<EntVal<'a>> {
        if stored.len() >= ENT_HEADER_FULL {
            let hash: &[u8; 32] = stored[..32]
                .try_into()
                .expect("checked fixed-width field has the destination width");
            let flags = stored[32];
            if flags & !ENT_FLAGS_KNOWN != 0 {
                return Err(Error::Corrupt);
            }
            let clock =
                Clock::decode(&stored[33..41]).expect("clock field has exactly eight bytes");
            Ok(EntVal {
                hash_byte: hash[0],
                hash: Some(hash),
                flags,
                clock,
                userval: &stored[41..],
            })
        } else if stored.len() >= ENT_HEADER_COMPACT {
            let flags = stored[1];
            if flags & !ENT_FLAGS_KNOWN != 0 {
                return Err(Error::Corrupt);
            }
            let clock = Clock::decode(&stored[2..10]).expect("clock field has exactly eight bytes");
            Ok(EntVal { hash_byte: stored[0], hash: None, flags, clock, userval: &stored[10..] })
        } else {
            Err(Error::Corrupt)
        }
    }

    /// Compare two entries for user-visible equality: same liveness and, if
    /// live, same userval. Clocks are deliberately ignored so that identical
    /// data written at different times does not produce sync deltas.
    ///
    /// For full-layout entries the 32-byte hashes are compared instead of
    /// the uservals; this is also what makes over-the-wire header-only
    /// comparison possible.
    pub fn content_eq(&self, other: &EntVal<'_>) -> bool {
        if self.is_tombstone() || other.is_tombstone() {
            return self.is_tombstone() == other.is_tombstone();
        }
        match (self.hash, other.hash) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.userval == other.userval,
            // Layout is a pure function of userval length, so a full entry
            // can never equal a compact one.
            _ => false,
        }
    }
}

/// Encode a syncable leaf entry value into `out` (which is cleared first).
/// `hash` must be `crate::hash::entry_hash(...)` for this key/flags/userval.
pub fn encode_ent_val(
    out: &mut alloc::vec::Vec<u8>,
    hash: &[u8; 32],
    flags: u8,
    clock: Clock,
    userval: &[u8],
) {
    out.clear();
    if userval.len() > ENT_USERVAL_COMPACT_MAX {
        out.reserve(ENT_HEADER_FULL + userval.len());
        out.extend_from_slice(hash);
    } else {
        out.reserve(ENT_HEADER_COMPACT + userval.len());
        out.push(hash[0]);
    }
    out.push(flags);
    out.extend_from_slice(&clock.encode());
    out.extend_from_slice(userval);
}

/// Database metadata, stored at [`META_KEY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// True when the database maintains the prolly tree, clocks, and hashes
    /// required for synchronization.
    pub syncable: bool,
    /// Expected tree fan-out (syncable databases; 2..=128).
    pub branch_factor: u8,
    /// Random identity of this replica. Local diagnostics only: it is not
    /// part of any hash or the sync protocol.
    pub replica_id: u64,
    /// Clock of the most recently committed write transaction (updated on
    /// every syncable commit).
    pub clock: Clock,
    /// Keyed-BLAKE3 key shared by every replica of one database.
    pub hash_key: [u8; 32],
}

pub const META_MAGIC: [u8; 4] = *b"kvdb";
pub const META_VERSION: u8 = 1;
pub const META_SIZE: usize = 56;
const META_FLAG_SYNCABLE: u8 = 1 << 0;

impl Metadata {
    pub fn encode(&self) -> [u8; META_SIZE] {
        let mut out = [0u8; META_SIZE];
        out[0..4].copy_from_slice(&META_MAGIC);
        out[4] = META_VERSION;
        out[5] = if self.syncable { META_FLAG_SYNCABLE } else { 0 };
        out[6] = self.branch_factor;
        // out[7] reserved
        out[8..16].copy_from_slice(&self.replica_id.to_le_bytes());
        out[16..24].copy_from_slice(&self.clock.encode());
        out[24..56].copy_from_slice(&self.hash_key);
        out
    }

    pub fn decode(stored: &[u8]) -> Result<Metadata> {
        if stored.len() != META_SIZE || stored[0..4] != META_MAGIC {
            return Err(Error::Corrupt);
        }
        if stored[4] != META_VERSION || stored[5] & !META_FLAG_SYNCABLE != 0 || stored[7] != 0 {
            return Err(Error::Version);
        }
        let syncable = stored[5] & META_FLAG_SYNCABLE != 0;
        let branch_factor = stored[6];
        if syncable && !(2..=128).contains(&branch_factor) {
            return Err(Error::Corrupt);
        }
        Ok(Metadata {
            syncable,
            branch_factor,
            replica_id: u64::from_le_bytes(
                stored[8..16]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"),
            ),
            clock: Clock::decode(&stored[16..24]).expect("clock field has exactly eight bytes"),
            hash_key: stored[24..56]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        })
    }
}

#[cfg(test)]
mod tests;
