//! Identity-bound volume File destinations. Backing preparation runs before the
//! short publication transaction; final checks share the replication gate.
use super::*;
use crate::store::{
    EntryId,
    volume_index::{self, EntryInfo, EntryKind, IndexSnapshot, VolumeEntry},
};
use std::io::Read;

/// Constructed only by an authorized open. Existing files retain their identity;
/// each open against absence owns a different fresh identity.
#[derive(Debug)]
pub struct FileDestination {
    volume: Id,
    directory: Option<EntryId>,
    parent: Option<EntryId>,
    entry: EntryId,
    name: String,
    create: bool,
    exclusive: bool,
    attachment: Option<(EntryId, EntryId)>,
    creation_context: Option<volume_index::namespace::CreationContext>,
    provider: bool,
    provider_version: Option<([u8; 32], [u8; 32])>,
    provider_move: Option<(Option<EntryId>, String)>,
}

/// One already-authorized writer destination. This authority never crosses a
/// codec boundary and cannot be used to open another File or execute guest data.
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
pub(crate) struct FileStagingAccess {
    access: Access,
    target: FileDestination,
    session: Option<Session>,
}

struct DestinationEntry {
    id: EntryId,
    create: bool,
    attachment: Option<(EntryId, EntryId)>,
}

#[derive(Debug)]
pub struct FilePublicationFailure {
    pub error: Error,
    /// A storage commit error does not prove that publication did not occur.
    pub publication_unknown: bool,
}

impl LocalStore {
    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
    pub(crate) fn capture_file_staging(
        &self,
        access: &Access,
        target: FileDestination,
    ) -> Result<FileStagingAccess> {
        if target.provider {
            return Err(Error::InvalidInput("provider destination is not an app File writer"));
        }
        let operations = self.access_operations(access, target.volume)?;
        let _guards = operations.lock()?;
        self.validate_file_destination(access, &target)?;
        let session = match access.session_id.as_deref() {
            Some(id) if self.has_automatic_scopes(id)? => Some(self.session(id)?),
            _ => None,
        };
        Ok(FileStagingAccess { access: access.clone(), target, session })
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
    pub(crate) fn check_file_staging(&self, staging: &FileStagingAccess) -> Result<()> {
        self.file_staging_access_with_session(
            &staging.access,
            &staging.target,
            staging.session.as_ref(),
        )
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime", test))]
    pub(crate) fn publish_staged_file(
        &self,
        staging: &FileStagingAccess,
        source: &mut impl Read,
        sync: bool,
    ) -> std::result::Result<(), FilePublicationFailure> {
        self.publish_file_with_session(
            &staging.access,
            &staging.target,
            source,
            sync,
            staging.session.as_ref(),
        )
    }

    /// File control retains the original build's Root provisioning. Selecting
    /// code cannot withdraw an existing grant, while identity/policy loss can.
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn admit_file_binding(
        &self,
        access: &Access,
        retained: &Session,
        volume: Id,
    ) -> Result<()> {
        if !self.has_automatic_scopes(&retained.id)? {
            return self.admit_binding(access, volume);
        }
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.require_account_authority()?;
        let current = self.session(&retained.id)?;
        if access.account != self.account.id
            || access.session_id.as_deref() != Some(retained.id.as_str())
            || access.build_id.as_deref() != Some(retained.resolved_build_id.as_str())
            || !current.same_identity(retained)
            || self.session_denial(&retained.id)?.is_some()
        {
            return Err(Error::AccessDenied("retained File binding changed"));
        }
        if volume == current.root_volume && self.has_implicit_local_root(&current)? {
            if current.shared_volume.is_some() && self.shared_session_initialized(&current.id)? {
                self.check_participant(current.replica_volume(), access.account)?;
                self.check_dependency_authority(access.account, current.replica_volume())?;
            }
            return Ok(());
        }
        self.admit_access_with_file_session(access, &self.index(volume)?, Some(retained))?;
        self.check_dependency_authority(access.account, volume)?;
        if current.replica_volume() != volume
            && ((current.shared_volume.is_some()
                && self.shared_session_initialized(&current.id)?)
                || !self.has_implicit_local_root(&current)?)
        {
            self.check_dependency_authority(access.account, current.replica_volume())?;
        }
        Ok(())
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn file_binding_policy(
        &self,
        access: &Access,
        retained: &Session,
        volume: Id,
    ) -> Result<MetadataPolicy> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        let retained = self.has_automatic_scopes(&retained.id)?.then_some(retained);
        self.authorize_with_file_session(access, &policy, Permission::Read, retained)?;
        Ok(policy)
    }

    pub fn provider_destination(
        &self,
        volume: Id,
        id: EntryId,
        parent: Option<EntryId>,
        name: &str,
        content: &[u8],
        metadata: &[u8],
    ) -> Result<FileDestination> {
        self.provider_volume(volume)?;
        volume_index::validate_name(name)?;
        if name.contains('/') {
            return Err(Error::InvalidInput("provider filename contains slash"));
        }
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        let current = self.provider_item_locked(volume, &access, &policy, &snapshot, id)?;
        if current.content_version != content || current.metadata_version != metadata {
            return Err(Error::AlreadyExists("provider item version changed"));
        }
        let source = snapshot.namespace_entry(id)?;
        let entry = snapshot.live_entry(id)?;
        self.authorize_file_replacement(&access, &policy, &snapshot, &entry)?;
        let target = FileDestination {
            volume,
            directory: source.parent,
            parent: source.parent,
            entry: id,
            name: source.name.clone(),
            create: false,
            exclusive: false,
            attachment: Some((source.parent_event, source.name_event)),
            creation_context: None,
            provider: true,
            provider_version: Some((current.content_version, current.metadata_version)),
            provider_move: (source.parent != parent || source.name != name)
                .then(|| (parent, name.to_owned())),
        };
        self.validate_file_destination(&access, &target)?;
        Ok(target)
    }

    #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
    pub(crate) fn file_delivery_volumes(&self, access: &Access, volume: Id) -> Result<Vec<Id>> {
        let mut volumes = vec![volume];
        if let Some(id) = &access.session_id {
            let session = self.session(id)?;
            volumes.extend([session.replica_volume()]);
        }
        volumes.sort_unstable();
        volumes.dedup();
        Ok(volumes)
    }

    /// Resolve relative to the retained directory identity, including after moves.
    pub fn file_entry(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
        path: &str,
    ) -> Result<VolumeEntry> {
        volume_index::validate_name(path)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.file_directory_snapshot(access, &policy, volume, parent, &snapshot)?;
        let entry = snapshot.lookup_from(parent, path)?;
        self.check_authority(access, volume, Some(entry.id))?;
        snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry.id,
            Permission::Read,
        )?;
        Ok(entry)
    }

    pub fn file_directory(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
    ) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.file_directory_locked(access, volume, parent)
    }

