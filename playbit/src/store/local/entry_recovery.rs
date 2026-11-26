//! Explicit recovery publishes a fresh identity; retirement is never erased.
use super::*;
#[cfg(test)]
use crate::store::object_contract::Principal;
use crate::store::{
    DatabaseId, EntryId, MAX_REPLICA_DELTA_BYTES,
    volume_index::{
        self, EntryAddress, EntryInfo, EntryKind, HistoryCursor, HistoryPage, IndexSnapshot,
        VolumeEntry,
    },
};
mod backing;
mod clone;
#[cfg(test)]
mod clone_tests;
mod discovery;
mod duplicate;
#[cfg(test)]
mod tests;

struct Source {
    entry: VolumeEntry,
    permissions: EntryAcl,
    kind: EntryKind,
    gate: VolumeOperation,
    epoch: u64,
    archive: Option<PathBuf>,
}
impl Source {
    fn check(&self) -> Result<()> {
        if self.gate.write_epoch.snapshot().ok() != Some(self.epoch) {
            return Err(Error::LocalContention("entry changed during recovery preparation"));
        }
        Ok(())
    }
}

impl LocalStore {
    /// History includes losing revisions and retired entries, under current Read.
    pub fn entry_revisions(
        &self,
        access: &Access,
        address: EntryAddress,
        after: Option<HistoryCursor>,
        limit: usize,
    ) -> Result<HistoryPage> {
        let operations = self.access_operations(access, address.volume)?;
        let guards = operations.lock()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        self.authorize_retained_read(access, address, &snapshot)?;
        drop(guards);
        let mut page = snapshot.history(address.entry, after, limit)?;
        for cut in received::rejected::cuts(&index)? {
            if cut.volume != address.volume {
                return Err(Error::corrupt("rejected entry cut belongs to a different volume"));
            }
            if cut.entries.binary_search(&address.entry).is_err() {
                continue;
            }
            let directory = self.path.join(crate::paths::RECEIVED).join(cut.operation.to_string());
            let retained = open_replica(&directory.join("rejected/index.kvdb"), 512 * 1024)?;
            cut.verify_index(&retained)?;
            let history =
                IndexSnapshot::new(retained.begin_read()?).history(address.entry, after, limit)?;
            page = merge_retained_revisions(page, history, limit)?;
        }
        let _guards = operations.lock()?;
        let current_index = self.db(address.volume, DatabaseId::Index)?;
        let current = IndexSnapshot::new(current_index.begin_read()?);
        self.authorize_retained_read(access, address, &current)?;
        Ok(page)
    }

    /// Background-only. A failed preparation leaves no usable new index entry.
    /// An explicit blob revision selects retained content; databases use one
    /// current pinned backing/schema cut, never an invented historical snapshot.
    pub fn recover_entry(
        &self,
        access: &Access,
        address: EntryAddress,
        name: &str,
        revision: Option<EntryId>,
    ) -> Result<VolumeEntry> {
        self.recover_entry_cut(access, address, name, revision, None)
    }

    /// Retained local cuts are not source facts and never republish themselves.
    /// The explicit recovery result has a fresh identity and current admission.
    pub fn recover_rejected_entry(
        &self,
        access: &Access,
        address: EntryAddress,
        name: &str,
        cut: Id,
    ) -> Result<VolumeEntry> {
        self.recover_entry_cut(access, address, name, None, Some(cut))
    }

