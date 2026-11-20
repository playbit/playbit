//! Signed SDK acquisition and installation for the supported macOS host.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use playbit::release_channel::ReleaseChannel;
use serde::{Deserialize, Serialize};
use std::{
    io::IsTerminal,
    time::{Duration, Instant},
};

const MAX_CATALOG: u64 = 65536;
pub(super) const MAX_ARCHIVE: u64 = 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 4 * 1024 * 1024 * 1024;
pub(super) const ORIGIN: &str = "https://blob.playbit.app/";
const DRIVER: u32 = 1;
const SDK_KEY: &[u8] = include_bytes!("public-key");

fn channel() -> Result<ReleaseChannel> {
    // Main selected the installation's channel before dispatch, including remembered selection.
    Ok(crate::sdk_channel())
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Artifact {
    pub(super) language: Language,
    pub(super) version: String,
    pub(super) platform: String,
    pub(super) driver: u32,
    pub(super) url: String,
    pub(super) bytes: u64,
    pub(super) expanded_bytes: u64,
}
impl Artifact {
    fn display_version(&self) -> &str {
        self.version.split_once("-playbit.").map_or(self.version.as_str(), |(version, _)| version)
    }
}

struct DownloadProgress {
    label: String,
    total_mib: u64,
    terminal: bool,
    last: Instant,
}
impl DownloadProgress {
    fn new(artifact: &Artifact) -> Self {
        let mut progress = Self {
            label: format!(
                "pb: Fetching {} SDK {}",
                name(artifact.language),
                artifact.display_version()
            ),
            total_mib: artifact.bytes.div_ceil(1024 * 1024),
            terminal: std::io::stderr().is_terminal(),
            last: Instant::now(),
        };
        progress.report(0, false);
        progress
    }

    fn report(&mut self, received: u64, complete: bool) {
        if received != 0 && !complete && self.last.elapsed() < Duration::from_millis(250) {
            return;
        }
        let amount = if complete {
            format!("{} MiB", self.total_mib)
        } else {
            format!("{:.1} / {} MiB", received as f64 / (1024.0 * 1024.0), self.total_mib)
        };
        if self.terminal {
            eprint!("\r\x1b[K{} ({amount})", self.label);
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{} ({amount})", self.label);
        }
        self.last = Instant::now();
    }
}
impl Drop for DownloadProgress {
    fn drop(&mut self) {
        if self.terminal {
            eprintln!();
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    channel: ReleaseChannel,
    sequence: u64,
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: String,
}
pub(super) fn catalog(bytes: &[u8], channel: ReleaseChannel) -> Result<Catalog> {
    catalog_with_key(bytes, channel, SDK_KEY)
}
pub(super) fn catalog_with_key(
    bytes: &[u8],
    channel: ReleaseChannel,
    key: &[u8],
) -> Result<Catalog> {
    if bytes.len() as u64 > MAX_CATALOG {
        return Err("SDK catalog exceeds limit".into());
    }
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    let payload = STANDARD.decode(envelope.payload)?;
    let signature = STANDARD.decode(envelope.signature)?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(&payload, &signature)
        .map_err(|_| "invalid SDK catalog signature")?;
    let value: Catalog = serde_json::from_slice(&payload)?;
    if value.channel != channel
        || value.sequence == 0
        || value.artifacts.is_empty()
        || value.artifacts.len() > 32
    {
        return Err("invalid SDK catalog channel or bounds".into());
    }
    for artifact in &value.artifacts {
        if artifact.version.is_empty()
            || artifact.version.len() > 128
            || artifact.bytes == 0
            || artifact.bytes > MAX_ARCHIVE
            || artifact.expanded_bytes == 0
            || artifact.expanded_bytes > MAX_EXPANDED
            || artifact.platform.is_empty()
            || artifact.platform.len() > 64
            || artifact.driver == 0
        {
            return Err("invalid SDK artifact size/version".into());
        }
        address(artifact)?;
    }
    Ok(value)
}
pub(super) fn address(artifact: &Artifact) -> Result<String> {
    let address = artifact.url.strip_prefix(ORIGIN).ok_or("invalid SDK blob origin")?;
    let _: [u8; 32] = playbit::identifier::decode(address)?;
    Ok(address.to_owned())
}
pub(super) fn artifact(value: &Catalog, language: Language) -> Result<&Artifact> {
    value
        .artifacts
        .iter()
        .rfind(|a| a.language == language && a.platform == "macos-arm64" && a.driver == DRIVER)
        .ok_or_else(|| {
            "No compatible SDK is published for this language/platform/build driver".into()
        })
}
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent("Playbit SDK/1")
        .https_only(true)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(15 * 60)))
        .build()
        .into()
}
fn fetch_catalog(channel: ReleaseChannel) -> Result<Vec<u8>> {
    let url = format!("{}/download/sdk.json", channel.website());
    let mut response =
        agent().get(&url).call().map_err(|error| format!("failed to fetch {url} ({error})"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_CATALOG + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read {url} ({error})"))?;
    catalog(&bytes, channel)
        .map_err(|error| format!("invalid SDK catalog from {url} ({error})"))?;
    Ok(bytes)
}
fn selection(root: &Path, channel: ReleaseChannel, language: Language) -> PathBuf {
    root.join(format!("{}-{}.json", channel.name(), name(language)))
}
pub(super) fn managed(language: Language, update: bool) -> Result<Tools> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    return Err("Downloaded SDKs currently require an arm64 Mac".into());
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let root = cache()?;
        let channel = channel()?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("install.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                eprintln!("pb: Waiting for another SDK installation");
                lock.lock()?;
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let record = selection(&root, channel, language);
        let cached = if !update && record.is_file() {
            let bytes = read(&record, MAX_CATALOG)?;
            let value = catalog(&bytes, channel)?;
            artifact(&value, language).is_ok().then_some(bytes)
        } else {
            None
        };
        let fetched = cached.is_none();
        let bytes = match cached {
            Some(bytes) => bytes,
            None => fetch_catalog(channel)?,
        };
        let value = catalog(&bytes, channel)?;
        if fetched {
            for language in [Language::Rust, Language::Ocaml] {
                let known = selection(&root, channel, language);
                if known.is_file() {
                    check_sequence(&catalog(&read(&known, MAX_CATALOG)?, channel)?, &value)?;
                }
            }
        }
        let selected = artifact(&value, language)?;
        let id = address(selected)?;
        let destination = root.join(&id);
        if !destination.is_dir() {
            // Under the installation lock these are only abandoned attempts.
            let staging = root.join("incoming");
            if staging.exists() {
                fs::remove_dir_all(&staging)?;
            }
            let download = root.join("download.tar.xz");
            let mut file = File::create(&download)?;
            let mut progress = DownloadProgress::new(selected);
            let mut response = agent()
                .get(&selected.url)
                .call()
                .map_err(|error| format!("failed to fetch {} ({error})", selected.url))?;
            let mut input = response.body_mut().as_reader();
            let mut hasher = blake3::Hasher::new();
            let mut count = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let n = input
                    .read(&mut buffer)
                    .map_err(|error| format!("failed to read {} ({error})", selected.url))?;
                if n == 0 {
                    break;
                }
                count += n as u64;
                if count > selected.bytes {
                    return Err("SDK download exceeds manifest size".into());
                }
                hasher.update(&buffer[..n]);
                file.write_all(&buffer[..n])?;
                progress.report(count, false);
            }
            if count != selected.bytes
                || playbit::identifier::encode(hasher.finalize().as_bytes()) != id
            {
                return Err("SDK download does not match catalog".into());
            }
            progress.report(count, true);
            drop(progress);
            file.sync_all()?;
            eprintln!("pb: Extracting {} SDK {}", name(language), selected.display_version());
            archive::extract(&download, &staging, selected.expanded_bytes)?;
            let installed = staging.join("sdk");
            eprintln!("pb: Checking {} SDK {}", name(language), selected.display_version());
            let tools = tools_at(language, &installed, id.clone())?;
            probe(&tools)?;
            project_probe(&tools, &installed)?;
            File::create_new(installed.join("lease"))?.sync_all()?;
            fs::rename(installed, &destination)?;
            fs::remove_dir(&staging)?;
            fs::remove_file(download)?;
        }
        // Acquire the read lease while the installer lock still excludes eviction.
        let lease = File::open(destination.join("lease"))?;
        lease.lock_shared()?;
        let previous = record.with_extension("previous.json");
        if record.is_file() && read(&record, MAX_CATALOG)? != bytes {
            atomic(&previous, &read(&record, MAX_CATALOG)?)?;
        }
        atomic(&record, &bytes)?;
        let mut tools = tools_at(language, &destination, id)?;
        tools._lease = Some(Arc::new(lease));
        tools.description = format!("{} ({})", selected.display_version(), tools.identity);
        if let Err(error) = prune(&root) {
            eprintln!("pb: SDK installed; old cache cleanup failed: {error}");
        }
        Ok(tools)
    }
}
fn prune(root: &Path) -> Result<()> {
    let mut retained = std::collections::BTreeSet::new();
    for c in [ReleaseChannel::Public, ReleaseChannel::Testing] {
        for l in [Language::Rust, Language::Ocaml] {
            let selected = selection(root, c, l);
            for path in [selected.clone(), selected.with_extension("previous.json")] {
                if path.is_file() {
                    let bytes = read(&path, MAX_CATALOG)?;
                    if let Ok(catalog) = catalog(&bytes, c) {
                        retained.insert(address(artifact(&catalog, l)?)?);
                    }
                }
            }
        }
    }
    for entry in fs::read_dir(root)?.take(1024) {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if playbit::identifier::valid(name, 32)
            && entry.file_type()?.is_dir()
            && !retained.contains(name)
        {
            let lease = File::open(entry.path().join("lease"))?;
            match lease.try_lock() {
                Ok(()) => fs::remove_dir_all(entry.path())?,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(super) fn check_sequence(previous: &Catalog, next: &Catalog) -> Result<()> {
    if next.sequence < previous.sequence
        || (next.sequence == previous.sequence
            && serde_json::to_vec(previous)? != serde_json::to_vec(next)?)
    {
        return Err("SDK catalog sequence regressed or changed without advancing".into());
    }
    Ok(())
}
