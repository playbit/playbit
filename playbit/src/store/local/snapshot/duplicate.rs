//! Duplicate a consistent authorized cut into unpublished, independently keyed
//! storage. Publish its account identity only after every live entry is durable.
use super::*;
use crate::store::volume_index::{self, DatabaseProtocol, EntryInfo};

const DUPLICATION_INTENT: &[u8] = b"duplicating-volume";

impl LocalStore {
    pub fn duplicate_volume(&self, source: Id) -> Result<Volume> {
        let access = Access::owner(self.account.id);
        self.require_volume_available(source)?;
        if self.replica_root(source, DatabaseId::Index)?.is_none() {
            return Err(Error::NotFound(
                "volume data is unavailable; connect a device that has it before duplicating",
            ));
        }
        let _credit = entry_blobs::Preparation::enter(&self.entry_blobs.active)?;
        let snapshot = self.capture_volume(&access, source)?;
        let original = snapshot.with_database(DatabaseId::Index, |tx| {
            Ok(MetadataPolicy::read_snapshot(&tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?
                .volume()
                .clone())
        })?;
        let mut name = original.name;
        while name.len() + " Copy".len() > 256 {
            name.pop();
        }
        name.push_str(" Copy");
        let volume = Volume {
            id: Id::generate()?,
            name,
            owner: self.account.id,
            personal: true,
            share_key_with_service: original.share_key_with_service,
        };
        let credentials = Credentials { database_key: random_array()?, blob_key: random_array()? };
        let directory = snapshot.directory.0.join("duplicate");
        fs::DirBuilder::new().mode(0o700).create(&directory).map_err(io)?;
        self.prepare_duplicate(&snapshot, &directory, &volume, &credentials)?;
        self.publish_duplicate(source, &directory, &volume, credentials)?;
        self.changes.volume_available(volume.id);
        Ok(volume)
    }

    fn prepare_duplicate(
        &self,
        source: &VolumeSnapshot,
        directory: &Path,
        volume: &Volume,
        credentials: &Credentials,
    ) -> Result<()> {
        let index = create_duplicate_index(directory, volume, credentials)?;
        source.with_database(DatabaseId::Index, |tx| {
            let snapshot = IndexSnapshot::new(tx);
            // Copy parents first, without rejected facts or the source volume’s causal context.
            let mut pending = snapshot.children(None)?;
            while let Some(header) = pending.pop() {
                let entry = snapshot.live_entry(header.id)?;
                let mut info = EntryInfo {
                    namespace: volume_index::namespace::Birth {
                        parent: header.parent,
                        ..Default::default()
                    },
                    origin: volume_index::namespace::Origin {
                        installation: self.installation(),
                        label: None,
                    },
                    name: header.name,
                    initial_permissions: EntryAcl::Inherit,
                    created_revision: crate::store::EntryId::generate()?,
                    ..entry.info
                };
                if matches!(info.kind, EntryKind::Blob { .. }) {
                    let version = snapshot.blob_version(
                        EntryAddress { volume: source.volume, entry: header.id },
                        None,
                    )?;
                    let mut file = source.open_blob(&version)?;
                    info.kind = self.prepare_entry_blob_with_key(
                        EntryAddress { volume: volume.id, entry: header.id },
                        info.created_revision,
                        &mut file,
                        &credentials.blob_key,
                        true,
                        || self.lock_profile(),
                    )?;
                } else if matches!(info.kind, EntryKind::Directory {}) {
                    pending.extend(snapshot.children(Some(header.id))?);
                    if pending.len() > volume_index::namespace::MAX_ENTRIES {
                        return Err(Error::LimitExceeded);
                    }
                } else {
                    copy_backing(
                        source,
                        directory,
                        volume.id,
                        credentials,
                        header.id,
                        info.protocol,
                    )?;
                }
                let mut tx = index.begin_write(now_unix_secs())?;
                volume_index::create(&mut tx, header.id, &info)?;
                if !matches!(info.kind, EntryKind::Directory {}) {
                    volume_index::mark_backing_ready(&mut tx, header.id)?;
                }
                tx.commit()?;
            }
            Ok(())
        })?;
        drop(index);
        initialization::sync_directory(directory)
    }

    fn publish_duplicate(
        &self,
        source: Id,
        directory: &Path,
        volume: &Volume,
        credentials: Credentials,
    ) -> Result<()> {
        let operations = self.operations(&[source, volume.id])?;
        let _guards = operations.lock()?;
        self.require_volume_available(source)?;
        let _profile = self.lock_profile()?;
        self.recover_volume_duplication()?;
        let target = self.path.join(crate::paths::VOLUMES).join(volume.id.to_string());
        if target.try_exists().map_err(io)? {
            return Err(Error::AlreadyExists("duplicate volume identity"));
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(DUPLICATION_INTENT, &encode(&volume.id)?)?;
        tx.commit()?;
        fs::rename(directory, &target).map_err(io)?;
        initialization::sync_directory(&self.path.join(crate::paths::VOLUMES))?;
        let pending = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::RegisterVolume { volume: volume.clone() },
        };
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(format!("volume/{}", volume.id).as_bytes(), &encode(volume)?)?;
        tx.put(format!("credentials/{}", volume.id).as_bytes(), &encode(&credentials)?)?;
        tx.put(format!("pending/{}", pending.id).as_bytes(), &encode(&pending)?)?;
        let changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::Volume { volume: volume.clone(), credentials },
        )?;
        tx.del(DUPLICATION_INTENT)?;
        tx.commit()?;
        self.committed_account_record(changed);
        Ok(())
    }

