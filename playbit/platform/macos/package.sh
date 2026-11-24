#!/bin/sh
set -eu
cd "$(dirname "$0")/../../.."
pb_build_profile=${1:-debug}
pb_developer_render=${2:-}
if [ "$#" -gt 2 ] || { [ -n "$pb_developer_render" ] && [ "$pb_developer_render" != --dev-always-render ]; }; then
    echo 'Usage: package.sh [debug|release] [--dev-always-render]' >&2
    exit 1
fi
case "$pb_build_profile" in
    debug) pb_icon=icon-debug; pb_app_id=playbit.engine.debug; pb_app_name='Playbit Debug'; set -- ;;
    release) pb_icon=icon; pb_app_id=playbit.engine; pb_app_name=Playbit; set -- --release ;;
    *) echo 'Usage: package.sh [debug|release] [--dev-always-render]' >&2; exit 1 ;;
esac
if [ -n "$pb_developer_render" ]; then
    set -- "$@" --features dev_always_render
fi
test "$(uname -m)" = arm64 || { echo 'Playbit requires an arm64 Mac' >&2; exit 1; }
. scripts/macos-env.sh
pb_sign_identity=${PB_SIGN_IDENTITY:--}
if [ -n "${PB_PROVISIONING_PROFILE:-}" ]; then
    test "$pb_build_profile" = release || { echo 'Provisioned builds require release' >&2; exit 1; }
    test "$pb_sign_identity" != - || { echo 'Provisioned builds require PB_SIGN_IDENTITY' >&2; exit 1; }
fi
pb_keychain_group=$(python3 playbit/platform/macos/keychain-signing.py group)
# One Cargo graph gives the GUI and its clients the same source/IPC identity
# and avoids rebuilding Playbit once for each composition.
PLAYBIT_KEYCHAIN_ACCESS_GROUP="$pb_keychain_group" cargo build --locked "$@" \
    -p playbit -p playbit-macos-runtime --lib --bins
pb_release=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
pb_release_sequence=${PLAYBIT_RELEASE_SEQUENCE:-0}
pb_bundle_version=${pb_release%%+*}
pb_bundle_version=${pb_bundle_version%%-*}
# Ship the independently built C SDK with the developer tools. It stays a
# resource, not a dependency of the desktop executable or a source-project copy.
sh playbit/platform/native/build-sdk.sh > /dev/null
app="target/$pb_build_profile/Playbit.app"
# Recreate the bundle so every packaged executable belongs to this build.
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp "target/$pb_build_profile/libplaybit_runtime.dylib" "$app/Contents/Frameworks/"
xcrun clang -std=c17 -Oz -Wall -Wextra -Werror -fno-unwind-tables -fno-asynchronous-unwind-tables \
    -mmacosx-version-min="$MACOSX_DEPLOYMENT_TARGET" \
    '-DPLAYBIT_RUNTIME_PATH="@executable_path/../Frameworks/libplaybit_runtime.dylib"' \
    playbit/platform/macos/launcher.c -o "$app/Contents/MacOS/Playbit"
xcrun clang -std=c17 -Oz -Wall -Wextra -Werror -fno-unwind-tables -fno-asynchronous-unwind-tables \
    -mmacosx-version-min="$MACOSX_DEPLOYMENT_TARGET" \
    playbit/platform/macos/launcher.c -o "$app/Contents/Resources/guest-launcher"
xcrun strip "$app/Contents/MacOS/Playbit" "$app/Contents/Resources/guest-launcher"
cp "target/$pb_build_profile/playbit-engine" "target/$pb_build_profile/pb" "target/$pb_build_profile/playbit-mcp" "$app/Contents/MacOS/"
cp -R target/native-mac/sdk "$app/Contents/Resources/native-sdk"
cp playbit/platform/macos/runtime-entitlements.plist "$app/Contents/Resources/runtime-entitlements.plist"
cp playbit/platform/macos/guest-entitlements.plist "$app/Contents/Resources/guest-entitlements.plist"
python3 playbit/platform/macos/developer-resources.py "$app/Contents/Resources" "$pb_release"
sed -e "s/@MACOSX_DEPLOYMENT_TARGET@/$MACOSX_DEPLOYMENT_TARGET/g" \
    -e "s/@PLAYBIT_BUNDLE_VERSION@/$pb_bundle_version/g" \
    -e "s/@PLAYBIT_VERSION@/$pb_release/g" \
    -e "s/@PLAYBIT_RELEASE_SEQUENCE@/$pb_release_sequence/g" \
    -e "s/@PLAYBIT_APP_ID@/$pb_app_id/g" \
    -e "s/@PLAYBIT_APP_NAME@/$pb_app_name/g" \
    playbit/platform/macos/Info.plist > "$app/Contents/Info.plist"
