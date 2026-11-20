.PHONY: wasm wasm-release wasm-test clean
wasm:
	sh tests/build-wasm.sh debug

wasm-release:
	sh tests/build-wasm.sh release

wasm-test: wasm
	python3 tests/test-wasm.py
	cd ../../.. && SCCACHE_SERVER_UDS="$$(pwd)/_sccache.sock" cargo test --locked -p playbit --no-default-features --features wasm-runtime --lib ocaml_counter -- --ignored --nocapture

clean:
	rm -rf _build
