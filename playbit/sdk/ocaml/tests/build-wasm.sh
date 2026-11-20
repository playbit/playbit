#!/bin/sh
# Build the compiler/runtime fixture from the maintained counter guest.
set -eu
cd "$(dirname "$0")/.."
repo=$(CDPATH= cd ../../.. && pwd)
mode=${1:-debug}
case "$mode" in debug) optimization=1 ;; release) optimization=3 ;; *) echo 'usage: build-wasm.sh [debug|release]' >&2; exit 1 ;; esac
compiler_root=${JS_OF_OCAML_ROOT:-"$PWD/_build/js_of_ocaml"}
export OPAMROOT="$PWD/_build/opam"
export PATH="$PWD/_build/binaryen-version_133/bin:$PATH"
if [ ! -d "$OPAMROOT/playbit" ]; then
    echo 'Run sh playbit/sdk/ocaml/toolchain/setup-wasm.sh with OCaml 5.3.0 on PATH first.' >&2
    exit 1
fi
if [ -f _build/compiler-bin ]; then
    compiler_bin=$(cat _build/compiler-bin)
    export PATH="$compiler_bin:$PATH"
fi
if ! command -v ocamlc >/dev/null 2>&1; then
    echo 'Run toolchain/setup-wasm.sh with OCaml 5.3.0 on PATH.' >&2; exit 1
fi
export SCCACHE_SERVER_UDS="$repo/_sccache.sock"
export CARGO_TARGET_DIR="$PWD/_build/bindgen-target"
cargo run --quiet --locked --manifest-path bindgen/Cargo.toml -- "$repo/playbit-api" _build/generated
build="$PWD/_build/wasm/$mode"
mkdir -p "$build"
opam exec --switch=playbit -- sh -eu -c '
    build=$1
    repo=$2
    ocamlc -I "$build" -c -o "$build/playbit_generated.cmi" _build/generated/playbit_generated.mli
    ocamlc -I "$build" -c -o "$build/playbit_generated.cmo" _build/generated/playbit_generated.ml
    ocamlc -I "$build" -c -o "$build/playbit.cmi" playbit.mli
    ocamlc -I "$build" -c -o "$build/playbit.cmo" playbit.ml
    ocamlc -I "$build" -c -o "$build/counter.cmo" "$repo/guests/counter-ocaml/src/app.ml"
    ocamlc -no-check-prims -I "$build" "$build/playbit_generated.cmo" "$build/playbit.cmo" "$build/counter.cmo" -o "$build/app.byte"
' sh "$build" "$repo"
"$compiler_root/_build/default/compiler/bin-wasm_of_ocaml/wasm_of_ocaml.exe" \
    --target playbit --opt "$optimization" -o "$build/staging.wasm" \
    _build/generated/playbit_raw.wat _build/generated/playbit_ffi.wat "$build/app.byte"
wasm-tools validate "$build/staging.wasm"
mv "$build/staging.wasm" "$build/counter.wasm"