if [ "$pb_build_profile" = debug ] || [ -n "$pb_developer_render" ]; then
    /usr/libexec/PlistBuddy -c 'Add :PlaybitDeveloperAlwaysRender bool true' "$app/Contents/Info.plist"
fi
# Compile once during packaging; guest launches only copy the prepared assets.
# Anchor asset compiler paths to this checkout even when its service is shared.
xcrun actool "$PWD/playbit/platform/macos/$pb_icon.icon" \
    --compile "$PWD/$app/Contents/Resources" --app-icon "$pb_icon" \
    --platform macosx --minimum-deployment-target "$MACOSX_DEPLOYMENT_TARGET" \
    --output-partial-info-plist "$PWD/target/$pb_build_profile/icon-info.plist"
test -s "$app/Contents/Resources/Assets.car"
test -s "$app/Contents/Resources/$pb_icon.icns"
/usr/libexec/PlistBuddy -c "Merge $PWD/target/$pb_build_profile/icon-info.plist" "$app/Contents/Info.plist"
mkdir -p "$app/Contents/Resources/guest-icon"
xcrun actool "$PWD/playbit/platform/macos/icon-guest.icon" \
    --compile "$PWD/$app/Contents/Resources/guest-icon" --app-icon icon-guest \
    --platform macosx --minimum-deployment-target "$MACOSX_DEPLOYMENT_TARGET" \
    --output-partial-info-plist "$PWD/target/$pb_build_profile/guest-icon-info.plist"
test -s "$app/Contents/Resources/guest-icon/icon-guest.icns"
cp "$app/Contents/Resources/guest-icon/icon-guest.icns" "$app/Contents/Resources/pbapp.icns"
PYTHONPATH="$PWD/scripts" python3 - "$app/Contents/Resources/guest-icon/Assets.car" \
    > "$app/Contents/Resources/guest-icon/version" <<'PYTHON'
import hashlib
import sys
from identifier import encode

with open(sys.argv[1], "rb") as assets:
    print(encode(hashlib.file_digest(assets, "sha256").digest()))
PYTHON
cp vendor/accesskit_macos/LICENSE-MIT "$app/Contents/Resources/AccessKit-LICENSE-MIT.txt"
python3 playbit/platform/macos/build-provider.py "$app" "target/$pb_build_profile/provider-signing"
python3 playbit/platform/macos/build-thumbnail.py "$app" "target/$pb_build_profile/thumbnail-signing"
# Developer ID and notarization credentials are chosen by private distribution
# tooling. Local builds use ad-hoc signing.
pb_entitlements=playbit/platform/macos/runtime-entitlements.plist
pb_engine_entitlements="target/$pb_build_profile/playbit-signing-entitlements.plist"
python3 playbit/platform/macos/keychain-signing.py "$app" "$pb_engine_entitlements"
if [ -z "${PB_PROVISIONING_PROFILE:-}" ]; then
    pb_engine_entitlements=$pb_entitlements
fi
pb_engine="$app/Contents/Helpers/playbit-engine.app"
if [ "$pb_sign_identity" = - ]; then
    codesign --force --sign - "$pb_engine"
else
    codesign --force --sign "$pb_sign_identity" --timestamp --options runtime \
        --entitlements "$pb_engine_entitlements" "$pb_engine"
