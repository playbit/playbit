//! Recovery writes a new typed backing off the live volume writer boundary.
use super::*;
use crate::store::{bloblog::Log, volume_index::DatabaseProtocol};

pub(super) struct PreparedDatabase {
    // Close redb before removing its staging directory.
    _database: Database,
    directory: ProjectionDirectory,
}
impl PreparedDatabase {
    pub(super) fn publish(&self, volume: &Path, entry: EntryId) -> Result<()> {
        let source = self.directory.0.join("backing.kvdb");
        let destination = volume.join(DatabaseId::Entry(entry).file_name());
        match fs::hard_link(&source, &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::AlreadyExists("entry backing identity"));
            }
            Err(error) => return Err(io(error)),
        }
        initialization::sync_directory(volume)
    }
}

impl LocalStore {
    pub(super) fn prepare_recovered_database(
        &self,
        access: &Access,
        address: EntryAddress,
        destination: EntryAddress,
        source: &Source,
    ) -> Result<PreparedDatabase> {
        let original;
        let pinned;
        {
            let operations = self.access_operations(access, address.volume)?;
            let _guards = operations.lock()?;
            source.check()?;
            original = if let Some(directory) = &source.archive {
                Arc::new(open_replica(
                    &directory.join(DatabaseId::Entry(address.entry).file_name()),
                    512 * 1024,
                )?)
            } else {
                self.entry_backing(address.volume, address.entry, false)?
            };
            pinned = original.begin_read()?;
            source.check()?;
        }
        let parent = self.path.join(crate::paths::STAGING).join("entry-recovery");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&parent).map_err(io)?;
        let directory = ProjectionDirectory(parent.join(Id::generate()?.to_string()));
        fs::DirBuilder::new().mode(0o700).create(&directory.0).map_err(io)?;
        let key = DatabaseId::Entry(destination.entry)
            .hash_key(&self.credentials(destination.volume)?.database_key, destination.volume);
        let path = directory.0.join("backing.kvdb");
        let database = create_db(&path, Some(key), 512 * 1024)?;
        if source.entry.info.protocol == Some(DatabaseProtocol::BlobLog) {
            recover_log(&original, &database, address, destination, source)?;
        } else {
            copy_records(&pinned, &database, source)?;
            if matches!(source.kind, EntryKind::ObjectDb { .. }) {
                let tx = database.begin_write(now_unix_secs())?;
                crate::store::object_database::validate_all(&tx)?;
            }
        }
        source.check()?;
        fs::File::open(path).map_err(io)?.sync_all().map_err(io)?;
        initialization::sync_directory(&directory.0)?;
        Ok(PreparedDatabase { _database: database, directory })
    }
}

fn copy_records(
    source: &kvdb::ReadTxn<'_, RedbStorage>,
    target: &Database,
    cut: &Source,
) -> Result<()> {
    let mut after = Vec::new();
    let mut exclusive = false;
    loop {
        cut.check()?;
        let mut tx = target.begin_write(now_unix_secs())?;
        let mut copied = 0;
        while copied < 64 {
            let Some(key) = source.next_key(&after, exclusive)? else {
                if copied != 0 {
                    tx.commit()?;
                }
                return cut.check();
            };
            let value = source
                .value_reader(&key)?
                .ok_or(Error::corrupt("recovery source record is missing"))?;
            tx.put_from_with_clock(
                &key,
                &value,
                value.clock().ok_or(Error::corrupt("recovery source record clock is missing"))?,
            )?;
            after = key;
            exclusive = true;
            copied += 1;
        }
        tx.commit()?;
    }
}

fn recover_log(
    original: &Database,
    target: &Database,
    address: EntryAddress,
    destination: EntryAddress,
    cut: &Source,
) -> Result<()> {
    let original = Log {
        database: original,
        volume: address.volume,
        entry: address.entry,
        encryption_key: None,
        now: now_unix_secs(),
    };
    let target = Log {
        database: target,
        volume: destination.volume,
        entry: destination.entry,
        encryption_key: None,
        now: now_unix_secs(),
    };
    let mut validation = None;
    loop {
        cut.check()?;
        validation = original.validate_page(validation.as_deref(), 64)?;
        if validation.is_none() {
            break;
        }
    }
    let mut after = None;
    loop {
        cut.check()?;
        let page = original.get(after.as_ref())?;
        cut.check()?;
        let entries = page
            .entries
            .into_iter()
            .map(|record| record.recover_into(destination.entry))
            .collect::<Vec<_>>();
        target.apply(&entries, true)?;
        if !page.more {
            return cut.check();
        }
        after = Some(page.cursor);
    }
}
