//! Live copies use fresh identities and destination ACLs, never recovery rules.
//! One bounded manifest retains names and metadata digests, not database schemas.
use super::*;
use crate::store::volume_index::namespace;

#[cfg(test)]
mod tests;

struct Item {
    id: EntryId,
    canonical: EntryId,
    parent: Option<EntryId>,
    name: String,
    directory: bool,
    children: Vec<EntryId>,
    fingerprint: [u8; 32],
}

impl LocalStore {
    /// Background-only. Copies the current visible subtree under its effective
    /// parent. A later failure retains and explicitly identifies a partial copy.
    pub fn duplicate_entry(&self, access: &Access, address: EntryAddress) -> Result<VolumeEntry> {
        if access.session_id.is_some() || access.account != self.account.id {
            return Err(Error::AccessDenied("entry copy requires local account authority"));
        }
        let (items, name) = self.duplicate_manifest(access, address)?;
        self.verify_duplicate_contents(access, address.volume, &items)?;
        let mut copied = BTreeMap::new();
        let mut root = None;
        for item in &items {
            let parent = if item.id == address.entry {
                item.parent
            } else {
                Some(
                    *copied
                        .get(&item.parent.expect("subtree child has a parent"))
                        .expect("parent-first manifest has copied the parent"),
                )
            };
            let leaf = if item.id == address.entry { &name } else { &item.name };
            match self.duplicate_item(access, address.volume, item, parent, leaf) {
                Ok(entry) => {
                    copied.insert(item.canonical, entry.id);
                    if root.is_none() {
                        root = Some(entry);
                    }
                }
                Err(error) => {
                    return Err(partial_error(root.as_ref(), copied.len(), items.len(), error));
                }
            }
        }
        if let Err(error) = self.recheck_duplicate_manifest(access, address.volume, &items) {
            return Err(partial_error(root.as_ref(), copied.len(), items.len(), error));
        }
        Ok(root.expect("manifest contains the requested source"))
    }

