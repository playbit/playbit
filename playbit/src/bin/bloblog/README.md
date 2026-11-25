# Bloblog playground

A native testbed for docdb: opaque bytes in append-only logs belonging to a
Playbit volume. No docdb dependency or application schema is required.

[docdb](../../../docdb/README.md) is experimental and its schemas, bindings and
stored data are excluded from Playbit's first-release compatibility promise.
Transporting docdb bytes through bloblog does not extend that promise to their
contents.

Install the current [notarized Apple-silicon Playbit download](https://playbit.app/download/Playbit-macos-arm64.zip),
open Playbit, and sign in. Build this CLI from the matching release source;
the download's [manifest](https://playbit.app/download/macos.json) identifies
its exact revision. With Playbit running:

```sh
cargo build -p playbit --bin bloblog
# List volumes already available to this signed-in account:
target/debug/bloblog volumes
# Or create one (service database replication must be enabled):
target/debug/bloblog create-volume 'Multiplayer experiments'

# Replace VOLUME with its base58-encoded 16-byte ID. Create a named log entry in that volume.
VOLUME=YOUR_VOLUME_ID
target/debug/bloblog create "$VOLUME" 'logs/activity'
# Set LOG to the returned entry.id, a stable base58-encoded 15-byte ID.
LOG=YOUR_LOG_ENTRY_ID
# Supply a fresh base58-encoded 16-byte append ID and retain it with these payloads.
APPEND_ID=YOUR_APPEND_ID
target/debug/bloblog put "$VOLUME" "$LOG" "$APPEND_ID" 'first edit' 'second edit'
target/debug/bloblog get "$VOLUME" "$LOG"
target/debug/bloblog get "$VOLUME" "$LOG" CURSOR_FROM_PREVIOUS_GET
target/debug/bloblog watch "$VOLUME" "$LOG"
FILE_APPEND_ID=ANOTHER_APPEND_ID
target/debug/bloblog put-file "$VOLUME" "$LOG" "$FILE_APPEND_ID" edit.bin
```

`watch` waits on local engine notifications; it does not poll the AP. It prints
JSON pages when entries arrive. `put-file ... -` reads stdin. Each put waits for remote durable acceptance. Failed or uncertain puts require an
explicit repeat with the same ID and payload; restarting the CLI does not retry them. `PLAYBIT_PROFILE` selects an isolated profile
whose `playbit-engine` is already running. Run `bloblog` without arguments
for help.

To share a volume with a different account, use an ordinary Playbit session
invitation whose root volume is that volume, with volume read/write
permissions or entry overrides. Have each participant join and use the same volume/log IDs. The
playground does not create a new invitation or permission system. Owner devices
can access their owned volumes directly. Volume permissions and session
revocations apply to reads, writes, bootstrap, and publication.

## Using pb

The installed `pb` utility also supports every operation:

| Standalone command | pb command |
|---|---|
| `bloblog volumes` | `pb volume list` |
| `bloblog create-volume NAME` | `pb volume create NAME` |
| `bloblog create VOLUME NAME` | `pb bloblog create VOLUME NAME` |
| `bloblog put VOLUME LOG APPEND_ID TEXT ...` | `pb bloblog put VOLUME LOG APPEND_ID TEXT ...` |
| `bloblog put-file VOLUME LOG APPEND_ID FILE ...` | `pb bloblog put-file VOLUME LOG APPEND_ID FILE ...` |
| `bloblog get VOLUME LOG [CURSOR]` | `pb bloblog get VOLUME LOG [CURSOR]` |
| `bloblog watch VOLUME LOG [CURSOR]` | `pb bloblog watch VOLUME LOG [CURSOR]` |
| `bloblog sync` | `pb sync` |

Use `pb help bloblog` for log command help. These commands use pb's normal
profile selection and engine startup, with the same payload bounds, cursor
semantics and explicit append retries as the standalone utility.

## Rust API

Add `playbit = { path = ".../archipelago/playbit", default-features = false, features = ["process"] }` and Tokio to
your native application's dependencies:

```rust,no_run
use playbit::client::{Id, bloblog};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let volume = Id::parse("99dn6s7bZoVpjzYciVNgN")?;
    let log = bloblog::create(volume, "logs/activity".into()).await?;
    let append_id = Id::generate()?; // Retain this ID and payload before submitting.
    let receipt = bloblog::put(volume, log.id, append_id, vec![b"opaque edit".to_vec()]).await?;
    let page = bloblog::get(volume, log.id, Some(receipt)).await?;
    for entry in page.entries {
        // Feed entry.data to docdb. entry.id is stable across replication.
        println!("{}: {:?}", entry.id, entry.data);
    }
    // Save page.cursor. If page.more, immediately fetch the next page.
    Ok(())
}
```

Use `None` to read from the beginning. Empty blobs are valid. Calls are bounded:
1–128 blobs per put, at most 32 KiB per blob, and at most 64 KiB including binary
framing per batch/page. A page has `entries`, `cursor`, and `more`; each entry
contains `data` plus immutable identity/origin metadata. The async library
runs IPC off the Tokio executor, with at most eight admitted calls per process.

## Cursor and durability semantics

- Cursors are opaque and scoped to a volume and log. Passing one to another
  log is an error. Keep the last **get** cursor when continuing an existing read;
  a put receipt is not proof that the caller consumed earlier entries.
- Each replica assigns an HLC arrival position when it first accepts an entry.
  Canonical entry IDs travel with entries. A recovered committed entry receives a
  new local arrival position, so an earlier read cursor cannot hide it.
- A cursor from another replica incarnation replays from the beginning. This
  trades duplicate delivery for correctness, without a clock-skew margin or a
  shared local arrival sequence. Restore/rebootstrap may replay entries too.
- `put` returns after AP durably accepts the whole batch. Its receipt starts before
  the original batch, including on a duplicate-ID call; it may replay earlier entries.
  Same account/volume/log/ID and ordered payloads return the same logical success.
  Different payloads with that ID conflict. Different IDs create distinct appends.
  Timeout or disconnection can be uncertain: repeat the same ID to resolve it.
- `get` returns nonempty cached pages offline. The next call beyond the cache reports
  `Unavailable` if AP cannot be reached. Online empty reads require a bounded remote
  observation. `more=false` means no further locally available page.
- Legacy pending entries remain hidden from normal reads and available for explicit
  recovery. Copying a log retains its payloads as unconfirmed data in the new log;
  those payloads need explicit online submission before they are visible there.

## Storage and replication

Each log is a named kv database entry with an append protocol. Its 15-byte entry
ID selects its backing file and remains stable across rename. The volume index
owns the name, permissions and retirement; volume snapshots include its backing
and checkpoints. Its HLC and ordinary storage transactions are reused, while
its append records use their own AP operations and arrival cursors. Ordinary
kvdb full sync never merges append records or local checkpoints. Log payloads
are independent of named file/blob entries.

The CLI/API and AP store and return payload bytes exactly as supplied. Callers
own serialization, payload encryption and cryptographic authenticity; plaintext
payloads are readable by AP. Entry IDs, clocks and integrity checks remain
visible to AP and do not authenticate a payload against a malicious service.

The lower-level `store::bloblog::Log` retains an optional `encryption_key` for
client/peer storage. `Some(key)` encrypts stored entry values and decrypts them
on reads; `None` stores the ordinary single-entry encoding. AP always uses
`None`. This storage option does not change wire entries or encrypt outgoing
payloads: callers requiring E2EE through AP must encrypt payload bytes before
appending. Encrypted storage is currently supported on native clients; Web
rejects `Some(key)` explicitly.

AP commits the canonical batch, ordered reference and append outcome in one
conditional central-record transaction. Local AP indexes are disposable projections
of those committed batches; restoration never promotes uncommitted local entries
or legacy S3 objects. Canonical IDs deduplicate projection repair and repeated calls.
The volume database key remains required for kvdb hashing and metadata access;
it does not encrypt bloblog payloads.

Publications share the existing volume subscription, sequence/gap detection,
authorization and queue budgets. Up to 2,048 bytes including the catch-up cursor
are sent inline; larger batches send a volume bloblog hint and recipients fetch
bounded pages. Bootstrap/reconnect catch up committed entries after subscription
establishment; they never submit appends. No freshness timer or per-log subscription is added.
Native, Wasm and browser guests can append and read through the
[bloblog syscalls](../../../../playbit-api/syscalls.md#append-only-blob-logs) and
Rust `Context` helpers. Browser append owns one bounded online request, independently of local edits.

This is a testbed, with the existing AP admission/fanout limits (including 16
subscriptions per volume). It requires service-readable volumes. Private-volume
peer transport, multiple APs exchanging live bloblog publications, compaction,
retention and optimized small-edit packing remain outside this playground. Read cursors retain local arrival positions.

## Verification and budgets

The changed journey is CLI append/read/watch across signed-in engines. There
are no new graphical controls or startup workers. Cached reads and local
interaction retain the under-8-ms/next-frame budget; append completion includes
remote storage and network time. Native log calls have eight admission slots
and a 60-second deadline. Network waits run outside local storage workers and
unrelated editing can proceed. Qualification measures remote append completion separately from local responsiveness.

```sh
cargo test -p playbit
cargo build -p playbit --release --bin bloblog
cargo build -p todo --release --target wasm32-unknown-unknown
# From service/ap/:
cargo test --test bloblog
# Operator-only, optimized 1,000-sample IPC measurement:
PLAYBIT_QUALIFY_PERF=1 cargo test --release --test bloblog -- --nocapture
```

The integration test uses three real local engine owners, authenticated loopback
QUIC, simultaneous writers, caller-encrypted/empty/maximum-size payloads,
inline/32-KiB updates, explicit same-ID replay, offline cache reads and reconnect,
read-only authorization, cursor scoping, and the built CLI executable. Tests need
permission to open local sockets. Unit tests cover binary/empty payloads,
pagination, delayed clocks, peer changes, immutable identities, payload preservation,
restart and bounds. Loopback/file tests do not establish deployed S3 throughput,
WAN latency, native GUI presentation or multi-AP fanout performance.

Historical measurements with AP-encrypted bloblog storage, September 16, 2026:
Apple M5 Max MacBook Pro (Mac17,7), 128 GB,
macOS 26.6.2, optimized build, local durable files and loopback AP, 1,000
100-byte puts. Three local engines remained connected. Local put: 1.52 ms p99
(2.50 ms max); local get: 1.36 ms p99 (1.74 ms max). Correlated writer-request to
reader-observation latency: 3.59 ms p99 (5.90 ms max), including IPC, local disk,
AP transit/storage/fanout, and the reader's notification/get. These cover the earlier local-completion implementation; there is no pre-change bloblog baseline. Power/thermal state was
not controlled. This does not qualify actual S3 or WAN performance.

A separate traced run observed 1,007 AP fanouts at 6.38 microseconds p99
(15.96 microseconds max), with zero dropped trace records. The marked connected
inline/large-edit interval made no kvdb freshness pulls. Traced end-to-end p99
was 4.60 ms; trace overhead is not qualified by subtracting independent
percentiles. Idle receive-coroutine storage remains within the existing 64 KiB
per-account bound (56,576 bytes for 32); catch-up allocates its transient request
state only when needed, with one bounded page per active subscription.

Historical live deployment checks also passed with two packaged engine processes against
AP1: two writers, inline notification, 8-KiB notification/fetch, restart and empty
cursor reads. All three S3 objects authenticated and decrypted with the volume
database key; a wrong key was rejected. This is a functional WAN/S3 smoke check,
not a throughput or latency qualification.
