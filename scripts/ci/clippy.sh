#!/bin/bash
# Shared verification commands for local and GitHub execution.
set -euo pipefail

# Generate repository app metadata.
sh scripts/prepare-apps.sh

# Clippy with desktop.
cargo clippy --locked --workspace --exclude gpui --all-targets --no-deps

# Clippy without desktop.
cargo clippy --locked --workspace --exclude gpui --all-targets --no-deps --no-default-features

# Build and test independent local engine embedding.
sh playbit/platform/native/build-engine-host.sh

# Check connected engine and native apps without Wasm.
cargo clippy --locked -p playbit --lib --no-default-features --features connected-engine --no-deps -- -D warnings
cargo clippy --locked -p playbit --lib --no-default-features --features connected-engine,native-runtime --no-deps -- -D warnings