    pub fn rejected_entry_cuts(&self, access: &Access, address: EntryAddress) -> Result<Vec<Id>> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let current = IndexSnapshot::new(index.begin_read()?);
        let mut result = Vec::new();
        for cut in received::rejected::cuts(&index)? {
            if cut.volume != address.volume {
                return Err(Error::corrupt("rejected entry cut belongs to a different volume"));
            }
            if cut.entries.binary_search(&address.entry).is_err() {
                continue;
            }
            let original = open_replica(
                &self
                    .path
                    .join(crate::paths::RECEIVED)
                    .join(cut.operation.to_string())
                    .join("rejected/index.kvdb"),
                512 * 1024,
            )?;
            cut.verify_index(&original)?;
            let retained = IndexSnapshot::new(original.begin_read()?);
            self.authorize_rejected_read(access, address, &current, &retained)?;
            result.push(cut.operation);
        }
        if result.is_empty() {
            self.authorize_retained_read(access, address, &current)?;
        }
        Ok(result)
    }

    fn recover_entry_cut(
        &self,
        access: &Access,
        address: EntryAddress,
        name: &str,
        revision: Option<EntryId>,
        cut: Option<Id>,
    ) -> Result<VolumeEntry> {
        volume_index::validate_name(name)?;
        let source = self.recovery_source(access, address, name, revision, cut)?;
        let _credit = entry_blobs::Preparation::enter(&self.entry_blobs)?;
        let id = EntryId::generate()?;
        let mut info = EntryInfo {
            namespace: Default::default(),
            origin: crate::store::volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
            initial_permissions: source.permissions.clone(),
            created_revision: EntryId::generate()?,
            name: name.rsplit('/').next().expect("validated path").to_owned(),
            kind: source.kind.clone(),
            protocol: source.entry.info.protocol,
        };
        let prepared = if matches!(source.kind, EntryKind::Blob { .. }) {
            info.kind = self.prepare_recovered_blob(address, id, info.created_revision, &source)?;
            None
        } else if matches!(source.kind, EntryKind::Directory {}) {
            None
        } else {
            Some(self.prepare_recovered_database(
                access,
                address,
                EntryAddress { volume: address.volume, entry: id },
                &source,
            )?)
        };
        self.before_entry_recovery_publication()?;
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let current = self.recovery_source_locked(access, address, name, revision, cut)?;
        source.check()?;
        if current.entry != source.entry
            || current.permissions != source.permissions
            || current.kind != source.kind
        {
            return Err(Error::LocalContention("entry changed during recovery preparation"));
        }
        if let Some(prepared) = &prepared {
            prepared.publish(
                &self.path.join(crate::paths::VOLUMES).join(address.volume.to_string()),
                id,
            )?;
        }
        let index = self.db(address.volume, DatabaseId::Index)?;
        let mut tx = index.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        super::entry_blobs::stamp_publication(&mut info.kind)?;
        volume_index::create_at_path(&mut tx, id, name, &info)?;
        volume_index::mark_backing_ready(&mut tx, id)?;
        let delta = tx.commit_with_delta()?;
        self.changes.publish(address.volume, DatabaseId::Index, delta);
        IndexSnapshot::new(index.begin_read()?).live_entry(id)
    }

    fn authorize_retained_read(
        &self,
        access: &Access,
        address: EntryAddress,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
    ) -> Result<()> {
        let policy = self.index(address.volume)?;
        self.admit_access(access, &policy)?;
        self.check_authority(access, address.volume, Some(address.entry))?;
        snapshot.authorize_retained_read(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            address.entry,
        )
    }

    fn recovery_source(
        &self,
        access: &Access,
        address: EntryAddress,
        name: &str,
        revision: Option<EntryId>,
        cut: Option<Id>,
    ) -> Result<Source> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.recovery_source_locked(access, address, name, revision, cut)
    }

    fn recovery_source_locked(
        &self,
        access: &Access,
        address: EntryAddress,
        name: &str,
        revision: Option<EntryId>,
        cut: Option<Id>,
    ) -> Result<Source> {
        let gate = self.volume_operation(address.volume)?;
        let epoch = gate.write_epoch.snapshot()?;
        let index = self.db(address.volume, DatabaseId::Index)?;
        let current = IndexSnapshot::new(index.begin_read()?);
        let retained_cut = retained_cut(&index, address, cut)?;
        let archive = retained_cut
            .as_ref()
            .map(|retained| {
                let directory =
                    self.path.join(crate::paths::RECEIVED).join(retained.operation.to_string());
                retained.verify(&directory)?;
                Ok::<_, Error>(directory.join("rejected"))
            })
            .transpose()?;
        let original = archive
            .as_ref()
            .map(|directory| open_replica(&directory.join("index.kvdb"), 512 * 1024))
            .transpose()?;
        let snapshot = IndexSnapshot::new(original.as_ref().unwrap_or(&index).begin_read()?);
        if cut.is_some() {
            self.authorize_rejected_read(access, address, &current, &snapshot)?;
        } else {
            self.authorize_retained_read(access, address, &snapshot)?;
        }
        let entry = if cut.is_some() {
            snapshot.source_entry(address.entry)?
        } else {
            snapshot.entry(address.entry)?
        };
        if cut.is_none() && !entry.deleted {
            return Err(Error::InvalidInput("recovery requires a retired entry"));
        }
        let policy = self.index(address.volume)?;
        self.authorize_entry_creation(access, &policy)?;
        if entry.info.protocol == Some(volume_index::DatabaseProtocol::BlobLog) {
            // Recovered appends use the same Write admission as log upload.
            self.authorize(access, &policy, Permission::Write)?;
        }
        let permissions =
            self.recovery_permissions(access, address, &current, &snapshot, &policy)?;
        match current.lookup(name) {
            Ok(_) => return Err(Error::AlreadyExists("entry name")),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let backing_ready = || {
            if cut.is_some() {
                snapshot.source_backing_ready(address.entry)
            } else {
                snapshot.backing_ready(address.entry)
            }
        };
        if !matches!(entry.info.kind, EntryKind::Blob { .. })
            && (!backing_ready()? || snapshot.schema_pending(address.entry)?)
        {
            return Err(Error::NotFound("retained entry backing is unavailable"));
        }
        let kind = retained_kind(&snapshot, &entry, revision)?;
        let source = Source { entry, permissions, kind, gate, epoch, archive };
        source.check()?;
        Ok(source)
    }

    fn recovery_permissions(
        &self,
        access: &Access,
        address: EntryAddress,
        current: &IndexSnapshot<'_, RedbStorage>,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        policy: &MetadataPolicy,
    ) -> Result<EntryAcl> {
        let permissions = match current.permissions(address.entry) {
            Ok(permissions) => permissions,
            Err(Error::NotFound(_)) => snapshot.permissions(address.entry)?,
            Err(error) => return Err(error),
        };
        if matches!(permissions, EntryAcl::Replace(_)) {
            self.authorize(access, policy, Permission::ManagePermissions)?;
            if access.account != policy.volume().owner {
                return Err(Error::AccessDenied("recovery must preserve the entry override"));
            }
        }
        Ok(permissions)
    }

    fn prepare_recovered_blob(
        &self,
        address: EntryAddress,
        id: EntryId,
        revision: EntryId,
        source: &Source,
    ) -> Result<EntryKind> {
        let EntryKind::Blob { digest, .. } = &source.kind else {
            return Err(Error::InvalidInput("recovery source is not a blob"));
        };
        let mut file = if let Some(directory) = &source.archive {
            fs::File::open(directory.join(format!("{}.blob", hex(digest)))).map_err(io)?
        } else {
            let _cache = self
                .blob_cache
                .lock()
                .map_err(|_| Error::Internal("blob cache lock is poisoned"))?;
            self.open_entry_blob(digest).map_err(io)?
        };
        let prepared = self.prepare_entry_blob(address.volume, id, revision, &mut file)?;
        let mut source_kind = source.kind.clone();
        if let EntryKind::Blob { published_at_micros, .. } = &mut source_kind {
            *published_at_micros = 0;
        }
        if prepared != source_kind {
            return Err(Error::corrupt("recovered blob differs from its declared content"));
        }
        source.check()?;
        Ok(prepared)
    }

    pub(super) fn authorize_rejected_read(
        &self,
        access: &Access,
        address: EntryAddress,
        current: &IndexSnapshot<'_, RedbStorage>,
        retained: &IndexSnapshot<'_, RedbStorage>,
    ) -> Result<()> {
        match self.authorize_retained_read(access, address, current) {
            Err(Error::NotFound(_)) => {
                let policy = self.index(address.volume)?;
                self.authorize(access, &policy, Permission::Read)?;
                retained.authorize_retained_read(
                    policy.volume(),
                    policy.default_permissions(),
                    self.data_actor(access, policy.volume())?,
                    address.entry,
                )
            }
            result => result,
        }
    }

    fn before_entry_recovery_publication(&self) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = self
            .entry_blobs
            .recovery_before_publish
            .lock()
            .map_err(|_| Error::Internal("entry recovery publication hook lock is poisoned"))?
            .clone()
        {
            hook()?;
        }
        Ok(())
    }
}

