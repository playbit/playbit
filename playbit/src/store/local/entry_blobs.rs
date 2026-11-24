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

pub struct BlobUploadPage {
    pub versions: Vec<BlobVersion>,
    pub next: Option<(EntryId, EntryId)>,
}

#[derive(Default)]
pub(super) struct BlobPreparations {
    pub(super) active: AtomicUsize,
    #[cfg(test)]
    #[allow(clippy::type_complexity, reason = "The hook stops exactly at blob index publication.")]
    pub(super) before_publish: Mutex<Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>>,
    #[cfg(test)]
    #[allow(clippy::type_complexity, reason = "Pause recovery before its final authority check.")]
    pub(super) recovery_before_publish: Mutex<Option<Arc<dyn Fn() -> Result<()> + Send + Sync>>>,
}

fn next_preparation(active: usize) -> Option<usize> {
    if active < MAX_PREPARATIONS { Some(active + 1) } else { None }
}

pub(super) struct Preparation<'a>(&'a AtomicUsize);
impl<'a> Preparation<'a> {
    pub(super) fn enter(active: &'a AtomicUsize) -> Result<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, next_preparation)
            .map_err(|_| Error::LocalContention("Both blob preparation slots are in use"))?;
        Ok(Self(active))
    }
}
impl Drop for Preparation<'_> {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::AcqRel);
        assert!(
            previous > 0 && previous <= MAX_PREPARATIONS,
            "preparation releases its own credit"
        );
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

fn queue_upload(tx: &mut kvdb::WriteTxn<'_, RedbStorage>, version: &BlobVersion) -> Result<()> {
    let upload = upload_key(version);
    if tx.get(upload.as_bytes())?.is_none() {
        let count = pending_count_key(&version.digest);
        let active =
            tx.get(count.as_bytes())?.map(|bytes| decode::<u64>(&bytes)).transpose()?.unwrap_or(0);
        let next = active.checked_add(1).ok_or(Error::LimitExceeded)?;
        tx.put(count.as_bytes(), &encode(&next)?)?;
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
        let _preparation = Preparation::enter(&self.entry_blobs.active)?;
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
        let _preparation = Preparation::enter(&self.entry_blobs.active)?;
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
        let _preparation = Preparation::enter(&self.entry_blobs.active)?;
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
        use std::io::Seek;
        self.check_blob_version(access, version, Permission::Write)?;
        let mut plaintext = self.verify_blob_for(access, version, Permission::Write)?;
        let _preparation = Preparation::enter(&self.entry_blobs.active)?;
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
        self.check_blob_version(access, version, Permission::Write)?;
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
        self.check_blob_version(access, version, Permission::Read)?;
        let _preparation = Preparation::enter(&self.entry_blobs.active)?;
        let mut plaintext =
            super::blob_files::BlobStage::new(&self.blob_stage_directory(durable)?)?;
        crate::store::open_blob_to(
            &self.credentials(version.address.volume)?.blob_key,
            version,
            ciphertext,
            &mut plaintext.file,
        )?;
        plaintext.sync()?;
        self.publish_received_blob(access, version, &plaintext, durable)
    }

    fn publish_received_blob(
        &self,
        access: &Access,
        version: &BlobVersion,
        plaintext: &super::blob_files::BlobStage,
        durable: bool,
    ) -> Result<()> {
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
            self.before_entry_blob_publication()?;
            let operations = self.access_operations(access, version.address.volume)?;
            let _guards = operations.lock()?;
            self.check_blob_version_locked(access, version, Permission::Read)?;
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
            self.retain_received_entry_blob(version, durable)?;
            let database = self.db(version.address.volume, crate::store::DatabaseId::Index)?;
            let current = match IndexSnapshot::new(database.begin_read()?)
                .blob_version(version.address, None)
            {
                Ok(current) => Some(current),
                Err(Error::NotFound("retired entry")) => None,
                Err(error) => return Err(error),
            };
            if current.as_ref() == Some(version) {
                let mut tx = database.begin_write(now_unix_secs())?;
                volume_index::mark_backing_ready(&mut tx, version.address.entry)?;
                tx.commit()?;
            }
            return Ok(());
        }
        Err(Error::LocalContention("blob cache changed during verified publication"))
    }

    fn retain_received_entry_blob(&self, version: &BlobVersion, durable: bool) -> Result<()> {
        let key = format!(
            "{}{}/{}/{}",
            pin_prefix(&version.digest),
            version.address.volume,
            version.address.entry,
            version.revision
        );
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(key.as_bytes(), &encode(version)?)?;
        let changed = durable && tx.get(durable_key(version).as_bytes())?.is_none();
        if durable {
            tx.put(durable_key(version).as_bytes(), &encode(&true)?)?;
            if changed {
                Self::advance_provider_upload(&mut tx, version.address.volume)?;
            }
            retire_upload(&mut tx, version)?;
        } else if tx.get(durable_key(version).as_bytes())?.is_none() {
            queue_upload(&mut tx, version)?;
        }
        tx.commit()?;
        #[cfg(any(test, feature = "file-provider"))]
        if changed {
            self.changes.provider_status_changed(version.address.volume);
        }
        Ok(())
    }

    /// Page both live current and retained historical bodies. A metadata ack
    /// never removes this queue: only verified AP body durability does.
    pub fn pending_entry_blobs(
        &self,
        access: &Access,
        volume: Id,
        after: Option<(EntryId, EntryId)>,
    ) -> Result<BlobUploadPage> {
        self.pending_blob_uploads(access, volume, None, after)
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn pending_selected_entry_blobs(
        &self,
        access: &Access,
        address: EntryAddress,
        after: Option<(EntryId, EntryId)>,
    ) -> Result<BlobUploadPage> {
        self.pending_blob_uploads(access, address.volume, Some(address.entry), after)
    }

    fn pending_blob_uploads(
        &self,
        access: &Access,
        volume: Id,
        selected: Option<EntryId>,
        after: Option<(EntryId, EntryId)>,
    ) -> Result<BlobUploadPage> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.admit_access(access, &policy)?;
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
        let mut versions = Vec::with_capacity(UPLOAD_PAGE);
        let mut scanned = 0;
        let mut next = None;
        while let Some((key, value)) = iter.next().transpose()? {
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let version: BlobVersion = decode(&value)?;
            if &key[..] != upload_key(&version).as_bytes() || version.address.volume != volume {
                return Err(Error::corrupt("blob upload record does not match its key or volume"));
            }
            let cursor = (version.address.entry, version.revision);
            if Some(cursor) == after {
                continue;
            }
            if scanned == UPLOAD_PAGE {
                break;
            }
            scanned += 1;
            next = Some(cursor);
            match self.check_blob_version_locked(access, &version, Permission::Write) {
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
        #[cfg(test)]
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

    #[cfg(test)]
    #[allow(
        clippy::type_complexity,
        reason = "Tests pause exactly before final authorization and publication."
    )]
    pub(crate) fn set_blob_publication_hook(
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
