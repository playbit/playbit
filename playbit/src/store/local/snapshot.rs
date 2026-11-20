//! Bounded immutable volume cuts. No mount, copy command or current catalog.
use super::*;
use crate::store::{
    DatabaseId, EntryType,
    object_contract::Principal,
    volume_index::{BlobVersion, EntryAddress, EntryHeader, EntryKind, IndexSnapshot},
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_CAPTURES: usize = 2;
const MAX_ATTEMPTS: usize = 2;
const COPY_BYTES: usize = 64 * 1024;
const CACHE_BYTES: usize = 512 * 1024;
const CHANGED: &str = "volume changed during snapshot capture";
const MANIFEST_FILE: &str = ".snapshot.kvdb";

mod duplicate;
mod rejected;

#[derive(Default)]
pub(super) struct SnapshotCaptures {
    active: AtomicUsize,
    #[cfg(test)]
    #[allow(clippy::type_complexity, reason = "Inject one mutation between real backing copies.")]
    pub(super) after_copy: Mutex<Option<Arc<dyn Fn(DatabaseId) -> Result<()> + Send + Sync>>>,
}

fn next_capture(active: usize) -> Option<usize> {
    if active < MAX_CAPTURES { Some(active + 1) } else { None }
}

struct CaptureCredit<'a>(&'a AtomicUsize);
impl Drop for CaptureCredit<'_> {
    fn drop(&mut self) {
        let active = self.0.fetch_sub(1, Ordering::AcqRel);
        assert!(active > 0 && active <= MAX_CAPTURES, "capture releases its own credit");
    }
}

struct Cut {
    handles: [Option<VolumeOperation>; MAX_OPERATION_VOLUMES],
    epochs: [u64; MAX_OPERATION_VOLUMES],
}
impl Cut {
    fn check(&self) -> Result<()> {
        for (handle, epoch) in self.handles.iter().zip(self.epochs) {
            if let Some(gate) = handle
                && gate.write_epoch.snapshot().ok() != Some(epoch)
            {
                return Err(Error::LocalContention(CHANGED));
            }
        }
        Ok(())
    }

    fn lock(&self) -> Result<[Option<operation_gate::OperationGuard<'_>>; MAX_OPERATION_VOLUMES]> {
        let mut guards = std::array::from_fn(|_| None);
        for (slot, handle) in guards.iter_mut().zip(&self.handles) {
            if let Some(handle) = handle {
                *slot = Some(handle.lock()?);
            }
        }
        Ok(guards)
    }
}

/// Private immutable files admitted as one cut. Content authorization is checked
/// during capture; only listed backings and retained blob versions are exposed.
/// The owner must drop this artifact on a background thread.
pub struct VolumeSnapshot {
    volume: Id,
    directory: ProjectionDirectory,
}

#[derive(Serialize, Deserialize)]
struct BackingCut {
    root: ReplicaRoot,
    clock: u64,
    hash_key: [u8; 32],
}

impl BackingCut {
    fn check(&self, database: &Database) -> Result<()> {
        let snapshot = database.begin_read()?;
        if snapshot.root_hash()? != self.root
            || snapshot.clock()?.as_u64() != self.clock
            || database.meta().hash_key != self.hash_key
        {
            return Err(Error::corrupt(
                "snapshot database root, clock or hash key differs from its manifest",
            ));
        }
        Ok(())
    }
}
impl VolumeSnapshot {
    pub fn volume(&self) -> Id {
        self.volume
    }

    pub fn with_database<T>(
        &self,
        database: DatabaseId,
        f: impl FnOnce(kvdb::ReadTxn<'_, RedbStorage>) -> Result<T>,
    ) -> Result<T> {
        let manifest = self.manifest()?;
        let cut = get::<BackingCut>(&manifest, database_key(database).as_bytes())?
            .ok_or(Error::AccessDenied("snapshot database content"))?;
        let db = open_replica(&self.directory.0.join(database.file_name()), CACHE_BYTES)?;
        cut.check(&db)?;
        drop(manifest);
        f(db.begin_read()?)
    }

    pub fn open_blob(&self, version: &BlobVersion) -> Result<fs::File> {
        if version.address.volume != self.volume {
            return Err(Error::AccessDenied("snapshot blob volume"));
        }
        let manifest = self.manifest()?;
        let admitted: BlobVersion = required(&manifest, blob_key(version))?;
        if &admitted != version {
            return Err(Error::AccessDenied("snapshot blob version"));
        }
        fs::File::open(self.directory.0.join(crate::paths::BLOBS).join(hex(&version.digest)))
            .map_err(io)
    }

    fn manifest(&self) -> Result<Database> {
        let manifest = open_replica(&self.directory.0.join(MANIFEST_FILE), CACHE_BYTES)?;
        if get::<Id>(&manifest, b"complete")? != Some(self.volume) {
            return Err(Error::NotFound("complete volume snapshot"));
        }
        Ok(manifest)
    }
}

fn database_key(database: DatabaseId) -> String {
    format!("database/{}", hex(&database.to_bytes()))
}

fn blob_key(version: &BlobVersion) -> String {
    format!("blob/{}/{}", version.address.entry, version.revision)
}

struct Capture {
    snapshot: VolumeSnapshot,
    manifest: Database,
    cut: Cut,
}

impl LocalStore {
    /// Background-only snapshot preparation for every readable named database,
    /// append-log checkpoint and blob version, including retired recovery data.
    pub fn capture_volume(&self, access: &Access, volume: Id) -> Result<VolumeSnapshot> {
        self.snapshots
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, next_capture)
            .map_err(|_| Error::LocalContention("both volume snapshot slots are in use"))?;
        let _credit = CaptureCredit(&self.snapshots.active);
        for attempt in 0..MAX_ATTEMPTS {
            match self.capture_once(access, volume) {
                Err(Error::LocalContention(CHANGED)) if attempt + 1 < MAX_ATTEMPTS => {}
                result => return result,
            }
        }
        unreachable!("the final bounded attempt returns its result")
    }

