# wasm32 ABI layout fixture

Compile all 34 Rust ABI records for wasm32 and inspect the
`playbit.abi.records` custom section: size, alignment, field count, then each
field's offset and width. The fixture also compiles the immutable name/type/
field-offset and full-struct-size checks in `../abi_prefix.rs`.

The `Event` header retains its separate
`playbit.abi.layout` custom section without executing the module. The five
little-endian `u32` values record size, alignment, and the offsets of `generation`,
`kind`, and `target`. This fixture is independent of any guest language.

```sh
cargo build --locked --manifest-path playbit-api/tests/wasm-layout/Cargo.toml \
    --target wasm32-unknown-unknown
```

The OCaml contract test in `playbit/sdk/ocaml/tests/test-wasm.py` builds this fixture
into its own scratch directory and checks that the generated adapter uses the
actual wasm32 layout. Native Rust layout is not a substitute for this check.

The independent C/Rust lowering probe compares current records with
`playbit/platform/native/wasm-probe/layout-v2.tsv`. The earlier `layout.tsv` and
`abi_prefix.rs` retain the published prefixes, including the 80-byte Volume.
Current Volume records append the role at offset 80, occupy 88 bytes, and retain
the 80-byte minimum. Callers supply their own array stride; hosts write a role
only when its complete four-byte field fits the offered record.