    fn duplicate_manifest(
        &self,
        access: &Access,
        address: EntryAddress,
    ) -> Result<(Vec<Item>, String)> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&index, &self.file_namespace_cache)?;
        let mut items = vec![self.capture_duplicate_item(access, address, &snapshot)?];
        let root = &items[0];
        self.authorize_duplicate_parent(access, address.volume, root.parent, &snapshot)?;
        let siblings: BTreeSet<String> =
            snapshot.children(root.parent)?.into_iter().map(|entry| entry.name).collect();
        let name = copy_name(&root.name, !root.directory, |name| Ok(siblings.contains(name)))?;
        let mut seen = BTreeSet::from([root.canonical]);
        let mut next = 0;
        while next < items.len() {
            let children = items[next].children.clone();
            for id in children {
                if items.len() == namespace::MAX_ENTRIES || !seen.insert(id) {
                    return Err(Error::LimitExceeded);
                }
                items.push(self.capture_duplicate_item(
                    access,
                    EntryAddress { volume: address.volume, entry: id },
                    &snapshot,
                )?);
            }
            next += 1;
        }
        if items.len() > 1 {
            self.authorize(access, &self.index(address.volume)?, Permission::Write)?;
        }
        check_copy_paths(&snapshot, &items, &name)?;
        check_entry_capacity(&index.begin_read()?, items.len())?;
        Ok((items, name))
    }

    fn capture_duplicate_item(
        &self,
        access: &Access,
        address: EntryAddress,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
    ) -> Result<Item> {
        let source = self.duplicate_source(access, address, snapshot)?;
        let entry = snapshot.namespace_entry(address.entry)?;
        let directory = matches!(source.kind, EntryKind::Directory {});
        let mut children: Vec<EntryId> = if directory {
            snapshot.children(Some(address.entry))?.into_iter().map(|entry| entry.id).collect()
        } else {
            Vec::new()
        };
        children.sort_unstable();
        Ok(Item {
            id: address.entry,
            canonical: entry.id,
            parent: entry.parent,
            name: entry.name.clone(),
            directory,
            children,
            fingerprint: source_fingerprint(&source)?,
        })
    }

    fn duplicate_source(
        &self,
        access: &Access,
        address: EntryAddress,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
    ) -> Result<Source> {
        self.authorize_retained_read(access, address, snapshot)?;
        let entry = snapshot.live_entry(address.entry)?;
        snapshot.namespace_entry(address.entry)?;
        self.authorize_clone_destination(access, address.volume, entry.info.protocol)?;
        if !snapshot.backing_ready(address.entry)? || snapshot.schema_pending(address.entry)? {
            return Err(Error::NotFound("copy source contents are unavailable"));
        }
        if !matches!(entry.info.kind, EntryKind::Blob { .. } | EntryKind::Directory {}) {
            self.entry_backing(address.volume, address.entry, false)?;
        }
        let permissions = snapshot.permissions(address.entry)?;
        let gate = self.volume_operation(address.volume)?;
        let epoch = gate.write_epoch.snapshot()?;
        let kind = entry.info.kind.clone();
        Ok(Source { entry, permissions, kind, gate, epoch, archive: None })
    }

    fn authorize_duplicate_parent(
        &self,
        access: &Access,
        volume: Id,
        parent: Option<EntryId>,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
    ) -> Result<()> {
        let policy = self.index(volume)?;
        self.authorize_entry_creation(access, &policy)?;
        self.file_directory_snapshot(access, &policy, volume, parent, snapshot)?;
        if let Some(parent) = parent {
            snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                self.data_actor(access, policy.volume())?,
                parent,
                Permission::Write,
            )?;
        }
        Ok(())
    }

    fn verify_duplicate_contents(&self, access: &Access, volume: Id, items: &[Item]) -> Result<()> {
        for item in items.iter().filter(|item| !item.directory) {
            let address = EntryAddress { volume, entry: item.id };
            let entry = self.entry(access, volume, item.id)?;
            if matches!(entry.info.kind, EntryKind::Blob { .. }) {
                let version = self.blob_version(access, address, None)?;
                self.verify_blob_version(access, &version)?;
            }
        }
        Ok(())
    }

    fn checked_duplicate_source(&self, access: &Access, volume: Id, item: &Item) -> Result<Source> {
        let index = self.db(volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&index, &self.file_namespace_cache)?;
        let address = EntryAddress { volume, entry: item.id };
        let current = self.capture_duplicate_item(access, address, &snapshot)?;
        if current.fingerprint != item.fingerprint
            || current.canonical != item.canonical
            || current.parent != item.parent
            || current.name != item.name
            || current.children != item.children
        {
            return Err(Error::LocalContention("source subtree changed during copy preparation"));
        }
        self.duplicate_source(access, address, &snapshot)
    }

    fn duplicate_item(
        &self,
        access: &Access,
        volume: Id,
        item: &Item,
        parent: Option<EntryId>,
        name: &str,
    ) -> Result<VolumeEntry> {
        let source = {
            let operations = self.access_operations(access, volume)?;
            let _guards = operations.lock()?;
            self.checked_duplicate_source(access, volume, item)?
        };
        let destination = EntryAddress { volume, entry: EntryId::generate()? };
        let address = EntryAddress { volume, entry: item.id };
        if !matches!(source.kind, EntryKind::Blob { .. } | EntryKind::Directory {}) {
            return self.clone_database_checked(
                access,
                address,
                destination,
                parent,
                name,
                Some(&source),
            );
        }
        let _credit = entry_blobs::Preparation::enter(&self.entry_blobs.active)?;
        let mut info = EntryInfo {
            namespace: namespace::Birth { parent, ..Default::default() },
            origin: namespace::Origin { installation: self.installation(), label: None },
            initial_permissions: EntryAcl::Inherit,
            created_revision: EntryId::generate()?,
            name: name.to_owned(),
            kind: source.kind.clone(),
            protocol: source.entry.info.protocol,
        };
        if matches!(source.kind, EntryKind::Blob { .. }) {
            info.kind = self.prepare_recovered_blob(
                address,
                destination.entry,
                info.created_revision,
                &source,
            )?;
        }
        self.before_entry_recovery_publication()?;
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        self.checked_duplicate_source(access, volume, item)?;
        source.check()?;
        let index = self.db(volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::cached(&index, &self.file_namespace_cache)?;
        self.authorize_duplicate_parent(access, volume, parent, &snapshot)?;
        let mut tx = index.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        entry_blobs::stamp_publication(&mut info.kind)?;
        volume_index::create(&mut tx, destination.entry, &info)?;
        volume_index::mark_backing_ready(&mut tx, destination.entry)?;
        self.changes.publish(volume, DatabaseId::Index, tx.commit_with_delta()?);
        IndexSnapshot::new(index.begin_read()?).live_entry(destination.entry)
    }

    fn recheck_duplicate_manifest(
        &self,
        access: &Access,
        volume: Id,
        items: &[Item],
    ) -> Result<()> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        for item in items {
            self.checked_duplicate_source(access, volume, item)?;
        }
        Ok(())
    }
}