    fn begin_capture(&self, access: &Access, volume: Id) -> Result<Capture> {
        let cut = {
            let operations = self.access_operations(access, volume)?;
            let _guards = operations.lock()?;
            self.authorize(access, &self.index(volume)?, Permission::Read)?;
            let mut epochs = [0; MAX_OPERATION_VOLUMES];
            for (slot, gate) in operations.handles.iter().enumerate() {
                if let Some(gate) = gate {
                    epochs[slot] =
                        gate.write_epoch.snapshot().map_err(|_| Error::LocalContention(CHANGED))?;
                }
            }
            Cut { epochs, handles: operations.handles.clone() }
        };
        let parent = self.path.join(crate::paths::STAGING).join("snapshots");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&parent).map_err(io)?;
        let directory = ProjectionDirectory(parent.join(Id::generate()?.to_string()));
        fs::DirBuilder::new().mode(0o700).create(&directory.0).map_err(io)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(directory.0.join(crate::paths::BLOBS))
            .map_err(io)?;
        let manifest = create_db(&directory.0.join(MANIFEST_FILE), None, CACHE_BYTES)?;
        Ok(Capture { snapshot: VolumeSnapshot { volume, directory }, manifest, cut })
    }

    fn capture_once(&self, access: &Access, volume: Id) -> Result<VolumeSnapshot> {
        let capture = self.begin_capture(access, volume)?;
        self.capture_database(&capture, DatabaseId::Index)?;
        self.capture_database(&capture, DatabaseId::Revocations)?;
        {
            let index = open_replica(
                &capture.snapshot.directory.0.join(DatabaseId::Index.file_name()),
                CACHE_BYTES,
            )?;
            let (volume, defaults) = {
                let policy = MetadataPolicy::read(&index)?;
                (policy.volume().clone(), policy.default_permissions().clone())
            };
            let snapshot = IndexSnapshot::new(index.begin_read()?);
            let mut after = None;
            loop {
                capture.cut.check()?;
                let page = snapshot.headers(after, 64)?;
                for entry in page.entries {
                    self.capture_entry(&capture, &snapshot, &volume, &defaults, access, entry)?;
                }
                let Some(next) = page.next else { break };
                after = Some(next);
            }
        }
        self.capture_rejected(&capture, access)?;
        {
            let operations = self.access_operations(access, volume)?;
            let _guards = operations.lock()?;
            capture.cut.check()?;
            self.authorize(access, &self.index(volume)?, Permission::Read)?;
        }
        initialization::sync_directory(&capture.snapshot.directory.0.join(crate::paths::BLOBS))?;
        initialization::sync_directory(&capture.snapshot.directory.0)?;
        put(&capture.manifest, b"complete", &volume)?;
        Ok(capture.snapshot)
    }

    fn capture_database(&self, capture: &Capture, database: DatabaseId) -> Result<()> {
        let source;
        let pinned;
        let expected;
        {
            let _guards = capture.cut.lock()?;
            capture.cut.check()?;
            source = self.db(capture.snapshot.volume, database)?;
            pinned = source.begin_read()?;
            expected = BackingCut {
                root: pinned.root_hash()?,
                clock: pinned.clock()?.as_u64(),
                hash_key: source.meta().hash_key,
            };
            capture.cut.check()?;
        }
        let source_path = self
            .path
            .join(crate::paths::VOLUMES)
            .join(capture.snapshot.volume.to_string())
            .join(database.file_name());
        let target_path = capture.snapshot.directory.0.join(database.file_name());
        copy_file(&mut fs::File::open(source_path).map_err(io)?, &target_path, &capture.cut)?;
        capture.cut.check()?;
        let copied = open_replica(&target_path, CACHE_BYTES)?;
        expected.check(&copied)?;
        drop(copied);
        drop(pinned);
        drop(source);
        put(&capture.manifest, database_key(database).as_bytes(), &expected)?;
        #[cfg(test)]
        if let Some(hook) = self
            .snapshots
            .after_copy
            .lock()
            .map_err(|_| Error::Internal("snapshot copy hook lock is poisoned"))?
            .clone()
        {
            hook(database)?;
        }
        capture.cut.check()
    }

    fn capture_entry(
        &self,
        capture: &Capture,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        volume: &Volume,
        defaults: &Acl,
        access: &Access,
        entry: EntryHeader,
    ) -> Result<()> {
        match snapshot.authorize_retained_read(
            volume,
            defaults,
            Principal::Human(access.account),
            entry.id,
        ) {
            Ok(()) => {}
            Err(Error::AccessDenied(_)) => return Ok(()),
            Err(error) => return Err(error),
        }
        if entry.kind == EntryType::Directory {
            return Ok(());
        }
        if !snapshot.backing_ready(entry.id)? {
            return Err(Error::NotFound("snapshot entry backing is unavailable"));
        }
        if entry.kind != EntryType::Blob {
            return self.capture_database(capture, DatabaseId::Entry(entry.id));
        }
        let address = EntryAddress { volume: volume.id, entry: entry.id };
        let mut after = None;
        loop {
            capture.cut.check()?;
            let page = snapshot.revisions(entry.id, after, 64)?;
            for revision in page.revisions {
                match snapshot.retained_blob_kind(entry.id, Some(revision.revision)) {
                    Ok((revision, EntryKind::Blob { len_bytes, digest, encrypted_key, .. })) => {
                        self.capture_blob(
                            capture,
                            &BlobVersion { address, revision, len_bytes, digest, encrypted_key },
                        )?;
                    }
                    Ok(_) => {
                        return Err(Error::corrupt(
                            "snapshot blob differs from its declared version",
                        ));
                    }
                    Err(Error::NotFound("blob version")) => {}
                    Err(error) => return Err(error),
                }
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        Ok(())
    }

    fn capture_blob(&self, capture: &Capture, version: &BlobVersion) -> Result<()> {
        let content_key = format!("content/{}", hex(&version.digest));
        match get::<u64>(&capture.manifest, content_key.as_bytes())? {
            Some(len) if len != version.len_bytes => {
                return Err(Error::corrupt(
                    "snapshot blob length differs between references to the same digest",
                ));
            }
            Some(_) => {}
            None => {
                let target = capture
                    .snapshot
                    .directory
                    .0
                    .join(crate::paths::BLOBS)
                    .join(hex(&version.digest));
                let (mut source, linked) = self.retain_snapshot_blob(&version.digest, &target)?;
                if !linked {
                    copy_file(&mut source, &target, &capture.cut)?;
                }
                verify_blob_file(&target, version, &capture.cut)?;
                put(&capture.manifest, content_key.as_bytes(), &version.len_bytes)?;
            }
        }
        put(&capture.manifest, blob_key(version).as_bytes(), version)
    }

    fn retain_snapshot_blob(&self, digest: &[u8; 32], target: &Path) -> Result<(fs::File, bool)> {
        let _guard =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        for recoverable in [false, true] {
            let path = self.blob_path(digest, recoverable);
            let source = match fs::File::open(&path) {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io(error)),
            };
            let linked = match fs::hard_link(&path, target) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => false,
                Err(error) => return Err(io(error)),
            };
            return Ok((source, linked));
        }
        Err(Error::NotFound("snapshot blob content is unavailable"))
    }
}

