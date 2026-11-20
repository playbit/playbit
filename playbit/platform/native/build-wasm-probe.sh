#!/bin/sh
# Compile independently, then execute both modules against a scalar-import observer.
# Requires LLVM 22 or newer (clang and wasm-ld on PATH).
set -eu
case "${1-}" in
    ""|--build-only) ;;
    *) echo "Usage: $0 [--build-only]" >&2; exit 2 ;;
esac
cd "$(dirname "$0")/../../.."
sh playbit/platform/native/check-abi-types.sh
mkdir -p target/wasm-lowering
clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/app.c \
    -o target/wasm-lowering/app.o
wasm-ld --no-entry --export=probe_base --export=probe_layout --export=probe_imports \
    --export=probe_invalid target/wasm-lowering/app.o -o target/wasm-lowering/c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name playbit_api \
    --crate-type rlib -D warnings -C opt-level=2 -C panic=abort \
    playbit-api/src/lib.rs -o target/wasm-lowering/libplaybit_api.rlib
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name lowering_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/app.rs -o target/wasm-lowering/rust.wasm
clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/memory-app.c \
    -o target/wasm-lowering/memory-app.o
wasm-ld --no-entry --export=probe_memory_begin --export=probe_memory_take target/wasm-lowering/memory-app.o \
    -o target/wasm-lowering/memory-c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name memory_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/memory-app.rs -o target/wasm-lowering/memory-rust.wasm
clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/result-app.c \
    -o target/wasm-lowering/result-app.o
wasm-ld --no-entry --export=probe_results --export=probe_result_begin --export=probe_result_take target/wasm-lowering/result-app.o \
    -o target/wasm-lowering/result-c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name result_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/result-app.rs -o target/wasm-lowering/result-rust.wasm

clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/schema-app.c \
    -o target/wasm-lowering/schema-app.o
wasm-ld --no-entry --export=probe_schema --export=probe_schema_establish_begin --export=probe_schema_establish_take --export=probe_schema_read_begin target/wasm-lowering/schema-app.o \
    -o target/wasm-lowering/schema-c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name schema_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/schema-app.rs -o target/wasm-lowering/schema-rust.wasm
clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/query-app.c \
    -o target/wasm-lowering/query-app.o
wasm-ld --no-entry --export=probe_query_begin --export=probe_query_take target/wasm-lowering/query-app.o \
    -o target/wasm-lowering/query-c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name query_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/query-app.rs -o target/wasm-lowering/query-rust.wasm
clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/pending-app.c \
    -o target/wasm-lowering/pending-app.o
wasm-ld --no-entry --export=probe_pending target/wasm-lowering/pending-app.o \
    -o target/wasm-lowering/pending-c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name pending_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/pending-app.rs -o target/wasm-lowering/pending-rust.wasm
clang --target=wasm32 -std=c17 -ffreestanding -nostdlib -Wall -Wextra -Werror -O2 \
    -I playbit-api/include -c playbit/platform/native/wasm-probe/authenticated-app.c \
    -o target/wasm-lowering/authenticated-app.o
wasm-ld --no-entry --export=probe_authenticated --export=probe_authenticated_begin target/wasm-lowering/authenticated-app.o \
    -o target/wasm-lowering/authenticated-c.wasm
rustc --edition=2024 --target wasm32-unknown-unknown --crate-name authenticated_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/authenticated-app.rs -o target/wasm-lowering/authenticated-rust.wasm

rustc --edition=2024 --target wasm32-unknown-unknown --crate-name events_probe \
    --crate-type cdylib -D warnings -C opt-level=2 -C panic=abort \
    --extern playbit_api=target/wasm-lowering/libplaybit_api.rlib \
    playbit/platform/native/wasm-probe/events-app.rs -o target/wasm-lowering/events-rust.wasm

# The browser checks consume these same independent modules without native tests.
[ "${1-}" != --build-only ] || exit 0

# All fixtures exist before Cargo builds either test executable.
sh scripts/prepare-apps.sh
cargo test --locked -p playbit --test wasm_lowering -- --ignored --nocapture
cargo test --locked -p playbit --lib -- --ignored --nocapture --exact \
    runtime::wasm::operations::tests::compiler_mutations_use_separate_begin_and_take_exports \
    runtime::wasm::operations::result_tests::compiler_variable_results_use_separate_begin_and_take_exports \
    runtime::wasm::operations::schema_tests::compiler_schema_records_use_separate_begin_and_take_exports \
    runtime::wasm::operations::query_tests::compiler_query_records_use_separate_begin_and_take_exports \
    runtime::wasm::operations::pending_tests::compiler_pending_records_reach_named_imports \
    runtime::wasm::operations::authenticated_tests::compiler_authenticated_records_reach_named_imports
