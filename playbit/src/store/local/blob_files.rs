//! Verified immutable file handles and private bounded transfer staging.
use super::*;
use crate::store::{BLOB_CHUNK_BYTES, MAX_BLOB_PLAINTEXT_BYTES, volume_index::BlobVersion};
use std::io::{Read, Seek};
use std::os::unix::fs::{FileExt, MetadataExt};

#[cfg(test)]
type ReadLeaseHook = fn(&LocalStore, &Access, &BlobVersion);

#[cfg(test)]
thread_local! {
    pub(in crate::store::local) static CRASH_AFTER_QUARANTINE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static READ_LEASE_RESERVED: std::cell::Cell<Option<ReadLeaseHook>> = const {
        std::cell::Cell::new(None)
    };
}

/// Aggregate per-store admission, including metadata-only views. Equal digests
/// share their storage charge; each view still consumes a reader slot.
pub const BLOB_DESCRIPTOR_LIMIT: usize = 32;
const MAX_BLOB_READERS: usize = BLOB_DESCRIPTOR_LIMIT;
const MAX_BLOB_READ_BYTES: u64 = 256 * 1024 * 1024;
const _: () = assert!(MAX_BLOB_READ_BYTES >= MAX_BLOB_PLAINTEXT_BYTES);
const _: () = assert!(MAX_BLOB_READERS * size_of::<(BlobVersion, [u8; 32], u32)>() <= 8 * 1024);
const _: () = assert!(MAX_BLOB_READERS * size_of::<(BlobVersion, [u8; 32])>() <= 8 * 1024);

pub(super) struct BlobCache {
    // Last field records an over-budget eviction deferred by this pin.
    pins: std::collections::BTreeMap<String, (u32, u64, bool)>,
    readers: usize,
    bytes: u64,
    #[cfg(any(
        feature = "connected-engine",
        feature = "native-runtime",
        feature = "wasm-runtime"
    ))]
    verified: descriptors::VerifiedCache,
    // At most MAX_BLOB_READERS exact identities (at most 8 KiB of payload).
    // These preserve an admitted view, never its caller's authority.
    captures: Vec<(BlobVersion, [u8; 32], u32)>,
}
impl Default for BlobCache {
    fn default() -> Self {
        Self {
            pins: Default::default(),
            readers: 0,
            bytes: 0,
            #[cfg(any(
                feature = "connected-engine",
                feature = "native-runtime",
                feature = "wasm-runtime"
            ))]
            verified: Default::default(),
            captures: Vec::with_capacity(MAX_BLOB_READERS),
        }
    }
}
impl BlobCache {
    #[cfg(any(feature = "connected-engine", feature = "native-runtime", feature = "wasm-runtime"))]
    pub(super) fn expire_verified(&mut self, generation: u64) {
        self.verified.expire_evicted(generation);
    }

    pub(super) fn defer_pinned(&mut self, name: &str, eligible: bool) -> bool {
        if let Some(pin) = self.pins.get_mut(name) {
            pin.2 |= eligible;
            true
        } else {
            false
        }
    }
    pub(super) fn clear_deferred(&mut self) {
        for pin in self.pins.values_mut() {
            pin.2 = false;
        }
    }
}

