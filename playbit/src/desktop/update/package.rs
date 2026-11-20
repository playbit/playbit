//! HTTPS discovery and signature-verified preparation. All calls are background work.
use super::archive;
use crate::engine::{
    Result,
    installation::{self, Installation, ReleaseIdentity},
};
use crate::release_channel::ReleaseChannel;
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

const BLOB_ORIGIN: &str = "https://blob.playbit.app/";
const MAX_MANIFEST: u64 = 64 * 1024;
pub(super) const REQUIREMENT: &str = "=anchor apple generic and identifier \"playbit.engine\" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"KBJTKLMMGM\"";

/// Admission is independent of install location and runs before any update work.
/// Do not force an online notarization lookup: distributed bundles are stapled.
pub(super) fn enabled(executable: &Path, development_override: Option<&OsStr>) -> Result<bool> {
    if development_override == Some(OsStr::new("1")) {
        return Ok(true);
    }
    let Some(app) =
        executable.ancestors().filter(|p| p.extension().is_some_and(|v| v == "app")).last()
    else {
        return Ok(false);
    };
    Ok(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "--all-architectures", "-R"])
        .arg(format!("{REQUIREMENT} and notarized"))
        .arg(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub app_version: String,
    pub sequence: u64,
    pub url: String,
    pub bundle_id: String,
    pub filename: String,
    pub sha256: String,
    pub bytes: u64,
    pub platform: String,
    pub revision: String,
    pub checks_sha256: String,
}
impl Release {
    pub fn parse_download(bytes: &[u8], channel: ReleaseChannel) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Download {
            channel: ReleaseChannel,
            artifact: Release,
        }
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err("Release manifest exceeds limit".into());
        }
        let download: Download = serde_json::from_slice(bytes)?;
        if download.channel != channel {
            return Err("Release manifest belongs to another channel".into());
        }
        download.artifact.validate()?;
        Ok(download.artifact)
    }

    fn validate(&self) -> Result<()> {
        let release = self;
        if release.app_version.len() > 128
            || release.bundle_id != "playbit.engine"
            || release.filename != "Playbit-macos-arm64.zip"
            || release.platform != "macos-arm64"
            || !(1..=archive::MAX_ZIP).contains(&release.bytes)
            || !hex(&release.sha256, 64)
            || !hex(&release.checks_sha256, 64)
            || !hex(&release.revision, 40)
        {
            return Err("Invalid Mac release manifest".into());
        }
        release.identity().validate()?;
        release.content_hash()?;
        if release.sequence == 0 {
            return Err("An unpublished build cannot be a release".into());
        }
        Ok(())
    }

    pub fn identity(&self) -> ReleaseIdentity {
        ReleaseIdentity { app_version: self.app_version.clone(), sequence: self.sequence }
    }

    fn content_hash(&self) -> Result<[u8; 32]> {
        let hash = self.url.strip_prefix(BLOB_ORIGIN).ok_or("Invalid release artifact origin")?;
        crate::identifier::decode(hash).map_err(Into::into)
    }
}
fn hex(value: &str, count: usize) -> bool {
    value.len() == count && value.bytes().all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
pub fn newer(candidate: &ReleaseIdentity, installed: &ReleaseIdentity) -> Result<bool> {
    candidate.validate()?;
    installed.validate()?;
    Ok(candidate.sequence > installed.sequence)
}
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into()
}
pub fn check(channel: ReleaseChannel) -> Result<Option<Release>> {
    let Some(manifest) = channel.macos_manifest() else { return Ok(None) };
    let mut response = agent(Duration::from_secs(30)).get(manifest).call()?;
    if response.status() != 200 {
        return Err("Release server did not return a manifest".into());
    }
    let mut bytes = Vec::new();
    response.body_mut().as_reader().take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    let release = Release::parse_download(&bytes, channel)
        .map_err(|_| "The release server returned an invalid response. Try again later.")?;
    Ok(newer(&release.identity(), &ReleaseIdentity::running())?.then_some(release))
}
pub fn prepare(installation: &Installation, release: &Release) -> Result<()> {
    release.validate()?;
    let download = installation.state.join("download.zip");
    let incoming = installation.state.join("incoming");
    if incoming.exists() {
        fs::remove_dir_all(&incoming)?;
    }
    let mut file = installation.file("download.zip")?;
    file.set_len(0)?;
    let mut response = agent(Duration::from_secs(15 * 60)).get(&release.url).call()?;
    if response.status() != 200 {
        return Err("Release server did not return an update".into());
    }
    transfer(&mut response.body_mut().as_reader(), &mut file, release)?;
    file.sync_all()?;
    archive::extract(&download, &incoming)?;
    verify(&incoming.join("Playbit.app"), &release.identity())?;
    if !newer(&release.identity(), &installation.release()?)? {
        return Err("The prepared update is not newer than this installation".into());
    }
    let staged = installation.state.join("staged.app");
    if staged.exists() {
        fs::remove_dir_all(&staged)?;
    }
    fs::rename(incoming.join("Playbit.app"), staged)?;
    fs::remove_dir(incoming)?;
    fs::remove_file(download)?;
    Ok(())
}
pub fn verify(app: &Path, identity: &ReleaseIdentity) -> Result<()> {
    let status = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "--all-architectures", "-R", REQUIREMENT])
        .arg(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("The update does not have a valid Playbit release signature".into());
    }
    let info = installation::plist(app)?;
    if installation::string(&info, "CFBundleIdentifier")? != "playbit.engine"
        || installation::string(&info, "CFBundleExecutable")? != "Playbit"
        || ReleaseIdentity::from_info(&info)? != *identity
    {
        return Err("Signed update identity does not match the release".into());
    }
    identity.validate()?;
    let required = os_version(&installation::string(&info, "LSMinimumSystemVersion")?)?;
    let current = Command::new("/usr/bin/sw_vers").arg("-productVersion").output()?;
    if !current.status.success()
        || required > os_version(std::str::from_utf8(&current.stdout)?.trim())?
    {
        return Err("This update requires a newer version of macOS".into());
    }
    Ok(())
}
fn os_version(value: &str) -> Result<[u32; 3]> {
    let mut version = [0; 3];
    let mut count = 0;
    for part in value.split('.') {
        if count == 3 || part.is_empty() || !part.bytes().all(|v| v.is_ascii_digit()) {
            return Err("Invalid macOS version".into());
        }
        version[count] = part.parse()?;
        count += 1;
    }
    if count == 0 {
        return Err("Missing macOS version".into());
    }
    Ok(version)
}

