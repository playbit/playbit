//! Independent immutable content preparation and named blob publication.
//! Entry authority is the volume index. Version facts remain retained; pending
//! plaintext stays durable, and acknowledged plaintext can enter the cache.
use super::*;
use crate::store::{
    EntryId, MAX_REPLICA_DELTA_BYTES,
    volume_index::{
        self, BlobVersion, EntryAddress, EntryInfo, EntryKind, IndexSnapshot, VolumeEntry,
    },
};
use std::io::{Read, Seek, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Match AP's development envelope limit, including authentication overhead.
pub const MAX_ENTRY_BLOB_BYTES: usize = crate::store::MAX_BLOB_PLAINTEXT_BYTES as usize;
const MAX_PREPARATIONS: usize = 2;
const UPLOAD_PAGE: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlobReceipt {
    Hosted,
    PendingUpload,
    #[cfg(feature = "connected-engine")]
    PrivatePeer,
    #[cfg(feature = "connected-engine")]
    PrivateUpload,
}

impl BlobReceipt {
    fn uploaded_by_peer(self) -> bool {
        #[cfg(feature = "connected-engine")]
        {
            self == Self::PrivateUpload
        }
        #[cfg(not(feature = "connected-engine"))]
        {
            false
        }
    }

    fn permission(self) -> Permission {
        if self.uploaded_by_peer() { Permission::Write } else { Permission::Read }
    }
}

pub struct BlobUploadPage {
    pub versions: Vec<BlobVersion>,
    pub next: Option<(EntryId, EntryId)>,
}

#[derive(Default)]
pub(super) struct BlobPreparations {
    pub(super) active: AtomicUsize,
    #[cfg(feature = "connected-engine")]
    released: tokio::sync::Notify,
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::type_complexity, reason = "The hook stops exactly at blob index publication.")]
    pub(super) before_publish: Mutex<Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>>,
    #[cfg(test)]
    #[allow(clippy::type_complexity, reason = "Pause recovery before its final authority check.")]
    pub(super) recovery_before_publish: Mutex<Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>>,
}

fn next_preparation(active: usize) -> Option<usize> {
    if active < MAX_PREPARATIONS { Some(active + 1) } else { None }
}

#[cfg(feature = "connected-engine")]
impl BlobPreparations {
    async fn available(&self) {
        loop {
            let released = self.released.notified();
            if self.active.load(Ordering::Acquire) < MAX_PREPARATIONS {
                return;
            }
            released.await;
        }
    }
}

#[cfg(feature = "connected-engine")]
impl LocalStore {
    /// A live stream can arm this even after the original bulk request ended.
    /// The next transfer still needs to acquire its own preparation credit.
    pub(crate) async fn file_preparation_available(&self) {
        self.entry_blobs.available().await;
    }
}

pub(super) struct Preparation<'a>(&'a BlobPreparations);
impl<'a> Preparation<'a> {
    pub(super) fn enter(slots: &'a BlobPreparations) -> Result<Self> {
        slots
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, next_preparation)
            .map_err(|_| Error::LocalContention("Both blob preparation slots are in use"))?;
        Ok(Self(slots))
    }

    #[cfg(feature = "connected-engine")]
    async fn for_transfer(slots: &'a BlobPreparations) -> Self {
        loop {
            if let Ok(slot) = Self::enter(slots) {
                return slot;
            }
            slots.available().await;
        }
    }
}
impl Drop for Preparation<'_> {
    fn drop(&mut self) {
        let previous = self.0.active.fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous > 0 && previous <= MAX_PREPARATIONS,
            "preparation releases its own credit"
        );
        #[cfg(feature = "connected-engine")]
        self.0.released.notify_waiters();
    }
}

fn pin_prefix(digest: &[u8; 32]) -> String {
    format!("entry-content/{}/", hex(digest))
}

