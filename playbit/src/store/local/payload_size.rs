//! Background-only current stored data sizes, without hydrating or copying data.
use super::*;
use crate::store::{
    DatabaseId, EntryId, EntryType,
    object_contract::Principal,
    volume_index::{self, EntryAddress, IndexSnapshot},
};
use std::sync::atomic::{AtomicBool, Ordering};

const QUANTUM: usize = 64;
const CHANGED: &str = "data changed during size calculation";

struct Cut {
    handles: [Option<VolumeOperation>; MAX_OPERATION_VOLUMES],
    epochs: [u64; MAX_OPERATION_VOLUMES],
}
impl Cut {
    fn check(&self) -> Result<()> {
        for (handle, epoch) in self.handles.iter().zip(self.epochs) {
            if let Some(handle) = handle
                && handle.write_epoch.snapshot().ok() != Some(epoch)
            {
                return Err(Error::LocalContention(CHANGED));
            }
        }
        Ok(())
    }
}

struct Capture<'a> {
    policy: MetadataPolicy,
    snapshot: IndexSnapshot<'a, RedbStorage>,
    cut: Cut,
    directory: bool,
}

impl LocalStore {
    /// Current live application backing keys/values plus current blob plaintext
    /// lengths. Database framing, tombstones, volume metadata and history do not
    /// count. Protocol records within an application backing do count.
    ///
    /// One background caller owns this scan. No volume/consent lock spans the
    /// scan; a changed authorization or write epoch discards the entire result.
    /// Cold namespace projection is one noninterruptible phase within its
    /// existing 16-MiB source and 4-Mi-unit work bounds. Check cancellation
    /// immediately around it; these bounds do not establish a latency result.
    pub fn payload_size(
        &self,
        access: &Access,
        volume: Id,
        entry: Option<EntryId>,
        cancelled: &AtomicBool,
    ) -> Result<u64> {
        self.payload_size_checked(access, volume, entry, || {
            if cancelled.load(Ordering::Acquire) {
                Err(Error::LocalContention("size calculation cancelled"))
            } else {
                Ok(())
            }
        })
    }

    fn payload_size_checked(
        &self,
        access: &Access,
        volume: Id,
        entry: Option<EntryId>,
        progress: impl Fn() -> Result<()>,
    ) -> Result<u64> {
        progress()?;
        let database = self.db(volume, DatabaseId::Index)?;
        let Capture { policy, snapshot, cut, directory } =
            self.capture_payload_size(access, volume, entry, &database)?;
        let check = || {
            progress()?;
            cut.check()
        };
        check()?;
        let bytes = if directory {
            let root = snapshot.canonical_directory(entry)?;
            if let Some(entry) = entry {
                snapshot.authorize_namespace(
                    policy.volume(),
                    policy.default_permissions(),
                    Principal::Human(access.account),
                    entry,
                    Permission::Read,
                )?;
            }
            check()?;
            let mut after = None;
            let mut bytes = 0u64;
            loop {
                check()?;
                let page = snapshot.page(after, QUANTUM)?;
                check()?;
                for row in page.entries {
                    if row.deleted || !within_directory(&snapshot, root, row.parent)? {
                        continue;
                    }
                    let size =
                        self.entry_payload_size(access, &policy, &snapshot, row.id, &check)?;
                    bytes = bytes.checked_add(size).ok_or(Error::LimitExceeded)?;
                }
                let Some(next) = page.next else { break };
                after = Some(next);
            }
            bytes
        } else {
            self.entry_payload_size(
                access,
                &policy,
                &snapshot,
                entry.expect("a non-directory size has an entry identity"),
                &check,
            )?
        };
        check()?;
        {
            let operations = self.access_operations(access, volume)?;
            let _guards = operations.lock()?;
            cut.check()?;
            self.admit_access(access, &policy)?;
            self.check_authority(access, volume, entry)?;
            if directory {
                self.authorize(access, &policy, Permission::Read)?;
            }
        }
        Ok(bytes)
    }

