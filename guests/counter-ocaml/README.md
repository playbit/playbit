# OCaml counter

A Playbit guest with Increment and Reset buttons. It keeps OCaml strings,
closures and a bounded history in the Wasm GC heap between turns. Reopening or
replacing its package starts a new heap; the example does not store durable data.

With the matching installed Playbit developer tools, run from this directory:

```sh
pb build
pb run 'target/debug/OCaml Counter.pbapp'
```

`pb build --release` packages the optimized app under `target/release/`.
The adjacent role initializer creates fresh private data when running the bundle.
The manifest's volume is a placeholder for template initialization.

For a source-watching development project with its own volume, use `pb init` in
a new directory, then `pb run`:

```sh
mkdir my-counter && cd my-counter
pb init --lang ocaml
pb run
```

Playbit stages this guest's source, manifest and role initializer into the OCaml
starter during packaging. Edit `src/app.ml` here to maintain both the example and
the starter. Language bindings and compiler tests live in
[`playbit/sdk/ocaml/`](../../playbit/sdk/ocaml/README.md). See
[developer SDKs](../../docs/developer-sdks.md) for tool selection and supported modules.
