---
name: playbit
description: Build and modify Playbit apps with a live native GUI and hot reload; explain Playbit volumes, sessions, and synchronization using the installed SDK and tools. Use for Playbit app development and help, not development of the Playbit runtime itself.
---

# Playbit

Playbit apps are living documents backed by independently stored data. Build
against the user's installed Playbit.app. Do not require an implementation
checkout or recreate its runtime in a web page.

## Find the installed capabilities

Resolve this skill's symlink to its real directory in
`Playbit.app/Contents/Resources/skills/playbit`. The matching CLI is
`../../../MacOS/pb` relative to that directory; the Rust SDK is
`../../playbit-api`. Use absolute, quoted paths (app paths can contain spaces).
Use that CLI's `version` and `help` to discover commands. Keep the CLI, engine,
SDK, and GUI from the same installation and use the same `PLAYBIT_PROFILE`.

Read [app development](references/developing-apps.md) when creating or editing
an app, and [data and images](references/data-and-images.md) when adding storage
or images. The installed SDK source is the reference for precise Rust signatures:
`../../playbit-api/src/context/` and `../../playbit-api/src/ui/mod.rs`.

For general product context, see [Playbit](https://playbit.app/). The bundled
references work offline. If website guidance differs from the installed tools,
follow the installed capabilities. Report an unavailable capability instead of
inventing an API, command, permission, or substitute architecture.

## Use CLI and MCP together

Use `pb` for scaffolding, building, the retained `pb run` watcher, and GUI
inspection. When the agent has Playbit MCP connected, use its `playbit` tool
for structured session and authorized data operations and `playbit_gui` for
instance-targeted canvas input and rendered screenshots. Discover the installed
tools through `tools/list`. Both interfaces use the same engine and profile.

The bundled stdio server is `../../../MacOS/playbit-mcp` relative to this skill.
An MCP client must be configured to launch it; installing this skill alone does
not connect MCP. Runtime calls start the engine on demand. Keep the MCP process
alive between opening and closing a data context, since contexts belong to that
process. Inspect state before retrying a mutation whose result is uncertain.

## Create, show, then iterate

1. Use the user's chosen source directory. If none is established, ask where
   to keep the app. Keep source outside the app bundle. Run
   `pb init path/to/app --name "App Name"`; it creates missing directories,
   including parents, and requires an empty destination. It supplies a working
   TODO starter, a matching SDK dependency, and a development root volume.
2. Change into the source directory. Immediately run `pb run` in a retained
   terminal/process and let it open the native GUI **before implementing the
   requested changes**. Keep this source
   watcher running while editing. Save the session ID from its JSON output.
3. Inspect the generated `src/app.rs`, `playbit.toml`, and relevant SDK methods.
   Make small usable changes. Successful builds hot-reload the same session;
   failed builds retain the working app and report errors on stderr. App logs
   also stream there. Do not start another session for each edit.
4. Keep data in volumes, not just Wasm memory. The root initializer seeds fresh
   roots only; editing it does not change a live database. Use the volume editor
   or documented `pb` data operations for live schema changes.
5. Exercise the requested behavior in the GUI, including persistence after
   reopening. Inspect the actual window as well as logs/build output. `pb frame`,
   `pb input`, `pb screenshot`, and `pb logs` address the known session instance;
   their command help describes the supported arguments.

Stopping the watcher retains the session and data. Resume development from the
same source with `pb run --session SESSION`; reopen without source watching with
`pb session join SESSION`, which follows guest logs until the app exits or Ctrl-C
detaches. Add `--detach` to return after launch. Report the source directory,
session, demonstrated behavior, and any unverified limitation. Publishing or sharing is a separate
user intention; creating an app does not require a deployment.

## Data vocabulary

- A **volume** owns named databases/files, permissions, and durable data. Its
  identity is independent of the app that reads it. Store structured records
  other apps can read, rather than hiding reusable data in an opaque app snapshot.
- The **root volume** is the app's configured starting data. A **session** selects
  a build and retains its root identity. Another app can
  use personal data through explicitly selected volumes.
- **Personal** means a non-root personal volume, not the app's private root.
  Prefer one named `personal`; otherwise use the sole personal volume. Inspect
  identities and roles rather than guessing from a label. Ask when multiple
  candidates remain; offer creation when none exists. Finding a volume does not
  grant the app access to it. Request only the necessary read/write access.
- Local writes are durable locally; connected replicas synchronize through
  Playbit. Render local changes without waiting for a server. React to data and
  operation-ready events instead of polling for freshness.

The current Wasm target is `wasm32-unknown-unknown`, with Playbit's syscall
interface, not a browser or WASI environment. Check dependencies against that
target and its available imports. There are no general host-filesystem APIs:
an app cannot open or watch `~/Library/Messages/` yet. Importing a file through
the supported user/tool workflow does not grant arbitrary host-path access.
