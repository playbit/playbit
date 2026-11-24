//! Developer toolchain selection. Downloads never occur on the engine/UI path.
use playbit::{
    engine::Result,
    package::{Language, Manifest},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod archive;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod download;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use download::managed;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn managed(_: Language, _: bool) -> Result<Tools> {
    Err("Downloaded SDKs currently require an arm64 Mac".into())
}
mod ocaml;
#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod tests;

pub(super) fn language(text: &str) -> Result<Language> {
    match text {
        "rust" => Ok(Language::Rust),
        "ocaml" => Ok(Language::Ocaml),
        _ => Err("expected rust or ocaml".into()),
    }
}
fn name(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::Ocaml => "ocaml",
    }
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    #[default]
    Auto,
    Builtin,
    System,
}
impl Policy {
    pub(super) fn parse(text: &str) -> Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "builtin" => Ok(Self::Builtin),
            "system" => Ok(Self::System),
            _ => Err("expected --sdk=auto|builtin|system".into()),
        }
    }
}
#[derive(Clone)]
struct Tools {
    language: Language,
    programs: BTreeMap<String, PathBuf>,
    environment: BTreeMap<String, OsString>,
    identity: String,
    description: String,
    _lease: Option<Arc<File>>,
}
thread_local! { static ACTIVE: RefCell<Option<Tools>> = const { RefCell::new(None) }; }
pub(super) struct Scope(Option<Tools>);
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with_borrow_mut(|current| *current = self.0.take());
    }
}
pub(super) fn configure(command: &mut Command) {
    ACTIVE.with_borrow(|tools| {
        if let Some(tools) = tools {
            command.envs(&tools.environment);
            if tools.language == Language::Ocaml {
                command.env("OCAMLFIND_CONF", "/dev/null");
            }
        }
    });
}
pub(super) fn command(program: &str) -> Command {
    let mut command = ACTIVE.with_borrow(|tools| {
        Command::new(
            tools
                .as_ref()
                .and_then(|t| t.programs.get(program))
                .map_or_else(|| Path::new(program), PathBuf::as_path),
        )
    });
    configure(&mut command);
    command
}
fn cache() -> Result<PathBuf> {
    let path = dirs::cache_dir().ok_or("missing user cache directory")?.join("Playbit/SDK");
    fs::create_dir_all(&path)?;
    Ok(path)
}
fn resources() -> Result<PathBuf> {
    crate::project::resources(&std::env::current_exe()?)
}
fn read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err("SDK metadata must be a regular file".into());
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err("SDK metadata exceeds limit".into());
    }
    Ok(bytes)
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", playbit::store::Id::generate()?));
    let mut file = File::create_new(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    File::open(path.parent().ok_or("missing SDK parent")?)?.sync_all()?;
    Ok(())
}
fn find(program: &str) -> Result<PathBuf> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = directory.join(program);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!("{program} is not on PATH").into())
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn tools_at(language: Language, root: &Path, identity: String) -> Result<Tools> {
    let names: &[&str] = match language {
        Language::Rust => &["cargo", "rustc"],
        Language::Ocaml => {
            &["ocamlc", "ocamldep", "wasm_of_ocaml", "wasm-opt", "wasm-merge", "wasm-metadce"]
        }
    };
    let mut tools = Tools {
        language,
        programs: BTreeMap::new(),
        environment: BTreeMap::new(),
        description: identity.clone(),
        identity,
        _lease: None,
    };
    for name in names {
        let path = root.join("bin").join(name);
        if !path.is_file() {
            return Err(format!("SDK is missing {name}").into());
        }
        tools.programs.insert((*name).into(), path);
    }
    let paths = std::iter::once(root.join("bin"))
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()))
        .collect::<Vec<_>>();
    tools.environment.insert("PATH".into(), std::env::join_paths(paths)?);
    if language == Language::Ocaml {
        tools.environment.insert("OCAMLLIB".into(), root.join("lib/ocaml").into_os_string());
    }
    if language == Language::Rust {
        tools.environment.insert("RUSTC".into(), root.join("bin/rustc").into_os_string());
        tools.environment.insert("RUSTDOC".into(), root.join("bin/rustdoc").into_os_string());
    }
    tools.environment.insert("RUSTUP_AUTO_INSTALL".into(), "0".into());
    if language == Language::Ocaml {
        tools.environment.insert("OCAMLFIND_CONF".into(), "/dev/null".into());
    }
    Ok(tools)
}
fn system(language: Language, directory: &Path) -> Result<Tools> {
    let names: &[&str] = match language {
        Language::Rust => &["cargo", "rustc"],
        Language::Ocaml => {
            &["ocamlc", "ocamldep", "wasm_of_ocaml", "wasm-opt", "wasm-merge", "wasm-metadce"]
        }
    };
    let mut tools = Tools {
        language,
        programs: BTreeMap::new(),
        environment: BTreeMap::from([("RUSTUP_AUTO_INSTALL".into(), "0".into())]),
        identity: String::new(),
        description: "system".into(),
        _lease: None,
    };
    for name in names {
        tools.programs.insert((*name).into(), find(name)?);
    }
    if language == Language::Rust {
        let launcher = std::path::absolute(&tools.programs["rustc"])?;
        let rustup = launcher.canonicalize()?.file_name().is_some_and(|v| v == "rustup");
        let root =
            PathBuf::from(output(&tools, "rustc", &["--print", "sysroot"], directory)?.trim());
        if !root.join("bin/rustc").is_file() {
            return Err("system Rust sysroot has no compiler to pin for this build".into());
        }
        if !root.join("bin/cargo").is_file()
            && tools.programs["cargo"].canonicalize()?.file_name().is_some_and(|v| v == "rustup")
        {
            return Err("system Rust sysroot has no Cargo to pin for this build".into());
        }
        for name in ["rustc", "cargo"] {
            let program = root.join("bin").join(name);
            if program.is_file() {
                tools.programs.insert(name.into(), program);
            }
        }
        tools.environment.insert("RUSTC".into(), tools.programs["rustc"].as_os_str().to_owned());
        if rustup {
            // Rustup supplies loader paths for compiler children such as rust-lld.
            // Keep that launcher, but pin its selection to the probed sysroot.
            // Setting DYLD_* on the build shell is insufficient: macOS strips it.
            tools.environment.insert("RUSTC".into(), launcher.into_os_string());
            tools.environment.insert("RUSTUP_TOOLCHAIN".into(), root.as_os_str().to_owned());
        }
        let paths = std::iter::once(root.join("bin"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()))
            .collect::<Vec<_>>();
        tools.environment.insert("PATH".into(), std::env::join_paths(paths)?);
    }
    tools.identity = fingerprint(&tools.programs)?;
    let version = if language == Language::Rust {
        output(&tools, "rustc", &["--version"], directory)?
    } else {
        output(&tools, "ocamlc", &["-version"], directory)?
    };
    tools.description = format!("system ({})", version.trim());
    Ok(tools)
}
fn fingerprint(programs: &BTreeMap<String, PathBuf>) -> Result<String> {
    let mut identity = blake3::Hasher::new();
    for (name, path) in programs {
        let metadata = path.metadata()?;
        identity.update(name.as_bytes());
        identity.update(path.as_os_str().as_encoded_bytes());
        identity.update(&metadata.len().to_le_bytes());
        identity.update(
            &metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos().to_le_bytes(),
        );
    }
    Ok(playbit::identifier::encode(identity.finalize().as_bytes()))
}

