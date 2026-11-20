#!/bin/sh
# Build a compatibility harness against the production browser adapters and host.
set -eu
cd "$(dirname "$0")/../../../.."
export SCCACHE_SERVER_UDS="$PWD/_sccache.sock"
sh playbit/sdk/ocaml/tests/build-wasm.sh debug
cargo rustc --locked --target wasm32-unknown-unknown -p playbit --lib --crate-type cdylib --no-default-features
if [ ! -x playbit/platform/web/node_modules/.bin/esbuild ]; then
    npm ci --prefix playbit/platform/web --no-audit --no-fund
fi
playbit/platform/web/node_modules/.bin/esbuild --bundle --format=esm --log-level=warning \
    playbit/sdk/ocaml/tests/browser.ts --outfile=playbit/sdk/ocaml/_build/browser.js
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) ;;
    *) echo 'The installed-browser harness currently targets arm64 macOS.' >&2; exit 1 ;;
esac
if [ ! -x playbit/sdk/ocaml/_build/geckodriver ]; then
    curl -fL https://github.com/mozilla/geckodriver/releases/download/v0.37.1/geckodriver-v0.37.1-macos-aarch64.tar.gz -o playbit/sdk/ocaml/_build/geckodriver.tar.gz
    printf '%s  %s\n' d02b3f7003f999caf90974a2ef5da0286c05d01cee19112c86846d759fdba4f5 playbit/sdk/ocaml/_build/geckodriver.tar.gz | shasum -a 256 -c -
    tar -xzf playbit/sdk/ocaml/_build/geckodriver.tar.gz -C playbit/sdk/ocaml/_build
fi
swiftc -module-cache-path playbit/sdk/ocaml/_build/swift-module-cache \
    playbit/sdk/ocaml/tests/check-webkit.swift -o playbit/sdk/ocaml/_build/check-webkit
