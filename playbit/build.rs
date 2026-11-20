use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(not(test))]
#[path = "../scripts/build-version.rs"]
mod build_version;

#[cfg(not(test))]
const MAX_RELEASE_SEQUENCE: u64 = 9_007_199_254_740_991;

fn source_files(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            source_files(&path, files)?;
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// Deterministic across checkout paths and enumeration order. Hash file names
/// and lengths too, so renames and file boundaries cannot alias concatenations.
pub fn source_identity(root: &Path, inputs: &[&str]) -> io::Result<[u8; 16]> {
    let mut files = Vec::new();
    for input in inputs {
        let path = root.join(input);
        if path.is_dir() {
            source_files(&path, &mut files)?;
        } else {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    let mut hash = blake3::Hasher::new();
    hash.update(b"playbit-local-ipc-source-v1\0");
    for path in files {
        let name = path
            .strip_prefix(root)
            .expect("input is within source root")
            .to_str()
            .ok_or_else(|| io::Error::other("source path is not UTF-8"))?;
        let bytes = fs::read(&path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(hash.finalize().as_bytes()[..16].try_into().expect("16-byte identity"))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelEndpoints {
    ap_hostname: String,
    website: String,
    macos_manifest: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Channels {
    public: ChannelEndpoints,
    testing: ChannelEndpoints,
}

pub fn channel_endpoints(root: &Path, output: &Path) -> io::Result<()> {
    use std::io::Read as _;
    let file = fs::File::open(root.join("playbit/src/channels.json"))?;
    let mut bytes = Vec::with_capacity(4097);
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("runtime channel definitions exceed 4 KiB"));
    }
    let channels: Channels = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let hostname = |value: &str| {
        !value.is_empty()
            && value.len() <= 253
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
            })
    };
    if channels.public.ap_hostname == channels.testing.ap_hostname
        || channels.public.website == channels.testing.website
    {
        return Err(io::Error::other("runtime channels require separate AP and website origins"));
    }
    let mut constants = String::new();
    use std::fmt::Write as _;
    for (name, endpoints, filename) in [
        ("PUBLIC", &channels.public, "macos.json"),
        ("TESTING", &channels.testing, "macos-testing.json"),
    ] {
        if !hostname(&endpoints.ap_hostname)
            || !endpoints.website.strip_prefix("https://").is_some_and(hostname)
            || endpoints.macos_manifest
                != format!("{}/download/{filename}", channels.public.website)
        {
            return Err(io::Error::other("invalid runtime channel endpoints"));
        }
        for (field, value) in [
            ("AP_HOSTNAME", &endpoints.ap_hostname),
            ("WEBSITE", &endpoints.website),
            ("MACOS_MANIFEST", &endpoints.macos_manifest),
        ] {
            writeln!(constants, "pub const {name}_{field}: &str = {value:?};")
                .expect("writing channel constants to a String");
        }
    }
    fs::write(output.join("release_channel_endpoints.rs"), constants)
}

#[cfg(not(test))]
fn main() -> io::Result<()> {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let root = manifest.parent().expect("workspace root");
    let version = build_version::emit(root)?;
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    channel_endpoints(root, &output)?;
    println!("cargo:rerun-if-env-changed=PLAYBIT_RELEASE_SEQUENCE");
    let sequence = match std::env::var("PLAYBIT_RELEASE_SEQUENCE") {
        Err(std::env::VarError::NotPresent) => 0,
        Ok(value) => {
            let sequence = value.parse::<u64>().map_err(io::Error::other)?;
            if !(1..=MAX_RELEASE_SEQUENCE).contains(&sequence) || sequence.to_string() != value {
                return Err(io::Error::other(
                    "release sequence must be a canonical positive safe JSON integer",
                ));
            }
            sequence
        }
        Err(error) => return Err(io::Error::other(error)),
    };
    fs::write(output.join("release_sequence.rs"), format!("{sequence}u64\n"))?;
    println!("cargo:rerun-if-env-changed=PLAYBIT_KEYCHAIN_ACCESS_GROUP");
    let group = match std::env::var("PLAYBIT_KEYCHAIN_ACCESS_GROUP") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(error) => return Err(io::Error::other(error)),
    };
    if !group.is_empty() && std::env::var("PROFILE").as_deref() != Ok("release") {
        return Err(io::Error::other(
            "Keychain backup requires a provisioned distribution release",
        ));
    }
    if group.len() > 255
        || !group.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(io::Error::other("invalid compiled Keychain access group"));
    }
    println!("cargo:rustc-env=PLAYBIT_KEYCHAIN_ACCESS_GROUP={group}");
    // Include all client Rust sources rather than maintain a second list of
    // archived types. Unrelated source edits intentionally change the identity.
    let inputs = [
        "playbit/src",
        "playbit/build.rs",
        "playbit/src/channels.json",
        "scripts/build-version.rs",
        "playbit/Cargo.toml",
        "playbit-api/src",
        "playbit-api/Cargo.toml",
        "playbit/kvdb/src",
        "playbit/kvdb/Cargo.toml",
        "Cargo.toml",
        "Cargo.lock",
    ];
    for input in inputs {
        println!("cargo:rerun-if-changed={}", root.join(input).display());
    }
    let identity = source_identity(root, &inputs)?;
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    fs::write(
        output.join("ipc_identity.rs"),
        format!("pub const BUILD_ID: [u8; 16] = {identity:?};\n"),
    )?;
    if std::env::var_os("CARGO_FEATURE_IOS").is_some() {
        // Package this beside the static library so Xcode uses its exact version.
        let profile = output.ancestors().nth(3).expect("Cargo profile output directory");
        let release = std::env::var("CARGO_PKG_VERSION").expect("Cargo package version");
        let numeric = release.split(['-', '+']).next().expect("version has a numeric core");
        fs::write(
            profile.join("playbit-version.xcconfig"),
            format!(
                "// Generated by the Rust library build.\nMARKETING_VERSION = {numeric}\nPLAYBIT_VERSION = {version}\n"
            ),
        )?;
    }
    Ok(())
}