fn copy_file(source: &mut fs::File, target: &Path, cut: &Cut) -> Result<()> {
    let length = source.metadata().map_err(io)?.len();
    let mut source = source.take(length);
    let mut target =
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(target).map_err(io)?;
    let mut buffer = [0; COPY_BYTES];
    let mut copied = 0u64;
    loop {
        cut.check()?;
        let count = source.read(&mut buffer).map_err(io)?;
        if count == 0 {
            break;
        }
        target.write_all(&buffer[..count]).map_err(io)?;
        copied += count as u64;
    }
    if copied != length {
        return Err(Error::corrupt("snapshot blob copy length differs from its declaration"));
    }
    rustix::io::retry_on_intr(|| rustix::fs::fsync(&target)).map_err(|e| io(e.into()))?;
    cut.check()
}

fn verify_blob_file(path: &Path, version: &BlobVersion, cut: &Cut) -> Result<()> {
    let mut source = fs::File::open(path).map_err(io)?;
    if source.metadata().map_err(io)?.len() != version.len_bytes {
        return Err(Error::corrupt("snapshot blob file length differs from its declaration"));
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; COPY_BYTES];
    loop {
        cut.check()?;
        let count = source.read(&mut buffer).map_err(io)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if hasher.finalize().as_bytes() != &version.digest {
        return Err(Error::corrupt("snapshot blob digest differs from its declaration"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
