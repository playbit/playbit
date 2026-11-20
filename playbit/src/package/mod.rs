//! Deterministic distributable app content and the source-only playbit.toml.
//! Admit bounded headers and indexes before reading payloads. Full bundle
//! decoding requires canonical bytes; display-only readers confer no authority.
//! Package work belongs to loading/building, outside interactive rendering.
#![deny(clippy::unwrap_used)]
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Read, path::Path};

#[cfg(feature = "artwork")]
pub mod artwork;

pub mod codec;
pub mod identity;
pub mod initialization;
mod json;
pub use identity::{
    AppIdentity, AutomaticVolumeRole, LocalPrincipal, MAX_APP_ID_BYTES, effective_app_id,
    valid_app_id,
};
pub use initialization::VolumeInitializers;
#[cfg(any(
    target_arch = "wasm32",
    feature = "wasm-runtime",
    feature = "native-runtime",
    feature = "connected-engine",
    feature = "desktop-ui"
))]
pub mod filesystem;
pub mod metadata;
pub mod signature;
pub mod trace_catalog;
pub use codec::Package;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const MANIFEST_FILE: &str = "playbit.toml";
pub const MAX_BUNDLE_BYTES: usize = 512 * 1024 * 1024;
// Settings (64 KiB) and optional trace declarations (256 KiB).
const MAX_MANIFEST_BYTES: usize = 320 * 1024;
pub const VOLUME_INITIALIZERS_FILE: &str = "playbit-volumes.json";
const MAX_NAME_BYTES: usize = 256;
const MAX_BUILD_ARTIFACT_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_host_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<Language>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<trace_catalog::TraceCatalog>,
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_dir: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build_cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_artifact: Option<String>,
    #[serde(default)]
    pub release: BuildCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeBuild>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildCommand {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build_cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_artifact: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBuild {
    pub build_cmd: String,
    pub release: BuildCommand,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Ocaml,
}

#[derive(Clone, Copy)]
pub struct BuildMode {
    pub release: bool,
    pub native: bool,
}
fn release_command() -> BuildCommand {
    BuildCommand { build_cmd: "cargo build --release".into(), build_artifact: None }
}
impl Manifest {
    pub fn read(directory: &Path) -> Result<Self> {
        let mut source = String::new();
        fs::File::open(directory.join(MANIFEST_FILE))?
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_string(&mut source)?;
        Self::from_source(&source)
    }
    /// Parse the same bounded source document used by the filesystem entry point.
    // fuzz: package_contract
    pub fn from_source(source: &str) -> Result<Self> {
        if source.len() > MAX_MANIFEST_BYTES {
            return Err("source manifest exceeds 320 KiB".into());
        }
        let document: toml::Value = toml::from_str(source)?;
        let release_omitted = document.get("release").is_none();
        let mut value: Self = document.try_into()?;
        if value.lang == Some(Language::Ocaml) {
            if !value.build_cmd.is_empty()
                || !value.release.build_cmd.is_empty()
                || value.build_artifact.is_some()
                || value.release.build_artifact.is_some()
                || value.native.is_some()
                || value.trace.is_some()
            {
                return Err("managed OCaml projects cannot declare build commands, artifacts, native builds or trace declarations".into());
            }
        } else if release_omitted {
            value.release = release_command();
        }
        if !valid_app_id(&value.app_id)
            || value.name.trim().is_empty()
            || value.name.len() > MAX_NAME_BYTES
            || (value.lang != Some(Language::Ocaml)
                && (value.build_cmd.trim().is_empty() || value.release.build_cmd.trim().is_empty()))
            || value.build_cmd.len() > 8192
            || value.release.build_cmd.len() > 8192
            || value.native.as_ref().is_some_and(|native| {
                native.build_cmd.trim().is_empty()
                    || native.build_cmd.len() > 8192
                    || native.release.build_cmd.trim().is_empty()
                    || native.release.build_cmd.len() > 8192
            })
        {
            return Err("invalid source name or build command".into());
        }
        if let Some(trace) = &value.trace {
            trace.validate()?;
        }
        codec::validate_required_features(&value.required_host_features)?;
        value.validate_build_artifacts()?;
        Ok(value)
    }

    fn validate_build_artifacts(&self) -> Result<()> {
        if self.build_artifact.is_some() != self.release.build_artifact.is_some() {
            return Err("build_artifact requires explicit debug and [release] paths".into());
        }
        for path in [&self.build_artifact, &self.release.build_artifact].into_iter().flatten() {
            let path = Path::new(path);
            if path.as_os_str().len() > MAX_BUILD_ARTIFACT_BYTES
                || path.extension().is_none_or(|extension| extension != "wasm")
                || !path.components().all(|part| matches!(part, std::path::Component::Normal(_)))
            {
                return Err(
                    "build_artifact must be a relative .wasm path inside the source directory"
                        .into(),
                );
            }
        }
        if self.native.as_ref().is_some_and(|native| native.release.build_artifact.is_some()) {
            return Err("native builds use Cargo artifact discovery".into());
        }
        Ok(())
    }

    pub fn wasm_artifact(&self, release: bool) -> Option<&str> {
        if release {
            self.release.build_artifact.as_deref()
        } else {
            self.build_artifact.as_deref()
        }
    }
    pub fn build_command(&self, mode: BuildMode) -> Result<&str> {
        if mode.native {
            let native = self.native.as_ref().ok_or(
                "--native requires [native] and [native.release] build_cmd in playbit.toml",
            )?;
            Ok(if mode.release { &native.release.build_cmd } else { &native.build_cmd })
        } else {
            Ok(if mode.release { &self.release.build_cmd } else { &self.build_cmd })
        }
    }
    pub fn write(&self, directory: &Path) -> Result<()> {
        fs::write(directory.join(MANIFEST_FILE), toml::to_string_pretty(self)?)?;
        Ok(())
    }
    pub fn bundle(&self, directory: &Path, wasm: Vec<u8>) -> Result<Package> {
        let sources = self.sources(directory, wasm)?;
        let mut bytes = Vec::new();
        codec::write(&sources, &mut bytes)?;
        Package::from_bytes(&bytes)
    }
    pub fn write_bundle(
        &self,
        directory: &Path,
        wasm: Vec<u8>,
        output: &mut impl std::io::Write,
    ) -> Result<()> {
        codec::write(&self.sources(directory, wasm)?, output)
    }
    pub fn signed_bundle(
        &self,
        directory: &Path,
        wasm: Vec<u8>,
        signer: &signature::PackageSigner,
        signed_at: u64,
    ) -> Result<Package> {
        let mut bytes = Vec::new();
        self.write_signed_bundle(directory, wasm, signer, signed_at, &mut bytes)?;
        Package::from_bytes(&bytes)
    }
    pub fn write_signed_bundle(
        &self,
        directory: &Path,
        wasm: Vec<u8>,
        signer: &signature::PackageSigner,
        signed_at: u64,
        output: &mut impl std::io::Write,
    ) -> Result<()> {
        codec::write_signed(self.sources(directory, wasm)?, signer, signed_at, output)
    }
    fn sources(&self, directory: &Path, wasm: Vec<u8>) -> Result<BTreeMap<String, codec::Source>> {
        metadata::check_wasm(self, &wasm)?;
        let initializers = read_volume_initializers(directory)?;
        let mut sources = BTreeMap::from([
            (
                codec::MANIFEST.into(),
                codec::Source::Bytes(codec::manifest_bytes(
                    &self.app_id,
                    &self.name,
                    &self.required_host_features,
                )?),
            ),
            (codec::EXECUTABLE.into(), codec::Source::Bytes(wasm)),
            (codec::INITIALIZERS.into(), codec::Source::Bytes(initializers.to_json()?)),
        ]);
        if let Some(trace) = &self.trace {
            sources.insert(
                trace_catalog::FILE.into(),
                codec::Source::Bytes(json::encode(trace, trace_catalog::MAX_BYTES)?),
            );
        }
        if let Some(icon) = &self.icon {
            let file = source_file(directory, icon)?;
            if file.metadata()?.len() > codec::MAX_ICON_BYTES as u64 {
                return Err("icon exceeds 1 MiB".into());
            }
            sources.insert(codec::ICON.into(), codec::Source::File(file));
        }
        for (path, source) in &self.files {
            add_source(&mut sources, path, source_file(directory, source)?)?;
        }
        if let Some(import) = &self.files_dir {
            let root = source_path(directory, import)?;
            let mut pending = vec![(root, String::new())];
            let mut visited = 0usize;
            while let Some((folder, prefix)) = pending.pop() {
                visited += 1;
                if visited > codec::MAX_ENTRIES {
                    return Err("too many source directories".into());
                }
                for child in fs::read_dir(folder)? {
                    let child = child?;
                    let name =
                        child.file_name().into_string().map_err(|_| "source path is not UTF-8")?;
                    let path = format!("{prefix}{name}");
                    if !codec::valid_path(&path) {
                        return Err("invalid source path".into());
                    }
                    let kind = child.file_type()?;
                    if kind.is_dir() {
                        if pending.len() == codec::MAX_ENTRIES {
                            return Err("too many source directories".into());
                        }
                        pending.push((child.path(), format!("{path}/")));
                    } else if kind.is_file() {
                        add_source(
                            &mut sources,
                            &path,
                            source_file(directory, &format!("{import}/{path}"))?,
                        )?;
                    } else {
                        return Err(
                            "bundle sources must not contain symlinks or special files".into()
                        );
                    }
                }
            }
        }
        Ok(sources)
    }
}

/// Read the closed role map before source packaging or trusted initialization.
/// Existing data never receives these seeds again.
pub fn read_volume_initializers(directory: &Path) -> Result<VolumeInitializers> {
    let path = directory.join(VOLUME_INITIALIZERS_FILE);
    let mut bytes = Vec::new();
    source_file(directory, VOLUME_INITIALIZERS_FILE)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .take(initialization::MAX_INITIALIZERS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    VolumeInitializers::from_json(&bytes)
        .map_err(|error| format!("{}: {error}", path.display()).into())
}

fn source_path(directory: &Path, source: &str) -> Result<std::path::PathBuf> {
    use std::path::Component;
    if source.is_empty() || Path::new(source).is_absolute() {
        return Err("source must be inside project".into());
    }
    let mut path = directory.to_owned();
    for part in Path::new(source).components() {
        match part {
            Component::Normal(name) => path.push(name),
            _ => return Err("invalid source path".into()),
        }
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err("source traverses symlink".into());
        }
    }
    Ok(path)
}
#[cfg(unix)]
fn source_file(directory: &Path, source: &str) -> Result<fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };
    source_path(directory, source)?;
    let mut file = fs::File::open(directory)?;
    let mut components = Path::new(source).components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err("invalid source path".into());
        };
        let name = CString::new(name.as_bytes())?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if components.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        // Retained directory descriptors prevent a component being replaced by
        // a symlink between path validation and opening the source bytes.
        // SAFETY: the descriptor and NUL-terminated name remain live for openat.
        #[allow(unsafe_code, reason = "Open project components without following symlinks.")]
        let descriptor = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat transferred ownership of this new descriptor.
        #[allow(unsafe_code, reason = "Own the newly opened source descriptor.")]
        {
            file = unsafe { fs::File::from_raw_fd(descriptor) };
        }
    }
    if !file.metadata()?.is_file() {
        return Err("source is not a regular file".into());
    }
    Ok(file)
}
#[cfg(not(unix))]
fn source_file(_: &Path, _: &str) -> Result<fs::File> {
    Err("source packaging requires Unix filesystem access".into())
}
fn add_source(
    sources: &mut BTreeMap<String, codec::Source>,
    path: &str,
    file: fs::File,
) -> Result<()> {
    if !codec::valid_path(path)
        || path == "app"
        || path.starts_with("app/") && !codec::optional_extension(path)
        || sources.len() == codec::MAX_ENTRIES
    {
        return Err("invalid or reserved bundle path".into());
    }
    if sources.insert(path.into(), codec::Source::File(file)).is_some() {
        return Err("duplicate bundle path".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod language_tests {
    use super::*;
    #[test]
    fn managed_ocaml_omits_commands_and_rejects_conflicting_build_configuration() {
        let source = "app_id='counter'\nname='Counter'\nlang='ocaml'\n";
        let value = Manifest::from_source(source).expect("managed OCaml manifest");
        assert_eq!(value.lang, Some(Language::Ocaml));
        assert!(value.build_cmd.is_empty());
        assert!(Manifest::from_source(&format!("{source}build_cmd='anything'\n")).is_err());
        assert!(Manifest::from_source(&format!("{source}build_artifact='app.wasm'\n")).is_err());
        assert!(
            Manifest::from_source(&format!("{source}[release]\nbuild_cmd='anything'\n")).is_err()
        );
    }
}
