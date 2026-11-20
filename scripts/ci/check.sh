#!/bin/bash
# Execute one public check group in a prepared, independent source tree.
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo"
group=${1:?Usage: check.sh GROUP}
case "$group" in
    style|kvdb-coverage|clippy|linux|macos|macos-packaged|web|tooling) ;;
    *) echo "Unknown public check group: $group" >&2; exit 1 ;;
esac
case "${CI_STORAGE_HISTORY:-true}" in
    true|false) ;;
    *) echo 'CI_STORAGE_HISTORY must be true or false' >&2; exit 1 ;;
esac
export RUSTUP_TOOLCHAIN=1.99.0 RUST_BACKTRACE=1 CARGO_INCREMENTAL=0
export GITHUB_WORKSPACE="$repo"
export CARGO_TARGET_DIR="$repo/target"
unset CARGO_BUILD_TARGET CARGO_ENCODED_RUSTFLAGS RUSTFLAGS
temporary=0
if [ -z "${RUNNER_TEMP:-}" ]; then
    RUNNER_TEMP=$(mktemp -d "${TMPDIR:-/tmp}/playbit-check.XXXXXXXX")
    temporary=1
fi
export RUNNER_TEMP
export GITHUB_STEP_SUMMARY=${GITHUB_STEP_SUMMARY:-$RUNNER_TEMP/summary.md}
started=$SECONDS
cleanup() {
    result=$?
    trap - EXIT
    printf '\n%s: exit %s, %ss\n' "$group" "$result" "$((SECONDS - started))"
    if [ "$temporary" = 1 ]; then
        rm -rf "$RUNNER_TEMP"
    fi
    exit "$result"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
case "$group" in
    macos|macos-packaged|clippy)
        test "$(uname -s)" = Darwin || { echo "$group requires macOS and Xcode" >&2; exit 1; }
        . scripts/macos-env.sh
        ;;
    linux|web|kvdb-coverage)
        test "$(uname -s)" = Linux || { echo "$group requires Linux" >&2; exit 1; }
        ;;
esac
case "$group" in
    clippy|linux|kvdb-coverage) export RUSTFLAGS='-D warnings' ;;
esac
case "$group" in
    macos|macos-packaged|web)
        node -e 'if (process.versions.node.split(".")[0] !== "26") throw Error("CI requires Node 26")'
        ;;
esac
if [ "$group" = kvdb-coverage ]; then
    export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2
fi
. "$repo/scripts/ci/$group.sh"
