//! Bounded, eager build caching, independent of session/volume admission.
//! Completion wakes the owner; failed transfers retry on reconnect or explicit open.
use super::{Result, completion::Wake};
use crate::{
    network::ap::ApConnection,
    store::{LocalStore, Session},
};
#[cfg(test)]
use crate::{package::Package, store::Id};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

const LIMIT: usize = 256;
const CONCURRENT: usize = 1;
const MAX_BUILD_BYTES: usize = crate::package::MAX_BUNDLE_BYTES;

#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuildState {
    #[default]
    Checking,
    NotDownloaded,
    Downloading,
    Ready,
    Unreadable {
        error: String,
    },
    Invalid {
        error: String,
    },
    DownloadFailed {
        error: String,
    },
    MetadataFailed {
        error: String,
    },
}
impl BuildState {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::NotDownloaded => "not_downloaded",
            Self::Downloading => "downloading",
            Self::Ready => "ready",
            Self::Unreadable { .. } => "unreadable",
            Self::Invalid { .. } => "invalid",
            Self::DownloadFailed { .. } => "download_failed",
            Self::MetadataFailed { .. } => "metadata_failed",
        }
    }
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Checking => Some("Checking local build…".into()),
            Self::NotDownloaded => Some("Build not downloaded yet".into()),
            Self::Downloading => Some("Downloading build…".into()),
            Self::Ready => None,
            Self::Unreadable { error } => Some(format!("Cannot read local build: {error}")),
            Self::Invalid { error } => Some(format!("Invalid local build: {error}")),
            Self::DownloadFailed { error } => Some(format!("Build download failed: {error}")),
            Self::MetadataFailed { error } => Some(format!("Could not save app name: {error}")),
        }
    }
}
fn error_text(error: impl std::fmt::Display) -> String {
    let text = error.to_string();
    if text.len() <= 4096 {
        return text;
    }
    let mut end = 4093;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(4096);
    bounded.push_str(&text[..end]);
    bounded.push('…');
    bounded
}

