//! Account-wide deletion is an irreversible account record, independent of data
//! availability. Retained backing files confer no access after its local fence.
use super::*;

impl LocalStore {
    /// Capture only bounded current policy metadata, retaining the owner repair
    /// path. This does not open entry backings, scan files, or contact a peer.
    pub fn volume_details(&self, access: &Access, volume: Id) -> Result<VolumeDetails> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let db = self.db(volume, crate::store::DatabaseId::Index)?;
        let tx = db.begin_read()?;
        let policy = MetadataPolicy::read_snapshot(&tx, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
        if policy.volume.id != volume {
            return Err(Error::AccessDenied("volume policy identity"));
        }
        self.authorize(access, &policy, Permission::Read)?;
        let created_clock =
            tx.get_with_clock(&FORMAT_KEY)?.ok_or(Error::UnsupportedVolumeFormat)?.0.as_u64();
        let name_clock =
            tx.get_with_clock(&VOLUME_KEY)?.ok_or(Error::NotFound("volume policy"))?.0.as_u64();
        Ok(VolumeDetails {
            volume: policy.volume,
            permissions: policy.permissions,
            created_clock,
            name_clock,
        })
    }

    pub(crate) fn volume_deleted(&self, volume: Id) -> Result<bool> {
        self.metadata
            .begin_read()?
            .has(format!("deleted-volume/{volume}").as_bytes())
            .map_err(Into::into)
    }
    pub(super) fn require_volume_available(&self, volume: Id) -> Result<()> {
        if self.volume_deleted(volume)? {
            return Err(Error::NotFound("volume was deleted from this account"));
        }
        Ok(())
    }
    /// Deleting unavailable private data needs only its recovered identity.
    pub fn delete_volume(&self, volume: Id) -> Result<()> {
        let operations = self.operations(&[volume])?;
        let gate = self.volume_operation(volume)?;
        let _guards = operations.lock_for_publication(&gate)?;
        let _profile = self.lock_profile()?;
        if self.volume_deleted(volume)? {
            return Ok(());
        }
        let descriptor = self.volume(volume)?;
        let withdrawal_owner = self.withdrawal_owner_on_delete(volume)?;
        if withdrawal_owner.is_some() {
            profile_format::enable_volume_invitations(self)?;
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        self.stage_volume_deletion(&mut tx, volume, withdrawal_owner)?;
        let changed = self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::VolumeDeleted { volume, withdrawal_owner },
        )?;
        tx.commit()?;
        self.committed_account_record(changed);
        self.changes.observed_changed(Some(volume));
        self.notify_volume_invitation_policy(&descriptor);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful isolated store operations.")]
mod tests {
    use super::*;
    use crate::store::{DatabaseId, volume_index::EntryKind};

    #[test]
    fn duplicate_volume_preserves_current_contents_with_independent_identity() {
        // Copy KV, object, blob and append-log contents, then reopen and edit only the copy.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        let original =
            store.initialize_root(Id::generate().unwrap(), "Original", true, &declaration).unwrap();
        let access = Access::owner(store.account.id);
        let kv =
            store.create_database(&access, original.id, "settings", EntryKind::KvDb {}).unwrap();
        store
            .with_replica(original.id, DatabaseId::Entry(kv.id), |db| {
                let mut tx = db.begin_write(now_unix_secs())?;
                tx.put(b"key", b"value")?;
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        let blob = store.create_blob(&access, original.id, "notes/draft", b"hello").unwrap();
        store.rename_entry(&access, original.id, blob.id, "notes/final").unwrap();
        let log = store.create_bloblog(&access, original.id, "activity").unwrap();
        store.bloblog_put(&access, original.id, log.id, &[b"append".to_vec()]).unwrap();
        let copy = store.duplicate_volume(original.id).unwrap();
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.name, "Original Copy");
        assert!(copy.share_key_with_service);
        assert_ne!(store.credentials(original.id).unwrap(), store.credentials(copy.id).unwrap());
        assert_eq!(store.read_blob_entry(&access, copy.id, blob.id).unwrap(), b"hello");
        assert_eq!(store.resolve_entry(&access, copy.id, "notes/final").unwrap().id, blob.id);
        assert!(store.resolve_entry(&access, copy.id, "notes/draft").is_err());
        assert_eq!(store.bloblog_pending(&access, copy.id, log.id).unwrap()[0].data, b"append");
        for entry in [kv.id, declaration.databases[0].id] {
            let old = store.entry_backing(original.id, entry, false).unwrap();
            let new = store.entry_backing(copy.id, entry, false).unwrap();
            let rows = |db: &Database| {
                db.begin_read()
                    .unwrap()
                    .iter()
                    .map(|row| {
                        let (k, v) = row.unwrap();
                        (k.to_vec(), v.to_vec())
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(rows(&old), rows(&new));
        }
        store.write_blob(&access, copy.id, blob.id, b"changed").unwrap();
        assert_eq!(store.read_blob_entry(&access, original.id, blob.id).unwrap(), b"hello");
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.read_blob_entry(&access, copy.id, blob.id).unwrap(), b"changed");
    }

    #[test]
    fn duplicate_volume_names_preserve_unicode_within_the_label_limit() {
        // Appending the copy suffix truncates only the new label at UTF-8 boundaries.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        for (name, expected) in [
            ("a".repeat(251), format!("{} Copy", "a".repeat(251))),
            ("é".repeat(128), format!("{} Copy", "é".repeat(125))),
        ] {
            let original = store.create_volume(&name, false).unwrap();
            let copy = store.duplicate_volume(original.id).unwrap();
            assert_eq!(copy.name, expected);
            assert!(copy.name.len() <= 256);
            assert_eq!(store.volume(original.id).unwrap().name, name);
        }
    }

    #[test]
    fn delete_volume_replicates_and_stale_credentials_cannot_restore_access() {
        // A second device receives the deletion, including when it never had the data.
        let directory = tempfile::tempdir().unwrap();
        let source = LocalStore::open(directory.path().join("source")).unwrap();
        let unlock = |store: &LocalStore| {
            store
                .save_recovery_state(&RecoveryState {
                    secret: [7; 16],
                    acknowledged: true,
                    revision: 1,
                    synchronized_digest: None,
                    pending: None,
                })
                .unwrap()
        };
        unlock(&source);
        let volume = source.create_volume("Private", false).unwrap();
        let creation = source.pending_account_record(None).unwrap().unwrap();
        let mut target = LocalStore::open(directory.path().join("target")).unwrap();
        target.bind_authenticated_account(source.account.id).unwrap();
        unlock(&target);
        target.stage_account_record(&creation).unwrap().unwrap();
        target.reconcile_account_records().unwrap();
        assert_eq!(target.volumes().unwrap().len(), 1);
        assert!(
            target.duplicate_volume(volume.id).unwrap_err().to_string().contains("unavailable")
        );
        source.delete_volume(volume.id).unwrap();
        source.acknowledge_account_record(&creation).unwrap();
        let deletion = source.pending_account_record(None).unwrap().unwrap();
        assert_ne!(deletion.key, creation.key);
        assert_eq!(source.account_record_dependencies(&deletion).unwrap(), [None, None]);
        target.stage_account_record(&deletion).unwrap().unwrap();
        target.reconcile_account_records().unwrap();
        target.stage_account_record(&creation).unwrap();
        target.reconcile_account_records().unwrap();
        assert!(target.volumes().unwrap().is_empty());
        assert!(target.volume(volume.id).is_err());
        drop(target);
        let target = LocalStore::open(directory.path().join("target")).unwrap();
        assert!(target.volume(volume.id).is_err());
        assert!(target.volumes().unwrap().is_empty());
    }

    #[test]
    fn deleted_volume_rejects_data_and_does_not_delete_its_duplicate() {
        // Explicit deletion fences the source while its independent copy remains usable.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Original", false).unwrap();
        let access = Access::owner(store.account.id);
        let blob = store.create_blob(&access, volume.id, "note", b"saved").unwrap();
        let copy = store.duplicate_volume(volume.id).unwrap();
        store.delete_volume(volume.id).unwrap();
        assert!(store.entries(&access, volume.id, None, 32).is_err());
        assert!(store.read_blob_entry(&access, volume.id, blob.id).is_err());
        assert!(store.duplicate_volume(volume.id).is_err());
        assert_eq!(store.read_blob_entry(&access, copy.id, blob.id).unwrap(), b"saved");
    }
}
