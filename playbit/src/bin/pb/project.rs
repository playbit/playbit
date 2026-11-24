//! Copies the installed project template. Checks layout, size and SDK paths before writing.
use playbit::engine::Result;
use playbit::package::{BuildCommand, Manifest, NativeBuild};
use playbit::store::Id;
mod initialization;
pub(super) use initialization::Initialization;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_TEMPLATE_BYTES: usize = 512 * 1024;
const MAX_TEMPLATE_ENTRIES: usize = 24;
const MAX_TEMPLATE_DEPTH: usize = 4;

pub(super) fn resources(executable: &Path) -> Result<PathBuf> {
    let executable = executable.canonicalize()?;
    let directory = executable.parent().ok_or("missing installation directory")?;
    if !directory.ends_with("Contents/MacOS") {
        return Err("use pb from a complete Playbit.app installation".into());
    }
    Ok(directory.parent().ok_or("missing app contents")?.join("Resources"))
}

fn template(
    name: &str,
    app_id: &str,
    executable: &Path,
    language: playbit::package::Language,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    if name.trim().is_empty() || name.len() > 256 {
        return Err("name must contain 1 to 256 bytes".into());
    }
    let resources = resources(executable)?;
    let ocaml = language == playbit::package::Language::Ocaml;
    let mut files =
        read_template(&resources.join(if ocaml { "ocaml-template" } else { "project-template" }))?;
    if !ocaml {
        let cargo =
            files.get_mut(Path::new("Cargo.toml")).ok_or("template is missing Cargo.toml")?;
        *cargo = cargo_manifest(cargo, &resources)?.into_bytes();
    }
    for required in ["playbit.toml", "playbit-volumes.json", ".gitignore"] {
        if !files.contains_key(Path::new(required)) {
            return Err(format!("project template is missing {required}").into());
        }
    }
    let source = if ocaml { "src/app.ml" } else { "src/app.rs" };
    if !files.contains_key(Path::new(source)) {
        return Err(format!("project template is missing {source}").into());
    }
    let mut manifest =
        Manifest::from_source(std::str::from_utf8(&files[Path::new("playbit.toml")])?)?;
    manifest.name = name.into();
    if !playbit::package::valid_app_id(app_id) {
        return Err("invalid app_id: provide --app-id with 1–64 lowercase ASCII letters/digits in nonempty components separated by dots or hyphens".into());
    }
    manifest.app_id = app_id.into();
    manifest.lang = Some(language);
    manifest.icon.get_or_insert_with(|| "icon.svg".into());
    if !ocaml {
        manifest.build_cmd = "cargo rustc --lib --crate-type cdylib".into();
        manifest.release = BuildCommand {
            build_cmd: "cargo rustc --release --lib --crate-type cdylib".into(),
            build_artifact: None,
        };
        manifest.native = Some(NativeBuild {
        build_cmd: "cargo rustc --bin playbit-app --features native --target aarch64-apple-darwin -- @target/playbit/native.rsp".into(),
        release: BuildCommand {
            build_cmd: "cargo rustc --release --bin playbit-app --features native --target aarch64-apple-darwin -- @target/playbit/native.rsp".into(),
            build_artifact: None,
        },
    });
    }
    files.insert("playbit.toml".into(), toml::to_string_pretty(&manifest)?.into_bytes());
    let readme = project_readme(&resources.with_file_name("MacOS"));
    files.insert("README.md".into(), readme.into_bytes());
    if files.values().map(Vec::len).sum::<usize>() > MAX_TEMPLATE_BYTES {
        return Err("configured project template exceeds 512 KiB".into());
    }
    Ok(files)
}

fn project_readme(bin_directory: &Path) -> String {
    let path = bin_directory
        .to_str()
        .filter(|path| !path.contains(':') && !path.chars().any(char::is_control))
        .unwrap_or("/Applications/Playbit.app/Contents/MacOS");
    let path = path.replace('\'', "'\\''");
    format!(
        r#"# Playbit project

## Set up your terminal

Add the `Contents/MacOS` folder inside your copy of Playbit to `PATH`. Run this
command in each new terminal, or add it to `~/.zshrc` for future zsh sessions.

```sh
export PATH='{path}':"$PATH"
```

Update the path if you move Playbit or use a different copy. Run `pb --version`
to check that your terminal can find `pb`.

## Run your app

From the project folder containing `playbit.toml`, run the following command.

```sh
pb run
```

This builds your app and opens a new Playbit session. Keep the command running
while editing your source files. Playbit rebuilds and updates the app when you save.

Press Ctrl-C to stop watching. To resume working in the same session, find its ID
with `pb session list` and run `pb run --session SESSION_ID` from this folder.
"#
    )
}

