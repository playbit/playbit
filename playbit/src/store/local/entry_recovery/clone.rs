//! Database copies publish a prepared typed backing and fresh namespace birth.
//! A local receipt in the birth transaction makes an uncertain copy retry safe.
use super::*;
use kvdb::LocalIndexes;

const RECEIPT_PREFIX: &[u8] = b"file-provider-clone/";
const RECEIPT_COUNT: &[u8] = b"file-provider-clone-count";
const MAX_RECEIPTS: u32 = 4096;

impl LocalStore {
    /// Clone a current database into a fresh, caller-retained identity. Reusing
    /// that identity requires the same source, destination and requested path.
    pub fn clone_database(
        &self,
        access: &Access,
        source: EntryAddress,
        destination: EntryAddress,
        parent: Option<EntryId>,
        name: &str,
    ) -> Result<VolumeEntry> {
        self.clone_database_checked(access, source, destination, parent, name, None)
    }

    pub(super) fn clone_database_checked(
        &self,
        access: &Access,
        source: EntryAddress,
        destination: EntryAddress,
        parent: Option<EntryId>,
        name: &str,
        expected: Option<&Source>,
    ) -> Result<VolumeEntry> {
        volume_index::validate_name(name)?;
        if name.contains('/') {
            return Err(Error::InvalidInput("copy filename contains slash"));
        }
        if access.session_id.is_some() || access.account != self.account.id {
            return Err(Error::AccessDenied("database copy requires local account authority"));
        }
        let receipt =
            serde_json::to_vec(&(source, parent, name)).map_err(Error::decode::<EntryAddress>)?;
        let source_cut = {
            let operations = self.operations(&[source.volume, destination.volume])?;
            let _guards = operations.lock()?;
            let source_cut = self.clone_source_locked(access, source)?;
            check_expected_source(&source_cut, expected)?;
            self.authorize_clone_destination(
                access,
                destination.volume,
                source_cut.entry.info.protocol,
            )?;
            self.file_directory_locked(access, destination.volume, parent)?;
            if let Some(entry) = self.cloned_database(destination, &receipt)? {
                return Ok(entry);
            }
            source_cut
        };
        let _credit = entry_blobs::Preparation::enter(&self.entry_blobs.active)?;
        let prepared = self.prepare_recovered_database(access, source, destination, &source_cut)?;
        if expected.is_some() {
            self.before_entry_recovery_publication()?;
        }
        let operations = self.operations(&[source.volume, destination.volume])?;
        let _guards = operations.lock()?;
        // Preparation confers no authority. Check both ends again under the
        // same sorted operation locks used by revocation and namespace writes.
        check_expected_source(&self.clone_source_locked(access, source)?, expected)?;
        self.authorize_clone_destination(
            access,
            destination.volume,
            source_cut.entry.info.protocol,
        )?;
        source_cut.check()?;
        self.file_directory_locked(access, destination.volume, parent)?;
        if let Some(parent) = parent {
            let index = self.db(destination.volume, DatabaseId::Index)?;
            let policy = self.index(destination.volume)?;
            IndexSnapshot::new(index.begin_read()?).authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                parent,
                Permission::Write,
            )?;
        }
        self.publish_cloned_database(destination, parent, name, &receipt, &source_cut, &prepared)
    }

    pub(super) fn authorize_clone_destination(
        &self,
        access: &Access,
        volume: Id,
        protocol: Option<volume_index::DatabaseProtocol>,
    ) -> Result<()> {
        let policy = self.index(volume)?;
        self.authorize_entry_creation(access, &policy)?;
        if protocol == Some(volume_index::DatabaseProtocol::BlobLog) {
            if !policy.volume().share_key_with_service {
                return Err(Error::AccessDenied(
                    "append logs require service database replication",
                ));
            }
            self.authorize(access, &policy, Permission::Write)?;
        }
        Ok(())
    }

    fn clone_source_locked(&self, access: &Access, address: EntryAddress) -> Result<Source> {
        let gate = self.volume_operation(address.volume)?;
        let epoch = gate.write_epoch.snapshot()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        self.authorize_retained_read(access, address, &snapshot)?;
        let entry = snapshot.live_entry(address.entry)?;
        if !matches!(entry.info.kind, EntryKind::KvDb {} | EntryKind::ObjectDb { .. }) {
            return Err(Error::InvalidInput("copy source is not a database"));
        }
        if !snapshot.backing_ready(address.entry)? || snapshot.schema_pending(address.entry)? {
            return Err(Error::NotFound("database contents are unavailable"));
        }
        let kind = entry.info.kind.clone();
        let permissions = snapshot.permissions(address.entry)?;
        Ok(Source { entry, permissions, kind, gate, epoch, archive: None })
    }

    fn cloned_database(
        &self,
        destination: EntryAddress,
        receipt: &[u8],
    ) -> Result<Option<VolumeEntry>> {
        let index = self.db(destination.volume, DatabaseId::Index)?;
        let snapshot = index.begin_read()?;
        let Some(saved) = snapshot.local_get(&receipt_key(destination.entry))? else {
            return Ok(None);
        };
        if saved.as_ref() != receipt {
            return Err(Error::InvalidInput("database copy identity was already used"));
        }
        drop(saved);
        Ok(Some(IndexSnapshot::new(snapshot).entry(destination.entry)?))
    }

    fn publish_cloned_database(
        &self,
        destination: EntryAddress,
        parent: Option<EntryId>,
        name: &str,
        receipt: &[u8],
        source: &Source,
        prepared: &backing::PreparedDatabase,
    ) -> Result<VolumeEntry> {
        if let Some(entry) = self.cloned_database(destination, receipt)? {
            return Ok(entry);
        }
        let index = self.db(destination.volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        match snapshot.entry(destination.entry) {
            Ok(_) => return Err(Error::AlreadyExists("copy entry identity")),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match snapshot.lookup_from(parent, name) {
            Ok(_) => return Err(Error::AlreadyExists("entry name")),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        drop(snapshot);
        let directory = self.path.join(crate::paths::VOLUMES).join(destination.volume.to_string());
        let path = directory.join(DatabaseId::Entry(destination.entry).file_name());
        // An earlier attempt may have linked its backing before dying before
        // the birth commit. No birth or receipt exists while these locks hold.
        match fs::remove_file(path) {
            Ok(()) => initialization::sync_directory(&directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        prepared.publish(&directory, destination.entry)?;
        let info = EntryInfo {
            namespace: volume_index::namespace::Birth { parent, ..Default::default() },
            origin: volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::generate()?,
            name: name.to_owned(),
            kind: source.kind.clone(),
            protocol: source.entry.info.protocol,
        };
        let mut tx = index.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        write_receipt(&mut tx.local_indexes(), destination.entry, receipt)?;
        volume_index::create(&mut tx, destination.entry, &info)?;
        volume_index::mark_backing_ready(&mut tx, destination.entry)?;
        let delta = tx.commit_with_delta()?;
        self.changes.publish(destination.volume, DatabaseId::Index, delta);
        IndexSnapshot::new(index.begin_read()?).live_entry(destination.entry)
    }
}

fn check_expected_source(current: &Source, expected: Option<&Source>) -> Result<()> {
    if expected.is_some_and(|expected| {
        current.entry != expected.entry
            || current.permissions != expected.permissions
            || current.kind != expected.kind
    }) {
        return Err(Error::LocalContention("entry changed during copy preparation"));
    }
    Ok(())
}

fn receipt_key(entry: EntryId) -> Vec<u8> {
    let mut key = RECEIPT_PREFIX.to_vec();
    key.extend_from_slice(entry.as_bytes());
    key
}

fn write_receipt(
    tx: &mut LocalIndexes<'_, '_, RedbStorage>,
    entry: EntryId,
    receipt: &[u8],
) -> Result<()> {
    let count = match tx.get(RECEIPT_COUNT)? {
        Some(bytes) => u32::from_le_bytes(
            bytes.as_ref().try_into().map_err(|_| Error::corrupt("copy receipt count"))?,
        ),
        None => 0,
    };
    if count >= MAX_RECEIPTS {
        return Err(Error::LimitExceeded);
    }
    tx.put(&receipt_key(entry), receipt)?;
    tx.put(RECEIPT_COUNT, &(count + 1).to_le_bytes())?;
    Ok(())
}
