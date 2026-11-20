#!/bin/bash
# Shared verification commands for local and GitHub execution.
set -euo pipefail

# Clippy (Linux & wasm).
cargo clippy --locked -p playbit --bins --no-default-features --features process,artwork --no-deps
cargo clippy --locked -p playbit -p playbit-trace --lib --no-default-features --target wasm32-unknown-unknown --no-deps

# Build and test the portable client.
cargo build --locked -p playbit --bins --no-default-features --features process,artwork
cargo rustc --locked -p playbit --lib --crate-type cdylib --no-default-features --target wasm32-unknown-unknown
sh scripts/prepare-apps.sh
cargo rustc --locked -p todo --lib --crate-type cdylib --release --target wasm32-unknown-unknown
cargo build --locked -p shared-note -p ui-gallery --release --target wasm32-unknown-unknown
cargo test --locked -p playbit-api -p kvdb --features kvdb/redb

# Build and test independent local engine embedding.
sh playbit/platform/native/build-engine-host.sh

# Check profile ownership across fork.
cargo test --locked -p playbit --no-default-features --features process --test store-profile-lock

# Check bounded event recording and retained transport.
cargo test --locked -p playbit-trace

# Check static musl trace executable and process lifecycle.
target="$(uname -m)-unknown-linux-musl"
rustup target add "$target"
linker="$(rustc --print sysroot)/lib/rustlib/$(rustc --print host-tuple)/bin/rust-lld"
RUSTFLAGS="$RUSTFLAGS -C link-arg=--build-id=sha1" cargo clippy --locked -p playbit-trace --all-targets --no-deps --target "$target" --config "target.$target.linker=\"$linker\""
RUSTFLAGS="$RUSTFLAGS -C link-arg=--build-id=sha1" cargo test --locked -p playbit-trace --target "$target" --config "target.$target.linker=\"$linker\""

# Check kvdb with warnings denied.
cargo clippy --locked -p kvdb --features redb --all-targets

# Build freestanding kvdb for WebAssembly and Cortex-M.
cargo build --locked -p kvdb --target wasm32-unknown-unknown
cargo build --locked -p kvdb --target thumbv7em-none-eabi
