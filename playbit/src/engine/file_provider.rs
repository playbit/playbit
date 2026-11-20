//! Apple File Provider's bounded operation boundary. Each profile retains
//! one owner of its durable replica; filesystem writes use ordinary store
//! publication and the existing AP outbox, subscriptions and gap recovery.
use super::{Result, blob::BlobTransport};
use crate::store::{
    Access, DatabaseReference, EntryId, EntryType, Id, LocalStore,
    file_provider::{PROVIDER_PAGE_ITEMS, ProviderChange},
    volume_index::EntryAddress,
};
#[cfg(all(target_os = "macos", feature = "process"))]
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
#[cfg(all(target_os = "macos", feature = "process"))]
use std::{
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

pub mod replica;

const REQUEST_BYTES: u64 = 64 * 1024;
#[cfg(all(target_os = "macos", feature = "process"))]
const REPLY_BYTES: usize = crate::store::file_provider::PROVIDER_REPLY_BYTES;
#[cfg(all(target_os = "macos", feature = "process"))]
const WORKERS: usize = 4;
#[cfg(all(target_os = "macos", feature = "process"))]
const SOCKET: &str = "file-provider.sock";

/// macOS domains are shared by bundle identity. An isolated launch must never
/// adopt the OS user's public domains, even when it overrides HOME.
#[cfg(all(target_os = "macos", feature = "process"))]
pub fn require_system_profile(profile: &Path) -> Result<()> {
    let public = crate::release_channel::ReleaseChannel::Public;
    if super::profile::for_profile(profile)? != public
        || !super::profile::is_default_profile(profile, public)?
    {
        return Err("File Provider drives require the OS user's default public profile".into());
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos", feature = "process"))]
#[path = "file_provider_tests.rs"]
mod tests;

#[cfg(all(target_os = "macos", feature = "process"))]
pub(crate) struct Coordinator(Option<std::process::Child>);
#[cfg(all(target_os = "macos", feature = "process"))]
impl Coordinator {
    pub(crate) fn start(profile: &Path) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let Some(app) = executable
            .ancestors()
            .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
            .last()
        else {
            return Ok(Self(None));
        };
        if let Err(error) = require_system_profile(profile) {
            crate::diagnostic!("File Provider is unavailable for this profile: {error}");
            return Ok(Self(None));
        }
        let helper = app.join("Contents/MacOS/Playbit");
        let child = std::process::Command::new(helper)
            .arg("--file-provider-drives")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .env("PLAYBIT_PROFILE", profile)
            .spawn()?;
        Ok(Self(Some(child)))
    }
}
#[cfg(all(target_os = "macos", feature = "process"))]
impl Drop for Coordinator {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Root,
    VolumeItem {
        volume: Id,
    },
    CreateVolume {
        id: Id,
        name: String,
    },
    RenameVolume {
        volume: Id,
        name: String,
        metadata: Vec<u8>,
    },
    Catalog {
        known: Option<Vec<Id>>,
    },
    Volumes {
        after: Option<(Id, u64)>,
        known: Option<Vec<Id>>,
        volume: Option<Id>,
    },
    PushToken {
        token: String,
        invalidated: bool,
        bundle: String,
        sandbox: bool,
    },
    List {
        volume: Id,
        parent: Option<EntryId>,
        after: Option<EntryId>,
    },
    Scan {
        volume: Id,
        after: Option<EntryId>,
    },
    Item {
        volume: Id,
        entry: EntryId,
    },
    Anchor {
        volume: Id,
    },
    Fetch {
        volume: Id,
        entry: EntryId,
        version: Vec<u8>,
        path: PathBuf,
    },
    Create {
        volume: Id,
        id: EntryId,
        parent: Option<EntryId>,
        name: String,
        directory: bool,
        path: Option<PathBuf>,
    },
    Modify {
        volume: Id,
        entry: EntryId,
        parent: Option<EntryId>,
        name: String,
        version: Vec<u8>,
        metadata: Vec<u8>,
        path: Option<PathBuf>,
    },
    Delete {
        volume: Id,
        entry: EntryId,
        version: Vec<u8>,
        metadata: Vec<u8>,
        recursive: bool,
    },
}

fn item(store: &LocalStore, volume: Id, entry: EntryId) -> Result<Value> {
    Ok(json!({"item": store.provider_item(volume, entry)?.in_account_domain(volume)}))
}
fn check_version(store: &LocalStore, volume: Id, entry: EntryId, version: &[u8]) -> Result<()> {
    let current = store.provider_item(volume, entry)?;
    if current.content_version != version {
        return Err(crate::store::Error::AlreadyExists("provider content version changed").into());
    }
    Ok(())
}
fn input(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > super::blob::MAX_FILE_BYTES {
        return Err("invalid provider content file".into());
    }
    Ok(file)
}

pub fn execute(store: &Arc<LocalStore>, blobs: &BlobTransport, bytes: &[u8]) -> Result<Value> {
    execute_request(store, blobs, bytes, false)?.finish(blobs)
}

/// Preserve permanent store failures at the native boundary. Treating them as
/// connectivity errors would make Finder retry an operation it cannot perform.
pub fn error_response(error: &(dyn std::error::Error + Send + Sync + 'static)) -> Value {
    use crate::store::Error;
    let code = match error.downcast_ref::<Error>() {
        Some(Error::AccessDenied(_) | Error::ParticipantRevoked { .. }) => "permission_denied",
        Some(Error::InvalidInput(_)) => "invalid_input",
        Some(Error::AlreadyExists(_)) => "version_changed",
        Some(Error::NotFound(_)) => "not_found",
        Some(Error::LimitExceeded) => "limit_exceeded",
        _ if error.is::<serde_json::Error>() => "invalid_input",
        _ => "unavailable",
    };
    json!({"error": error.to_string(), "code": code})
}

/// Read only the bounded canonical reference supplied for a database copy.
/// The iOS client snapshots these bytes privately before source initialization;
/// the subsequent commit checks both source and destination admission.
pub(crate) fn database_copy_source(bytes: &[u8]) -> Result<Option<DatabaseReference>> {
    if bytes.len() > REQUEST_BYTES as usize {
        return Err("provider request exceeds limit".into());
    }
    let mut value: Value = serde_json::from_slice(bytes)?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("account");
    }
    let Request::Create { name, directory: false, path: Some(path), .. } =
        serde_json::from_value(value)?
    else {
        return Ok(None);
    };
    if !name.ends_with(".pbdb") {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(crate::store::MAX_DATABASE_REFERENCE_BYTES);
    input(&path)?
        .take((crate::store::MAX_DATABASE_REFERENCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(Some(DatabaseReference::parse(&bytes)?))
}

enum Response {
    Local(Value),
    Push(Box<crate::network::ap::operation::ServiceRequest>),
}
impl Response {
    fn finish(self, blobs: &BlobTransport) -> Result<Value> {
        match self {
            Self::Local(value) => Ok(value),
            Self::Push(request) => blobs.register_provider_push(&request),
        }
    }
}

/// Registration captures identities and transport only. A remote acknowledgment
/// must not retain a store owner or block enrollment/local host requests.
pub(crate) fn push_request(
    store: &LocalStore,
    token: String,
    invalidated: bool,
    bundle: String,
    sandbox: bool,
) -> Result<crate::network::ap::operation::ServiceRequest> {
    store.provider_account()?;
    let operation_id = Id::generate()?.to_string();
    let request = if invalidated {
        crate::network::ap::operation::ServiceRequest::InvalidateFileProviderAccountPush {
            operation_id,
            token,
            bundle,
            sandbox,
        }
    } else {
        crate::network::ap::operation::ServiceRequest::RegisterFileProviderAccountPush {
            operation_id,
            token,
            bundle,
            sandbox,
        }
    };
    request
        .validate()
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
    Ok(request)
}

/// System domains require current device enrollment, independently of retained
/// recovery keys and local account data. This bounded credential read occurs on
/// the existing file worker while its store lease prevents account rebinding.
#[cfg(all(target_os = "macos", feature = "process"))]
fn execute_system_request(
    profile: &Path,
    store: &Arc<LocalStore>,
    blobs: &BlobTransport,
    bytes: &[u8],
) -> Result<Response> {
    let identity = crate::network::ap::ClientIdentity::load(&profile.join("instance.json"))?;
    if !identity.auth_token.is_empty()
        && Id::from_bytes(identity.account_id()?) == store.account().id
    {
        return execute_request(store, blobs, bytes, false);
    }
    if bytes.len() > REQUEST_BYTES as usize {
        return Err("provider request exceeds limit".into());
    }
    let mut value: Value = serde_json::from_slice(bytes)?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("account");
    }
    match serde_json::from_value::<Request>(value)? {
        Request::Volumes { .. } => {
            let (revision, _) = store.delivery_provider_revision(None, None);
            Ok(Response::Local(json!({
                "account": store.account().id, "volumes": [], "retired": [],
                "anchor": null, "changed": [], "revision": revision,
                "provider_ready": false, "connected": false,
            })))
        }
        _ => Err(crate::store::Error::AccessDenied("File Provider account is no longer signed in")
            .into()),
    }
}

fn execute_request(
    store: &Arc<LocalStore>,
    blobs: &BlobTransport,
    bytes: &[u8],
    independent: bool,
) -> Result<Response> {
    if bytes.len() > REQUEST_BYTES as usize {
        return Err("provider request exceeds limit".into());
    }
    let mut value: Value = serde_json::from_slice(bytes)?;
    if let Some(account) = value.as_object_mut().and_then(|fields| fields.remove("account"))
        && account.as_str() != Some(store.account().id.to_string().as_str())
    {
        return Err(crate::store::Error::AccessDenied(
            "File Provider account is no longer signed in",
        )
        .into());
    }
    let request: Request = serde_json::from_value(value)?;
    store.provider_account()?;
    let access = Access::owner(store.account().id);
    let response = match request {
        Request::Root => Ok(json!({"item": store.provider_root_item()?})),
        Request::VolumeItem { volume } => Ok(json!({"item": store.provider_volume_item(volume)?})),
        Request::CreateVolume { id, name } => {
            store.provider_create_volume(id, &name, independent)?;
            Ok(json!({"item": store.provider_volume_item(id)?}))
        }
        Request::RenameVolume { volume, name, metadata } => {
            store.provider_rename_volume(volume, &name, &metadata)?;
            Ok(json!({"item": store.provider_volume_item(volume)?}))
        }
        Request::PushToken { token, invalidated, bundle, sandbox } => {
            return Ok(Response::Push(Box::new(push_request(
                store,
                token,
                invalidated,
                bundle,
                sandbox,
            )?)));
        }
        Request::Catalog { known } => Ok(json!({
            "items": store.provider_volume_items()?,
            "retired": store.provider_retired_volumes(known.as_deref().unwrap_or_default())?,
        })),
        Request::Volumes { after, known, volume } => {
            // Capture before reading the catalog: concurrent changes must stay
            // after this fence and trigger the next event-driven refresh.
            let (revision, changed) = store.delivery_provider_revision(after, None);
            let revision = revision.ok_or("File Provider change history is unavailable")?;
            let retired = store.provider_retired_volumes(known.as_deref().unwrap_or_default())?;
            let anchor = match volume {
                Some(volume) => serde_json::to_value(store.provider_cached_anchor(volume)?)?,
                None => json!(store.provider_account_anchor()?),
            };
            let provider_ready = store.recovery_state()?.is_some_and(|state| state.acknowledged);
            if changed.is_none() {
                return Ok(Response::Local(
                    json!({"account": store.account().id, "volumes": Value::Null,
                    "retired": retired, "anchor": anchor, "changed": [], "revision": revision, "provider_ready":provider_ready, "connected": blobs.provider_connected()}),
                ));
            }
            let volumes = store.provider_volumes()?;
            let changed = volumes
                .iter()
                .filter(|volume| {
                    store.delivery_provider_revision(after, Some(&[volume.id])).1.is_some()
                })
                .map(|volume| volume.id)
                .collect::<Vec<_>>();
            Ok(
                json!({"account": store.account().id, "volumes": volumes, "retired": retired, "anchor": anchor, "changed": changed, "revision": revision, "provider_ready":provider_ready, "connected":blobs.provider_connected()}),
            )
        }
        Request::List { volume, parent, after } => {
            let page = store.provider_entries(volume, parent, after, PROVIDER_PAGE_ITEMS)?;
            let items = page
                .items
                .into_iter()
                .map(|item| item.in_account_domain(volume))
                .collect::<Vec<_>>();
            Ok(json!({"page": {"items": items, "next": page.next}}))
        }
        Request::Scan { volume, after } => {
            let page = store.provider_scan(volume, after)?;
            let items = page
                .items
                .into_iter()
                .map(|item| item.in_account_domain(volume))
                .collect::<Vec<_>>();
            Ok(json!({"items": items, "next": page.next}))
        }
        Request::Item { volume, entry } => item(store, volume, entry),
        Request::Anchor { volume } => Ok(json!({"anchor": store.provider_anchor(volume)?})),
        Request::Fetch { volume, entry, version, path } => {
            check_version(store, volume, entry, &version)?;
            let metadata = store.provider_item(volume, entry)?;
            let mut destination =
                fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
            let result = (|| -> Result<()> {
                if matches!(metadata.kind, EntryType::KvDb | EntryType::ObjectDb) {
                    destination
                        .write_all(DatabaseReference { volume, entry }.to_string().as_bytes())?;
                } else if metadata.kind == EntryType::Blob {
                    let (captured, file) = blobs.verified_file(
                        store.clone(),
                        access.clone(),
                        None,
                        EntryAddress { volume, entry },
                    )?;
                    let mut offset = 0;
                    while offset < file.len() {
                        let bytes =
                            file.read_range(offset, crate::store::BLOB_CHUNK_BYTES as u32)?;
                        destination.write_all(&bytes)?;
                        offset += bytes.len() as u64;
                    }
                    if store.blob_version(&access, captured.address, None)? != captured {
                        return Err("provider content version changed".into());
                    }
                } else {
                    return Err("cannot fetch a directory".into());
                }
                destination.sync_all()?;
                check_version(store, volume, entry, &version)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&path);
            }
            result?;
            item(store, volume, entry)
        }
        Request::Create { volume, id, parent, name, directory, path } => {
            store.provider_volume(volume)?;
            let database = !directory && name.ends_with(".pbdb");
            let name = filename(&name, database)?;
            // The OS retains template identity across an uncertain completion.
            // Only acknowledge an exact retry; never discard changed local bytes.
            let existing = match store.provider_item(volume, id) {
                Ok(existing) => Some(existing),
                Err(crate::store::Error::NotFound(_)) => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(existing) = &existing {
                if existing.parent != parent || store.entry(&access, volume, id)?.info.name != name
                {
                    return Err(
                        "provider creation identity already exists with different metadata".into(),
                    );
                }
                if directory && existing.kind == EntryType::Directory {
                    return item(store, volume, id).map(Response::Local);
                }
                if !database {
                    let expected =
                        store.blob_version(&access, EntryAddress { volume, entry: id }, None)?;
                    let (digest, length) = match &path {
                        Some(path) => hash_input(input(path)?)?,
                        None => (*blake3::hash(&[]).as_bytes(), 0),
                    };
                    if expected.digest != digest || expected.len_bytes != length {
                        return Err("provider creation retry has different content".into());
                    }
                    check_version(store, volume, id, &existing.content_version)?;
                    return item(store, volume, id).map(Response::Local);
                }
            }
            if directory {
                store.provider_create_directory(volume, id, parent, &name)?;
            } else if let Some(path) = path {
                let mut source = input(&path)?;
                if database {
                    let mut bytes = Vec::with_capacity(crate::store::MAX_DATABASE_REFERENCE_BYTES);
                    source
                        .take((crate::store::MAX_DATABASE_REFERENCE_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)?;
                    let reference = DatabaseReference::parse(&bytes)?;
                    store.provider_volume(reference.volume)?;
                    store.clone_database(
                        &access,
                        EntryAddress { volume: reference.volume, entry: reference.entry },
                        EntryAddress { volume, entry: id },
                        parent,
                        &name,
                    )?;
                } else {
                    let mut target =
                        store.file_destination(&access, volume, parent, &name, true)?;
                    target.set_creation_id(id)?;
                    store.publish_file(&access, &target, &mut source, true).map_err(|e| e.error)?;
                }
            } else {
                if database {
                    return Err("a database copy requires a .pbdb reference".into());
                }
                let mut target = store.file_destination(&access, volume, parent, &name, true)?;
                target.set_creation_id(id)?;
                store
                    .publish_file(&access, &target, &mut std::io::empty(), true)
                    .map_err(|e| e.error)?;
            }
            item(store, volume, id)
        }
        Request::Modify { volume, entry, parent, name, version, metadata, path } => {
            check_version(store, volume, entry, &version)?;
            let current = store.provider_item(volume, entry)?;
            let database = matches!(current.kind, EntryType::KvDb | EntryType::ObjectDb);
            let name = if name == current.filename {
                store.entry(&access, volume, entry)?.info.name
            } else {
                filename(&name, database)?
            };
            let mut uploaded = false;
            if let Some(path) = path {
                let mut source = input(&path)?;
                if database {
                    let mut bytes = Vec::with_capacity(crate::store::MAX_DATABASE_REFERENCE_BYTES);
                    source
                        .take((crate::store::MAX_DATABASE_REFERENCE_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)?;
                    if DatabaseReference::parse(&bytes)? != (DatabaseReference { volume, entry }) {
                        return Err("database reference contents cannot be replaced".into());
                    }
                } else {
                    let target = store
                        .provider_destination(volume, entry, parent, &name, &version, &metadata)?;
                    store.publish_file(&access, &target, &mut source, true).map_err(|e| e.error)?;
                    uploaded = true;
                }
            }
            if !uploaded {
                store.provider_change(
                    volume,
                    entry,
                    &version,
                    &metadata,
                    ProviderChange::Move { parent, name: &name },
                )?;
            }
            item(store, volume, entry)
        }
        Request::Delete { volume, entry, version, metadata, recursive } => {
            store.provider_change(
                volume,
                entry,
                &version,
                &metadata,
                ProviderChange::Delete { recursive },
            )?;
            Ok(json!({"deleted": true}))
        }
    }?;
    Ok(Response::Local(response))
}
fn hash_input(mut file: fs::File) -> Result<([u8; 32], u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut length = 0u64;
    let mut bytes = [0; crate::store::BLOB_CHUNK_BYTES];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        length = length.checked_add(count as u64).ok_or("provider content exceeds limit")?;
        if length > super::blob::MAX_FILE_BYTES {
            return Err("provider content exceeds limit".into());
        }
        hasher.update(&bytes[..count]);
    }
    Ok((*hasher.finalize().as_bytes(), length))
}
fn filename(name: &str, database: bool) -> Result<String> {
    let name =
        if database { name.strip_suffix(".pbdb").ok_or("databases require .pbdb")? } else { name };
    if name.is_empty() || name.contains('/') || name.len() > 255 {
        return Err(crate::store::Error::InvalidInput("invalid provider filename").into());
    }
    Ok(name.replace("~~", "~"))
}

#[cfg(all(target_os = "macos", feature = "process"))]
pub fn call(profile: &Path, bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > REQUEST_BYTES as usize {
        return Err("provider request exceeds limit".into());
    }
    let mut stream = UnixStream::connect(profile.join(SOCKET))?;
    super::ipc::verify_peer(&stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(90)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(bytes)?;
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    if size > REPLY_BYTES {
        return Err("provider response exceeds limit".into());
    }
    let mut reply = vec![0; size];
    stream.read_exact(&mut reply)?;
    Ok(reply)
}

#[cfg(all(target_os = "macos", feature = "process"))]
pub struct Server {
    cancel: UnixStream,
    identity: (u64, u64),
    active: Arc<AtomicUsize>,
    path: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}
#[cfg(all(target_os = "macos", feature = "process"))]
impl Server {
    pub fn start(
        profile: &Path,
        services: &super::Services,
        blobs: BlobTransport,
        changed: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let path = profile.join(SOCKET);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let metadata = fs::symlink_metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        let (cancel, cancelled) = UnixStream::pair()?;
        let active = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(changed);
        let (send, receive) = mpsc::sync_channel::<UnixStream>(WORKERS);
        let receive = Arc::new(std::sync::Mutex::new(receive));
        let mut workers = Vec::with_capacity(WORKERS);
        let store = services.provider_store.clone();
        *store.write() = Arc::downgrade(&services.store);
        for index in 0..WORKERS {
            let cancelled = cancelled.try_clone()?;
            let receive = receive.clone();
            let store = store.clone();
            let blobs = blobs.clone();
            let changed = changed.clone();
            let active = active.clone();
            let profile = profile.to_path_buf();
            workers.push(
                std::thread::Builder::new().name(format!("file-provider-{index}")).spawn(
                    move || loop {
                        let Ok(mut stream) =
                            receive.lock().expect("provider queue poisoned").recv()
                        else {
                            break;
                        };
                        let _ = serve(&mut stream, &cancelled, &store, &blobs, &profile);
                        active.fetch_sub(1, Ordering::AcqRel);
                        changed();
                    },
                )?,
            );
        }
        let admitted = active.clone();
        let thread = std::thread::Builder::new().name("file-provider".into()).spawn(move || {
            loop {
                match accept_provider(&listener, &cancelled) {
                    Ok(Some(stream)) => {
                        admitted.fetch_add(1, Ordering::AcqRel);
                        if send.try_send(stream).is_err() {
                            admitted.fetch_sub(1, Ordering::AcqRel);
                        }
                        changed();
                    }
                    Ok(None) => break,
                    Err(error) => {
                        crate::diagnostic!("File Provider admission: {error}");
                        break;
                    }
                }
            }
            drop(send);
            for worker in workers {
                let _ = worker.join();
            }
        })?;
        Ok(Self { cancel, identity, active, path, thread: Some(thread) })
    }
    pub(crate) fn busy(&self) -> bool {
        self.active.load(Ordering::Acquire) != 0
    }
}
#[cfg(all(target_os = "macos", feature = "process"))]
impl Drop for Server {
    fn drop(&mut self) {
        // EOF wakes admission even when the profile and socket were deleted.
        let _ = self.cancel.shutdown(std::net::Shutdown::Write);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
#[cfg(all(target_os = "macos", feature = "process"))]
#[allow(unsafe_code, reason = "poll borrows two live descriptors and initialized output.")]
fn accept_provider(listener: &UnixListener, cancelled: &UnixStream) -> Result<Option<UnixStream>> {
    use std::os::fd::AsRawFd;
    let mut events = [
        libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: cancelled.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: both descriptors remain owned throughout the cancellation-aware wait.
        if unsafe { libc::poll(events.as_mut_ptr(), 2, -1) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if events[1].revents != 0 {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(Some(stream));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
#[cfg(all(target_os = "macos", feature = "process"))]
fn serve(
    stream: &mut UnixStream,
    cancelled: &UnixStream,
    store: &RwLock<std::sync::Weak<LocalStore>>,
    blobs: &BlobTransport,
    profile: &Path,
) -> Result<()> {
    super::ipc::verify_peer(stream)?;
    stream.set_nonblocking(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut size = [0; 4];
    read_provider_request(stream, cancelled, &mut size, deadline)?;
    let size = u32::from_le_bytes(size) as u64;
    if size > REQUEST_BYTES {
        return Err("provider request exceeds limit".into());
    }
    let mut bytes = vec![0; size as usize];
    read_provider_request(stream, cancelled, &mut bytes, deadline)?;
    stream.set_nonblocking(false)?;
    // Partial clients do not retain the store. Hold a shared lease through
    // store access so binding can drain these owners without rejecting sign-in.
    let lease = store.read();
    let store = lease.upgrade().ok_or("File Provider store is closed")?;
    let response = execute_system_request(profile, &store, blobs, &bytes);
    drop(store);
    drop(lease);
    // Authentication opens the AP connection before local binding. Push
    // registration must not hold that binding behind its AP request.
    let reply = match response.and_then(|response| response.finish(blobs)) {
        Ok(value) => value,
        Err(error) => error_response(error.as_ref()),
    };
    let bytes = serde_json::to_vec(&reply)?;
    if bytes.len() > REPLY_BYTES {
        return Err("provider response exceeds limit".into());
    }
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

#[cfg(all(target_os = "macos", feature = "process"))]
#[allow(unsafe_code, reason = "poll borrows the live request and cancellation descriptors.")]
fn read_provider_request(
    stream: &mut UnixStream,
    cancelled: &UnixStream,
    mut bytes: &mut [u8],
    deadline: std::time::Instant,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut events = [
        libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: cancelled.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    // Unadmitted partial requests must not delay engine retirement. Once a
    // complete request starts, its owner still finishes the accepted operation.
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("File Provider request timed out".into());
        }
        let millis = remaining.as_millis().saturating_add(1).min(5000) as i32;
        // SAFETY: two initialized entries and owned sockets, bounded deadline.
        if unsafe { libc::poll(events.as_mut_ptr(), 2, millis) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if events[1].revents != 0 {
            return Err("File Provider is stopping".into());
        }
        match stream.read(bytes) {
            Ok(0) => return Err("Incomplete File Provider request".into()),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(feature = "ios")]
pub(crate) struct AppSnapshot {
    pub revision: (Id, u64),
    pub account: Option<Value>,
}

/// Credentials cross only the containing app's protected App Group boundary.
/// Backing edits skip catalog reads through the existing delivery history.
#[cfg(feature = "ios")]
pub(crate) fn app_snapshot(
    engine: &super::Engine,
    previous: Option<(Id, u64)>,
    known: &[Id],
) -> Result<AppSnapshot> {
    let store = &engine.services.store;
    let (revision, changed) = store.delivery_catalog_revision(previous);
    let revision = revision.ok_or("File Provider store is closed")?;
    if changed.is_none() {
        return Ok(AppSnapshot { revision, account: None });
    }
    if engine.channel() != crate::release_channel::ReleaseChannel::Public || !engine.enrolled() {
        return Ok(AppSnapshot { revision, account: Some(json!({})) });
    }
    let Some(recovery) = store.recovery_state()?.filter(|state| state.acknowledged) else {
        return Ok(AppSnapshot { revision, account: Some(json!({})) });
    };
    let volumes = store.provider_volumes()?;
    Ok(AppSnapshot {
        revision,
        account: Some(json!({
            "account": store.account().id,
            "email": engine.email().ok_or("File Provider account email unavailable")?,
            "secret": recovery.secret,
            "volumes": volumes.iter().map(|v| json!({"id":v.id, "name":v.name})).collect::<Vec<_>>(),
            "retired": store.provider_retired_volumes(known)?,
        })),
    })
}