#[cfg(any(feature = "connected-engine", feature = "native-runtime", feature = "wasm-runtime"))]
mod descriptors {
    use super::*;
    pub(super) struct Slot {
        version: BlobVersion,
        access: Access,
        file: Arc<VerifiedBlob>,
        eviction_generation: u64,
    }
    pub(super) struct VerifiedCache {
        pub(super) slots: Vec<Slot>,
    }
    impl Default for VerifiedCache {
        fn default() -> Self {
            Self { slots: Vec::with_capacity(BLOB_DESCRIPTOR_LIMIT) }
        }
    }
    fn cache_capacity(count: usize, reclaimable: bool) -> bool {
        count < BLOB_DESCRIPTOR_LIMIT || count == BLOB_DESCRIPTOR_LIMIT && reclaimable
    }
    impl VerifiedCache {
        pub(super) fn expire_evicted(&mut self, generation: u64) {
            self.slots.retain(|slot| slot.eviction_generation == generation);
        }
        pub(super) fn get(
            &mut self,
            version: &BlobVersion,
            access: &Access,
            generation: u64,
        ) -> Option<Arc<VerifiedBlob>> {
            self.expire_evicted(generation);
            let index =
                self.slots.iter().position(|s| s.version == *version && s.access == *access)?;
            let slot = self.slots.remove(index);
            let file = slot.file.clone();
            self.slots.push(slot);
            Some(file)
        }
        pub(super) fn insert(
            &mut self,
            version: BlobVersion,
            access: Access,
            file: Arc<VerifiedBlob>,
            eviction_generation: u64,
        ) -> Result<()> {
            if self.get(&version, &access, eviction_generation).is_some() {
                return Ok(());
            }
            let reclaim = self.slots.iter().position(|s| Arc::strong_count(&s.file) == 1);
            if !cache_capacity(self.slots.len(), reclaim.is_some()) {
                return Err(Error::LimitExceeded);
            }
            if self.slots.len() == BLOB_DESCRIPTOR_LIMIT {
                self.slots
                    .remove(reclaim.expect("full cache admission requires a reclaimable slot"));
            }
            self.slots.push(Slot { version, access, file, eviction_generation });
            assert!(self.slots.len() <= BLOB_DESCRIPTOR_LIMIT);
            Ok(())
        }
        pub(super) fn invalidate(&mut self, version: &BlobVersion) {
            self.slots.retain(|slot| slot.version != *version);
        }
    }
}

/// Keeps a captured revision's storage charge until its last owner drops it.
/// This lease confers no authority to read contents or metadata.
pub struct BlobReadLease {
    cache: Arc<Mutex<BlobCache>>,
    name: Option<String>,
    version: BlobVersion,
    creation: [u8; 32],
}
impl BlobReadLease {
    /// Current authority and the original birth must both still admit this lease.
    pub(crate) fn validate(&self, store: &LocalStore, access: &Access) -> Result<()> {
        if store.admitted_blob_creation(access, &self.version)? != self.creation {
            return Err(Error::AccessDenied("captured entry creation changed"));
        }
        Ok(())
    }

    /// Returns whether this last pin deferred a recoverable cache eviction.
    pub(crate) fn release(&mut self) -> bool {
        let Some(name) = self.name.take() else { return false };
        let mut cache = self.cache.lock().expect("blob cache lock is not poisoned");
        let (references, length, deferred) =
            cache.pins.get_mut(&name).expect("active read lease has a storage charge");
        *references -= 1;
        let reclaim = *references == 0 && *deferred;
        if *references == 0 {
            let length = *length;
            cache.pins.remove(&name);
            cache.bytes -= length;
        }
        let index = cache
            .captures
            .iter()
            .position(|(version, creation, _)| {
                *version == self.version && *creation == self.creation
            })
            .expect("active read lease retains its captured identity");
        cache.captures[index].2 -= 1;
        if cache.captures[index].2 == 0 {
            cache.captures.swap_remove(index);
        }
        cache.readers -= 1;
        reclaim
    }
}
impl Drop for BlobReadLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// One verified file, at most 1024 block digests, and no content-sized buffer.
/// Range reads verify only the one or two physical blocks they touch.
pub struct VerifiedBlob {
    pub(super) file: fs::File,
    length: u64,
    blocks: Vec<[u8; 32]>,
}
impl VerifiedBlob {
    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn read_range(&self, offset: u64, limit: u32) -> Result<Vec<u8>> {
        if offset > self.length || limit == 0 || limit as usize > BLOB_CHUNK_BYTES {
            return Err(Error::InvalidInput("invalid blob read range"));
        }
        let end = offset.saturating_add(u64::from(limit)).min(self.length);
        let mut bytes = Vec::with_capacity((end - offset) as usize);
        let mut position = offset;
        let mut buffer = [0; BLOB_CHUNK_BYTES];
        while position < end {
            let index = position / BLOB_CHUNK_BYTES as u64;
            let start = index * BLOB_CHUNK_BYTES as u64;
            let count = (self.length - start).min(BLOB_CHUNK_BYTES as u64) as usize;
            self.file
                .read_exact_at(&mut buffer[..count], start)
                .map_err(|error| Error::read_failure("read verified blob block", error))?;
            if self.blocks.get(index as usize) != Some(blake3::hash(&buffer[..count]).as_bytes()) {
                return Err(Error::corrupt("verified blob block digest changed"));
            }
            let from = (position - start) as usize;
            let to = count.min((end - start) as usize);
            bytes.extend_from_slice(&buffer[from..to]);
            position = start + to as u64;
        }
        Ok(bytes)
    }
}

