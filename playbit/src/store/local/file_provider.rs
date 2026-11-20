//! Provider metadata uses one authorized causal cut, without hydrating contents.
use super::entries::granted;
use super::*;
use crate::store::{
    EntryId,
    file_provider::*,
    object_contract::Principal,
    volume_index::{EntryKind, IndexSnapshot},
};

#[cfg(feature = "file-provider")]
#[derive(Serialize)]
pub(crate) struct ProviderAnchor {
    namespace: [ReplicaRoot; 2],
    uploads: u64,
}

impl LocalStore {
    pub fn provider_account(&self) -> Result<Id> {
        let _authority = self.lock_account_authority()?;
        Ok(self.account.id)
    }

    /// Root/catalog access has no selected volume, but must retain the same
    /// account-authority fence as admitted namespace operations.
    pub fn provider_volumes(&self) -> Result<Vec<Volume>> {
        let _authority = self.lock_account_authority()?;
        let volumes = self.personal_volumes()?;
        if volumes.len() > PROVIDER_VOLUME_LIMIT {
            return Err(Error::LimitExceeded);
        }
        Ok(volumes)
    }

    pub fn provider_root_item(&self) -> Result<ProviderNamespaceItem> {
        let _authority = self.lock_account_authority()?;
        let version = *blake3::hash(self.account.id.as_bytes()).as_bytes();
        Ok(ProviderNamespaceItem {
            id: PROVIDER_ROOT_ID.into(),
            parent: PROVIDER_ROOT_ID.into(),
            role: "root".into(),
            filename: "Playbit".into(),
            kind: EntryType::Directory,
            len_bytes: 0,
            readable: true,
            writable: true,
            uploaded: true,
            unavailable: false,
            content_version: version,
            metadata_version: version,
        })
    }

    pub fn provider_volume_items(&self) -> Result<Vec<ProviderNamespaceItem>> {
        self.provider_catalog().map(|(_, items)| items)
    }

    pub fn provider_catalog(&self) -> Result<(Vec<Volume>, Vec<ProviderNamespaceItem>)> {
        let _authority = self.lock_account_authority()?;
        let volumes = self.personal_volumes()?;
        let names = volume_filenames(&volumes)?;
        let items = volumes
            .iter()
            .map(|volume| {
                let status = self
                    .db(volume.id, crate::store::DatabaseId::Index)
                    .and_then(|db| {
                        Ok(ProviderVolumeStatus {
                            available: replica_root(&db)?.is_some(),
                            uploaded: self.provider_volume_uploaded(volume)?,
                        })
                    })
                    .unwrap_or_default();
                volume_namespace_item(volume, names[&volume.id].clone(), status)
            })
            .collect();
        Ok((volumes, items))
    }

    /// Synthetic folders require only discovered account metadata. An index
    /// awaiting initialization must not hide the folder or block its siblings.
    pub fn provider_volume_item(&self, volume: Id) -> Result<ProviderNamespaceItem> {
        self.provider_volume_items()?
            .into_iter()
            .find(|item| item.id == volume.to_string())
            .ok_or(Error::AccessDenied("File Provider personal volume"))
    }

