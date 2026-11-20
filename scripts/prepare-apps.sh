#!/bin/sh
# Generate repository app metadata with the same CLI used by installed projects.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
unset CARGO_BUILD_TARGET
cargo build --target-dir "$repo/target" --locked -p playbit --bin pb --no-default-features --features process
for app in todo shared-note; do
    (cd "guests/$app" && "$repo/target/debug/pb" meta-gen)
done
