# Building and testing Playbit

Run commands from the repository root. The full contributor kit requires an
Apple silicon Mac with macOS 15+, Rust (CI uses 1.99.0), Xcode and its command-line
tools, Python 3, and Node.js 26+ with npm.

## Build the contributor kit

```sh
xcodebuild -downloadComponent MetalToolchain
rustup target add wasm32-unknown-unknown
./build.sh
```

`build.sh` fixes its output directory to this checkout's `target/` and builds:

| Output | Location |
| --- | --- |
| Mac app | `target/debug/Playbit.app` |
| Packaged CLI, engine and MCP | `target/debug/Playbit.app/Contents/MacOS/` |
| TODO, Micro Kart and UI Gallery bundles | `target/debug/{todo,micro-kart,ui-gallery}.pbapp` |
| Web client | `target/web/` |
| Workspace test binaries | `target/debug/` |

The kit uses debug builds; its bundled native SDK is always built in release
mode. Metal shaders are compiled and embedded at build time, so the Xcode Metal
Toolchain is required. Compiling a Wasm app against the installed SDK does not
need Metal.

The Mac app is ad-hoc signed. No Apple developer account, private package
credentials or remote service credentials are needed to build. Starter
preparation uses temporary source directories and isolated offline profiles,
leaving checked-in manifests and your normal profile untouched. Subsequent
builds reuse Cargo artifacts. GPUI is built as a dependency; its upstream
example apps are excluded from the kit.

Apple app icons are shared Icon Composer sources in `playbit/platform/macos/`.
Packaging selects `icon-debug.icon` for Debug and `icon.icon` for Release.
Debug apps use the name Playbit Debug and a `.debug` bundle identifier so both
configurations can be installed. Local Mac profiles follow normal profile
selection. Set separate `PLAYBIT_PROFILE` paths when running both builds.
Mac guests without custom artwork use the precompiled `icon-guest.icon` assets.
Repackage after editing an icon; cached guest bundles are keyed by that artwork.

For an optimized Mac package:

```sh
sh playbit/platform/macos/package.sh release
open target/release/Playbit.app
```

Use `pb` from the complete app bundle for [app development](developing-apps.md).
The package contains the matching project template, Rust API and native SDK.

## Tests and lint

Install the Wasm target once, then run the suite directly. Cargo builds and embeds
the release guest fixtures through a shared test dependency. Always use
`--release` for Cargo tests, including targeted tests and `--no-run` builds;
large publication-history tests can take many minutes without optimization.
Enable debug assertions and overflow checks to retain those correctness checks:

```sh
rustup target add wasm32-unknown-unknown
sh scripts/prepare-apps.sh
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true
export CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true
cargo test --release --locked --workspace --exclude gpui
RUSTFLAGS="-D warnings" cargo clippy --locked --workspace --exclude gpui --all-targets --no-deps
```

Fixture builds use a separate Cargo output directory, including when the suite
uses a custom `CARGO_TARGET_DIR`. Ordinary builds do not compile test fixtures.

Some tests use loopback sockets. After building the contributor kit, exercise
the installed developer workflow on a Mac with a GUI session:

```sh
python3 scripts/test-packaged-workflow.py --native
```

This uses a temporary profile, generates source with `pb init`, checks builds
and bundles, runs TODO in its matching GUI owner, submits edits, and checks
retained data after reopening. It also checks the generated native build path
and failed-build retention. Engine operations run with outbound IP networking
denied; native SDK consumers build without checkout access. It is a correctness
check, not physical input, visual or latency qualification.

See [AGENTS.md](../AGENTS.md) for contribution requirements,
[the runtime map](../playbit/src/runtime/README.md) for focused tests, and
[local performance diagnostics](../scripts/PERFORMANCE.md) for explicit
reference-device measurements. CI does not run benchmarks.

For a running isolated engine, the launch-check diagnostic is:

```sh
cargo run --release -p playbit --example benchmark-startup-check -- PROFILE ENGINE_EXECUTABLE
```

It measures 1,000 connect/protocol/executable-identity checks after 100 warmups,
including OS-account validation. It does not measure GUI startup; run it only
as an explicit local benchmark.

## Web

Tests verifying Ed25519 certificate requests require OpenSSL 3 on `PATH`;
macOS's system LibreSSL is insufficient. With Homebrew, set this before testing:

```sh
export PATH="$(brew --prefix openssl@3)/bin:$PATH"
```

```sh
sh playbit/platform/web/build.sh debug
sh playbit/platform/web/check.sh
node --test playbit/platform/web/src/*.test.mjs
python3 -m http.server 8876 --bind 127.0.0.1 --directory target/web
```

The build installs pinned esbuild and TypeScript through npm when needed,
compiles the Rust web runtime and example Wasm, and bundles TypeScript for the
page and workers. Tests load TypeScript using Node's type stripping.

Open `http://127.0.0.1:8876/tests/reconnect-browser.html` in Chrome for the
reconnect regression. It exercises the actual worker and replication driver
with synthetic AP/storage boundaries. It covers transport loss, repeated setup
failure, pending publication, revocation and close.

After building, `node scripts/check-browser-account-outbox.mjs` checks account
publication with real Chromium IndexedDB, OPFS, WebCrypto and Wasm. Set `CHROME`
to select a browser executable. These component checks do not establish real
AP convergence, sleep/wake or production account replication. See
[browser storage](../playbit/platform/web/STORAGE.md) for further checks.

## Other targets and build metadata

- [iOS](../playbit/platform/ios/README.md) has its own packaging and device checks.
- [Native embedding](../playbit/platform/native/README.md) covers engine-only hosts and the native SDK.
- Linux CLI/engine builds use `cargo build --locked -p playbit --bins --no-default-features --features process,artwork`; this does not build the Mac UI.

The root workspace does not depend on `service/`. The remote AP service is a
separate private workspace with its own build and test instructions.

Release mirrors contain only the allowlisted client source, with one commit per
merged change that affects published paths and a tag per release. If working
from a mirror, submit patches as diffs attached to issues: the next release
replaces its tree. Mirror access remains private during beta.

The macOS minimum version is defined in [`.cargo/config.toml`](../.cargo/config.toml)
and read by native scripts through [`scripts/macos-env.sh`](../scripts/macos-env.sh).
Shaders, bundles and exported SDK metadata use that same target.

The client and SDK release version comes from the root `Cargo.toml`; examples
have independent package versions. CLI, MCP, About and diagnostic metadata add
the first eight Git revision characters, or `+src` in an archive without `.git`.
Uncommitted changes do not add a marker. Apple marketing versions use the numeric
release core; `PlaybitVersion` carries the complete version.

Before direct workspace Cargo tests or Clippy, run `sh scripts/prepare-apps.sh`.
This builds `pb` and uses `pb meta-gen` to prepare the repository apps' ignored
metadata. `build.sh` and CI perform this step automatically. Repeat it after
editing app manifests or deleting their `target/` directories.
