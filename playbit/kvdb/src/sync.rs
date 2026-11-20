//! Replica synchronization: sans-io, pull-based, resumable.
//!
//! Sync runs between exactly two peers over any reliable ordered byte
//! transport. It is asymmetric: the **receiver** pulls and merges, while
//! the **sender** answers from a read snapshot. The immediate `SyncReceiver`
//! holds a writer; `PreparedSync` receives without one and merges afterward.
//! Bidirectional sync is two sessions.
//!
//! Both sides are pure state machines: the application moves bytes between
//! `process()` calls and its transport, and may suspend/resume at any
//! fragment boundary. Transport I/O and clocks belong to the caller;
//! storage and staging operations use the caller-supplied backends.
//!
//! ## Protocol (version 1)
//!
//! Frames are `[size: u32][type: u16][flags: u16]` + payload, all integers
//! little-endian, `size` including the 8-byte header. Types:
//!
//! 1. `PullRequest` — receiver → sender: protocol version, branch factor,
//!    hash-key fingerprint, receiver's root.
//! 2. `PullResponse` — sender's status (differs / no difference / empty /
//!    snapshot / error), root, and db clock. Snapshot is followed immediately
//!    by a RangeResponse covering all leaves.
//! 3. `NodesRequest` — `[count:u16, reserved:u16=0]` followed by `count`
//!    `[level:u8, reserved:u8=0, key_len:u16, key]` descriptors. A single
//!    descriptor can name any branch; batches name at most REQUEST_BATCH_MAX
//!    level-1 branches in ascending key order. Responses follow that order.
//! 4. `NodesResponse` — the chunk's members in key order: 32-byte hashes for
//!    branch children, verbatim stored entries for leaf children. Large
//!    chunks span several frames chained with the MORE flag.
//! 5. `Bye` — receiver → sender: session complete.
//! 7. `RangeResponse` — same entry framing as NodesResponse, always level 0,
//!    but covers the entire requested subtree's key range. One response kind
//!    must be used throughout a span. Type 6 belongs to pushed deltas.
//!
//! An empty receiver gets a streamed snapshot in the first response. A small
//! differing database/subtree (encoded size <= SMALL_RANGE_BYTES) also streams
//! directly, avoiding discovery. Probing reads sizes only, stops at the byte
//! threshold, and retains no values. Snapshots have no aggregate entry cap:
//! each frame and retained parser/staging state remain bounded independently.
//!
//! ## Walk
//!
//! The receiver walks the sender's tree depth-first, batching adjacent leaf
//! spans on the same ordered stream. Parents are discovered one at a time so
//! leaf resolution remains globally sorted without buffering/reordering values. Each remote branch
//! node is compared with the local branch at the *same* storage key: equal
//! hashes prove the subtree's members identical. It is skipped only if the
//! local and remote spans also end at the same key; otherwise its children
//! are fetched so local-only chunk heads cannot escape comparison. Chunk spans
//! partition the key space and a chunk's span is `[its head key, next
//! sibling's key)`, the receiver derives every span bound from the parent's
//! child list. The sender derives the same bounds from its pinned snapshot. At level 1 the
//! remote chunk's leaves are merge-joined against the local snapshot within
//! the span, and every difference is put to the [`Resolver`].
//!
//! Local reads use a stable snapshot taken at session start; merge writes
//! go to a write transaction created at the same instant for `SyncReceiver`.
//! `PreparedSync` instead journals differences without a writer and later
//! resolves them against current data in a separate live transaction.
//! Committing the immediate receiver is explicit via
//! [`SyncReceiver::finish`]. Applied entries keep their *originating*
//! clocks, which keeps last-writer-wins convergent across any number of
//! replicas and relay paths.

use crate::clock::Clock;
use crate::db::{Db, ReadTxn, WriteTxn};
use crate::encoding::{self, ENT_FLAG_TOMBSTONE, EntVal};
use crate::error::{Error, Result};
use crate::storage::ValueRead;
use crate::storage::{Storage, StorageTxn};
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

pub const PROTOCOL_VERSION: u16 = 1;
/// Stable baseline algorithm identity, independent of storage format/build.
pub const PULL_ALGORITHM: &str = "kvdb.pull.v1";
pub const DELTA_ALGORITHM: &str = "kvdb.delta.v1";

/// Send a divergent range directly when its encoded entries fit in 16 KiB.
/// This bounds speculative scanning and extra traffic, not streaming snapshots.
pub const SMALL_RANGE_BYTES: usize = 16 * 1024;
/// Adjacent leaf spans share a request/response flight, in key order.
pub const REQUEST_BATCH_MAX: usize = 32;
/// One batch header plus bounded (level, padding, key length, key) descriptors.
pub const MAX_REQUEST_SIZE: usize = 12 + REQUEST_BATCH_MAX * (4 + crate::KEY_SIZE_MAX);
const _: () = assert!(MAX_REQUEST_SIZE <= DEFAULT_MSG_CAP);

/// Maximum emitted transport fragment and retained branch-span payload.
/// Large logical frames cross output calls without changing wire framing.
pub const DEFAULT_MSG_CAP: usize = 256 * 1024;
/// Hard cap on logical frame size, emitted and accepted. [`crate::VALUE_SIZE_MAX`] is derived from it so that a
/// maximum-size entry always fits in one `NodesResponse` frame.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;
const FRAME_HEADER: usize = 8;

// Pin VALUE_SIZE_MAX to the wire format: one frame must carry exactly one
// maximum-size entry (frame header + payload header + entry length fields
// + max key + stored entry header + max value).
const _: () = assert!(
    FRAME_HEADER
        + 8
        + (2 + 4)
        + crate::KEY_SIZE_MAX
        + crate::encoding::ENT_HEADER_FULL
        + crate::VALUE_SIZE_MAX
        == MAX_FRAME
);

const T_PULL_REQ: u16 = 1;
const T_PULL_RES: u16 = 2;
const T_NODES_REQ: u16 = 3;
const T_NODES_RES: u16 = 4;
const T_BYE: u16 = 5;
const T_DELTA: u16 = 6;
const T_RANGE_RES: u16 = 7;

const DELTA_HEADER: usize = 32;
const F_MORE: u16 = 1;

const S_DIFF: u8 = 0;
const S_NODIFF: u8 = 1;
const S_EMPTY: u8 = 2;
const S_SNAPSHOT: u8 = 3;
const S_ERR_VERSION: u8 = 0x20;
const S_ERR_HASHKEY: u8 = 0x21;
const S_ERR_BRANCH_FACTOR: u8 = 0x22;

/// Result of a `process()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// The session needs more input (or its output delivered).
    Running,
    /// The session is complete; a receiver should call `finish()`.
    Done,
}

/// How a completed receiver session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEnd {
    /// Roots were identical; nothing exchanged beyond the handshake.
    NoDifference,
    /// The remote database is empty; the resolver saw local keys as remote-missing.
    RemoteEmpty,
    /// Trees differed; differences were resolved (see the counts).
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    pub end: SyncEnd,
    /// Differences surfaced to the resolver.
    pub deltas: usize,
    /// Differences applied locally (resolver chose the remote side).
    pub applied: usize,
    /// Whether a merge transaction was committed.
    pub committed: bool,
}

/// One side of a difference between the replicas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntState<'a> {
    /// No trace of the key.
    Missing,
    /// A tombstone: deleted at `clock`.
    Deleted {
        clock: Clock,
    },
    Present {
        value: &'a [u8],
        clock: Clock,
    },
}

/// A difference discovered during sync, put to the [`Resolver`].
#[derive(Debug, Clone, Copy)]
pub struct Delta<'a> {
    pub key: &'a [u8],
    pub local: EntState<'a>,
    pub remote: EntState<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Make the local entry match the remote side (applied with the
    /// remote's original clock).
    UseRemote,
    /// Keep the local state unchanged.
    KeepLocal,
}

/// Decides each difference during a receiver session.
pub trait Resolver {
    fn resolve(&mut self, delta: &Delta<'_>) -> Resolution;
}

/// Default resolver: last writer wins on HLC clocks, with a deterministic,
/// replica-symmetric content tiebreak for exactly equal clocks (present
/// beats deleted, then larger value bytes win). A remote with no trace of a
/// key never removes local data.
pub struct Lww;

impl Resolver for Lww {
    fn resolve(&mut self, delta: &Delta<'_>) -> Resolution {
        // Total order over states: clock first, then liveness (present
        // beats deleted beats missing), then value bytes. Real entries
        // never carry the ZERO clock, so a missing side always ranks
        // lowest and can never displace local data.
        fn rank<'a>(s: &EntState<'a>) -> (Clock, u8, &'a [u8]) {
            match s {
                EntState::Missing => (Clock::ZERO, 0, &[]),
                EntState::Deleted { clock } => (*clock, 1, &[]),
                EntState::Present { value, clock } => (*clock, 2, value),
            }
        }
        if rank(&delta.remote) > rank(&delta.local) {
            Resolution::UseRemote
        } else {
            Resolution::KeepLocal
        }
    }
}

impl<F: FnMut(&Delta<'_>) -> Resolution> Resolver for F {
    fn resolve(&mut self, delta: &Delta<'_>) -> Resolution {
        self(delta)
    }
}

// ---------------------------------------------------------------- framing

/// Fingerprint sent in the handshake so mismatched hash keys fail fast.
/// The fixed context string domain-separates this from every other keyed
/// hash in the system, so the cleartext fingerprint can never be the
/// prefix of an entry or branch hash.
fn hashkey_fingerprint(hash_key: &[u8; 32]) -> [u8; 8] {
    let mut h = blake3::Hasher::new_keyed(hash_key);
    h.update(b"kvdb hashkey fingerprint");
    h.finalize().as_bytes()[..8]
        .try_into()
        .expect("checked fixed-width field has the destination width")
}

/// Accumulates transport bytes and yields complete frames.
#[derive(Default)]
struct Framer {
    buf: Vec<u8>,
    off: usize,
}

