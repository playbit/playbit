# Playbit MCP

`playbit-mcp` is a local stdio MCP server packaged with Playbit.app. It exposes
one tool, `playbit`, for managing apps, sessions and authorized data
through the same local engine as `pb`.

## Connect

Use a complete [Playbit.app build](building.md), then configure your MCP client
to launch its server executable:

| Setting | Value |
| --- | --- |
| Transport | stdio |
| Command | Absolute path to `Playbit.app/Contents/MacOS/playbit-mcp` |
| Arguments | None |
| Environment (optional) | `PLAYBIT_PROFILE=/absolute/path/to/profile` |

For a checkout's debug build, the command is
`/absolute/path/to/archipelago/target/debug/Playbit.app/Contents/MacOS/playbit-mcp`.
The packaged MCP uses the matching engine in `Contents/Helpers/playbit-engine.app`.
Unbundled Cargo/Linux products keep it beside `playbit-engine`. Use the same `PLAYBIT_PROFILE` as
the GUI and CLI to operate on their data; omit it to use the
[default profile](developing-apps.md#sessions-and-profiles).

Initialization and tool discovery do not start the engine. A valid runtime
call starts it when needed; closing MCP leaves the engine running. The server
acts with your local profile's authority, so connect a client you trust.

Provisioned distribution builds expose `saved-account` and
`retry-account-key-backup` through the same typed engine requests as the native UI.
`status` reports `account_key_backup` separately from account readiness, with
`unavailable`, `pending`, `saved`, or `failed`. `saved-account` returns optional
email and account-key fields without signing in. Eligible native forms request this
once to fill untouched fields; opening a retained local account needs no Keychain lookup.

## Call the tool

An MCP client normally handles initialization and JSON-RPC framing. For a raw
stdio client, send one UTF-8 JSON object per line, for example:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"example","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"playbit","arguments":{"op":"status"}}}
```

Successful tool results contain JSON encoded in `result.content[0].text`.
Operation errors set `result.isError`; protocol errors use JSON-RPC `error`.
Diagnostics go to stderr. Input is limited to 4 MiB per line, excluding LF or
CRLF. Oversized or invalid UTF-8 input closes the connection.

The following examples are **tool arguments**, not complete JSON-RPC messages.
Build your app externally with `pb build`, or compile and then use `pb bundle`.
Use absolute bundle paths and replace the placeholder IDs with returned values:

```json
{"op":"authenticate-package","bundle":"/absolute/path/My TODO.pbapp"}
{"op":"publish-app","bundle":"/absolute/path/My TODO.pbapp","notes":"Initial publication"}
{"op":"create-app-session","app":{"publisher":"PUBLISHER_ID","app_id":"my-todo"}}
{"op":"prepare-session","session":"SESSION_ID"}
{"op":"session-reference","session":"SESSION_ID"}
```

App identity consists of the certified publisher's base58-encoded 16-byte account
ID and the effective app ID. Volumes use base58-encoded 16-byte IDs. Human-facing
CLI/MCP session values are 22-character base58 codes encoding a retained 16-byte
seed; internal session IDs and build IDs are base58-encoded 32-byte values. `pb build` and `pb bundle` sign source-built
packages through the local profile's certified signer. First-time signing needs
an authenticated connection; a retained valid signer works offline. Imported
signed packages retain their original publisher.

`create-session {bundle,root?,user?}` imports an authenticated package as an exact
selection. `create-build-session {build,root?,user?}` selects an exact build;
`create-app-session {app,root?,user?}` follows the app's published code. Omitted
bindings select the six lazy app scopes. Explicit Root and User overrides must
already exist and be authorized. Listing scopes with `automatic-volumes {session}`
does not allocate their backing storage.

New World roots use AP database replication. Fresh app sessions with an explicit
Shared (`session`) initializer and the default World binding also synchronize
their separate Shared scope after its first use. An empty declaration qualifies;
omitting it keeps the existing World-backed session behavior.
`invitation {session}` returns the capability for joining the session's retained
backing. `session-reference` remains a launch reference; code publication alone
grants no access to private data. Explicit Root overrides retain the selected
volume's policy and do not opt into synchronized Shared. User and Local scopes,
their session variants, and existing local-only data stay on their original
device. Ordinary sessions retain their supported invitations and synchronization. See
[automatic scopes](root-initialization.md) for the boundary.

`prepare-session` prepares metadata and asynchronous bootstrap; it does not
execute a guest. Open the session with `pb session join SESSION --detach` to
return after launch; omit `--detach` to follow guest logs in the terminal.
GUI inspection, input and logs go through `pb frame`, `pb input` and `pb logs`; pass
`--instance ID` when multiple instances are running. Find IDs with
`pb session instances SESSION`.

Use `update-session {session,bundle}` to advance an exact development session
owned by its creator. App followers advance through publication; unrelated exact
sessions stay pinned. Code updates retain all session scope bindings and never
reapply seed data. Personal-volume mounts require explicit selection and persist
across build updates.

Other common arguments:

```json
{"op":"list-apps"}
{"op":"set-app-enabled","app":{"publisher":"PUBLISHER_ID","app_id":"my-todo"},"enabled":false}
{"op":"delete-app","app":{"publisher":"PUBLISHER_ID","app_id":"my-todo"}}
{"op":"transfer-status"}
{"op":"wait-changes"}
```

Pass an app page's `next` value as `after` to read the next page. Only the
publisher may publish, enable, disable or delete that app. Disable it before
deleting it; neither operation deletes data or ends retained sessions. Public app
code notifications use AP push without registering the local session or sharing
its data. `wait-changes` returns a cursor; pass it as `after` on the next call to
wait without polling. Inspect current state after waking, including
`transfer-status` for bootstrap failures.

## Data operations

Open an explicit tool scope with `tool-context-open`, submit typed `syscall`
operations, and release it with `tool-context-close`. A scope is
`{"kind":"account"}`, `{"kind":"session","binding":"SESSION_ID"}` or
`{"kind":"volume","binding":"VOLUME_ID"}`. Retain the exact returned context
and keep the same MCP server process alive across these calls: the context is
bound to its process owner and cannot be reused after restarting the server.
Operation IDs must increase within that context. Set `mutation` to zero for a
read, or to the operation ID for a mutation.

The `call` payload uses the shared typed data contract, including a 16-byte
integer array for its volume. Refer to the current
[data types](../playbit/src/runtime/host/data.rs),
[context types](../playbit/src/engine/service_call.rs) and
[tool schema](../playbit/src/bin/playbit-mcp/main.rs) when constructing calls.
Unreleased interfaces may change with the installed build. Supported published
interfaces follow the [compatibility contract](compatibility.md).

An interrupted response does not mean a mutation failed. Inspect state before
explicitly retrying; never automatically replay an uncertain mutation.
