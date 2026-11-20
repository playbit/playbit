#!/bin/sh
# Build the local macOS testing kit. No signing identity or notarization needed.
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo 'build.sh requires macOS and Xcode.' >&2
    exit 1
fi
if [ "$#" -ne 0 ]; then
    echo 'Usage: ./build.sh' >&2
    exit 1
fi

repo=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$repo"
# All outputs belong to this checkout, including when invoked from another cwd.
export CARGO_TARGET_DIR="$repo/target"
unset CARGO_BUILD_TARGET

sh scripts/prepare-apps.sh
cargo build --locked --workspace --exclude gpui --all-targets
sh playbit/platform/macos/package.sh
sh playbit/platform/web/build.sh debug

# Use the same public source workflow as a developer. Each temporary source has
# a real isolated development volume; no fabricated root IDs or retired CLI.
for guest in todo micro-kart ui-gallery; do
    python3 playbit/platform/ios/prepare-fixture.py "$guest" "target/debug/$guest.pbapp" \
        --kit target/debug/Playbit.app --profile debug
done

cat <<EOF

Debug build ready:
  App:     ./target/debug/Playbit.app
  Tools:   ./target/debug/Playbit.app/Contents/MacOS/{pb,playbit-mcp}
  Apps:    ./target/debug/{todo,micro-kart,ui-gallery}.pbapp
  Web:     ./target/web

See docs/developing-apps.md for app development and docs/building.md for tests.
Web: python3 -m http.server 8787 --bind 127.0.0.1 --directory "$repo/target/web"
EOF
