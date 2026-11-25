# Developing Playbit apps

New World roots use AP database replication. Fresh native sessions that declare
a Shared initializer and use the default World address also synchronize their
Shared scope after first use. An explicit empty `session` declaration qualifies;
omitting that declaration keeps the existing World-backed session behavior.
User, User-session, Local and Local-session remain on their original device;
existing local-only data is not promoted or reseeded. Browser execution of these
new app sessions remains deferred. App publication shares signed code and
advances its current build; it grants no access to private data.

Use a complete Playbit.app installation, including its SDK and tools. To build
one from this checkout, follow [Building and testing](building.md). From the
repository root, package an optimized app and create a project:

```sh
sh playbit/platform/macos/package.sh release
export PATH="$PWD/target/release/Playbit.app/Contents/MacOS:$PATH"
pb init /tmp/my-playbit-app --name "My TODO"
cd /tmp/my-playbit-app
pb build
pb run
```

Opening Playbit.app registers its bundled `playbit` agent skill at
`~/.agents/skills/playbit` and, when `~/.claude` exists, `~/.claude/skills/playbit`.
These are symlinks to `Contents/Resources/skills/playbit` in that installation.
An existing user-owned skill directory or unrelated link is preserved and the
conflict is reported on stderr. The bundled references and SDK describe the
installed capabilities and take precedence over newer website documentation.

`pb build` writes `target/debug/My TODO.pbapp`; `--release` writes
`target/release/My TODO.pbapp`. Rust builds use Cargo's configured target directory.
Playbit uses compatible local tools or downloads a managed SDK automatically.
Init defaults to Rust with `src/app.rs`; `--lang ocaml` creates `src/app.ml`.
The optional path defaults to the current directory; missing directories are
created, including parents. The app name defaults to the resolved directory's
last component, including when the path is `.` or omitted. Use `--name` to override it.
The `app_id` also defaults from that directory component, independently of the
display name: ASCII letters are lowercased, unsupported characters are stripped,
and leading, trailing, or repeated dots and hyphens are removed. For example,
`My TODO` becomes `mytodo`. Use `--app-id ID` to set an exact identifier. It must
contain 1–64 lowercase ASCII letters or digits in nonempty components separated
by dots or hyphens; an unusable derived identifier requires an explicit override.
Rust's generated `Cargo.toml` points to the installed API copied into
`target/playbit/sdk/playbit-api`. Builds use the matching installed API without
a developer checkout, including after moving Playbit.app.
Init resolves your publisher's World root from the app ID without opening data. Source files contain no selected volume IDs. Other automatic
scopes initialize lazily when an admitted data or File operation first uses them.
The CLI starts the installed local engine using its readiness
handshake. JSON results go to stdout; build output and progress go to stderr.
Use `pb help`, `pb init --help` or `pb publish --help` to explore the
commands without a running engine or source directory. Help is human-readable
stderr; `pb version` and `pb --version` print the release and source revision as
plain text to stdout. Help and version reporting never initialize a project.

## Project files

If scaffolding fails, keep the directory and retry `pb init` with the same
`PLAYBIT_PROFILE`. The temporary `.pb-init.json` records the original app ID,
profile and complete template before creating source files. Retries preserve that
intent, create missing files and report conflicts without overwriting edits.
The intent disappears after app metadata and Git setup succeed. An
existing Playbit source directory can also be initialized for another publisher;
its source and existing data are preserved.

The Rust starter includes the Little List source in `src/`, its corkboard artwork
in `icon.png`, and the project files `Cargo.toml`, `playbit.toml`,
`playbit-volumes.json`, `.gitignore` and `.cargo/config.toml`. Its only app
dependency is `playbit-api`.

`playbit.toml` declares the app ID, build commands, bundled resources, required
host features and optional trace names. See the [manifest reference](playbit-toml.md).

`playbit-volumes.json` is a closed map of
`root`, `session`, `user`, `user-session`, `local` and `local-session` declarations.
Each has the existing permissions, database/schema, seed-object and blob fields,
bounded to 64 KiB and 64 seed objects per role. Omitted roles are empty. Omitted
World defaults to Public Read; other omitted roles are private. Private and
Shared declarations cannot choose readers or writers: the host sets their scope.

