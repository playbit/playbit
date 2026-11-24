#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$repo"
profile=${1:-release}
case "$profile" in
    debug) set -- ; sourcemap=--sourcemap ;;
    release) set -- --release ; sourcemap= ;;
    *) echo 'Usage: build.sh [debug|release]' >&2; exit 1 ;;
esac
web=playbit/platform/web
# esbuild is the only JavaScript dependency; package-lock.json pins it.
[ -x "$web/node_modules/.bin/esbuild" ] || npm ci --prefix "$web" --no-audit --no-fund
# The web client is the playbit library built as a wasm32 cdylib; the manifest
# declares only rlib so desktop builds do not link it.
sh scripts/prepare-apps.sh
cargo rustc --locked "$@" --target wasm32-unknown-unknown -p playbit --lib --crate-type cdylib --no-default-features
cargo rustc --locked "$@" --target wasm32-unknown-unknown -p todo --lib --crate-type cdylib
cargo build --locked "$@" --target wasm32-unknown-unknown -p micro-kart -p shared-note
cargo build --locked -p playbit --bin pb --bin playbit-engine --bin playbit-mcp --no-default-features --features process

if [ "$profile" = debug ]; then
    # The browser verifies every example against one publisher, even in a local build.
    signer_dir=$(mktemp -d /tmp/pb-web.XXXXXX)
    trap 'rm -rf "$signer_dir"' EXIT
    trap 'exit 1' HUP INT TERM
    PLAYBIT_PROFILE=$(cd "$signer_dir" && pwd -P)/profile
    export PLAYBIT_PROFILE
    unset PLAYBIT_CHANNEL PLAYBIT_TESTING_CREDENTIALS
    python3 scripts/offline_fixture.py "$PLAYBIT_PROFILE"
fi

pb_packager="$repo/target/debug/pb"
for project in todo micro-kart shared-note; do
    (cd "guests/$project" && "$pb_packager" bundle "$@")
done
output="$repo/target/web"
rm -rf "$output"
mkdir -p "$output/tests"
# One module bundle per browser context: the page, the runtime Worker and the
# AP SharedWorker. index.html and the entry modules refer to these names.
"$web/node_modules/.bin/esbuild" --bundle --format=esm --log-level=warning $sourcemap --outdir="$output" \
    playbit="$web/src/app.ts" runtime-worker="$web/src/runtime-worker.ts" ap-worker="$web/src/ap-worker.ts" native-handoff="$web/src/native-handoff.ts"
# Browser probes for manual and scripted checks (STORAGE.md, scripts/). They are
# served beside the app at /tests/ and excluded from release asset manifests.
"$web/node_modules/.bin/esbuild" --bundle --format=esm --log-level=warning --outdir="$output/tests" \
    "$web"/tests/account-bootstrap-browser.mjs "$web"/tests/account-outbox-browser.mjs "$web"/tests/account-store-browser.mjs \
    "$web"/tests/browser-account.mjs "$web"/tests/personal-store-browser.mjs \
    "$web"/tests/installation-browser.mjs "$web"/tests/storage-registration.mjs "$web"/tests/opfs-recovery-browser.mjs
cp "$web"/tests/*.html "$web/tests/reconnect-browser-worker.js" "$output/tests/"
cp "$web/controls.css" "$web/index.html" "$output/"
cp playbit/src/channels.json "$output/"
cp playbit/assets/InterVariable.ttf playbit/assets/check-*.svg "$output/"
cp "target/wasm32-unknown-unknown/$profile/playbit.wasm" "$output/"
cp "target/$profile/TODO.pbapp" "$output/todo.pbapp"
cp "target/$profile/Micro Kart.pbapp" "$output/micro-kart.pbapp"
cp "target/$profile/Shared Note.pbapp" "$output/shared-note.pbapp"
python3 "$web/example-publisher.py" "$repo/target/debug/playbit-mcp" "$output"
printf 'Web build: %s\nRun: python3 -m http.server 8787 --bind 127.0.0.1 --directory %s\n' "$output" "$output"
