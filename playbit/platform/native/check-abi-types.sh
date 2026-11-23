#!/bin/sh
# Syntax-only field-kind checks require no target libc or linker installation.
set -eu
cd "$(dirname "$0")/../../.."
for target in wasm32-unknown-unknown arm64-apple-macos x86_64-linux-musl; do
    "${CC:-clang}" --target="$target" -ffreestanding -nostdlibinc -std=c17 -Wall -Wextra -Werror -pedantic-errors \
        -I playbit-api/include -I playbit/include -fsyntax-only playbit/platform/native/wasm-probe/type-check.c
    "${CXX:-clang++}" --target="$target" -ffreestanding -nostdlibinc -std=c++20 -x c++ -Wall -Wextra -Werror -pedantic-errors \
        -I playbit-api/include -I playbit/include -fsyntax-only playbit/platform/native/wasm-probe/type-check.c
done
