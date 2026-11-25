use crate::usage_error;
use playbit::engine::{
    Request, Result,
    reply::{DeveloperReply, Reply},
};
use playbit::identifier::valid as valid_id;
use playbit::package::{BuildMode, Manifest};
use playbit::store::Id;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

/// Drain immediately with a fixed buffer; compiler and script output never enters JSON stdout.
pub(super) fn execute(mut command: Command) -> Result<()> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
    let copied = (|| -> Result<()> {
        let mut output = child.stdout.take().ok_or("missing command output")?;
        let mut buffer = [0; 8192];
        loop {
            let size = match output.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if size == 0 {
                break;
            }
            // A quiet compiler may block read indefinitely. Hold stderr only
            // while copying bytes so the app's independent log consumer runs.
            std::io::stderr().lock().write_all(&buffer[..size])?;
        }
        Ok(())
    })();
    if copied.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    copied?;
    if !status.success() {
        return Err(format!("command failed ({status})").into());
    }
    Ok(())
}

pub(super) fn init(
    args: &[String],
    directory: &Path,
    executable: &Path,
    profile: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Value> {
    let options = InitOptions::parse(args).map_err(usage_error)?;
    let started = Instant::now();
    let destination = directory.join(options.path.unwrap_or("."));
    let destination: PathBuf = destination.components().collect();
    create_init_directory(&destination)
        .map_err(|error| format!("creating {}: {error}", destination.display()))?;
    let directory = destination.canonicalize()?;
    if directory.join(playbit::package::MANIFEST_FILE).is_file()
        && !directory.join(".pb-init.json").exists()
    {
        let manifest = Manifest::read(&directory)?;
        if options.name.is_some_and(|name| name != manifest.name)
            || options.app_id.is_some_and(|app_id| app_id != manifest.app_id)
            || options.language.is_some_and(|lang| manifest.lang != Some(lang))
        {
            return Err("existing source identity differs from initialization options".into());
        }
        let _api = crate::project::prepare_sdk(&directory, executable)?;
        playbit::package::metadata::generate(&directory, &manifest)?;
        let declaration = playbit::package::read_volume_initializers(&directory)?
            .for_role(playbit::package::AutomaticVolumeRole::Root);
        let root = initialize_app(&manifest, declaration, call)?;
        return Ok(
            json!({"source":directory,"volume":root,"timing_us":{"total":started.elapsed().as_micros()}}),
        );
    }
    let mut project = crate::project::Initialization::begin_with_language(
        &directory,
        options.name,
        options.app_id,
        options.language,
        executable,
        profile,
    )?;
    let root = (|| -> Result<Id> {
        project.scaffold()?;
        let _api = crate::project::prepare_sdk(&directory, executable)?;
        let (manifest, declaration) = project.validate()?;
        playbit::package::metadata::generate(&directory, &manifest)?;
        let root = initialize_app(&manifest, declaration, call)?;
        let mut options = git2::RepositoryInitOptions::new();
        options.initial_head("main").external_template(false).no_reinit(true);
        let repository = match git2::Repository::init_opts(&directory, &options) {
            Ok(repository) => repository,
            Err(error) if error.code() == git2::ErrorCode::Exists => {
                git2::Repository::open(&directory)?
            }
            Err(error) => return Err(error.into()),
        };
        if repository.workdir() != Some(directory.as_path()) {
            return Err("initialization repository belongs to another directory".into());
        }
        Ok(root)
    })()
    .map_err(|error| project.recovery_error(error))?;
    project.finish()?;
    playbit::diagnostic!("Created .git, src, playbit.toml, the app icon and README.md");
    playbit::diagnostic!(
        "Run the app with `pb run` and use `pb schema-gen src/schema.json` if you change the schema"
    );
    Ok(
        json!({"source":directory,"volume":root,"timing_us":{"total":started.elapsed().as_micros()}}),
    )
}

fn initialize_app(
    manifest: &Manifest,
    declaration: playbit::store::RootInitialization,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Id> {
    let Reply::Volume(volume) = call(Request::InitializeApp {
        app_id: manifest.app_id.clone(),
        name: manifest.name.clone(),
        declaration: Box::new(declaration),
    })?
    else {
        return Err("expected initialized app root".into());
    };
    let app =
        playbit::package::AppIdentity { publisher: volume.owner, app_id: manifest.app_id.clone() };
    if volume.personal
        || volume.id
            != app.volume_id(playbit::package::AutomaticVolumeRole::Root, None, None, None)?
    {
        return Err("initialized app root has a different binding".into());
    }
    Ok(volume.id)
}

fn create_init_directory(directory: &Path) -> Result<()> {
    use rustix::fs::{Access, AtFlags, CWD, accessat};
    let mut missing = Vec::new();
    let mut existing = directory;
    while !existing.try_exists()? {
        missing.push(existing);
        existing = existing.parent().ok_or("missing destination parent")?;
    }
    // A retry can find the last mkdir before its parent was synced. Persist
    // that entry unless its parent is an external, search-only boundary.
    // Every mkdir below first opens its parent, so this helper cannot create
    // an entry under an unreadable parent while permissions stay unchanged.
    if let Some(parent) = existing.parent() {
        match fs::File::open(parent) {
            Ok(parent) => parent.sync_all()?,
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && matches!(
                        accessat(CWD, parent, Access::WRITE_OK, AtFlags::EACCESS),
                        Err(rustix::io::Errno::ACCESS)
                    ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    for path in missing.into_iter().rev() {
        let parent = path.parent().ok_or("missing destination parent")?;
        let parent = fs::File::open(parent)?;
        fs::create_dir_all(path)?;
        #[cfg(test)]
        if INTERRUPT_INIT_DIRECTORY.with(|interrupt| interrupt.replace(false)) {
            return Err("injected interruption after mkdir".into());
        }
        parent.sync_all()?;
    }
    if !directory.is_dir() {
        return Err("destination must be a directory".into());
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static INTERRUPT_INIT_DIRECTORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Default)]
struct InitOptions<'a> {
    path: Option<&'a str>,
    name: Option<&'a str>,
    app_id: Option<&'a str>,
    language: Option<playbit::package::Language>,
}

impl<'a> InitOptions<'a> {
    fn parse(args: &'a [String]) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = args.iter();
        while let Some(argument) = arguments.next() {
            let (flag, value) = argument.split_once('=').unwrap_or((argument, ""));
            if matches!(flag, "--name" | "--app-id" | "--lang") {
                let value = if argument.contains('=') {
                    value
                } else {
                    arguments.next().ok_or_else(|| format!("missing {flag} value"))?
                };
                match flag {
                    "--name" if options.name.is_none() => options.name = Some(value),
                    "--app-id" if options.app_id.is_none() => {
                        if !playbit::package::valid_app_id(value) {
                            return Err("invalid --app-id: expected 1–64 lowercase ASCII \
                                letters/digits in nonempty components separated by dots or \
                                hyphens"
                                .into());
                        }
                        options.app_id = Some(value);
                    }
                    "--lang" if options.language.is_none() => {
                        options.language = Some(crate::sdk::language(value)?);
                    }
                    _ => return Err(format!("repeated {flag}").into()),
                }
            } else if !argument.starts_with('-') && options.path.is_none() {
                options.path = Some(argument);
            } else {
                return Err(
                    "expected pb init [PATH] [--name NAME] [--app-id ID] [--lang rust|ocaml]"
                        .into(),
                );
            }
        }
        Ok(options)
    }
}

#[derive(Default, Debug)]
struct Options {
    release: bool,
    native: bool,
    template: bool,
    verbose: bool,
    trace: bool,
    gas_limit: Option<NonZeroU64>,
    sdk: crate::sdk::Policy,
    root: Option<Id>,
    user: Option<Id>,
    build: Option<String>,
    app: Option<playbit::package::AppIdentity>,
    session: Option<String>,
    namespace: Option<String>,
    notes: Option<String>,
    name: Option<String>,
    mounts: Vec<playbit::store::PersonalVolumeGrant>,
    fs: Vec<String>,
}
impl Options {
    fn build_mode(&self) -> BuildMode {
        BuildMode { release: self.release, native: self.native }
    }
    fn enter_sdk(&self, directory: &Path, manifest: &Manifest) -> Result<crate::sdk::Scope> {
        crate::sdk::enter(directory, manifest, self.sdk, self.native, self.verbose)
    }
    fn manifest(&self, directory: &Path) -> Result<Manifest> {
        let mut manifest = Manifest::read(directory).map_err(|error| {
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                format!(
                    "No playbit.toml in {}. Run this command in a Playbit app directory, \
                     or create one with pb init PATH.",
                    directory.display()
                )
                .into()
            } else {
                error
            }
        })?;
        manifest.app_id =
            playbit::package::effective_app_id(&manifest.app_id, self.namespace.as_deref())?;
        Ok(manifest)
    }
    fn parse(args: &[String], allowed: &[&str], named: bool) -> Result<Self> {
        let mut value = Self::default();
        let mut seen = std::collections::HashSet::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if !arg.starts_with('-') && named && value.name.is_none() {
                value.name = Some(arg.clone());
                continue;
            }
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
            let flag = if flag == "-v" { "--verbose" } else { flag };
            let key = if flag == "--dev" { "--namespace" } else { flag };
            if !allowed.contains(&flag)
                || (!seen.insert(key) && !matches!(flag, "--mount" | "--fs"))
            {
                return Err(usage_error(format!("unexpected or repeated argument: {arg}")));
            }
            if matches!(
                flag,
                "--release" | "--native" | "--template" | "--verbose" | "--trace" | "--dev"
            ) {
                if inline.is_some() {
                    return Err(usage_error(format!("{flag} does not take a value")));
                }
                match flag {
                    "--release" => value.release = true,
                    "--native" => value.native = true,
                    "--template" => value.template = true,
                    "--verbose" => value.verbose = true,
                    "--trace" => value.trace = true,
                    "--dev" => value.namespace = Some("dev".into()),
                    _ => unreachable!(),
                }
                continue;
            }
            let next = inline
                .or_else(|| args.next().map(String::as_str))
                .filter(|next| !next.starts_with("--"))
                .ok_or_else(|| usage_error(format!("missing value for {flag}")))?;
            value.set_option(flag, next).map_err(usage_error)?;
        }
        if value.template && value.native {
            return Err(usage_error("--template supports only Wasm packages"));
        }
        Ok(value)
    }
    fn set_option(&mut self, flag: &str, next: &str) -> Result<()> {
        match flag {
            "--sdk" => self.sdk = crate::sdk::Policy::parse(next)?,
            "--gas-limit" => self.gas_limit = parse_gas_limit(next)?,
            "--root-volume" => self.root = Some(Id::parse(next)?),
            "--user-volume" => self.user = Some(Id::parse(next)?),
            "--app" => self.app = Some(app_identity(next)?),
            "--session" => {
                if next.len() > 256 {
                    return Err("invalid session ID or link".into());
                }
                self.session = Some(next.to_owned());
            }
            "--namespace" => {
                playbit::package::effective_app_id("a", Some(next))?;
                self.namespace = Some(next.to_owned());
            }
            "--build" => {
                if !valid_id(next, 32) {
                    return Err("invalid build ID".into());
                }
                self.build = Some(next.to_owned());
            }
            "--notes" => {
                if !playbit::network::ap::operation::valid_app_notes(next) {
                    return Err("publication notes exceed 32 KiB of JSON-escaped text".into());
                }
                self.notes = Some(next.to_owned());
            }
            "--mount" => self.mount(next)?,
            "--fs" => {
                if self.fs.len() == playbit::engine::host_mount::MAX_MOUNTS {
                    return Err("Too many host filesystem mounts".into());
                }
                self.fs.push(next.to_owned());
            }
            _ => return Err(format!("unsupported option {flag}").into()),
        }
        Ok(())
    }
    fn mount(&mut self, next: &str) -> Result<()> {
        let (id, rights) = next.split_once(':').unwrap_or((next, "rw"));
        let volume = Id::parse(id)?;
        let write = match rights {
            "r" => false,
            "rw" => true,
            _ => return Err("expected --mount VOLUME[:r|rw]".into()),
        };
        if self.mounts.len() >= playbit_api::abi::syscall::MAX_VOLUMES - 7
            || self.mounts.iter().any(|grant| grant.volume == volume)
        {
            return Err("too many or repeated personal volumes".into());
        }
        self.mounts.push(playbit::store::PersonalVolumeGrant { volume, write });
        Ok(())
    }
}

pub(super) fn app_identity(text: &str) -> Result<playbit::package::AppIdentity> {
    let (publisher, app_id) = text.split_once('/').ok_or("expected PUBLISHER/APP-ID")?;
    let app = playbit::package::AppIdentity {
        publisher: Id::parse(publisher)?,
        app_id: app_id.to_owned(),
    };
    app.validate()?;
    Ok(app)
}

fn parse_gas_limit(value: &str) -> Result<Option<NonZeroU64>> {
    let (digits, multiplier) = match value.strip_suffix('M') {
        Some(digits) => (digits, 1_000_000),
        None => (value, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("invalid --gas-limit: expected an unsigned decimal value, \
            optionally followed by M (1,000,000); 0 is unlimited"
            .into());
    }
    let limit = digits
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
        .ok_or("invalid --gas-limit: value exceeds u64 fuel capacity")?;
    Ok(NonZeroU64::new(limit))
}

fn build(directory: &Path, command: &str) -> Result<u128> {
    crate::sdk::before_build(directory, &Manifest::read(directory)?)?;
    let _api = crate::project::prepare_sdk(directory, &std::env::current_exe()?)?;
    playbit::diagnostic!("Building app from source: {command}");
    let started = Instant::now();
    playbit::package::metadata::generate(directory, &Manifest::read(directory)?)?;
    if Manifest::read(directory)?.lang == Some(playbit::package::Language::Ocaml) {
        crate::sdk::build_ocaml(directory, command == "release")?;
        return Ok(started.elapsed().as_micros());
    }
    let mut child = Command::new("sh");
    crate::sdk::configure(&mut child);
    child.arg("-c").arg(command).current_dir(directory);
    execute(child)?;
    Ok(started.elapsed().as_micros())
}

pub(super) fn artifact(
    directory: &Path,
    mode: BuildMode,
    target_directory: Option<&Path>,
) -> Result<PathBuf> {
    let manifest = Manifest::read(directory)?;
    if !mode.native
        && let Some(path) = manifest.wasm_artifact(mode.release)
    {
        return Ok(directory.join(path));
    }
    if manifest.lang == Some(playbit::package::Language::Ocaml) {
        return Ok(directory.join(if mode.release {
            "target/release/app.wasm"
        } else {
            "target/debug/app.wasm"
        }));
    }
    let metadata = cargo_metadata(directory, target_directory)?;
    artifact_path(directory, mode, &metadata)
}

fn cargo_metadata(directory: &Path, target_directory: Option<&Path>) -> Result<Value> {
    let mut command = crate::sdk::command("cargo");
    if let Some(target) = target_directory {
        command.env("CARGO_TARGET_DIR", target);
    }
    let mut child = command
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or("missing Cargo metadata")?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > 1024 * 1024 {
        let _ = child.kill();
        let _ = child.wait();
        read?;
        return Err("Cargo metadata exceeds 1 MiB".into());
    }
    if !child.wait()?.success() {
        return Err("Cargo metadata failed".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn artifact_path(directory: &Path, mode: BuildMode, metadata: &Value) -> Result<PathBuf> {
    let manifest = directory.join("Cargo.toml").canonicalize()?;
    let package = metadata["packages"]
        .as_array()
        .ok_or("missing Cargo packages")?
        .iter()
        .find(|p| p["manifest_path"].as_str().is_some_and(|path| Path::new(path) == manifest))
        .ok_or("current directory must contain the app's Cargo.toml")?;
    let targets = package["targets"].as_array().ok_or("missing Cargo targets")?;
    let mut selected = targets.iter().filter(|t| {
        // Cargo build scripts also have crate_type=bin, but are not app binaries.
        let kinds = if mode.native { &t["kind"] } else { &t["crate_types"] };
        kinds.as_array().is_some_and(|k| {
            k.iter().any(|k| {
                if mode.native { k == "bin" } else { k == "lib" || k == "rlib" || k == "cdylib" }
            })
        })
    });
    let error = if mode.native {
        "native app needs exactly one Cargo binary target"
    } else {
        "app needs exactly one Cargo library target"
    };
    let target = selected.next().ok_or(error)?;
    if selected.next().is_some() {
        return Err(error.into());
    }
    let name = target["name"].as_str().ok_or("missing Cargo target name")?;
    if name.is_empty()
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err("invalid Cargo target name".into());
    }
    let target_directory =
        Path::new(metadata["target_directory"].as_str().ok_or("missing Cargo target directory")?);
    Ok(target_directory
        .join(if mode.native { "aarch64-apple-darwin" } else { "wasm32-unknown-unknown" })
        .join(if mode.release { "release" } else { "debug" })
        .join(if mode.native { name.into() } else { format!("{}.wasm", name.replace('-', "_")) }))
}

pub(super) fn product_path(artifact: &Path, mode: BuildMode, name: &str) -> Result<PathBuf> {
    let target = artifact
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("invalid Cargo artifact path")?;
    product_in(target, mode, name)
}

fn product_in(target: &Path, mode: BuildMode, name: &str) -> Result<PathBuf> {
    let extension = if mode.native { "app" } else { "pbapp" };
    // The display name must also be one filename, including its extension.
    if name.trim().is_empty()
        || name.len() > 255 - extension.len() - 1
        || name.contains(['/', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(format!(
            "app name must fit a 255-byte .{extension} filename and contain no slash, \
             colon or control character"
        )
        .into());
    }
    Ok(target
        .join(if mode.release { "release" } else { "debug" })
        .join(format!("{name}.{extension}")))
}

fn bundle(
    directory: &Path,
    manifest: &Manifest,
    release: bool,
    template: bool,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<(PathBuf, String)> {
    playbit::diagnostic!("Running `pb bundle`");
    let mode = BuildMode { release, native: false };
    let artifact = artifact(directory, mode, None)?;
    let output = if manifest.lang == Some(playbit::package::Language::Ocaml)
        || manifest.wasm_artifact(release).is_some()
    {
        product_in(&directory.join("target"), mode, &manifest.name)?
    } else {
        product_path(&artifact, mode, &manifest.name)?
    };
    if !artifact.is_file() {
        return Err(format!("build artifact is missing: {}", artifact.display()).into());
    }
    if manifest.wasm_artifact(release).is_some()
        && !artifact.canonicalize()?.starts_with(directory.canonicalize()?)
    {
        return Err("build_artifact leaves the source directory".into());
    }
    let mut wasm = Vec::new();
    fs::File::open(artifact)?
        .take(playbit::package::codec::MAX_WASM_BYTES as u64 + 1)
        .read_to_end(&mut wasm)?;
    if wasm.len() > playbit::package::codec::MAX_WASM_BYTES {
        return Err("Wasm exceeds 64 MiB file limit".into());
    }

    let products = output.parent().ok_or("missing bundle directory")?;
    fs::create_dir_all(products)?;
    let temporary = products.join(format!(".staging-{}.pbapp", Id::generate()?));
    let mut file = fs::File::create_new(&temporary)?;
    let mut id = String::new();
    let result = (|| -> Result<()> {
        manifest.write_bundle(directory, wasm, &mut file)?;
        file.sync_all()?;
        drop(file);
        let expected = if template {
            None
        } else {
            let Reply::Developer(DeveloperReply::PackageSigned { build_id }) =
                call(Request::SignPackage { bundle: temporary.clone() })?
            else {
                return Err("expected authenticated package signing confirmation".into());
            };
            Some(build_id)
        };
        id = playbit::package::Package::open(&temporary)?.id()?;
        if expected.is_some_and(|expected| id != expected) {
            return Err("signed package identity differs from its receipt".into());
        }
        fs::rename(&temporary, &output)?;
        fs::File::open(products)?.sync_all()?;
        Ok(())
    })();
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    result?;
    playbit::diagnostic!("Creating {}:{id}", if template { "template" } else { "build" });
    Ok((output, id))
}

pub(super) fn bundle_command(
    args: &[String],
    directory: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Value> {
    let options = Options::parse(
        args,
        &["--release", "--native", "--template", "--namespace", "--dev"],
        false,
    )?;
    package_build(directory, &options.manifest(directory)?, &options, call)
}

pub(super) fn build_command(
    args: &[String],
    directory: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Value> {
    let options = Options::parse(
        args,
        &["--release", "--native", "--template", "--sdk", "--verbose", "--namespace", "--dev"],
        false,
    )?;
    if options.native {
        crate::native::bundle::supported()?;
    }
    let manifest = options.manifest(directory)?;
    let _sdk = options.enter_sdk(directory, &manifest)?;
    if options.native {
        crate::native::build(
            directory,
            &manifest,
            manifest.build_command(options.build_mode())?,
            call,
        )?;
    } else {
        build(
            directory,
            if manifest.lang == Some(playbit::package::Language::Ocaml) {
                if options.release { "release" } else { "debug" }
            } else {
                manifest.build_command(options.build_mode())?
            },
        )?;
    }
    package_build(directory, &manifest, &options, call)
}

fn package_build(
    directory: &Path,
    manifest: &Manifest,
    options: &Options,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Value> {
    if options.native {
        let path = crate::native::bundle::bundle(directory, manifest, options.release, call)?;
        Ok(json!({"bundle":path}))
    } else {
        let (path, id) = bundle(directory, manifest, options.release, options.template, call)?;
        if options.template {
            Ok(json!({"bundle":path,"template":true}))
        } else {
            Ok(json!({"bundle":path,"build_id":id}))
        }
    }
}

pub(super) fn create_session(args: &[String], directory: &Path) -> Result<Request> {
    let options = Options::parse(args, &["--root-volume", "--user-volume"], true)?;
    let bundle = options.name.filter(|name| name.ends_with(".pbapp")).ok_or_else(|| {
        usage_error("expected pb session create APP.pbapp [--root-volume ID] [--user-volume ID]")
    })?;
    Ok(Request::CreateSession {
        bundle: directory.join(bundle),
        root: options.root,
        user: options.user,
    })
}

fn selected_mounts(options: &Options) -> Option<Vec<playbit::store::PersonalVolumeGrant>> {
    (!options.mounts.is_empty()).then(|| options.mounts.clone())
}

pub(super) fn run(
    args: &[String],
    directory: &Path,
    profile: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
    channel: playbit::release_channel::ReleaseChannel,
) -> Result<()> {
    let started = Instant::now();
    let options = run_options(args, channel)?;
    let mounts = options
        .fs
        .iter()
        .map(|argument| playbit::engine::host_mount::HostMount::from_cli(argument, directory))
        .collect::<Result<Vec<_>>>()?;
    playbit::engine::host_mount::validate(&mounts)?;
    let source_run = options.name.is_none()
        && options.build.is_none()
        && options.app.is_none()
        && directory.join(playbit::package::MANIFEST_FILE).is_file();
    if !source_run && (options.namespace.is_some() || options.release) {
        return Err(usage_error("--namespace, --dev and --release require a source build"));
    }
    let _sdk = if source_run {
        Some(options.enter_sdk(directory, &options.manifest(directory)?)?)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let mut watcher = source_watch(directory, profile, &options, source_run)?;
    #[cfg(not(target_os = "macos"))]
    if source_run {
        return Err("development source watching requires macOS".into());
    }
    let watch_setup_us = started.elapsed().as_micros();
    let (request, compiler_us) = if source_run {
        let (request, compiler_us) = build_run_request(directory, &options, call)?;
        #[cfg(target_os = "macos")]
        if let Some(watcher) = &mut watcher {
            watcher.record_artifact()?;
        }
        (Some(request), compiler_us)
    } else {
        (run_selection_request(directory, &options)?, 0)
    };
    let (id, updated) = select_session(request, options.session.as_deref(), call)?;
    playbit::diagnostic!("{} session", if updated { "Updating" } else { "Opening" });
    let (prepared, instance) = open_selected_run(profile, &id, mounts, &options, call)?;
    let reference = playbit::engine::session_presentation::reference_from_reply(
        call(Request::SessionReference { session: prepared.session.id.clone() })?,
        channel,
    )?;
    let human =
        playbit::engine::session_presentation::session_value(&prepared.session, &reference)?;
    println!(
        "{}",
        json!({"session":human,"availability":prepared.availability,"update":prepared.update,
        "instance":instance,
        "timing_us":{"total":started.elapsed().as_micros(),"compiler":compiler_us,"watch_setup":watch_setup_us}})
    );
    std::io::stdout().flush()?;
    #[cfg(target_os = "macos")]
    if let Some(watcher) = watcher {
        watch_source(
            directory,
            profile,
            &id,
            &options,
            watcher,
            (instance, prepared.session.resolved_build_id),
            call,
        )?;
    }
    Ok(())
}

fn run_options(
    args: &[String],
    channel: playbit::release_channel::ReleaseChannel,
) -> Result<Options> {
    let mut options = Options::parse(
        args,
        &[
            "--release",
            "--root-volume",
            "--user-volume",
            "--build",
            "--app",
            "--session",
            "--namespace",
            "--dev",
            "--mount",
            "--fs",
            "--sdk",
            "--verbose",
            "--trace",
            "--gas-limit",
            "--native",
        ],
        true,
    )?;
    validate_run(&options).map_err(usage_error)?;
    if let Some(session) = options.session.as_deref() {
        options.session = Some(crate::session_argument(session, channel)?);
    }
    Ok(options)
}

fn open_selected_run(
    profile: &Path,
    id: &str,
    mounts: Vec<playbit::engine::host_mount::HostMount>,
    options: &Options,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<(playbit::engine::reply::PreparedSession, Id)> {
    if let Some(grants) = selected_mounts(options) {
        let selected = call(Request::AddPersonalVolumes { session: id.into(), grants })?;
        if !matches!(selected, Reply::PersonalVolumesSelected { personal_volumes_selected: true }) {
            return Err("Session volume mounts were not confirmed".into());
        }
    }
    let prepared = prepare(id, call)?;
    let fresh = options.session.is_none() || !options.mounts.is_empty() || !mounts.is_empty();
    let instance = launch_with_grants(
        profile,
        &prepared.session,
        mounts,
        fresh,
        options.gas_limit,
        selected_mounts(options),
    )?;
    if options.trace {
        crate::trace::start(profile, instance).map_err(|error| {
            format!(
                "Instance {instance} was launched, but tracing could not start: {error}. \
                 Inspect this instance before launching again."
            )
        })?;
    }
    Ok((prepared, instance))
}

fn validate_run(options: &Options) -> Result<()> {
    if options.gas_limit.is_some()
        && options.name.as_ref().is_some_and(|name| name.ends_with(".app"))
    {
        return Err(
            "positive --gas-limit cannot be enforced by native executables; pb run requires Wasm"
                .into(),
        );
    }
    if options.native {
        return Err(if options.gas_limit.is_some() {
            "positive --gas-limit cannot be enforced by native executables; pb run requires Wasm"
        } else {
            "pb run requires Wasm; use pb build --native to build a native executable"
        }
        .into());
    }
    if usize::from(options.name.is_some())
        + usize::from(options.build.is_some())
        + usize::from(options.app.is_some())
        > 1
    {
        return Err("select one package, exact build or published app".into());
    }
    if options.name.as_ref().is_some_and(|name| !name.ends_with(".pbapp")) {
        return Err("expected an app bundle ending in .pbapp".into());
    }
    if options.session.is_some()
        && (options.root.is_some()
            || options.user.is_some()
            || options.build.is_some()
            || options.app.is_some())
    {
        return Err(
            "--session retains its app and volume bindings; overrides require a new session".into(),
        );
    }
    Ok(())
}

fn run_selection_request(directory: &Path, options: &Options) -> Result<Option<Request>> {
    if let Some(name) = &options.name {
        let bundle = directory.join(name);
        return Ok(Some(match &options.session {
            Some(session) => Request::UpdateSession { session: session.clone(), bundle },
            None => Request::CreateSession { bundle, root: options.root, user: options.user },
        }));
    }
    if let Some(build) = &options.build {
        return Ok(Some(Request::CreateBuildSession {
            build: build.clone(),
            root: options.root,
            user: options.user,
        }));
    }
    if let Some(app) = &options.app {
        return Ok(Some(Request::CreateAppSession {
            app: app.clone(),
            root: options.root,
            user: options.user,
        }));
    }
    if options.session.is_none() {
        return Err("missing playbit.toml; select a package, --build, --app or --session".into());
    }
    Ok(None)
}

fn build_run_request(
    directory: &Path,
    options: &Options,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<(Request, u128)> {
    let manifest = options.manifest(directory)?;
    let compiler_us = build(
        directory,
        if manifest.lang == Some(playbit::package::Language::Ocaml) {
            if options.release { "release" } else { "debug" }
        } else {
            manifest.build_command(options.build_mode())?
        },
    )?;
    let (bundle, _) = bundle(directory, &manifest, options.release, false, call)?;
    let request = match &options.session {
        Some(session) => Request::UpdateSession { session: session.clone(), bundle },
        None => Request::CreateSession { bundle, root: options.root, user: options.user },
    };
    Ok((request, compiler_us))
}

fn select_session(
    request: Option<Request>,
    retained: Option<&str>,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<(String, bool)> {
    let Some(request) = request else {
        return Ok((retained.ok_or("missing retained session")?.to_owned(), false));
    };
    match call(request)? {
        Reply::Developer(DeveloperReply::Updated { session, .. }) => Ok((session, true)),
        Reply::Developer(DeveloperReply::Session(session)) => Ok((session.id, false)),
        _ => Err("unexpected session reply".into()),
    }
}

#[cfg(target_os = "macos")]
fn source_watch(
    directory: &Path,
    profile: &Path,
    options: &Options,
    source_run: bool,
) -> Result<Option<crate::watch::SourceWatch>> {
    if !source_run {
        return Ok(None);
    }
    let artifact = artifact(directory, options.build_mode(), None)?;
    let manifest = options.manifest(directory)?;
    let output_directory = if manifest.lang == Some(playbit::package::Language::Ocaml)
        || manifest.wasm_artifact(options.release).is_some()
    {
        artifact.parent()
    } else {
        artifact.parent().and_then(Path::parent).and_then(Path::parent)
    }
    .ok_or("invalid build artifact path")?;
    Ok(Some(crate::watch::SourceWatch::start(directory, &artifact, output_directory, profile)?))
}

#[cfg(target_os = "macos")]
fn watch_source(
    directory: &Path,
    profile: &Path,
    id: &str,
    options: &Options,
    mut watcher: crate::watch::SourceWatch,
    launched: (Id, String),
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<()> {
    let (instance, build) = launched;
    let owner = crate::control::wait_owner(id, instance, call)?;
    let _logs = crate::logs::Follow::start(profile, &owner)?;
    let mut packaged = (build, true);
    playbit::diagnostic!(
        "Watching source and build artifact. Ctrl-C ends watching; the session is retained."
    );
    loop {
        let change = watcher.next()?;
        match update_source(
            directory,
            options,
            id,
            change.source,
            &mut packaged,
            &mut watcher,
            call,
        ) {
            Ok(Some(mut value)) => {
                value["timing_us"]["notification_to_schedule"] = json!(change.scheduling_us);
                println!("{value}");
                std::io::stdout().flush()?;
            }
            Ok(None) => {}
            Err(error) => playbit::diagnostic!(
                "Development update failed: {error}. The last working app is retained; watching continues."
            ),
        }
    }
}

#[cfg(target_os = "macos")]
fn update_source(
    directory: &Path,
    options: &Options,
    session: &str,
    rebuild: bool,
    packaged: &mut (String, bool),
    watcher: &mut crate::watch::SourceWatch,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Option<Value>> {
    let started = Instant::now();
    let release = options.release;
    let manifest = options.manifest(directory)?;
    let compiler_us = if rebuild {
        build(
            directory,
            if manifest.lang == Some(playbit::package::Language::Ocaml) {
                if release { "release" } else { "debug" }
            } else {
                manifest.build_command(BuildMode { release, native: false })?
            },
        )?
    } else {
        0
    };
    if rebuild {
        watcher.record_artifact()?;
    }
    let prepare = Instant::now();
    let (bundle, build_id) = bundle(directory, &manifest, release, false, call)?;
    let bundle_us = prepare.elapsed().as_micros();
    // Our compiler's artifact notification may arrive after its source event.
    // Content identity suppresses that duplicate without a delay or timestamp guess.
    if packaged.0 == build_id && (packaged.1 || !rebuild) {
        return Ok(None);
    }
    *packaged = (build_id, false);
    let service = Instant::now();
    let result = call(Request::UpdateSession { session: session.into(), bundle })?;
    packaged.1 = true;
    let service_us = service.elapsed().as_micros();
    let Reply::Developer(DeveloperReply::Updated { update, resolved_build_id, .. }) = &result
    else {
        return Err("unexpected session update reply".into());
    };
    if update.is_none() && resolved_build_id.as_deref() == Some(packaged.0.as_str()) {
        playbit::diagnostic!("Updated development session");
    } else {
        playbit::diagnostic!("Development update is blocked: {}", serde_json::to_string(update)?);
    }
    let channel = playbit::engine::profile::resolve_environment()?.1;
    let reference = playbit::engine::session_presentation::reference_from_reply(
        call(Request::SessionReference { session: session.into() })?,
        channel,
    )?;
    let result = crate::human_reply(&result, channel, call)?;
    Ok(Some(json!({"event":"development_update","session":reference.code(),"result":result,
        "timing_us":{"compiler":compiler_us,"bundle":bundle_us,"service_request":service_us,
            "total":started.elapsed().as_micros()}})))
}

/// Preparation admits account/cache work only, never an engine-owned VM.
pub(super) fn prepare(
    id: &str,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<playbit::engine::reply::PreparedSession> {
    let Reply::Changes(mut token) = call(Request::WaitChanges { after: None })? else {
        return Err("Unexpected local change token".into());
    };
    let mut transfer = match call(Request::PrepareSession { session: id.into() })? {
        Reply::SessionPrepared(prepared) => return Ok(prepared),
        Reply::Transfer { transfer } => transfer,
        _ => return Err("Unexpected prepared-session reply".into()),
    };
    playbit::diagnostic!("Preparing session {id}…");
    loop {
        use playbit::engine::reply::TransferKind;
        match transfer.kind {
            TransferKind::Complete => break,
            TransferKind::Running => {
                let Reply::Changes(changed) = call(Request::WaitChanges { after: Some(token) })?
                else {
                    return Err("Unexpected session preparation wake".into());
                };
                token = changed;
                // PrepareSession retains metadata/starts work and produces its
                // own notification. Observe the accepted transfer rather than
                // invoking preparation again on that notification.
                let Reply::Transfer { transfer: current } = call(Request::TransferStatus)? else {
                    return Err("Unexpected preparation transfer status".into());
                };
                transfer = current;
            }
            _ => {
                return Err(format!(
                    "Session preparation stopped: {}",
                    transfer.message.unwrap_or_else(|| format!("{:?}", transfer.kind))
                )
                .into());
            }
        }
    }
    match call(Request::PrepareSession { session: id.into() })? {
        Reply::SessionPrepared(prepared) => Ok(prepared),
        _ => Err("The completed transfer did not prepare this session. \
            Inspect its recorded state; preparation was not repeated."
            .into()),
    }
}

fn launch_with_grants(
    profile: &Path,
    session: &playbit::store::Session,
    mounts: Vec<playbit::engine::host_mount::HostMount>,
    fresh: bool,
    gas_limit: Option<NonZeroU64>,
    extra_volumes: Option<Vec<playbit::store::PersonalVolumeGrant>>,
) -> Result<Id> {
    #[cfg(all(target_os = "macos", feature = "desktop"))]
    {
        playbit::desktop::guest_app::launch_with_options(
            profile,
            session,
            playbit::desktop::guest_app::LaunchOptions {
                fresh,
                focus_window: true,
                host_mounts: mounts,
                restore_host_mounts: true,
                fuel_limit: gas_limit,
                extra_volumes,
            },
        )
    }
    #[cfg(not(all(target_os = "macos", feature = "desktop")))]
    {
        let _ = (profile, session, mounts, fresh, gas_limit, extra_volumes);
        Err("Independent GUI launch requires the macOS desktop composition".into())
    }
}

pub(super) fn publish(
    args: &[String],
    directory: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Reply> {
    let mut options =
        Options::parse(args, &["--notes", "--namespace", "--dev", "--sdk", "--verbose"], true)?;
    let bundle = if let Some(name) = &options.name {
        if !name.ends_with(".pbapp") || options.namespace.is_some() {
            return Err(usage_error(
                "publish expects signed APP.pbapp; namespaces require rebuilding source",
            ));
        }
        directory.join(name)
    } else {
        options.release = true;
        let manifest = options.manifest(directory)?;
        let _sdk = options.enter_sdk(directory, &manifest)?;
        build(
            directory,
            if manifest.lang == Some(playbit::package::Language::Ocaml) {
                "release"
            } else {
                manifest.build_command(options.build_mode())?
            },
        )?;
        bundle(directory, &manifest, true, false, call)?.0
    };
    call(Request::PublishApp { bundle, notes: options.notes.unwrap_or_default() })
}

pub(super) fn list_apps(
    call: &impl Fn(Request) -> Result<Reply>,
    output: &mut impl Write,
) -> Result<()> {
    let mut after = None;
    loop {
        let Reply::Developer(DeveloperReply::Apps(page)) =
            call(Request::ListApps { after: after.clone() })?
        else {
            return Err("unexpected app list reply".into());
        };
        let next = page.next.clone();
        if next.as_ref().is_some_and(|next| after.as_ref().is_some_and(|after| next <= after)) {
            return Err("app cursor did not advance".into());
        }
        serde_json::to_writer(&mut *output, &page)?;
        output.write_all(b"\n")?;
        output.flush()?;
        let Some(next) = next else {
            return Ok(());
        };
        after = Some(next);
    }
}

pub(super) fn app_lifecycle(
    args: &[String],
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<Reply> {
    if args.len() != 2 {
        return Err(usage_error("expected pb app enable|disable|delete PUBLISHER/APP-ID"));
    }
    let app = app_identity(&args[1]).map_err(usage_error)?;
    let request = match args[0].as_str() {
        "enable" => Request::SetAppEnabled { app, enabled: true },
        "disable" => Request::SetAppEnabled { app, enabled: false },
        "delete" => Request::DeleteApp { app },
        _ => return Err(usage_error("expected pb app enable, disable or delete")),
    };
    call(request)
}

#[cfg(test)]
mod tests;