fn has_other_pin(
    tx: &kvdb::WriteTxn<'_, RedbStorage>,
    prefix: &str,
    pin_key: &str,
) -> Result<bool> {
    for row in tx.iter_from(prefix.as_bytes()).take(2) {
        let (key, _) = row?;
        if key.starts_with(prefix.as_bytes()) && key.as_ref() != pin_key.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn pending_count_key(digest: &[u8; 32]) -> String {
    format!("entry-pending-content/{}", hex(digest))
}

fn add_content_pin(tx: &mut kvdb::WriteTxn<'_, RedbStorage>, digest: &[u8; 32]) -> Result<()> {
    let count = pending_count_key(digest);
    let active =
        tx.get(count.as_bytes())?.map(|bytes| decode::<u64>(&bytes)).transpose()?.unwrap_or(0);
    let next = active.checked_add(1).ok_or(Error::LimitExceeded)?;
    tx.put(count.as_bytes(), &encode(&next)?)?;
    Ok(())
}

/// Private peer copies contribute one durable retention reference per digest.
/// Older cache eviction code honors this same counter. The marker makes lost
/// receipts and other private versions of the same bytes idempotent.
#[cfg(feature = "connected-engine")]
fn pin_private_content(tx: &mut kvdb::WriteTxn<'_, RedbStorage>, digest: &[u8; 32]) -> Result<()> {
    let marker = format!("entry-private-content/{}", hex(digest));
    if !tx.has(marker.as_bytes())? {
        add_content_pin(tx, digest)?;
        tx.put(marker.as_bytes(), &[])?;
    }
    Ok(())
}

fn queue_upload(tx: &mut kvdb::WriteTxn<'_, RedbStorage>, version: &BlobVersion) -> Result<()> {
    let upload = upload_key(version);
    if tx.get(upload.as_bytes())?.is_none() {
        add_content_pin(tx, &version.digest)?;
        tx.put(upload.as_bytes(), &encode(version)?)?;
    }
    Ok(())
}

fn retire_upload(tx: &mut kvdb::WriteTxn<'_, RedbStorage>, version: &BlobVersion) -> Result<()> {
    let count = pending_count_key(&version.digest);
    let active = tx.get(count.as_bytes())?.map(|bytes| decode::<u64>(&bytes)).transpose()?;
    if tx.get(upload_key(version).as_bytes())?.is_some() {
        let next = active
            .and_then(|active| active.checked_sub(1))
            .ok_or(Error::corrupt("pending blob upload count is missing or zero"))?;
        tx.put(count.as_bytes(), &encode(&next)?)?;
        tx.del(upload_key(version).as_bytes())?;
    } else if active.is_none() {
        // This verified receipt is the first local observation of recoverability.
        tx.put(count.as_bytes(), &encode(&0u64)?)?;
    }
    Ok(())
}

pub(super) fn publication_time() -> Result<i64> {
    timestamp_micros(SystemTime::now())
}
fn timestamp_micros(time: SystemTime) -> Result<i64> {
    let nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(value) => i128::try_from(value.as_nanos()).map_err(|_| Error::LimitExceeded)?,
        Err(error) => {
            -i128::try_from(error.duration().as_nanos()).map_err(|_| Error::LimitExceeded)?
        }
    };
    i64::try_from(nanos.div_euclid(1000)).map_err(|_| Error::LimitExceeded)
}

pub(super) fn stamp_publication(kind: &mut EntryKind) -> Result<()> {
    if let EntryKind::Blob { published_at_micros, .. } = kind {
        *published_at_micros = publication_time()?;
    }
    Ok(())
}

impl LocalStore {
    /// An app's owned Local scopes never publish their files. Reclaim historical
    /// and orphaned pins too. File bodies are shared by digest across volumes.
    /// Caller holds the volume gate and has fenced new opens.
    pub(super) fn remove_local_blob_pins(&self, volume: Id) -> Result<()> {
        let snapshot = self.metadata.begin_read()?;
        let durable = format!("entry-durable/{volume}/");
        // Ordinary Local pins are all pending and therefore indexed by volume.
        // Explicit local durability receipts also have historical content pins.
        let has_receipt = snapshot
            .iter_from(durable.as_bytes())
            .next()
            .transpose()?
            .is_some_and(|(key, _)| key.starts_with(durable.as_bytes()));
        let prefix = if has_receipt {
            "entry-content/".to_owned()
        } else {
            format!("entry-upload/{volume}/")
        };
        for row in snapshot.iter_from(prefix.as_bytes()) {
            let (key, bytes) = row?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let version: BlobVersion = decode(&bytes)?;
            if version.address.volume != volume {
                if has_receipt {
                    continue;
                }
                return Err(Error::corrupt("local blob upload volume differs"));
            }
            let digest_prefix = pin_prefix(&version.digest);
            let pin_key =
                format!("{}{volume}/{}/{}", digest_prefix, version.address.entry, version.revision);
            let source_key = if has_receipt { pin_key.clone() } else { upload_key(&version) };
            if key.as_ref() != source_key.as_bytes() {
                return Err(Error::corrupt("blob pin identity differs"));
            }
            self.remove_local_blob_pin(&version, &digest_prefix, &pin_key)?;
        }
        Ok(())
    }

    fn remove_local_blob_pin(
        &self,
        version: &BlobVersion,
        prefix: &str,
        pin_key: &str,
    ) -> Result<()> {
        {
            let _profile = self.lock_profile()?;
            let mut tx = self.metadata.begin_write(now_unix_secs())?;
            let pin =
                tx.get(pin_key.as_bytes())?.ok_or(Error::corrupt("local blob pin is missing"))?;
            if decode::<BlobVersion>(&pin)? != *version {
                return Err(Error::corrupt("local blob pin differs from retained version"));
            }
            drop(pin);
            if has_other_pin(&tx, prefix, pin_key)? {
                retire_upload(&mut tx, version)?;
                tx.del(pin_key.as_bytes())?;
                tx.commit()?;
                return Ok(());
            }
            let pending = tx
                .get(pending_count_key(&version.digest).as_bytes())?
                .map(|bytes| decode::<u64>(&bytes))
                .transpose()?
                .unwrap_or(0);
            if pending != u64::from(tx.has(upload_key(version).as_bytes())?) {
                return Err(Error::corrupt("last local blob pin disagrees with pending uploads"));
            }
            drop(tx);
            self.unlink_local_blob(&version.digest)?;
        }
        // Keep the last pin until both directory changes survive a crash. Let
        // other profile writers publish new pins while these barriers complete.
        for recoverable in [false, true] {
            let path = self.blob_path(&version.digest, recoverable);
            let parent = path.parent().expect("blob paths always have a parent");
            if parent.try_exists().map_err(io)? {
                super::initialization::sync_directory(parent)?;
            }
        }
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        retire_upload(&mut tx, version)?;
        tx.del(pin_key.as_bytes())?;
        if !has_other_pin(&tx, prefix, pin_key)? {
            tx.del(pending_count_key(&version.digest).as_bytes())?;
        }
        tx.commit()?;
        Ok(())
    }

    fn unlink_local_blob(&self, digest: &[u8; 32]) -> Result<()> {
        let mut cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        if cache.defer_pinned(&hex(digest), false) {
            return Err(Error::LocalContention("local app file is still open"));
        }
        for recoverable in [false, true] {
            match fs::remove_file(self.blob_path(digest, recoverable)) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(io(e)),
            }
        }
        #[cfg(any(
            feature = "connected-engine",
            feature = "native-runtime",
            feature = "wasm-runtime"
        ))]
        {
            let previous = self
                .blob_eviction_generation
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
                .map_err(|_| Error::LimitExceeded)?;
            cache.expire_verified(previous + 1);
        }
        Ok(())
    }

    pub(super) fn has_entry_blob_pin_text(&self, digest: &str) -> Result<bool> {
        let key = format!("entry-pending-content/{digest}");
        // Unknown files are conservatively retained after interrupted preparation.
        Ok(get::<u64>(&self.metadata, key.as_bytes())? != Some(0))
    }

    /// Create a named blob independently of any structured-object reference.
    /// Publication samples the originating wall clock inside the index transaction.
    pub fn create_blob(
        &self,
        access: &Access,
        volume: Id,
        name: &str,
        bytes: &[u8],
    ) -> Result<VolumeEntry> {
        if bytes.len() > MAX_ENTRY_BLOB_BYTES {
            return Err(Error::LimitExceeded);
        }
        self.create_blob_from_reader(access, volume, name, &mut &bytes[..])
    }

    /// Import a bounded input into private immutable staging before publication.
    pub fn create_blob_from_reader(
        &self,
        access: &Access,
        volume: Id,
        name: &str,
        source: &mut impl Read,
    ) -> Result<VolumeEntry> {
        self.create_blob_from_reader_inner(access, volume, None, name, source)
    }

    /// A directory move or rename during staging follows its original identity.
    pub fn create_blob_from_reader_in(
        &self,
        access: &Access,
        volume: Id,
        parent: EntryId,
        name: &str,
        source: &mut impl Read,
    ) -> Result<VolumeEntry> {
        self.create_blob_from_reader_inner(access, volume, Some(parent), name, source)
    }

    fn create_blob_from_reader_inner(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
        name: &str,
        source: &mut impl Read,
    ) -> Result<VolumeEntry> {
        if parent.is_some() {
            volume_index::namespace::component(name)?;
        } else {
            volume_index::validate_name(name)?;
        }
        {
            let operations = self.access_operations(access, volume)?;
            let _guards = operations.lock()?;
            self.authorize_blob_mutation(access, volume, None)?;
            if parent.is_some() {
                self.file_directory_locked(access, volume, parent)?;
            }
            let database = self.db(volume, crate::store::DatabaseId::Index)?;
            match IndexSnapshot::new(database.begin_read()?).lookup_from(parent, name) {
                Ok(_) => return Err(Error::AlreadyExists("entry name")),
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let _preparation = Preparation::enter(&self.entry_blobs)?;
        let id = EntryId::generate()?;
        let revision = EntryId::generate()?;
        let kind = self.prepare_entry_blob(volume, id, revision, source)?;
        self.before_entry_blob_publication()?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.authorize_blob_mutation(access, volume, None)?;
        if parent.is_some() {
            self.file_directory_locked(access, volume, parent)?;
        }
        let database = self.db(volume, crate::store::DatabaseId::Index)?;
        let mut info = EntryInfo {
            namespace: volume_index::namespace::Birth { parent, ..Default::default() },
            origin: crate::store::volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
            created_revision: revision,
            name: name.rsplit('/').next().expect("validated path").to_owned(),
            kind,
            protocol: None,
        };
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        stamp_publication(&mut info.kind)?;
        volume_index::create_at_path(&mut tx, id, name, &info)?;
        volume_index::mark_backing_ready(&mut tx, id)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        IndexSnapshot::new(database.begin_read()?).live_entry(id)
    }

    /// Replace content without republishing the name or discarding older versions.
    pub fn write_blob(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        bytes: &[u8],
    ) -> Result<VolumeEntry> {
        if bytes.len() > MAX_ENTRY_BLOB_BYTES {
            return Err(Error::LimitExceeded);
        }
        self.write_blob_from_reader(access, volume, entry, &mut &bytes[..])
    }

    /// Return the qualified current metadata. A successful publication remains
    /// retained even when its revision is pending or loses content resolution.
    pub fn write_blob_from_reader(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        source: &mut impl Read,
    ) -> Result<VolumeEntry> {
        let _timing = crate::trace::Span::new("storage.blob_write");
        self.admit_blob_mutation(access, volume, Some(entry))?;
        let _preparation = Preparation::enter(&self.entry_blobs)?;
        let revision = EntryId::generate()?;
        let mut kind = self.prepare_entry_blob(volume, entry, revision, source)?;
        self.before_entry_blob_publication()?;
        let _publication = crate::trace::Span::new("storage.blob_index_publication");
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.authorize_blob_mutation(access, volume, Some(entry))?;
        let database = self.db(volume, crate::store::DatabaseId::Index)?;
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        stamp_publication(&mut kind)?;
        volume_index::update_content(
            &mut tx,
            entry,
            kind.clone(),
            revision,
            &crate::store::volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
        )?;
        volume_index::mark_published_blob_ready(&mut tx, entry, &kind)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        IndexSnapshot::new(database.begin_read()?).live_entry(entry)
    }

    pub(crate) fn admit_blob_mutation(
        &self,
        access: &Access,
        volume: Id,
        entry: Option<EntryId>,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.blob_mutation_admission");
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.authorize_blob_mutation(access, volume, entry)
    }

    /// The caller holds the current volume/session/consent boundary.
    pub(super) fn authorize_blob_mutation(
        &self,
        access: &Access,
        volume: Id,
        entry: Option<EntryId>,
    ) -> Result<()> {
        let policy = self.index(volume)?;
        self.check_authority(access, volume, entry)?;
        if let Some(grant) = self.admit_access(access, &policy)?
            && !grant.write
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        match entry {
            Some(entry) => {
                let database = self.db(volume, crate::store::DatabaseId::Index)?;
                let snapshot = IndexSnapshot::new(database.begin_read()?);
                snapshot.authorize(
                    policy.volume(),
                    policy.default_permissions(),
                    self.data_actor(access, policy.volume())?,
                    entry,
                    Permission::Write,
                )?;
                if !matches!(snapshot.live_entry(entry)?.info.kind, EntryKind::Blob { .. }) {
                    return Err(Error::InvalidInput("entry is not an independent blob"));
                }
            }
            None => self.authorize_entry_creation(access, &policy)?,
        }
        Ok(())
    }

    /// Expensive hashing/sealing/file fsync runs outside the volume operation gate.
    /// Pin before index publication. A rejected publication retains an orphan
    /// conservatively; it never exposes a named entry or authorizes content access.
    pub(super) fn prepare_entry_blob(
        &self,
        volume: Id,
        entry: EntryId,
        revision: EntryId,
        source: &mut impl Read,
    ) -> Result<EntryKind> {
        self.prepare_file_blob(volume, entry, revision, source, true)
    }

    pub(super) fn prepare_file_blob(
        &self,
        volume: Id,
        entry: EntryId,
        revision: EntryId,
        source: &mut impl Read,
        durable: bool,
    ) -> Result<EntryKind> {
        let key = self.credentials(volume)?.blob_key;
        self.prepare_entry_blob_with_key(
            EntryAddress { volume, entry },
            revision,
            source,
            &key,
            durable,
            || {
                let profile = self.lock_profile()?;
                if self.credentials(volume)?.blob_key != key {
                    return Err(Error::AccessDenied(
                        "volume credentials changed during file preparation",
                    ));
                }
                Ok(profile)
            },
        )
    }

    /// The caller admits publication under the profile writer after staging and
    /// content verification. Initialization already owns that writer.
    pub(super) fn prepare_entry_blob_with_key<G>(
        &self,
        address: EntryAddress,
        revision: EntryId,
        source: &mut impl Read,
        blob_key: &[u8; 32],
        durable: bool,
        admit: impl Fn() -> Result<G>,
    ) -> Result<EntryKind> {
        let EntryAddress { volume, entry } = address;
        let (mut stage, digest, length) = self.stage_entry_input(source, durable)?;
        let encrypted_key = crate::store::seal_blob_to(
            blob_key,
            volume,
            &digest,
            length,
            &mut stage.file,
            &mut std::io::sink(),
        )?;
        let address = EntryAddress { volume, entry };
        let pin = BlobVersion { address, revision, digest, len_bytes: length, encrypted_key };
        self.publish_entry_content(&stage, &pin, durable, admit)?;
        Ok(EntryKind::Blob { len_bytes: length, digest, encrypted_key, published_at_micros: 0 })
    }

    /// Caller owns the profile guard. Copy, hashing and sealing have completed;
    /// only durable pin and upload bookkeeping share this short transaction.
    fn pin_prepared_blob_profile_locked(&self, pin: &BlobVersion) -> Result<()> {
        let BlobVersion { address: EntryAddress { volume, entry }, revision, digest, .. } = pin;
        let key = format!("{}{volume}/{entry}/{revision}", pin_prefix(digest));
        let encoded = encode(pin)?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(key.as_bytes(), &encoded)?;
        queue_upload(&mut tx, pin)?;
        tx.commit()?;
        Ok(())
    }

    fn stage_entry_input(
        &self,
        source: &mut impl Read,
        durable: bool,
    ) -> Result<(blob_files::BlobStage, [u8; 32], u64)> {
        let mut stage = blob_files::BlobStage::new(&self.blob_stage_directory(false)?)?;
        let mut buffer = [0; crate::store::BLOB_CHUNK_BYTES];
        let mut hash = blake3::Hasher::new();
        let mut length = 0u64;
        loop {
            let count = match source.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io(error)),
            };
            if count == 0 {
                break;
            }
            length = length.checked_add(count as u64).ok_or(Error::LimitExceeded)?;
            if length > MAX_ENTRY_BLOB_BYTES as u64 {
                return Err(Error::LimitExceeded);
            }
            hash.update(&buffer[..count]);
            stage.file.write_all(&buffer[..count]).map_err(io)?;
        }
        if durable {
            stage.sync()?;
        }
        stage.file.rewind().map_err(io)?;
        Ok((stage, *hash.finalize().as_bytes(), length))
    }

    fn publish_entry_content<G>(
        &self,
        stage: &blob_files::BlobStage,
        version: &BlobVersion,
        durable: bool,
        admit: impl Fn() -> Result<G>,
    ) -> Result<()> {
        let destination = self.blob_path(&version.digest, false);
        for _ in 0..2 {
            let existing = self.inspect_blob(version, &destination)?;
            if matches!(existing, blob_files::ExistingBlob::Corrupt { .. }) {
                return Err(Error::corrupt("retained blob failed verification"));
            }
            if durable && !existing.sync_verified(&destination)? {
                continue;
            }
            // File publication and its pin must be indivisible to a last-pin eraser.
            let _profile = admit()?;
            let _cache = self
                .blob_cache
                .lock()
                .map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
            // The directory barriers below cover a new link and an existing
            // link left by an ordinary close.
            if existing.publish(stage, &destination, false)? {
                if durable {
                    super::initialization::sync_directory(
                        destination.parent().expect("blob parent"),
                    )?;
                    super::initialization::sync_directory(&self.path)?;
                }
                return self.pin_prepared_blob_profile_locked(version);
            }
        }
        Err(Error::LocalContention("blob content changed during import"))
    }

    /// Read one captured immutable version; verify bytes outside the volume
    /// gate, then recheck current authorization before returning.
    pub fn read_blob_entry(&self, access: &Access, volume: Id, entry: EntryId) -> Result<Vec<u8>> {
        let version = self.blob_version(access, EntryAddress { volume, entry }, None)?;
        self.read_blob_version(access, &version)
    }

    pub fn blob_version(
        &self,
        access: &Access,
        address: EntryAddress,
        revision: Option<EntryId>,
    ) -> Result<BlobVersion> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.blob_version_locked(access, address, revision, Permission::Read, None)
    }

    fn blob_version_locked(
        &self,
        access: &Access,
        address: EntryAddress,
        revision: Option<EntryId>,
        permission: Permission,
        captured: Option<&BlobVersion>,
    ) -> Result<BlobVersion> {
        let policy = self.index(address.volume)?;
        self.check_authority(access, address.volume, Some(address.entry))?;
        let grant = self.admit_access(access, &policy)?;
        if permission == Permission::Write && grant.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let database = self.db(address.volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(database.begin_read()?);
        if permission == Permission::Write {
            // This private version path is used for sealing/acknowledging an
            // upload, never for editing a blob. Creation bodies admit Append.
            snapshot.authorize_blob_upload(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                address.entry,
                revision.ok_or(Error::InvalidInput("upload revision required"))?,
            )?;
            if access.invitation_peer.is_some() {
                self.authorize_private_file_owner(&policy, &snapshot, address)?;
            }
        } else if permission == Permission::Read && revision.is_some() {
            snapshot.authorize_retained_read(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                address.entry,
            )?;
        } else {
            snapshot.authorize(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                address.entry,
                permission,
            )?;
        }
        let version = match captured {
            Some(version) => {
                if permission != Permission::Read
                    || !self
                        .captured_blob_version(version, snapshot.blob_creation(address.entry)?)?
                {
                    snapshot.validate_blob_version(version)?;
                }
                version.clone()
            }
            None => snapshot.blob_version(address, revision)?,
        };
        if version.len_bytes > MAX_ENTRY_BLOB_BYTES as u64 {
            return Err(Error::LimitExceeded);
        }
        Ok(version)
    }

    /// The owner can decrypt the incoming envelope. Entry overrides can remove
    /// their content access while still allowing them to manage the policy.
    fn authorize_private_file_owner(
        &self,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        address: EntryAddress,
    ) -> Result<()> {
        let owner = policy.volume().owner;
        self.check_authority(&Access::owner(owner), address.volume, Some(address.entry))?;
        snapshot
            .authorize(
                policy.volume(),
                policy.default_permissions(),
                owner,
                address.entry,
                Permission::Read,
            )
            .map_err(|error| match error {
                Error::AccessDenied(_) => Error::AccessDenied(Error::PRIVATE_FILE_OWNER_READ),
                other => other,
            })
    }

    pub(super) fn admitted_blob_creation(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<[u8; 32]> {
        let operations = self.access_operations(access, version.address.volume)?;
        let _guards = operations.lock()?;
        self.check_blob_version_locked(access, version, Permission::Read)?;
        let database = self.db(version.address.volume, crate::store::DatabaseId::Index)?;
        IndexSnapshot::new(database.begin_read()?).blob_creation(version.address.entry)
    }

    /// Recheck the original entry's current Read authority for an exact body.
    /// An active lease preserves its admitted identity across source replacement.
    pub fn validate_blob_version(&self, access: &Access, version: &BlobVersion) -> Result<()> {
        self.check_blob_version(access, version, Permission::Read)
    }

    pub(super) fn check_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
        permission: Permission,
    ) -> Result<()> {
        let operations = self.access_operations(access, version.address.volume)?;
        let _guards = operations.lock()?;
        self.check_blob_version_locked(access, version, permission)
    }

    fn check_blob_version_locked(
        &self,
        access: &Access,
        version: &BlobVersion,
        permission: Permission,
    ) -> Result<()> {
        self.blob_version_locked(
            access,
            version.address,
            Some(version.revision),
            permission,
            Some(version),
        )
        .map(|_| ())
    }

    pub fn read_blob_version(&self, access: &Access, version: &BlobVersion) -> Result<Vec<u8>> {
        use std::io::Read;
        self.check_blob_version(access, version, Permission::Read)?;
        let BlobVersion { len_bytes, digest, .. } = *version;
        let _preparation = Preparation::enter(&self.entry_blobs)?;
        let file = self.open_entry_blob(&digest).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound("blob content")
            } else {
                io(error)
            }
        })?;
        if file.metadata().map_err(io)?.len() != len_bytes {
            return Err(Error::corrupt("blob file length differs from its declaration"));
        }
        let mut bytes = Vec::with_capacity(len_bytes as usize);
        file.take(len_bytes + 1).read_to_end(&mut bytes).map_err(io)?;
        if bytes.len() as u64 != len_bytes || *blake3::hash(&bytes).as_bytes() != digest {
            return Err(Error::corrupt("blob file length or digest differs from its declaration"));
        }
        self.check_blob_version(access, version, Permission::Read)?;
        Ok(bytes)
    }

    /// Sealing and hashing run outside the authorization boundary. Recheck the
    /// exact retained version and current write rights before returning bytes.
    pub fn encrypt_blob_version(&self, access: &Access, version: &BlobVersion) -> Result<fs::File> {
        self.check_blob_version(access, version, Permission::Write)?;
        let slot = Preparation::enter(&self.entry_blobs)?;
        self.seal_blob_version(access, version, Permission::Write, slot)
    }

    /// The private transfer worker has its own deadline and cancellation.
    /// Its one slot covers verification and sealing without a second race.
    #[cfg(feature = "connected-engine")]
    pub(crate) async fn encrypt_peer_blob_upload(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<fs::File> {
        self.validate_peer_blob_upload(access, version)?;
        let slot = Preparation::for_transfer(&self.entry_blobs).await;
        self.check_blob_version(access, version, Permission::Write)?;
        self.seal_blob_version(access, version, Permission::Write, slot)
    }

    /// Serve only an authenticated reader's exact retained file revision.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn encrypt_peer_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<fs::File> {
        if access.invitation_peer.is_none() {
            return Err(Error::AccessDenied("private file reader required"));
        }
        self.check_blob_version(access, version, Permission::Read)?;
        let slot = Preparation::enter(&self.entry_blobs)?;
        self.seal_blob_version(access, version, Permission::Read, slot)
    }

    fn seal_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
        permission: Permission,
        slot: Preparation<'_>,
    ) -> Result<fs::File> {
        use std::io::Seek;
        let mut plaintext = self.verify_blob_for(access, version, permission, &slot)?;
        let mut ciphertext =
            super::blob_files::blob_transfer_file(&self.path.join(crate::paths::STAGING))?;
        let encrypted_key = crate::store::seal_blob_to(
            &self.credentials(version.address.volume)?.blob_key,
            version.address.volume,
            &version.digest,
            version.len_bytes,
            &mut plaintext.file,
            &mut ciphertext,
        )?;
        if encrypted_key != version.encrypted_key {
            return Err(Error::corrupt("encrypted blob digest differs from its declaration"));
        }
        rustix::io::retry_on_intr(|| rustix::fs::fsync(&ciphertext))
            .map_err(|error| io(error.into()))?;
        ciphertext.rewind().map_err(io)?;
        self.check_blob_version(access, version, permission)?;
        Ok(ciphertext)
    }

    /// An authenticated body can arrive for an older retained version. Current
    /// ACLs still apply; its receipt never changes which version is current.
    pub fn receive_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
        ciphertext: &mut impl std::io::Read,
        durable: bool,
    ) -> Result<()> {
        let receipt = if durable { BlobReceipt::Hosted } else { BlobReceipt::PendingUpload };
        self.receive_blob(access, version, ciphertext, receipt).map(|_| ())
    }

    /// A file received from its private owner remains pinned for offline use.
    /// Its recipient has no obligation to publish it to the hosted outbox.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn receive_peer_blob_version(
        &self,
        access: &Access,
        version: &BlobVersion,
        ciphertext: &mut impl std::io::Read,
    ) -> Result<()> {
        let mut access = access.clone();
        access.invitation_peer = Some(self.volume(version.address.volume)?.owner);
        self.receive_blob(&access, version, ciphertext, BlobReceipt::PrivatePeer).map(|_| ())
    }

    /// Called for a writer-to-owner Push on either endpoint. File publication
    /// admits Append only for the creation body, and requires Write for edits.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn validate_peer_blob_upload(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<()> {
        self.check_peer_blob_writer(access, version)?;
        self.check_blob_version(access, version, Permission::Write)
    }

    #[cfg(feature = "connected-engine")]
    fn check_peer_blob_writer(&self, access: &Access, version: &BlobVersion) -> Result<()> {
        let owner = self.volume(version.address.volume)?.owner;
        let peer = if self.account.id == owner { access.account } else { owner };
        if access.session_id.is_some()
            || access.account == owner
            || access.invitation_peer != Some(peer)
        {
            return Err(Error::AccessDenied("private file writer required"));
        }
        Ok(())
    }

    /// Retain the body of a previously admitted version on the private owner.
    /// A true result wakes readers after the store records its scoped change.
    #[cfg(feature = "connected-engine")]
    pub(crate) async fn receive_peer_blob_upload(
        &self,
        access: &Access,
        version: &BlobVersion,
        ciphertext: &mut impl std::io::Read,
    ) -> Result<bool> {
        if access.account == self.account.id {
            return Err(Error::AccessDenied("private file upload requires its owner as receiver"));
        }
        self.validate_peer_blob_upload(access, version)?;
        let slot = Preparation::for_transfer(&self.entry_blobs).await;
        self.check_blob_version(access, version, Permission::Write)?;
        self.receive_blob_admitted(access, version, ciphertext, BlobReceipt::PrivateUpload, slot)
    }

    fn receive_blob(
        &self,
        access: &Access,
        version: &BlobVersion,
        ciphertext: &mut impl std::io::Read,
        receipt: BlobReceipt,
    ) -> Result<bool> {
        self.check_blob_version(access, version, receipt.permission())?;
        let slot = Preparation::enter(&self.entry_blobs)?;
        self.receive_blob_admitted(access, version, ciphertext, receipt, slot)
    }

    fn receive_blob_admitted(
        &self,
        access: &Access,
        version: &BlobVersion,
        ciphertext: &mut impl std::io::Read,
        receipt: BlobReceipt,
        _slot: Preparation<'_>,
    ) -> Result<bool> {
        let mut plaintext = super::blob_files::BlobStage::new(
            &self.blob_stage_directory(receipt == BlobReceipt::Hosted)?,
        )?;
        crate::store::open_blob_to(
            &self.credentials(version.address.volume)?.blob_key,
            version,
            ciphertext,
            &mut plaintext.file,
        )?;
        plaintext.sync()?;
        self.publish_received_blob(access, version, &plaintext, receipt)
    }

    fn publish_received_blob(
        &self,
        access: &Access,
        version: &BlobVersion,
        plaintext: &super::blob_files::BlobStage,
        receipt: BlobReceipt,
    ) -> Result<bool> {
        let durable = receipt == BlobReceipt::Hosted;
        for _ in 0..2 {
            let mut destinations = vec![self.blob_path(&version.digest, durable)];
            let retained = self.blob_path(&version.digest, false);
            if durable && retained.try_exists().map_err(io)? {
                destinations.push(retained);
            }
            let inspections = destinations
                .iter()
                .map(|destination| self.inspect_blob(version, destination))
                .collect::<Result<Vec<_>>>()?;
            // A private owner acknowledges this local copy as durable. An
            // ordinary File close can leave an identical unsynced inode/link.
            if receipt.uploaded_by_peer()
                && let super::blob_files::ExistingBlob::Verified(_) = &inspections[0]
            {
                let target = &destinations[0];
                if !inspections[0].sync_verified(target)? {
                    continue;
                }
                super::initialization::sync_directory(target.parent().expect("blob parent"))?;
            }
            self.before_entry_blob_publication()?;
            let operations = self.access_operations(access, version.address.volume)?;
            let _guards = operations.lock()?;
            self.check_blob_version_locked(access, version, receipt.permission())?;
            let _profile = self.lock_profile()?;
            let _cache = self
                .blob_cache
                .lock()
                .map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
            let mut published = true;
            for (destination, existing) in destinations.iter().zip(&inspections) {
                published &= existing.publish(plaintext, destination, true)?;
            }
            if !published {
                continue;
            }
            super::initialization::sync_directory(&self.path)?;
            let new_version = self.retain_received_entry_blob(version, receipt)?;
            let became_ready = self.mark_received_file_ready(version)?;
            let restored = !matches!(&inspections[0], super::blob_files::ExistingBlob::Verified(_));
            let available = new_version || became_ready || restored;
            if receipt.uploaded_by_peer() && available {
                self.private_file_available(
                    version.address.volume,
                    Some((version.address.entry, version.revision)),
                );
            }
            return Ok(available);
        }
        Err(Error::LocalContention("blob cache changed during verified publication"))
    }

    /// The caller holds the volume gate through admission and readiness.
    /// A historical body stays available without selecting an older head.
    fn mark_received_file_ready(&self, version: &BlobVersion) -> Result<bool> {
        let database = self.db(version.address.volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(database.begin_read()?);
        match snapshot.blob_version(version.address, None) {
            Ok(current)
                if current == *version && !snapshot.backing_ready(version.address.entry)? => {}
            Ok(_) | Err(Error::NotFound("retired entry")) => return Ok(false),
            Err(error) => return Err(error),
        }
        drop(snapshot);
        let mut tx = database.begin_write(now_unix_secs())?;
        volume_index::mark_backing_ready(&mut tx, version.address.entry)?;
        tx.commit()?;
        Ok(true)
    }

    fn retain_received_entry_blob(
        &self,
        version: &BlobVersion,
        receipt: BlobReceipt,
    ) -> Result<bool> {
        let key = format!(
            "{}{}/{}/{}",
            pin_prefix(&version.digest),
            version.address.volume,
            version.address.entry,
            version.revision
        );
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let new_version = !tx.has(key.as_bytes())?;
        tx.put(key.as_bytes(), &encode(version)?)?;
        #[cfg(feature = "connected-engine")]
        if matches!(receipt, BlobReceipt::PrivatePeer | BlobReceipt::PrivateUpload) {
            pin_private_content(&mut tx, &version.digest)?;
        }
        let changed =
            receipt == BlobReceipt::Hosted && tx.get(durable_key(version).as_bytes())?.is_none();
        if receipt == BlobReceipt::Hosted {
            tx.put(durable_key(version).as_bytes(), &encode(&true)?)?;
            if changed {
                Self::advance_provider_upload(&mut tx, version.address.volume)?;
            }
            retire_upload(&mut tx, version)?;
        } else if receipt == BlobReceipt::PendingUpload
            && tx.get(durable_key(version).as_bytes())?.is_none()
        {
            queue_upload(&mut tx, version)?;
        }
        tx.commit()?;
        #[cfg(any(test, feature = "file-provider"))]
        if changed {
            self.changes.provider_status_changed(version.address.volume);
        }
        Ok(new_version)
    }

    /// Page both live current and retained historical bodies. A metadata ack
    /// never removes this queue. The appropriate hosted or private owner must
    /// acknowledge the exact file transfer.
    pub fn pending_entry_blobs(
        &self,
        access: &Access,
        volume: Id,
        after: Option<(EntryId, EntryId)>,
    ) -> Result<BlobUploadPage> {
        self.pending_blob_uploads(access, volume, None, after, UPLOAD_PAGE)
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn pending_selected_entry_blobs(
        &self,
        access: &Access,
        address: EntryAddress,
        after: Option<(EntryId, EntryId)>,
    ) -> Result<BlobUploadPage> {
        self.pending_blob_uploads(access, address.volume, Some(address.entry), after, UPLOAD_PAGE)
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn next_private_file_upload(
        &self,
        volume: Id,
        after: Option<(EntryId, EntryId)>,
    ) -> Result<BlobUploadPage> {
        self.pending_blob_uploads(&Access::owner(self.account.id), volume, None, after, 1)
    }

    fn pending_blob_uploads(
        &self,
        access: &Access,
        volume: Id,
        selected: Option<EntryId>,
        after: Option<(EntryId, EntryId)>,
        limit: usize,
    ) -> Result<BlobUploadPage> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.admit_access(access, &policy)?;
        let mut access = access.clone();
        if access.session_id.is_none()
            && access.account == self.account.id
            && policy.volume().personal
            && !policy.volume().share_key_with_service
            && policy.volume().owner != self.account.id
            && self.has_direct_volume_grant(volume)?
        {
            access.invitation_peer = Some(policy.volume().owner);
        }
        let prefix = selected.map_or_else(
            || format!("entry-upload/{volume}/"),
            |entry| format!("entry-upload/{volume}/{entry}/"),
        );
        if after.is_some_and(|(entry, _)| selected.is_some_and(|selected| selected != entry)) {
            return Err(Error::InvalidInput("upload cursor outside selected entry"));
        }
        let start =
            after.map(|(entry, revision)| format!("entry-upload/{volume}/{entry}/{revision}"));
        let tx = self.metadata.begin_read()?;
        let mut iter = tx.iter_from(start.as_ref().map_or(prefix.as_bytes(), |key| key.as_bytes()));
        let mut versions = Vec::with_capacity(limit);
        let mut scanned = 0;
        let mut next = None;
        while let Some((key, value)) = iter.next().transpose()? {
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            if scanned == UPLOAD_PAGE || versions.len() == limit {
                return Ok(BlobUploadPage { versions, next });
            }
            let version: BlobVersion = decode(&value)?;
            if &key[..] != upload_key(&version).as_bytes() || version.address.volume != volume {
                return Err(Error::corrupt("blob upload record does not match its key or volume"));
            }
            let cursor = (version.address.entry, version.revision);
            if Some(cursor) == after {
                continue;
            }
            scanned += 1;
            next = Some(cursor);
            match self.check_blob_version_locked(&access, &version, Permission::Write) {
                Ok(()) => versions.push(version),
                // Interrupted publication and permanent retirement retain pins,
                // but never authorize a remote upload or resurrect an entry.
                Err(Error::NotFound(_) | Error::AccessDenied(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(BlobUploadPage { versions, next: if scanned == UPLOAD_PAGE { next } else { None } })
    }

    pub fn blob_version_durable(&self, access: &Access, version: &BlobVersion) -> Result<bool> {
        self.check_blob_version(access, version, Permission::Read)?;
        self.blob_durable_receipt(version)
    }

    /// Caller has already authorized this exact immutable version.
    pub(super) fn blob_durable_receipt(&self, version: &BlobVersion) -> Result<bool> {
        Ok(get::<bool>(&self.metadata, durable_key(version).as_bytes())?.unwrap_or(false))
    }

    /// Upload receipts disclose durability, never existing plaintext. Check
    /// Write and ServiceRead under the same gate as the exact retained version.
    #[cfg(any(test, feature = "connected-engine"))]
    pub(crate) fn blob_upload_durable(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<bool> {
        let operations = self.access_operations(access, version.address.volume)?;
        let _guards = operations.lock()?;
        self.check_blob_upload_locked(access, version)?;
        self.blob_durable_receipt(version)
    }

    fn check_blob_upload_locked(&self, access: &Access, version: &BlobVersion) -> Result<()> {
        self.check_blob_version_locked(access, version, Permission::Write)?;
        let policy = self.index(version.address.volume)?;
        if !policy.volume().share_key_with_service {
            return Err(Error::AccessDenied("waiting for an authorized peer"));
        }
        let index = self.db(version.address.volume, crate::store::DatabaseId::Index)?;
        IndexSnapshot::new(index.begin_read()?).authorize(
            policy.volume(),
            policy.default_permissions(),
            crate::store::PLAYBIT_REPLICA_PRINCIPAL,
            version.address.entry,
            Permission::Read,
        )
    }

    #[cfg(any(test, feature = "connected-engine"))]
    pub(crate) fn acknowledge_blob_upload(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<()> {
        self.acknowledge_blob_version_inner(access, version, true)
    }

    /// The live index publication and explicit synchronization use the same
    /// writer outbox. The upload itself separately authorizes current access.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn peer_blob_upload_pending(&self, version: &BlobVersion) -> Result<bool> {
        let pending: Option<BlobVersion> = get(&self.metadata, upload_key(version).as_bytes())?;
        Ok(pending.as_ref() == Some(version))
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn peer_blob_upload_eligible(&self, version: &BlobVersion) -> Result<bool> {
        if !self.peer_blob_upload_pending(version)? {
            return Ok(false);
        }
        let owner = self.volume(version.address.volume)?.owner;
        let access = Access::private_volume_peer(self.account.id, owner);
        match self.validate_peer_blob_upload(&access, version) {
            Ok(()) => Ok(true),
            // Match the durable outbox scan: concurrent retirement and
            // permission changes leave this reference for future recovery.
            Err(Error::NotFound(_) | Error::AccessDenied(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Retire only this writer's acknowledged version. Keep its local bytes
    /// pinned even if an unrelated hosted entry has the same plaintext.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn acknowledge_peer_blob_upload(
        &self,
        access: &Access,
        version: &BlobVersion,
    ) -> Result<()> {
        if access.account != self.account.id {
            return Err(Error::AccessDenied("private file receipt requires its writer"));
        }
        let operations = self.access_operations(access, version.address.volume)?;
        let _guards = operations.lock()?;
        self.check_peer_blob_writer(access, version)?;
        self.check_blob_version_locked(access, version, Permission::Write)?;
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let Some(queued) = tx.get(upload_key(version).as_bytes())? else {
            return Ok(());
        };
        if decode::<BlobVersion>(&queued)? != *version {
            return Err(Error::InvalidInput("private file receipt differs from queued version"));
        }
        drop(queued);
        pin_private_content(&mut tx, &version.digest)?;
        retire_upload(&mut tx, version)?;
        tx.commit()?;
        Ok(())
    }

    /// Trusted transport calls this only after the service acknowledged the
    /// exact envelope's durable storage. Permission changes still deny commit.
    pub fn acknowledge_blob_version(&self, access: &Access, version: &BlobVersion) -> Result<()> {
        self.acknowledge_blob_version_inner(access, version, false)
    }

    fn acknowledge_blob_version_inner(
        &self,
        access: &Access,
        version: &BlobVersion,
        service: bool,
    ) -> Result<()> {
        let operations = self.access_operations(access, version.address.volume)?;
        let _guards = operations.lock()?;
        if service {
            self.check_blob_upload_locked(access, version)?;
        } else {
            self.check_blob_version_locked(access, version, Permission::Write)?;
        }
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let changed = tx.get(durable_key(version).as_bytes())?.is_none();
        tx.put(durable_key(version).as_bytes(), &encode(&true)?)?;
        if changed {
            Self::advance_provider_upload(&mut tx, version.address.volume)?;
        }
        retire_upload(&mut tx, version)?;
        tx.commit()?;
        #[cfg(any(test, feature = "file-provider"))]
        if changed {
            self.changes.provider_status_changed(version.address.volume);
        }
        drop(_profile);
        drop(_guards);
        drop(operations);
        self.cache_acknowledged_entry_blob(&version.digest)?;
        Ok(())
    }

    pub(super) fn open_entry_blob(&self, digest: &[u8; 32]) -> std::io::Result<fs::File> {
        match fs::File::open(self.blob_path(digest, false)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::File::open(self.blob_path(digest, true))
            }
            result => result,
        }
    }

    fn cache_acknowledged_entry_blob(&self, digest: &[u8; 32]) -> Result<()> {
        let _cache =
            self.blob_cache.lock().map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
        if self.has_entry_blob_pin_text(&hex(digest))? {
            return Ok(());
        }
        let retained = self.blob_path(digest, false);
        if !retained.try_exists().map_err(io)? {
            return Ok(());
        }
        let cached = self.blob_path(digest, true);
        let parent = cached.parent().ok_or(Error::Internal("cached blob path has no parent"))?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(io)?;
        fs::rename(&retained, &cached).map_err(io)?;
        super::initialization::sync_directory(parent)?;
        super::initialization::sync_directory(
            retained.parent().ok_or(Error::Internal("retained blob path has no parent"))?,
        )?;
        Ok(())
    }

    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(super) fn has_pending_entry_blob_uploads(&self) -> Result<bool> {
        let tx = self.metadata.begin_read()?;
        for item in tx.iter_from(b"entry-upload/") {
            let (key, value) = item?;
            if !key.starts_with(b"entry-upload/") {
                break;
            }
            let version: BlobVersion = decode(&value)?;
            if &key[..] != upload_key(&version).as_bytes() {
                return Err(Error::corrupt("blob upload record does not match its key"));
            }
            if tx.get(durable_key(&version).as_bytes())?.is_some() {
                continue;
            }
            let id = version.address.volume;
            if tx.has(format!("deleted-volume/{id}").as_bytes())?
                || tx.has(app_removal::erased_local_volume(id).as_bytes())?
            {
                continue;
            }
            let record =
                tx.get(format!("volume/{id}").as_bytes())?.ok_or(Error::NotFound("volume"))?;
            let volume: Volume = decode(&record)?;
            if !volume.share_key_with_service {
                // A local/private owner already holds its files. Only a current
                // invitation writer can have work to send to a private owner.
                if volume.owner != self.account.id && self.has_direct_volume_grant(id)? {
                    let writer = Access::private_volume_peer(self.account.id, volume.owner);
                    match self.check_blob_version(&writer, &version, Permission::Write) {
                        Ok(()) => return Ok(true),
                        Err(
                            Error::NotFound(_)
                            | Error::AccessDenied(_)
                            | Error::ParticipantRevoked { .. },
                        ) => {}
                        Err(error) => return Err(error),
                    }
                }
                continue;
            }
            let operations = self.operations(&[volume.id])?;
            let _guards = operations.lock()?;
            let database = self.db(volume.id, crate::store::DatabaseId::Index)?;
            match IndexSnapshot::new(database.begin_read()?).validate_blob_version(&version) {
                Ok(()) => return Ok(true),
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    pub(super) fn before_entry_blob_publication(&self) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        {
            let hook = self
                .entry_blobs
                .before_publish
                .lock()
                .map_err(|_| Error::Internal("blob publication hook lock is poisoned"))?
                .clone();
            if let Some(hook) = hook {
                hook()?;
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(
        clippy::type_complexity,
        reason = "Tests pause exactly before final authorization and publication."
    )]
    pub fn set_blob_publication_hook(
        &self,
        hook: Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>,
    ) -> Result<()> {
        *self
            .entry_blobs
            .before_publish
            .lock()
            .map_err(|_| Error::Internal("blob publication hook lock is poisoned"))? = hook;
        Ok(())
    }
}

fn upload_key(version: &BlobVersion) -> String {
    format!(
        "entry-upload/{}/{}/{}",
        version.address.volume, version.address.entry, version.revision
    )
}

fn durable_key(version: &BlobVersion) -> String {
    format!("entry-durable/{}/{}", version.address.volume, hex(&version.encrypted_key))
}

#[cfg(test)]
mod import_tests;
