# `playbit.toml` reference

This `playbit.toml` describes a Rust tea timer that ships a bell sound and a help file.

```toml
app_id = "tea-timer"
name = "Tea Timer"
lang = "rust"
icon = "icon.svg"
required_host_features = ["file.open.begin", "file.open.take", "file.read.begin", "file.read.take"]
build_cmd = "cargo rustc --lib --crate-type cdylib"

# The app opens assets/sounds/bell.wav as sounds/bell.wav.
files_dir = "assets"

[files]
"help/start.txt" = "docs/getting-started.txt"

[release]
build_cmd = "cargo rustc --release --lib --crate-type cdylib"

[native]
build_cmd = "cargo rustc --bin playbit-app --features native --target aarch64-apple-darwin -- @target/playbit/native.rsp"

[native.release]
build_cmd = "cargo rustc --release --bin playbit-app --features native --target aarch64-apple-darwin -- @target/playbit/native.rsp"

[trace]
version = 1

[[trace.names]]
name = "timer.ring"
kind = "mark"
description = "The timer rang"
```

The build commands come from the Rust starter. Cargo supplies the debug and
release Wasm paths, so the example omits `build_artifact`. The app includes
the generated `target/playbit/metadata.rs` to record its `TIMER_RING` trace mark.

`pb` reads `playbit.toml` from the app's project root. It accepts up to
320 KiB of UTF-8 TOML and rejects unknown top-level keys. Every project also
needs a sibling `playbit-volumes.json` for its volume initializers. Use `{}` for an app with
no declarations. [Volume initialization](root-initialization.md)

## Bundled resources

Use the top-level `files_dir` string to import a directory, the `[files]` table
to choose individual files and their bundle paths, or both. Put `files_dir`
before the first table header so TOML assigns it to the project. The example
above produces these paths.

| File in the project | Path in the `.pbapp` | How it gets there |
| --- | --- | --- |
| `assets/sounds/bell.wav` | `sounds/bell.wav` | `files_dir` copies the directory's contents recursively, preserving paths relative to `assets/`. |
| `docs/getting-started.txt` | `help/start.txt` | Each `[files]` **key** is a bundle destination and its **value** is a source file relative to `playbit.toml`. |

Quote destination keys containing slashes or dots so TOML reads the whole
path as one key. Both forms take literal paths. Directory imports include
hidden files and apply no glob or ignore rules. Directories in the bundle are
implicit, so empty directories produce no entries. Omitting both declarations
adds no resource files.

**Path and size rules**

- Sources must stay inside the project. Absolute paths, parent traversal
  (`..`), symlinks in any component, and special files are rejected.
- Bundle paths are relative, case-sensitive UTF-8 and use `/` as the separator.
  Each path can contain up to 512 bytes and 64 components of up to 255 bytes
  each. Empty components, `.`, `..`, NUL and backslashes are invalid.
- Each destination must be unique across both declarations. A duplicate fails
  packaging even if both files have identical bytes. A file and a directory
  cannot share a path.
- Keep ordinary resources outside the reserved `app/` namespace. Set the
  top-level `icon` field for `app/icon`. The package format also defines a
  namespace for optional extension data. [Bundle bytes and paths](../playbit/src/package/README.md#bundle-bytes-and-paths)
- A resource can contain up to 256 MiB. The complete bundle can contain up to
  1,024 file entries, including Playbit's own entries, and occupy up to 512 MiB.
  A directory import can visit up to 1,024 directories including its root.

`pb build` embeds the file bytes. After a resource-only edit, `pb bundle`
repackages the existing Wasm artifact. At runtime, find the read-only volume
with backing `VOLUME_BUNDLE` through `volume.list`. Open paths such as
`sounds/bell.wav` relative to that volume's root and read them with the
asynchronous File API. [File operations](../playbit-api/syscalls.md#files)

## Project fields

| Top-level key | Type | Meaning |
| --- | --- | --- |
| `app_id` | String, required | App identity within the publisher's account. Use 1–64 bytes of lowercase ASCII letters and digits, with single dots or hyphens between nonempty components. |
| `name` | String, required | Display name. `pb` also uses it for the output filename, which must fit in 255 bytes including the extension. Blank names, slashes, colons and control characters are rejected when building. |
| `lang` | String | `"rust"` or `"ocaml"`. Omitting it allows a Cargo build or a custom build command. `"ocaml"` selects the managed OCaml build. |
| `required_host_features` | Array of strings | Host capabilities required for execution. Defaults to `[]`. Supply up to 64 distinct names in ascending byte order. Each name is 1–128 bytes of lowercase ASCII letters, digits, dots, underscores or hyphens. |
| `icon` | String | Project-relative source file to embed as `app/icon`, up to 1 MiB. Omit it for the default app icon. [Supported artwork](developing-apps.md#icons) |
| `files_dir` | String | Project-relative directory whose contents are imported at the bundle root. |
| `[files]` | Table of strings | Maps bundle destinations to project-relative source files. [Bundled resources](#bundled-resources) |
| `[trace]` | Table | Optional versioned catalog of ordered event names. Set `version = 1` and declare each event with `[[trace.names]]`. [App trace declarations](app-trace-declarations.md) |

## Build commands and artifacts

Each `build_cmd` is a nonblank shell command of up to 8,192 bytes, run by
`sh -c` from the project root. The build mode selects its fields as follows.

| Build mode | Manifest fields |
| --- | --- |
| Wasm, debug | Top-level `build_cmd` is required for command-driven builds. Optional `build_artifact` gives the project-relative path to the resulting `.wasm` file. |
| Wasm, release | Omitting `[release]` selects `cargo build --release`. If the table is present it must contain its own `build_cmd`. Use `build_artifact` here for the release Wasm path. |
| Native Mac | To support `--native`, supply a `build_cmd` in both `[native]` and `[native.release]`. Cargo supplies the native executable path. |

Set `build_artifact` for **both** Wasm modes when providing explicit paths.
Both files must resolve inside the project. With both paths omitted, `pb`
discovers the Wasm artifact from the project's Cargo library target.
[Build examples](developing-apps.md#build-run-and-update)

For `lang = "ocaml"`, omit build commands, artifact paths, native build tables
and trace declarations. The managed driver compiles `src/app.ml` and packages
the result with the same resource rules. [Developer SDKs](developer-sdks.md)