fn verify_file(mut file: fs::File, digest: &[u8; 32], length: u64) -> Result<VerifiedBlob> {
    if length > MAX_BLOB_PLAINTEXT_BYTES {
        return Err(Error::LimitExceeded);
    }
    if file.metadata().map_err(io)?.len() != length {
        return Err(Error::corrupt("blob file length differs from its declaration"));
    }
    let mut blocks = Vec::with_capacity(length.div_ceil(BLOB_CHUNK_BYTES as u64) as usize);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; BLOB_CHUNK_BYTES];
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(BLOB_CHUNK_BYTES as u64) as usize;
        file.read_exact(&mut buffer[..count])
            .map_err(|error| Error::read_failure("read blob during verification", error))?;
        hasher.update(&buffer[..count]);
        blocks.push(*blake3::hash(&buffer[..count]).as_bytes());
        remaining -= count as u64;
    }
    if hasher.finalize().as_bytes() != digest || file.read(&mut buffer[..1]).map_err(io)? != 0 {
        return Err(Error::corrupt("blob digest differs from its declaration"));
    }
    file.rewind().map_err(io)?;
    Ok(VerifiedBlob { file, length, blocks })
}

#[cfg(any(feature = "connected-engine", feature = "native-runtime", feature = "wasm-runtime"))]
impl LocalStore {
    pub(crate) fn cached_verified_blob(
        &self,
        version: &BlobVersion,
        access: &Access,
    ) -> Result<Option<Arc<VerifiedBlob>>> {
        let mut cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        Ok(cache.verified.get(version, access, self.blob_eviction_generation()))
    }
    /// Callers have verified contents and current authority. The generation
    /// rejects a descriptor prepared across eviction, under the same cache lock.
    pub(crate) fn cache_verified_blob(
        &self,
        version: BlobVersion,
        access: Access,
        file: Arc<VerifiedBlob>,
        generation: u64,
    ) -> Result<()> {
        let mut cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        let current = self.blob_eviction_generation();
        cache.verified.expire_evicted(current);
        if generation == current {
            cache.verified.insert(version, access, file, generation)?;
        }
        Ok(())
    }
    pub(crate) fn invalidate_verified_blob(&self, version: &BlobVersion) -> Result<()> {
        let mut cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        cache.verified.invalidate(version);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn verified_blob_count(&self) -> usize {
        self.blob_cache.lock().expect("blob cache lock is not poisoned").verified.slots.len()
    }
}

impl LocalStore {
    /// Background-only verification. Hold a descriptor through cache eviction,
    /// then recheck the declared version and current permission before return.
    pub fn verify_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<VerifiedBlob> {
        self.verify_blob_for(access, version, Permission::Read)
    }