impl Framer {
    fn push(&mut self, data: &[u8]) {
        assert!(self.off <= self.buf.len(), "framer cursor stays within retained bytes");
        // (next() resets the buffer whenever it is fully consumed, so no
        // compaction is needed here.)
        self.buf.extend_from_slice(data);
    }

    /// Next complete frame as `(type, flags, payload)`, copied out.
    fn next(&mut self) -> Result<Option<(u16, u16, Vec<u8>)>> {
        assert!(self.off <= self.buf.len(), "consumed bytes cannot exceed framed input");
        let avail = &self.buf[self.off..];
        if avail.len() < FRAME_HEADER {
            return Ok(None);
        }
        let size = u32::from_le_bytes(
            avail[0..4].try_into().expect("checked fixed-width field has the destination width"),
        ) as usize;
        if !(FRAME_HEADER..=MAX_FRAME).contains(&size) {
            return Err(Error::Protocol);
        }
        if avail.len() < size {
            return Ok(None);
        }
        let typ = u16::from_le_bytes(
            avail[4..6].try_into().expect("checked fixed-width field has the destination width"),
        );
        let flags = u16::from_le_bytes(
            avail[6..8].try_into().expect("checked fixed-width field has the destination width"),
        );
        let payload = avail[FRAME_HEADER..size].to_vec();
        self.off += size;
        if self.off == self.buf.len() {
            self.buf.clear();
            self.off = 0;
        }
        Ok(Some((typ, flags, payload)))
    }
}

fn reserve_sync_output(out: &mut Vec<u8>, additional: usize) -> Result<()> {
    if out.len().checked_add(additional).is_none_or(|n| n > DEFAULT_MSG_CAP) {
        return Err(Error::Capacity);
    }
    out.try_reserve_exact(additional).map_err(|_| Error::Capacity)
}

fn frame_begin(out: &mut Vec<u8>, typ: u16, flags: u16) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&typ.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    at
}

fn frame_end(out: &mut [u8], at: usize) {
    assert!(at <= out.len(), "frame starts within its output buffer");
    assert!(out.len() - at >= FRAME_HEADER, "frame_begin reserved the header");
    assert!(out.len() - at <= MAX_FRAME, "encoded frame respects the wire cap");
    let size = (out.len() - at) as u32;
    out[at..at + 4].copy_from_slice(&size.to_le_bytes());
}

/// Bounds-checked payload reader.
struct Rd<'a> {
    p: &'a [u8],
    off: usize,
}

impl<'a> Rd<'a> {
    fn new(p: &'a [u8]) -> Rd<'a> {
        Rd { p, off: 0 }
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.off.checked_add(n).ok_or(Error::Protocol)?;
        let s = self.p.get(self.off..end).ok_or(Error::Protocol)?;
        self.off = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.bytes(2)?.try_into().expect("checked fixed-width field has the destination width"),
        ))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("checked fixed-width field has the destination width"),
        ))
    }
    fn done(&self) -> Result<()> {
        if self.off == self.p.len() { Ok(()) } else { Err(Error::Protocol) }
    }
}

fn decode_ent_state(stored: &[u8]) -> Result<EntState<'_>> {
    let ent = EntVal::decode(stored)?;
    Ok(if ent.is_tombstone() {
        EntState::Deleted { clock: ent.clock }
    } else {
        EntState::Present { value: ent.userval, clock: ent.clock }
    })
}

/// Decode an entry received from the peer, holding it to what this replica
/// could have written itself: a value no larger than [`crate::VALUE_SIZE_MAX`]
/// (so the entry can be served onward in one frame) and a real clock (the
/// zero clock is the "no clock" sentinel that ranks below every entry).
fn remote_ent(stored: &[u8]) -> Result<EntVal<'_>> {
    let ent = EntVal::decode(stored).map_err(|_| Error::Protocol)?;
    if ent.userval.len() > crate::VALUE_SIZE_MAX
        || ent.clock.is_zero()
        || ent.flags & !ENT_FLAG_TOMBSTONE != 0
        || (ent.is_tombstone() && !ent.userval.is_empty())
    {
        return Err(Error::Protocol);
    }
    Ok(ent)
}

fn remote_ent_state(stored: &[u8]) -> Result<EntState<'_>> {
    let ent = remote_ent(stored)?;
    Ok(if ent.is_tombstone() {
        EntState::Deleted { clock: ent.clock }
    } else {
        EntState::Present { value: ent.userval, clock: ent.clock }
    })
}

/// Random-access staging for one encoded remote entry. Implementations own
/// their backing resource and discard it on drop. Native hosts use an unlinked
/// file; OPFS hosts may use a worker-owned file. No whole-value RAM staging.
pub trait SyncStaging: crate::storage::ValueRead {
    fn clear(&mut self) -> Result<()>;
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
}

#[derive(Clone, Copy)]
pub enum SyncEntState<'a> {
    Missing,
    Deleted { clock: Clock },
    Present { value: &'a dyn crate::storage::ValueRead, clock: Clock },
}
pub struct SyncDelta<'a> {
    pub key: &'a [u8],
    pub local: SyncEntState<'a>,
    pub remote: SyncEntState<'a>,
}
/// Full-sync resolution can read either value without materializing it.
pub trait SyncResolver {
    fn resolve(&mut self, delta: &SyncDelta<'_>) -> Result<Resolution>;
}
impl<F: FnMut(&SyncDelta<'_>) -> Result<Resolution>> SyncResolver for F {
    fn resolve(&mut self, delta: &SyncDelta<'_>) -> Result<Resolution> {
        self(delta)
    }
}
impl SyncResolver for Lww {
    fn resolve(&mut self, delta: &SyncDelta<'_>) -> Result<Resolution> {
        fn rank(state: SyncEntState<'_>) -> (Clock, u8) {
            match state {
                SyncEntState::Missing => (Clock::ZERO, 0),
                SyncEntState::Deleted { clock } => (clock, 1),
                SyncEntState::Present { clock, .. } => (clock, 2),
            }
        }
        let mut order = rank(delta.remote).cmp(&rank(delta.local));
        if order.is_eq()
            && let (SyncEntState::Present { value: a, .. }, SyncEntState::Present { value: b, .. }) =
                (delta.remote, delta.local)
        {
            order = compare_values(a, b)?;
        }
        Ok(if order.is_gt() { Resolution::UseRemote } else { Resolution::KeepLocal })
    }
}
fn compare_values(
    a: &dyn crate::storage::ValueRead,
    b: &dyn crate::storage::ValueRead,
) -> Result<core::cmp::Ordering> {
    let mut aa = [0; 4096];
    let mut bb = [0; 4096];
    let mut offset = 0;
    while offset < a.len().min(b.len()) {
        let n = aa.len().min(a.len().min(b.len()) - offset);
        a.read_exact_at(offset, &mut aa[..n])?;
        b.read_exact_at(offset, &mut bb[..n])?;
        let order = aa[..n].cmp(&bb[..n]);
        if !order.is_eq() {
            return Ok(order);
        }
        offset += n;
    }
    Ok(a.len().cmp(&b.len()))
}

struct ValueSlice<'a> {
    source: &'a dyn crate::storage::ValueRead,
    start: usize,
}
impl crate::storage::ValueRead for ValueSlice<'_> {
    fn len(&self) -> usize {
        self.source.len() - self.start
    }
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<usize> {
        if offset > self.len() {
            return Err(Error::InvalidArgument);
        }
        self.source.read_at(self.start + offset, out)
    }
}
struct StreamEntry<'a> {
    value: ValueSlice<'a>,
    clock: Clock,
    flags: u8,
    header: [u8; 41],
    header_len: usize,
}
impl<'a> StreamEntry<'a> {
    fn new(source: &'a dyn crate::storage::ValueRead) -> Result<Self> {
        let n = source.len().min(encoding::ENT_HEADER_FULL);
        let mut header = [0; 41];
        source.read_exact_at(0, &mut header[..n])?;
        let ent = EntVal::decode(&header[..n]).map_err(|_| Error::Protocol)?;
        let start = if source.len() >= encoding::ENT_HEADER_FULL {
            encoding::ENT_HEADER_FULL
        } else {
            encoding::ENT_HEADER_COMPACT
        };
        let len = source.len() - start;
        if len > crate::VALUE_SIZE_MAX
            || ent.clock.is_zero()
            || ent.flags & !ENT_FLAG_TOMBSTONE != 0
            || (ent.is_tombstone() && len != 0)
        {
            return Err(Error::Protocol);
        }
        Ok(Self {
            value: ValueSlice { source, start },
            clock: ent.clock,
            flags: ent.flags,
            header,
            header_len: n,
        })
    }
    fn state(&self) -> SyncEntState<'_> {
        if self.flags & ENT_FLAG_TOMBSTONE != 0 {
            SyncEntState::Deleted { clock: self.clock }
        } else {
            SyncEntState::Present { value: &self.value, clock: self.clock }
        }
    }
    fn same(&self, other: &Self) -> Result<bool> {
        Ok(self.clock == other.clock
            && EntVal::decode(&self.header[..self.header_len])?
                .content_eq(&EntVal::decode(&other.header[..other.header_len])?))
    }
}
mod prepared;
pub use prepared::{PreparedChanges, PreparedInspection, PreparedMerge, PreparedSync};

// --------------------------------------------------------------- receiver
struct Pending {
    level: u8,
    ukey: Vec<u8>,
    hash: [u8; 32],
    stop: Option<Vec<u8>>,
}