    pub(in crate::store::local) fn recover_volume_duplication(&self) -> Result<()> {
        let snapshot = self.metadata.begin_read()?;
        let Some(bytes) = snapshot.get(DUPLICATION_INTENT)? else { return Ok(()) };
        let volume: Id = decode(&bytes)?;
        if snapshot.has(format!("volume/{volume}").as_bytes())?
            || snapshot.has(format!("credentials/{volume}").as_bytes())?
        {
            return Err(Error::corrupt("unpublished duplicate has retained credentials"));
        }
        drop(bytes);
        drop(snapshot);
        match fs::remove_dir_all(self.path.join(crate::paths::VOLUMES).join(volume.to_string())) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        initialization::sync_directory(&self.path.join(crate::paths::VOLUMES))?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.del(DUPLICATION_INTENT)?;
        tx.commit()?;
        Ok(())
    }
}

fn create_duplicate_index(
    directory: &Path,
    volume: &Volume,
    credentials: &Credentials,
) -> Result<Database> {
    let index = create_replica(
        &directory.join(DatabaseId::Index.file_name()),
        DatabaseId::Index.hash_key(&credentials.database_key, volume.id),
        CACHE_BYTES,
    )?;
    let policy = MetadataPolicy::new(volume.clone(), Acl::default())?;
    policy.write(&index, now_unix_secs())?;
    drop(create_replica(
        &directory.join(DatabaseId::Revocations.file_name()),
        DatabaseId::Revocations.hash_key(&credentials.database_key, volume.id),
        CACHE_BYTES,
    )?);
    Ok(index)
}

fn copy_backing(
    source: &VolumeSnapshot,
    directory: &Path,
    volume: Id,
    credentials: &Credentials,
    entry: crate::store::EntryId,
    protocol: Option<DatabaseProtocol>,
) -> Result<()> {
    let id = DatabaseId::Entry(entry);
    let target = create_replica(
        &directory.join(id.file_name()),
        id.hash_key(&credentials.database_key, volume),
        CACHE_BYTES,
    )?;
    if protocol == Some(DatabaseProtocol::BlobLog) {
        // Validate the captured backing against its manifest before opening Log.
        source.with_database(id, |_| Ok(()))?;
        let original = open_replica(&source.directory.0.join(id.file_name()), CACHE_BYTES)?;
        let old = crate::store::bloblog::Log {
            database: &original,
            volume: source.volume,
            entry,
            encryption_key: None,
            now: now_unix_secs(),
        };
        let new = crate::store::bloblog::Log {
            database: &target,
            volume,
            entry,
            encryption_key: None,
            now: now_unix_secs(),
        };
        let mut after = None;
        loop {
            let page = old.get(after.as_ref())?;
            new.apply(&page.entries, true)?;
            if !page.more {
                break;
            }
            after = Some(page.cursor);
        }
    } else {
        source.with_database(id, |source| copy_records(&source, &target))?;
    }
    Ok(())
}

fn copy_records(source: &kvdb::ReadTxn<'_, RedbStorage>, target: &Database) -> Result<()> {
    let mut after = Vec::new();
    let mut exclusive = false;
    loop {
        let mut tx = target.begin_write(now_unix_secs())?;
        let mut copied = 0;
        while copied < 64 {
            let Some(key) = source.next_key(&after, exclusive)? else {
                if copied != 0 {
                    tx.commit()?;
                }
                return Ok(());
            };
            let value = source
                .value_reader(&key)?
                .ok_or(Error::corrupt("duplicate source record is missing"))?;
            tx.put_from_with_clock(
                &key,
                &value,
                value.clock().ok_or(Error::corrupt("duplicate source clock is missing"))?,
            )?;
            after = key;
            exclusive = true;
            copied += 1;
        }
        tx.commit()?;
    }
}