    /// Reserve storage for one captured revision before exposing a read view.
    /// Reserving absent content also protects bytes hydrated later in its lifetime.
    pub fn lease_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<BlobReadLease> {
        if version.len_bytes > MAX_BLOB_PLAINTEXT_BYTES {
            return Err(Error::LimitExceeded);
        }
        let creation = self.admitted_blob_creation(access, version)?;
        let name = hex(&version.digest);
        {
            let mut cache = self
                .blob_cache
                .lock()
                .map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
            if cache.readers == MAX_BLOB_READERS {
                return Err(Error::LimitExceeded);
            }
            if let Some((references, length, _)) = cache.pins.get_mut(&name) {
                if *length != version.len_bytes {
                    return Err(Error::corrupt("shared blob length differs from its declaration"));
                }
                *references += 1;
            } else {
                let bytes =
                    cache.bytes.checked_add(version.len_bytes).ok_or(Error::LimitExceeded)?;
                if bytes > MAX_BLOB_READ_BYTES {
                    return Err(Error::LimitExceeded);
                }
                cache.pins.insert(name.clone(), (1, version.len_bytes, false));
                cache.bytes = bytes;
            }
            if let Some((_, _, references)) = cache
                .captures
                .iter_mut()
                .find(|(prior, birth, _)| prior == version && *birth == creation)
            {
                *references += 1;
            } else {
                assert!(cache.captures.len() < MAX_BLOB_READERS);
                cache.captures.push((version.clone(), creation, 1));
            }
            cache.readers += 1;
        }
        let mut lease = BlobReadLease {
            cache: self.blob_cache.clone(),
            name: Some(name),
            version: version.clone(),
            creation,
        };
        #[cfg(test)]
        if let Some(check) = READ_LEASE_RESERVED.take() {
            check(self, access, version);
        }
        if let Err(error) = lease.validate(self, access) {
            if lease.release()
                && let Err(cleanup) = self.enforce_cache_budget()
            {
                eprintln!("Could not reclaim blob cache after read admission failed: {cleanup}");
            }
            return Err(error);
        }
        Ok(lease)
    }

    /// Transport-only Write verification never supplies a plaintext read lease
    /// to an app. Public readers always enter through verify_blob_version.
    pub(super) fn verify_blob_for(
        &self,
        access: &Access,
        version: &BlobVersion,
        permission: Permission,
    ) -> Result<VerifiedBlob> {
        self.check_blob_version(access, version, permission)?;
        let _preparation = entry_blobs::Preparation::enter(&self.entry_blobs.active)?;
        let file = {
            let _cache = self
                .blob_cache
                .lock()
                .map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
            self.open_entry_blob(&version.digest).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Error::NotFound("blob content")
                } else {
                    io(error)
                }
            })?
        };
        let verified = verify_file(file, &version.digest, version.len_bytes)?;
        self.check_blob_version(access, version, permission)?;
        Ok(verified)
    }

    pub(super) fn blob_stage_directory(&self, recoverable: bool) -> Result<PathBuf> {
        if recoverable {
            Ok(self
                .blob_cache_path
                .parent()
                .ok_or(Error::Internal("blob cache path has no parent"))?
                .join("blob-staging"))
        } else {
            Ok(self.path.join(crate::paths::STAGING).join(crate::paths::BLOBS))
        }
    }

    pub(super) fn inspect_blob(&self, version: &BlobVersion, path: &Path) -> Result<ExistingBlob> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ExistingBlob::Missing);
            }
            Err(error) => return Err(io(error)),
        };
        let fingerprint = Fingerprint::read(&file.metadata().map_err(io)?);
        match verify_file(file.try_clone().map_err(io)?, &version.digest, version.len_bytes) {
            Ok(_) => Ok(ExistingBlob::Verified(fingerprint)),
            Err(Error::Corrupt(_)) => {
                file.rewind().map_err(io)?;
                let mut retained = BlobStage::new(&self.path.join("staging/corrupt-blobs"))?;
                copy_corrupt(&mut file, &mut retained.file, fingerprint.length)?;
                retained.sync()?;
                if Fingerprint::read(&file.metadata().map_err(io)?) != fingerprint {
                    return Err(Error::LocalContention("corrupt blob changed during retention"));
                }
                let destination = self.path.join(crate::paths::CORRUPT_BLOBS).join(format!(
                    "{}-{}",
                    hex(&version.digest),
                    Id::generate()?
                ));
                Ok(ExistingBlob::Corrupt { fingerprint, retained, destination })
            }
            Err(error) => Err(error),
        }
    }
}