Database and blob declarations inherit their role permissions; per-entry ACLs
can be edited explicitly after initialization. Initializer fields outside the
closed declaration format are rejected.

The publishing account and effective app ID identify an app across builds and
sessions. `pb build --namespace testing` signs `pinball.testing` when the source
ID is `pinball`; `--dev` is exactly `--namespace dev`. These invocation options do
not edit source. A literal `pinball.dev` ID has the same identity as `pinball`
built with `--dev`. Copies of a signed package preserve their original publisher.
Build/signing requires a valid publisher certificate. Once cached, it works
offline until expiry; unavailable or revoked authority fails explicitly.

Use the GUI volume editor or `pb` to edit live data. The default Rust starter,
Little List, keeps tasks in World (`root`). By default, sessions with the same
publisher and effective app ID reuse those tasks on this profile. Updating,
building or reopening never replays seed objects or overwrites live data. Schema
changes remain explicit data operations in this increment.

`pb init` generates editor metadata. `pb build` and `pb run` refresh it before
compiling. `pb meta-gen` only prepares ignored files under `target/playbit/`, with
no engine request or volume changes. Run it before direct Cargo builds after
manifest edits or `cargo clean`. The starter's Wasm command explicitly builds a
cdylib. See [app trace declarations](app-trace-declarations.md).

## Native Mac apps

The same generated `src/app.rs` also builds as a native Mac app:

```sh
pb build --native --release
open "target/release/My TODO.app"
# Or run its executable with an explicit isolated data profile:
"target/release/My TODO.app/Contents/MacOS/app" /tmp/my-native-profile
```

`pb sdk-path` locates the static SDK installed in Playbit.app. `pb build --native`
prepares native launch constants, compiles the matching release Wasm package,
and then runs Cargo to link the installed launcher and SDK. Generated inputs
live in `target/playbit/`; neither the SDK launcher nor a build script is copied
into the project. Native packaging checks both the trace declarations and the
expected portable package digest, name and trace declarations. The portable package
contains the role declarations authored in `playbit-volumes.json`.
The same portable package is stored at `Contents/Resources/app.pbapp` and distributed to other Playbit devices; the native app executes
its compiled callbacks directly, with no VM, IPC endpoint or helper process.
`PLAYBIT_NATIVE_SDK` can select another matching SDK. No engine Rust library or
implementation checkout is needed after installation. The app runs independently
of the installation and source directory after linking. Native bundles include
the configured name/icon, minimum-OS metadata and licenses, and are ad-hoc signed
for local use. Developer ID signing and notarization are separate distribution
steps. Native output is `target/debug/<Name>.app`, or
`target/release/<Name>.app` with `--release`, under Cargo's configured target
directory. Rebuilding replaces the named product only after the new bundle is
complete and its signature is verified; failed builds retain the previous output.

On first launch, sign in and save or restore your account key. Create a new
session. First use of a new World root enables AP database replication; AP can
read those databases under the existing permission model. A fresh session with
a declared Shared initializer and the default World binding also synchronizes
its separate Shared data after first use. Its invitation lets another authorized
device join that same Shared backing.

A new synchronized Shared volume has one default grant for **Session
participants** with **Read + Write**. A signed-in player who opens the session
invitation can use those permissions in that session. The owner can inspect or
change the grant in the volume's permissions. Entry-specific permissions still
apply. Shared uses ordinary AP database replication and file contents retain
their existing encryption.
Copying a signed `.pbapp` alone creates a separate session and grants no access
to private data. User and Local scopes, their session variants, and existing
local-only data stay on their original device. Ordinary sessions retain their
supported remote invitations.

The default native profile is
`~/.playbit`. Use `PLAYBIT_PROFILE` to select an isolated profile when running alongside
the developer engine profile. Reopening retains its account, session and local
edits, including offline edits. The native launcher requires the session's exact
package build. Rebuilding changed app code requires a separate profile/session;
this host cannot hot-swap native
callbacks for Wasm updates. This target supports arm64 macOS 15+.

