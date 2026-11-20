Playbit is a personal software environment where data lives independently of any one application. Apps are living documents: things you can open, copy, modify, share, and keep. They continue working across your devices and can perform useful work after you close their interface.

This repository is the published part of Playbit's source: everything that runs on a user's device. It is a read-only mirror regenerated at every release; see [README.md](README.md) and [LICENSE](LICENSE).

## Layout

- `playbit/`: the client, one crate. `src/store` (local data), `src/runtime` (the Wasm runtime), `src/engine` (the local `playbit-engine`), `src/network/ap` (the client side of the AP protocol), `src/package`, `src/platform/{ios,web}`, `src/desktop` (the macOS app, behind the default `desktop` feature), and the binaries under `src/bin`. Platform projects are under `playbit/platform/`.
- `playbit-api/`: what apps compile against, installed in Playbit.app and referenced by projects created with `pb init`.
- `playbit/kvdb/`: the local-first key-value store, a separate crate with a `no_std` core and optional durable backend.
- `playbit/fuzz/`: the shared runtime, ABI and kvdb fuzz workspace.
- `guests/`: example apps compiled to Wasm.
- `vendor/`: third-party code under its own licenses.

## Engineering principles

- **KISS.** Straightforward control flow over meta-programming; explicit branches and typed structs over hidden dynamic behavior; obvious, localized error paths.
- **YAGNI.** No features, configuration, or abstractions without a concrete caller. Unsupported paths are explicit errors, not silent no-ops.
- **DRY, by the rule of three.** Duplicate small local logic when that is clearer; extract shared helpers only after a pattern has repeated and settled.
- **Fail fast.** Explicit errors for unsupported or unsafe states. Never silently broaden permissions or capabilities.
- **Delete superseded unreleased shapes; preserve released contracts.** Remove abandoned unreleased routes, clients, tests, docs and config keys in the same change, without speculative aliases or migrations. The first supported release freezes its declared contracts, even before product version 1.0. Published identities, fields, defaults and meanings remain stable; deprecation is permanent. Preserve existing data and use concrete, supported migrations when necessary. Follow [the compatibility contract](docs/compatibility.md).
- **Product quality and performance are constraints, not later work.** Every change must keep a usable experience and stay within the interaction and startup budgets; bound queues, memory, and concurrency; keep interactive work isolated from slow background and network work; never trade durability or authorization for a timing result.

- **Tiger Style.** [docs/tiger-style.md](docs/tiger-style.md) is normative for tiers A and B. The tier assignment table is in that document. The panic policy and error/assert/unwrap vocabulary are decided there, not per change.
  Guest and example programs, including `guests/` and `playbit/sdk/*/template/`, are exempt from Tiger Style. Write them in plain, idiomatic code; do not add assertions, limits, helper layers, or verbose error handling solely to satisfy Tiger Style.

## Working in the code

1. Read the existing implementation before editing.
2. One concern per change; no mixed feature, refactor, and infrastructure patches.
3. Keep the patch minimal and the program working at every step.
4. Write tests for what you change and run the affected correctness tests before committing. Performance qualification is on demand, not a routine development or commit requirement; use targeted benchmarks when requested or useful for performance-focused work. Performance budgets still apply.
5. Serialize git index writes: never run `git add`, `git commit`, `git rm`, or `git mv` in parallel.

## Building and testing

Always pass `--release` to every agent-run `cargo test` command, including
targeted tests, workspace tests, and `--no-run` builds. Apply this rule even when
another command example omits the flag. Large publication-history tests can take
many minutes without optimization. Keep debug assertions and overflow checks
enabled for correctness tests:

```sh
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true
export CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true
```

- Run `sh scripts/prepare-apps.sh` before direct workspace Cargo tests or Clippy.
- `cargo test --release --locked --workspace --exclude gpui` runs the suite; its fixture crate builds the required Wasm apps.
- `./build.sh` builds the macOS testing kit: the app, the tools, the example apps, and the web client.
- Keep the codebase small, direct, and easy to audit.


## App trace declarations

Keep an app's `playbit.toml` `[trace]` names current with
its instrumentation. Generated `metadata.rs` belongs to the project's `target/playbit/`
folder; never edit or check it in. Name order defines the IDs bundled with that
executable. Run `pb meta-gen` before direct Cargo app builds; `pb build` and `pb run` do it automatically. Build preparation and package admission share validation in
`playbit/src/package/trace_catalog.rs`. App labels, descriptions and numeric trace
observations are private app data, never anonymous telemetry or AP timing labels.
See [app trace declarations](docs/app-trace-declarations.md).

## Identifier text

Use the [shared identifier codecs](playbit/src/identifier.rs). All user-visible opaque
Playbit IDs use canonical base58, including CLI/GUI, JSON APIs, logs, support/admin
tools, documentation and examples. Use the shared codecs and exact decoded byte
widths; do not hand-format IDs as hex, UUIDs or byte arrays.
