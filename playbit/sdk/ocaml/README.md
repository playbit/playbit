# OCaml SDK integration

The installed workflow is `pb init --lang ocaml` followed by `pb run`. See
[developer SDKs](../../../docs/developer-sdks.md) for tool selection, supported
modules and downloadable SDKs. The maintained app and starter source is
[`guests/counter-ocaml/`](../../../guests/counter-ocaml/README.md).

## Source layout

- `playbit.ml`, `playbit.mli`: handwritten OCaml API, errors and event retry handling.
- `playbit_ffi.wat`: GC buffer adapter and bounded linear-memory scratch storage.
- `bindgen/`: Rust AST generator for ABI constants and scalar syscall wrappers.
- `template/`: starter files combined with the counter guest during app packaging.
- `toolchain/`: isolated compiler setup, release bootstrap and pinned opam dependencies.
- `tests/`: compiler/binding contract checks and web browser compatibility harnesses.
- `_build/`: ignored compiler dependencies, generated inputs and test artifacts.

Shared toolchain assembly/signing/qualification tools live in `../toolchains/`.
The wasm32 ABI fixture lives in `playbit-api/tests/wasm-layout/`.

## Binding maintenance and target contract

`bindgen/` parses the existing Rust syscall enums, six selected wrappers,
error/event values, host limits/platforms and view wire constants.
It emits OCaml declarations/constants and scalar Wasm wrappers. The compiler
fork contains no syscall opcode table. Wrapper signatures or bodies outside the
supported AST forms fail generation and require explicit review.

