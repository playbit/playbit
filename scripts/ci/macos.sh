#!/bin/bash
# Shared verification commands for local and GitHub execution.
set -euo pipefail

# Exercise the shared tooling's macOS cleanup paths on this platform too.
bash scripts/ci/check.sh tooling

# Test the shared macOS build target.
python3 scripts/test_macos_target.py
python3 scripts/test_offline_fixture.py

# Check Universal Link signing grants and the actual AppKit activity callback.
python3 playbit/platform/macos/test_provider_signing.py
python3 playbit/platform/macos/test_thumbnail.py
cargo test --locked -p gpui --lib platform::mac::platform::tests::universal_links_deliver_fragments_before_and_after_launch

# Check the iOS library with warnings denied.
(
    export RUSTFLAGS='-D warnings'
    cargo check --locked -p playbit --lib --no-default-features --features ios --target aarch64-apple-ios
    cargo check --locked -p playbit --lib --no-default-features --features file-provider --target aarch64-apple-ios
)

# Check the shared replicated extension against the minimum iOS deployment target.
ios_sdk=$(xcrun --sdk iphoneos --show-sdk-path)
xcrun --sdk iphoneos swiftc -typecheck -sdk "$ios_sdk" -target arm64-apple-ios18.0 \
    -module-cache-path "$RUNNER_TEMP/provider-swift-cache" -application-extension \
    -import-objc-header playbit/platform/apple/FileProvider/ProviderBridge.h \
    playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/FileProvider/*.swift \
    playbit/platform/apple/FileProvider/*.swift

# Build and check independent C and Rust Wasm ABI fixtures.
PATH="$(brew --prefix llvm)/bin:$(brew --prefix lld)/bin:$PATH" \
    sh playbit/platform/native/build-wasm-probe.sh

# Keep the full storage-history workloads, running their repeated scans below
# with optimization and assertions enabled, as on main.
cargo test --locked --workspace --exclude gpui -- \
    --skip store::volume_index::namespace::source::cut::tests::five_hundred_sequential_births_fit_acceptance_and_durable_receipt \
    --skip store::volume_index::namespace::source::cut::tests::thousand_sequential_edits_fit_acceptance_and_durable_receipt \
    --skip store::volume_index::tests::one_thousand_publications_fit_persisted_causal_source_budgets
if [ "${CI_STORAGE_HISTORY:-true}" = true ]; then
    bash scripts/ci/storage-history.sh
else
    echo 'Storage history: no affected inputs'
fi

# Check SDK probe staging.
python3 -c 'import tomllib'
python3 playbit/platform/native/mac-probe/test_build.py
python3 playbit/platform/macos/test_developer_resources.py

# Build generated app with its installed SDK and declarations.
cargo test --locked -p playbit --bin pb developer::tests::starter_metadata_is_optional_and_removed_names_do_not_survive -- --exact --ignored

# Check bounded event recording.
cargo test --locked -p playbit-trace --lib

# Check renderer layout and native text regressions.
python3 scripts/test-renderer.py

# Check evidence tooling correctness.
cargo test --locked -p playbit --bin check-presentation
cargo test --locked -p playbit --example local-ipc-driver
cargo test --locked -p playbit --example benchmark-account-record

# Check UIKit snapshots and owner admission state.
swift_target="$(uname -m)-apple-macosx$MACOSX_DEPLOYMENT_TARGET"
swiftc -target "$swift_target" -O playbit/platform/apple/FileProvider/Operation.swift playbit/platform/macos/Tests/ProviderOperationTests.swift -o "$RUNNER_TEMP"/provider-operation-tests
"$RUNNER_TEMP"/provider-operation-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/macos/FileProvider/Drives.swift playbit/platform/macos/Tests/ProviderPushTests.swift -o "$RUNNER_TEMP"/provider-push-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift \
    playbit/platform/apple/FileProvider/Item.swift playbit/platform/apple/FileProvider/Enumerator.swift \
    playbit/platform/macos/Tests/ProviderNamespaceTests.swift -o "$RUNNER_TEMP"/provider-namespace-tests
"$RUNNER_TEMP"/provider-namespace-tests
"$RUNNER_TEMP"/provider-push-tests
clang -target "$swift_target" -std=c17 -Wall -Wextra -Werror -O2 -c playbit/platform/ios/Tests/ProviderRuntimeFixture.c -o "$RUNNER_TEMP"/provider-runtime-fixture.o
swiftc -target "$swift_target" -O -import-objc-header playbit/platform/ios/Tests/ProviderRuntimeFixture.h \
    playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/FileProvider/Account.swift \
    playbit/platform/ios/FileProvider/Runtime.swift playbit/platform/ios/Tests/ProviderRuntimeTests.swift \
    "$RUNNER_TEMP"/provider-runtime-fixture.o -o "$RUNNER_TEMP"/provider-runtime-tests
"$RUNNER_TEMP"/provider-runtime-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/GuestScene.swift playbit/platform/ios/Tests/SceneBridgeFixture.swift playbit/platform/ios/Tests/GuestSceneTests.swift -o "$RUNNER_TEMP"/guest-scene-tests
"$RUNNER_TEMP"/guest-scene-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/CuratedExample.swift playbit/platform/ios/Tests/CuratedExampleTests.swift -o "$RUNNER_TEMP"/curated-example-tests
"$RUNNER_TEMP"/curated-example-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/PersonalVolumeAccess.swift playbit/platform/ios/Tests/PersonalVolumeAccessTests.swift -o "$RUNNER_TEMP"/personal-volume-tests
"$RUNNER_TEMP"/personal-volume-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/LogSnapshot.swift playbit/platform/ios/Tests/LogSnapshotTests.swift -o "$RUNNER_TEMP"/log-snapshot-tests
"$RUNNER_TEMP"/log-snapshot-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/SessionArtwork.swift playbit/platform/ios/Tests/SessionArtworkTests.swift -o "$RUNNER_TEMP"/session-artwork-tests
"$RUNNER_TEMP"/session-artwork-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/SessionListSnapshot.swift playbit/platform/ios/Tests/SessionListSnapshotTests.swift -o "$RUNNER_TEMP"/session-list-tests
"$RUNNER_TEMP"/session-list-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/InvitationInput.swift playbit/platform/ios/Tests/InvitationInputTests.swift -o "$RUNNER_TEMP"/invitation-input-tests
"$RUNNER_TEMP"/invitation-input-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/AppPage.swift playbit/platform/ios/Tests/AppPageTests.swift -o "$RUNNER_TEMP"/app-page-tests
"$RUNNER_TEMP"/app-page-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/RecoverySetup.swift playbit/platform/ios/Tests/RecoverySetupTests.swift -o "$RUNNER_TEMP"/recovery-setup-tests
"$RUNNER_TEMP"/recovery-setup-tests
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Host/GuestInput.swift playbit/platform/ios/Host/EngineHost.swift playbit/platform/ios/Host/LogSnapshot.swift playbit/platform/ios/Host/GuestScene.swift playbit/platform/ios/Host/PersonalVolumeAccess.swift playbit/platform/ios/Host/AppPage.swift playbit/platform/ios/Tests/SceneBridgeFixture.swift playbit/platform/ios/Tests/EngineHostTests.swift -o "$RUNNER_TEMP"/engine-host-tests
"$RUNNER_TEMP"/engine-host-tests

# Check native resource counter units.
clang -std=c17 -Wall -Wextra -Werror -O2 scripts/process-resources.c -o "$RUNNER_TEMP"/process-resources
"$RUNNER_TEMP"/process-resources 0

# The contributor kit must build with a signed-out profile and no Testing credentials.
PLAYBIT_CHANNEL=testing PLAYBIT_PROFILE="$RUNNER_TEMP/unused-build-profile" \
    PLAYBIT_TESTING_CREDENTIALS="$RUNNER_TEMP/missing-testing-credentials" ./build.sh
test ! -e "$RUNNER_TEMP/unused-build-profile"
swiftc -target "$swift_target" -O -warnings-as-errors -parse-as-library \
    -module-cache-path "$RUNNER_TEMP/thumbnail-swift-cache" \
    -import-objc-header playbit/platform/macos/QuickLook/ThumbnailBridge.h \
    playbit/platform/macos/QuickLook/ThumbnailProvider.swift playbit/platform/macos/Tests/ThumbnailTests.swift \
    target/quicklook/release/libplaybit.a -liconv -o "$RUNNER_TEMP/thumbnail-tests"
"$RUNNER_TEMP/thumbnail-tests"
cargo test --locked -p playbit --features quicklook --lib platform::quicklook

# Check compiled starter selection.
python3 playbit/platform/ios/test_starter_bundles.py

# Collect available public products.
python3 scripts/collect-ci-products.py "$GITHUB_WORKSPACE" "$RUNNER_TEMP/client-products"

# Every native text shell shares identifier vectors with Rust and the browser.
swiftc -target "$swift_target" -O playbit/platform/ios/Host/Identifier.swift playbit/platform/ios/Tests/IdentifierTests.swift -o "$RUNNER_TEMP"/identifier-tests
"$RUNNER_TEMP"/identifier-tests