struct Entry {
    state: BuildState,
    connection: Option<Weak<ApConnection>>,
    sessions: BTreeSet<String>,
    running: bool,
    generation: u64,
}
struct Catalog {
    epoch: u64,
    connection: Option<Weak<ApConnection>>,
}
#[derive(Default)]
pub(crate) struct Builds {
    entries: Mutex<BTreeMap<String, Entry>>,
    catalog: Mutex<Option<Catalog>>,
    refresh_requested: AtomicBool,
    changed: AtomicBool,
    uploads_pending: AtomicBool,
}
impl Builds {
    pub(crate) fn states(&self) -> BTreeMap<String, BuildState> {
        self.entries
            .lock()
            .expect("build state lock is not poisoned")
            .iter()
            .map(|(id, entry)| (id.clone(), entry.state.clone()))
            .collect()
    }
    pub(crate) fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
    pub(crate) fn take_uploads_pending(&self) -> bool {
        self.uploads_pending.swap(false, Ordering::AcqRel)
    }
    /// A validated foreground installation supersedes any older fetch result.
    pub(crate) fn installed(&self, build: &str) {
        if let Some(entry) =
            self.entries.lock().expect("build state lock is not poisoned").get_mut(build)
        {
            entry.generation = entry.generation.checked_add(1).expect("build generation exhausted");
            entry.state = BuildState::Ready;
            self.refresh_requested.store(true, Ordering::Release);
            self.changed.store(true, Ordering::Release);
        }
    }
    pub(crate) fn retry(&self, build: &str) {
        if let Some(entry) =
            self.entries.lock().expect("build state lock is not poisoned").get_mut(build)
            && !entry.running
            && !matches!(entry.state, BuildState::Ready)
        {
            // Opening a ready session checks its authorized update before fetching again.
            entry.state = BuildState::Checking;
            entry.connection = None;
            self.refresh_requested.store(true, Ordering::Release);
        }
    }
    fn changed_sessions(
        &self,
        store: &LocalStore,
        identity: &Option<Weak<ApConnection>>,
    ) -> Result<Option<Vec<Session>>> {
        // Metadata writes, connection replacement and job completion are the
        // only reasons to revisit the retained set. Ordinary app turns must
        // not repeatedly decode every invitation on the interactive owner.
        let epoch = store.session_catalog_epoch();
        let requested = self.refresh_requested.swap(false, Ordering::AcqRel);
        let unchanged =
            self.catalog.lock().expect("catalog lock is not poisoned").as_ref().is_some_and(
                |catalog| {
                    Some(catalog.epoch) == epoch
                        && match (&catalog.connection, &identity) {
                            (Some(previous), Some(current)) => previous.ptr_eq(current),
                            (None, None) => true,
                            _ => false,
                        }
                },
            );
        if unchanged && !requested {
            return Ok(None);
        }
        let sessions = match store.retained_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                self.refresh_requested.store(true, Ordering::Release);
                return Err(error.into());
            }
        };
        // Cache only a projection wholly between writes. Active or intervening
        // writers force another refresh even if this scan saw their new rows.
        let after = store.session_catalog_epoch();
        *self.catalog.lock().expect("catalog lock is not poisoned") = epoch
            .filter(|_| epoch == after)
            .map(|epoch| Catalog { epoch, connection: identity.clone() });
        Ok(Some(sessions))
    }
    pub(crate) fn refresh(
        self: &Arc<Self>,
        store: Arc<LocalStore>,
        connection: Option<Arc<ApConnection>>,
        runtime: &tokio::runtime::Handle,
        wake: Arc<Wake>,
    ) -> Result<()> {
        let connection = connection.filter(|c| !c.is_closed());
        let identity = connection.as_ref().map(Arc::downgrade);
        let Some(sessions) = self.changed_sessions(&store, &identity)? else { return Ok(()) };
        assert!(sessions.len() <= LIMIT, "retained session display limit was validated");
        let mut groups: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for session in &sessions {
            groups.entry(&session.resolved_build_id).or_default().insert(session.id.clone());
        }
        let mut entries = self.entries.lock().expect("build state lock is not poisoned");
        entries.retain(|id, entry| entry.running || groups.contains_key(id.as_str()));
        let mut running = entries.values().filter(|entry| entry.running).count();
        for (build, session_ids) in groups {
            if running == CONCURRENT {
                break;
            }
            if !entries.contains_key(build) && entries.len() == LIMIT {
                continue;
            }
            let entry = entries.entry(build.to_owned()).or_insert(Entry {
                state: BuildState::Checking,
                connection: None,
                sessions: BTreeSet::new(),
                running: false,
                generation: 0,
            });
            if !session_ids.is_subset(&entry.sessions) && matches!(entry.state, BuildState::Ready) {
                entry.state = BuildState::Checking;
            }
            entry.sessions = session_ids;
            let retry_connection = identity.as_ref().is_some_and(|current| {
                entry.connection.as_ref().is_none_or(|previous| !previous.ptr_eq(current))
            }) && matches!(
                entry.state,
                BuildState::NotDownloaded | BuildState::DownloadFailed { .. }
            );
            if entry.running || !(matches!(entry.state, BuildState::Checking) || retry_connection) {
                continue;
            }
            entry.running = true;
            entry.connection = identity.clone();
            running += 1;
            let generation = entry.generation;
            let (builds, store, connection, wake, build) =
                (self.clone(), store.clone(), connection.clone(), wake.clone(), build.to_owned());
            runtime.spawn(async move {
                let state =
                    builds.fetch(&store, &build, generation, connection.as_deref(), &wake).await;
                builds.finish(&build, generation, state);
                builds.changed.store(true, Ordering::Release);
                wake.notify();
            });
        }
        Ok(())
    }
    fn finish(&self, build: &str, generation: u64, state: BuildState) {
        if let Some(entry) =
            self.entries.lock().expect("build state lock is not poisoned").get_mut(build)
        {
            if entry.generation == generation {
                entry.state = state;
            }
            entry.running = false;
            self.refresh_requested.store(true, Ordering::Release);
        }
    }
    async fn fetch(
        &self,
        store: &Arc<LocalStore>,
        build: &str,
        generation: u64,
        connection: Option<&ApConnection>,
        wake: &Wake,
    ) -> BuildState {
        let path = store.profile_path().join(crate::paths::BUILDS).join(build);
        let local = probe(path.clone(), build.to_owned()).await;
        match local {
            Ok(name) => {
                return remember(
                    store.clone(),
                    build.to_owned(),
                    name,
                    None,
                    false,
                    Some(&self.uploads_pending),
                )
                .await;
            }
            Err(BuildState::NotDownloaded) => {}
            Err(error) => return error,
        }
        let Some(connection) = connection else { return BuildState::NotDownloaded };
        if let Some(entry) =
            self.entries.lock().expect("build state lock is not poisoned").get_mut(build)
            && entry.generation == generation
        {
            entry.state = BuildState::Downloading;
        }
        self.changed.store(true, Ordering::Release);
        wake.notify();
        match download(connection, build, &path).await {
            Ok(downloaded) => {
                if downloaded.admission.is_some()
                    && let Err(error) = refresh_downloaded_publisher(
                        connection,
                        store.clone(),
                        path,
                        build.to_owned(),
                    )
                    .await
                {
                    return BuildState::DownloadFailed { error: error_text(error) };
                }
                remember(
                    store.clone(),
                    build.to_owned(),
                    downloaded.name,
                    downloaded.admission,
                    true,
                    None,
                )
                .await
            }
            Err(error) => BuildState::DownloadFailed { error: error_text(error) },
        }
    }
}

