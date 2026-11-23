//! Named entry publication shares the volume operation boundary with replication.
use super::*;
use crate::store::{
    EntryId,
    volume_index::{
        self, DatabaseProtocol, DirectoryListing, DirectoryTarget, EntryDetails, EntryInfo,
        EntryKind, EntryPage, IndexSnapshot, VolumeEntry,
    },
};

impl LocalStore {
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn admit_catalog_read(&self, access: &Access, volume: Id) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.authorize(access, &self.index(volume)?, Permission::Read)
    }

    pub fn resolve_entry(&self, access: &Access, volume: Id, name: &str) -> Result<VolumeEntry> {
        let _timing = crate::trace::Span::new("storage.entry_resolve");
        volume_index::validate_name(name)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize(access, &policy, Permission::Read)?;
        let database = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(database.begin_read()?);
        let entry = snapshot.lookup(name)?;
        snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry.id,
            Permission::Read,
        )?;
        Ok(entry)
    }
    pub(super) fn entry_backing(
        &self,
        volume: Id,
        entry: EntryId,
        create: bool,
    ) -> Result<Arc<Database>> {
        self.replica_db(
            volume,
            crate::store::DatabaseId::Entry(entry),
            create.then(|| self.credentials(volume).map(|value| value.database_key)).transpose()?,
        )
    }

    pub fn entries(
        &self,
        access: &Access,
        volume: Id,
        after: Option<EntryId>,
        limit: usize,
    ) -> Result<EntryPage> {
        let _timing = crate::trace::Span::new("storage.entry_page");
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize(access, &policy, Permission::Read)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(db.begin_read()?);
        // Namespace projection is pinned with the bounded summary page. No
        // additive schema is materialized; current permissions and consent
        // remain locked through capture.
        snapshot.page(after, limit)
    }

    /// Catalog Read exposes management metadata even when an entry's content
    /// override excludes the reader. Current consent and Read remain locked
    /// through the single-cut directory capture; no content authority is added.
    #[allow(
        clippy::too_many_arguments,
        reason = "The bounded catalog query keeps authority and cut explicit."
    )]
    pub fn directory_entries(
        &self,
        access: &Access,
        volume: Id,
        target: DirectoryTarget,
        cut: Option<u64>,
        after: Option<EntryId>,
        limit: usize,
        compare: fn(&str, &str) -> std::cmp::Ordering,
    ) -> Result<DirectoryListing> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize(access, &policy, Permission::Read)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        IndexSnapshot::cached(&db, &self.file_namespace_cache)?
            .directory_entries(target, cut, after, limit, compare)
    }

    pub fn entry(&self, access: &Access, volume: Id, entry: EntryId) -> Result<VolumeEntry> {
        let _timing = crate::trace::Span::new("storage.entry_read");
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.check_authority(access, volume, Some(entry))?;
        self.admit_access(access, &policy)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(db.begin_read()?);
        snapshot.authorize(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry,
            Permission::Read,
        )?;
        snapshot.live_entry(entry)
    }

    pub fn entry_details(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
    ) -> Result<EntryDetails> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        let current = match self.check_authority(access, volume, Some(entry)) {
            Ok(()) => true,
            Err(Error::AccessDenied(_)) if access.account == policy.volume().owner => false,
            Err(error) => return Err(error),
        };
        let personal_grant = self.admit_access(access, &policy)?;
        let can_mutate = personal_grant.as_ref().is_none_or(|grant| grant.write);
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(db.begin_read()?);
        let mut record = snapshot.entry(entry)?;
        let directly_retired = record.deleted;
        let catalog_directory = matches!(record.info.kind, EntryKind::Directory {})
            && granted(self.authorize(access, &policy, Permission::Read))?;
        if catalog_directory {
            snapshot.project_directory_details(&mut record)?;
        }
        let volume = policy.volume();
        let defaults = policy.default_permissions();
        let principal = self.data_actor(access, volume)?;
        let permits = |permission| {
            if !current {
                return Ok(false);
            }
            let admitted = if catalog_directory {
                snapshot.authorize_namespace(volume, defaults, principal, entry, permission)
            } else {
                snapshot.authorize(volume, defaults, principal, entry, permission)
            };
            entry_details_permitted(admitted)
        };
        let acl = snapshot.permissions(entry)?;
        let manage_permissions = personal_grant.is_none()
            && (access.account == volume.owner || permits(Permission::ManagePermissions)?);
        let retained_readable = current
            && granted(snapshot.authorize_retained_read(volume, defaults, principal, entry))?;
        let backing_ready = snapshot.backing_ready(entry)?;
        let schema_pending = snapshot.schema_pending(entry)?;
        let recoverable = (directly_retired && record.deleted && retained_readable && can_mutate)
            && (matches!(record.info.kind, EntryKind::Blob { .. })
                || (backing_ready && !schema_pending))
            && granted(self.authorize_entry_creation(access, &policy))?
            && (matches!(acl, EntryAcl::Inherit) || access.account == volume.owner);
        let retained_permissions =
            self.configured_permissions(access, volume.id, entry, &acl, manage_permissions)?;
        let details = EntryDetails {
            entry: record,
            retained_permissions,
            permissions: acl,
            created_clock: snapshot.created_clock(entry)?,
            default_permissions: policy.default_permissions().clone(),
            backing_ready,
            schema_pending,
            readable: permits(Permission::Read)?,
            retained_readable,
            recoverable,
            writable: can_mutate && permits(Permission::Write)?,
            appendable: can_mutate && permits(Permission::Append)?,
            manage_permissions,
        };
        self.admit_entry_details(access, &policy, details)
    }

    fn admit_entry_details(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        details: EntryDetails,
    ) -> Result<EntryDetails> {
        if access.account != policy.volume().owner
            && !details.retained_readable
            && !(details.writable || details.appendable)
            && !granted(self.authorize(access, policy, Permission::Read))?
        {
            return Err(Error::AccessDenied("entry access or catalog Read required"));
        }
        Ok(details)
    }

    pub fn create_database(
        &self,
        access: &Access,
        volume: Id,
        name: &str,
        kind: EntryKind,
    ) -> Result<VolumeEntry> {
        self.create_database_inner(access, volume, None, name, kind, DatabaseProtocol::Kvdb)
    }

    /// Create one component under the retained directory identity.
    pub fn create_database_in(
        &self,
        access: &Access,
        volume: Id,
        parent: EntryId,
        name: &str,
        kind: EntryKind,
    ) -> Result<VolumeEntry> {
        self.create_database_inner(access, volume, Some(parent), name, kind, DatabaseProtocol::Kvdb)
    }

    pub fn create_bloblog(&self, access: &Access, volume: Id, name: &str) -> Result<VolumeEntry> {
        self.create_database_inner(
            access,
            volume,
            None,
            name,
            EntryKind::KvDb {},
            DatabaseProtocol::BlobLog,
        )
    }

    pub fn create_bloblog_in(
        &self,
        access: &Access,
        volume: Id,
        parent: EntryId,
        name: &str,
    ) -> Result<VolumeEntry> {
        self.create_database_inner(
            access,
            volume,
            Some(parent),
            name,
            EntryKind::KvDb {},
            DatabaseProtocol::BlobLog,
        )
    }

    fn create_database_inner(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
        name: &str,
        kind: EntryKind,
        protocol: DatabaseProtocol,
    ) -> Result<VolumeEntry> {
        if parent.is_some() {
            volume_index::namespace::component(name)?;
        } else {
            volume_index::validate_name(name)?;
        }
        if matches!(kind, EntryKind::Blob { .. }) {
            return Err(Error::InvalidInput("blob entries require prepared immutable content"));
        }
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize_entry_creation(access, &policy)?;
        let index = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&index, &self.file_namespace_cache)?;
        if parent.is_some() {
            self.file_directory_snapshot(access, &policy, volume, parent, &snapshot)?;
        }
        match snapshot.lookup_from(parent, name) {
            Ok(_) => return Err(Error::AlreadyExists("entry name")),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        drop(snapshot);
        let id = EntryId::generate()?;
        self.check_entry_backing_absent(volume, id)?;
        let info = EntryInfo {
            namespace: volume_index::namespace::Birth { parent, ..Default::default() },
            origin: crate::store::volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
            created_revision: EntryId::generate()?,
            name: name.rsplit('/').next().expect("validated path").to_owned(),
            kind,
            protocol: Some(protocol),
        };
        info.validate()?;
        let backing = self.entry_backing(volume, id, true)?;
        backing.begin_read()?;
        super::initialization::sync_directory(
            &self.path.join(crate::paths::VOLUMES).join(volume.to_string()),
        )?;
        let mut tx =
            index.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        volume_index::create_at_path(&mut tx, id, name, &info)?;
        volume_index::mark_backing_ready(&mut tx, id)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        IndexSnapshot::new(index.begin_read()?).live_entry(id)
    }

    fn check_entry_backing_absent(&self, volume: Id, id: EntryId) -> Result<()> {
        let path = self
            .path
            .join(crate::paths::VOLUMES)
            .join(volume.to_string())
            .join(crate::store::DatabaseId::Entry(id).file_name());
        if path.try_exists().map_err(io)? {
            return Err(Error::AlreadyExists("entry backing identity"));
        }
        Ok(())
    }

    pub(super) fn authorize_entry_creation(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
    ) -> Result<()> {
        match self.authorize(access, policy, Permission::Write) {
            Ok(()) => {}
            Err(Error::AccessDenied(_)) => self.authorize(access, policy, Permission::Append)?,
            Err(error) => return Err(error),
        }
        Ok(())
    }

    pub fn rename_entry(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        name: &str,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.entry_rename");
        volume_index::validate_name(name)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.check_authority(access, volume, Some(entry))?;
        if let Some(grant) = self.admit_access(access, &policy)?
            && !grant.write
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let catalog = match self.authorize(access, &policy, Permission::Read) {
            Ok(()) => true,
            Err(Error::AccessDenied(_)) if !name.contains('/') => false,
            Err(error) => return Err(error),
        };
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        {
            let snapshot = IndexSnapshot::new(db.begin_read()?);
            let authorize =
                if catalog { IndexSnapshot::authorize_namespace } else { IndexSnapshot::authorize };
            authorize(
                &snapshot,
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                entry,
                Permission::Write,
            )?;
        }
        let (parent, component) = match name.rsplit_once('/') {
            Some((path, component)) => {
                (Some(self.rename_parent_locked(access, volume, &policy, path)?), component)
            }
            None => (None, name),
        };
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let revision = EntryId::generate()?;
        let origin =
            volume_index::namespace::Origin { installation: self.installation(), label: None };
        if catalog {
            volume_index::move_entry(&mut tx, entry, parent, Some(component), revision, &origin)?;
        } else {
            volume_index::assign_entry_root(&mut tx, entry, component, revision, &origin)?;
        }
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        Ok(())
    }

    /// Change one visible component, retaining the current effective parent.
    /// Path-based callers continue to use rename_entry for explicit moves.
    pub fn rename_entry_component(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        name: &str,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.entry_rename");
        volume_index::namespace::component(name)?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.check_authority(access, volume, Some(entry))?;
        if self.admit_access(access, &policy)?.is_some_and(|grant| !grant.write) {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        // Resolving the current parent and checking siblings requires the
        // catalog cut. This never falls back to a root assignment without Read.
        self.authorize(access, &policy, Permission::Read)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        IndexSnapshot::cached(&db, &self.file_namespace_cache)?.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry,
            Permission::Write,
        )?;
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let origin =
            volume_index::namespace::Origin { installation: self.installation(), label: None };
        volume_index::rename(&mut tx, entry, name, EntryId::generate()?, &origin)?;
        self.changes.publish(volume, crate::store::DatabaseId::Index, tx.commit_with_delta()?);
        Ok(())
    }

    /// The caller holds the volume operation gate and has authorized the source
    /// entry. Creating destination directories separately gives the subsequent
    /// move a strictly later causal clock; a failed move may leave empty parents.
    fn rename_parent_locked(
        &self,
        access: &Access,
        volume: Id,
        policy: &MetadataPolicy,
        path: &str,
    ) -> Result<EntryId> {
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        match IndexSnapshot::new(db.begin_read()?).lookup(path) {
            Ok(entry) if matches!(entry.info.kind, EntryKind::Directory {}) => return Ok(entry.id),
            Ok(_) => return Err(Error::InvalidInput("destination parent is not a directory")),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        self.authorize_entry_creation(access, policy)?;
        let id = EntryId::generate()?;
        let info = EntryInfo {
            namespace: volume_index::namespace::Birth { automatic: true, ..Default::default() },
            origin: volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
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

    pub fn remove_entry(&self, access: &Access, volume: Id, entry: EntryId) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.check_authority(access, volume, Some(entry))?;
        if let Some(grant) = self.admit_access(access, &policy)?
            && !grant.write
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let catalog = match self.authorize(access, &policy, Permission::Read) {
            Ok(()) => true,
            Err(Error::AccessDenied(_)) => false,
            Err(error) => return Err(error),
        };
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(db.begin_read()?);
        let authorize =
            if catalog { IndexSnapshot::authorize_namespace } else { IndexSnapshot::authorize };
        authorize(
            &snapshot,
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry,
            Permission::Write,
        )?;
        drop(snapshot);
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let retire = if catalog { volume_index::retire_namespace } else { volume_index::retire };
        retire(
            &mut tx,
            entry,
            EntryId::generate()?,
            &volume_index::namespace::Origin { installation: self.installation(), label: None },
        )?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        Ok(())
    }

    pub fn set_entry_permissions(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        permissions: EntryAcl,
    ) -> Result<()> {
        self.set_entry_permissions_with_retained(access, volume, entry, permissions, None)
    }

    /// Remember disabled grants on this device before changing active permissions.
    pub fn set_entry_permissions_with_retained(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        permissions: EntryAcl,
        retained_permissions: Option<Acl>,
    ) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        if self.admit_access(access, &policy)?.is_some() {
            return Err(Error::AccessDenied("session cannot manage personal volume permissions"));
        }
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let previous = {
            let snapshot = IndexSnapshot::new(db.begin_read()?);
            let previous = snapshot.permissions_for_edit(entry)?;
            if access.account != policy.volume().owner {
                self.check_authority(access, volume, Some(entry))?;
                snapshot.authorize(
                    policy.volume(),
                    policy.default_permissions(),
                    self.data_actor(access, policy.volume())?,
                    entry,
                    Permission::ManagePermissions,
                )?;
            }
            previous
        };
        match &permissions {
            EntryAcl::Replace(acl) => {
                acl.validate()?;
                if retained_permissions.as_ref().is_some_and(|retained| retained != acl) {
                    return Err(Error::InvalidInput(
                        "retained entry ACL contradicts active grants",
                    ));
                }
            }
            EntryAcl::Inherit => {
                let retained = retained_permissions.as_ref().or(match &previous {
                    EntryAcl::Replace(acl) => Some(acl),
                    EntryAcl::Inherit => None,
                });
                if let Some(acl) = retained {
                    self.remember_entry_permissions(access.account, volume, entry, acl)?;
                }
            }
        }
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        volume_index::set_permissions(&mut tx, entry, &permissions)?;
        volume_index::retain_permission_publication(&mut tx, entry, access.account)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(volume, crate::store::DatabaseId::Index, delta);
        Ok(())
    }
}

fn entry_details_permitted(result: Result<()>) -> Result<bool> {
    match result {
        Err(Error::NotFound(_)) => Ok(false),
        result => granted(result),
    }
}

pub(super) fn granted(result: Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(Error::AccessDenied(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod rename_tests;
