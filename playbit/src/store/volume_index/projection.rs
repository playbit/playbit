//! Catalog navigation uses admitted immutable namespace facts. A pinned snapshot
//! computes the projection once; known-ID backing/ACL access stays independent.
use super::*;
use std::sync::{Arc, Mutex, Weak};

/// One reusable full-cut projection per local store. A cached projection never
/// supplies authority and never survives a change to its exact pinned snapshot.
/// The existing cut materialization ceiling bounds this additional retained
/// projection to 2 MiB, plus fixed cache bookkeeping. No database is kept open.
pub(crate) struct NamespaceCache<S: Storage> {
    entry: Mutex<Option<CachedNamespace<S>>>,
}
impl<S: Storage> Default for NamespaceCache<S> {
    fn default() -> Self {
        Self { entry: Mutex::new(None) }
    }
}
#[cfg(any(feature = "local-store", test))]
impl<S: Storage> NamespaceCache<S> {
    /// Capture identity before the writer's first namespace mutation. No
    /// projection is pinned; extraction happens after preparing its one edit.
    pub(crate) fn predecessor(
        &self,
        database: &Arc<kvdb::Db<S>>,
        tx: &WriteTxn<'_, S>,
    ) -> CacheSnapshot<'_, S> {
        CacheSnapshot {
            cache: self,
            database: Arc::downgrade(database),
            clock: tx.committed_clock(),
        }
    }

    /// Only after commit succeeds, with the clock of that exact transaction.
    /// Current-control and backing-ready writes do not change projection input.
    pub(crate) fn retain_committed(
        &self,
        database: &Arc<kvdb::Db<S>>,
        clock: kvdb::Clock,
        projection: namespace::source::cut::Projection,
    ) -> Result<()> {
        CacheSnapshot { cache: self, database: Arc::downgrade(database), clock }
            .retain(Arc::new(projection))
    }
}
struct CachedNamespace<S: Storage> {
    database: Weak<kvdb::Db<S>>,
    clock: kvdb::Clock,
    projection: Arc<namespace::source::cut::Projection>,
}
pub(crate) struct CacheSnapshot<'db, S: Storage> {
    cache: &'db NamespaceCache<S>,
    database: Weak<kvdb::Db<S>>,
    clock: kvdb::Clock,
}
impl<S: Storage> CacheSnapshot<'_, S> {
    /// Consume only an exclusive exact predecessor. Live readers or a concurrent
    /// cache replacement cause a miss, not another pinned old materialization.
    pub(crate) fn take(self) -> Result<Option<namespace::source::cut::Projection>> {
        let previous = {
            let mut cache = self
                .cache
                .entry
                .lock()
                .map_err(|_| Error::Internal("namespace cache lock is poisoned"))?;
            if cache.as_ref().is_some_and(|entry| {
                entry.database.ptr_eq(&self.database)
                    && entry.clock == self.clock
                    && Arc::strong_count(&entry.projection) == 1
            }) {
                cache.take()
            } else {
                None
            }
        };
        // Move or release the allocation only after releasing the cache mutex.
        Ok(previous.and_then(|entry| Arc::try_unwrap(entry.projection).ok()))
    }

    fn get(&self) -> Result<Option<Arc<namespace::source::cut::Projection>>> {
        let cache = self
            .cache
            .entry
            .lock()
            .map_err(|_| Error::Internal("namespace cache lock is poisoned"))?;
        Ok(cache
            .as_ref()
            .filter(|entry| entry.database.ptr_eq(&self.database) && entry.clock == self.clock)
            .map(|entry| entry.projection.clone()))
    }
    fn retain(&self, projection: Arc<namespace::source::cut::Projection>) -> Result<()> {
        let previous = self
            .cache
            .entry
            .lock()
            .map_err(|_| Error::Internal("namespace cache lock is poisoned"))?
            .replace(CachedNamespace {
                database: self.database.clone(),
                clock: self.clock,
                projection,
            });
        // Releasing a large old projection must not hold the cache mutex.
        drop(previous);
        Ok(())
    }
}
#[cfg(any(feature = "local-store", test))]
impl<'db, S: Storage> IndexSnapshot<'db, S> {
    pub(crate) fn cached(
        database: &'db Arc<kvdb::Db<S>>,
        cache: &'db NamespaceCache<S>,
    ) -> Result<Self> {
        let tx = database.begin_read()?;
        let clock = tx.clock()?;
        // Syncable index commits advance this clock even for local-only writes.
        // A plain database has no such identity and cannot enter this cache.
        if !database.meta().syncable {
            return Err(Error::InvalidInput("namespace cache requires a syncable index"));
        }
        Ok(Self {
            tx,
            namespace: std::sync::OnceLock::new(),
            namespace_cache: Some(CacheSnapshot {
                cache,
                database: Arc::downgrade(database),
                clock,
            }),
        })
    }
}

