//! kvdb — a local-first key-value store for Playbit Core.
//!
//! kvdb provides the Core v1 application-private transactional ordered
//! key-value store, with an optional per-database "syncable" mode that
//! maintains a prolly tree (content-defined-chunked Merkle B-tree) with
//! hybrid-logical-clock versioning, enabling efficient replica
//! synchronization over any byte transport.
//!
//! The crate is `no_std + alloc` and sans-io: it performs no I/O, spawns no
//! threads, and reads no clocks. Storage backends implement [`Storage`], and
//! the embedding runtime supplies wall-clock time and randomness at the few
//! call sites that need them.

#![cfg_attr(not(feature = "redb"), no_std)]
// Production is entirely safe. The test-only redb preallocation benchmark
// needs a local FFI exception; Rust forbids overriding a `forbid` lint.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]
#![deny(clippy::unwrap_used)]

extern crate alloc;
#[cfg(test)]
extern crate std;

/// The durable native storage backend, built on redb. Needs `std`.
#[cfg(feature = "redb")]
pub mod redb;

pub mod clock;
pub mod db;
pub mod encoding;
pub mod error;
pub mod hash;
pub mod storage;
pub mod sync;
mod tree;

pub use clock::Clock;
pub use db::{CreateOptions, Db, LocalIndexes, OpenOptions, ReadTxn, WriteTxn};
pub use error::Error;
pub use storage::{Storage, StorageTxn, ValueRead};
pub use sync::{
    CommittedDelta, DeltaReceiver, Lww, Resolver, SyncDelta, SyncEntState, SyncOutcome,
    SyncReceiver, SyncResolver, SyncSender, SyncStaging,
};

/// Maximum key size in bytes.
pub const KEY_SIZE_MAX: usize = 511;

/// Maximum value size in bytes.
///
/// This is the largest value guaranteed to fit in a single sync frame:
/// the frame hard cap ([`sync::MAX_FRAME`]) minus the frame header, the
/// `NodesResponse` payload header, the per-entry length fields, a
/// maximum-size key, and the stored entry header. A compile-time
/// assertion in the sync module pins this arithmetic to the wire format.
pub const VALUE_SIZE_MAX: usize = sync::MAX_FRAME
    - 8 // frame header
    - 8 // NodesResponse payload header
    - (2 + 4) // per-entry key/value length fields
    - KEY_SIZE_MAX
    - encoding::ENT_HEADER_FULL;
