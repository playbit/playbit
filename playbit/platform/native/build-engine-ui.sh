#!/bin/sh
# AppKit owns the UI; only local-store is selected in the Playbit library.
set -eu
cd "$(dirname "$0")/../../.."
. scripts/macos-env.sh
cargo +stable build --locked -p playbit --lib --release --no-default-features \
    --features local-store --target-dir target/native-services
rustc +stable --edition=2024 -D warnings -C opt-level=3 -C lto=thin -C panic=abort \
    --crate-type staticlib --crate-name engine_ui \
    --extern playbit=target/native-services/release/libplaybit.rlib \
    -L dependency=target/native-services/release/deps \
    playbit/platform/native/engine-ui/host.rs -o target/native-services/libengine_ui.a
rustc +stable --edition=2024 -D warnings --test -C opt-level=3 -C lto=thin \
    --extern playbit=target/native-services/release/libplaybit.rlib \
    -L dependency=target/native-services/release/deps \
    playbit/tests/embedding_host.rs -o target/native-services/engine-ui-tests
target/native-services/engine-ui-tests
app=target/native-services/LocalLists.app
mkdir -p "$app/Contents/MacOS"
sed "s/@MACOSX_DEPLOYMENT_TARGET@/$MACOSX_DEPLOYMENT_TARGET/g" \
    playbit/platform/native/engine-ui/Info.plist > "$app/Contents/Info.plist"
clang -fobjc-arc -Wall -Wextra -Werror -O2 \
    playbit/platform/native/engine-ui/main.m target/native-services/libengine_ui.a \
    -framework Cocoa -liconv -Wl,-dead_strip -Wl,-fatal_warnings \
    -Wl,-map,target/native-services/engine-ui.map -o "$app/Contents/MacOS/LocalLists"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
printf '%s\n' "$PWD/$app/Contents/MacOS/LocalLists"