impl Pending {
    fn assert_valid(&self) {
        assert!(self.level > 0, "only branches are queued for descent");
        assert!(self.level <= encoding::LEVEL_MAX, "queued level was validated");
        assert!(self.ukey.len() <= crate::KEY_SIZE_MAX, "queued key was validated");
        assert!(
            self.stop.as_ref().is_none_or(|stop| self.ukey < *stop),
            "pending subtree has a strictly ordered exclusive bound"
        );
    }
}
enum RState {
    AwaitPullRes,
    AwaitNodes {
        kind: u16,
        child_level: u8,
        cursor: Vec<u8>,
        exclusive: bool,
        stop: Option<Vec<u8>>,
        last: Option<Vec<u8>>,
        acc: Vec<(Vec<u8>, Vec<u8>)>,
    },
    Done(SyncEnd),
    Failed,
}
#[derive(Default)]
struct ReceiveFrame {
    header: [u8; 16],
    have: usize,
    remaining: usize,
    typ: u16,
    flags: u16,
    count: u32,
    key_len: usize,
    value_len: usize,
    entry_header: [u8; 6],
    entry_have: usize,
    key: Vec<u8>,
    control: Vec<u8>,
    active: bool,
}

/// Full receipt keeps only four key bounds, not a mutation/value list. The
/// interval comes from the current repaired write tree; resolver/protocol
/// reads continue to use the receiver's immutable starting snapshot.
#[derive(Default)]
struct PendingRepair {
    interval: Option<(Vec<u8>, Option<Vec<u8>>)>,
    lo: Vec<u8>,
    hi: Vec<u8>,
    count: u8,
    stable: bool,
}

const REPAIR_CHANGE_MAX: u8 = 32;
const PENDING_BRANCH_MAX: usize = 256;
const SPAN_ENTRY_MAX: usize = 4096;

impl PendingRepair {
    fn assert_valid(&self) {
        assert!(self.count <= REPAIR_CHANGE_MAX, "deferred repair respects its change cap");
        if self.count != 0 {
            let (lower, upper) = self.interval.as_ref().expect("changes have a prepared interval");
            assert!(self.lo <= self.hi, "dirty key bounds are ordered");
            assert!(self.lo >= *lower, "dirty window starts in its prepared interval");
            assert!(
                upper.as_ref().is_none_or(|end| self.hi < *end),
                "dirty window ends inside its interval"
            );
            assert!(!self.stable || self.count == 1, "stable boundary shortcut covers one change");
        }
    }

    fn prepare<S: Storage>(&mut self, txn: &mut WriteTxn<'_, S>, key: &[u8]) -> Result<()> {
        self.assert_valid();
        if self.count != 0
            && (self.count == REPAIR_CHANGE_MAX
                || self.interval.as_ref().is_some_and(|(lo, hi)| {
                    key < lo.as_slice() || hi.as_ref().is_some_and(|hi| key >= hi.as_slice())
                }))
        {
            self.flush(txn)?;
        }
        if self.count == 0 {
            // Drop an unused interval before its replacement is allocated.
            self.interval = None;
            self.interval = Some(txn.sync_leaf_interval(key)?);
        }
        Ok(())
    }

    fn changed(&mut self, key: &[u8], stable: bool) {
        self.assert_valid();
        assert!(self.count < REPAIR_CHANGE_MAX, "prepare flushes before the repair batch is full");
        assert!(self.interval.is_some(), "prepare establishes the interval before mutation");
        if self.count == 0 {
            self.lo = key.to_vec();
            self.hi = key.to_vec();
            self.stable = stable;
        } else {
            if key < self.lo.as_slice() {
                self.lo = key.to_vec();
            }
            if key > self.hi.as_slice() {
                self.hi = key.to_vec();
            }
            self.stable = false;
        }
        self.count += 1;
        self.assert_valid();
    }

    fn flush<S: Storage>(&mut self, txn: &mut WriteTxn<'_, S>) -> Result<()> {
        self.assert_valid();
        if self.count != 0 {
            txn.sync_repair(&self.lo, &self.hi, self.stable)?;
        }
        *self = Self::default();
        Ok(())
    }
}