/// Establish issuer trust through this authenticated connection before the
/// independently verified receipt reaches the existing metadata worker.
async fn refresh_downloaded_publisher(
    connection: &ApConnection,
    store: Arc<LocalStore>,
    path: PathBuf,
    build: String,
) -> Result<()> {
    let issuer =
        connection.publisher_issuer().ok_or("authenticated AP publisher issuer is unavailable")?;
    let (publisher, signer) = tokio::task::spawn_blocking(move || -> Result<_> {
        let package = crate::package::Package::open(&path)?;
        if package.id()? != build {
            return Err("downloaded build changed before publisher status admission".into());
        }
        let certificate =
            package.publisher_certificate().ok_or("package has no publisher signature")?;
        Ok((certificate.claims.publisher, certificate.claims.signer))
    })
    .await
    .unwrap_or_else(|error| {
        if error.is_cancelled() {
            return Err("build publisher probe cancelled".into());
        }
        std::panic::resume_unwind(error.into_panic())
    })?;
    let request = crate::network::ap::operation::ServiceRequest::GetPublisherKeyStatus {
        operation_id: crate::store::Id::generate()?.to_string(),
        publisher,
        signer,
    };
    let reply = crate::network::ap::service_client::request(connection, &request).await?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Status {
        status: Vec<u8>,
    }
    let status: Status =
        serde_json::from_value(reply.data.ok_or("publisher status reply is missing")?)?;
    let binding = crate::network::ap::publisher::KeyStatus::from_bytes(&status.status)?;
    if binding.claims.publisher != publisher || binding.claims.signer != signer {
        return Err("publisher status differs from requested signing key".into());
    }
    tokio::task::spawn_blocking(move || {
        store.cache_publisher_status(issuer, &status.status, crate::store::now_unix_secs())
    })
    .await
    .unwrap_or_else(|error| {
        if error.is_cancelled() {
            return Err(crate::store::Error::Internal("build publisher metadata worker cancelled"));
        }
        std::panic::resume_unwind(error.into_panic())
    })?;
    Ok(())
}