fn read_template(template: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    if !fs::symlink_metadata(template).is_ok_and(|info| info.is_dir()) {
        return Err("project template is missing; use a complete Playbit.app installation".into());
    }
    let mut files = BTreeMap::new();
    let mut pending = vec![(template.to_path_buf(), 0)];
    let mut entries = 1; // The CLI adds README.md to the template.
    let mut size = 0;
    while let Some((directory, depth)) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries += 1;
            if entries > MAX_TEMPLATE_ENTRIES {
                return Err("project template exceeds 24 entries".into());
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() && depth < MAX_TEMPLATE_DEPTH {
                pending.push((path, depth + 1));
            } else if kind.is_file() {
                let mut bytes = Vec::new();
                fs::File::open(&path)?
                    .take((MAX_TEMPLATE_BYTES - size + 1) as u64)
                    .read_to_end(&mut bytes)?;
                size += bytes.len();
                if size > MAX_TEMPLATE_BYTES {
                    return Err("project template exceeds 512 KiB".into());
                }
                files.insert(path.strip_prefix(template)?.to_path_buf(), bytes);
            } else {
                return Err(
                    "project template contains a link, special file or excessive depth".into()
                );
            }
        }
    }
    Ok(files)
}

fn cargo_manifest(bytes: &[u8], resources: &Path) -> Result<String> {
    let mut text = std::str::from_utf8(bytes)?.to_owned();
    for (token, path) in [
        ("@PLAYBIT_API@", resources.join("playbit-api")),
        ("@PLAYBIT_NATIVE_MAIN@", resources.join("native-sdk/rust/main.rs")),
    ] {
        let required =
            if token == "@PLAYBIT_API@" { path.join("Cargo.toml") } else { path.clone() };
        if !required.is_file() {
            return Err(format!("installed SDK is missing {}", required.display()).into());
        }
        let path = if token == "@PLAYBIT_API@" {
            "target/playbit/sdk/playbit-api"
        } else {
            "target/playbit/sdk/native-main.rs"
        };
        let token = format!("\"{token}\"");
        if text.matches(&token).count() != 1 {
            return Err(format!("project template must contain exactly one {token}").into());
        }
        text = text.replace(&token, &toml::Value::String(path.into()).to_string());
    }
    let _: toml::Value = toml::from_str(&text)?;
    Ok(text)
}

#[cfg(test)]
pub(super) fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temporary installation");
    let resources = root.path().join("Playbit.app/Contents/Resources");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("platform/macos/developer-resources.py");
    let status = std::process::Command::new("python3")
        .arg(script)
        .arg(&resources)
        .arg(env!("CARGO_PKG_VERSION"))
        .arg("todo")
        .status()
        .expect("stage developer resources");
    assert!(status.success());
    let executable = root.path().join("Playbit.app/Contents/MacOS/pb");
    fs::create_dir_all(executable.parent().expect("executable parent")).expect("MacOS directory");
    fs::write(executable, b"fixture").expect("fixture executable");
    root
}

#[cfg(test)]
pub(super) fn scaffold(directory: &Path, name: &str, executable: &Path) -> Result<()> {
    let mut init = Initialization::begin(directory, Some(name), executable, directory)?;
    init.scaffold()?;
    prepare_sdk(directory, executable).map(|_| ())
}

