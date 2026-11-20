# Local event recording

Tracing is off by default. Native instrumentation publishes starts, ends and
numeric instants to bounded process-owned queues. Guest apps build complete
variable-size events in their own 16-KiB buffers and submit them with the single
`trace.record` syscall. Neither ingestion nor recording maintains a span tree.
An end can arrive before its start; missing endpoints and references stay missing.

```sh
pb trace start [SESSION_INSTANCE_ID] [--clean]
pb trace status [SESSION_INSTANCE_ID]
pb trace tail [SESSION_INSTANCE_ID] [--since TIMESTAMP]
pb trace stop [SESSION_INSTANCE_ID] [--clean]
```

The optional canonical base58 ID selects one running session instance. The owner
resolves it against the existing execution's trusted app scope; unknown, stale,
inaccessible and wrong-profile selectors fail. Commands do not launch an app or
engine. Profile-wide start/stop sets the default and clears instance overrides.
Targeted commands change only that execution, including when other instances
share its host process. Native process observations follow the profile default;
they are not attributed to whichever guest happens to be running.

Tail sends the last 10,000 retained events for the selected instance or profile,
then follows new events. `--since TIMESTAMP` instead sends every retained and
new event with a strictly greater event timestamp. Definitions are replayed so
names remain available. A stopped recording ends successfully; loss and
incompleteness remain visible. Timestamps are unique within one emitting thread,
not across all producers, and buffered events can arrive out of timestamp order.
`--since` is a timestamp filter, not a lossless replay checkpoint.

The tail worker counts backward from the newest groups until it has enough
matching events, then replays the last 10,000 from its attachment snapshot.
Each pass advances one bounded block per turn;
it stores no event history, pins no segment and adds no producer bookkeeping.
Explicit timestamp filters need only one pass. Concurrent retention expiry is
reported rather than hidden.

Recordings live in `<profile>/traces/global/` and
`<profile>/traces/INSTANCE_ID/`, using canonical base58 instance IDs. Global files
contain native observations and profile metadata; instance files contain that
execution's events and metadata. Starting preserves existing recordings. With
`--clean`, profile-wide start/stop removes all recordings; a targeted command
removes only that instance's recordings. Start then enables a fresh generation;
stop remains off. Cleanup runs through the recorder so queued old observations
cannot recreate cleared data. Attached tails are invalidated; a partially sent
frame requires disconnecting that reader rather than breaking its framing.

The recorder, control clients and tail clients authenticate their local Unix
peers. Profile directories, sockets and retained files are private. iOS uses
bounded in-process socket admission because sandbox paths can exceed Unix socket
path limits. Four tail clients each retain at most one frame; slow clients do not
block producers or retain rotated files.

## Wire and retained events

The guest event layout and encoding helpers are defined in
`playbit-api/src/abi/trace.rs`, with C layouts in
`playbit-api/include/playbit/sys/abi.h`. Host decoding and validation live in
`src/input.rs`. Submissions contain only concatenated events; generation and
cumulative producer loss are separate syscall arguments. Header integers are
little-endian; each event has a 40-byte header, up to 4 KiB of metadata and zero
padding to eight-byte alignment.
Metadata is caller-supplied UTF-8 JSON object data. Ingestion validates bounds and
framing without interpreting JSON. The CLI preserves malformed metadata bytes
and reports the parse error. A whole malformed batch is rejected before any
publication. Every outcome consumes the submission; callers never retry it.

The host supplies the execution incarnation and thread ID. Current guests have
one stable thread (`tid = 1`) across callbacks and set `reference_tid = 0`, meaning
the emitting thread. SDK timestamps are unique within that thread, including
across flushes and callback boundaries. Span tokens retain their recording
generation; a stopped capture's data cannot be relabeled into its replacement.

Each native shared queue entry and retained `Events` payload is an explicit
16-byte attribution prefix followed by one complete guest-layout event:

| Offset | Field |
| --- | --- |
| 0 | `scope: u32` (zero for native process observations) |
| 4 | trusted `tid: u32` |
| 8 | capture `generation: u64` |
| 16 | 40-byte event header, metadata, zero padding |

The block envelope supplies the random source incarnation. A nonzero app scope
identifies one execution incarnation within that source. Its definition records
the trusted instance ID, optional session/build identities and private declared
name catalog. Sources and app scopes are independent of OS scheduling threads.
Native explicit references may name another native thread; there is no ambient
context propagation or implicit parent installed around host calls.

New event payloads use block tags 13–18. Segment framing remains `PBTRAC01` with
checksummed independent blocks. Historical semantic payload tags are unsupported
and reported explicitly; they are never decoded as the new events or removed by
cleanup. Start/restart follows the existing bounded retention policy.

## Bounds and loss

The existing 8-MiB aggregate allocation allowance includes all eight admitted
source mappings, their native name/TLS/path and bounded attachment state, five
16-KiB SDK rings per admitted source, and a 1,280-KiB recorder/tail allowance. A
compile-time bound enforces that accounting. OS socket buffers and allocator
metadata are separate bounded platform overheads.

Each process has 32 reusable 16-KiB producer queues, five active app catalogs and
at most 1,024 native names. The recorder admits eight sources and eight pending
registrations. Collection handles at most 256 events per turn. Emission performs
bounded atomic publication and only wakes the event-driven recorder when needed;
there is no timer, idle polling or application wait for disk or readers.

Retained bytes across all instance and global directories may occupy at most
20% of currently available filesystem space, checked before write admission.
The recorder evicts the oldest segment groups across instances until the next
bounded write fits. If `T` bytes are retained and `F` bytes are free, reserving
against `(F + T) / 6` accounts for the space the trace itself consumes. Other
filesystem users can change available space between those checks.

Groups contain at most 16 MiB on desktop or 4 MiB on iOS across up to 41 streams.
A bounded roster lets readers merge those streams in recorder sequence order
without retaining all historical files or pinning an evicted file. Start creates
fresh group files and never truncates existing groups. Desktop write admission
remains an 8-MiB burst and 1 MiB/s; iOS retains a 256-KiB burst and 16 KiB/s
allowance. Event writes leave one second of the same allowance for catalogs,
control boundaries and loss. Segment rotation and explicit capture reset serialize
with one bounded local file copy. Socket writes occur after the copy lock is
released, so slow clients cannot hold storage ownership or block recording.
Rotation reserves 2 MiB for a full collection turn, including maximum catalogs,
events, loss and retirement metadata. Each control request prepares its own phase.
Full queues refuse whole events, including all
metadata, and count loss. Retained loss records contain scope and cumulative
lost-event count.
Delivery sequence gaps and retention expiry are
explicit. Stop never certifies that paused or crashed producers flushed their
tails; crash/retirement state is distinct from an application span ending.

`Span`, `Interval`, `mark` and `mark_value` are local native conveniences. `Gauge`
emits changed numeric values as ordinary instants. No viewer, causal model, seal,
span-completion protocol or automatic request-context propagation is retained.