fn copy_corrupt(source: &mut fs::File, target: &mut fs::File, length: u64) -> Result<()> {
    if length > MAX_BLOB_PLAINTEXT_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut remaining = length;
    let mut buffer = [0; BLOB_CHUNK_BYTES];
    while remaining > 0 {
        let count = remaining.min(BLOB_CHUNK_BYTES as u64) as usize;
        source.read_exact(&mut buffer[..count]).map_err(io)?;
        std::io::Write::write_all(target, &buffer[..count]).map_err(io)?;
        remaining -= count as u64;
    }
    if source.read(&mut buffer[..1]).map_err(io)? != 0 {
        return Err(Error::LocalContention("corrupt blob changed during retention"));
    }
    Ok(())
}

pub(super) enum ExistingBlob {
    Missing,
    Verified(Fingerprint),
    Corrupt { fingerprint: Fingerprint, retained: BlobStage, destination: PathBuf },
}
impl ExistingBlob {
    /// A prior ordinary close may have linked this inode without syncing it.
    /// Sync before taking the profile writer. Publication rechecks its fingerprint
    /// and uses the already-synced stage if the inspected file disappears.
    pub(super) fn sync_verified(&self, target: &Path) -> Result<bool> {
        let Self::Verified(expected) = self else { return Ok(true) };
        let file = match fs::File::open(target) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io(error)),
        };
        if Fingerprint::read(&file.metadata().map_err(io)?) != *expected {
            return Ok(false);
        }
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&file)).map_err(|error| io(error.into()))?;
        Ok(true)
    }

    /// Both inputs are prepared outside gates. The caller retains its cache
    /// guard and current admission through publication; quarantine is durable
    /// before an unchanged corrupt target is replaced with verified bytes.
    pub(super) fn publish(&self, stage: &BlobStage, target: &Path, durable: bool) -> Result<bool> {
        let Self::Corrupt { fingerprint, retained, destination } = self else {
            return stage.publish(
                target,
                match self {
                    Self::Missing => None,
                    Self::Verified(fingerprint) => Some(fingerprint),
                    Self::Corrupt { .. } => unreachable!("corrupt publication has its own branch"),
                },
                durable,
            );
        };
        if !unchanged(target, fingerprint)? {
            return Ok(false);
        }
        if !retained.publish(destination, None, true)? {
            return Err(Error::AlreadyExists("corrupt blob retention identity"));
        }
        initialization::sync_directory(
            destination
                .parent()
                .ok_or(Error::Internal("blob publication path has no parent"))?
                .parent()
                .ok_or(Error::Internal("blob publication path has no parent"))?,
        )?;
        #[cfg(test)]
        if CRASH_AFTER_QUARANTINE.get() {
            std::process::exit(73);
        }
        if !unchanged(target, fingerprint)? {
            return Ok(false);
        }
        let replacement = stage.directory.0.join(Id::generate()?.to_string());
        fs::hard_link(stage.directory.0.join("content"), &replacement).map_err(io)?;
        fs::rename(replacement, target).map_err(io)?;
        initialization::sync_directory(
            target.parent().ok_or(Error::Internal("blob retention path has no parent"))?,
        )?;
        Ok(true)
    }
}

fn unchanged(target: &Path, fingerprint: &Fingerprint) -> Result<bool> {
    match fs::metadata(target) {
        Ok(metadata) => Ok(fingerprint == &Fingerprint::read(&metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(error)),
    }
}

#[derive(Eq, PartialEq)]
pub(super) struct Fingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl Fingerprint {
    fn read(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

pub(super) struct BlobStage {
    pub(super) file: fs::File,
    directory: ProjectionDirectory,
}
impl BlobStage {
    pub(super) fn new(parent: &Path) -> Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(io)?;
        let directory = ProjectionDirectory(parent.join(Id::generate()?.to_string()));
        fs::DirBuilder::new().mode(0o700).create(&directory.0).map_err(io)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.0.join("content"))
            .map_err(io)?;
        Ok(Self { file, directory })
    }

    pub(super) fn sync(&self) -> Result<()> {
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&self.file))
            .map_err(|error| io(error.into()))
    }

    /// Caller owns the cache guard and has rechecked entry admission. Never
    /// overwrite another inode or trust a target changed since verification.
    pub(super) fn publish(
        &self,
        target: &Path,
        existing: Option<&Fingerprint>,
        durable: bool,
    ) -> Result<bool> {
        let parent = target.parent().ok_or(Error::Internal("blob target path has no parent"))?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(io)?;
        match fs::metadata(target) {
            Ok(metadata) if existing == Some(&Fingerprint::read(&metadata)) => return Ok(true),
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        match fs::hard_link(self.directory.0.join("content"), target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(io(error)),
        }
        if durable {
            initialization::sync_directory(parent)?;
        }
        Ok(true)
    }
}