/// Full-sync pull receiver. Each remote entry is staged separately; branch
/// metadata and traversal are bounded independently of logical frame size.
pub struct SyncReceiver<'db, S: Storage, R: SyncResolver, B: SyncStaging> {
    snapshot: ReadTxn<'db, S>,
    wtxn: Option<WriteTxn<'db, S>>,
    observed_clock: Clock,
    resolver: R,
    staging: B,
    frame: ReceiveFrame,
    state: RState,
    pending: Vec<Pending>,
    requested: VecDeque<Pending>,
    deltas: usize,
    applied: usize,
    span_bytes: usize,
    span_entries: usize,
    repair: PendingRepair,
}
impl<'db, S: Storage, R: SyncResolver, B: SyncStaging> SyncReceiver<'db, S, R, B> {
    pub fn new(
        db: &'db Db<S>,
        resolver: R,
        staging: B,
        wall_unix_secs: u64,
        out: &mut Vec<u8>,
    ) -> Result<Self> {
        if !db.meta().syncable {
            return Err(Error::Mode);
        }
        let wtxn = db.begin_write(wall_unix_secs)?;
        Self::start(db, resolver, staging, Some(wtxn), out)
    }
    fn start(
        db: &'db Db<S>,
        resolver: R,
        staging: B,
        wtxn: Option<WriteTxn<'db, S>>,
        out: &mut Vec<u8>,
    ) -> Result<Self> {
        let snapshot = db.begin_read()?;
        let meta = db.meta();
        let root = snapshot.root_hash()?;
        reserve_sync_output(out, 52)?;
        let at = frame_begin(out, T_PULL_REQ, 0);
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out.push(meta.branch_factor);
        out.push(root.map_or(0, |(level, _)| level));
        out.extend_from_slice(&hashkey_fingerprint(&meta.hash_key));
        out.extend_from_slice(&root.map_or([0; 32], |(_, h)| h));
        frame_end(out, at);
        Ok(Self {
            snapshot,
            wtxn,
            observed_clock: Clock::ZERO,
            resolver,
            staging,
            frame: ReceiveFrame::default(),
            state: RState::AwaitPullRes,
            pending: Vec::new(),
            requested: VecDeque::new(),
            deltas: 0,
            applied: 0,
            span_bytes: 0,
            span_entries: 0,
            repair: PendingRepair::default(),
        })
    }
    pub fn new_with_delta(
        db: &'db Db<S>,
        resolver: R,
        staging: B,
        wall: u64,
        max_bytes: usize,
        out: &mut Vec<u8>,
    ) -> Result<Self> {
        let mut receiver = Self::new(db, resolver, staging, wall, out)?;
        receiver.wtxn.as_mut().ok_or(Error::Mode)?.capture_changes(max_bytes);
        Ok(receiver)
    }
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<SyncStatus> {
        if matches!(self.state, RState::Failed) {
            return Err(Error::Protocol);
        }
        self.repair.assert_valid();
        assert!(
            self.requested.len() < REQUEST_BATCH_MAX,
            "one response is active outside the queue"
        );
        assert!(self.pending.len() <= PENDING_BRANCH_MAX, "receiver traversal stays bounded");
        assert!(self.span_entries <= SPAN_ENTRY_MAX, "accepted entries stay within the span cap");
        let result = self.process_inner(input, out);
        if result.is_err() {
            self.state = RState::Failed;
        }
        result
    }
    fn process_inner(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<SyncStatus> {
        while !input.is_empty() {
            if matches!(self.state, RState::Done(_)) {
                return Err(Error::Protocol);
            }
            if !self.frame.active && !self.receive_frame_header(&mut input)? {
                continue;
            }
            if self.frame.typ == T_PULL_RES {
                self.receive_pull_payload(&mut input, out)?;
                continue;
            }
            if self.frame.have < 16 && !self.receive_nodes_header(&mut input)? {
                continue;
            }
            if self.frame.count > 0 {
                self.receive_entry(&mut input)?;
            }
            self.complete_nodes_frame(out)?;
        }
        Ok(if matches!(self.state, RState::Done(_)) {
            SyncStatus::Done
        } else {
            SyncStatus::Running
        })
    }
    fn receive_frame_header(&mut self, input: &mut &[u8]) -> Result<bool> {
        let n = (8 - self.frame.have).min(input.len());
        self.frame.header[self.frame.have..self.frame.have + n].copy_from_slice(&input[..n]);
        self.frame.have += n;
        *input = &input[n..];
        if self.frame.have < 8 {
            return Ok(false);
        }
        let size = u32::from_le_bytes(
            self.frame.header[..4]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        ) as usize;
        if !(8..=MAX_FRAME).contains(&size) {
            return Err(Error::Protocol);
        }
        self.frame.typ = u16::from_le_bytes(
            self.frame.header[4..6]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        );
        self.frame.flags = u16::from_le_bytes(
            self.frame.header[6..8]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        );
        self.frame.remaining = size - 8;
        self.frame.active = true;
        match (&mut self.state, self.frame.typ) {
            (RState::AwaitPullRes, T_PULL_RES) if size == 52 && self.frame.flags == 0 => {}
            (RState::AwaitNodes { kind, child_level, .. }, T_NODES_RES | T_RANGE_RES)
                if size >= 16 && self.frame.flags & !F_MORE == 0 =>
            {
                if *kind != 0 && *kind != self.frame.typ {
                    return Err(Error::Protocol);
                }
                *kind = self.frame.typ;
                if *kind == T_RANGE_RES {
                    *child_level = 0;
                }
            }
            _ => return Err(Error::Protocol),
        }
        Ok(true)
    }
    fn receive_pull_payload(&mut self, input: &mut &[u8], out: &mut Vec<u8>) -> Result<()> {
        let n = input.len().min(self.frame.remaining);
        self.frame.control.extend_from_slice(&input[..n]);
        self.frame.remaining -= n;
        *input = &input[n..];
        if self.frame.remaining == 0 {
            let payload = core::mem::take(&mut self.frame.control);
            self.pull_response(&payload, out)?;
            self.frame = ReceiveFrame::default();
        }
        Ok(())
    }
    fn receive_nodes_header(&mut self, input: &mut &[u8]) -> Result<bool> {
        let n = (16 - self.frame.have).min(input.len()).min(self.frame.remaining);
        self.frame.header[self.frame.have..self.frame.have + n].copy_from_slice(&input[..n]);
        self.frame.have += n;
        self.frame.remaining -= n;
        *input = &input[n..];
        if self.frame.have < 16 {
            return Ok(false);
        }
        let RState::AwaitNodes { child_level, .. } = self.state else {
            return Err(Error::Protocol);
        };
        if self.frame.header[8] != child_level {
            return Err(Error::Protocol);
        }
        self.frame.count = u32::from_le_bytes(
            self.frame.header[12..16]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        );
        // A streamed range retains no entry list; bound each frame, while
        // ordinary chunks also retain their aggregate entry limit.
        let limit = if self.frame.typ == T_RANGE_RES {
            SPAN_ENTRY_MAX
        } else {
            SPAN_ENTRY_MAX - self.span_entries
        };
        if self.frame.count as usize > limit {
            return Err(Error::Capacity);
        }
        Ok(true)
    }
    fn receive_entry(&mut self, input: &mut &[u8]) -> Result<()> {
        if self.frame.entry_have < 6 {
            let n = (6 - self.frame.entry_have).min(input.len()).min(self.frame.remaining);
            self.frame.entry_header[self.frame.entry_have..self.frame.entry_have + n]
                .copy_from_slice(&input[..n]);
            self.frame.entry_have += n;
            self.frame.remaining -= n;
            *input = &input[n..];
            if self.frame.entry_have == 6 {
                self.start_entry()?;
            }
        }
        if self.frame.entry_have == 6 && self.frame.key.len() < self.frame.key_len {
            let n = (self.frame.key_len - self.frame.key.len()).min(input.len());
            self.frame.key.extend_from_slice(&input[..n]);
            self.frame.remaining -= n;
            *input = &input[n..];
        }
        if self.frame.entry_have == 6 && self.frame.key.len() == self.frame.key_len {
            let n =
                (self.frame.value_len - self.staging.len()).min(input.len()).min(DEFAULT_MSG_CAP);
            self.staging.append(&input[..n])?;
            self.frame.remaining -= n;
            *input = &input[n..];
            if self.staging.len() == self.frame.value_len {
                let key = core::mem::take(&mut self.frame.key);
                self.accept_entry(&key)?;
                self.frame.count -= 1;
                self.frame.entry_have = 0;
            }
        }
        Ok(())
    }
    fn start_entry(&mut self) -> Result<()> {
        self.frame.key_len = u16::from_le_bytes(
            self.frame.entry_header[..2]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        ) as usize;
        self.frame.value_len = u32::from_le_bytes(
            self.frame.entry_header[2..]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        ) as usize;
        if self.frame.key_len > crate::KEY_SIZE_MAX
            || self
                .frame
                .key_len
                .checked_add(self.frame.value_len)
                .is_none_or(|n| n > self.frame.remaining)
        {
            return Err(Error::Protocol);
        }
        let RState::AwaitNodes { child_level, .. } = self.state else {
            return Err(Error::Protocol);
        };
        if (child_level == 0
            && (self.frame.key_len == 0
                || self.frame.value_len > crate::VALUE_SIZE_MAX + encoding::ENT_HEADER_FULL))
            || (child_level != 0 && self.frame.value_len != 32)
        {
            return Err(Error::Protocol);
        }
        self.staging.clear()?;
        Ok(())
    }
    fn complete_nodes_frame(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if self.frame.remaining == 0 {
            if self.frame.count != 0 || self.frame.entry_have != 0 {
                return Err(Error::Protocol);
            }
            let more = self.frame.flags & F_MORE != 0;
            self.frame = ReceiveFrame::default();
            if !more {
                self.end_span(out)?;
            }
        } else if self.frame.count == 0 {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    fn pull_response(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        assert!(matches!(self.state, RState::AwaitPullRes), "pull response follows the handshake");
        let mut rd = Rd::new(payload);
        let status = rd.u8()?;
        let level = rd.u8()?;
        rd.u16()?;
        let hash =
            rd.bytes(32)?.try_into().expect("checked fixed-width field has the destination width");
        let clock = Clock::decode(rd.bytes(8)?).expect("clock field has exactly eight bytes");
        rd.done()?;
        match status {
            S_NODIFF => self.end(SyncEnd::NoDifference, out),
            S_EMPTY => self.empty_remote(out),
            S_DIFF | S_SNAPSHOT => {
                if level == 0 || level > encoding::LEVEL_MAX {
                    return Err(Error::Protocol);
                }
                self.observed_clock = self.observed_clock.max(clock);
                if let Some(wtxn) = &mut self.wtxn {
                    wtxn.set_clock(wtxn.clock().recv(clock, 0)?)?;
                }
                if status == S_SNAPSHOT {
                    self.state = RState::AwaitNodes {
                        kind: T_RANGE_RES,
                        child_level: 0,
                        cursor: vec![0],
                        exclusive: false,
                        stop: None,
                        last: None,
                        acc: Vec::new(),
                    };
                    Ok(())
                } else {
                    self.pending.push(Pending { level, ukey: Vec::new(), hash, stop: None });
                    self.advance(out)
                }
            }
            S_ERR_VERSION => Err(Error::Version),
            S_ERR_HASHKEY => Err(Error::HashKeyMismatch),
            S_ERR_BRANCH_FACTOR => Err(Error::BranchFactorMismatch),
            _ => Err(Error::Remote),
        }
    }
    fn empty_remote(&mut self, out: &mut Vec<u8>) -> Result<()> {
        // An empty source is a complete missing-key interval. LWW keeps local
        // entries, while a qualified mirror resolver may explicitly forget them.
        self.state = RState::AwaitNodes {
            kind: T_RANGE_RES,
            child_level: 0,
            cursor: vec![0],
            exclusive: false,
            stop: None,
            last: None,
            acc: Vec::new(),
        };
        self.local_before(None)?;
        self.state = RState::AwaitPullRes;
        self.end(SyncEnd::RemoteEmpty, out)
    }
    fn accept_entry(&mut self, key: &[u8]) -> Result<()> {
        assert!(
            matches!(self.state, RState::AwaitNodes { .. }),
            "entries follow a subtree request"
        );
        let RState::AwaitNodes { child_level, cursor, last, stop, acc, .. } = &mut self.state
        else {
            return Err(Error::Protocol);
        };
        if key < &cursor[1..]
            || last.as_ref().is_some_and(|v| v.as_slice() >= key)
            || stop.as_ref().is_some_and(|v| key >= v.as_slice())
        {
            return Err(Error::Protocol);
        }
        *last = Some(key.to_vec());
        if self.frame.typ != T_RANGE_RES {
            self.span_entries += 1;
        }
        if *child_level != 0 {
            if self.staging.len() != 32 {
                return Err(Error::Protocol);
            }
            self.span_bytes += key.len() + 32;
            if self.span_bytes > DEFAULT_MSG_CAP
                || acc.len() + self.pending.len() >= PENDING_BRANCH_MAX
            {
                return Err(Error::Capacity);
            }
            let mut hash = vec![0; 32];
            self.staging.read_exact_at(0, &mut hash)?;
            acc.try_reserve_exact(1).map_err(|_| Error::Capacity)?;
            acc.push((key.to_vec(), hash));
            return Ok(());
        }
        self.accept_leaf(key)
    }
    fn accept_leaf(&mut self, key: &[u8]) -> Result<()> {
        if key.is_empty() {
            return Err(Error::Protocol);
        }
        self.local_before(Some(key))?;
        let mut skey = vec![0];
        skey.extend_from_slice(key);
        let source = self.snapshot.raw().snapshot_value(&skey)?;
        let local = source.as_ref().map(|v| StreamEntry::new(v)).transpose()?;
        let remote = StreamEntry::new(&self.staging)?;
        self.observed_clock = self.observed_clock.max(remote.clock);
        if let Some(wtxn) = &mut self.wtxn
            && remote.clock >= wtxn.clock()
        {
            wtxn.set_clock(wtxn.clock().recv(remote.clock, 0)?)?;
        }
        if !local.as_ref().map(|l| l.same(&remote)).transpose()?.unwrap_or(false) {
            self.deltas += 1;
            let delta = SyncDelta {
                key,
                local: local.as_ref().map_or(SyncEntState::Missing, StreamEntry::state),
                remote: remote.state(),
            };
            if self.resolver.resolve(&delta)? == Resolution::UseRemote {
                let wtxn = self.wtxn.as_mut().ok_or(Error::Mode)?;
                self.repair.prepare(wtxn, key)?;
                if let Some(stable) =
                    wtxn.sync_put_from_deferred(key, &remote.value, remote.flags, remote.clock)?
                {
                    self.repair.changed(key, stable);
                    self.applied += 1;
                }
            }
        }
        if let RState::AwaitNodes { cursor, exclusive, .. } = &mut self.state {
            *cursor = skey;
            *exclusive = true;
        }
        Ok(())
    }
    fn local_before(&mut self, before: Option<&[u8]>) -> Result<()> {
        assert!(
            matches!(self.state, RState::AwaitNodes { child_level: 0, .. }),
            "local comparison runs at the leaf level"
        );
        loop {
            let RState::AwaitNodes { cursor, exclusive, stop, .. } = &mut self.state else {
                return Err(Error::Protocol);
            };
            let Some(key) = self.snapshot.raw().next_key(cursor, *exclusive)? else {
                break;
            };
            if key.first() != Some(&0)
                || before.is_some_and(|limit| &key[1..] >= limit)
                || stop.as_ref().is_some_and(|limit| key[1..] >= limit[..])
            {
                break;
            }
            let source = self.snapshot.raw().snapshot_value(&key)?.ok_or(Error::Corrupt)?;
            let local = StreamEntry::new(&source)?;
            self.deltas += 1;
            if self.resolver.resolve(&SyncDelta {
                key: &key[1..],
                local: local.state(),
                remote: SyncEntState::Missing,
            })? == Resolution::UseRemote
            {
                let wtxn = self.wtxn.as_mut().ok_or(Error::Mode)?;
                self.repair.prepare(wtxn, &key[1..])?;
                if wtxn.sync_forget_deferred(&key[1..])? {
                    self.repair.changed(&key[1..], false);
                    self.applied += 1;
                }
            }
            *cursor = key;
            *exclusive = true;
        }
        Ok(())
    }
    fn end_span(&mut self, out: &mut Vec<u8>) -> Result<()> {
        assert!(
            matches!(self.state, RState::AwaitNodes { .. }),
            "only a requested span can finish"
        );
        if matches!(self.state, RState::AwaitNodes { child_level: 0, .. }) {
            self.local_before(None)?;
        }
        let RState::AwaitNodes { child_level, stop, acc, .. } =
            core::mem::replace(&mut self.state, RState::AwaitPullRes)
        else {
            return Err(Error::Protocol);
        };
        if child_level != 0 {
            if acc.is_empty() {
                return Err(Error::Protocol);
            }
            let mut next_stop = stop;
            for (key, hash) in acc.into_iter().rev() {
                let stop = next_stop.replace(key.clone());
                self.pending.try_reserve_exact(1).map_err(|_| Error::Capacity)?;
                self.pending.push(Pending {
                    level: child_level,
                    ukey: key,
                    hash: hash
                        .as_slice()
                        .try_into()
                        .expect("checked fixed-width field has the destination width"),
                    stop,
                });
            }
        }
        if self.requested.is_empty() {
            self.advance(out)
        } else {
            // Only leaf spans are batched, so no newly discovered children can
            // precede an outstanding response. Resolver order remains sorted.
            assert_eq!(child_level, 0, "batched responses contain leaves");
            self.await_requested();
            Ok(())
        }
    }
    fn advance(&mut self, out: &mut Vec<u8>) -> Result<()> {
        assert!(
            matches!(self.state, RState::AwaitPullRes),
            "completed spans return to the traversal dispatcher"
        );
        assert!(self.requested.is_empty(), "drain responses before discovering more work");
        let mut request = Vec::new();
        let at = frame_begin(&mut request, T_NODES_REQ, 0);
        request.extend_from_slice(&[0; 4]); // count and reserved
        while self.pending.last().is_some_and(|p| self.requested.is_empty() || p.level == 1) {
            let p = self.pending.pop().expect("the traversal has a pending branch");
            p.assert_valid();
            let mut skey = vec![p.level];
            skey.extend_from_slice(&p.ukey);
            if self.snapshot.raw().get(&skey)?.is_some_and(|g| *g == p.hash[..]) {
                // The hash covers members, not the gap before the next sibling.
                let local_stop = self
                    .snapshot
                    .raw()
                    .next_key(&skey, true)?
                    .filter(|key| key.first() == Some(&p.level));
                if local_stop.as_ref().map(|key| &key[1..]) == p.stop.as_deref() {
                    continue;
                }
            }
            request.push(p.level);
            request.push(0);
            request.extend_from_slice(&(p.ukey.len() as u16).to_le_bytes());
            request.extend_from_slice(&p.ukey);
            let leaf = p.level == 1;
            self.requested.try_reserve_exact(1).map_err(|_| Error::Capacity)?;
            self.requested.push_back(p);
            if !leaf
                || self.requested.len() == REQUEST_BATCH_MAX
                || self.pending.last().is_none_or(|next| next.level != 1)
            {
                break;
            }
        }
        if self.requested.is_empty() {
            return self.end(SyncEnd::Merged, out);
        }
        assert!(request.len() <= MAX_REQUEST_SIZE, "bounded descriptors fit one request");
        request[8..10].copy_from_slice(&(self.requested.len() as u16).to_le_bytes());
        frame_end(&mut request, at);
        reserve_sync_output(out, request.len())?;
        out.extend_from_slice(&request);
        self.await_requested();
        Ok(())
    }

    fn await_requested(&mut self) {
        let p = self.requested.pop_front().expect("a response has a queued span");
        p.assert_valid();
        self.span_bytes = 0;
        self.span_entries = 0;
        let mut cursor = vec![0];
        cursor.extend_from_slice(&p.ukey);
        self.state = RState::AwaitNodes {
            kind: 0,
            child_level: p.level - 1,
            cursor,
            exclusive: false,
            stop: p.stop,
            last: None,
            acc: Vec::new(),
        };
    }
    fn end(&mut self, end: SyncEnd, out: &mut Vec<u8>) -> Result<()> {
        assert!(
            matches!(self.state, RState::AwaitPullRes),
            "a receiver sends completion exactly once"
        );
        assert!(self.pending.is_empty(), "completion follows every queued subtree");
        assert!(self.requested.is_empty(), "completion follows every requested span");
        reserve_sync_output(out, 9)?;
        let at = frame_begin(out, T_BYE, 0);
        out.push(0);
        frame_end(out, at);
        self.state = RState::Done(end);
        Ok(())
    }
    /// Inspect the completed effective transaction before deciding to commit.
    /// Repair pending tree hashes so all read APIs describe the same result.
    /// Dropping the receiver after failed application validation rolls back.
    pub fn pending_transaction(&mut self) -> Result<&WriteTxn<'db, S>> {
        if !matches!(self.state, RState::Done(_)) {
            return Err(Error::Protocol);
        }
        let wtxn = self.wtxn.as_mut().ok_or(Error::Mode)?;
        self.repair.flush(wtxn)?;
        Ok(wtxn)
    }

    /// Consumes the receiver: Rust ownership prevents a second finish/commit.
    /// A dropped or failed receiver aborts; premature finish is a protocol error.
    pub fn finish(mut self) -> Result<SyncOutcome> {
        let RState::Done(end) = self.state else {
            return Err(Error::Protocol);
        };
        let mut wtxn = self.wtxn.take().ok_or(Error::Mode)?;
        self.repair.flush(&mut wtxn)?;
        let committed = self.applied > 0;
        if committed {
            wtxn.commit()?;
        }
        Ok(SyncOutcome { end, deltas: self.deltas, applied: self.applied, committed })
    }
    pub fn finish_with_delta(mut self) -> Result<(SyncOutcome, Option<CommittedDelta>)> {
        let RState::Done(end) = self.state else {
            return Err(Error::Protocol);
        };
        let mut wtxn = self.wtxn.take().ok_or(Error::Mode)?;
        self.repair.flush(&mut wtxn)?;
        let committed = self.applied > 0;
        let delta = if committed { Some(wtxn.commit_with_delta()?) } else { None };
        Ok((SyncOutcome { end, deltas: self.deltas, applied: self.applied, committed }, delta))
    }

    /// Repair local secondary records after complete source resolution, before
    /// the same atomic commit. The callback runs only when source changes apply.
    /// Its error aborts both source and secondary changes. It cannot write source
    /// records or add derived data to the captured publication.
    pub fn repair_local_indexes<E: From<Error>>(
        mut self,
        update: impl FnOnce(&mut crate::LocalIndexes<'_, 'db, S>) -> core::result::Result<(), E>,
    ) -> core::result::Result<Self, E> {
        self.pending_transaction()?;
        if self.applied > 0 {
            let wtxn = self.wtxn.as_mut().ok_or(Error::Mode)?;
            update(&mut wtxn.local_indexes())?;
        }
        Ok(self)
    }
}
// ---------------------------------------------------------- pushed deltas

/// A committed transaction's bounded publication. `PullRequired` is a
/// notification to fetch current state using the ordinary pull protocol.
/// Bytes contain only changed entries; they are not a full database image.
#[derive(Debug)]
pub enum CommittedDelta {
    Bytes(Vec<u8>),
    PullRequired,
}

/// Project one complete standalone transaction delta without opening a database.
/// Every entry is validated, including rejected entries. The returned frame
/// preserves the database fingerprint, branch factor, transaction clock and
/// verbatim retained entries; only frame length and retained count change.
/// Input and output are bounded by `max_bytes` (and `MAX_FRAME`); allocation is
/// at most the input length. This validates encoding, not sender authorization
/// or a fingerprint against a particular database. The caller supplies policy.
pub fn project_delta(
    bytes: &[u8],
    max_bytes: usize,
    mut accept: impl FnMut(&[u8]) -> bool,
) -> Result<Vec<u8>> {
    if bytes.len() < DELTA_HEADER || bytes.len() > max_bytes.min(MAX_FRAME) {
        return Err(Error::Protocol);
    }
    let mut rd = Rd::new(bytes);
    if rd.u32()? as usize != bytes.len() || rd.u16()? != T_DELTA || rd.u16()? != 0 {
        return Err(Error::Protocol);
    }
    if rd.u16()? != 1 {
        return Err(Error::Version);
    }
    if !(2..=128).contains(&rd.u8()?) || rd.u8()? != 0 {
        return Err(Error::Protocol);
    }
    rd.bytes(8)?; // Fingerprint is preserved; no database/key is supplied here.
    let clock = Clock::decode(rd.bytes(8)?).ok_or(Error::Protocol)?;
    if clock.is_zero() {
        return Err(Error::Protocol);
    }
    let count = rd.u32()?;
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..DELTA_HEADER]);
    let mut retained = 0u32;
    let mut previous: &[u8] = &[];
    for _ in 0..count {
        let start = rd.off;
        let klen = rd.u16()? as usize;
        let vlen = rd.u32()? as usize;
        let key = rd.bytes(klen)?;
        if key.is_empty() || key.len() > crate::KEY_SIZE_MAX || key <= previous {
            return Err(Error::Protocol);
        }
        previous = key;
        let remote = remote_ent(rd.bytes(vlen)?)?;
        if remote.clock > clock {
            return Err(Error::Protocol);
        }
        if accept(key) {
            out.extend_from_slice(&bytes[start..rd.off]);
            retained += 1;
        }
    }
    rd.done()?;
    out[28..DELTA_HEADER].copy_from_slice(&retained.to_le_bytes());
    frame_end(&mut out, 0);
    Ok(out)
}