/// Refresh only pb's generated, source-relative SDK copy. Projects survive moving the app.
pub(super) fn prepare_sdk(directory: &Path, executable: &Path) -> Result<Option<fs::File>> {
    let cargo = directory.join("Cargo.toml");
    if !cargo.is_file() {
        return Ok(None);
    }
    let mut source = String::new();
    fs::File::open(&cargo)?.take(1024 * 1024 + 1).read_to_string(&mut source)?;
    if source.len() > 1024 * 1024 {
        return Err("Cargo manifest exceeds 1 MiB".into());
    }
    let value: toml::Value = toml::from_str(&source)?;
    if value
        .get("dependencies")
        .and_then(|d| d.get("playbit-api"))
        .and_then(|d| d.get("path"))
        .and_then(toml::Value::as_str)
        != Some("target/playbit/sdk/playbit-api")
    {
        return Ok(None);
    }
    let resources = resources(executable)?;
    let api = resources.join("playbit-api");
    let mut files = BTreeMap::new();
    let mut pending = vec![api.clone()];
    let mut total = 0;
    while let Some(parent) = pending.pop() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if entry.file_type()?.is_file() {
                let mut bytes = Vec::new();
                fs::File::open(&path)?.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                total += bytes.len();
                if total > 4 * 1024 * 1024 || files.len() >= 512 {
                    return Err("installed SDK exceeds file/byte limits".into());
                }
                files.insert(Path::new("playbit-api").join(path.strip_prefix(&api)?), bytes);
            } else {
                return Err("installed SDK contains a link or special file".into());
            }
        }
    }
    files.insert("native-main.rs".into(), fs::read(resources.join("native-sdk/rust/main.rs"))?);
    let mut digest = blake3::Hasher::new();
    for (path, bytes) in &files {
        digest.update(path.as_os_str().as_encoded_bytes());
        digest.update(bytes);
    }
    let target = directory.join("target/playbit/sdk");
    for relative in ["target", "target/playbit", "target/playbit/sdk"] {
        let path = directory.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(info) if !info.is_dir() || info.is_symlink() => {
                return Err("generated SDK directory must not be a link or file".into());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(e) => return Err(e.into()),
        }
    }
    let lock_path = target.join("lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|info| !info.is_file() || info.is_symlink()) {
        return Err("generated SDK lock must be a regular file".into());
    }
    let lock =
        fs::File::options().read(true).write(true).create(true).truncate(false).open(lock_path)?;
    lock.lock()?;
    if fs::read(target.join("identity")).ok().as_deref()
        == Some(digest.finalize().as_bytes().as_slice())
    {
        return Ok(Some(lock));
    }
    let old_api = target.join("playbit-api");
    if let Ok(info) = fs::symlink_metadata(&old_api) {
        if !info.is_dir() || info.is_symlink() {
            return Err("generated SDK API must be a directory".into());
        }
        fs::remove_dir_all(&old_api)?;
    }
    for (path, bytes) in files {
        let mut parent = target.clone();
        for component in path.parent().ok_or("missing SDK parent")?.components() {
            parent.push(component);
            match fs::symlink_metadata(&parent) {
                Ok(info) if !info.is_dir() || info.is_symlink() => {
                    return Err("generated SDK parent must not be a link or file".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&parent)?
                }
                Err(error) => return Err(error.into()),
            }
        }
        let target = target.join(path);
        if fs::symlink_metadata(&target).is_ok_and(|info| info.is_symlink()) {
            return Err("generated SDK file is a link".into());
        }
        fs::write(target, bytes)?;
    }
    fs::write(target.join("identity"), digest.finalize().as_bytes())?;
    Ok(Some(lock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incomplete_or_oversized_templates_before_writing() {
        // Each invalid installation must leave the caller's empty directory untouched.
        let install = fixture();
        let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
        let template = resources(&executable).expect("resources").join("project-template");
        let source = tempfile::tempdir().expect("source");
        fs::write(template.join("oversized"), vec![0; MAX_TEMPLATE_BYTES + 1]).expect("fixture");
        assert!(scaffold(source.path(), "Test", &executable).is_err());
        fs::remove_file(template.join("oversized")).expect("remove fixture");
        for i in 0..MAX_TEMPLATE_ENTRIES {
            fs::write(template.join(format!("extra-{i}")), b"").expect("fixture");
        }
        assert!(scaffold(source.path(), "Test", &executable).is_err());
        for i in 0..MAX_TEMPLATE_ENTRIES {
            fs::remove_file(template.join(format!("extra-{i}"))).expect("remove fixture");
        }
        fs::remove_file(template.join("playbit.toml")).expect("remove declaration");
        assert!(scaffold(source.path(), "Test", &executable).is_err());
        assert_eq!(fs::read_dir(source.path()).expect("source").count(), 0);
    }

    #[test]
    fn installed_paths_are_escaped_and_missing_sdk_leaves_source_empty() {
        // Exercise shell metacharacters and Unicode in the actual installation path.
        let install = fixture();
        let app = install.path().join("Playbit 'quoted' \"$HOME\" \\ `exit 88` 🐚.app");
        fs::rename(install.path().join("Playbit.app"), &app).expect("move installation");
        let executable = app.join("Contents/MacOS/pb");
        let source = tempfile::tempdir().expect("source");
        scaffold(source.path(), "Test", &executable).expect("configure escaped paths");
        let cargo = fs::read_to_string(source.path().join("Cargo.toml")).expect("manifest");
        let cargo: toml::Value = toml::from_str(&cargo).expect("valid TOML");
        assert_eq!(
            cargo["dependencies"]["playbit-api"]["path"].as_str(),
            Some("target/playbit/sdk/playbit-api")
        );
        let readme = fs::read_to_string(source.path().join("README.md")).expect("README");
        let setup =
            readme.lines().find(|line| line.starts_with("export PATH=")).expect("setup command");
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("{setup}\nprintf '%s' \"$PATH\"")])
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("run README command");
        assert!(output.status.success());
        let bin = app.join("Contents/MacOS").canonicalize().expect("actual bin directory");
        assert_eq!(output.stdout, format!("{}:/usr/bin:/bin", bin.display()).into_bytes());
        fs::remove_file(app.join("Contents/Resources/playbit-api/Cargo.toml")).expect("remove SDK");
        let source = tempfile::tempdir().expect("source");
        assert!(scaffold(source.path(), "Test", &executable).is_err());
        assert_eq!(fs::read_dir(source.path()).expect("source").count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_template_symlinks_and_resolves_cli_symlinks() {
        // Executable symlinks select the real installation; template links cannot copy outside it.
        let install = fixture();
        let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
        let link = install.path().join("pb");
        std::os::unix::fs::symlink(&executable, &link).expect("CLI link");
        assert_eq!(resources(&link).expect("linked CLI"), resources(&executable).expect("CLI"));
        let template = resources(&executable).expect("resources").join("project-template");
        std::os::unix::fs::symlink(&executable, template.join("outside")).expect("template link");
        let source = tempfile::tempdir().expect("source");
        assert!(scaffold(source.path(), "Test", &executable).is_err());
        assert_eq!(fs::read_dir(source.path()).expect("source").count(), 0);
    }
}
