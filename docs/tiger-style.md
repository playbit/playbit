# Playbit Tiger Style

This document says how Playbit's core is written. It is our adaptation of
[TigerBeetle's TIGER_STYLE](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
which itself builds on NASA/JPL's
["The Power of Ten: Rules for Developing Safety-Critical Code"](https://spinroot.com/gerard/pdf/P10.pdf)
by Gerard Holzmann. Where a rule is kept, the reasoning is theirs and we do
not repeat all of it here; read the original, it is short and the motivations
are the point. Where a rule is changed or dropped, this document says what
changed and why, so that nobody has to relitigate it.

The rules here are normative for the code they scope (see *Tiers*). They are
implementation constraints, not suggestions, in the same sense as the
engineering principles in [AGENTS.md](../AGENTS.md).

## Why

Playbit keeps people's data on their own devices and converges it between
those devices and the people they share with. That is a distributed system
with the usual failure modes: partitions, reconnects, replays, revocations,
and crashes in the middle of a write. It also runs other people's programs in
a sandbox and enforces who may read and write what. A bug in the core is not a
wrong pixel; it is lost data, two replicas that never agree again, or a
program reading something it was never granted.

Two more facts shape the style. Much of this code is written and reviewed by
coding agents, and the apps that run on it are often written by language
models. A precise, mechanical style is a checklist for a reviewer and an
instruction for an agent. Unenforced style decays faster under agents than
under people. The enforcement section distinguishes checks that run today
from checks that remain to be implemented; review must cover those gaps.

## Tiers

Not all code carries the same consequence, and pretending otherwise makes the
rules noise where they do not matter and lets them slip where they do. Code is
assigned to a tier by what breaks when one of its invariants is violated.

Guest and example programs, including `guests/`, example programs elsewhere in
the repository, and `playbit/sdk/*/template/`, are outside these tiers and
exempt from Tiger Style, including rules labeled "All tiers." Write them in
plain, idiomatic code suited to the language and the example. Do not add
assertions, artificial limits, helper layers, or verbose error handling solely
to meet this document's rules. Normal correctness, API contracts, formatting,
and compiler and standard Clippy checks still apply. The runtime and
`playbit-api` retain their assigned tiers; the host's responsibility to contain
guest failures still applies.

| Tier | A violated invariant means | Where |
|---|---|---|
| A | Data is lost or corrupted, replicas diverge, a guest escapes its sandbox, or authorization is bypassed. | `playbit-api`, `playbit/kvdb`, `playbit/src/store`, `playbit/src/runtime`, `playbit/src/network`, `playbit/src/package`, the archive/signature/installation boundary in `playbit/src/desktop/update`, and the Rust web host in `playbit/src/platform/web`. |
| B | A session, connection, or recovery fails, recovers wrongly, or reports the wrong state to the user. | `playbit/src/engine`, `playbit/trace`, the shared panic policy in `playbit/src/platform/failure.rs`, the binaries in `playbit/src/bin`, and the Rust side of `playbit/src/platform/ios`. |
| C | Something looks wrong. Nothing is lost, because the data and the decisions live in tiers A and B. | `playbit/src/desktop`, `playbit/src/ui`, the Swift host in `playbit/platform/ios`, the TypeScript in `playbit/platform/web/src`, `scripts`, and the public website. |

Three rules apply across tiers:

- The boundary between tiers belongs to the stricter tier. A tier C shell that
  decodes a scene from the engine validates it once, at the boundary, under
  tier A rules; past that point it trusts the result.
- Tier C code never asserts on data it received from another tier. A GUI panic
  is a crash the user sees. It validates, degrades, and reports.
- Logic that belongs in tier A but is written in a language without the tier A
  tooling (today: parts of the TypeScript web host) follows the tier A rules
  the language can express, and is moved into Rust when it is touched. Fewer
  implementations of the same rule is the fix, not better TypeScript.

Every rule below states which tiers it binds. Where a rule says "tier A", it
is required there, recommended in tier B, and optional in tier C.

## Safety

### Simple, explicit control flow

All tiers. Prefer straight-line code, `match` on an enum, and loops with an
obvious bound. Avoid recursion. Where a data structure is recursive by nature
(a view tree, a menu tree) the depth is a named constant, the recursion asserts
the bound on entry, and the constant is small enough to reason about. Do not
write `async` where a function that runs to completion would do. Avoiding
suspension removes one opportunity for interleaving; it does not preserve
preconditions by itself. Writes, callbacks, reentrant calls, and other threads
can invalidate them. Maintain invariants through ownership, synchronization,
and transaction boundaries, and revalidate after operations that can
invalidate a checked condition.

Keep control flow in the parent and keep helpers non-branchy. A dispatcher
that matches on an opcode and calls one function per arm is the right shape
even when the match itself is long; the arms must not grow their own
branching.

### Limits on everything

All tiers. Bound queues, buffers, retries, tree depth, messages, allocations,
and work per operation or scheduling turn. State each bound as a named
constant or derive it from validated input under an enforced resource limit;
document where it comes from. An event loop may run for the lifetime of the
process, but each turn does bounded work before yielding or waiting for
readiness. Bound aggregate resource use as well as individual objects.

Check external input before admitting work. Assert the bounds inside code
that relies on them where the type or ownership structure does not already
establish them. An edge check catches bad input. An internal assertion catches
the bug that bypassed the edge.

### Plan allocation

Tiers A and B. TigerBeetle allocates all memory at startup and never again.
Playbit runs on a framework, a JIT, and a QUIC stack that allocate, and it
serves a variable number of sessions, so the literal rule does not apply.
Enforce individual and aggregate resource limits before admitting work, and
size allocations for the validated work. A maximum message size does not
require every message buffer to reserve that maximum. Account for retained
capacity across concurrent sessions and buffers.

Preallocate or reuse storage where needed to meet measured latency budgets.
Keep allocation and growth costs explicit in critical-path measurements;
avoid repeated growth when the required size is already known. Preallocation
must also fit the startup and memory budgets.

### Assertions

Tier A required, tier B recommended. Assertions are how invariants become
executable. They found real bugs at TigerBeetle before any test did, and they
turn a silent corruption into a loud stop.

- Assert meaningful preconditions, postconditions, and state invariants that
  types and existing checks do not establish. There is no per-function quota:
  getters and wrappers do not need redundant assertions to raise a count.
  Assertion density is an audit signal, not evidence of correctness.
- Pair assertions. The same property is checked from two different places,
  for example where a value is written and where it is read.
- Assert both positive and negative space: what must happen and what must
  never happen.
- Split compound conditions into separate assertions so a failure names the
  actual culprit.
- Assert relationships between compile-time constants with `const _: () =
  assert!(...)`, so a change to one constant cannot silently invalidate
  another.
- State the invariant positively (`index < count`), not the violation.

Assertions stay enabled in release builds by default, including in hot loops.
Use `debug_assert!` only when the check is diagnostic and production
correctness does not depend on it; document what establishes the invariant
without that check. Checks necessary for memory safety, authorization, or
data integrity must remain enforced in production. If performance motivates
omitting a check, measure its cost and record the evidence. A module comment
identifies its data-plane loops, but that classification alone does not
justify disabling assertions.

### The vocabulary: error, assertion, or unwrap

All tiers. Rust gives three ways to react to a condition, and each means
something different. Use them for what they mean.

| Condition | Write | Because |
|---|---|---|
| Input from outside this tier may be wrong: bytes from a guest, a peer, the network, disk, or the user. | `return Err(...)` with a specific error | It is expected, it is handled, and the caller decides. |
| A programmer error: an invariant this code and its callers promise. | `assert!(...)` with a message that says what was expected | It is a bug. Stop here, where the state is still intelligible. |
| A diagnostic invariant check whose omission is justified under *Assertions*. | `debug_assert!(...)` | Production correctness is established independently of this check. |
| A `Result` or `Option` that cannot fail here. | `assert!` or `expect("why it cannot fail")`, never bare `unwrap()` | A bare `unwrap()` is an assertion with no statement of what it asserts. |

Use `?` to propagate errors to the layer that can decide the outcome. Every
error path in tiers A and B has an owner: the function that decides what
happens, and a test that exercises that decision. Reporting failure and
terminating an operation can be correct handling. Logging an error and
continuing without restoring the required state is not.

### What happens when an assertion fails

All tiers. An assertion failure is a bug, and the reaction to it is a product
decision made once, here, not by whoever writes the assertion.

- **Host handling of a guest app failure.** The guest traps. With developer
  tools enabled, the guest stays halted and can be reloaded but not re-entered,
  so the developer sees the state that failed. Without developer tools, the
  guest stops with a clear message that names the app, not the runtime. The
  host never panics because a guest did. The failure is always logged locally;
  anonymous crash capture for app developers is a later step.
- **Everything else** (the engine, the CLI, the desktop and mobile hosts, the
  web host): crash, log, and, when there is a user, tell them what happened
  in plain words and what to do next. Acknowledge durable writes only after
  commit, and recover from committed state on restart. Durability does not
  prove that committed data is logically correct; recovery must validate the
  invariants it relies on and report corruption explicitly. A crash with a
  clear message is better than continuing in a state nobody understands.

### Functions and scope

All tiers. A function fits on one screen: 70 lines, counting comments and
blank lines. Tests are exempt from the limit but not from being readable; a
test states its goal and its method in a comment at the top. Declare a
variable in the smallest scope that needs it and as close as possible to its
use; a value computed far from where it is checked is a place-of-check to
place-of-use bug waiting to happen. Avoid independently maintained copies of
the same fact: they can drift apart. Borrowing existing state does not create
a second copy; use Rust's ownership and borrowing rules to express access.

### Sizes and integers

Tier A. At every ABI and wire boundary, integers have an explicit width and
byte order (`u32`, little-endian) and the encoding is fixed-layout. Inside
Rust code `usize` is idiomatic for indices and lengths and is used as such.
Treat index, count, and size as distinct quantities even though they share a
type: an index becomes a count by adding one, a count becomes a size by
multiplying by a unit, and every division says whether it rounds and which
way.

### Warnings

All tiers. Run the compiler and clippy at their strictest useful setting and
fail the build on warnings. A warning that is wrong in one place is silenced in
that place with an `allow` and a reason, never globally. `unsafe` is
forbidden in tier A crates except where a foreign interface leaves no choice,
and each such block states the invariant that makes it sound.

### Bounded, event-driven work

All tiers. TigerBeetle's rule is "do not react to external events, run at
your own pace", which for a transaction database means batching for
throughput. Playbit's contract puts local latency first, so the rule is
restated:

**React to readiness, never to a timer. Take ready work in bounded batches.
Local interaction never waits on the network.**

Concretely: no polling loops for freshness of anything; a queue or channel
wakes the consumer, and the consumer drains up to a bound and yields. Protocol
deadlines and bounded backoff after failure are allowed; periodic fetches
are not. Interactive work runs isolated from bulk and network work so that a
stalled transfer cannot delay a keystroke. The macOS updater checks its release manifest hourly while the
main app runs. Its installer uses event-driven process and lock waits.

## Performance

All tiers, in proportion to what the tier touches. The largest performance
wins are made in design, before the first line, by sketching the four
resources (network, disk, memory, CPU) for the workload in question and
finding the slowest one. Playbit already requires naming the workload, the
budget, and the measurement boundary before implementing a critical path;
this document adds the habits that make those budgets reachable:

- Batch. Amortize per-item cost across a bounded batch wherever the interface
  allows it, in the control plane. Do not add a batching delay to interactive
  work; batch what is already ready.
- Separate control plane from data plane. Validate and plan work before
  entering hot loops where that avoids repeated checks. Preserve checks for
  invariants that earlier validation cannot establish or that mutation can
  invalidate, including checks that must occur inside a transaction.
- Keep hot loops' data dependencies explicit. Where useful, pass only the
  values and slices the loop needs rather than an entire object. In Rust,
  `self` is a parameter; removing it does not guarantee better code generation.
  Verify performance changes with measurements; use generated code to
  investigate unexpected results.
- Measure end to end at p99 under concurrent load, and report the boundary
  of what was measured. A benchmark of one part in isolation does not
  establish a responsive product.

## Developer experience

### Naming

All tiers. We write idiomatic Rust: `snake_case` items, `CamelCase` types,
clippy's acronym casing (`VsrState`, not `VSRState`), and `rustfmt` layout.
Within that, TigerBeetle's naming rules apply because they encode
understanding:

- Get the noun and the verb exactly right. A name is the smallest place to
  record what you understood about the domain.
- Do not abbreviate, except for the conventional single letters in tight
  arithmetic.
- Put units and qualifiers last, most significant first: `latency_ms_max`, so
  related names sort together and align.
- Choose symmetric names of equal length for symmetric things: `source` and
  `target`, not `src` and `dest`.
- Name a callback or helper after the function that calls it:
  `read_sector` and `read_sector_callback`.
- Do not overload a name with two meanings in two contexts.
- Order matters: `main` first, then the important types. Declare a Rust
  struct's fields in its definition and group its methods in nearby `impl`
  blocks; put associated types in the relevant trait declarations and trait
  implementations. Alphabetical where nothing else decides.
- Where two arguments of the same type could be swapped, take a struct with
  named fields.

Prefer the simplest signature that carries the information: `()` over `bool`,
`bool` over a number, a number over `Option`, `Option` over `Result`. Every
step up is a dimension every caller must handle, and it spreads.

### Comments and commits

All tiers. Comments say *why*, in full sentences, with a capital letter and a
full stop, because they are read as prose. A module starts with a comment
that states what it owns and what its invariants are; those sentences are the
specification the assertions implement. A test starts with its goal and its
method. Commit messages describe the change and its reason in the message,
not only in the pull request, because `git blame` shows the message and not
the PR.

### Layout

All tiers; check Rust layout with `cargo fmt --check`. Four-space indentation, 100
columns, trailing commas so `rustfmt` keeps one item per line, braces on
every `if` that spans lines. These are TigerBeetle's numbers and they are
also what this tree already does, so the tool exists to keep it that way.

### Dependencies are debts

All tiers, strictest in tier A. TigerBeetle has zero dependencies. Playbit
cannot: it runs a WebAssembly engine, a QUIC stack, a durable store, and a
GUI toolkit that would each take years to write and are not our product.
What we keep from the rule is the accounting. **Every third-party crate is a
liability we have chosen to carry, and it is recorded as such.** The record
lives next to the dependency in `Cargo.toml`: why it is there, what it
would take to remove it, and which tier it serves.

- `playbit-api` keeps its ABI layer dependency-free. Its default `docdb` feature
  re-exports the first-party document model, bundled with the installed SDK.
  ABI-only consumers can disable default features.
- `kvdb` depends on a hash function and, optionally, a storage backend. The
  backend is a recorded debt: the design allows replacing it with our own
  storage, and that is the intended direction.
- The client and the engine carry a justified list. Adding to it requires the
  Cargo.toml record, a feature flag when only some builds need it, and a
  reviewer who agrees that writing it ourselves is worse.
- The remote service carries what its cloud provider requires.

Prefer a crate with few dependencies of its own; measure the transitive
count before adding one, and record it. Vendor when we must patch; vendored
code is still a dependency and still recorded.

### Tooling

All tiers, as a guideline. TigerBeetle writes everything, including scripts,
in Zig, to keep one tool and one set of skills. Playbit's core is Rust and
tooling is written in Rust when that is reasonable, because a build or
release step is a program with bugs like any other. It is a guideline and not
a rule: a deployment step that runs on a fresh machine without a Rust
toolchain may be shell, and cross-compiling a tool to avoid a shell script
can add more failure modes than it removes. Choose the fewer moving parts and
say why in the file.

## Zero technical debt

All tiers. Delete superseded unreleased shapes in the same change; do not keep
speculative aliases, fallbacks or migrations for them. Released contracts follow
the [compatibility contract](compatibility.md): preserve published identities and
meanings, retain permanent deprecations, and use concrete supported migrations
when necessary. Compatibility obligations begin with the first supported release,
even before product version 1.0. This document adds the reason TigerBeetle gives
for the rule, because it also explains the
assertions and the limits: a problem solved in design costs hours, in
implementation days, and in production weeks, and the difference is paid
with interest. Simplicity is not the first draft. It is the last revision, and
it is worth the passes it takes.

## Enforcement

These are requirements for review, not a claim that the existing tree already
complies. Enforcement status as of 2026-09-21:

| Rule | Implemented checks | Remaining work |
|---|---|---|
| Warnings, clippy, `unsafe` in tier A, bare `unwrap` in tier A | Workspace/member lint configuration; warnings-denied first-party Clippy with and without desktop; iOS library check; tier A unwrap regression gate | Finish removing existing unwraps in the later scoped work items; unsafe FFI exceptions remain locally documented |
| Layout | First-party Rust is formatted; client, kvdb and fuzz-workspace formatting checks run in CI | Maintain formatting as code changes; vendor retains its upstream policy |
| Function length, assertion density, limit constants | The style report inventories all three; its tier A gate rejects newly overlong functions; density and limits are report-only | Review meaningful invariants and limit coverage rather than counts |
| Every decoder of untrusted bytes has a fuzz target | kvdb and the runtime boundary have fuzz smoke tests; the style report heuristically matches decoder names or annotations to targets | Review actual decoder coverage and add missing targets; a textual match does not prove execution |
| Panic policy | Existing host and service tests cover parts of failure handling | Injected-failure tests covering each policy, including shared-state containment before service continuation |
| Dependencies | The style report counts transitive crates | Complete the per-dependency Cargo.toml ledger and review it on changes |

Assertion density and limit constants are reported, not gated. Counts do not
replace review of meaningful invariants or actual decoder coverage. Update this
status when the checks change. Run `python3 scripts/style-report.py` for the
current source report.
Track remaining work in issues and keep measured run evidence outside Git.

## Error ownership and test scope

An admitted operation has a terminal owner through completion, including when
its requester drops or a reply channel closes. Cancellation does not undo a
durable write, release still-owned credit early or authorize replay of an
uncertain mutation. Return the dependency failure or retain an explicit failed
state; do not fabricate acknowledgment, empty data or successful rollback.

Boundary tests cover representative validation, authority, generation,
cancellation and dependency failures. A tested ownership boundary does not prove
every OS/RNG/storage/transport variant was injected at every propagation site.
Keep untested variants explicit; do not add production fault switches solely to
claim exhaustive call-site coverage. Ordinary `Option` absence is not an error.
A caught panic alone does not establish that shared state can safely continue;
the tier panic policy still applies.