## Icons

The starter uses the shared Controls check glyph as its editable icon.
The Mac session sidebar displays the bundle's `icon` resource. Keep PNG/JPEG
artwork within 1 MiB and 1024×1024 pixels, or use simple SVG shapes/gradients
within 16 KiB and 512 XML nodes. SVG text, images, filters and references outside
local gradients are unsupported; export complex artwork as PNG. Previews are
24 points at 2× resolution. Missing or unsupported artwork does not block the
app; unsupported artwork is reported in the sidebar and stderr.
The package retains the original icon. AP metadata uses a generated 120×120 PNG
preview within 64 KiB, independently of the source icon's size.

## Build, run and update

`pb run APP.pbapp` authenticates the self-contained package and creates a new
session. World follows the package's publisher and effective app ID; Shared,
User-session and Local-session are specific to the new session. User and Local
span that app's sessions for the current account/profile.

`pb session create APP.pbapp [--root-volume ID] [--user-volume ID]` creates a
session without launching. It returns JSON including `id`, `root_volume` and
`resolved_build_id`. Use `pb session join ID` to open it. Explicit World or User
overrides must already exist and be authorized; they are retained in the session
and never reseeded. Resume with `pb run --session ID` to preserve all bindings.

An app can read its current session with `ctx.session_info()?` in Rust or
`Session.info ()` in OCaml. The record contains the 32-byte internal ID and,
when the host retains it, the 16-byte invitation key. The Rust `code()` method
and OCaml `Session.code` function encode the key as the 22-character URL fragment.
A guest in the production environment can build a link like this.

```rust
let url = ctx.session_info()?.and_then(|info| info.code())
    .map(|code| format!("https://playbit.app/s#{code}"));
```

Use the website origin of the session's deployment. Reading the key requires no
user action. Anyone given the code can use a published invitation, so keep it
out of logs. The read is local. Opening the link on another device requires
the session to support sharing and its invitation to be available there.

Copied or cloned source uses the same effective identity for its publishing
account. No canonical source path, runner account or source-side volume binding
changes the publisher's World identity. A namespace creates a separate app
identity for development or testing.

`pb run` builds, bundles and opens a new Mac session, then watches its source and
build artifact using native filesystem events. Successful builds update only that
session and its participants. Failed builds retain the working app and report the
error on stderr. Ctrl-C ends watching and retains the session and its data. The
initial result and successful update results are separate JSON lines on stdout.
`pb run --release` chooses
`[release].build_cmd`. `pb build [--release]` runs the same configured command and
returns the signed `.pbapp` after the profile owner signs its staged content. `--native` chooses
`[native].build_cmd` or `[native.release].build_cmd` and returns a Mac `.app`.
Init writes all four commands; edit them to use your build tools:

```toml
build_cmd = "cargo rustc --lib --crate-type cdylib"

[release]
build_cmd = "cargo rustc --release --lib --crate-type cdylib"

[native]
build_cmd = "cargo rustc --bin playbit-app --features native --target aarch64-apple-darwin -- @target/playbit/native.rsp"

[native.release]
build_cmd = "cargo rustc --release --bin playbit-app --features native --target aarch64-apple-darwin -- @target/playbit/native.rsp"
```

Non-Cargo Wasm projects declare both source-relative artifact paths:

```toml
build_cmd = "sh build.sh debug"
build_artifact = "_build/debug/app.wasm"

[release]
build_cmd = "sh build.sh release"
build_artifact = "_build/release/app.wasm"
```

These builds do not invoke Cargo metadata. Artifacts must be `.wasm` files inside
this source directory; a missing or escaping path fails packaging. Products go to
`target/debug/NAME.pbapp` or `target/release/NAME.pbapp`. Keep generated files under
`_build/` or `target/`, and source outside the artifact's generated output directory. Source watching
excludes that directory when it differs from the source root. Publish completed artifacts with an atomic rename.

`pb` generates `target/playbit/native.rsp` with the installed SDK archive path.
Cargo passes these arguments to the final binary while retaining your Cargo
configuration and compiler flags.

