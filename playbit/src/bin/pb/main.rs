use playbit::engine::{Request, Result, ipc, reply::Reply};
mod bloblog;
mod control;
mod data;
mod developer;
mod digest;
mod files;
mod help;
#[cfg(target_os = "macos")]
mod logs;
mod native;
mod project;
mod schema;
mod sdk;
thread_local! { static SDK_CHANNEL: std::cell::Cell<playbit::release_channel::ReleaseChannel> = const { std::cell::Cell::new(playbit::release_channel::ReleaseChannel::Public) }; }
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn sdk_channel() -> playbit::release_channel::ReleaseChannel {
    SDK_CHANNEL.with(std::cell::Cell::get)
}
mod trace;
mod volume;
#[cfg(target_os = "macos")]
mod watch;
use playbit::store::Id;
use std::fs;
#[cfg(target_os = "macos")]
use std::{
    io::Write,
    process::{Command, Stdio},
};
fn main() {
    if let Err(error) = run() {
        playbit::diagnostic!(
            "pb: error: {}",
            playbit::engine::failure::cli_message(error.as_ref())
        );
        eprintln!(
            "Playbit v{} ({}-{})",
            playbit::VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        eprintln!(
            "If you think this is a bug, please let us know by emailing the output of the command to support@playbit.app"
        );
        std::process::exit(1);
    }
}
fn argument(args: &[String], n: usize) -> Result<&str> {
    args.get(n).map(String::as_str).ok_or_else(|| "missing argument; run pb help".into())
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(text) = help::render(&args) {
        eprint!("{text}");
        return Ok(());
    }
    let command = args[0].as_str();
    #[cfg(target_os = "macos")]
    if command == "engine-service" {
        let start = match args.get(1).map(String::as_str) {
            Some("start") if args.len() == 2 => true,
            Some("unregister") if args.len() == 2 => false,
            _ => return Err("expected pb engine-service start or unregister".into()),
        };
        return playbit::engine::launch_agent::control(start);
    }
    if matches!(command, "version" | "--version") {
        if args.len() != 1 {
            return Err("expected pb version or pb --version".into());
        }
        println!("{}", playbit::VERSION);
        return Ok(());
    }
    if command == "hash-file" {
        if args.len() != 2 {
            return Err("expected pb hash-file PATH".into());
        }
        println!("{}", serde_json::to_string(&digest::file(std::path::Path::new(&args[1]))?)?);
        return Ok(());
    }
    if command == "schema-gen" {
        return schema::generate(&args[1..]);
    }
    if matches!(command, "build" | "bundle")
        && args[1..].iter().any(|arg| arg.split('=').next() == Some("--template"))
    {
        // Built-in templates are sealed by their containing app, then signed
        // by the user's engine at installation. Assembly needs no profile.
        let call = |_| Err("Template packaging cannot use the engine".into());
        let directory = std::env::current_dir()?;
        let value = if command == "build" {
            developer::build_command(&args[1..], &directory, &call)?
        } else {
            developer::bundle_command(&args[1..], &directory, &call)?
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let _installation = playbit::engine::installation::client_lease(&std::env::current_exe()?)?;
    let (profile, channel) = playbit::engine::profile::resolve_environment()?;
    if command == "trace" {
        playbit::engine::profile::initialize_selected(&profile, channel)?;
        if args.get(1).is_some_and(|command| command == "tail") {
            return trace::tail(&profile, &args[2..]);
        }
        let command = match args.get(1).map(String::as_str) {
            Some("start") => playbit::trace::control::Command::Start,
            Some("stop") => playbit::trace::control::Command::Stop,
            Some("status") => playbit::trace::control::Command::Observe,
            _ => return Err("expected pb trace start|stop [SESSION-INSTANCE-ID] [--clean], pb trace status [SESSION-INSTANCE-ID], or pb trace tail [SESSION-INSTANCE-ID] [--since TIMESTAMP]".into()),
        };
        let (selector, clean) = trace::control_arguments(command, &args[2..])?;
        trace::control(&profile, command, selector, clean)?;
        return Ok(());
    }
    if let Err(error) = playbit::trace::initialize(3, &[]) {
        playbit::diagnostic!("Playbit tracing unavailable: {error}");
    }
    let started = std::cell::Cell::new(false);
    let call = |request: Request| {
        if !started.get() {
            playbit::engine::profile::initialize_selected(&profile, channel)?;
            playbit::platform::failure::install_native(&profile);
            playbit::platform::failure::startup(&profile);
            let executable = std::env::current_exe()?.canonicalize()?;
            let engine = playbit::engine::launch::executable_for(&executable)?;
            playbit::engine::launch::ensure(&profile, &engine)?;
            if let Err(error) = playbit::trace::attach(&profile) {
                playbit::diagnostic!("Playbit trace attachment unavailable: {error}");
            }
            started.set(true);
        }
        ipc::call_archive(&profile, &request)?.into_owned()
    };
    // Personal deployments use the existing signed Testing SDK catalog.
    let sdk_channel = if channel.is_personal() {
        playbit::release_channel::ReleaseChannel::Testing
    } else {
        channel
    };
    SDK_CHANNEL.with(|selected| selected.set(sdk_channel));
    let output =
        |request| -> Result<serde_json::Value> { human_reply(&call(request)?, channel, &call) };
    let value = match command {
        "meta-gen" => {
            if args.len() != 1 {
                return Err("expected pb meta-gen".into());
            }
            let directory = std::env::current_dir()?;
            let manifest = playbit::package::Manifest::read(&directory)?;
            let _api = project::prepare_sdk(&directory, &std::env::current_exe()?)?;
            let output = playbit::package::metadata::generate(&directory, &manifest)?;
            serde_json::json!({"directory":output})
        }
        "sdk" => sdk::update(&args[1..])?,
        "sdk-path" => {
            if args.len() != 1 {
                return Err("expected pb sdk-path".into());
            }
            serde_json::json!({"path": native::sdk_path(&std::env::current_exe()?.canonicalize()?)?})
        }
        "init" => developer::init(
            &args[1..],
            &std::env::current_dir()?,
            &std::env::current_exe()?,
            &profile,
            &call,
        )?,
        "build" => developer::build_command(&args[1..], &std::env::current_dir()?, &call)?,
        "bundle" => developer::bundle_command(&args[1..], &std::env::current_dir()?, &call)?,
        "run" => {
            developer::run(&args[1..], &std::env::current_dir()?, &profile, &call, channel)?;
            return Ok(());
        }
        "publish" => serde_json::to_value(developer::publish(&args[1..], &std::env::current_dir()?, &call)?)?,
        "app" => match argument(&args, 1)? {
            "list" if args.len() == 2 => {
                developer::list_apps(&call, &mut std::io::stdout().lock())?;
                return Ok(());
            }
            "enable" | "disable" | "delete" => serde_json::to_value(developer::app_lifecycle(&args[1..], &call)?)?,
            _ => return Err("expected pb app list, enable, disable or delete".into()),
        },
        "session" => match argument(&args, 1)? {
            "create" => {
                output(developer::create_session(&args[2..], &std::env::current_dir()?)?)?
            }
            "volumes" if args.len() == 3 => {
                output(Request::AutomaticVolumes { session: session_argument(&args[2], channel)? })?
            }
            "instances" if args.len() == 3 => {
                output(Request::InstanceList { session: session_argument(&args[2], channel)? })?
            }
            "list" if args.len() == 2 => {
                let Reply::Status(status) = call(Request::Status)? else {
                    return Err("unexpected status reply".into());
                };
                let mut sessions = Vec::with_capacity(status.sessions.len());
                for session in status.sessions {
                    let reference = playbit::engine::session_presentation::reference_from_reply(
                        call(Request::SessionReference { session: session.id.clone() })?,
                        channel,
                    )?;
                    sessions.push(playbit::engine::session_presentation::session_value(
                        &session, &reference,
                    )?);
                }
                serde_json::to_value(sessions)?
            }
            "delete" => {
                if args.len() != 3 {
                    return Err("expected pb session delete SESSION".into());
                }
                output(Request::ForgetSession { session: session_argument(&args[2], channel)? })?
            }
            "join" => return join(&args[2..], &profile, &call, channel),
            _ => {
                return Err(
                    "expected pb session create APP.pbapp [--root-volume ID] [--user-volume ID], list, delete SESSION, or join SESSION [--new-instance] [--background] [--detach]"
                        .into(),
                );
            }
        },
        "volume" => match argument(&args, 1)? {
            "create" => output(volume::create(&args[2..])?)?,
            "list" if args.len() == 2 => {
                let Reply::Status(status) = call(Request::Status)? else {
                    return Err("unexpected status reply".into());
                };
                serde_json::to_value(status.volumes)?
            }
            "label" if args.len() == 4 => {
                let volume = Id::parse(&args[2])?;
                playbit::store::validate_volume_label(&args[3])?;
                playbit::diagnostic!("Renaming volume…");
                output(Request::LabelVolume { volume, label: args[3].clone() })?
            }
            "grant" if args.len() == 5 => {
                let volume = Id::parse(&args[2])?;
                let email = args[3].trim().to_ascii_lowercase();
                if !playbit::network::ap::operation::valid_account_email(&email) {
                    return Err("invalid recipient email".into());
                }
                let write = match args[4].as_str() {
                    "r" => false,
                    "rw" => true,
                    _ => return Err("access must be r or rw".into()),
                };
                playbit::diagnostic!("Granting volume access…");
                output(Request::GrantVolume { volume, email, write })?
            }
            _ => return Err(
                "expected pb volume create [--no-shared-key] [LABEL], list, label ID LABEL, or grant ID EMAIL r|rw"
                    .into(),
            ),
        },
        "bloblog" => return bloblog::run(&args[1..], &profile, &call),
        "file-write" | "file-read" => files::run(&args, &call, channel)?,
        "cache-budget" => output(Request::CacheBudget { bytes: argument(&args, 1)?.parse()? })?,
        "usage" => output(Request::Usage { volume: Id::parse(argument(&args, 1)?)? })?,
        "request-code" if args.len() == 2 => {
            playbit::diagnostic!("We use your email address for sign-in. Privacy policy: https://playbit.app/help/privacy");
            output(Request::RequestCode { email: args[1].clone() })?
        }
        "accept-terms" => {
            use playbit::network::ap::registration::{TERMS_REVISION, TERMS_URL, terms_digest};
            if args.len() != 3 || args[1] != "--revision" || args[2] != TERMS_REVISION.to_string() {
                return Err(format!("Read {TERMS_URL}, then run pb accept-terms --revision {TERMS_REVISION} to agree.").into());
            }
            let Reply::Status(status) = call(Request::Status)? else {
                return Err("could not identify the signed-in account".into());
            };
            output(Request::AcceptAgreement { account: status.account.id, digest: terms_digest() })?
        }
        "saved-account" => output(Request::SavedAccount)?,
        "retry-account-key-backup" => output(Request::RetryAccountKeyBackup)?,
        "sign-in" if args.len() == 2 => {
            use playbit::network::ap::registration::{TERMS_REVISION, TERMS_URL};
            let mut signed_in = output(Request::SignIn { code: args[1].clone() })?;
            let Reply::Status(status) = call(Request::Status)? else {
                return Err("could not check agreement status for the signed-in account".into());
            };
            if status.account_profile.agreement_required {
                signed_in["agreement"] = serde_json::json!({
                    "required": true,
                    "revision": TERMS_REVISION,
                    "read": TERMS_URL,
                    "accept": format!("pb accept-terms --revision {TERMS_REVISION}"),
                });
            }
            signed_in
        }
        "sync" => output(Request::Sync)?,
        "disconnect" => output(Request::Disconnect)?,
        "offline" => {
            let enabled = match (args.get(1).map(String::as_str), args.len()) {
                (Some("on"), 2) => true,
                (Some("off"), 2) => false,
                _ => return Err("expected pb offline on|off".into()),
            };
            output(Request::WorkOffline { enabled })?
        }
        "join" => return join(&args[1..], &profile, &call, channel),
        "provision" => output(Request::Provision {
            volume: Id::parse(argument(&args, 1)?)?,
            version: argument(&args, 2)?.into(),
        })?,
        "permissions" => output(Request::Permissions {
            volume: Id::parse(argument(&args, 1)?)?,
            acl: serde_json::from_slice(&fs::read(argument(&args, 2)?)?)?,
        })?,
        "revoke" => output(Request::Revoke {
            volume: Id::parse(argument(&args, 1)?)?,
            account: Id::parse(argument(&args, 2)?)?,
            reason: argument(&args, 3)?.into(),
        })?,
        "status" => output(Request::Status)?,
        "schema" | "object-create" | "objects" => data::run(&args, &call)?,
        "stop" | "frame" | "input" | "logs" | "screenshot" => control::run(&args, &profile, &call, channel)?,
        "invite" => {
            let value = call(Request::Invitation {
                session: session_argument(argument(&args, 1)?, channel)?,
            })?;
            let Reply::Invitation { invitation: _invitation, .. } = &value else {
                return Err("unexpected invitation reply".into());
            };
            if args.get(2).map(String::as_str) == Some("--copy") {
                #[cfg(target_os = "macos")]
                {
                    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
                    child
                        .stdin
                        .take()
                        .ok_or("clipboard input unavailable")?
                        .write_all(_invitation.as_bytes())?;
                    if !child.wait()?.success() {
                        return Err("clipboard copy failed".into());
                    }
                }
                #[cfg(not(target_os = "macos"))]
                return Err("clipboard copy is available on macOS".into());
            }
            serde_json::to_value(value)?
        }
        _ => return Err("unknown command; run pb help".into()),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// A session named as its base58 ID or as any form of its link; the code derives the ID.
pub(crate) fn session_argument(
    value: &str,
    channel: playbit::release_channel::ReleaseChannel,
) -> Result<String> {
    playbit::engine::session_presentation::session_argument(value, channel)
}

pub(crate) fn human_reply(
    reply: &Reply,
    channel: playbit::release_channel::ReleaseChannel,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<serde_json::Value> {
    playbit::engine::session_presentation::reply_value(reply, channel, |id| {
        playbit::engine::session_presentation::reference_from_reply(
            call(Request::SessionReference { session: id.into() })?,
            channel,
        )
    })
}

#[cfg(test)]
#[test]
fn qualified_session_arguments_reject_another_environment_before_engine_access() {
    use playbit::release_channel::ReleaseChannel;
    let seed = [7; 16];
    for selected in [ReleaseChannel::Public, ReleaseChannel::Testing] {
        let other = if selected == ReleaseChannel::Public {
            ReleaseChannel::Testing
        } else {
            ReleaseChannel::Public
        };
        let session = playbit::store::session_id(&seed).expect("fixture session ID");
        for value in [
            playbit::store::session_link(&seed, selected),
            playbit::store::session_uri(&seed, selected),
        ] {
            assert_eq!(session_argument(&value, selected).expect("matching channel"), session);
            assert!(
                join(
                    &[value],
                    std::path::Path::new("/unused"),
                    &|_| panic!("foreign link reached the engine"),
                    other
                )
                .expect_err("foreign channel")
                .to_string()
                .contains("belongs to")
            );
        }
    }
}

#[cfg(test)]
#[test]
fn join_rejects_invalid_options_before_engine_access() {
    // Invalid launch flags must not join an invitation or start an engine.
    for args in [
        vec![],
        vec!["session", "--unknown"],
        vec!["session", "--new-instance", "--new-instance"],
        vec!["session", "--background", "--background"],
        vec!["session", "--detach", "--detach"],
        vec!["session", "another-session"],
        vec!["session", "--new-instance", "--background", "extra"],
        vec!["session", "--new-instance", "--background", "--detach", "extra"],
    ] {
        let args = args.into_iter().map(String::from).collect::<Vec<_>>();
        let error = join(
            &args,
            std::path::Path::new("/unused"),
            &|_| panic!("invalid options reached the engine"),
            playbit::release_channel::ReleaseChannel::Public,
        )
        .expect_err("invalid join options");
        assert!(error.to_string().contains("expected pb session join"), "{error}");
    }
}

#[cfg(test)]
#[test]
fn join_accepts_complete_codes_and_invitation_links() {
    // Every human spelling carries the same full join credential.
    let channel = playbit::release_channel::ReleaseChannel::Public;
    let seed = [7; 16];
    let id = playbit::store::session_id(&seed).expect("fixture session ID");
    assert!(session_argument(&id, channel).is_err());
    for value in [
        playbit::store::session_code(&seed),
        playbit::store::session_link(&seed, channel),
        playbit::store::session_uri(&seed, channel),
    ] {
        for flags in [
            vec![],
            vec!["--new-instance"],
            vec!["--background"],
            vec!["--new-instance", "--background"],
            vec!["--background", "--new-instance"],
            vec!["--detach"],
            vec!["--new-instance", "--background", "--detach"],
            vec!["--detach", "--background", "--new-instance"],
        ] {
            let mut args = vec![value.clone()];
            args.extend(flags.into_iter().map(String::from));
            let error = join(
                &args,
                std::path::Path::new("/unused"),
                &|request| {
                    assert!(matches!(request, Request::Join { invitation } if invitation == value));
                    Err("fixture stops before launch".into())
                },
                channel,
            )
            .expect_err("fixture stops before launch");
            assert_eq!(error.to_string(), "fixture stops before launch");
        }
    }
}

fn join(
    args: &[String],
    profile: &std::path::Path,
    call: &impl Fn(Request) -> Result<Reply>,
    channel: playbit::release_channel::ReleaseChannel,
) -> Result<()> {
    if !(1..=4).contains(&args.len()) {
        return Err(
            "expected pb session join SESSION [--new-instance] [--background] [--detach]".into()
        );
    }
    let mut fresh = false;
    let mut background = false;
    let mut detach = false;
    for option in &args[1..] {
        match option.as_str() {
            "--new-instance" if !fresh => fresh = true,
            "--background" if !background => background = true,
            "--detach" if !detach => detach = true,
            _ => {
                return Err(
                    "expected pb session join SESSION [--new-instance] [--background] [--detach]"
                        .into(),
                );
            }
        }
    }

    let value = &args[0];
    let session = session_argument(value, channel)?;
    let joined = call(Request::Join { invitation: value.into() })?;
    if !matches!(joined, Reply::Session(joined) if joined.id == session) {
        return Err("missing joined session".into());
    }
    let prepared = developer::prepare(&session, call)?;

    #[cfg(all(target_os = "macos", feature = "desktop"))]
    {
        let instance = playbit::desktop::guest_app::launch(
            profile,
            &prepared.session,
            fresh,
            !background,
            vec![],
            false,
            None,
        )?;
        let human = human_reply(&Reply::SessionPrepared(prepared), channel, call)?;
        println!("{}", serde_json::json!({"prepared": human, "instance": instance}));
        std::io::stdout().flush()?;
        if !detach {
            let owner = control::wait_owner(&session, instance, call)?;
            let logs = logs::Follow::start(profile, &owner)?;
            playbit::diagnostic!("Following app logs. Ctrl-C detaches; the app keeps running.");
            logs.wait();
        }
        Ok(())
    }
    #[cfg(not(all(target_os = "macos", feature = "desktop")))]
    {
        let _ = (profile, prepared, fresh, background, detach);
        Err("Independent GUI launch requires the macOS desktop composition".into())
    }
}

fn entry_address(args: &[String]) -> Result<playbit::store::volume_index::EntryAddress> {
    Ok(playbit::store::volume_index::EntryAddress {
        volume: Id::parse(argument(args, 1)?)?,
        entry: playbit::store::EntryId::parse(argument(args, 2)?)?,
    })
}