impl<S: Storage> IndexSnapshot<'_, S> {
    pub(super) fn namespace_projection(&self) -> Result<&namespace::source::cut::Projection> {
        if self.namespace.get().is_none() {
            let cached =
                self.namespace_cache.as_ref().map(CacheSnapshot::get).transpose()?.flatten();
            let projection = match cached {
                Some(projection) => projection,
                None => {
                    let projection = Arc::new(namespace::source::cut::project(&self.tx)?);
                    if let Some(cache) = &self.namespace_cache {
                        cache.retain(projection.clone())?;
                    }
                    projection
                }
            };
            // Concurrent readers may have computed the same pinned projection.
            let _ = self.namespace.set(projection);
        }
        Ok(self.namespace.get().expect("successful projection is retained"))
    }

    /// Namespace operations check projected visibility and the original
    /// identity's current ACL. Coalescence never substitutes another ACL.
    pub(crate) fn authorize_namespace(
        &self,
        volume: &Volume,
        defaults: &Acl,
        principal: impl Into<DataActor>,
        id: EntryId,
        permission: Permission,
    ) -> Result<()> {
        self.namespace_entry(id)?;
        self.authorize_acl(volume, defaults, principal, id, permission)
    }

    pub(crate) fn namespace_entry(&self, id: EntryId) -> Result<&namespace::Entry> {
        visible_entry(&self.namespace_projection()?.admission.view, id)
    }

    /// Resolve only same-incarnation aliases. The caller retains the original
    /// identity for scope and ACL checks; this ID is for namespace traversal.
    pub(crate) fn canonical_directory(&self, parent: Option<EntryId>) -> Result<Option<EntryId>> {
        if parent.is_none() {
            return Ok(None);
        }
        canonical_directory(&self.namespace_projection()?.admission.view, parent)
    }

    /// Catalog directory details use the same incarnation and effective name
    /// as navigation. Keep the caller's original identity for all ACL checks.
    #[cfg(any(feature = "local-store", test))]
    pub(crate) fn project_directory_details(&self, record: &mut VolumeEntry) -> Result<()> {
        let id = record.id;
        let projection = self.namespace_projection()?;
        let canonical = projection
            .admission
            .view
            .canonical
            .get(&id)
            .ok_or(Error::NotFound("namespace directory"))?;
        let entry = projection
            .admission
            .view
            .entries
            .get(canonical)
            .ok_or(Error::corrupt("missing canonical directory"))?;
        if entry.kind != namespace::Kind::Directory {
            return Err(Error::InvalidInput("entry is not a directory"));
        }
        record.info.name = entry.name.clone();
        record.info.namespace.parent = entry.parent;
        record.name_revision = projection
            .sources
            .get(&entry.name_event)
            .ok_or(Error::corrupt("missing directory name source"))?
            .revision;
        record.deleted = !entry.visible;
        record.name_conflict = entry.visible && entry.name != entry.requested_name;
        Ok(())
    }

    pub fn directory(&self, parent: Option<EntryId>) -> Result<()> {
        self.canonical_directory(parent).map(|_| ())
    }

    pub fn lookup(&self, path: &str) -> Result<VolumeEntry> {
        self.lookup_from(None, path)
    }

    pub fn lookup_from(&self, parent: Option<EntryId>, path: &str) -> Result<VolumeEntry> {
        validate_name(path)?;
        let mut parent = self.canonical_directory(parent)?;
        let projection = self.namespace_projection()?;
        let mut components = path.split('/').peekable();
        while let Some(name) = components.next() {
            let entry = projection
                .admission
                .view
                .entries
                .values()
                .find(|entry| entry.visible && entry.parent == parent && entry.name == name)
                .ok_or(Error::NotFound("entry"))?;
            if components.peek().is_none() {
                let mut record = self.entry(entry.id)?;
                if entry.kind == namespace::Kind::File {
                    let selected = projection
                        .current
                        .get(&entry.id)
                        .ok_or(Error::corrupt("accepted file has no current sources"))?;
                    let content = &projection.sources[&selected.content.unwrap_or(selected.birth)];
                    record.info.kind = match self
                        .read_revision(entry.id, content.revision, Some(content.read(&self.tx)?))?
                        .change
                    {
                        EntryChange::Created { info } => info.kind,
                        EntryChange::Content { kind } => kind,
                        _ => return Err(Error::corrupt("invalid accepted content source")),
                    };
                    record.content_revision = content.revision;
                }
                record.info.name = entry.name.clone();
                record.info.namespace.parent = entry.parent;
                record.name_revision = projection
                    .sources
                    .get(&entry.name_event)
                    .ok_or(Error::corrupt("missing namespace name source"))?
                    .revision;
                record.deleted = false;
                record.name_conflict = entry.name != entry.requested_name;
                return Ok(record);
            }
            if entry.kind != namespace::Kind::Directory {
                return Err(Error::InvalidInput("entry path component is not a directory"));
            }
            parent = Some(entry.id);
        }
        Err(Error::InvalidInput("empty entry path"))
    }

    pub(super) fn namespace_summary(&self, entry: &namespace::Entry) -> Result<EntrySummary> {
        let mut summary = self.summary(entry.id)?;
        summary.parent = entry.parent;
        summary.name = entry.name.clone().into_boxed_str().into_string();
        summary.deleted = !entry.visible;
        summary.name_conflict = entry.visible && entry.name != entry.requested_name;
        Ok(summary)
    }

    /// Capture effective names from one causal cut, without opening backing data.
    pub fn children(&self, parent: Option<EntryId>) -> Result<Vec<EntrySummary>> {
        let parent = self.canonical_directory(parent)?;
        let view = &self.namespace_projection()?.admission.view;
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        for entry in view.entries.values().filter(|entry| entry.visible && entry.parent == parent) {
            if entries.len() == 16 * 1024 {
                return Err(Error::LimitExceeded);
            }
            bytes = bytes
                .checked_add(entry.name.len() + size_of::<EntrySummary>())
                .ok_or(Error::LimitExceeded)?;
            if bytes > 4 * 1024 * 1024 {
                return Err(Error::LimitExceeded);
            }
            entries.push(self.namespace_summary(entry)?);
        }
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

pub(super) fn visible_entry(view: &namespace::View, id: EntryId) -> Result<&namespace::Entry> {
    let canonical = view.canonical.get(&id).ok_or(Error::NotFound("namespace entry"))?;
    let entry = view.entries.get(canonical).ok_or(Error::corrupt("missing canonical entry"))?;
    if !entry.visible {
        return Err(Error::NotFound("namespace entry"));
    }
    Ok(entry)
}

pub(super) fn canonical_directory(
    view: &namespace::View,
    parent: Option<EntryId>,
) -> Result<Option<EntryId>> {
    let Some(parent) = parent else { return Ok(None) };
    let entry = match visible_entry(view, parent) {
        Err(Error::NotFound(_)) => return Err(Error::NotFound("destination directory")),
        result => result?,
    };
    if entry.kind != namespace::Kind::Directory {
        return Err(Error::InvalidInput("entry is not a directory"));
    }
    Ok(Some(entry.id))
}
