#!/bin/sh
# Local compiler/dependencies only; invoke with an OCaml 5.3.0 compiler on PATH.
set -eu
cd "$(dirname "$0")/.."
compiler_root=${JS_OF_OCAML_ROOT:-"$PWD/_build/js_of_ocaml"}
upstream=61beaa1103b2edf2c079e0cd63f99574af0fbf21
if [ "$(ocamlc -version)" != 5.3.0 ]; then
    echo 'This toolchain requires OCaml 5.3.0 on PATH.' >&2; exit 1
fi
mkdir -p _build
if [ ! -e "$compiler_root" ]; then
    git clone https://github.com/playbit/js_of_ocaml.git "$compiler_root"
fi
if ! git -C "$compiler_root" rev-parse --git-dir >/dev/null; then
    echo "The compiler directory is not a Git checkout: $compiler_root" >&2
    exit 1
fi
compiler_bin=$(dirname "$(command -v ocamlc)")
printf '%s\n' "$compiler_bin" > _build/compiler-bin
export PATH="$compiler_bin:$PWD/_build/binaryen-version_133/bin:$PATH"
export OPAMROOT="$PWD/_build/opam"
if [ ! -f "$OPAMROOT/config" ]; then
    opam init --bare --no-setup --disable-sandboxing -y default https://opam.ocaml.org
fi
if [ ! -d "$OPAMROOT/playbit" ]; then
    opam switch create playbit ocaml-system.5.3.0 -y
fi
# This manifest records every installed dependency, not floating package names.
opam install --switch=playbit -y $(awk '{print $1 "." $2}' toolchain/ocaml.opam-packages)
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) binaryen_platform=arm64-macos; binaryen_sha=ad66da82ac13f163e424b1643f16c6dfcccc98b5966296b43e52d3cab04f84a8 ;;
    *) echo 'This pinned setup currently supports arm64 macOS.' >&2; exit 1 ;;
esac
if [ ! -x _build/binaryen-version_133/bin/wasm-opt ]; then
    curl -fL "https://github.com/WebAssembly/binaryen/releases/download/version_133/binaryen-version_133-$binaryen_platform.tar.gz" -o _build/binaryen.tar.gz
    printf '%s  %s\n' "$binaryen_sha" _build/binaryen.tar.gz | shasum -a 256 -c -
    tar -xzf _build/binaryen.tar.gz -C _build
fi
if ! git -C "$compiler_root" merge-base --is-ancestor "$upstream" HEAD; then
    echo "The compiler fork must descend from $upstream." >&2
    exit 1
fi
# The Playbit target changes live in the separate fork; setup never resets it.
cd "$compiler_root"
opam exec --switch=playbit -- dune build compiler/bin-wasm_of_ocaml/wasm_of_ocaml.exe
"_build/default/compiler/bin-wasm_of_ocaml/wasm_of_ocaml.exe" --help=plain | grep -q 'target=HOST'
