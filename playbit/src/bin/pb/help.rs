use std::fmt::Write;

struct Command {
    usage: &'static str,
    description: &'static str,
    command: &'static [&'static str],
    core: bool,
}

impl Command {
    fn add_to_output(&self, output: &mut String) {
        writeln!(output, "{}\n  {}\n", self.usage, self.description)
            .expect("Help command formatting should not be able to fail");
    }
}

const COMMANDS: &[Command] = &[
    Command {
        usage: "pb bloblog create VOLUME NAME",
        description: "Create a named append-only log. VOLUME is a base58 16-byte ID; the returned entry ID is the base58 15-byte LOG.",
        command: &["bloblog", "create"],
        core: false,
    },
    Command {
        usage: "pb bloblog put VOLUME LOG APPEND_ID TEXT [TEXT ...]",
        description: "Append text blobs and wait for remote durability. APPEND_ID is a caller-provided base58 16-byte ID; repeat the same ID and payload after an uncertain result.",
        command: &["bloblog", "put"],
        core: false,
    },
    Command {
        usage: "pb bloblog put-file VOLUME LOG APPEND_ID FILE [FILE ...]",
        description: "Append binary file contents; - reads stdin. Each blob is limited to 32 KiB. Waits for remote durability; retain APPEND_ID for explicit retry.",
        command: &["bloblog", "put-file"],
        core: false,
    },
    Command {
        usage: "pb bloblog get VOLUME LOG [CURSOR]",
        description: "Print one bounded JSON page with blob bytes as arrays. Follow cursor while more=true.",
        command: &["bloblog", "get"],
        core: false,
    },
    Command {
        usage: "pb bloblog watch VOLUME LOG [CURSOR]",
        description: "Print JSON pages as entries arrive, waiting for engine change notifications without polling. Ctrl-C stops watching.",
        command: &["bloblog", "watch"],
        core: false,
    },
    Command {
        usage: "pb schema-gen SCHEMA.json...",
        description: "Generate experimental docdb Rust bindings beside each JSON schema, replacing the .json extension with .rs. Stops on the first error. Does not open a profile or start the engine.",
        command: &["schema-gen"],
        core: false,
    },
    Command {
        usage: "pb hash-file PATH",
        description: "Print a regular file's BLAKE3 address for blob URLs, SHA-256 for download manifests, and byte length as JSON. Files are limited to 256 MiB. Does not open a profile or start the engine.",
        command: &["hash-file"],
        core: false,
    },
    Command {
        usage: "pb trace status [SESSION-INSTANCE-ID]",
        description: "Show the running profile owner's acknowledged recording state. Does not launch an engine.",
        command: &["trace", "status"],
        core: false,
    },
    Command {
        usage: "pb trace start [SESSION-INSTANCE-ID] [--clean]",
        description: "Start recording across the running local profile or for one instance, appending to retained data by default. --clean asks the owner to delete all profile recordings, or only the selected instance's recordings, before starting. Reports unavailable sources and setup failures.",
        command: &["trace", "start"],
        core: false,
    },
    Command {
        usage: "pb trace stop [SESSION-INSTANCE-ID] [--clean]",
        description: "Stop recording while retaining captured data by default. --clean asks the owner to delete all profile recordings, or only the selected instance's recordings, after stopping. Open or missing work remains incomplete; unflushed tails may be missing.",
        command: &["trace", "stop"],
        core: false,
    },
    Command {
        usage: "pb trace tail [SESSION-INSTANCE-ID] [--since TIMESTAMP]",
        description: "Print the most recent 10,000 retained events, then follow new events, with metadata, losses and recording boundaries. Recordings live in <profile>/traces/{global,INSTANCE_ID}; oldest groups are evicted globally when their aggregate size reaches 20% of current free disk space. --since selects retained and future events strictly newer than the unsigned decimal timestamp. Timestamps increase only per thread; out-of-order buffer arrival makes this a filter, not a lossless resume checkpoint. Stops successfully when recording stops; Ctrl-C stops reading.",
        command: &["trace", "tail"],
        core: false,
    },
    Command {
        usage: "pb init [PATH] [--name NAME] [--app-id ID] [--lang rust|ocaml]",
        description: "Create a Rust or OCaml project in a directory empty apart from an optional .git entry, or initialize an existing Playbit source project for the signed-in publisher. Git setup uses an existing repository in the project or a parent directory. Existing source files and data are retained. App IDs default from the directory name; --app-id chooses an exact ID. Interrupted scaffolding resumes from .pb-init.json using its original profile. The default World root derives from the publisher and app ID, with no root identity saved in source.",
        command: &["init"],
        core: true,
    },
    Command {
        usage: "pb run [APP.pbapp] [--release] [--namespace NAME|--dev] [--root-volume ID] [--user-volume ID]\npb run --session SESSION [APP.pbapp]\npb run --build ID [--root-volume ID] [--user-volume ID]\npb run --app PUBLISHER/APP-ID [--root-volume ID] [--user-volume ID]",
        description: "Build, sign and watch the current source, open a signed exact package/build, follow a published app, or resume a retained session. --namespace NAME changes the effective app ID to APP-ID.NAME; --dev means --namespace dev. Namespaces require source. Root/User overrides select existing authorized volumes for a new session and stay retained on resume. --mount ID[:r|rw] selects additional personal data for this local instance. All forms accept repeated --fs PATH[:LABEL[:ro|rw]], --trace and --gas-limit N. Fuel is unlimited by default or with 0; positive unsigned decimal values may use M for one million. Fuel resets each guest turn; exhaustion halts the instance. Positive fuel cannot be enforced for native executables. --sdk=auto|builtin|system and -v/--verbose control source toolchain selection. Failed updates retain working code and data.",
        command: &["run"],
        core: true,
    },
    Command {
        usage: "pb sdk update ocaml|rust",
        description: "Download and select a newer compatible Playbit SDK. Cached SDKs are reused without network checks until explicitly updated. SDK downloads are currently published on the testing channel for arm64 Macs.",
        command: &["sdk", "update"],
        core: false,
    },
    Command {
        usage: "pb sdk-path",
        description: "Print the installed native SDK path as JSON without starting the engine.",
        command: &["sdk-path"],
        core: false,
    },
    Command {
        usage: "pb meta-gen",
        description: "Generate local build metadata from playbit.toml without compiling, launching or changing a volume.",
        command: &["meta-gen"],
        core: false,
    },
    Command {
        usage: "pb build [--release] [--native|--template] [--namespace NAME|--dev] [--sdk=auto|builtin|system] [--verbose]",
        description: "Build and sign a .pbapp, or with --native a Mac .app, using the signed-in publisher. The engine keeps signing keys private. Cached valid certificates support offline builds; first use requires an authenticated publisher certificate. --template creates an unsigned Wasm template without opening a profile or engine; it must be signed before execution. No app is launched.",
        command: &["build"],
        core: true,
    },
    Command {
        usage: "pb bundle [--release] [--native|--template] [--namespace NAME|--dev]",
        description: "Package and sign an existing build artifact without compiling or launching it. Signing completes before replacing the previous output. Native wrappers use the verified publisher/app identity. --template creates an unsigned Wasm template without opening a profile or engine; it must be signed before execution.",
        command: &["bundle"],
        core: true,
    },
    Command {
        usage: "pb publish [APP.pbapp] [--namespace NAME|--dev] [--notes TEXT] [--sdk=auto|builtin|system]",
        description: "Build a signed release of the current source or select an existing signed package, upload missing content, and publish it as this app's current build. App identity is the verified publisher/app-ID pair. An exact package retains its original publisher.",
        command: &["publish"],
        core: true,
    },
    Command {
        usage: "pb app list",
        description: "List the signed-in publisher's apps as bounded JSON pages.",
        command: &["app", "list"],
        core: false,
    },
    Command {
        usage: "pb app enable PUBLISHER/APP-ID",
        description: "Allow new sessions to follow this published app.",
        command: &["app", "enable"],
        core: false,
    },
    Command {
        usage: "pb app disable PUBLISHER/APP-ID",
        description: "Prevent new following sessions; retained sessions keep working.",
        command: &["app", "disable"],
        core: false,
    },
    Command {
        usage: "pb app delete PUBLISHER/APP-ID",
        description: "Delete a disabled app publication without deleting volumes or retained sessions.",
        command: &["app", "delete"],
        core: false,
    },
    Command {
        usage: "pb session create APP.pbapp [--root-volume ID] [--user-volume ID]",
        description: "Create a retained session from a verified package without launching it. Its default World root derives from the package publisher/app ID. New sessions get distinct Shared, User-session and Local-session scopes. Overrides select existing authorized data and remain bound on resume. Print session metadata as JSON; use pb session join SESSION to open it.",
        command: &["session", "create"],
        core: false,
    },
    Command {
        usage: "pb session list",
        description: "List retained sessions with their links. Every SESSION or LINK argument accepts the selected environment's https://playbit.app/s#CODE or https://testing.playbit.app/s#CODE link, playbit:session:public:CODE or playbit:session:testing:CODE, session:CODE, or the bare fixed 22-character join CODE. Qualified links must match the selected Environment.",
        command: &["session", "list"],
        core: false,
    },
    Command {
        usage: "pb session volumes SESSION",
        description: "Show the session's automatic volume slots and their current lazy/access states. Listing a slot does not initialize it. Additional selected personal data belongs to the local instance.",
        command: &["session", "volumes"],
        core: false,
    },
    Command {
        usage: "pb session delete SESSION",
        description: "Remove a session from this device's retained list. Running instances, volumes, saved app data and other participants' access are preserved. Example: pb session delete https://playbit.app/s#CODE. Reopen its invitation to restore it.",
        command: &["session", "delete"],
        core: false,
    },
    Command {
        usage: "pb session join SESSION [--new-instance] [--background] [--detach]",
        description: "Open a session by its complete 22-character join code or invitation link, joining an unknown invitation first. Print the launch result as JSON on stdout, then stream that instance's retained and new guest logs to stderr until it exits. Ctrl-C detaches without quitting the app. --detach returns after launch without following logs. --new-instance creates another independent instance in the same session. --background suppresses activation and still follows logs unless --detach is also supplied.",
        command: &["session", "join"],
        core: false,
    },
    Command {
        usage: "pb session instances SESSION",
        description: "List recorded session-instance identities and their verified process owners.",
        command: &["session", "instances"],
        core: false,
    },
    Command {
        usage: "pb join SESSION [--new-instance] [--background] [--detach]",
        description: "Alias for pb session join, accepting the same session join codes, invitation links and flags. Streams guest logs to stderr after printing launch JSON on stdout; Ctrl-C detaches without quitting the app. --new-instance creates another independent instance; --background suppresses activation; --detach returns after launch without following logs.",
        command: &["join"],
        core: true,
    },
    Command {
        usage: "pb volume create [--no-shared-key] [LABEL]",
        description: "Create a volume (Untitled by default). Share its database key with AP unless --no-shared-key is given.",
        command: &["volume", "create"],
        core: false,
    },
    Command {
        usage: "pb volume list",
        description: "List locally available volumes.",
        command: &["volume", "list"],
        core: false,
    },
    Command {
        usage: "pb volume label ID LABEL",
        description: "Rename a volume for all participants. Only its owner may rename it; offline changes synchronize on reconnect.",
        command: &["volume", "label"],
        core: false,
    },
    Command {
        usage: "pb volume grant ID EMAIL r|rw",
        description: "Grant the recipient read or read/write access through AP.",
        command: &["volume", "grant"],
        core: false,
    },
    Command {
        usage: "pb invite SESSION [--copy]",
        description: "Print the invitation result; --copy also copies its URL on macOS.",
        command: &["invite"],
        core: true,
    },
    Command {
        usage: "pb stop SESSION [--instance ID]",
        description: "Ask the selected app to Quit through its lifecycle. Admission does not assert accepted work has drained; failures remain visible in the app.",
        command: &["stop"],
        core: false,
    },
    Command {
        usage: "pb frame SESSION [--instance ID]",
        description: "Inspect the selected GUI owner's last validated scene without executing a guest turn.",
        command: &["frame"],
        core: false,
    },
    Command {
        usage: "pb screenshot SESSION FILE.png [--instance ID]",
        description: "Capture the selected GUI owner's rendered window as a PNG. Writes the file after a successful capture. Multiple running instances require --instance ID.",
        command: &["screenshot"],
        core: false,
    },
    Command {
        usage: "pb logs SESSION [--instance ID]",
        description: "Read retained logs from the selected GUI owner. Multiple running instances require --instance ID.",
        command: &["logs"],
        core: false,
    },
    Command {
        usage: "pb status",
        description: "Read local runtime/account status.",
        command: &["status"],
        core: false,
    },
    Command {
        usage: "pb version\npb --version",
        description: "Print the installed release and source revision without starting the engine.",
        command: &["version"],
        core: true,
    },
    Command {
        usage: "pb help [COMMAND [SUBCOMMAND]\npb --help\npb [COMMAND [SUBCOMMAND]] --help",
        description: "Print help message with either a short-list of useful commands or the information pertaining to a specific command.",
        command: &["help"],
        core: true,
    },
    Command {
        usage: "pb help-all\npb --help-all\n",
        description: "Print help message with complate list of commands.",
        command: &["help-all"],
        core: true,
    },
    Command {
        usage: "pb file-write VOLUME ENTRY PATH",
        description: "Diagnostic: replace a named blob from a local path.",
        command: &["file-write"],
        core: false,
    },
    Command {
        usage: "pb file-read VOLUME ENTRY PATH [SESSION]",
        description: "Diagnostic: read a named blob to a local path.",
        command: &["file-read"],
        core: false,
    },
    Command {
        usage: "pb cache-budget BYTES",
        description: "Diagnostic: set the local blob-cache budget.",
        command: &["cache-budget"],
        core: false,
    },
    Command {
        usage: "pb usage VOLUME",
        description: "Diagnostic: inspect volume usage.",
        command: &["usage"],
        core: false,
    },
    Command {
        usage: "pb request-code EMAIL",
        description: "Request a sign-in code by email. Privacy policy: https://playbit.app/help/privacy",
        command: &["request-code"],
        core: false,
    },
    Command {
        usage: "pb accept-terms --revision 1",
        description: "Accept the Playbit User Agreement for the signed-in account. Read https://playbit.app/help/user-agreement/1/ before accepting.",
        command: &["accept-terms"],
        core: true,
    },
    Command {
        usage: "pb saved-account",
        description: "Read optional saved email and account key without signing in.",
        command: &["saved-account"],
        core: false,
    },
    Command {
        usage: "pb retry-account-key-backup",
        description: "Retry saving the confirmed account key.",
        command: &["retry-account-key-backup"],
        core: false,
    },
    Command {
        usage: "pb sign-in CODE",
        description: "Sign in with the six-digit email code. If prompted, review the agreement and use pb accept-terms --revision 1 to agree.",
        command: &["sign-in"],
        core: true,
    },
    Command {
        usage: "pb sync",
        description: "Wait for synchronization. The service must confirm acceptance of the current user agreement first. A pending acceptance is submitted before syncing.",
        command: &["sync"],
        core: false,
    },
    Command {
        usage: "pb disconnect",
        description: "Diagnostic: disconnect the account transport.",
        command: &["disconnect"],
        core: false,
    },
    Command {
        usage: "pb offline on|off",
        description: "Remember Work Offline for this profile. Local data remains available; turning it off resumes automatic connection.",
        command: &["offline"],
        core: true,
    },
    Command {
        usage: "pb provision VOLUME BUILD",
        description: "Diagnostic: provision a volume for an exact build.",
        command: &["provision"],
        core: false,
    },
    Command {
        usage: "pb permissions VOLUME JSON_FILE",
        description: "Diagnostic: set volume permissions from a JSON file.",
        command: &["permissions"],
        core: false,
    },
    Command {
        usage: "pb revoke VOLUME ACCOUNT REASON",
        description: "Diagnostic: revoke a volume participant.",
        command: &["revoke"],
        core: false,
    },
    Command {
        usage: "pb schema VOLUME DATABASE JSON_FILE",
        description: "Diagnostic: add declarations to an object database schema.",
        command: &["schema"],
        core: false,
    },
    Command {
        usage: "pb object-create VOLUME DATABASE TYPE JSON_FILE",
        description: "Diagnostic: create an object from JSON fields.",
        command: &["object-create"],
        core: false,
    },
    Command {
        usage: "pb objects VOLUME DATABASE TYPE [AFTER]",
        description: "Diagnostic: read a bounded object page, with its next cursor.",
        command: &["objects"],
        core: false,
    },
    Command {
        usage: "pb input SESSION click TARGET [--instance ID]\npb input SESSION submit TARGET TEXT [--instance ID]\npb input SESSION key [TARGET] KEY down|up [--instance ID]\npb input SESSION pointer TARGET move|down|up|cancel X Y [none|primary] [--instance ID]",
        description: "Admit one input to the selected GUI owner. Use a Playbit key name for window input. An explicit canvas target accepts left, right, up, down, or space. Pointer X and Y are logical canvas-local coordinates; a down defaults to primary, other phases to none. A move with primary drags. A lost reply never repeats input.",
        command: &["input"],
        core: false,
    },
];

pub(super) fn render(args: &[String]) -> Option<String> {
    // I would like to do this in the function which calls this but
    // that is more viral than I'm will to do in this exact moment
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let args = args.as_slice();

    let mut output = String::new();

    match args {
        [] | ["help"] | ["--help"] => {
            COMMANDS.iter().filter(|c| c.core).for_each(|c| c.add_to_output(&mut output));
            output.push_str("Run again with --help-all for a complete list\n");
        }

        ["help-all"] | ["--help-all"] => {
            COMMANDS.iter().for_each(|c| c.add_to_output(&mut output));
        }

        ["help", command @ ..]
        | ["--help", command @ ..]
        | [command @ .., "help"]
        | [command @ .., "--help"] => {
            let matched = COMMANDS
                .iter()
                .filter(|c| c.command.starts_with(command))
                .map(|c| c.add_to_output(&mut output))
                .count();

            if matched == 0 {
                eprintln!("unknown help topic '{}'; run pb help", command.join(" "));
                std::process::exit(1);
            }
        }

        _ => return None,
    };

    Some(output)
}