`pb bundle [--release] [--native]` packages an already built artifact without
compiling. Both commands print the absolute `bundle` path as JSON; Wasm results
also include `build_id`. Build commands run in the current source directory and
their output goes to stderr. Native packaging currently requires an arm64 Mac
and exactly one Cargo binary target; Wasm packaging requires one library target
compiled as a `cdylib`.

`pb build --template` and `pb bundle --template` create unsigned Wasm templates
without opening a profile, starting an engine or using a publisher account.
They return `bundle` and `template: true`, without a signed `build_id`.
This mode supplies the examples sealed inside the macOS app; the user's engine
signs a verified copy before execution. Templates cannot run or publish until
signed, and `--template` cannot be combined with `--native`.

During a source `pb run`, app logs stream to stderr, including while Cargo is
building an update. JSON results remain on stdout. Failed builds retain the
working app and its log stream. Use `pb session instances SESSION` to find running
app instances, then
`pb logs SESSION --instance ID` to read logs from a specific GUI owner.
Closing the CLI does not quit that app.

Add `--trace` to any `pb run` form to start recording the instance it launches,
equivalent to `pb trace start INSTANCE`. Existing history is retained. Use the
returned instance ID with `pb trace tail INSTANCE` and `pb trace stop INSTANCE`.
Recording continues when source watching ends.

Every `pb run` form runs Wasm without a computation limit by default;
`--gas-limit=0` explicitly selects the same unmetered execution. Add
`--gas-limit=1M` (or `--gas-limit 1000000`) to allow 1,000,000 fuel units per
guest turn. Values are unsigned decimal integers with an optional `M` suffix
meaning decimal million. A positive budget resets before each `pb_main`
invocation. Exhausting it is a fatal trap that halts the instance; execution is
not suspended or resumed. This option belongs only to the launched local
instance, survives its hot reloads, and is neither packaged nor replicated to
other instances. Ordinary Mac and iOS launches are unlimited. The option is
unsupported for native executables, which cannot enforce Wasm fuel limits.

Use `pb run --root-volume ID` or `--user-volume ID` for explicit bindings on a
new session. Run installed content with `pb run --build ID` or
`pb run --app PUBLISHER/APP-ID`. Exact builds stay pinned; app selections follow
the app's published build while retaining their own session and volume bindings.

`pb run --mount VOLUME[:r|rw]` grants an explicit personal-volume mount only to the
new local instance; repeat for more mounts. The default is `rw`. A new instance's
mount list does not replace another instance's choices. Resume source watching
with `pb run --session SESSION`; without new mounts it resumes its retained
instance preferences. `pb session volumes SESSION` reports the six automatic
slots without materializing their backing storage. User override selection is an
owner-only, retained session binding.

## Sessions and profiles

Each Mac session opens in an independent app process, which owns guest execution,
rendering and logs. The shared local engine owns storage, accounts and replication.
`pb session join SESSION` accepts a retained local code or a supported session
invitation. Session codes encode the retained 16-byte seed as 22 base58 characters;
the engine and AP use the derived 32-byte session ID internally. A local code
reopens its existing profile record. For a supported remote invitation, the
command joins it first, then opens the app without recompiling. A session whose
backing is still local or uninitialized has no remotely usable invitation;
copying its launch reference alone does not publish its data.
Joining a running session activates its existing app; `--new-instance` opens
another instance of the same session. `--background` suppresses activation for
both existing and new instances. `pb join` is an alias with the same arguments
and flags. Use `pb stop SESSION --instance ID` to request that app's Quit lifecycle.

Both join commands print the launch result as JSON on stdout, then stream that
instance's retained and new guest logs to stderr until the app exits. Ctrl-C
detaches the CLI without closing the app or deleting its data. `--background`
still follows logs. Add `--detach` to return after launch without following logs,
for example `pb join SESSION --background --detach` in a script.