/// Only overlapping callers reuse this result. A later explicit check always
/// makes a new request, including after a failed request.
pub fn coalesced_check(
    installation: &Installation,
    requested: std::time::SystemTime,
    channel: ReleaseChannel,
) -> Result<Option<Release>> {
    coalesced_check_with(installation, requested, channel, check)
}

fn coalesced_check_with(
    installation: &Installation,
    requested: std::time::SystemTime,
    channel: ReleaseChannel,
    fetch: impl FnOnce(ReleaseChannel) -> Result<Option<Release>>,
) -> Result<Option<Release>> {
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Cached {
        channel: ReleaseChannel,
        finished: std::time::SystemTime,
        result: std::result::Result<Option<Release>, String>,
    }
    let path = installation.state.join("check.json");
    let bytes = read_check_cache(&path)?;
    // This is a disposable result cache, including across updater versions.
    // Unsafe files and I/O failures remain errors; unusable contents do not
    // prevent an explicit check from fetching a newly validated manifest.
    if let Some(bytes) = bytes
        && let Ok(cached) = serde_json::from_slice::<Cached>(&bytes)
    {
        let valid = match &cached.result {
            Ok(Some(release)) => release.validate().is_ok(),
            _ => true,
        };
        if cached.channel == channel
            && cached.finished >= requested
            && cached.finished <= std::time::SystemTime::now()
            && valid
        {
            return cached.result.map_err(Into::into);
        }
    }
    let result = fetch(channel).map_err(|error| error.to_string());
    let cached = Cached { channel, finished: std::time::SystemTime::now(), result };
    installation.write("check.json", &serde_json::to_vec(&cached)?)?;
    cached.result.map_err(Into::into)
}

fn read_check_cache(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::engine::profile::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err("Unsafe update check cache file".into());
    }
    if metadata.len() > MAX_MANIFEST {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_MANIFEST).then_some(bytes))
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod cache_tests;

pub(super) fn transfer(
    body: &mut impl Read,
    output: &mut impl Write,
    release: &Release,
) -> Result<()> {
    let mut buffer = [0; 64 * 1024];
    let mut length = 0u64;
    let mut digest = Context::new(&SHA256);
    let mut content = blake3::Hasher::new();
    loop {
        let count = body.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > release.bytes || length > archive::MAX_ZIP {
            return Err("Update exceeds advertised size".into());
        }
        digest.update(&buffer[..count]);
        content.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    let hash = digest.finish().as_ref().iter().map(|v| format!("{v:02x}")).collect::<String>();
    if length != release.bytes
        || hash != release.sha256
        || content.finalize().as_bytes() != &release.content_hash()?
    {
        return Err("Update download did not match the manifest; try again".into());
    }
    Ok(())
}
