#!/bin/sh
set -eu
cd "$(dirname "$0")/../../.."
. scripts/macos-env.sh
cargo build --locked -p playbit -p playbit-api --lib --release --no-default-features \
    --features native-runtime,local-store --target-dir target/native-runtime
cc -std=c17 -Wall -Wextra -Werror -O2 -g -I playbit-api/include \
    -c playbit/platform/native/probe/app.c -o target/native-runtime/app.o
rustc --edition=2024 -D warnings --test -C opt-level=3 -C lto=thin \
    --extern playbit=target/native-runtime/release/libplaybit.rlib \
    --extern playbit_api=target/native-runtime/release/libplaybit_api.rlib \
    -L dependency=target/native-runtime/release/deps \
    -C link-arg=target/native-runtime/app.o \
    playbit/platform/native/probe/host.rs -o target/native-runtime/runtime-probe
target/native-runtime/runtime-probe --nocapture