    pub(super) fn file_directory_locked(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
    ) -> Result<()> {
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.file_directory_snapshot(access, &policy, volume, parent, &snapshot)
    }

    pub(super) fn file_directory_snapshot(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        volume: Id,
        parent: Option<EntryId>,
        snapshot: &IndexSnapshot<'_, impl kvdb::Storage>,
    ) -> Result<()> {
        self.file_directory_snapshot_with_session(access, policy, volume, parent, snapshot, None)
    }

    fn file_directory_snapshot_with_session(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        volume: Id,
        parent: Option<EntryId>,
        snapshot: &IndexSnapshot<'_, impl kvdb::Storage>,
        retained: Option<&Session>,
    ) -> Result<()> {
        self.authorize_with_file_session(access, policy, Permission::Read, retained)?;
        snapshot.directory(parent)?;
        if let Some(parent) = parent {
            self.check_authority(access, volume, Some(parent))?;
            snapshot.authorize_retained_read(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                parent,
            )?;
        }
        Ok(())
    }

    pub fn file_entries(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
    ) -> Result<Vec<volume_index::EntrySummary>> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.file_directory_snapshot(access, &policy, volume, parent, &snapshot)?;
        snapshot.children(parent)
    }

    /// Captured listings and private recovery survive namespace deletion, but
    /// never a change to the original grant, participant or retained Read ACL.
    pub fn file_retained_directory_access(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
    ) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize(access, &policy, Permission::Read)?;
        if let Some(parent) = parent {
            self.check_authority(access, volume, Some(parent))?;
            let db = self.db(volume, crate::store::DatabaseId::Index)?;
            IndexSnapshot::new(db.begin_read()?).authorize_retained_read(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                parent,
            )?;
        }
        Ok(())
    }

    /// Create missing parents at open, leaving them in place after discard. No
    /// file or pathname reservation is published until close.
    pub fn file_destination(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
        path: &str,
        exclusive: bool,
    ) -> Result<FileDestination> {
        volume_index::validate_name(path)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        if self.admit_access(access, &policy)?.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let mut snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.file_directory_snapshot(access, &policy, volume, parent, &snapshot)?;
        let directory = parent;
        let (parent, name) = match path.rsplit_once('/') {
            None => (parent, path),
            Some((path, name)) => {
                let existing = snapshot.lookup_from(parent, path);
                let id = match existing {
                    Ok(entry) if matches!(entry.info.kind, EntryKind::Directory {}) => entry.id,
                    Ok(_) => {
                        return Err(Error::InvalidInput("entry path component is not a directory"));
                    }
                    Err(Error::NotFound(_)) => {
                        let id = self.create_file_directory(access, &policy, &db, parent, path)?;
                        snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
                        id
                    }
                    Err(error) => return Err(error),
                };
                (Some(id), name)
            }
        };
        self.file_directory_snapshot(access, &policy, volume, parent, &snapshot)?;
        let DestinationEntry { id: entry, create, attachment } =
            self.file_destination_entry(access, &policy, &snapshot, parent, name, exclusive)?;
        let creation_context = if create {
            Some(volume_index::namespace::observations::Publication::capture(&db.begin_read()?)?)
        } else {
            None
        };
        Ok(FileDestination {
            volume,
            directory,
            parent,
            entry,
            name: name.to_owned(),
            create,
            exclusive,
            attachment,
            creation_context,
            provider: false,
            provider_version: None,
            provider_move: None,
        })
    }

    fn file_destination_entry(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        parent: Option<EntryId>,
        name: &str,
        exclusive: bool,
    ) -> Result<DestinationEntry> {
        match snapshot.lookup_from(parent, name) {
            Ok(_) if exclusive => Err(Error::AlreadyExists("file destination")),
            Ok(entry) => {
                self.authorize_file_replacement(access, policy, snapshot, &entry)?;
                let location = snapshot.namespace_entry(entry.id)?;
                Ok(DestinationEntry {
                    id: entry.id,
                    create: false,
                    attachment: Some((location.parent_event, location.name_event)),
                })
            }
            Err(Error::NotFound(_)) => {
                self.authorize_blob_mutation(access, policy.volume().id, None)?;
                Ok(DestinationEntry { id: EntryId::generate()?, create: true, attachment: None })
            }
            Err(error) => Err(error),
        }
    }

    fn authorize_file_replacement(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        entry: &VolumeEntry,
    ) -> Result<()> {
        if !matches!(entry.info.kind, EntryKind::Blob { .. }) {
            return Err(Error::InvalidInput("entry is not an independent blob"));
        }
        self.check_authority(access, policy.volume().id, Some(entry.id))?;
        snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry.id,
            Permission::Write,
        )?;
        Ok(())
    }

    fn create_file_directory(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        db: &Database,
        parent: Option<EntryId>,
        path: &str,
    ) -> Result<EntryId> {
        let volume = policy.volume().id;
        self.authorize_entry_creation(access, policy)?;
        let id = EntryId::generate()?;
        let info = EntryInfo {
            namespace: volume_index::namespace::Birth {
                parent,
                automatic: true,
                ..Default::default()
            },
            origin: volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::generate()?,
            name: path.rsplit('/').next().expect("validated path").to_owned(),
            kind: EntryKind::Directory {},
            protocol: None,
        };
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        volume_index::create_at_path(&mut tx, id, path, &info)?;
        let delta = tx.commit_with_delta()?;
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        Ok(id)
    }

    fn validate_file_destination(&self, access: &Access, target: &FileDestination) -> Result<()> {
        self.validate_file_destination_with_session(access, target, None)
    }

    fn validate_file_destination_with_session(
        &self,
        access: &Access,
        target: &FileDestination,
        retained: Option<&Session>,
    ) -> Result<()> {
        let policy = self.index(target.volume)?;
        let db = self.db(target.volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.validate_provider_destination(access, target, &policy, &snapshot)?;
        self.file_directory_snapshot_with_session(
            access,
            &policy,
            target.volume,
            target.parent,
            &snapshot,
            retained,
        )?;
        if let Some(directory) = target.directory.filter(|id| Some(*id) != target.parent) {
            self.check_authority(access, target.volume, Some(directory))?;
            snapshot.authorize_retained_read(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                directory,
            )?;
        }
        if self
            .admit_access_with_file_session(access, &policy, retained)?
            .is_some_and(|grant| !grant.write)
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        if target.create {
            self.authorize_file_creation(access, &policy, retained)?;
            if target.exclusive {
                match snapshot.lookup_from(target.parent, &target.name) {
                    Ok(_) => return Err(Error::AlreadyExists("file destination")),
                    Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        } else {
            self.check_authority(access, target.volume, Some(target.entry))?;
            snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                target.entry,
                Permission::Write,
            )?;
            // Coalescence and generated conflict names are derived routing,
            // not new attachment events. Intentional moves/renames still fail.
            let entry = snapshot.namespace_entry(target.entry)?;
            if entry.parent != snapshot.canonical_directory(target.parent)?
                || Some((entry.parent_event, entry.name_event)) != target.attachment
            {
                return Err(Error::AlreadyExists("file destination changed"));
            }
        }
        Ok(())
    }

    fn validate_provider_destination(
        &self,
        access: &Access,
        target: &FileDestination,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
    ) -> Result<()> {
        if target.provider {
            self.provider_volume(target.volume)?;
            if target.create
                && let Some(parent) = target.parent
            {
                snapshot.authorize_namespace(
                    policy.volume(),
                    policy.default_permissions(),
                    self.data_actor(access, policy.volume())?,
                    parent,
                    Permission::Write,
                )?;
            }
        }
        if let Some((content, metadata)) = target.provider_version {
            self.provider_volume(target.volume)?;
            let current =
                self.provider_item_locked(target.volume, access, policy, snapshot, target.entry)?;
            if current.content_version != content || current.metadata_version != metadata {
                return Err(Error::AlreadyExists("provider item version changed"));
            }
        }
        if let Some((parent, _)) = &target.provider_move {
            self.file_directory_snapshot(access, policy, target.volume, *parent, snapshot)?;
            if let Some(parent) = parent {
                snapshot.authorize_namespace(
                    policy.volume(),
                    policy.default_permissions(),
                    self.data_actor(access, policy.volume())?,
                    *parent,
                    Permission::Write,
                )?;
            }
        }
        Ok(())
    }

    pub fn file_staging_access(&self, access: &Access, target: &FileDestination) -> Result<()> {
        self.file_staging_access_with_session(access, target, None)
    }

    fn authorize_file_creation(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        retained: Option<&Session>,
    ) -> Result<()> {
        match self.authorize_with_file_session(access, policy, Permission::Write, retained) {
            Ok(()) => Ok(()),
            Err(Error::AccessDenied(_)) => {
                self.authorize_with_file_session(access, policy, Permission::Append, retained)
            }
            Err(error) => Err(error),
        }
    }

    fn file_staging_access_with_session(
        &self,
        access: &Access,
        target: &FileDestination,
        retained: Option<&Session>,
    ) -> Result<()> {
        let operations = self.access_operations(access, target.volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(target.volume)?;
        self.authorize_with_file_session(access, &policy, Permission::Read, retained)?;
        if self
            .admit_access_with_file_session(access, &policy, retained)?
            .is_some_and(|grant| !grant.write)
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let db = self.db(target.volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(db.begin_read()?);
        for parent in [target.directory, target.parent].into_iter().flatten() {
            self.check_authority(access, target.volume, Some(parent))?;
            snapshot.authorize_retained_read(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                parent,
            )?;
        }
        if target.create {
            self.authorize_file_creation(access, &policy, retained)
        } else {
            self.check_authority(access, target.volume, Some(target.entry))?;
            snapshot.authorize_staged_write(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                target.entry,
            )
        }
    }

    /// Publish complete private bytes. The caller consumes the destination and
    /// never retries an uncertain commit. Immutable content is prepared outside
    /// the operation gate, then current identity and authority are rechecked.
    pub fn publish_file(
        &self,
        access: &Access,
        target: &FileDestination,
        source: &mut impl Read,
        sync: bool,
    ) -> std::result::Result<(), FilePublicationFailure> {
        self.publish_file_with_session(access, target, source, sync, None)
    }

    fn publish_file_with_session(
        &self,
        access: &Access,
        target: &FileDestination,
        source: &mut impl Read,
        sync: bool,
        retained: Option<&Session>,
    ) -> std::result::Result<(), FilePublicationFailure> {
        let mut publication_unknown = false;
        let result = (|| {
            {
                let operations = self.access_operations(access, target.volume)?;
                let _guards = operations.lock()?;
                self.validate_file_destination_with_session(access, target, retained)?;
            }
            let _preparation = entry_blobs::Preparation::enter(&self.entry_blobs)?;
            let revision = EntryId::generate()?;
            let mut kind =
                self.prepare_file_blob(target.volume, target.entry, revision, source, sync)?;
            self.before_entry_blob_publication()?;
            let operations = self.access_operations(access, target.volume)?;
            let _guards = operations.lock()?;
            self.validate_file_destination_with_session(access, target, retained)?;
            let db = self.db(target.volume, crate::store::DatabaseId::Index)?;
            let mut tx =
                db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
            let predecessor = self.file_namespace_cache.predecessor(&db, &tx);
            entry_blobs::stamp_publication(&mut kind)?;
            let origin =
                volume_index::namespace::Origin { installation: self.installation(), label: None };
            let projection = if target.create {
                let info = target.birth_info(revision, kind.clone(), origin.clone());
                if target.exclusive {
                    volume_index::create(&mut tx, target.entry, &info)?;
                } else {
                    volume_index::create_competing_blob(&mut tx, target.entry, &info)?;
                }
                None
            } else {
                Some(volume_index::update_file_content(
                    &mut tx,
                    target.entry,
                    kind.clone(),
                    revision,
                    &origin,
                    Some(predecessor),
                )?)
            };
            volume_index::mark_published_blob_ready(&mut tx, target.entry, &kind)?;
            let clock = tx.clock();
            publication_unknown = true;
            let delta = tx.commit_with_delta()?;
            self.changes.publish(target.volume, crate::store::DatabaseId::Index, delta);
            self.publish_file_move(target, &db, &origin)?;
            if target.provider_move.is_none()
                && let Some(projection) = projection
                && let Err(error) =
                    self.file_namespace_cache.retain_committed(&db, clock, projection)
            {
                // Publication succeeded. An optional cache failure cannot turn
                // that result into an unpublished or uncertain File close.
                eprintln!("Could not retain committed File namespace projection: {error}");
            }
            Ok(())
        })();
        result.map_err(|error| FilePublicationFailure { error, publication_unknown })
    }

    fn publish_file_move(
        &self,
        target: &FileDestination,
        db: &Database,
        origin: &volume_index::namespace::Origin,
    ) -> Result<()> {
        if let Some((parent, name)) = &target.provider_move {
            // Namespace facts require strictly later transaction clocks.
            // The caller holds the volume gate through both publications and
            // reports an uncertain partial action if this second commit fails.
            let mut tx =
                db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
            volume_index::move_entry(
                &mut tx,
                target.entry,
                *parent,
                Some(name),
                EntryId::generate()?,
                origin,
            )?;
            self.changes.publish(
                target.volume,
                crate::store::DatabaseId::Index,
                tx.commit_with_delta()?,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

impl FileDestination {
    /// The provider retains a deterministic identity for uncertain create retries.
    pub fn set_creation_id(&mut self, id: EntryId) -> Result<()> {
        if !self.create {
            return Err(Error::AlreadyExists("provider creation destination"));
        }
        self.entry = id;
        self.provider = true;
        Ok(())
    }

    fn birth_info(
        &self,
        revision: EntryId,
        kind: EntryKind,
        origin: volume_index::namespace::Origin,
    ) -> EntryInfo {
        EntryInfo {
            namespace: volume_index::namespace::Birth {
                parent: self.parent,
                // Supply the captured observations for pre-insertion validation.
                // Insertion replaces them with the current publication context.
                observed: self
                    .creation_context
                    .as_ref()
                    .map_or_else(Vec::new, |context| context.observed.clone()),
                creation_context: self.creation_context.clone(),
                ..Default::default()
            },
            origin,
            initial_permissions: EntryAcl::Inherit,
            created_revision: revision,
            name: self.name.clone(),
            kind,
            protocol: None,
        }
    }
}