`pb session list` lists retained sessions with local codes. `pb session delete
SESSION` removes a session from this device's retained list, preserving running
instances and its data. Reopening a retained record restores it to the list.
For published sessions, `pb invite SESSION --copy` copies a supported remote
invitation and `pb session join 'https://playbit.app/s#CODE'` joins its retained
backing. Fresh sessions with a declared Shared initializer and default World
binding carry their Shared capability; supported root-backed sessions carry
their existing Root capability. Invitations do not
share User, User-session, Local or Local-session data.

Shared stores session-wide app data. User-session holds the current account's
private session state; Local-session holds durable state on this profile. World,
User and Local span the app's sessions. World uses AP database replication, and
fresh sessions with a declared Shared initializer and default World binding
synchronize Shared. The four User and Local scopes and existing local-only Shared
data remain on their original device.
Select scopes through the SDK's typed
`VolumeRole`, never through names or tags.

Published sessions support encrypted ephemeral live channels. Sessions whose
replicated backing is unpublished cannot use these channels. For a published session:

```rust
let session = context.session()?;
let drafts = session.channel(context, "drafts")?;
let subscription = drafts.subscribe(context)?;
```

Keep handles in app state across callbacks. Subscription readiness arrives through
DataReady; `context.take_channel(subscription)` returns `Completion::Ready(())`
when subscribed. `drafts.publish(context, bytes)` returns another operation handle;
its completion means AP accepted a live delivery attempt. A subscribed publisher
receives its own messages. Read ChannelMessage with `event.channel_message()` for
the channel handle, sender identity, message ID and bytes.

For a roster-only app, call `context.session()?` once at startup to start or
reuse live membership. Read the cached roster with `context.presence(&mut rows)`.
The first read can be empty while joining. On the first and subsequent
`PresenceChanged` events, reread the roster and use each participant's
`display_name()?` for its label. Opening a named channel is optional.

Names are case-sensitive, unnormalized UTF-8 strings of 1–223 bytes. Each instance
can open 16 names, and messages can contain at most 32 KiB. `unsubscribe` stops
receiving; `close` retires the handle. Retired-generation handles fail explicitly.
Channels provide no history, offline queue or publication replay. Reconnect
restores successful subscriptions and refreshes presence. Disconnected publishes
fail; an interrupted publish can report `Error::Uncertain`. A held session secret
allows channel access without granting any volume permissions.

The desktop app, local engine, CLI and MCP share the default profile at `~/.playbit`.
Cached app artwork lives in `cache/app-displays/`; corrupt blob
copies retained for diagnosis live in `received/corrupt-blobs/` and remain durable
until an explicit profile reset.

All profile-owned resources stay inside it: `profile.kvdb` contains local identity,
credentials, and installation-local UI settings; `account.kvdb` retains encrypted account replication records;
`volumes/` holds volume databases; `blobs/`, `builds/` and `received/` retain durable
work; `staging/` holds temporary replication inputs. Recoverable blobs and native
wrappers live in `cache/blobs/` and `apps/<app_id>.<account-suffix>/<name>.app`.
The wrapper shares its runtime with Playbit and represents all retained sessions
of that publishing account and app ID. Dock opens resume the most recently used
live session, or the most recently used retained session. Diagnostics live in
`logs/engine.log`, the desktop process's `logs/playbit-gui.log`, and `traces/`;
locks and sockets live in `control/`.

On macOS, tracing requires a canonical absolute profile path of at most 75 UTF-8
bytes because its Unix-domain socket names must fit the native address limit.
Choose a shorter profile location when needed. Little List's source runner keeps
its profile under `target/run/profile`, so its checkout path counts toward this limit.

Use `pb trace start INSTANCE` to record an owned running instance,
`pb trace tail INSTANCE` to inspect its independent events, and
`pb trace stop INSTANCE` to stop. Omit `INSTANCE` for the profile. App-defined
metadata is private recording data; the CLI reports loss and missing references
without inventing complete spans. See [event recording](../playbit/trace/README.md).
Pending uploads and rejected local-work archives remain durable across restart.
Move or restore a profile only while stopped; copying it copies its device identity.
If you set `PLAYBIT_PROFILE`, use the same value for those processes; the override
is used as provided. On macOS, CLI launches with an explicit profile create a
separate app process so an already-running profile cannot receive its invitation.

