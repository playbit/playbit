#!/bin/sh
# Link both C host and C app directly to the native engine/runtime static library.
set -eu
cd "$(dirname "$0")/../../.."
. scripts/macos-env.sh
cargo rustc --locked -p playbit --lib --release --no-default-features \
    --features local-store,native-runtime --target-dir target/native-c-apps --crate-type staticlib
cargo build --locked -p playbit -p playbit-api --lib --release --no-default-features \
    --features local-store,native-runtime --target-dir target/native-c-apps
rustc --edition=2024 -D warnings -C opt-level=3 -C lto=thin \
    --extern playbit=target/native-c-apps/release/libplaybit.rlib \
    --extern playbit_api=target/native-c-apps/release/libplaybit_api.rlib \
    -L dependency=target/native-c-apps/release/deps \
    playbit/platform/native/apps-probe/fixtures.rs -o target/native-c-apps/fixtures
target/native-c-apps/fixtures > target/native-c-apps/scenes.h
cc -std=c17 -Wall -Wextra -Werror -O2 -g \
    -I playbit/include -I playbit-api/include -I target/native-c-apps \
    playbit/platform/native/apps-probe/main.c target/native-c-apps/release/libplaybit.a \
    -liconv -Wl,-map,target/native-c-apps/link.map -o target/native-c-apps/native-host
target/native-c-apps/native-host --layout > target/native-c-apps/c-layout.txt
target/native-c-apps/fixtures --layout > target/native-c-apps/rust-layout.txt
cmp target/native-c-apps/c-layout.txt target/native-c-apps/rust-layout.txt
fixture=$(mktemp -d /private/tmp/playbit-c-native.XXXXXX)
trap 'rm -rf "$fixture"' EXIT
target/native-c-apps/fixtures "$fixture/first"
target/native-c-apps/fixtures "$fixture/second"
target/native-c-apps/native-host "$fixture/first" "$fixture/second"
printf 'Retired C native test profiles: %s\n' "$fixture"