    pub fn provider_rename_volume(&self, volume: Id, name: &str, metadata: &[u8]) -> Result<()> {
        // Finder supplies a displayed component. An unchanged generated name
        // is a no-op; recognized identity suffixes permit reversing its escapes.
        if name.contains('/') || name.contains('\0') {
            return Err(Error::InvalidInput("invalid provider volume filename"));
        }
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        self.provider_volume(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let mut current: Volume = {
            let snapshot = db.begin_read()?;
            record::decode(&snapshot.get(&VOLUME_KEY)?.ok_or(Error::NotFound("volume policy"))?)?
        };
        if current.owner != self.account.id || !current.personal {
            return Err(Error::AccessDenied("File Provider personal volume"));
        }
        let mut volumes = self.personal_volumes()?;
        let descriptor = volumes
            .iter_mut()
            .find(|item| item.id == volume)
            .ok_or(Error::AccessDenied("File Provider personal volume"))?;
        *descriptor = current.clone();
        let names = volume_filenames(&volumes)?;
        let existing = volume_namespace_item(
            &current,
            names[&volume].clone(),
            ProviderVolumeStatus { available: true, uploaded: false },
        );
        if existing.metadata_version != metadata {
            return Err(Error::AlreadyExists("provider item version changed"));
        }
        // An unchanged displayed name includes any generated collision suffix.
        // Preserve the underlying label rather than writing that suffix into it.
        if existing.filename == name || current.name == name {
            return Ok(());
        }
        let name = volume_label_from_filename(volume, name)?;
        validate_volume_label(&name)?;
        self.prepare_volume_name(volume)?;
        current.name = name;
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let bytes = record::rewrite(
            &tx.get(&VOLUME_KEY)?.ok_or(Error::NotFound("volume policy"))?,
            &current,
        )?;
        tx.put(&VOLUME_KEY, &bytes)?;
        self.changes.publish(volume, crate::store::DatabaseId::Index, tx.commit_with_delta()?);
        self.project_volume_name(volume)
    }

    /// Stable across process restarts. Only namespace and authority changes
    /// affect projected items; database backing edits do not change references.
    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_anchor(&self, volume: Id) -> Result<ProviderAnchor> {
        self.provider_volume(volume)?;
        self.provider_cached_anchor(volume)?.ok_or_else(|| {
            Error::System("File Provider location is waiting for initial synchronization".into())
        })
    }

    /// A notification stamp, not a working-set snapshot. Components remain
    /// independently captured; failed or uninitialized components have an
    /// explicit pending tag so they can never stand for an empty namespace.
    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_account_anchor(&self) -> Result<[u8; 32]> {
        let mut volumes = self.provider_volumes()?;
        volumes.sort_unstable_by_key(|volume| volume.id);
        let mut hash = blake3::Hasher::new();
        hash.update(self.account.id.as_bytes());
        for volume in volumes {
            hash.update(volume.id.as_bytes());
            hash.update(&(volume.name.len() as u64).to_le_bytes());
            hash.update(volume.name.as_bytes());
            match self.provider_cached_anchor(volume.id) {
                Ok(Some(anchor)) => {
                    hash.update(&[1]);
                    hash.update(
                        &serde_json::to_vec(&anchor)
                            .map_err(|_| Error::Internal("could not encode provider anchor"))?,
                    );
                }
                Ok(None) | Err(_) => {
                    self.require_account_authority()?;
                    hash.update(&[0]);
                }
            }
        }
        self.require_account_authority()?;
        Ok(*hash.finalize().as_bytes())
    }

    /// A fresh provider profile may not have discovered/materialized its drive.
    /// Missing state is explicit; contention, corruption and authority failures
    /// must propagate rather than advancing a change fence without its anchor.
    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_cached_anchor(&self, volume: Id) -> Result<Option<ProviderAnchor>> {
        if !self.personal_volumes()?.iter().any(|v| v.id == volume) {
            return Ok(None);
        }
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let index = self.db(volume, crate::store::DatabaseId::Index)?;
        let root = replica_root(&index)?;
        if root.is_none() {
            return Ok(None);
        }
        self.provider_volume(volume)?;
        self.authorize(&access, &self.index(volume)?, Permission::Read)?;
        let revocations = self.db(volume, crate::store::DatabaseId::Revocations)?;
        Ok(Some(ProviderAnchor {
            namespace: [root, replica_root(&revocations)?],
            uploads: self.provider_upload_revision(volume)?,
        }))
    }

    #[cfg(any(test, feature = "file-provider"))]
    pub(crate) fn provider_retired_volumes(&self, known: &[Id]) -> Result<Vec<Id>> {
        let _authority = self.lock_account_authority()?;
        // Deletion is immutable. Check bounded retained location identities:
        // a recovered account may have a tombstone without the old descriptor.
        // Absence from a partial recovery catalog alone never revokes a domain.
        if known.len() > PROVIDER_VOLUME_LIMIT {
            return Err(Error::InvalidInput("File Provider known location limit exceeded"));
        }
        let mut retired = Vec::new();
        for volume in known {
            if self.volume_deleted(*volume)? {
                retired.push(*volume);
            }
        }
        Ok(retired)
    }

    pub fn provider_item(&self, volume: Id, entry: EntryId) -> Result<ProviderItem> {
        self.provider_volume(volume)?;
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.provider_item_locked(volume, &access, &policy, &snapshot, entry)
    }

    pub(super) fn provider_item_locked(
        &self,
        volume: Id,
        access: &Access,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        id: EntryId,
    ) -> Result<ProviderItem> {
        self.authorize(access, policy, Permission::Read)?;
        snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            Principal::Human(access.account),
            id,
            Permission::Read,
        )?;
        let parent = snapshot.namespace_entry(id)?.parent;
        let mut children = snapshot.children(parent)?;
        children.retain(|entry| !entry.deleted);
        let summary = children
            .iter()
            .find(|entry| entry.id == id && !entry.deleted)
            .ok_or(Error::NotFound("provider item"))?;
        let names = filenames(&children)?;
        let writable = granted(snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            Principal::Human(access.account),
            id,
            Permission::Write,
        ))?;
        let entry = snapshot.lookup_from(parent, &summary.name)?;
        Ok(provider_item(
            volume,
            summary,
            &entry,
            names[&id].clone(),
            writable,
            self.provider_item_uploaded(volume, snapshot, policy, &entry)?,
        ))
    }

    pub fn provider_scan(&self, volume: Id, after: Option<EntryId>) -> Result<ProviderPage> {
        self.provider_volume(volume)?;
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize(&access, &policy, Permission::Read)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        let page = snapshot.page(after, crate::store::volume_index::MAX_ENTRY_PAGE)?;
        let mut names = BTreeMap::new();
        let mut items = Vec::with_capacity(page.entries.len());
        for entry in page.entries {
            if entry.deleted {
                continue;
            }
            if !granted(snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                Principal::Human(access.account),
                entry.id,
                Permission::Read,
            ))? {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(slot) = names.entry(entry.parent) {
                let mut children = snapshot.children(entry.parent)?;
                children.retain(|entry| !entry.deleted);
                slot.insert(filenames(&children)?);
            }
            let writable = granted(snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                Principal::Human(access.account),
                entry.id,
                Permission::Write,
            ))?;
            let current = snapshot.lookup_from(entry.parent, &entry.name)?;
            items.push(provider_item(
                volume,
                &entry,
                &current,
                names[&entry.parent][&entry.id].clone(),
                writable,
                self.provider_item_uploaded(volume, &snapshot, &policy, &current)?,
            ));
        }
        Ok(ProviderPage { items, next: page.next })
    }

    /// Filesystem metadata mutations compare the admitted version while holding
    /// the namespace gate, including against edits made in the Playbit app.
    pub fn provider_change(
        &self,
        volume: Id,
        id: EntryId,
        content: &[u8],
        metadata: &[u8],
        change: ProviderChange<'_>,
    ) -> Result<()> {
        use crate::store::volume_index;
        self.provider_volume(volume)?;
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        self.provider_volume(volume)?;
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        let current = self.provider_item_locked(volume, &access, &policy, &snapshot, id)?;
        if current.content_version != content || current.metadata_version != metadata {
            return Err(Error::AlreadyExists("provider item version changed"));
        }
        snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            Principal::Human(access.account),
            id,
            Permission::Write,
        )?;
        if let ProviderChange::Move { parent, name } = change {
            volume_index::validate_name(name)?;
            if name.contains('/') {
                return Err(Error::InvalidInput("provider filename contains slash"));
            }
            self.file_directory_snapshot(&access, &policy, volume, parent, &snapshot)?;
            if let Some(parent) = parent {
                snapshot.authorize_namespace(
                    policy.volume(),
                    policy.default_permissions(),
                    Principal::Human(access.account),
                    parent,
                    Permission::Write,
                )?;
            }
        }
        if let ProviderChange::Delete { recursive } = change
            && current.kind == crate::store::EntryType::Directory
        {
            self.authorize_provider_deletion(&access, &policy, &snapshot, id, recursive)?;
        }
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let revision = EntryId::generate()?;
        let origin =
            volume_index::namespace::Origin { installation: self.installation(), label: None };
        match change {
            ProviderChange::Move { parent, name } => {
                volume_index::move_entry(&mut tx, id, parent, Some(name), revision, &origin)?
            }
            ProviderChange::Delete { .. } => {
                volume_index::retire_namespace(&mut tx, id, revision, &origin)?
            }
        }
        self.changes.publish(volume, crate::store::DatabaseId::Index, tx.commit_with_delta()?);
        Ok(())
    }

    fn authorize_provider_deletion(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        id: EntryId,
        recursive: bool,
    ) -> Result<()> {
        let mut directories = vec![id];
        let mut visited = BTreeSet::new();
        while let Some(directory) = directories.pop() {
            if !visited.insert(directory)
                || visited.len() > crate::store::volume_index::namespace::MAX_ENTRIES
            {
                return Err(Error::LimitExceeded);
            }
            for child in snapshot.children(Some(directory))?.into_iter().filter(|c| !c.deleted) {
                if !recursive {
                    return Err(Error::InvalidInput("provider directory not empty"));
                }
                if !granted(snapshot.authorize_namespace(
                    policy.volume(),
                    policy.default_permissions(),
                    Principal::Human(access.account),
                    child.id,
                    Permission::Write,
                ))? {
                    return Err(Error::AccessDenied(
                        "provider directory not empty: protected child",
                    ));
                }
                if child.kind == crate::store::EntryType::Directory {
                    directories.push(child.id);
                }
            }
        }
        Ok(())
    }

    pub fn provider_create_directory(
        &self,
        volume: Id,
        id: EntryId,
        parent: Option<EntryId>,
        name: &str,
    ) -> Result<()> {
        use crate::store::volume_index::{self, EntryInfo};
        self.provider_volume(volume)?;
        volume_index::validate_name(name)?;
        if name.contains('/') {
            return Err(Error::InvalidInput("provider filename contains slash"));
        }
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.authorize_entry_creation(&access, &policy)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.file_directory_snapshot(&access, &policy, volume, parent, &snapshot)?;
        if let Some(parent) = parent {
            snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                Principal::Human(access.account),
                parent,
                Permission::Write,
            )?;
        }
        let info = EntryInfo {
            namespace: volume_index::namespace::Birth { parent, ..Default::default() },
            origin: volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::generate()?,
            name: name.to_owned(),
            kind: EntryKind::Directory {},
            protocol: None,
        };
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        volume_index::create(&mut tx, id, &info)?;
        self.changes.publish(volume, crate::store::DatabaseId::Index, tx.commit_with_delta()?);
        Ok(())
    }

    /// Locate the effective parent after moves, rather than the birth's parent.
    pub fn provider_parent(&self, volume: Id, entry: EntryId) -> Result<Option<EntryId>> {
        self.provider_volume(volume)?;
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        self.provider_volume(volume)?;
        let policy = self.index(volume)?;
        self.authorize(&access, &policy, Permission::Read)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        snapshot.authorize_namespace(
            policy.volume(),
            policy.default_permissions(),
            Principal::Human(access.account),
            entry,
            Permission::Read,
        )?;
        Ok(snapshot.namespace_entry(entry)?.parent)
    }
    /// Enumerate one directory with stable identity cursors. A provider domain
    /// is local account authority over an eligible personal volume only.
    pub fn provider_entries(
        &self,
        volume: Id,
        parent: Option<EntryId>,
        after: Option<EntryId>,
        limit: usize,
    ) -> Result<ProviderPage> {
        if limit == 0 || limit > PROVIDER_PAGE_ITEMS {
            return Err(Error::InvalidInput("invalid provider page size"));
        }
        self.provider_volume(volume)?;
        let access = Access::owner(self.account.id);
        let operations = self.access_operations(&access, volume)?;
        let _guards = operations.lock()?;
        self.provider_volume(volume)?;
        let policy = self.index(volume)?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&db, &self.file_namespace_cache)?;
        self.file_directory_snapshot(&access, &policy, volume, parent, &snapshot)?;
        let mut children = snapshot.children(parent)?;
        children.retain(|entry| !entry.deleted);
        let names = filenames(&children)?;
        children.sort_unstable_by_key(|entry| entry.id);
        let mut items = Vec::with_capacity(limit);
        let permits = |entry, permission| {
            granted(snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                Principal::Human(access.account),
                entry,
                permission,
            ))
        };
        for summary in children.iter().filter(|entry| after.is_none_or(|after| entry.id > after)) {
            if !permits(summary.id, Permission::Read)? {
                continue;
            }
            if items.len() == limit {
                let next = items.last().map(|item: &ProviderItem| item.id);
                return Ok(ProviderPage { items, next });
            }
            let entry = snapshot.lookup_from(parent, &summary.name)?;
            let writable = permits(summary.id, Permission::Write)?;
            items.push(provider_item(
                volume,
                summary,
                &entry,
                names[&summary.id].clone(),
                writable,
                self.provider_item_uploaded(volume, &snapshot, &policy, &entry)?,
            ));
        }
        Ok(ProviderPage { items, next: None })
    }

    pub fn provider_volume(&self, volume: Id) -> Result<Volume> {
        self.require_account_authority()?;
        let descriptor = self
            .personal_volumes()?
            .into_iter()
            .find(|v| v.id == volume)
            .ok_or(Error::AccessDenied("File Provider personal volume"))?;
        // Callers also recheck this while holding the namespace mutation gate.
        // Inspect the database directly; with_replica would acquire that gate
        // again and deadlock a local Files mutation.
        let database = self.db(volume, crate::store::DatabaseId::Index)?;
        if replica_root(&database)?.is_none() {
            return Err(Error::System(
                "File Provider location is waiting for initial synchronization".into(),
            ));
        }
        Ok(descriptor)
    }
}