/// Unlinked encrypted transfer scratch. Admission and the bounded envelope
/// belong to the caller's account transfer credit, retained through all I/O.
pub(crate) fn blob_transfer_file(scratch: &Path) -> Result<fs::File> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(scratch).map_err(io)?;
    let path = scratch.join(format!("blob-{}", Id::generate()?));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(io)?;
    fs::remove_file(path).map_err(io)?;
    Ok(file)
}

impl LocalStore {
    /// Retain body identity only for the same immutable birth. Callers must
    /// check current scope and Read before consulting this bounded set.
    pub(super) fn captured_blob_version(
        &self,
        version: &BlobVersion,
        creation: [u8; 32],
    ) -> Result<bool> {
        let cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        Ok(cache
            .captures
            .iter()
            .any(|(captured, birth, _)| captured == version && *birth == creation))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::store::EntryId;
    use std::io::Write;

    #[test]
    fn denied_read_admission_reclaims_eviction_deferred_by_its_temporary_pin() {
        // Interleave a budget change and revocation after pin reservation but
        // before final authorization. No ReadView exists to perform cleanup.
        use crate::store::volume_index::EntryAddress;
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Admission race", false).unwrap();
        let entry = store.create_blob(&access, volume.id, "file", b"contents").unwrap();
        let version = store
            .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
            .unwrap();
        let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
        store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
        store.acknowledge_blob_version(&access, &version).unwrap();
        READ_LEASE_RESERVED.set(Some(|store, access, version| {
            assert_eq!(store.set_cache_budget(0).unwrap(), version.len_bytes);
            store
                .set_entry_permissions(
                    access,
                    version.address.volume,
                    version.address.entry,
                    EntryAcl::Replace(Acl { grants: vec![] }),
                )
                .unwrap();
        }));
        assert!(matches!(store.lease_blob_version(&access, &version), Err(Error::AccessDenied(_))));
        assert!(!store.blob_path(&version.digest, true).exists());
        let cache = store.blob_cache.lock().unwrap();
        assert_eq!(cache.readers, 0);
        assert_eq!(cache.bytes, 0);
        assert!(cache.pins.is_empty());
        assert!(cache.captures.is_empty());
    }

    #[test]
    fn absent_views_reserve_storage_and_shared_digests_release_only_after_last_reader() {
        // Simulate replicated metadata before body delivery. Admission must
        // charge future hydration without allocating content-sized buffers.
        use crate::store::volume_index::{self, EntryAddress, EntryInfo, EntryKind, IndexSnapshot};
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Metadata-only views", false).unwrap();
        let database = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
        let mut versions = Vec::new();
        for n in 0..5 {
            let id = EntryId::generate().unwrap();
            let info = EntryInfo {
                namespace: Default::default(),
                origin: crate::store::volume_index::namespace::Origin {
                    installation: crate::store::Id::from_bytes([42; 16]),
                    label: None,
                },
                initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                created_revision: EntryId::generate().unwrap(),
                name: format!("file-{n}"),
                kind: EntryKind::Blob {
                    len_bytes: MAX_BLOB_PLAINTEXT_BYTES,
                    digest: [n; 32],
                    encrypted_key: [n; 32],
                    published_at_micros: 0,
                },
                protocol: None,
            };
            let mut tx = database.begin_write(now_unix_secs()).unwrap();
            volume_index::create(&mut tx, id, &info).unwrap();
            tx.commit().unwrap();
            versions.push(
                IndexSnapshot::new(database.begin_read().unwrap())
                    .blob_version(EntryAddress { volume: volume.id, entry: id }, None)
                    .unwrap(),
            );
        }
        let mut leases = Vec::new();
        for version in &versions[..4] {
            leases.push(store.lease_blob_version(&access, version).unwrap());
        }
        let shared = store.lease_blob_version(&access, &versions[3]).unwrap();
        assert!(matches!(
            store.lease_blob_version(&access, &versions[4]),
            Err(Error::LimitExceeded)
        ));
        leases.pop();
        assert!(matches!(
            store.lease_blob_version(&access, &versions[4]),
            Err(Error::LimitExceeded)
        ));
        drop(shared);
        assert!(store.lease_blob_version(&access, &versions[4]).is_ok());
    }

    #[test]
    fn verified_ranges_are_bounded_survive_unlink_and_detect_later_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blob");
        let content: Vec<u8> = (0..BLOB_CHUNK_BYTES * 2 + 1).map(|n| n as u8).collect();
        fs::write(&path, &content).unwrap();
        let verified = verify_file(
            fs::File::open(&path).unwrap(),
            blake3::hash(&content).as_bytes(),
            content.len() as u64,
        )
        .unwrap();
        assert_eq!(verified.blocks.len(), 3);
        assert_eq!(verified.read_range(65534, 8).unwrap(), content[65534..65542]);
        assert!(verified.read_range(content.len() as u64, 1).unwrap().is_empty());
        for (offset, limit) in [(0, 0), (0, 65537), (u64::MAX, 1)] {
            assert!(matches!(verified.read_range(offset, limit), Err(Error::InvalidInput(_))));
        }
        let mut writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(std::io::SeekFrom::Start(65536)).unwrap();
        writer.write_all(&[42]).unwrap();
        writer.sync_all().unwrap();
        assert!(matches!(verified.read_range(65535, 2), Err(Error::Corrupt(_))));
        assert_eq!(verified.read_range(0, 8).unwrap(), content[..8]);
        fs::remove_file(path).unwrap();
        assert_eq!(
            verified.read_range(0, 8).unwrap(),
            content[..8],
            "an admitted descriptor pins immutable bytes through eviction"
        );
    }

