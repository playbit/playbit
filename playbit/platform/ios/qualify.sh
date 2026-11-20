#!/bin/sh
# Explicit Phase 1 fixtures; normal app builds contain no starter or seed entry point.
set -eu
cd "$(dirname "$0")/../../.."
mode=${1:?Usage: qualify.sh simulator|device corrupt-store|todo|shared-note|todo-profile|todo-account|todo-session-editing|todo-sessions|todo-new-session|todo-sign-in|todo-controls|todo-surface|todo-consent|todo-consent-details|todo-consent-pending|todo-logs|todo-logs-failure|todo-text|todo-invitation|todo-invitation-entry|todo-invitation-cancel|ui-gallery|micro-kart|micro-kart-invalid-scene}
app=${2:?Usage: qualify.sh simulator|device corrupt-store|todo|shared-note|todo-profile|todo-account|todo-session-editing|todo-sessions|todo-new-session|todo-sign-in|todo-controls|todo-surface|todo-consent|todo-consent-details|todo-consent-pending|todo-logs|todo-logs-failure|todo-text|todo-invitation|todo-invitation-entry|todo-invitation-cancel|ui-gallery|micro-kart|micro-kart-invalid-scene}
case "$app" in
    todo) test_case=testTodoSurvivesProcessRestart ;;
    shared-note) test_case=testSharedNoteFileSaveSurvivesRestart ;;
    corrupt-store) test_case=testCorruptStoreCanResetAndOpenAgain; app=todo ;;
    todo-profile) test_case=testDisplayNameSavesOfflineAndBackDiscardsDraft; app=todo ;;
    todo-account) test_case=testAccountSignOutPreservesLocalSessionsAcrossRestart; app=todo ;;
    todo-session-editing) test_case=testSessionEditingReordersAndLeavesOnlyOneLocalEntry; app=todo ;;
    todo-sessions) test_case=testNamedSessionsKeepRootDistinctionsAndIdentityAfterRestart; app=todo ;;
    todo-new-session) test_case=testNewSessionOffersSignInAndJoinWithoutChangingLocalSessions; app=todo ;;
    todo-sign-in) test_case=testSignInPresentationKeepsLocalNavigationAvailable; app=todo ;;
    todo-controls) test_case=testTodoDraftAndAddControls; app=todo ;;
    todo-surface) test_case=testTodoSurfaceChangesPreserveFocusedDraftAndData; app=todo ;;
    todo-consent) test_case=testPersonalConsentPreservesGuestDraftAndSavedChoices; app=todo ;;
    todo-consent-details) test_case=testSessionDetailsShowsSavedPersonalAccessAndRevocation; app=todo ;;
    todo-consent-pending) test_case=testPendingPersonalConsentDisablesEditsAndCannotDismissAnotherChooser; app=todo ;;
    todo-logs) test_case=testTodoLogsShowActualRepeatedOutputAndKeepDraft; app=todo ;;
    todo-logs-failure) test_case=testLogFailureKeepsSnapshotAndCanReopen; app=todo ;;
    todo-text) test_case=testTodoNativeSelectionAndClipboard; app=todo ;;
    todo-invitation) test_case=testInvitationPauseSurvivesRestartAndResumeOnlyObserves; app=todo ;;
    todo-invitation-entry) test_case=testInvitationSheetKeepsFailedDraftAndCancels; app=todo ;;
    todo-invitation-cancel) test_case=testInvitationSheetCancelSuppressesLatePreview; app=todo ;;
    ui-gallery) test_case=testBasicControlsGallery ;;
    micro-kart-invalid-scene) test_case=testMalformedCanvasRetainsPriorSceneAndCanReopen; app=micro-kart ;;
    micro-kart) test_case=testMicroKartCanvasRespondsToTouchAndFrames ;;
    *) echo 'Expected corrupt-store, todo, shared-note, todo-profile, todo-account, todo-session-editing, todo-sessions, todo-new-session, todo-sign-in, todo-controls, todo-surface, todo-consent, todo-consent-details, todo-consent-pending, todo-logs, todo-logs-failure, todo-text, todo-invitation, todo-invitation-entry, todo-invitation-cancel, ui-gallery, micro-kart or micro-kart-invalid-scene' >&2; exit 2 ;;
esac
case "$mode" in
    simulator)
        : "${SIMULATOR_ID:?Set SIMULATOR_ID to an installed iPhone simulator UDID}"
        rust_target=aarch64-apple-ios-sim
        platform=iphonesimulator
        destination="platform=iOS Simulator,id=$SIMULATOR_ID"
        set -- CODE_SIGN_IDENTITY=-
        ;;
    device)
        : "${IOS_DEVICE_ID:?Set IOS_DEVICE_ID to the connected iPhone UDID}"
        : "${ARCHIPELAGO_APP_ID:?Set ARCHIPELAGO_APP_ID to a reviewed, registered company qualification identifier}"
        rust_target=aarch64-apple-ios
        platform=iphoneos
        destination="platform=iOS,id=$IOS_DEVICE_ID"
        set -- -allowProvisioningUpdates DEVELOPMENT_TEAM=KBJTKLMMGM
        ;;
    *) echo 'Expected simulator or device' >&2; exit 2 ;;
esac
run_id="r$(date -u +%Y%m%dT%H%M%SZ)"
app_name=$(printf '%s' "$app" | tr -d '-')
app_id=${ARCHIPELAGO_APP_ID:-playbit.phase1.$app_name.$run_id}
evidence="${TMPDIR:-/tmp}/playbit-ios-$mode-$app-$run_id"
mkdir -p "$evidence" "playbit/platform/ios/Generated/$platform"
sh playbit/platform/macos/package.sh release
python3 playbit/platform/ios/prepare-fixture.py "$app" playbit/platform/ios/Generated/starter.pbapp \
    --kit target/release/Playbit.app --profile release > "$evidence/bundle.json"
sh playbit/platform/ios/build-rust.sh "$rust_target" ios,phase1-qualification
shasum -a 256 "playbit/platform/ios/Generated/$platform/libplaybit.a" playbit/platform/ios/Generated/starter.pbapp > "$evidence/artifacts.sha256"
xcodebuild -project playbit/platform/ios/Archipelago.xcodeproj -scheme Archipelago -configuration Release \
    -destination "$destination" -derivedDataPath "$evidence/DerivedData" -resultBundlePath "$evidence/result.xcresult" \
    -only-testing:"ArchipelagoUITests/Phase1Tests/$test_case" -parallel-testing-enabled NO \
    ARCHS=arm64 ONLY_ACTIVE_ARCH=YES CODE_SIGNING_ALLOWED=YES "ARCHIPELAGO_APP_ID=$app_id" \
    PLAYBIT_QUALIFICATION=PHASE1_QUALIFICATION PLAYBIT_KEYCHAIN_ACCESS_GROUP= EXCLUDED_SOURCE_FILE_NAMES= "$@" test > "$evidence/test.log" 2>&1 || {
        tail -70 "$evidence/test.log"; exit 1;
    }
xcrun xcresulttool export attachments --path "$evidence/result.xcresult" --output-path "$evidence/screenshots"
xcrun xcresulttool export diagnostics --path "$evidence/result.xcresult" --output-path "$evidence/diagnostics"
printf 'Verified %s on %s. Evidence: %s\n' "$app" "$mode" "$evidence"
