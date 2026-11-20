#!/bin/sh
# Release builder only. Build the pinned native compiler for Playbit's minimum OS.
set -eu
sdk_builder_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
sdk_output=${1:?Usage: bootstrap-ocaml.sh ABSOLUTE_OUTPUT_DIRECTORY}
case "$sdk_output" in /*) ;; *) echo 'SDK output must be absolute' >&2; exit 1 ;; esac
mkdir -p "$sdk_output"
export MACOSX_DEPLOYMENT_TARGET=15.0
export CFLAGS="${CFLAGS:-} -mmacosx-version-min=15.0"
# Xcode 27 declares weak pipe2/dup3 symbols that are unavailable at this
# deployment target. Autoconf link-only probes otherwise enable crashing calls.
export LDFLAGS="${LDFLAGS:-} -mmacosx-version-min=15.0"
if [ ! -f "$sdk_output/native-compiler.identity" ]; then
    if [ ! -f "$sdk_output/ocaml-5.3.0.tar.gz" ]; then
        curl -fL https://codeload.github.com/ocaml/ocaml/tar.gz/refs/tags/5.3.0 -o "$sdk_output/ocaml-5.3.0.tar.gz"
    fi
    printf '%s  %s\n' eb9eab2f21758d3cfb1e78c7f83f0b4dd6302824316aba4abee047a5a4f85029 "$sdk_output/ocaml-5.3.0.tar.gz" | shasum -a 256 -c -
    if [ ! -d "$sdk_output/ocaml-5.3.0" ]; then tar -xzf "$sdk_output/ocaml-5.3.0.tar.gz" -C "$sdk_output"; fi
    (cd "$sdk_output/ocaml-5.3.0" && ac_cv_func_pipe2=no ac_cv_func_dup3=no ./configure --prefix="$sdk_output/ocaml" --without-zstd && gmake -j2 world.opt && gmake install)
    printf '%s\n' 'ocaml-5.3.0-macos15-without-zstd-pipe2-dup3' > "$sdk_output/native-compiler.identity"
fi
export PATH="$sdk_output/ocaml/bin:$PATH"
export OPAMROOT="$sdk_output/opam"
unset OPAMSWITCH
if [ ! -f "$OPAMROOT/config" ]; then opam init --bare --no-setup --disable-sandboxing -y default https://opam.ocaml.org; fi
if [ ! -d "$OPAMROOT/playbit" ]; then opam switch create playbit ocaml-system.5.3.0 -y; fi
opam install --switch=playbit --jobs=2 -y $(awk '{print $1 "." $2}' "$sdk_builder_dir/ocaml.opam-packages")
printf '\nNative OCaml: %s/ocaml\nIsolated OPAMROOT: %s\n' "$sdk_output" "$OPAMROOT"
