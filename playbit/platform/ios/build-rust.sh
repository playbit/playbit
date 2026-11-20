#!/bin/sh
# Build each concrete iOS composition separately. The extension has no VM/UI;
# copying immediately preserves its artifact before Cargo builds the app library.
set -eu
rust_target=${1:?Expected an iOS Rust target}
app_features=${2:-ios}
case "$rust_target" in
    aarch64-apple-ios) platform=iphoneos ;;
    aarch64-apple-ios-sim) platform=iphonesimulator ;;
    *) echo 'Expected aarch64-apple-ios or aarch64-apple-ios-sim' >&2; exit 2 ;;
esac
case "$app_features" in
    ios|ios,phase1-qualification) ;;
    *) echo 'Expected ios or ios,phase1-qualification' >&2; exit 2 ;;
esac
export IPHONEOS_DEPLOYMENT_TARGET=18.0
export CFLAGS_aarch64_apple_ios_sim=-mios-simulator-version-min=18.0
export CFLAGS_aarch64_apple_ios=-miphoneos-version-min=18.0
generated=playbit/platform/ios/Generated
mkdir -p "$generated/$platform"
cargo rustc --locked --release -p playbit --lib --crate-type staticlib --no-default-features --features file-provider --target "$rust_target"
cp "target/$rust_target/release/libplaybit.a" "$generated/$platform/libplaybit-provider.a"
cargo rustc --locked --release -p playbit --lib --crate-type staticlib --no-default-features --features "$app_features" --target "$rust_target"
cp "target/$rust_target/release/libplaybit.a" "$generated/$platform/libplaybit.a"
cp "target/$rust_target/release/playbit-version.xcconfig" "$generated/Version.xcconfig"
