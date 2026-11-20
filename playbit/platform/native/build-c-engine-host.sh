#!/bin/sh
# Pure C consumer of the exact local-store static library; no Rust host wrapper.
set -eu
cd "$(dirname "$0")/../../.."
. scripts/macos-env.sh
cargo rustc --locked -p playbit --lib --release --no-default-features \
    --features local-store --target-dir target/native-c-engine --crate-type staticlib
cc -std=c11 -Wall -Wextra -Werror -O2 -g \
    -I playbit/include -I playbit-api/include \
    playbit/platform/native/engine-probe/main.c target/native-c-engine/release/libplaybit.a \
    -liconv -Wl,-map,target/native-c-engine/link.map \
    -o target/native-c-engine/engine-host
cargo build --locked -p playbit --lib --release --no-default-features \
    --features local-store --target-dir target/native-c-engine
rustc --edition=2024 -D warnings -C opt-level=3 -C lto=thin \
    --extern playbit=target/native-c-engine/release/libplaybit.rlib \
    -L dependency=target/native-c-engine/release/deps \
    playbit/platform/native/engine-probe/layout.rs -o target/native-c-engine/layout
# Compare C and Rust compiler measurements, rather than hard-coded sizes.
target/native-c-engine/engine-host --layout > target/native-c-engine/c-layout.txt
target/native-c-engine/layout > target/native-c-engine/rust-layout.txt
cmp target/native-c-engine/c-layout.txt target/native-c-engine/rust-layout.txt
fixture=$(mktemp -d /private/tmp/playbit-c-engine.XXXXXX)
target/native-c-engine/engine-host "$fixture/first" "$fixture/second"
printf 'C engine profiles: %s\n' "$fixture"