pub(super) fn before_build(directory: &Path, manifest: &Manifest) -> Result<()> {
    ACTIVE.with_borrow(|tools| {
        let Some(tools) = tools else {
            return Ok(());
        };
        let language = manifest.lang.unwrap_or(Language::Rust);
        if tools.language != language {
            return Err("project language changed; restart pb run to select its SDK".into());
        }
        if tools._lease.is_none() && fingerprint(&tools.programs)? != tools.identity {
            return Err("system toolchain changed; restart pb run to validate it".into());
        }
        requirements(tools, directory)
    })
}

fn output(tools: &Tools, program: &str, args: &[&str], directory: &Path) -> Result<String> {
    let result = Command::new(&tools.programs[program])
        .args(args)
        .envs(&tools.environment)
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()?;
    if !result.status.success() {
        return Err(format!("{program}: {}", String::from_utf8_lossy(&result.stderr)).into());
    }
    if result.stdout.len() > 1024 * 1024 {
        return Err("compiler probe output exceeds limit".into());
    }
    Ok(String::from_utf8(result.stdout)?)
}
fn probe(tools: &Tools) -> Result<()> {
    match tools.language {
        Language::Rust => {
            let result = output(
                tools,
                "rustc",
                &["--print", "target-libdir", "--target", "wasm32-unknown-unknown"],
                Path::new("."),
            )?;
            if !Path::new(result.trim()).is_dir() {
                return Err("Missing rust wasm32-unknown-unknown target. Run `rustup target add wasm32-unknown-unknown` or use --sdk=builtin".into());
            }
        }
        Language::Ocaml => {
            if output(tools, "ocamlc", &["-version"], Path::new("."))?.trim() != "5.3.0" {
                return Err("OCaml SDK requires OCaml 5.3.0".into());
            }
        }
    }
    Ok(())
}
pub(super) fn enter(
    directory: &Path,
    manifest: &Manifest,
    policy: Policy,
    native: bool,
    verbose: bool,
) -> Result<Scope> {
    crate::project::prepare_sdk(directory, &std::env::current_exe()?)?;
    let language =
        if manifest.lang == Some(Language::Ocaml) { Language::Ocaml } else { Language::Rust };
    if native {
        if !matches!(policy, Policy::Auto | Policy::System) {
            return Err("downloaded SDKs support Wasm builds only".into());
        }
        return Ok(Scope(None));
    }
    if language == Language::Rust {
        if !directory.join("Cargo.toml").is_file() {
            if policy != Policy::Auto {
                return Err("Rust SDK selection requires a Cargo project".into());
            }
            return Ok(Scope(None));
        }
        if manifest.lang != Some(Language::Rust)
            && policy == Policy::Auto
            && !manifest.build_cmd.trim_start().starts_with("cargo ")
        {
            return Ok(Scope(None));
        }
    }
    // Project declarations are input errors, not a reason to download another compiler.
    let requested =
        if language == Language::Rust { Some(RustRequirements::read(directory)?) } else { None };
    // A checkout-built CLI also builds Cargo guests before an app installation
    // exists. Cargo's actual build checks that project's own SDK dependency.
    if language == Language::Rust
        && resources().is_err()
        && matches!(policy, Policy::Auto | Policy::System)
    {
        let selected = system(language, directory)?;
        probe(&selected)?;
        requirements(&selected, directory)?;
        if verbose {
            eprintln!("pb: Using {} SDK {}", name(language), selected.description);
        }
        return Ok(Scope(ACTIVE.with_borrow_mut(|current| current.replace(selected))));
    }
    let selected = if matches!(policy, Policy::Auto | Policy::System) {
        match system(language, directory).and_then(|t| {
            probe(&t)?;
            if let Some(requested) = &requested {
                requested.check(&t, directory)?;
            }
            project_probe(&t, directory)?;
            Ok(t)
        }) {
            Ok(tools) => tools,
            Err(error) if policy == Policy::Auto => {
                if verbose {
                    eprintln!("pb: Local {} toolchain unavailable: {error}", name(language));
                }
                managed(language, false).map_err(|failure| {
                    format!("Local {} toolchain unavailable: {error}\n{failure}", name(language))
                })?
            }
            Err(error) => {
                return Err(
                    format!("{error}. Use --sdk=builtin to select the Playbit toolchain").into()
                );
            }
        }
    } else {
        managed(language, false)?
    };
    requirements(&selected, directory)?;
    project_probe(&selected, directory)?;
    if verbose {
        eprintln!("pb: Using {} SDK {}", name(language), selected.description);
    }
    Ok(Scope(ACTIVE.with_borrow_mut(|current| current.replace(selected))))
}
#[derive(Default)]
struct RustRequirements {
    channel: Option<String>,
    minimum: Option<semver::Version>,
}
impl RustRequirements {
    fn read(directory: &Path) -> Result<Self> {
        if !directory.join("Cargo.toml").is_file() {
            return Ok(Self::default());
        }
        let mut channel = std::env::var("RUSTUP_TOOLCHAIN").ok();
        if channel.is_none() {
            'ancestors: for parent in directory.ancestors().take(64) {
                for path in [parent.join("rust-toolchain.toml"), parent.join("rust-toolchain")] {
                    if !path.is_file() {
                        continue;
                    }
                    let text = String::from_utf8(read(&path, 65536)?)?;
                    channel = Some(if path.extension().is_some() {
                        let value: toml::Value = toml::from_str(&text)?;
                        value
                            .get("toolchain")
                            .and_then(|v| v.get("channel"))
                            .and_then(toml::Value::as_str)
                            .ok_or(
                                "path-based Rust toolchains are not supported by this SDK resolver",
                            )?
                            .to_owned()
                    } else {
                        text.trim().to_owned()
                    });
                    break 'ancestors;
                }
            }
        }
        let document: toml::Value =
            toml::from_str(&String::from_utf8(read(&directory.join("Cargo.toml"), 1024 * 1024)?)?)?;
        let package = document.get("package");
        let requested = package.and_then(|p| p.get("rust-version"));
        let mut minimum = requested.and_then(toml::Value::as_str).map(str::to_owned);
        if requested.and_then(|v| v.get("workspace")).and_then(toml::Value::as_bool) == Some(true) {
            let explicit = package.and_then(|p| p.get("workspace")).and_then(toml::Value::as_str);
            let candidates = if let Some(path) = explicit {
                vec![directory.join(path)]
            } else {
                directory.ancestors().take(64).map(Path::to_path_buf).collect()
            };
            for parent in candidates {
                let path = parent.join("Cargo.toml");
                if !path.is_file() {
                    continue;
                }
                let value: toml::Value =
                    toml::from_str(&String::from_utf8(read(&path, 1024 * 1024)?)?)?;
                if let Some(workspace) = value.get("workspace") {
                    minimum = workspace
                        .get("package")
                        .and_then(|p| p.get("rust-version"))
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned);
                    break;
                }
            }
            if minimum.is_none() {
                return Err("workspace.package.rust-version is missing".into());
            }
        } else if requested.is_some() && minimum.is_none() {
            return Err("invalid package.rust-version".into());
        }
        let minimum = minimum
            .map(|mut value| {
                if value.matches('.').count() == 1 {
                    value.push_str(".0");
                }
                semver::Version::parse(&value)
            })
            .transpose()?;
        Ok(Self { channel, minimum })
    }
    fn check(&self, tools: &Tools, directory: &Path) -> Result<()> {
        let version = output(tools, "rustc", &["--version"], directory)?;
        let actual = version.split_whitespace().nth(1).ok_or("invalid rustc version")?;
        let mut actual_version = semver::Version::parse(actual)?;
        if let Some(requested) = &self.channel {
            let requested = requested.strip_suffix("-aarch64-apple-darwin").unwrap_or(requested);
            let root =
                PathBuf::from(output(tools, "rustc", &["--print", "sysroot"], directory)?.trim());
            let installed = root.file_name().and_then(|v| v.to_str()).unwrap_or("");
            let matches = requested == actual
                || (actual_version.pre.is_empty()
                    && (requested == actual_version.major.to_string()
                        || requested
                            == format!("{}.{}", actual_version.major, actual_version.minor)))
                || (requested == "stable" && !actual.contains('-'))
                || (requested == "nightly" && actual.contains("nightly"))
                || (requested == "beta" && actual.contains("beta"))
                || installed == requested
                || installed.starts_with(&format!("{requested}-"));
            if !matches {
                return Err(format!("project requires Rust {requested}, selected SDK provides {actual}; install the requested system toolchain or restart pb run after changing toolchain requirements").into());
            }
        }
        // Cargo ignores compiler prereleases when checking package.rust-version.
        actual_version.pre = semver::Prerelease::EMPTY;
        if self.minimum.as_ref().is_some_and(|minimum| actual_version < *minimum) {
            return Err("selected SDK is older than package.rust-version".into());
        }
        Ok(())
    }
}
fn requirements(tools: &Tools, directory: &Path) -> Result<()> {
    if tools.language == Language::Rust {
        RustRequirements::read(directory)?.check(tools, directory)?;
    }
    Ok(())
}
fn project_probe(tools: &Tools, directory: &Path) -> Result<()> {
    const RUST_PROBE: &str = r#"#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app((), |_, context, _| context.log("SDK probe"), true)
}
"#;
    let resources = resources()?;
    let mut identity = blake3::Hasher::new();
    identity.update(tools.identity.as_bytes());
    let api = match tools.language {
        Language::Rust => resources.join("playbit-api"),
        Language::Ocaml => resources.join("ocaml-sdk"),
    };
    hash_tree(&api, &mut identity)?;
    // Compiler behavior can depend on project-local rustup selection.
    if tools.language == Language::Rust {
        identity.update(RUST_PROBE.as_bytes());
        identity.update(output(tools, "rustc", &["-vV"], directory)?.as_bytes());
    }
    let root =
        cache()?.join("probes").join(playbit::identifier::encode(identity.finalize().as_bytes()));
    fs::create_dir_all(&root)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("lock"))?;
    lock.lock()?;
    if root.join("ok").is_file() {
        return Ok(());
    }
    let old = ACTIVE.with_borrow_mut(|current| current.replace(tools.clone()));
    let _scope = Scope(old);
    if tools.language == Language::Rust {
        let api = toml::Value::String(api.to_str().ok_or("invalid SDK path")?.into());
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname='playbit-sdk-probe'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='probe.rs'\ncrate-type=['cdylib']\n[dependencies]\nplaybit-api={{path={api}, features=['guest']}}\n[workspace]\n"
            ),
        )?;
        fs::write(root.join("probe.rs"), RUST_PROBE)?;
        let mut command = command("cargo");
        command
            .args(["build", "--offline", "--target", "wasm32-unknown-unknown"])
            .current_dir(&root);
        crate::developer::execute(command)?;
    } else {
        ocaml::compile(
            &root,
            &api,
            "let () = Playbit.run (fun () -> Playbit.log \"SDK probe\")\n",
            false,
        )?;
    }
    atomic(&root.join("ok"), b"1")?;
    Ok(())
}
fn hash_tree(root: &Path, hash: &mut blake3::Hasher) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            count += 1;
            if count > 4096 {
                return Err("SDK API exceeds file limit".into());
            }
            let path = entry.path();
            hash.update(path.strip_prefix(root)?.as_os_str().as_encoded_bytes());
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if entry.file_type()?.is_file() {
                hash.update(&read(&path, 4 * 1024 * 1024)?);
            } else {
                return Err("SDK API contains a link or special file".into());
            }
        }
    }
    Ok(())
}
pub(super) fn build_ocaml(directory: &Path, release: bool) -> Result<()> {
    ocaml::build(directory, &resources()?.join("ocaml-sdk"), release)
}
pub(super) fn update(args: &[String]) -> Result<serde_json::Value> {
    if args.len() != 2 || args[0] != "update" {
        return Err(crate::usage_error("expected pb sdk update ocaml|rust"));
    }
    let language = language(&args[1]).map_err(crate::usage_error)?;
    let tools = managed(language, true)?;
    Ok(serde_json::json!({"language":name(language), "sdk":tools.identity}))
}
