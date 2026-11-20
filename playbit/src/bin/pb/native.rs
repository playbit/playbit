//! SDK discovery and authenticated paired Wasm packaging for native Cargo builds.
pub(super) mod bundle;
mod package;
use playbit::engine::{Request, Result, reply::Reply};
use std::path::{Path, PathBuf};

pub(super) fn sdk_path(executable: &Path) -> Result<PathBuf> {
    let directory = executable.parent().ok_or("missing installation directory")?;
    let sdk = directory.parent().ok_or("missing app contents")?.join("Resources/native-sdk");
    if directory.file_name().is_none_or(|name| name != "MacOS")
        || !sdk.join("include/playbit/mac.h").is_file()
        || !sdk.join("aarch64-apple-darwin/libplaybit.a").is_file()
        || !sdk.join("aarch64-apple-darwin/link.json").is_file()
    {
        return Err("native SDK is missing; use pb from a complete Playbit.app installation".into());
    }
    Ok(sdk.canonicalize()?)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    target: String,
    minimum_os: String,
    archive: String,
    link_args: Vec<String>,
}
fn cargo_path(path: &Path) -> Result<&str> {
    let text = path.to_str().ok_or("native build paths must be UTF-8")?;
    if text.len() > 4096 || text.contains(['\n', '\r']) {
        return Err("native build path exceeds 4096 bytes or contains a line break".into());
    }
    Ok(text)
}
/// Prepare native SDK inputs before Cargo starts; no application build script.
pub(super) fn build(
    source: &Path,
    manifest: &playbit::package::Manifest,
    command: &str,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<u128> {
    let started = std::time::Instant::now();
    bundle::supported()?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let _api = crate::project::prepare_sdk(source, &executable)?;
    let sdk = match std::env::var_os("PLAYBIT_NATIVE_SDK") {
        Some(path) => PathBuf::from(path).canonicalize()?,
        None => sdk_path(&executable)?,
    };
    let output = playbit::package::metadata::generate(source, manifest)?;
    package::build(source, manifest, &output, call)?;
    configure(manifest, &output, &sdk)?;
    let mut child = std::process::Command::new("sh");
    child.arg("-c").arg(command).current_dir(source);
    crate::developer::execute(child)?;
    Ok(started.elapsed().as_micros())
}
fn configure(manifest: &playbit::package::Manifest, output: &Path, sdk: &Path) -> Result<String> {
    use std::{fmt::Write, io::Read};
    let package = playbit::package::Package::open(&output.join("native-app.pbapp"))?;
    if manifest.name.contains('\0') {
        return Err("native app name contains NUL".into());
    }
    let directory = sdk.join("aarch64-apple-darwin");
    let link = directory.join("link.json");
    let archive = directory.join("libplaybit.a");
    let mut bytes = Vec::new();
    std::fs::File::open(&link)?.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err("native SDK link metadata exceeds 8192 bytes".into());
    }
    let metadata: Link = serde_json::from_slice(&bytes)?;
    let archive_info = std::fs::metadata(&archive)?;
    if metadata.target != "aarch64-apple-darwin"
        || metadata.archive != "libplaybit.a"
        || metadata.minimum_os != env!("MACOSX_DEPLOYMENT_TARGET")
        || metadata.link_args.len() > 64
        || !archive_info.is_file()
        || archive_info.len() > 64 * 1024 * 1024
    {
        return Err("unsupported or incomplete native Mac SDK".into());
    }
    let mut directives = String::new();
    // Cargo passes this response file only to the final native binary. It does
    // not replace user RUSTFLAGS or apply the SDK to dependency/Wasm builds.
    let rustc_args = format!("-Clink-arg={}\n", cargo_path(&archive)?);
    let mut args = metadata.link_args.iter();
    while let Some(arg) = args.next() {
        let (kind, name) = if arg == "-framework" {
            ("framework", args.next().ok_or("missing SDK framework name")?.as_str())
        } else {
            ("dylib", arg.strip_prefix("-l").ok_or("unsupported SDK link argument")?)
        };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err("invalid SDK library name".into());
        }
        writeln!(
            directives,
            "#[allow(unsafe_code)]\n#[link(name = {name:?}, kind = {kind:?})]\nunsafe extern \"C\" {{}}"
        )?;
    }
    if directives.len() > 32768 {
        return Err("native build directives exceed 32 KiB".into());
    }
    // A replaced SDK at the same installation path must invalidate Cargo's
    // generated input even when its link metadata has not changed.
    let mut sdk_hash = blake3::Hasher::new();
    let count = std::io::copy(
        &mut std::fs::File::open(&archive)?.take(64 * 1024 * 1024 + 1),
        &mut sdk_hash,
    )?;
    if count > 64 * 1024 * 1024 {
        return Err("native SDK archive exceeds 64 MiB".into());
    }
    let mut config = format!("const APP_NAME: &str = {:?};\n", manifest.name);
    config.push_str(&format!("#[used]\n#[allow(unsafe_code)]\n#[unsafe(link_section = \"__TEXT,__pbbundle\")]\nstatic PACKAGE_DIGEST: [u8; 32] = {:?};\n", package.digest()));
    config.push_str(&format!("const _: [u8; 32] = {:?};\n", sdk_hash.finalize().as_bytes()));
    config.push_str(&directives);
    playbit::package::metadata::write_changed(&output.join("native-app.rs"), config.as_bytes())?;
    playbit::package::metadata::write_changed(&output.join("native.rsp"), rustc_args.as_bytes())?;
    Ok(directives)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[test]
    fn sdk_discovery_requires_a_complete_installation() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("Playbit.app/Contents/MacOS/pb");
        let sdk = root.path().join("Playbit.app/Contents/Resources/native-sdk");
        assert!(sdk_path(&executable).is_err());
        for path in [
            "include/playbit/mac.h",
            "aarch64-apple-darwin/libplaybit.a",
            "aarch64-apple-darwin/link.json",
        ] {
            let path = sdk.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        assert_eq!(sdk_path(&executable).unwrap(), sdk.canonicalize().unwrap());
        assert!(sdk_path(&root.path().join("pb")).is_err());
        std::fs::remove_file(sdk.join("aarch64-apple-darwin/libplaybit.a")).unwrap();
        assert!(sdk_path(&executable).is_err());
    }
    #[test]
    fn cargo_configuration_is_bounded_and_failures_preserve_generated_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path();
        let directory = source.join("aarch64-apple-darwin");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            source.join("playbit.toml"),
            "app_id='test'\nname= 'Native \"quoted\" 🐚'\nbuild_cmd = 'cargo build'\n",
        )
        .unwrap();
        std::fs::write(
            source.join(playbit::package::VOLUME_INITIALIZERS_FILE),
            include_bytes!("../../../../guests/todo/playbit-volumes.json"),
        )
        .unwrap();
        std::fs::write(directory.join("libplaybit.a"), b"fixture").unwrap();
        let good = serde_json::json!({"target":"aarch64-apple-darwin", "minimum_os":env!("MACOSX_DEPLOYMENT_TARGET"), "archive":"libplaybit.a", "link_args":["-framework","AppKit","-liconv"]});
        let link = directory.join("link.json");
        std::fs::write(&link, serde_json::to_vec(&good).unwrap()).unwrap();
        std::fs::write(
            source.join("native-app.pbapp"),
            playbit::package::Package::new(
                "test".into(),
                "Native \"quoted\" 🐚".into(),
                b"\0asm\x01\0\0\0".to_vec(),
                Default::default(),
            )
            .unwrap()
            .to_bytes()
            .unwrap(),
        )
        .unwrap();
        let manifest = playbit::package::Manifest::read(source).unwrap();
        let configure = || super::configure(&manifest, source, source);
        let directives = configure().unwrap();

        assert!(directives.contains("kind = \"framework\""));
        assert!(directives.contains("name = \"iconv\""));
        assert_eq!(
            std::fs::read_to_string(source.join("native.rsp")).unwrap(),
            format!("-Clink-arg={}\n", directory.join("libplaybit.a").display())
        );
        let generated = std::fs::read_to_string(source.join("native-app.rs")).unwrap();
        assert!(generated.contains(&format!("{:?}", "Native \"quoted\" 🐚")));
        assert!(!generated.contains("build_cmd"));
        let mut invalid = good.clone();
        invalid["minimum_os"] = serde_json::json!("99.0");
        std::fs::write(&link, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(configure().is_err());
        assert_eq!(std::fs::read_to_string(source.join("native-app.rs")).unwrap(), generated);
        for args in [
            serde_json::json!(["-framework"]),
            serde_json::json!(["-framework", "AppKit\ncargo:bad"]),
            serde_json::json!(["-Wl,-bad"]),
            serde_json::json!(vec!["-liconv"; 65]),
        ] {
            let mut invalid = good.clone();
            invalid["link_args"] = args;
            std::fs::write(&link, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(configure().is_err());
            assert_eq!(std::fs::read_to_string(source.join("native-app.rs")).unwrap(), generated);
        }
        std::fs::write(&link, vec![b' '; 8193]).unwrap();
        assert!(configure().unwrap_err().to_string().contains("exceeds 8192 bytes"));
        std::fs::write(&link, serde_json::to_vec(&good).unwrap()).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(directory.join("libplaybit.a"))
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        assert!(configure().is_err());
        assert_eq!(std::fs::read_to_string(source.join("native-app.rs")).unwrap(), generated);
        for path in ["/tmp/line\nbreak".to_owned(), "/".repeat(4097)] {
            assert!(cargo_path(Path::new(&path)).is_err());
        }
    }
}