/// Inspect a complete bounded delta without allocation or a database handle.
/// Entries borrow the input and include original clocks/tombstones. Callbacks
/// may precede a later malformed entry: perform no durable effects until this
/// returns success. This validates wire shape, not authority or a database key.
pub fn visit_delta<'a>(
    bytes: &'a [u8],
    max_bytes: usize,
    mut visit: impl FnMut(&'a [u8], EntState<'a>),
) -> Result<()> {
    if bytes.len() < DELTA_HEADER || bytes.len() > max_bytes.min(MAX_FRAME) {
        return Err(Error::Protocol);
    }
    let mut rd = Rd::new(bytes);
    if rd.u32()? as usize != bytes.len() || rd.u16()? != T_DELTA || rd.u16()? != 0 {
        return Err(Error::Protocol);
    }
    if rd.u16()? != 1 {
        return Err(Error::Version);
    }
    if !(2..=128).contains(&rd.u8()?) || rd.u8()? != 0 {
        return Err(Error::Protocol);
    }
    rd.bytes(8)?;
    let clock = Clock::decode(rd.bytes(8)?).ok_or(Error::Protocol)?;
    if clock.is_zero() {
        return Err(Error::Protocol);
    }
    let count = rd.u32()?;
    let mut previous: &[u8] = &[];
    for _ in 0..count {
        let klen = rd.u16()? as usize;
        let vlen = rd.u32()? as usize;
        let key = rd.bytes(klen)?;
        if key.is_empty() || key.len() > crate::KEY_SIZE_MAX || key <= previous {
            return Err(Error::Protocol);
        }
        previous = key;
        let stored = rd.bytes(vlen)?;
        let remote = remote_ent(stored)?;
        if remote.clock > clock {
            return Err(Error::Protocol);
        }
        visit(key, remote_ent_state(stored)?);
    }
    rd.done()
}

