# Playbit fuzzing

This standalone cargo-fuzz workspace covers the runtime, app ABI, AP and local IPC codecs, store, packages and kvdb. It
does not build as part of the stable root workspace. All thirteen targets share one
instrumented dependency build. Its native-runtime feature links the real C ABI
implementations retained by sanitizer coverage in the SDK. The host fixture is available only through
Playbit's existing `test-support` feature and cannot access files, the network,
or native presentation.

- `trace_decode` checks packed guest event/batch validation, variable metadata
  extents, trusted event attribution, app catalogs, segment/block framing and
  control status. Valid checksummed seeds reach the outer decoder; direct payload
  mutation reaches event validation independently of the checksum.
- `local_ipc` checks validated rkyv request/reply access, including cursor and log
  hooks, mutated valid archives, truncation, misalignment, frame prefixes,
  fragmented reads and receive-credit limits. Valid archive fixtures use the
  production serializer. This target does not dispatch operations, materialize
  unadmitted requests/replies or qualify operation-level resource admission.
- `package_contract` checks canonical bundles, bounded display metadata, manifest
  text, generated trace declarations, Wasm/Mach-O metadata sections and
  revalidation of mutated package fields. No filesystem operations.
- `store_contract` checks bounded invitation text/envelopes, schema evolution,
  field merges, ACLs, recipient delta projection, deterministic blob sealing and
  compact client/account-record framing, read/write requirements and opaque preservation.
- `ap_codec` checks frames, testing HELLO, named-database replica events and scoped terminals, plus
  operation JSON validation and native WebSocket stream packet admission. Seeds include every operation kind and wire event kind.
- `decode_view` sends arbitrary bytes through the scene and nested-menu decoders.
- `abi_reader` selects up to 32 Reader operations from the input, in arbitrary
  order, including continued reads after errors and finishing/resetting a reader.
  It checks that cursors remain bounded and monotonic between resets, and that
  admitted surface-event payloads encode back to exactly the input bytes.
- `wasm32_memory` checks scalar-ABI nested field/value/schema records, pointer/count bounds,
  output overlap, allocation limits and fuel admission. It also admits object
  output descriptors/arrays and checks that required-count publication changes only
  those three counts. Seeds include valid values and nested/output aliases; no VM,
  storage or host mutation occurs in this decoder target.
- `wasm_operations` invokes the two production scalar syscall imports through compiled
  Pulley wrappers. Inputs select an operation, six scalar arguments, and initial
  guest memory. Each call resets 2 MiB of memory and fresh deterministic authority,
  pending operations, resources, and an update. It checks queue/log/handle invariants,
  unchanged memory on errors other than count-only `BufferTooSmall`, and that
  rejected mutations never reach the host. The wrapper is compiled once per thread;
  no filesystem, network or presentation is available.
- `kvdb_encoding` exercises arbitrary storage headers, flag validation, metadata and clocks,
  plus valid compact/full value round trips.
- `kvdb_wire` checks arbitrary and corrupted sync streams against both protocol
  state machines.
- `kvdb_db_ops` checks database operations against a model and verifies canonical
  tree structure after every operation with the production verifier.
- `kvdb_sync_pair` checks convergence after interleaved edits and synchronization, verifying both trees after each operation
  and each completed exchange.

Install nightly Rust and cargo-fuzz, then run from the repository root:

```sh
python3 playbit/fuzz/seed-corpus.py
cargo +nightly fuzz build --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions decode_view -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions abi_reader -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions local_ipc -- -max_total_time=1800 -max_len=1048576
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions wasm_operations -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions wasm32_memory -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions kvdb_encoding -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions kvdb_wire -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions kvdb_db_ops -- -max_total_time=1800
cargo +nightly fuzz run --fuzz-dir playbit/fuzz --target aarch64-apple-darwin --debug-assertions kvdb_sync_pair -- -max_total_time=1800
```

Set `--target` to your host target. On ARM Linux with glibc, use
`aarch64-unknown-linux-gnu`. Keep ASan and debug assertions enabled.
Corpus files, crash artifacts, build outputs and the standalone lockfile are ignored. Generate seeds with `seed-corpus.py`;
never add generated `corpus/` files to Git.

`seed-corpus.py` provides valid nested scenes, exact record/depth limits, all
Reader selectors, every scalar syscall, invalid scalar arguments, and result buffers. Fuzzing mutates those
seeds and also generates malformed input. A clean run is evidence for the tested
inputs, not a proof that every decoder path or VM/JIT behavior was covered.