fn source_fingerprint(source: &Source) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    serde_json::to_writer(&mut hash, &(&source.entry, &source.permissions))
        .map_err(Error::decode::<VolumeEntry>)?;
    Ok(*hash.finalize().as_bytes())
}

fn partial_error(root: Option<&VolumeEntry>, copied: usize, total: usize, error: Error) -> Error {
    match root {
        Some(root) => Error::System(format!(
            "Copy partially completed as “{}” ({}): {copied}/{total} entries copied. {error}",
            root.info.name, root.id,
        )),
        None => error,
    }
}

fn copy_name(
    name: &str,
    file: bool,
    mut occupied: impl FnMut(&str) -> Result<bool>,
) -> Result<String> {
    let extension = if file { name.rfind('.') } else { None };
    let (stem, extension) = match extension {
        Some(dot) if dot != 0 && dot + 1 < name.len() => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for number in 1..=namespace::MAX_ENTRIES + 1 {
        let suffix = if number == 1 { " Copy".to_owned() } else { format!(" Copy {number}") };
        let budget = namespace::MAX_COMPONENT_BYTES
            .checked_sub(suffix.len() + extension.len())
            .ok_or(Error::LimitExceeded)?;
        let mut end = stem.len().min(budget);
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return Err(Error::LimitExceeded);
        }
        let candidate = format!("{}{suffix}{extension}", &stem[..end]);
        if !occupied(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(Error::LimitExceeded)
}

fn check_entry_capacity(tx: &kvdb::ReadTxn<'_, RedbStorage>, additional: usize) -> Result<()> {
    let mut cursor = vec![volume_index::ENTRY_ID_BYTE];
    let mut exclusive = false;
    let mut count = additional;
    while let Some(key) = tx.next_key(&cursor, exclusive)? {
        if key.first() != Some(&volume_index::ENTRY_ID_BYTE) {
            break;
        }
        count += 1;
        if count > namespace::MAX_ENTRIES {
            return Err(Error::LimitExceeded);
        }
        cursor = key;
        exclusive = true;
    }
    Ok(())
}

fn check_copy_paths(
    snapshot: &IndexSnapshot<'_, RedbStorage>,
    items: &[Item],
    name: &str,
) -> Result<()> {
    let root = &items[0];
    let (mut bytes, mut depth) = (0, 0);
    let mut parent = root.parent;
    while let Some(id) = parent {
        if depth == namespace::MAX_DEPTH {
            return Err(Error::LimitExceeded);
        }
        let entry = snapshot.namespace_entry(id)?;
        bytes += entry.name.len() + usize::from(depth != 0);
        depth += 1;
        parent = entry.parent;
    }
    let mut paths: BTreeMap<EntryId, (usize, usize)> = BTreeMap::new();
    for item in items {
        let (parent_bytes, parent_depth) = if item.id == root.id {
            (bytes, depth)
        } else {
            paths[&item.parent.expect("subtree child has a parent")]
        };
        let leaf = if item.id == root.id { name } else { &item.name };
        let length = parent_bytes + usize::from(parent_depth != 0) + leaf.len();
        if length > namespace::MAX_PATH_BYTES || parent_depth + 1 > namespace::MAX_DEPTH {
            return Err(Error::LimitExceeded);
        }
        paths.insert(item.canonical, (length, parent_depth + 1));
    }
    Ok(())
}
