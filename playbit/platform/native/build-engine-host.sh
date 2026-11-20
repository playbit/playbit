#!/bin/sh
# An independent consumer prevents Cargo's test/dev features from selecting the
# VM, network or process host behind this probe's back. No second workspace or
# duplicate lockfile is needed: compile the library with its exact composition,
# then link a plain native caller against that artifact.
set -eu
cd "$(dirname "$0")/../../.."
. scripts/macos-env.sh
cargo build --locked -p playbit --lib --release --no-default-features \
    --features local-store --target-dir target/native-services
rustc --edition=2024 -D warnings -C opt-level=3 -C lto=thin \
    --extern playbit=target/native-services/release/libplaybit.rlib \
    -L dependency=target/native-services/release/deps \
    playbit/tests/embedding_host.rs -o target/native-services/engine-host
rustc --edition=2024 -D warnings --test -C opt-level=3 -C lto=thin \
    --extern playbit=target/native-services/release/libplaybit.rlib \
    -L dependency=target/native-services/release/deps \
    playbit/tests/embedding_host.rs -o target/native-services/engine-host-tests
target/native-services/engine-host-tests
printf '%s\n' "$PWD/target/native-services/engine-host"