/// Combine two complete deltas from the same database using the ordinary LWW
/// rule. This preserves original entry clocks and tombstones, validates both
/// inputs (including entries superseded by the other delta), and performs no
/// database I/O. Caller authorization and recipient projection remain required.
/// Each input and the output must fit `max_bytes`; output allocation is bounded
/// by that limit. An oversized union returns `Protocol`, never a partial delta.
pub fn combine_deltas(left: &[u8], right: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    visit_delta(left, max_bytes, |_, _| {})?;
    visit_delta(right, max_bytes, |_, _| {})?;
    if left[8..20] != right[8..20] {
        return Err(Error::Protocol);
    }
    struct Entry<'a> {
        key: &'a [u8],
        stored: &'a [u8],
        encoded: &'a [u8],
    }
    fn next<'a>(reader: &mut Rd<'a>) -> Result<Option<Entry<'a>>> {
        if reader.off == reader.p.len() {
            return Ok(None);
        }
        let start = reader.off;
        let key_len = reader.u16()? as usize;
        let stored_len = reader.u32()? as usize;
        let key = reader.bytes(key_len)?;
        let stored = reader.bytes(stored_len)?;
        Ok(Some(Entry { key, stored, encoded: &reader.p[start..reader.off] }))
    }
    let cap = max_bytes.min(MAX_FRAME);
    let capacity = left.len().saturating_add(right.len()).min(cap);
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&left[..DELTA_HEADER]);
    out[20..28].copy_from_slice(core::cmp::max(&left[20..28], &right[20..28]));
    let mut left_reader = Rd::new(&left[DELTA_HEADER..]);
    let mut right_reader = Rd::new(&right[DELTA_HEADER..]);
    let mut a = next(&mut left_reader)?;
    let mut b = next(&mut right_reader)?;
    let mut count = 0u32;
    while a.is_some() || b.is_some() {
        let (take_left, take_right) = match (&a, &b) {
            (Some(a), Some(b)) => (a.key <= b.key, b.key <= a.key),
            (Some(_), None) => (true, false),
            (None, Some(_)) => (false, true),
            (None, None) => unreachable!(),
        };
        let chosen = if take_left && take_right {
            let a = a.as_ref().expect("take_left requires a left entry");
            let b = b.as_ref().expect("take_right requires a right entry");
            if Resolver::resolve(
                &mut Lww,
                &Delta {
                    key: a.key,
                    local: remote_ent_state(a.stored)?,
                    remote: remote_ent_state(b.stored)?,
                },
            ) == Resolution::UseRemote
            {
                b
            } else {
                a
            }
        } else if take_left {
            a.as_ref().expect("take_left requires a left entry")
        } else {
            b.as_ref().expect("take_right requires a right entry")
        };
        if chosen.encoded.len() > cap - out.len() {
            return Err(Error::Protocol);
        }
        out.extend_from_slice(chosen.encoded);
        count = count.checked_add(1).ok_or(Error::Protocol)?;
        if take_left {
            a = next(&mut left_reader)?;
        }
        if take_right {
            b = next(&mut right_reader)?;
        }
    }
    out[28..DELTA_HEADER].copy_from_slice(&count.to_le_bytes());
    frame_end(&mut out, 0);
    Ok(out)
}

pub(crate) struct DeltaCapture {
    entries: alloc::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    size: usize,
    cap: usize,
    pull: bool,
}

impl DeltaCapture {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: Default::default(),
            size: DELTA_HEADER,
            cap: cap.min(MAX_FRAME),
            pull: cap < DELTA_HEADER,
        }
    }

    pub(crate) fn require_pull(&mut self) {
        self.entries.clear();
        self.pull = true;
    }

    pub(crate) fn admits(&mut self, key: &[u8], stored_len: usize) -> bool {
        if self.pull {
            return false;
        }
        let previous = self.entries.get(key).map_or(0, |v| 6 + key.len() + v.len());
        if self.size - previous + 6 + key.len() + stored_len > self.cap {
            self.require_pull();
            return false;
        }
        true
    }

    pub(crate) fn record(&mut self, key: &[u8], stored: &[u8]) {
        if self.pull {
            return;
        }
        if let Some(old) = self.entries.remove(key) {
            self.size -= 6 + key.len() + old.len();
        }
        let size = self.size + 6 + key.len() + stored.len();
        if size > self.cap {
            self.require_pull();
            return;
        }
        self.entries.insert(key.to_vec(), stored.to_vec());
        self.size = size;
    }

    pub(crate) fn encode(self, meta: &encoding::Metadata, clock: Clock) -> CommittedDelta {
        if self.pull {
            return CommittedDelta::PullRequired;
        }
        let mut out = Vec::with_capacity(self.size);
        let at = frame_begin(&mut out, T_DELTA, 0);
        out.extend_from_slice(&1u16.to_le_bytes()); // independent delta version
        out.push(meta.branch_factor);
        out.push(0);
        out.extend_from_slice(&hashkey_fingerprint(&meta.hash_key));
        out.extend_from_slice(&clock.encode());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (key, stored) in self.entries {
            out.extend_from_slice(&(key.len() as u16).to_le_bytes());
            out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&key);
            out.extend_from_slice(&stored);
        }
        frame_end(&mut out, at);
        CommittedDelta::Bytes(out)
    }
}

struct DeltaMerge<'db, S: Storage, R: Resolver> {
    snapshot: ReadTxn<'db, S>,
    wtxn: WriteTxn<'db, S>,
    resolver: R,
    deltas: usize,
    applied: usize,
    local_changed: bool,
}
impl<'db, S: Storage, R: Resolver> DeltaMerge<'db, S, R> {
    fn new(db: &'db Db<S>, resolver: R, wall: u64) -> Result<Self> {
        if !db.meta().syncable {
            return Err(Error::Mode);
        }
        let wtxn = db.begin_write(wall)?;
        let snapshot = db.begin_read()?;
        Ok(Self { snapshot, wtxn, resolver, deltas: 0, applied: 0, local_changed: false })
    }
    fn finish(self) -> Result<SyncOutcome> {
        let committed = self.applied > 0 || self.local_changed;
        if committed {
            self.wtxn.commit()?;
        }
        Ok(SyncOutcome {
            end: SyncEnd::Merged,
            deltas: self.deltas,
            applied: self.applied,
            committed,
        })
    }
    fn finish_with_delta(self) -> Result<(SyncOutcome, Option<CommittedDelta>)> {
        let committed = self.applied > 0 || self.local_changed;
        let delta = if committed { Some(self.wtxn.commit_with_delta()?) } else { None };
        Ok((
            SyncOutcome {
                end: SyncEnd::Merged,
                deltas: self.deltas,
                applied: self.applied,
                committed,
            },
            delta,
        ))
    }
    fn resolve_apply(
        &mut self,
        key: &[u8],
        local: EntState<'_>,
        remote_stored: &[u8],
    ) -> Result<()> {
        let remote = remote_ent_state(remote_stored)?;
        let remote_clock = match remote {
            EntState::Present { clock, .. } | EntState::Deleted { clock } => clock,
            EntState::Missing => unreachable!(),
        };
        // Entries must not sneak an unobserved/exhausting clock past a peer's
        // advertised database clock. Observe any higher entry before commit.
        if remote_clock >= self.wtxn.clock() {
            self.wtxn.set_clock(self.wtxn.clock().recv(remote_clock, 0)?)?;
        }
        self.deltas += 1;
        let delta = Delta { key, local, remote };
        if matches!(self.resolver.resolve(&delta), Resolution::KeepLocal) {
            return Ok(());
        }
        let changed = match remote {
            EntState::Present { value, clock } => self.wtxn.sync_put(key, value, 0, clock)?,
            EntState::Deleted { clock } => {
                self.wtxn.sync_put(key, &[], ENT_FLAG_TOMBSTONE, clock)?
            }
            EntState::Missing => unreachable!(),
        };
        if changed {
            self.applied += 1;
        }
        Ok(())
    }
}

