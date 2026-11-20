# kvdb

A "local-first" key-value store, written in Rust for the Playbit runtime.
kvdb implements the Playbit Core v1 application-private transactional store
and, per database, an optional CRDT layer that lets any number of replicas
converge by exchanging minimal changes over any byte transport.

- Eventual consistency with efficient merging (a Merkle-ized prolly tree:
  sync cost is proportional to the difference, not the database size)
- Storage stays bounded over time regardless of edit count — no append-only
  log
- Hybrid logical clocks; entries written in one transaction share one clock
  (sync respects transaction boundaries)
- Built-in last-writer-wins conflict resolution with a pluggable resolver
- `no_std + alloc`, sans-io, `forbid(unsafe_code)` core; storage backends
  and platform integration live behind small traits
- Runs on native (redb backend), WebAssembly, and embedded (builds for
  `wasm32-unknown-unknown` and `thumbv7em-none-eabi`)

This is a clean-break Rust redesign of the earlier C implementations
(`playbit/kvdb` and its portable fork): all integers little-endian, keyed
BLAKE3, and a fixed-layout wire encoding. No compatibility with the C
formats.

## Crates

| crate | purpose |
|---|---|
| `kvdb` | the store: encoding, HLC clocks, prolly tree, transactions, sync engine, in-memory backend (`storage::mem`) |
| `kvdb::redb` (feature `redb`) | durable native storage backend built on [redb](https://www.redb.org) |

## Usage

Run `cargo test -p kvdb --features redb` for the database tests.

`Db::open(storage, &OpenOptions { verify_on_open: true })` verifies a syncable
store before returning it; `OpenOptions::default()` skips this diagnostic scan.
`Db::verify()` checks a committed snapshot and `WriteTxn::verify()` checks the
pending tree without modifying it. Verification reports corrupt leaf encodings,
hashes or noncanonical branches as errors, with bounded scratch rather than a
copy of the database. Tests and fuzz targets opt in by calling `verify()`
explicitly; to audit pending writes, call `write.verify()?` before
`write.commit()?` and discard the transaction on error. Ordinary commits retain
local invariant checks without scanning the whole tree, in debug and release.

```rust
use kvdb::{CreateOptions, Db, Lww, SyncReceiver, SyncSender};
use kvdb::storage::mem::MemStorage;

// Every replica of one database shares `hash_key` (generate it randomly
// when the replica set is created).
let opts = CreateOptions::syncable(hash_key, replica_id);
let db = Db::create(MemStorage::new(), &opts)?;

// Transactions: many snapshot readers, one writer, atomic commit.
// kvdb is sans-io: the caller passes wall-clock time (Unix seconds).
let mut w = db.begin_write(now_unix_secs)?;
w.put(b"greeting", b"hello")?;
w.del(b"obsolete")?; // writes a tombstone so the delete syncs
w.commit()?;

let r = db.begin_read()?;
assert_eq!(r.get(b"greeting")?.as_deref(), Some(&b"hello"[..]));
for entry in r.iter_from(b"g") { /* ordered, tombstones skipped */ }
```

`ReadTxn::get_with_clock` and `WriteTxn::get_with_clock` return a live value and
its originating entry clock from one backend guard. Missing/deleted keys return
`None`; a plain database returns `Error::Mode`.

Live changes use `begin_write_with_delta` and `commit_with_delta`. A durable
outbox can replay one committed entry with `ReadTxn::delta_for_key(key, max_bytes)`:
it captures that snapshot's value/clock, including tombstones, without a writer
or clock advance. Missing keys return `None`; an oversized entry returns
`CommittedDelta::PullRequired`. This is distinct from committing an unchanged
value, which correctly produces no changed entry.

Initial and recovery synchronization use a pull: the receiver merges and the
sender answers from a snapshot. Both sides are sans-io state machines; the
application moves the bytes:

```rust
let mut out = Vec::new();
let mut rx = SyncReceiver::new(&db, Lww, staging, now_unix_secs, &mut out)?;
// send `out` to the peer, feed replies to rx.process(input, &mut out),
// keep shuttling bytes until it reports SyncStatus::Done, then:
let outcome = rx.finish()?; // commits the merge atomically

// the other side:
let mut tx = SyncSender::new(&peer_db)?;
let status = tx.process(input)?;
let mut fragment = [0u8; 4096];
loop {
    let n = tx.write_output(&mut fragment)?;
    if n == 0 { break; }
    transport.write_all(&fragment[..n])?;
}
// Drain output even when status is Done; then finish the transport session.
```

Full sync requires a `SyncStaging` provider for one encoded remote entry.
Native callers supply `kvdb_store_redb::FileStaging` backed by a private,
unlinked temporary file, reused between entries without truncating. Reads are
limited to the current entry; disk use is bounded by the largest staged entry.
Web callers can implement the same interface using
an OPFS worker. The core does not open files. Staging preserves the existing
64 MiB logical-frame ceiling and legal key/value sizes; transport fragments
and sender output are bounded to 256 KiB. `SyncReceiver` holds one uncommitted
writer and a stable snapshot; dropping it aborts its changes. It suits immediate
in-process merges. Use `sync::PreparedSync` when transport may wait: it shares
the same protocol traversal but holds only a read snapshot and journals remote
differences, without copying the database or opening its writer.

```rust
let mut rx = kvdb::sync::PreparedSync::new(
    &db, entry_staging, journal, 128 * 1024 * 1024, &mut out,
)?;
// Exchange bytes through rx.process until SyncStatus::Done.
let changes = rx.finish()?; // releases the snapshot and entry scratch
// Revalidate authorization under the application's commit ordering boundary.
let pending = changes.merge(&db, current_resolver, now_unix_secs, 65536)?;
validate(pending.pending_transaction()?)?;
let (outcome, delta) = pending.finish_with_delta()?; // atomic durable commit
```

Prepared merge resolves against current committed entries, preserving concurrent
writes and origin clocks. Dropping a pending merge rolls it back. Callers own
current authorization and publication of its resulting delta. The journal is
also `SyncStaging`; native callers use a second private unlinked file with
`FileStaging::with_capacity(file, max_bytes)`. Admission rejects the complete
next record before exceeding the supplied journal bound. File staging keeps
its independently supplied cap; the normal constructor retains 64 MiB.
Journal copying uses at most 4 KiB scratch, including maximum legal values.
An incomplete, malformed or oversized receive cannot be finished.

An explicit storage benchmark uses ordinary fsync, 512 KiB redb caches and
100 samples each of initial 10,000-record/4 MiB payload and 1,000 missed
changes/1 MiB payload. It reports file creation, snapshot, receiver processing,
combined sender/receiver transfer, final merge/commit and journal bytes.
It has no network, authorization or UI and cannot qualify their latency.
Run on a quiet reference device from the workspace root:

```sh
cargo test --locked --release -p kvdb --features redb benchmark_prepared_sync -- --ignored --nocapture
```

Custom full-sync handling uses `SyncResolver` (or a closure returning
`Result<Resolution>`). `SyncDelta { key, local, remote }` exposes values as
`&dyn ValueRead`, so callers can inspect bounded ranges. Live `DeltaReceiver`
uses the existing bounded `Delta`/`Resolver` interface. Both use the same
transaction/tree implementation, original clocks and canonical wire format.

Native `ReadTxn::value_reader` pins the actual redb read page and snapshot
until that entry is released; repeated reads neither reload the page nor
copy the value. Redb may allocate a large page for a large value. Those
backend pages count toward process peak RSS, separately from explicit
transfer buffers. A 512 KiB page-cache setting does not cap an individual
page. Product memory qualification must include that peak and concurrent
jobs; passing the bounded-buffer tests is insufficient.

## Design & architecture

Although kvdb is technically a CRDT, it is implemented a bit differently
than most CRDTs: as a Merkle tree with probabilistic branching. A branch
is formed whenever the hash of one of its children falls below a
threshold that controls the average branching factor: a node whose first
hash byte is less than `256/branch_factor` starts a new branch (it is a
"boundary"). Because one byte decides, that threshold is a whole number
of the 256 byte values, so the average actually produced is 256 over it:
a `branch_factor` dividing 256 gets exactly what it asked for, while 33
averages about 36 and everything from 86 to 128 averages 128. This
general approach is known as a "prolly tree"; a hybrid between a B-tree
and a Merkle tree. All hashing is keyed BLAKE3 with the 32-byte hash key
shared by every replica of a database, so all replicas agree on tree
shape.

Here is an illustrated example, using a branching factor of 4 with 16
entries. The hash threshold for a boundary is `0x40 = 256/4`.

### Logical tree structure

```
┌───────────┬───────────┬───────────┬─────┬───────┐
│  level 2  │  level 1  │  level 0  │     │       │
│  root     │  branches │  leafs    │ key │ value │
└───────────┴───────────┴───────────┼─────┼───────┘
   3605808c ┬─ 79082d68 ┬─ 2c       │ a   │ ali
            │           ╰─ 5a       │ b   │ barb
            ├─ 6ade6050 ┬─ 26       │ c   │ casey
            │           ├─ 44       │ d   │ dee
            │           ├─ 41       │ e   │ emm
            │           ╰─ f1       │ k   │ kiko
            ╰─ 4b546f7e ┬─ 18       │ l   │ larry
                        ├─ 7b       │ m   │ mia
                        ├─ 5a       │ n   │ nina
                        ├─ e5       │ q   │ qi
                        ├─ 88       │ s   │ sam
                        ├─ 90       │ t   │ tegan
                        ├─ a4       │ u   │ ulla
                        ├─ 71       │ v   │ via
                        ├─ a4       │ x   │ xavier
                        ╰─ 7b       │ z   │ zelda
```

Entry `c=casey` has the hash byte `0x26`, a value less than `0x40`,
causing a new branch to be formed starting with `c=casey`. A branch
covers children from its own key up to the next boundary; the first
branch of each level — the *anchor* — covers from the beginning, whether
or not its first child is a boundary. The value of a branch is a keyed
hash over its children in key order (a child contributes its stored
hash, or its raw key and value for small leaves, plus its originating
clock). This is a recursive property; the root is the hash of the entire
database.

Thus, when making modifications to the entries stored, say changing the
value of `s` to `simon`, the hash of its parent (branch) will change,
and its parent's parent, and so on, ultimately resulting in a new root
hash. In practice only a few bookkeeping entries need updating when an
entry changes: each level is re-chunked over a small window around the
change and the result is diffed against what is stored. (Bulk builds run
the identical code over whole levels, so incremental and bulk
maintenance agree by construction.)

Because the tree shape is a pure function of the entries and the hash
key, two replicas holding the same data and originating clocks store byte-identical trees:
being in sync is a single root-hash comparison, and when replicas
differ, equal subtree hashes prune the walk, making sync work
proportional to the difference rather than the database size.

### Concrete data representation

The actual layout of the database is in a plain ordered key-value store:

```
              ┏━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
              ┃ KEY         ┃ VALUE                               ┃
┌────┬────────╂───────┬─────╂──────┬───────┬──────────┬───────────┨
│  # │ Type   ┃ Level │ Key ┃ Hash │ Flags │ Clock    │ Data      ┃
└────┼────────╂───────┼─────╂──────┼───────┼──────────┼───────────┨
   0 │ leaf   ┃ 00    │ a   ┃ 2c   │ 00    │ 3D9173.0 │ ali       ┃
   1 │ leaf   ┃ 00    │ b   ┃ 5a   │ 00    │ 3D9173.0 │ barb      ┃
   2 │ leaf   ┃ 00    │ c   ┃ 26   │ 00    │ 3D9173.1 │ casey     ┃
   3 │ leaf   ┃ 00    │ d   ┃ 44   │ 00    │ 3D9173.2 │ dee       ┃
   4 │ leaf   ┃ 00    │ e   ┃ 41   │ 00    │ 3D9173.2 │ emm       ┃
   5 │ leaf   ┃ 00    │ k   ┃ f1   │ 00    │ 3D9173.2 │ kiko      ┃
   6 │ leaf   ┃ 00    │ l   ┃ 18   │ 00    │ 3D9173.2 │ larry     ┃
   7 │ leaf   ┃ 00    │ m   ┃ 7b   │ 00    │ 3D9173.2 │ mia       ┃
   8 │ leaf   ┃ 00    │ n   ┃ 5a   │ 00    │ 3D9173.2 │ nina      ┃
   9 │ leaf   ┃ 00    │ q   ┃ e5   │ 00    │ 3D9173.3 │ qi        ┃
  10 │ leaf   ┃ 00    │ s   ┃ 88   │ 00    │ 3D9173.3 │ sam       ┃
  11 │ leaf   ┃ 00    │ t   ┃ 90   │ 00    │ 3D9173.3 │ tegan     ┃
  12 │ leaf   ┃ 00    │ u   ┃ a4   │ 00    │ 3D9173.3 │ ulla      ┃
  13 │ leaf   ┃ 00    │ v   ┃ 71   │ 00    │ 3D9174.0 │ via       ┃
  14 │ leaf   ┃ 00    │ x   ┃ a4   │ 00    │ 3D9174.0 │ xavier    ┃
  15 │ leaf   ┃ 00    │ z   ┃ 7b   │ 00    │ 3D9174.0 │ zelda     ┃
  16 │ branch ┃ 01    │     ┃ 79…  │       │          │           ┃
  17 │ branch ┃ 01    │ c   ┃ 6a…  │       │          │           ┃
  18 │ branch ┃ 01    │ l   ┃ 4b…  │       │          │           ┃
  19 │ branch ┃ 02    │     ┃ 36…  │       │          │           ┃
  20 │ meta   ┃ 80    │     ┃      │       │ 3D9174.0 │ kvdb\x01… ┃
```

In the illustration above, the `KEY` column signifies what bytes are
stored in the key (index) portion of the underlying storage and the
`VALUE` column what bytes are stored as the value. Split columns like
`Level` and `Key` indicate a logical part of key or value data. Clocks
are shown as `time.count`, where `time` is seconds (hex) since the kvdb
epoch (2024-10-04).

An entry like `c=casey` is stored 1:1 as a key-value pair with some
additional metadata: a hash byte, a flags byte, and a clock. Values of
up to 30 bytes use this compact 10-byte header, storing only the first
byte of the entry's keyed hash — just enough for the boundary decision.
Larger values store the full 32-byte hash instead, so two big values can
be compared by hash without transferring them. The two layouts are
discriminated by total value length.

The flags byte currently holds one flag: *tombstone*. Deleting a key
replaces its value with a header-only tombstone entry that keeps its
place in ordering and hashing, so deletions synchronize like any other
change (and carry the deletion's clock). A separate "delete and forget"
operation erases the entry without a trace, making the key look like it
never existed rather than deleted.

The clock is a hybrid logical clock[^1] which is monotonic across all
participants and is used for conflict resolution during syncing. This
clock value is also the identity of a transaction: entries with the same
clock value are part of the same transaction. In the example above we
can see that entries `a=ali` and `b=barb` were modified together as one
change, while `c=casey` was a lone change.

Branch entries like `01 c = 6a…` represent logical branches and are what
makes synchronization efficient. As entries are added, modified, and
removed, these branch entries are maintained incrementally by kvdb (the
bulk of the work kvdb does, beyond syncing). The empty-key branch at
each level (#16 and #19 above) is that level's anchor.

The `meta` entry serves two purposes: it acts as an anchor for scans —
the tree root is always the entry immediately before it, `02 = 36…` in
the example above — and it stores metadata about the database, including
the database clock (the clock of the most recent committed transaction,
rewritten on every commit).

The metadata is currently as follows:

```
NAME           TYPE    MEANING & NOTES

magic          u8[4]   "kvdb"
version        u8      Version of kvdb storage semantics.
                       Currently 0x01.
flags          u8      Bit 0: syncable — the database maintains the
                       tree, hashes, and clocks described here.
                       All other bits must be zero.
branch_factor  u8      Average branching factor, as asked for; the
                       average actually produced is coarser unless it
                       divides 256 (see above). If this changes, all
                       branches must be recalculated.
reserved       u8      Must be zero.
replica_id     u8[8]   Random identity of this replica. Local
                       diagnostics only; not part of any hash or the
                       sync protocol.
clock          u8[8]   Database clock (big-endian HLC); the clock of
                       the most recent committed transaction.
hash_key       u8[32]  Key for the keyed BLAKE3 hash function, shared
                       by all replicas of one database. For sync
                       compatibility and mitigation of hash attacks.
```

Unsupported versions, flag bits or reserved bytes refuse opening without
rewriting the stored metadata. Syncable writers repeat this check when reading
the committed clock.

Plain (non-syncable) databases skip all of the above: raw entries, no
headers, no clocks, zero overhead — that mode is the plain Core v1
store.

Sync protocol details are documented in `src/sync.rs`; the
storage contract in `src/storage.rs`.

## Example programs

Two runnable CLI examples live in `examples/`:

- `storage` — basic operation: a plain database, no sync, no CRDT.
- `kvdb` — full-featured operation: a syncable database with TCP sync
  between replicas (all databases created by the example share a fixed
  demo hash key so they can sync with each other).

```
cargo build -p kvdb --examples

target/debug/examples/storage /tmp/s.db set greeting hello
target/debug/examples/storage /tmp/s.db ls

# terminal 1: serve a database and edit it interactively
target/debug/examples/kvdb --listen /tmp/a.db
replica 79a19690356ca5e0 (tcp:127.0.0.1:52341)
Interactive mode (ls, set, get, rm or help)
> set greeting hello

# terminal 2: pull it into a second replica
target/debug/examples/kvdb /tmp/b.db pull tcp:127.0.0.1:52341
target/debug/examples/kvdb /tmp/b.db get greeting
```

Run either with `--help` for the full command list.

## Storage benchmark

The benchmark programs live in `benchmarks/` and are built as example targets,
so the `--example` commands below address them by name.

For outgoing changeset combination, run
`cargo run --release -p kvdb --example combine_deltas` separately. It measures
1,000 folds of 32 deltas totaling 32 KiB, for distinct and repeated keys, including
validation/allocation and verifying the resulting replica root. Fixture writes,
storage, authorization/projection, networking and UI are outside this CPU boundary.
This operator benchmark is never executed by CI and has no timing assertion.

Native tree scans use `StorageTxn::visit`: one retained range iterator exposes
borrowed keys and bounded `ValueRead` access during each callback. Returning false
stops the scan. A callback cannot retain a value or mutate its transaction. This
keeps large values out of tree scratch space and avoids reopening a table and
seeking again for every adjacent entry. Backend implementations must preserve
forward/reverse bounds, read-your-writes and snapshot isolation.

Build once, then run selected cases on an idle machine. Start with raw redb;
compare kvdb only after recording that backend baseline:

```sh
cargo build --release -p kvdb --features redb --example benchmark
cargo run --release -p kvdb --features redb --example benchmark -- raw_redb_file_fsync 32
cargo run --release -p kvdb --features redb --example benchmark -- raw_redb_memory 32
cargo run --release -p kvdb --features redb --example benchmark -- kvdb_plain_file_fsync 32
cargo run --release -p kvdb --features redb --example benchmark -- kvdb_syncable_file_fsync 32
cargo run --release -p kvdb --features redb --example benchmark -- kvdb_delta_file_fsync 32
```

Repeat with `10000` keys and each layer's `_memory` counterpart. Handles remain
open across 100 warmup and 1,000 measured writes. Each write overwrites one
existing 16-byte key with a 32-byte value; the cache is 512 KiB. There is no
UI, network, or encryption. The in-memory cases use redb's
`InMemoryBackend`, not kvdb's snapshot-cloning `MemStorage`, and are nondurable
CPU comparisons. Each case verifies every value before measurement, afterward,
and after reopening its database handle.

Compare the default back-to-back workload with `--pause-ms 250`, which idles
before each measured write, outside its timing. Use `--samples 100` for an
initial diagnostic, then the default 1,000 samples for qualification. Sample
counts are bounded to 1–10,000 and pauses to 0–1,000 ms. For example:

```sh
target/release/examples/benchmark --samples 100 --pause-ms 250 raw_redb_file_fsync 32
target/release/examples/benchmark --samples 100 --pause-ms 250 kvdb_delta_file_fsync 32
```

JSON records the pause and sample count. Bursts do not establish paced-input
latency; keep the fixture, device and durability identical when comparing them.
On macOS it also reports the requested thread QoS class and relative priority;
this does not reveal temporary overrides or process-wide policy limits.

One JSON line per completed case contains all samples, paired wall/CPU timings,
begin/put/commit intervals, backend I/O counts/bytes/times, and allocation
counts. Counters and clock-probe overhead are included. The per-sample
`software_plus_non_sync_io_ns` subtracts that sample's sync time from its total;
it includes scheduling and other I/O and is not a CPU measurement. CPU time is
measured separately with `CLOCK_THREAD_CPUTIME_ID`. Requested allocation bytes
are not peak RSS. These isolated measurements do not qualify concurrent work
or application responsiveness.

With no arguments, the program runs all fsync/memory cases, raw redb first.
`all` explicitly includes `_file_fullfsync` comparisons (redb's platform-default
`File::sync_data`, full flush on Apple). A specific full-flush case can also be
selected. Omitting the key count runs both fixture sizes. To avoid Cargo work
during timing, invoke the built `target/release/examples/benchmark` directly
with the same arguments. Collect JSON outside the public source tree.

## Testing

`cargo test -p kvdb --features redb` runs, among others:

- randomized model tests against `BTreeMap` across branch factors, with
  the tree structure verified byte-for-byte against a from-scratch rebuild
  after single steps;
- insertion-order independence of structure and root hash;
- multi-replica convergence under random edits, deletes, and random
  pairwise syncs, including exact-clock-tie symmetry;
- sync framing under fragmented delivery, tiny message caps (MORE
  chaining), hostile input, and a sublinear-transfer bound.

`cargo build -p kvdb --target wasm32-unknown-unknown` (or
`thumbv7em-none-eabi`) verifies the no_std core.

Line coverage of production code is measured with [cargo-llvm-cov] and
enforced at >= 99% in CI. Tests live in `tests.rs` files beside the module
they cover and are excluded from the measurement, as are the ignored
benchmarks:

    cargo llvm-cov -p kvdb --features redb \
        --ignore-filename-regex '(tests|prepared_bench)\.rs$' --text --output-path coverage.txt
    python3 playbit/kvdb/scripts/line-coverage.py coverage.txt 99

The script counts a line as covered when any monomorphization executed it
(llvm-cov's own summary credits a generic function with a single
instantiation). The remaining uncovered lines are defensive arms that
consistent storage and control flow cannot reach.

Fuzzing uses [cargo-fuzz] in the shared [Playbit fuzz workspace](../fuzz/README.md).
From the repository root, run
`cargo +nightly fuzz run --fuzz-dir playbit/fuzz --debug-assertions <target>`.
Three database targets live in `playbit/fuzz/fuzz_targets/`:

- `kvdb_wire` — arbitrary and bit-corrupted sync streams into both protocol
  state machines: malformed input must error, never panic.
- `kvdb_db_ops` — arbitrary operation sequences checked against a model, plus
  the canonical-structure invariant (replaying the final state reproduces
  the identical root hash).
- `kvdb_sync_pair` — arbitrary interleaved edits and pulls on two replicas,
  which must always converge.

CI runs each target for a short smoke window on every push.

[cargo-llvm-cov]: https://github.com/taiki-e/cargo-llvm-cov
[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz

## Research

- https://hal.inria.fr/hal-02303490/document
- https://0fps.net/2020/12/19/peer-to-peer-ordered-search-indexes/
- https://www.dolthub.com/blog/2020-04-01-how-dolt-stores-table-data/
- https://www.dolthub.com/blog/2022-06-27-prolly-chunker/
- https://github.com/attic-labs/noms/blob/33124142b7ea054f2c3767e7f9ff10b4acba8045/doc/intro.md#prolly-trees-probabilistic-b-trees
- https://joelgustafson.com/posts/2023-05-04/merklizing-the-key-value-store-for-fun-and-profit
- https://jzhao.xyz/thoughts/Prolly-Trees

[^1]: [Logical Physical Clocks and Consistent Snapshots in Globally Distributed Databases](https://cse.buffalo.edu/tech-reports/2014-04.pdf)

### Pushed transaction deltas

Use `db.begin_write_with_delta(now, max_bytes)` and
`txn.commit_with_delta()` to capture a transaction's final written entries.
A successful commit returns `CommittedDelta::Bytes(bytes)` or
`CommittedDelta::PullRequired`. Failed commits expose no publication. The
budget includes the entire wire message; oversized writes still commit and
request ordinary pull reconciliation. Forgetting an entry also requests pull;
normal synchronized deletions must use tombstones (`del`). The capture is
opt-in and ordinary transactions retain no change buffer.

Feed a received message, in any byte fragmentation, to
`DeltaReceiver::new(&db, resolver, now, max_bytes)`, then `process(input)`.
When it reports `SyncStatus::Done`, call `finish()` to commit atomically.
Malformed or incomplete input cannot be committed. The same `Resolver`/`Lww`
and originating entry clocks are used by both pull and push. Missing keys in
a delta are not interpreted as deletions. Each receiver consumes one message;
duplicates and reordered *messages* converge with LWW. Within a message the
transport must preserve byte order. Custom resolvers retain responsibility
for their own convergence policy.

For relays, both receivers provide `new_with_delta` and `finish_with_delta`.
The latter returns `(outcome, Option<CommittedDelta>)`, capturing only applied
changes and producing no publication for an unchanged merge. Integrators must
opt in for each writing path; kvdb does not run a global publication callback
or retain a durable outbox. A process failure between commit and publication
can lose a notification, so subscribers must pull after reconnect and upon a
`PullRequired` notice. Authenticate publishers and authorize database/channel
access outside kvdb. `DeltaReceiver` holds the single writer while input arrives;
apply a transport deadline and drop incomplete receivers.

The delta wire format is one frame capped at `min(max_bytes, MAX_FRAME)`:
`[size:u32, type:u16=6, flags:u16=0, version:u16=1, branch_factor:u8,
reserved:u8=0, hash_key_fingerprint:8, transaction_clock:8, count:u32]`,
then sorted unique `[key_length:u16, stored_length:u32, key, stored_entry]`
records. Integers are little-endian except the HLC's existing big-endian
encoding. This is a separate one-way message from pull protocol version 1.
No database ID or transport authorization is encoded in the frame.

### Clock-aware synchronization format

Plain and syncable stores use metadata version 1; the existing syncable flag
distinguishes their kinds. Syncable stores use pull protocol version 1.
Leaf clocks participate in branch hashes, and writing an equal value at a
new clock advances its version. Otherwise an old conflicting value delivered
after an equal-value refresh could win incorrectly. Compact and full entries
both carry this version into the root. Unsupported store and pull protocol
versions are rejected explicitly; there is no in-place upgrade or compatibility
mode.
Both peers must use pull protocol version 1. The catch-up optimizations do not
change stored data. Opening a rejected store does not erase it.

Clock advancement (`Clock::incr` and `recv`) is checked and returns
`Result<Clock>`. Exhaustion reports `Error::ClockExhausted` without overflow.
Write and merge transactions refuse to store a terminal clock lacking a
successor. Rejected remote clocks leave committed data and clocks unchanged.

To isolate overwrite costs while a read snapshot is retained, run
`cargo test --locked --release -p kvdb --features redb benchmark_pinned_snapshot_writes -- --ignored --nocapture`.
This reports begin/put/commit/total latency and file growth for 1,000 separate
4 KiB overwrite transactions over 32 records, with and without a pinned reader.
It also records positional-write count, bytes, write time, fsync time, resize
time and physical allocation. On macOS a third case reserves 64 MiB with
`F_PREALLOCATE` before the workload, without changing logical length. Compare
physical allocation before/after: redb resizing can discard the reservation,
so this case alone cannot establish whether allocation causes a latency spike.
The initialized-growth case writes new file space in 64 KiB chunks and charges
that work to the measured transaction, including write bytes and resize time.
Resize and file-write timings overlap in that case; do not add them together.
This is a diagnostic backend only, not production allocation behavior.
It is a lower-level diagnostic, not AP or UI qualification.

To separate first-write filesystem costs from database work, run
`cargo test --locked --release -p kvdb --features redb benchmark_file_page_writes -- --ignored --nocapture`.
Each case performs 1,000 separate 8 KiB positional writes, each followed by
ordinary fsync, into an 8,192,000-byte temporary file. Compare a reused page,
sequential unwritten pages, initialized pages, and macOS preallocation. Setup
time and physical allocation are reported separately; no redb transaction or
kvdb tree operation is involved.