fn retained_kind(
    snapshot: &IndexSnapshot<RedbStorage>,
    entry: &VolumeEntry,
    revision: Option<EntryId>,
) -> Result<EntryKind> {
    if matches!(entry.info.kind, EntryKind::Blob { .. }) {
        Ok(snapshot
            .retained_blob_kind(entry.id, Some(revision.unwrap_or(entry.content_revision)))?
            .1)
    } else if revision.is_some() {
        Err(Error::InvalidInput("database recovery uses its retained backing snapshot"))
    } else {
        Ok(entry.info.kind.clone())
    }
}

// Merge one bounded archive page at a time, retaining at most two input pages.
// These local-only revisions never enter the replicated source/history keys.
fn merge_retained_revisions(
    left: HistoryPage,
    right: HistoryPage,
    limit: usize,
) -> Result<HistoryPage> {
    let mut more = left.next.is_some() || right.next.is_some();
    // A byte-limited source page may stop before the row limit. Do not emit
    // older rows from another source ahead of that source's unread suffix.
    let boundary = left.next.into_iter().chain(right.next).max();
    let mut rows = BTreeMap::new();
    for revision in left.revisions.into_iter().chain(right.revisions) {
        if let Some(previous) = rows.insert(revision.revision, revision.clone())
            && previous != revision
        {
            return Err(Error::corrupt(
                "entry recovery contains conflicting records for one revision",
            ));
        }
    }
    let mut revisions = Vec::with_capacity(limit);
    let mut bytes = 0usize;
    let mut ordered: Vec<_> = rows.into_values().collect();
    ordered.sort_unstable_by_key(|record| std::cmp::Reverse(HistoryCursor::from(record)));
    for revision in ordered {
        let size = encode(&revision)?.len();
        if revisions.len() == limit
            || bytes + size > volume_index::MAX_ENTRY_PAGE_BYTES
            || boundary.is_some_and(|boundary| HistoryCursor::from(&revision) < boundary)
        {
            more = true;
            break;
        }
        bytes += size;
        revisions.push(revision);
    }
    if revisions.is_empty() && more {
        return Err(Error::LimitExceeded);
    }
    let next = if more { revisions.last().map(HistoryCursor::from) } else { None };
    Ok(HistoryPage { revisions, next })
}

fn retained_cut(
    index: &Database,
    address: EntryAddress,
    cut: Option<Id>,
) -> Result<Option<received::rejected::RejectedCut>> {
    Ok(if let Some(operation) = cut {
        let retained = received::rejected::cuts(index)?
            .into_iter()
            .find(|cut| {
                cut.operation == operation
                    && cut.volume == address.volume
                    && cut.entries.binary_search(&address.entry).is_ok()
            })
            .ok_or(Error::NotFound("rejected entry cut"))?;
        Some(retained)
    } else {
        None
    })
}