/// Receive one bounded transaction delta over arbitrarily fragmented input.
/// Uses the pull resolver and merge transaction; omitted keys are untouched.
/// The caller authenticates the publisher and database/channel before feeding
/// bytes. This protocol provides neither authorization nor reliable delivery:
/// reconcile using pull after reconnect or a `PullRequired` notification.
/// Dropping the receiver, any error, or finishing incomplete input rolls back.
/// The single database writer is held until finish/drop, so transports should
/// enforce a deadline. `max_bytes` caps the entire message, not each fragment.
pub struct DeltaReceiver<'db, S: Storage, R: Resolver> {
    inner: DeltaMerge<'db, S, R>,
    input: Vec<u8>,
    cap: usize,
    hash_key: [u8; 32],
    branch_factor: u8,
    done: bool,
    failed: bool,
}

impl<'db, S: Storage, R: Resolver> DeltaReceiver<'db, S, R> {
    pub fn new(db: &'db Db<S>, resolver: R, wall_unix_secs: u64, max_bytes: usize) -> Result<Self> {
        // A bounded live delta uses the same write transaction and tree
        // semantics without full-sync traversal or disk staging.
        let inner = DeltaMerge::new(db, resolver, wall_unix_secs)?;
        Ok(Self {
            inner,
            input: Vec::new(),
            cap: max_bytes.min(MAX_FRAME),
            hash_key: db.meta().hash_key,
            branch_factor: db.meta().branch_factor,
            done: false,
            failed: false,
        })
    }

    /// Receive and capture applied changes for onward publication after
    /// commit. Origin clocks remain unchanged through any number of relays.
    pub fn new_with_delta(
        db: &'db Db<S>,
        resolver: R,
        wall_unix_secs: u64,
        max_bytes: usize,
    ) -> Result<Self> {
        let mut receiver = Self::new(db, resolver, wall_unix_secs, max_bytes)?;
        receiver.inner.wtxn.capture_changes(max_bytes);
        Ok(receiver)
    }