async fn remember(
    store: Arc<LocalStore>,
    build: String,
    name: String,
    admission: Option<Vec<u8>>,
    downloaded: bool,
    uploads_pending: Option<&AtomicBool>,
) -> BuildState {
    match tokio::task::spawn_blocking(move || {
        if let Some(admission) = admission {
            store.cache_package_admission(
                &admission,
                crate::identifier::decode::<32>(&build)
                    .map_err(|_| crate::store::Error::InvalidInput("invalid build identity"))?,
                crate::store::now_unix_secs(),
            )?;
        }
        let queued = if downloaded {
            store.remember_uploaded_build(&build)?;
            false
        } else {
            let path = store.profile_path().join(crate::paths::BUILDS).join(&build);
            store.queue_build_upload(&build, path)?.is_some()
        };
        store.remember_app_name(&build, &name)?;
        Ok::<_, crate::store::Error>(queued)
    })
    .await
    .unwrap_or_else(|error| {
        if error.is_cancelled() {
            return Err(crate::store::Error::Internal("build metadata worker cancelled"));
        }
        std::panic::resume_unwind(error.into_panic())
    }) {
        Ok(queued) => {
            if queued && let Some(uploads_pending) = uploads_pending {
                uploads_pending.store(true, Ordering::Release);
            }
            BuildState::Ready
        }
        Err(error) => BuildState::MetadataFailed { error: error_text(error) },
    }
}
async fn probe(path: PathBuf, build: String) -> std::result::Result<String, BuildState> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BuildState::NotDownloaded
            } else {
                BuildState::Unreadable { error: error_text(error) }
            }
        })?;
        if !file
            .metadata()
            .map_err(|error| BuildState::Unreadable { error: error_text(error) })?
            .is_file()
        {
            return Err(BuildState::Unreadable { error: "build is not a regular file".into() });
        }
        let length = file
            .metadata()
            .map_err(|error| BuildState::Unreadable { error: error_text(error) })?
            .len();
        if length > MAX_BUILD_BYTES as u64 {
            return Err(BuildState::Invalid { error: "build exceeds package size limit".into() });
        }
        let verified = crate::package::codec::verify(&mut file)
            .map_err(|error| BuildState::Invalid { error: error_text(error) })?;
        if crate::identifier::encode(&verified.digest) != build {
            return Err(BuildState::Invalid { error: "build content hash mismatch".into() });
        }
        Ok(verified.name)
    })
    .await
    .unwrap_or_else(|error| {
        if error.is_cancelled() {
            return Err(BuildState::Unreadable { error: "build probe cancelled".into() });
        }
        std::panic::resume_unwind(error.into_panic())
    })
}
pub(crate) async fn download(
    connection: &ApConnection,
    build: &str,
    path: &Path,
) -> Result<super::build_download::Downloaded> {
    super::build_download::download(connection, build, path, None).await
}
#[cfg(test)]
fn install(bytes: &[u8], build: &str, path: &Path) -> Result<String> {
    let name = super::build_download::validate(bytes, build)?;
    let parent = path.parent().expect("build path belongs to the builds directory");
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".download-{}", Id::generate()?));
    let result = (|| {
        std::fs::write(&temporary, bytes)?;
        // Match the store's process-crash boundary. Apple's sync_all requests
        // a device-wide full flush, stalling unrelated interactive commits.
        let file = std::fs::File::open(&temporary)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(std::io::Error::from)?;
        std::fs::rename(&temporary, path)?;
        let directory = std::fs::File::open(parent)?;
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&directory))
            .map_err(std::io::Error::from)?;
        Ok(name)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert fixture operations and states.")]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cached_builds_queue_publication_until_acknowledged_and_downloads_are_already_known() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let package = Package::new(
            "test".into(),
            "Notes".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::new(),
        )
        .unwrap();
        let build = package.id().unwrap();
        let path = directory.path().join(crate::paths::BUILDS).join(&build);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, package.to_bytes().unwrap()).unwrap();
        let pending = AtomicBool::new(false);
        assert_eq!(
            remember(store.clone(), build.clone(), "Notes".into(), None, false, Some(&pending))
                .await,
            BuildState::Ready
        );
        assert!(pending.swap(false, Ordering::AcqRel));
        let operation = store.pending_operations().unwrap().pop().unwrap();
        assert!(matches!(operation.operation, crate::store::PendingKind::UploadBuild { .. }));
        store.acknowledge_pending(&operation).unwrap();
        assert_eq!(
            remember(store.clone(), build.clone(), "Notes".into(), None, false, Some(&pending))
                .await,
            BuildState::Ready
        );
        assert!(!pending.load(Ordering::Acquire));
        assert!(store.pending_operations().unwrap().is_empty());

        let downloaded = crate::identifier::encode(&[9; 32]);
        assert_eq!(
            remember(store.clone(), downloaded.clone(), "Downloaded".into(), None, true, None)
                .await,
            BuildState::Ready
        );
        assert!(store.build_uploaded(&downloaded).unwrap());
        assert!(store.pending_operations().unwrap().is_empty());
    }
    #[test]
    fn foreground_install_supersedes_a_failed_fetch_without_releasing_its_job() {
        // A completed foreground installation wins even if an older eager request fails later.
        let builds = Builds::default();
        let build = crate::identifier::encode(&[3; 32]);
        builds.entries.lock().unwrap().insert(
            build.clone(),
            Entry {
                state: BuildState::Downloading,
                connection: None,
                sessions: BTreeSet::new(),
                running: true,
                generation: 0,
            },
        );
        builds.installed(&build);
        assert!(builds.entries.lock().unwrap()[&build].running);
        builds.finish(&build, 0, BuildState::DownloadFailed { error: "blob not found".into() });
        assert_eq!(builds.states()[&build], BuildState::Ready);
        assert!(!builds.entries.lock().unwrap()[&build].running);
        builds.refresh_requested.store(false, Ordering::Release);
        builds.retry(&build);
        assert!(!builds.refresh_requested.load(Ordering::Acquire));
        assert_eq!(builds.states()[&build], BuildState::Ready);
        let error = error_text("é".repeat(16384));
        assert!(error.ends_with('…') && error.len() <= 4096 && error.capacity() <= 4096);
    }

    #[tokio::test]
    async fn local_probe_distinguishes_missing_unreadable_invalid_and_verified_builds() {
        // Exercise filesystem and parser errors separately, including a valid but wrong build.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("build");
        let package = Package::new(
            "test".into(),
            "Notes".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::new(),
        )
        .unwrap();
        let build = package.id().unwrap();
        assert_eq!(probe(path.clone(), build.clone()).await, Err(BuildState::NotDownloaded));
        assert!(matches!(
            probe(directory.path().into(), build.clone()).await,
            Err(BuildState::Unreadable { .. })
        ));
        std::fs::write(&path, b"broken package").unwrap();
        assert!(matches!(
            probe(path.clone(), build.clone()).await,
            Err(BuildState::Invalid { .. })
        ));
        let bytes = package.to_bytes().unwrap();
        assert!(install(&bytes, &crate::identifier::encode(&[42; 32]), &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken package");
        assert_eq!(install(&bytes, &build, &path).unwrap(), "Notes");
        assert_eq!(probe(path, build).await.unwrap(), "Notes");
    }
}
