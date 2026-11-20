# docdb
Experimental multiplayer library for Rust.

docdb's API, generated bindings, schemas and stored data are excluded from
Playbit's first-release compatibility promise. They may change incompatibly
without migrations. Use docdb for experiments with disposable or separately
exported data. The supported Playbit object database is a separate implementation.

The current generator assigns scalar offsets and secondary-object keys from
field declaration order. Reordering, removing or inserting fields can reinterpret
existing data. Before docdb becomes supported, its schema declarations and
generator must preserve permanent field locations and validate additive changes.

## Usage

The schema generator is part of `pb`, separate from the dependency-free
document library. From this directory, generate `types.rs` from `types.json`:

```
pb schema-gen types.json
```

Pass multiple JSON paths to generate several files. Each input must end in `.json`
and be at most 1 MiB. Generation replaces the sibling `.rs` file and stops with
a nonzero exit status on the first error. It does not require a running engine.

The output `types.rs` file should match the copy in `src`. Run the library tests
with `cargo test --release --locked`.
