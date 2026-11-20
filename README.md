# Playbit Archipelago

Playbit is a personal software environment where apps are documents you can
open, copy, modify and share. Apps use persistent local data and synchronize
across devices through Playbit's services.

This repository contains the client, runtime, Rust app SDK and example apps.
The main development target is Apple silicon running macOS 15 or newer; iOS,
web and native embedding code live under `playbit/platform/`.

## Build

Install Rust, Xcode with its command-line tools, Python 3, and Node.js 26+ with
npm. From the repository root:

```sh
xcodebuild -downloadComponent MetalToolchain
rustup target add wasm32-unknown-unknown
./build.sh
open target/debug/Playbit.app
```

This builds a local debug kit, including the Mac app, CLI/MCP tools, example
bundles and web client. No Apple signing account or service credentials are
needed to build. See [Building and testing](docs/building.md) for outputs,
tests and other build targets.

## Documentation

- [Developing apps](docs/developing-apps.md): create, run, package and share an app with `pb`.
- [Playbit MCP](docs/mcp.md): connect an MCP client and operate Playbit.
- [Runtime contributor map](playbit/src/runtime/README.md): implementation and test boundaries.
- [Native embedding](playbit/platform/native/README.md): use the engine and native SDK in a host app.

Code starts in [`playbit/`](playbit/) (client and engine),
[`playbit-api/`](playbit-api/) (app SDK), and [`guests/`](guests/) (examples).
Read [AGENTS.md](AGENTS.md) before contributing.

## Source and license

Client source is licensed under [Business Source License 1.1](LICENSE), with
exceptions listed there. This repository contains the client, app SDKs and
examples from the tagged release. The root workspace builds independently.