A small operation annotation list describes buffer directions and result types.
`playbit_ffi.wat` copies borrowed buffers between OCaml GC arrays and bounded
linear memory. `playbit.ml` handles errors and event capacity retries. Event
buffers carry the 24-byte Event record with caller capacity `size = 24` and
output extent `written = 24`; generation, kind and target retain all bits using `Int64` and `Int32`. The wasm32
Rust fixture checks the C event header layout; runtime tests compare complete
scenes with the Rust encoder. Semantic syscall changes still require reviewing
these adapters. `poll` returns one event at a time, preserving known events
already returned before an unsupported mandatory kind. Unmarked unknown kinds
and marked existing IDs raise `Host_error Unsupported`. Future optional advisory
kinds retain the high `optional_event_kind_bit` and raw payload; callers may
ignore them but must count them and their payloads within ordinary turn budgets.
See the [event requirement classification](../../../playbit-api/src/abi/README.md#event-requirement-classification).

Supported calls include cryptographically secure `random_fill` (at most 64 KiB),
log, submit view, poll event, host limit/platform and
quit-on-last-window; typed automatic-volume selection, asynchronous entry resolution and
paginated KV key listing and KV read/write; session presence and live channels;
asynchronous audio output, PCM sounds
and independent playback instances with position, gain and stereo balance controls.
Structured adapters marshal caller-owned wasm32 records through a bounded private
six-slot bridge. Signed syscall results use boxed `Int64`; handle-producing calls
return positive 63-bit capabilities directly. Error codes and bounded public
scalar results remain `Int32`, with checked conversion. Adapters initialize output record
capacities and validate each required written prefix before reading outputs.

`Data.volume` selects `Root`, `Session`, `User`, `User_session`, `Local` or
`Local_session` by the host's typed role, independently of names and tags.
`Data.volume_info role` returns the handle and write permission from the same
selection. The permission in `can_write` is a snapshot.
`Data.root_volume ()` preserves selection by the released Root flag, including
older hosts without a role tail. Listing roles does not initialize data;
the first admitted data operation does. Local-session data survives stopping and
resuming. Apps using these scopes require `automatic-volumes.v1` in
`required_host_features`.

`Session.presence ()` reads the host's current membership snapshot. Refresh it
on `EventKind.presence_changed`. Each member is a binary tuple of three 16-byte
IDs in account, device, instance order. `Session.identity ()` supplies the
local context's tuple. The transport can report a different device, so
`Session.same_instance` compares only the account and instance. Adopt a transport
tuple only when exactly one member matches, and leave it unresolved when there
are zero or multiple matches. Match authenticated senders from `Session.message`
against the full account, device, and instance tuple.

`Session.open_channel` opens a named live channel after the session's Shared
backing is initialized. Subscribe, publish, unsubscribe, and close asynchronously.
Match a `DataReady` event to the operation using `Session.ready`, then call
`Session.take` on the operation handle. Subscription completion establishes
readiness. Unsubscribing leaves already queued guest messages available to poll. Publication completion acknowledges a delivery attempt.

`Session.unsubscribe` stops delivery and keeps the channel handle available for
publishing or resubscribing. `Session.close` releases the channel handle as soon
as the host admits the operation and removes the subscription asynchronously.
Stop using that handle and take the close operation to complete cleanup. The instance can hold 16 named
handles at a time. Close unused names to make space for new channels. Reopening
the same name before closing returns the existing handle. After closing, finish
cleanup before subscribing under the new handle so the old unsubscribe does not
race the new subscription.

`Session.message` returns the channel, sender,
and payload of a live message, including subscribed publishers' own messages.
`Session.status` reads a terminal stream status such as `Unavailable` or
`LimitExceeded`. The host restores desired subscriptions on reconnect. Guests
request any needed fresh application state after a disconnect or dropped messages.

`monotonic_ns ()` reads the host's monotonic clock in nanoseconds. `wake_after`
replaces a guest's pending one-shot `Wake` event with a delay of 1,000 to
86,400,000 milliseconds. Use display-driven frame events for visible animation.

The view helper covers Column, Row, Text, Button, Context Menu and Canvas
rectangles, text and packed pixels. `View.context_menu` wraps a single child
with a list of action IDs and labels. macOS opens the menu by secondary click
or accessibility activation. Choosing an item delivers `EventKind.click`
with the action ID as its target. Pixel canvases use tightly packed sRGB RGBA8
with nearest-neighbor presentation. Packed pixels require `ui.canvas.pixels.v1`
and allow at most 32 MiB across a scene, within the 64 MiB view limit. Pointer/key/frame/surface decoders validate their exact payloads.
For a root canvas that redraws to the available surface, use
`View.viewport_canvas` and declare `ui.canvas.viewport.v1`. The macOS host
clips the current scene to the resized viewport while the app handles
`EventKind.surface_changed`. The layout works for both static and continuous canvases.

`Audio.open_begin` / `Audio.open_take` asynchronously request audio output;
wait for operation-ready events before retrying a pending take. A preference of
zero selects the host default; `Audio.sample_rate` reports the negotiated rate.
`Sound.open_pcm` copies interleaved PCM16LE, mono or left/right stereo, at
8,000–192,000 Hz. The host resamples into stereo output. Up to 16 sounds and
1 MiB of PCM data belong to one guest generation.

`Sound.instance sound id` identifies an independent playback using any `int32`
bit pattern as its ID. Up to eight instances, including completed ones, may
exist at once. Playing a pair again replaces that playback; `Sound.stop`
destroys it. Gain is an amplitude multiplier from 0.0 to 1.0. Start, length,
and seek use source nanoseconds, rounded down to PCM frames; length zero
is invalid. Tell rounds the source frame up to a whole nanosecond for seek round
trips, reports the renderer position, and excludes device latency. To select all
frames, length is the ceiling of `frame_count * 1,000,000,000 / sample_rate`.
`Sound.loop` repeats the selected segment. Stereo balance ranges from -1.0
(left) through 0.0 (unchanged stereo) to 1.0 (right), attenuating the opposite
channel. Playback commands are consumed once outside retained scenes. The host
silences inactive instances and discards stale one-shots; loops resume only
while their owning instance is active. Headless and non-Apple hosts return
Unsupported. Declare exact required host features in the app manifest.

The compiler emits one module with `memory` and `pb_main : () -> ()`, no start
section, and only the scalar `playbit.pb_syscall` import. First entry initializes
OCaml; subsequent entries invoke `Callback.register "playbit.turn"`'s callback.
It uses CPS effects and modern Wasm exceptions. Unsupported host primitives fail
explicitly. JavaScript, WASI, Dynlink, arbitrary Unix operations and preserving
the OCaml heap across Wasm package replacement are outside this target.

Playbit enables Wasmtime's copying GC collector. Linear memory and both GC
semispaces share the 128 MiB guest memory budget; VM metadata and native
stacks are additional process memory. Allocation failure and uncaught exceptions
halt that guest generation. Web browser compatibility tests have no per-module GC heap
limiter and do not qualify production web admission.

## Compiler development and checks

Compiler sources are maintained in
[`playbit/js_of_ocaml`](https://github.com/playbit/js_of_ocaml). Setup clones them into
`_build/js_of_ocaml` when needed. Set `JS_OF_OCAML_ROOT` to the absolute path of an
existing checkout to work on compiler changes there.

With OCaml 5.3.0 and opam on PATH, run from the repository root:

```sh
sh playbit/sdk/ocaml/toolchain/setup-wasm.sh
export SCCACHE_SERVER_UDS="$PWD/_sccache.sock"
gmake -C playbit/sdk/ocaml wasm-test
```

Setup records the compiler's location and installs the pinned dependency list in
`_build/opam`, without modifying another opam switch. Binaryen 133 is downloaded
with a verified checksum. The fixture builder in `tests/build-wasm.sh` compiles
`guests/counter-ocaml/src/app.ml` directly using the local fork; it atomically
publishes `_build/wasm/{debug,release}/counter.wasm`. It is a compiler test helper;
ordinary projects use the managed driver through `pb build`.

`gmake -C playbit/sdk/ocaml wasm` builds only the debug fixture;
`gmake -C playbit/sdk/ocaml wasm-release` builds the optimized one.
`python3 playbit/sdk/ocaml/tests/test-wasm.py` checks the target and bindings after
the debug fixture is built. The runtime check runs the actual OCaml app on
Cranelift and Pulley, compares its scene with Rust, and exercises retained state,
reset and event capacity retry. Contract checks cover initialization once,
caught/uncaught exceptions, malformed buffers, full-width integer bits and
unsupported primitives. The standard runtime suite also checks cyclic GC
reclamation, combined memory exhaustion, traps, fuel and GC with atomic lowering.

For the trusted-module compatibility harness in installed Chrome, Firefox and
system WebKit, run from the repository root:

```sh
sh playbit/sdk/ocaml/tests/build-browser-tests.sh
node playbit/sdk/ocaml/tests/check-browsers.mjs
```

The compatibility build needs the current Rust host's wasm32 artifact and web build
dependencies. It downloads the pinned official Firefox driver into `_build`.
Each web browser receives isolated test storage; the harness uses the production
syscall/scene adapters and Rust work owner. System WebKit uses a nonpersistent
WKWebView. `node playbit/sdk/ocaml/tests/check-browsers.mjs safari` additionally
uses Safari WebDriver when remote automation is already enabled.

The runtime and web browser harnesses report diagnostic timings. They do not establish
packaged startup, next-frame interaction or physical-iPhone qualification. Keep
benchmark outputs and screenshots outside Git.