fi
for executable in pb playbit-mcp; do
    if [ "$pb_sign_identity" = - ]; then
        codesign --force --sign - "$app/Contents/MacOS/$executable"
    else
        codesign --force --sign "$pb_sign_identity" --timestamp --options runtime \
            --entitlements "$pb_entitlements" "$app/Contents/MacOS/$executable"
    fi
done
# The launcher examples retain the signed packages issued by their publisher.
# The app seal also binds each installed resource to the release.
install_example() {
    pb_example="$app/Contents/Resources/examples/$1"
    mkdir -p "$pb_example"
    pb_filename="$pb_example/$1${5:-}.pbapp"
    cp "$3" "$pb_filename"
    cp "$4" "$pb_example/icon.png"
    pb_digest=$(PYTHONPATH="$PWD/scripts" python3 - "$pb_filename" <<'PY_DIGEST'
import hashlib
import sys
from identifier import encode

with open(sys.argv[1], "rb") as template:
    data = template.read(1024 * 1024 + 1)
if len(data) > 1024 * 1024:
    sys.exit(f"Bundled template exceeds 1 MiB: {sys.argv[1]}")
print(encode(hashlib.sha256(data).digest()))
PY_DIGEST
)
    /usr/libexec/PlistBuddy -c "Add :$2 string $pb_digest" "$app/Contents/Info.plist"
}
pb_packager="$PWD/$app/Contents/MacOS/pb"
install_channel_examples() {
    pb_sources=playbit/platform/macos/examples
    install_example "$1" "$2" "$pb_sources/$1.pbapp" "$3"
    install_example "$1" "${2}Testing" "$pb_sources/$1-testing.pbapp" "$3" -testing
    install_example "$1" "${2}Personal" "$pb_sources/$1-personal.pbapp" "$3" -personal
}
install_channel_examples little-list PlaybitLittleListTemplateSHA256 guests/little-list/icon.png
(
    pb_apps_commit=fbf5fabbc96f1d28a243be21495b8147cc023730
    pb_apps_cache="$PWD/target/$pb_build_profile/example-apps.git"
    git init --quiet --bare "$pb_apps_cache"
    if ! git --git-dir="$pb_apps_cache" cat-file -e "$pb_apps_commit^{commit}" 2>/dev/null; then
        git --git-dir="$pb_apps_cache" fetch --depth=1 --no-tags https://github.com/playbit/apps.git \
            "$pb_apps_commit:refs/pins/$pb_apps_commit"
    fi
    pb_apps_src=$(mktemp -d "$PWD/target/$pb_build_profile/example-apps.XXXXXX")
    trap 'rm -rf "$pb_apps_src"' EXIT
    git --git-dir="$pb_apps_cache" show "$pb_apps_commit:football/icon.png" > "$pb_apps_src/icon.png"
    install_channel_examples football PlaybitFootballTemplateSHA256 "$pb_apps_src/icon.png"
)
(cd guests/todo && "$pb_packager" build --release --sdk=system --template)
mkdir -p "$app/Contents/Resources/examples/todo"
cp target/release/TODO.pbapp "$app/Contents/Resources/examples/todo/todo.pbapp"

# Guest installation patches the template and re-signs it with its runtime
# constraint. The distribution template itself needs no guest entitlements.
if [ "$pb_sign_identity" = - ]; then
    codesign --force --sign - "$app/Contents/Frameworks/libplaybit_runtime.dylib"
    codesign --force --sign - "$app/Contents/Resources/guest-launcher"
else
    codesign --force --sign "$pb_sign_identity" --timestamp "$app/Contents/Frameworks/libplaybit_runtime.dylib"
    codesign --force --sign "$pb_sign_identity" --timestamp --options runtime "$app/Contents/Resources/guest-launcher"
fi
if [ "$pb_sign_identity" = - ]; then
    codesign --force --sign - --entitlements "target/$pb_build_profile/provider-signing/provider-host.plist" "$app"
else
    codesign --force --sign "$pb_sign_identity" --timestamp --options runtime \
        --entitlements "target/$pb_build_profile/provider-signing/provider-host.plist" "$app"
fi
codesign --verify --deep --strict "$app"
printf '%s\n' "$PWD/$app"