    fn capture_payload_size<'a>(
        &'a self,
        access: &Access,
        volume: Id,
        entry: Option<EntryId>,
        database: &'a Arc<Database>,
    ) -> Result<Capture<'a>> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = MetadataPolicy::read(database)?;
        if policy.volume().id != volume {
            return Err(Error::AccessDenied("volume policy identity"));
        }
        self.admit_access(access, &policy)?;
        let snapshot = IndexSnapshot::cached(database, &self.file_namespace_cache)?;
        let directory = match entry {
            Some(entry) => {
                self.check_authority(access, volume, Some(entry))?;
                let directory = snapshot.header(entry)?.kind == EntryType::Directory;
                // Directory visibility needs the full namespace projection,
                // which runs after the operation guards have been released.
                if !directory {
                    snapshot.authorize(
                        policy.volume(),
                        policy.default_permissions(),
                        Principal::Human(access.account),
                        entry,
                        Permission::Read,
                    )?;
                }
                directory
            }
            None => true,
        };
        if directory {
            self.authorize(access, &policy, Permission::Read)?;
        }
        let mut epochs = [0; MAX_OPERATION_VOLUMES];
        for (epoch, handle) in epochs.iter_mut().zip(&operations.handles) {
            if let Some(handle) = handle {
                *epoch =
                    handle.write_epoch.snapshot().map_err(|_| Error::LocalContention(CHANGED))?;
            }
        }
        let cut = Cut { handles: operations.handles.clone(), epochs };
        Ok(Capture { policy, snapshot, cut, directory })
    }

    fn entry_payload_size(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
        snapshot: &IndexSnapshot<'_, RedbStorage>,
        entry: EntryId,
        check: &impl Fn() -> Result<()>,
    ) -> Result<u64> {
        check()?;
        let volume = policy.volume().id;
        self.check_authority(access, volume, Some(entry))?;
        let header = snapshot.header(entry)?;
        if header.kind == EntryType::Directory {
            snapshot.authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                Principal::Human(access.account),
                entry,
                Permission::Read,
            )?;
            return Ok(0);
        }
        snapshot.authorize(
            policy.volume(),
            policy.default_permissions(),
            Principal::Human(access.account),
            entry,
            Permission::Read,
        )?;
        if header.kind == EntryType::Blob {
            // Current metadata carries the plaintext length independently of
            // lazy body delivery. Content Read was checked above.
            return Ok(snapshot.blob_version(EntryAddress { volume, entry }, None)?.len_bytes);
        }
        if !snapshot.backing_ready(entry)? || snapshot.schema_pending(entry)? {
            return Err(Error::NotFound("entry content is pending"));
        }
        let database = self.entry_backing(volume, entry, false)?;
        let tx = database.begin_read()?;
        let mut cursor = Vec::new();
        let mut exclusive = false;
        let mut bytes = 0u64;
        loop {
            check()?;
            // Include tombstones in the work quantum, but not in the result.
            // value_len reads only the fixed framing, never the stored value.
            for _ in 0..QUANTUM {
                let Some(key) = tx.next_source_key(&cursor, exclusive)? else {
                    return Ok(bytes);
                };
                if let Some(length) = tx.value_len(&key)? {
                    bytes = bytes
                        .checked_add(key.len() as u64)
                        .and_then(|bytes| bytes.checked_add(length as u64))
                        .ok_or(Error::LimitExceeded)?;
                }
                cursor = key;
                exclusive = true;
            }
            std::thread::yield_now();
        }
    }
}

