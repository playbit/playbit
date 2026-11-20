# Language SDK sources

Each language has a directory containing its Playbit integration. Compiler
toolchains are downloaded separately from the bindings shipped in Playbit.app.
See [developer SDKs](../../docs/developer-sdks.md) for the installed workflow.

| Directory | Contents |
| --- | --- |
| `ocaml/` | Bindings, Rust AST generator, starter template files, compiler setup and integration tests. |
| `rust/template/` | Cargo configuration and starter template files. The Rust API lives in `playbit-api/`. |
| `toolchains/` | Shared archive assembly, catalog signing and functional qualification tools. |

Example applications live in `guests/`. The Mac resource staging step copies the
maintained guest source and root initializer into each installed starter:
`guests/counter-ocaml` for OCaml and `guests/little-list` for Rust. Keep application
code there instead of maintaining another copy in a template.

```mermaid
flowchart LR
    ABI[playbit-api Rust definitions] --> Generator[ocaml/bindgen]
    Generator --> API[Installed OCaml bindings]
    Templates[Language template files] --> Resources[Playbit.app resources]
    Guests[Maintained example guests] --> Resources
    API --> Resources
    Toolchains[Shared toolchain assembly] --> Archives[Downloadable compiler archives]
    Resources --> Build[pb SDK selection and build drivers]
    Archives --> Build
    Build --> App[Packaged guest app]
```

SDK selection, caching and build drivers live in `playbit/src/bin/pb/sdk/`.
Shared browser syscall adapters remain in `playbit/platform/web/src/`; the OCaml
browser scenario and its runners live in `ocaml/tests/`. The independent wasm32
ABI layout fixture lives in `playbit-api/tests/wasm-layout/`.

Shared SDK tools do not depend on service code. All language SDK sources,
examples and ABI fixtures are included in source publication through their
containing client directories.