The CLI and MCP start their adjacent packaged engine when needed. Keep tools from
the same installation together: reuse requires matching protocol and build
versions. Quitting the library GUI or MCP does not stop the background engine.
Use a separate `PLAYBIT_PROFILE` for development with incompatible data formats.

## Publish and share

Sign in through Playbit, or use `pb request-code EMAIL` followed by `pb sign-in CODE`. After signing in, read the [Playbit User Agreement](https://playbit.app/help/user-agreement/1/) and run `pb accept-terms --revision 1` to agree.
Playbit starts syncing only after the service confirms the signed-in account’s receipt
for the current agreement. This also applies to background sync when you reopen
a profile. If you accepted offline, Playbit sends that acceptance when you reconnect.
`pb sync` checks the receipt again before transferring data.
Save or recover your account key before publishing. Provisioned distribution builds
pre-populate sign-in fields from an available saved account (`pb saved-account`
performs the lookup without signing in) and report their optional
recovery-copy status separately. Retry a failed copy with `pb retry-account-key-backup`.
Source stays in your editor and Git. Publish a source-built release or an
already signed portable package:

```sh
pb publish --notes "Initial TODO release"
pb app list
pb run --app PUBLISHER/APP-ID
```

Publication derives the app identity from its verified signed bytes. It uploads
the exact build before changing the app's current build. `--namespace NAME` or
`--dev` rebuilds source under that effective ID. Publishing copied bytes preserves
the original author and requires that publisher's authority. Publication shares
code only: another device creates its own local session and automatic data. App
code updates arrive through AP push without registering these local sessions
or exporting their scope credentials or data.

For supported ordinary volumes, `pb volume label VOLUME_ID "Shared name"`
renames a volume for its participants. Only
the owner may rename it. The command commits locally and returns JSON; connected
participants receive the metadata change through AP, and offline changes publish
on reconnect. IDs, data, permissions and existing invitations stay the same.

`pb volume grant VOLUME_ID EMAIL r` sets that existing account's volume grant to
read-only; `rw` sets read/write. Email resolution goes through AP and requires the
volume owner. Other subjects and object overrides are preserved; an existing
public grant still applies. The command does not send mail, transfer credentials,
or select personal volumes for an app. Share a supported session invitation
separately; a permission grant alone does not transfer its capability or publish
unsupported local scopes.
Unknown accounts fail without changing permissions. The successful JSON result
reports the resolved account and access; progress goes to stderr.

Publication notes may contain at most 32 KiB of JSON-escaped text.

`pb app list` streams bounded catalog pages. `pb app disable PUBLISHER/APP-ID`
prevents new app-based runs; `enable` permits them again. Existing sessions,
invitations, exact builds and data remain usable. `pb app delete PUBLISHER/APP-ID`
requires disabling first and permanently removes its entrypoint. It neither
deletes data nor changes existing session bindings.

Builds are immutable BLAKE3-addressed signed bundles. App identity consists of the
certified publisher and effective app ID. Sessions select an exact build or
follow a published app. Failed or blocked updates retain their previous working
build. Supported World, Shared and ordinary volumes synchronize automatically;
`pb sync` is a diagnostic barrier. Local-only scopes remain on their original device.

## Downloadable language toolchains

See [developer SDKs](developer-sdks.md) for `pb init --lang ocaml`, automatic
toolchain selection, `--sdk` controls and explicit SDK updates.

## Bundled files

In `playbit.toml`, top-level `files_dir = "assets"` packages
`assets/sounds/bell.wav` as `sounds/bell.wav`. To choose a destination for a
single file, add `"help/start.txt" = "docs/getting-started.txt"` under `[files]`.
You can use both forms in the same project. See the
[resource syntax, path rules and limits](playbit-toml.md#bundled-resources).

Find the volume with backing `VOLUME_BUNDLE` (`0x300`) through `volume.list`,
then open, list and read it with ordinary asynchronous File operations. This
volume is read-only and has no persistent database or KV addressing. `app/`
contains signed identity metadata, the portable executable, role declarations and optional
icon and trace declarations. See [the format specification](pbapp.md).