fn provider_item(
    volume: Id,
    summary: &crate::store::volume_index::EntrySummary,
    entry: &crate::store::volume_index::VolumeEntry,
    filename: String,
    writable: bool,
    uploaded: bool,
) -> ProviderItem {
    let mut content = blake3::Hasher::new();
    let len_bytes = match &entry.info.kind {
        EntryKind::Blob { len_bytes, digest, .. } => {
            content.update(entry.content_revision.as_bytes());
            content.update(digest);
            *len_bytes
        }
        EntryKind::Directory {} => 0,
        EntryKind::KvDb {} | EntryKind::ObjectDb { .. } => {
            let reference = crate::store::DatabaseReference { volume, entry: entry.id }.to_string();
            content.update(reference.as_bytes());
            reference.len() as u64
        }
    };
    let mut metadata = blake3::Hasher::new();
    metadata.update(filename.as_bytes());
    metadata.update(entry.name_revision.as_bytes());
    metadata.update(&[writable as u8, summary.kind as u8]);
    metadata.update(&len_bytes.to_le_bytes());
    if let Some(parent) = summary.parent {
        metadata.update(parent.as_bytes());
    }
    ProviderItem {
        id: entry.id,
        parent: summary.parent,
        filename,
        kind: summary.kind,
        len_bytes,
        readable: true,
        writable,
        uploaded,
        content_version: *content.finalize().as_bytes(),
        metadata_version: *metadata.finalize().as_bytes(),
    }
}

#[cfg(test)]
#[path = "file_provider_tests.rs"]
mod tests;
