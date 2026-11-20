#!/bin/bash
# Shared verification commands for local and GitHub execution.
set -euo pipefail

# Check evidence tooling correctness.
python3 scripts/test-benchmark-local.py
python3 scripts/test-process-capture.py
python3 scripts/test-performance-guard.py
python3 -m unittest discover -s scripts -p 'test_renderer.py'
python3 -m unittest discover -s scripts -p 'test_prepare_fixture.py'

# Prepare independent browser guest fixtures.
export PATH="/usr/lib/llvm-23/bin:$PATH"
test -x /usr/lib/llvm-23/bin/clang
test -x /usr/lib/llvm-23/bin/wasm-ld
clang --version
wasm-ld --version
sh playbit/platform/native/build-wasm-probe.sh --build-only
sh scripts/prepare-apps.sh

# Signed example packaging uses the ordinary local engine with explicit offline fixture authority.
signer_work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/pb-offline.XXXXXX")
python3 scripts/offline_fixture.py "$signer_work/profile"
# Build the browser client.
PLAYBIT_PROFILE="$signer_work/profile" sh playbit/platform/web/build.sh debug

# Check projected query browser codec.
node playbit/platform/web/tests/projected-query.mjs target/wasm32-unknown-unknown/debug/playbit.wasm

# Check browser journal and streamed synchronization.
node playbit/platform/web/tests/opfs-stream.mjs
node playbit/platform/web/tests/streaming-sync.mjs target/wasm32-unknown-unknown/debug/playbit.wasm
node playbit/platform/web/tests/account-store.mjs target/wasm32-unknown-unknown/debug/playbit.wasm

sh playbit/platform/web/check.sh

# This Node harness supplies an ordinary Root, not automatic Session scopes.
# Compile its explicit fixture only after packaging the strict default TODO.
cargo rustc --locked --lib --crate-type cdylib --release --target wasm32-unknown-unknown -p todo --features fixture-root
node --test playbit/platform/web/src/*.test.mjs

# Check actual browser consent and account outbox transactions.
node scripts/check-browser-account-outbox.mjs