fn within_directory(
    snapshot: &IndexSnapshot<'_, RedbStorage>,
    root: Option<EntryId>,
    mut parent: Option<EntryId>,
) -> Result<bool> {
    if root.is_none() {
        return Ok(true);
    }
    for _ in 0..volume_index::namespace::MAX_DEPTH {
        if parent == root {
            return Ok(true);
        }
        let Some(id) = parent else { return Ok(false) };
        parent = snapshot.namespace_entry(id)?.parent;
    }
    Err(Error::corrupt("size directory ancestry exceeds its depth limit"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated, authorized store operations.")]
mod tests {
    use super::*;
    use crate::store::{
        object_contract::{Grant, Subject},
        volume_index::EntryKind,
    };

    #[test]
    fn current_payload_sizes_cover_nested_data_and_exclude_retired_content() {
        // Measure real mixed store records and current files, then retire a
        // subtree to verify that metadata, old versions and hidden data do not count.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        let volume =
            store.initialize_root(Id::generate().unwrap(), "Data", true, &declaration).unwrap();
        let access = Access::owner(store.account.id);
        let cancelled = AtomicBool::new(false);
        let size = |entry| store.payload_size(&access, volume.id, entry, &cancelled).unwrap();
        let kv = store
            .create_database(&access, volume.id, "notes/settings", EntryKind::KvDb {})
            .unwrap();
        let address = EntryAddress { volume: volume.id, entry: kv.id };
        store.kv_put(&access, address, b"key", b"value").unwrap();
        store.kv_put(&access, address, b"retired", b"past").unwrap();
        store.kv_delete(&access, address, b"retired").unwrap();
        assert_eq!(size(Some(kv.id)), 8);
        let blob = store.create_blob(&access, volume.id, "notes/file", b"former content").unwrap();
        store.write_blob(&access, volume.id, blob.id, b"new").unwrap();
        let twin = store.create_blob(&access, volume.id, "other", b"new").unwrap();
        assert_eq!(size(Some(blob.id)), 3);
        let notes = store.resolve_entry(&access, volume.id, "notes").unwrap();
        assert_eq!(size(Some(notes.id)), 11);
        let log = store.create_bloblog(&access, volume.id, "activity").unwrap();
        store.bloblog_put(&access, volume.id, log.id, &[b"append".to_vec()]).unwrap();
        // Compare actual user records, independently of the length-only scan.
        let stored = |entry| {
            let db = store.entry_backing(volume.id, entry, false).unwrap();
            db.begin_read()
                .unwrap()
                .iter()
                .map(|row| {
                    let (key, value) = row.unwrap();
                    (key.len() + value.len()) as u64
                })
                .sum::<u64>()
        };
        let objects = declaration.databases[0].id;
        assert_eq!(size(Some(objects)), stored(objects));
        assert_eq!(size(Some(log.id)), stored(log.id));
        let total = 14 + stored(objects) + stored(log.id);
        assert_eq!(size(None), total);
        store.remove_entry(&access, volume.id, twin.id).unwrap();
        assert_eq!(size(None), total - 3);
        store.remove_entry(&access, volume.id, notes.id).unwrap();
        assert_eq!(size(None), total - 14);
        assert!(store.payload_size(&access, volume.id, Some(notes.id), &cancelled).is_err());
    }

    #[test]
    fn size_requires_content_read_and_complete_backings() {
        // Grant catalog Read, deny one file, and remove a database backing;
        // neither known file metadata nor an incomplete sum may become a total.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Private", false).unwrap();
        let owner = Access::owner(store.account.id);
        let reader = Access::owner(Id::generate().unwrap());
        let cancelled = AtomicBool::new(false);
        store.grant_volume(&owner, volume.id, reader.account, false).unwrap();
        let kv = store.create_database(&owner, volume.id, "settings", EntryKind::KvDb {}).unwrap();
        let blob = store.create_blob(&owner, volume.id, "file", b"secret").unwrap();
        assert_eq!(store.payload_size(&reader, volume.id, None, &cancelled).unwrap(), 6);
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                blob.id,
                EntryAcl::Replace(Acl {
                    grants: vec![Grant {
                        subject: Subject::Principal(Principal::Human(owner.account)),
                        permissions: vec![Permission::Read],
                    }],
                }),
            )
            .unwrap();
        for target in [None, Some(blob.id)] {
            assert!(matches!(
                store.payload_size(&reader, volume.id, target, &cancelled),
                Err(Error::AccessDenied(_))
            ));
        }
        assert_eq!(store.payload_size(&owner, volume.id, None, &cancelled).unwrap(), 6);
        let identity = DatabaseId::Entry(kv.id);
        store.close_db(volume.id, identity).unwrap();
        let path = directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(volume.id.to_string())
            .join(identity.file_name());
        fs::remove_file(&path).unwrap();
        assert!(store.payload_size(&owner, volume.id, None, &cancelled).is_err());
        assert!(store.payload_size(&owner, volume.id, Some(kv.id), &cancelled).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn size_discards_a_changed_cut_and_stops_cancelled_scans() {
        // Change content/authority or cancel between real database quanta;
        // the operation must discard its pinned partial sum without holding writers.
        use std::cell::Cell;
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Mutable", false).unwrap();
        let access = Access::owner(store.account.id);
        let kv = store.create_database(&access, volume.id, "settings", EntryKind::KvDb {}).unwrap();
        let address = EntryAddress { volume: volume.id, entry: kv.id };
        store
            .with_replica(volume.id, DatabaseId::Entry(kv.id), |db| {
                let mut tx = db.begin_write(now_unix_secs())?;
                for key in 0u32..130 {
                    tx.put(&key.to_be_bytes(), b"value")?;
                }
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        for action in ["write", "withdraw Read", "cancel"] {
            let turns = Cell::new(0);
            let result = store.payload_size_checked(&access, volume.id, Some(kv.id), || {
                turns.set(turns.get() + 1);
                if turns.get() == 5 {
                    match action {
                        "write" => store.kv_put(&access, address, b"changed", b"during scan")?,
                        "withdraw Read" => store.set_entry_permissions(
                            &access,
                            volume.id,
                            kv.id,
                            EntryAcl::Replace(Acl::default()),
                        )?,
                        _ => return Err(Error::LocalContention("size calculation cancelled")),
                    }
                }
                Ok(())
            });
            assert!(matches!(result, Err(Error::LocalContention(_))));
            assert_eq!(turns.get(), 5, "the second bounded key quantum stops");
            if action == "withdraw Read" {
                store.set_entry_permissions(&access, volume.id, kv.id, EntryAcl::Inherit).unwrap();
            }
        }
        let cancelled = AtomicBool::new(true);
        assert!(store.payload_size(&access, volume.id, None, &cancelled).is_err());
    }
}