    #[test]
    fn staging_never_overwrites_changed_or_corrupt_existing_content() {
        let directory = tempfile::tempdir().unwrap();
        let stage = BlobStage::new(directory.path()).unwrap();
        (&stage.file).write_all(b"prepared").unwrap();
        stage.sync().unwrap();
        let target = directory.path().join("retained");
        assert!(stage.publish(&target, None, true).unwrap());
        assert!(!stage.publish(&target, None, true).unwrap());
        let fingerprint = Fingerprint::read(&fs::metadata(&target).unwrap());
        assert!(stage.publish(&target, Some(&fingerprint), true).unwrap());
        let other = directory.path().join("other");
        fs::write(&other, b"different").unwrap();
        fs::rename(other, &target).unwrap();
        assert!(!stage.publish(&target, Some(&fingerprint), true).unwrap());
        assert_eq!(fs::read(target).unwrap(), b"different");
    }

    #[test]
    fn verification_refuses_wrong_length_digest_and_content_over_transfer_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blob");
        fs::write(&path, b"bytes").unwrap();
        for (length, digest) in [(4, *blake3::hash(b"bytes").as_bytes()), (5, [0; 32])] {
            assert!(matches!(
                verify_file(fs::File::open(&path).unwrap(), &digest, length),
                Err(Error::Corrupt(_))
            ));
        }
        assert!(matches!(
            verify_file(fs::File::open(path).unwrap(), &[0; 32], MAX_BLOB_PLAINTEXT_BYTES + 1),
            Err(Error::LimitExceeded)
        ));
    }
}