    /// Inspect the completed merge before application validation and commit.
    /// Incomplete or failed input never exposes a committable transaction.
    pub fn pending_transaction(&self) -> Result<&WriteTxn<'db, S>> {
        if self.failed || !self.done {
            return Err(Error::Protocol);
        }
        Ok(&self.inner.wtxn)
    }

    pub fn finish_with_delta(self) -> Result<(SyncOutcome, Option<CommittedDelta>)> {
        if self.failed || !self.done {
            return Err(Error::Protocol);
        }
        self.inner.finish_with_delta()
    }

    /// Same atomic secondary-index boundary as `SyncReceiver::repair_local_indexes`.
    /// Failed or incomplete input never calls `update`; duplicates do not repair
    /// or commit. Callers validate source authorization before this boundary.
    pub fn repair_local_indexes<E: From<Error>>(
        mut self,
        update: impl FnOnce(&mut crate::LocalIndexes<'_, 'db, S>) -> core::result::Result<(), E>,
    ) -> core::result::Result<Self, E> {
        self.pending_transaction()?;
        if self.inner.applied > 0 {
            update(&mut self.inner.wtxn.local_indexes())?;
        }
        Ok(self)
    }

    /// Apply independently authenticated local qualification even if every
    /// source loses conflict resolution. The callback reports whether local
    /// state changed; raw applied counts and captured source deltas stay exact.
    /// Any error aborts both source and local updates.
    pub fn qualify_local_indexes<E: From<Error>>(
        mut self,
        update: impl FnOnce(&mut crate::LocalIndexes<'_, 'db, S>) -> core::result::Result<bool, E>,
    ) -> core::result::Result<Self, E> {
        self.pending_transaction()?;
        self.inner.local_changed |= update(&mut self.inner.wtxn.local_indexes())?;
        Ok(self)
    }

    pub fn process(&mut self, input: &[u8]) -> Result<SyncStatus> {
        if self.failed {
            return Err(Error::Protocol);
        }
        match self.process_inner(input) {
            Ok(status) => Ok(status),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn process_inner(&mut self, input: &[u8]) -> Result<SyncStatus> {
        if self.done {
            return if input.is_empty() { Ok(SyncStatus::Done) } else { Err(Error::Protocol) };
        }
        if input.len() > self.cap.saturating_sub(self.input.len()) {
            return Err(Error::Protocol);
        }
        self.input.extend_from_slice(input);
        if self.input.len() < 4 {
            return Ok(SyncStatus::Running);
        }
        let size = u32::from_le_bytes(
            self.input[..4]
                .try_into()
                .expect("checked fixed-width field has the destination width"),
        ) as usize;
        if size < DELTA_HEADER || size > self.cap || self.input.len() > size {
            return Err(Error::Protocol);
        }
        if self.input.len() < size {
            return Ok(SyncStatus::Running);
        }
        let mut rd = Rd::new(&self.input[4..]);
        let clock = self.read_delta_header(&mut rd)?;
        self.inner.wtxn.set_clock(self.inner.wtxn.clock().recv(clock, 0)?)?;
        let count = rd.u32()?;
        let mut previous: &[u8] = &[];
        for _ in 0..count {
            let klen = rd.u16()? as usize;
            let vlen = rd.u32()? as usize;
            let key = rd.bytes(klen)?;
            if key.is_empty() || key.len() > crate::KEY_SIZE_MAX || key <= previous {
                return Err(Error::Protocol);
            }
            previous = key;
            let stored = rd.bytes(vlen)?;
            let remote = remote_ent(stored)?;
            if remote.clock > clock {
                return Err(Error::Protocol);
            }
            let mut skey = Vec::with_capacity(1 + key.len());
            skey.push(0);
            skey.extend_from_slice(key);
            let local = self.inner.snapshot.raw().get(&skey)?.map(|v| v.to_vec());
            let state = match &local {
                Some(v) => decode_ent_state(v)?,
                None => EntState::Missing,
            };
            let remote = remote_ent_state(stored)?;
            if state == remote {
                // Even an identical replay must reach current source admission.
                // It cannot change storage, clock, outcome counts or indexes.
                self.inner.resolver.resolve(&Delta { key, local: state, remote });
            } else {
                self.inner.resolve_apply(key, state, stored)?;
            }
        }
        rd.done()?;
        self.input.clear();
        self.done = true;
        Ok(SyncStatus::Done)
    }

    fn read_delta_header(&self, rd: &mut Rd<'_>) -> Result<Clock> {
        if rd.u16()? != T_DELTA || rd.u16()? != 0 {
            return Err(Error::Protocol);
        }
        if rd.u16()? != 1 {
            return Err(Error::Version);
        }
        if rd.u8()? != self.branch_factor || rd.u8()? != 0 {
            return Err(Error::Protocol);
        }
        if rd.bytes(8)? != hashkey_fingerprint(&self.hash_key) {
            return Err(Error::HashKeyMismatch);
        }
        let clock = Clock::decode(rd.bytes(8)?).ok_or(Error::Protocol)?;
        if clock.is_zero() {
            return Err(Error::Protocol);
        }
        Ok(clock)
    }

    pub fn finish(self) -> Result<SyncOutcome> {
        if self.failed || !self.done {
            return Err(Error::Protocol);
        }
        self.inner.finish()
    }
}

// ---------------------------------------------------------------- sender

enum SState {
    AwaitPullReq,
    Serving,
    Done,
}

/// The read-only, answering side of a sync session.
pub struct SyncSender<'db, S: Storage> {
    snapshot: ReadTxn<'db, S>,
    branch_factor: u8,
    hash_key: [u8; 32],
    db_clock: Clock,
    framer: Framer,
    state: SState,
    output: Vec<u8>,
    output_offset: usize,
    value: Option<(<S::Txn<'db> as StorageTxn>::ValueReader, usize)>,
    next: Option<Vec<u8>>,
    child_level: u8,
    range: bool,
    range_stop: Option<Vec<u8>>,
    requests: VecDeque<(u8, Vec<u8>)>,
}

impl<'db, S: Storage> SyncSender<'db, S> {
    /// Root of the pinned transfer snapshot. Later source commits cannot
    /// change this receipt; do not read the live database for a pull's root.
    pub fn root_hash(&self) -> Result<Option<(u8, [u8; 32])>> {
        self.snapshot.root_hash()
    }

    pub fn new(db: &'db Db<S>) -> Result<SyncSender<'db, S>> {
        if !db.meta().syncable {
            return Err(Error::Mode);
        }
        Ok(SyncSender {
            snapshot: db.begin_read()?,
            branch_factor: db.meta().branch_factor,
            hash_key: db.meta().hash_key,
            db_clock: db.clock(),
            framer: Framer::default(),
            state: SState::AwaitPullReq,
            output: Vec::new(),
            output_offset: 0,
            value: None,
            next: None,
            child_level: 0,
            range: false,
            range_stop: None,
            requests: VecDeque::new(),
        })
    }

    /// Feed a request fragment. Drain `write_output` until it returns zero
    /// before feeding the next request. Each request contains at most
    /// REQUEST_BATCH_MAX ordered leaf spans (or one non-leaf span).
    pub fn process(&mut self, input: &[u8]) -> Result<SyncStatus> {
        if self.output_offset < self.output.len()
            || self.value.is_some()
            || self.next.is_some()
            || !self.requests.is_empty()
        {
            return Err(Error::Protocol);
        }
        if input.len()
            > MAX_REQUEST_SIZE.saturating_sub(self.framer.buf.len().saturating_sub(self.framer.off))
        {
            return Err(Error::Protocol);
        }
        self.framer.push(input);
        if let Some((typ, flags, payload)) = self.framer.next()? {
            if flags != 0 {
                return Err(Error::Protocol);
            }
            let mut out = Vec::new();
            let result = self.handle_frame(typ, &payload, &mut out);
            if !out.is_empty() {
                self.output = out;
                self.output_offset = 0;
            }
            result?;
            if self.framer.buf.len() != self.framer.off {
                return Err(Error::Protocol);
            }
        }
        Ok(if matches!(self.state, SState::Done) { SyncStatus::Done } else { SyncStatus::Running })
    }

    /// Emit at most the supplied output capacity (and at most 256 KiB).
    /// Large logical frames retain their wire size and cross output calls.
    pub fn write_output(&mut self, out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Err(Error::InvalidArgument);
        }
        let mut written = 0;
        let limit = out.len().min(DEFAULT_MSG_CAP);
        while written < limit {
            if self.output_offset < self.output.len() {
                let n = (self.output.len() - self.output_offset).min(limit - written);
                out[written..written + n]
                    .copy_from_slice(&self.output[self.output_offset..self.output_offset + n]);
                self.output_offset += n;
                written += n;
            } else if let Some((value, offset)) = &mut self.value {
                let n = (value.len() - *offset).min(limit - written);
                value.read_exact_at(*offset, &mut out[written..written + n])?;
                *offset += n;
                written += n;
                if *offset == value.len() {
                    self.value = None;
                }
            } else if let Some(key) = self.next.take() {
                self.prepare_entry(key)?;
            } else if let Some((level, key)) = self.requests.pop_front() {
                self.serve_chunk(level, &key)?;
            } else {
                break;
            }
        }
        Ok(written)
    }

    fn handle_frame(&mut self, typ: u16, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
        match (&self.state, typ) {
            (SState::AwaitPullReq, T_PULL_REQ) => {
                let mut rd = Rd::new(payload);
                let ver = rd.u16()?;
                if ver != PROTOCOL_VERSION {
                    let at = frame_begin(out, T_PULL_RES, 0);
                    out.extend_from_slice(&[S_ERR_VERSION, 0, 0, 0]);
                    out.extend_from_slice(&[0; 32]);
                    out.extend_from_slice(&self.db_clock.encode());
                    frame_end(out, at);
                    self.state = SState::Done;
                    return Err(Error::Version);
                }
                let bf = rd.u8()?;
                let root_level = rd.u8()?;
                let fp: [u8; 8] = rd
                    .bytes(8)?
                    .try_into()
                    .expect("checked fixed-width field has the destination width");
                let root_hash: [u8; 32] = rd
                    .bytes(32)?
                    .try_into()
                    .expect("checked fixed-width field has the destination width");
                rd.done()?;

                let my_root = self.snapshot.root_hash()?;
                let (status, err) = if fp != hashkey_fingerprint(&self.hash_key) {
                    (S_ERR_HASHKEY, Some(Error::HashKeyMismatch))
                } else if bf != self.branch_factor {
                    (S_ERR_BRANCH_FACTOR, Some(Error::BranchFactorMismatch))
                } else if my_root.is_none() {
                    (S_EMPTY, None)
                } else if my_root == Some((root_level, root_hash)) && root_level != 0 {
                    (S_NODIFF, None)
                } else if root_level == 0 || self.range_fits(&[], None)? {
                    (S_SNAPSHOT, None)
                } else {
                    (S_DIFF, None)
                };

                let at = frame_begin(out, T_PULL_RES, 0);
                out.push(status);
                out.push(my_root.map_or(0, |(l, _)| l));
                out.extend_from_slice(&[0; 2]);
                out.extend_from_slice(&my_root.map_or([0; 32], |(_, h)| h));
                out.extend_from_slice(&self.db_clock.encode());
                frame_end(out, at);

                match err {
                    Some(e) => {
                        self.state = SState::Done;
                        Err(e)
                    }
                    None => {
                        self.state = if matches!(status, S_DIFF | S_SNAPSHOT) {
                            SState::Serving
                        } else {
                            SState::Done
                        };
                        if status == S_SNAPSHOT {
                            self.start_range(&[], None)?;
                        }
                        Ok(())
                    }
                }
            }
            (SState::Serving, T_NODES_REQ) => {
                let mut rd = Rd::new(payload);
                let count = rd.u16()? as usize;
                if rd.u16()? != 0 || !(1..=REQUEST_BATCH_MAX).contains(&count) {
                    return Err(Error::Protocol);
                }
                let mut requests = VecDeque::new();
                requests.try_reserve_exact(count).map_err(|_| Error::Capacity)?;
                for _ in 0..count {
                    let level = rd.u8()?;
                    if rd.u8()? != 0
                        || level == 0
                        || level > encoding::LEVEL_MAX
                        || (count > 1 && level != 1)
                    {
                        return Err(Error::Protocol);
                    }
                    let len = rd.u16()? as usize;
                    if len > crate::KEY_SIZE_MAX {
                        return Err(Error::Protocol);
                    }
                    let key = rd.bytes(len)?.to_vec();
                    if requests.back().is_some_and(|(_, previous): &(u8, Vec<u8>)| previous >= &key)
                    {
                        return Err(Error::Protocol);
                    }
                    requests.push_back((level, key));
                }
                rd.done()?;
                self.requests = requests;
                Ok(())
            }
            (SState::Serving, T_BYE) | (SState::AwaitPullReq, T_BYE) | (SState::Done, T_BYE) => {
                if payload != [0] {
                    return Err(Error::Protocol);
                }
                self.state = SState::Done;
                Ok(())
            }
            _ => Err(Error::Protocol),
        }
    }

    // Probe encoded bytes without loading values. Even zero-byte application
    // values cost an entry header/key, so the byte limit bounds iterations too.
    fn range_fits(&self, start: &[u8], stop: Option<&[u8]>) -> Result<bool> {
        let txn = self.snapshot.raw();
        let mut key = vec![0];
        key.extend_from_slice(start);
        let mut next = txn.next_key(&key, false)?;
        let mut bytes = 0usize;
        while let Some(key) = next {
            if key.first() != Some(&0) || stop.is_some_and(|stop| &key[1..] >= stop) {
                break;
            }
            let len = txn.value_len(&key)?.ok_or(Error::Corrupt)?;
            bytes = bytes.saturating_add(22 + key.len() - 1).saturating_add(len);
            if bytes > SMALL_RANGE_BYTES {
                return Ok(false);
            }
            next = txn.next_key(&key, true)?;
        }
        Ok(true)
    }

    fn start_range(&mut self, start: &[u8], stop: Option<Vec<u8>>) -> Result<()> {
        self.child_level = 0;
        self.range = true;
        self.range_stop = stop;
        let mut from = vec![0];
        from.extend_from_slice(start);
        let key = self.snapshot.raw().next_key(&from, false)?.ok_or(Error::Protocol)?;
        if key.first() != Some(&0)
            || self.range_stop.as_ref().is_some_and(|stop| key[1..] >= stop[..])
        {
            return Err(Error::Protocol);
        }
        // Delay entry framing until the preceding PullResponse is drained.
        self.next = Some(key);
        Ok(())
    }

    fn serve_chunk(&mut self, level: u8, ukey: &[u8]) -> Result<()> {
        if ukey.len() > crate::KEY_SIZE_MAX {
            return Err(Error::Protocol);
        }
        let txn = self.snapshot.raw();
        let mut branch = vec![level];
        branch.extend_from_slice(ukey);
        let branch = txn.next_key(&branch, false)?.ok_or(Error::Protocol)?;
        if branch.first() != Some(&level) || (!ukey.is_empty() && branch[1..] != *ukey) {
            return Err(Error::Protocol);
        }
        // Level-1 responses already stream leaves. Probing them cannot save
        // a discovery turn, and would duplicate their storage reads.
        if level > 1 {
            let stop = txn
                .next_key(&branch, true)?
                .filter(|key| key.first() == Some(&level))
                .map(|key| key[1..].to_vec());
            if self.range_fits(ukey, stop.as_deref())? {
                return self.start_range(ukey, stop);
            }
        }
        self.range = false;
        self.range_stop = None;
        self.child_level = level - 1;
        let mut from = vec![self.child_level];
        from.extend_from_slice(ukey);
        let key = txn.next_key(&from, false)?.ok_or(Error::Protocol)?;
        if key.first() != Some(&self.child_level) || (!ukey.is_empty() && key[1..] != *ukey) {
            return Err(Error::Protocol);
        }
        self.prepare_entry(key)
    }
    fn prepare_entry(&mut self, key: Vec<u8>) -> Result<()> {
        let txn = self.snapshot.raw();
        self.next = match txn.next_key(&key, true)? {
            Some(next) if next.first() == Some(&self.child_level) => {
                if self.range {
                    if self.range_stop.as_ref().is_some_and(|stop| next[1..] >= stop[..]) {
                        None
                    } else {
                        Some(next)
                    }
                } else {
                    let mut byte = [0];
                    if txn.read_value_at(&next, 0, &mut byte)? != Some(1) {
                        return Err(Error::Corrupt);
                    }
                    if encoding::is_boundary(byte[0], self.branch_factor) {
                        None
                    } else {
                        Some(next)
                    }
                }
            }
            _ => None,
        };
        let value = txn.snapshot_value(&key)?.ok_or(Error::Corrupt)?;
        let len = value.len();
        let size = 16 + 6 + key.len() - 1 + len;
        if size > MAX_FRAME {
            return Err(Error::ValueSize);
        }
        self.output.clear();
        self.output_offset = 0;
        let at = frame_begin(
            &mut self.output,
            if self.range { T_RANGE_RES } else { T_NODES_RES },
            if self.next.is_some() { F_MORE } else { 0 },
        );
        self.output.push(self.child_level);
        self.output.extend_from_slice(&[0; 3]);
        self.output.extend_from_slice(&1u32.to_le_bytes());
        self.output.extend_from_slice(&((key.len() - 1) as u16).to_le_bytes());
        self.output.extend_from_slice(&(len as u32).to_le_bytes());
        self.output.extend_from_slice(&key[1..]);
        self.output[at..at + 4].copy_from_slice(&(size as u32).to_le_bytes());
        self.value = Some((value, 0));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
