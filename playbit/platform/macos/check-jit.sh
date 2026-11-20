#!/bin/sh
# Prove that guest code runs under the hardened runtime with only the
# entitlements in runtime-entitlements.plist (allow-jit, no unsigned executable
# memory). Signs the playbit library test binary ad hoc with those entitlements
# and runs the runtime tests from it. If guest code ever stops going through
# the runtime's MAP_JIT allocator (playbit/src/runtime/jit_memory.rs), the
# process is killed at the first mprotect(PROT_EXEC).
set -eu
cd "$(dirname "$0")/../../.."
test "$(uname -s)" = Darwin || { echo 'check-jit.sh requires macOS' >&2; exit 1; }
binary=$(cargo test --locked -p playbit --lib --no-run 2>&1 | sed -n 's/.*(\(target\/[^)]*\)).*/\1/p' | tail -1)
test -n "$binary" || { echo 'could not locate the playbit test binary' >&2; exit 1; }
signed=$(mktemp -d)/playbit-jit-check
cp "$binary" "$signed"
codesign --force --sign - --options runtime \
    --entitlements playbit/platform/macos/runtime-entitlements.plist "$signed"
profile=$(mktemp -d)
cd playbit
PLAYBIT_PROFILE="$profile" "$signed" runtime:: "$@"
