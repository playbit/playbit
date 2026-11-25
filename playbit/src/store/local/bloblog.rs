use super::*;
use crate::store::{
    EntryId,
    bloblog::{self, Cursor, Entry, Log, Page},
    volume_index::{self, DatabaseProtocol, IndexSnapshot},
};

impl LocalStore {
    /// Authorize against the current index under the volume operation boundary.
    /// Empty cache reads are resolved by the online operation owner.
    fn with_bloblog<T>(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        permission: Permission,
        prepare: bool,
        f: impl FnOnce(Log<'_, kvdb::redb::RedbStorage>, &Database) -> Result<T>,
    ) -> Result<T> {
        let operations = self.access_operations(access, volume)?;
        let _guards = operations.lock()?;
        let policy = self.index(volume)?;
        self.check_authority(access, volume, Some(entry))?;
        if let Some(grant) = self.admit_access(access, &policy)?
            && !grant.write
            && permission == Permission::Write
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let index = self.db(volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        snapshot.authorize(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            entry,
            permission,
        )?;
        if snapshot.live_entry(entry)?.info.protocol != Some(DatabaseProtocol::BlobLog) {
            return Err(Error::InvalidInput("entry is not an append log"));
        }
        if permission == Permission::Write && !policy.volume().share_key_with_service {
            return Err(Error::Unsupported("append logs require service database replication"));
        }
        let ready = snapshot.backing_ready(entry)?;
        if snapshot.backing_quarantined(entry)? {
            return Err(Error::NotFound("accepted log backing is pending"));
        }
        if !ready && !prepare {
            return Err(Error::NotFound("append log backing is unavailable"));
        }
        drop(snapshot);
        let db = self.entry_backing(volume, entry, prepare && !ready)?;
        if prepare && !ready {
            super::initialization::sync_directory(
                &self.path.join(crate::paths::VOLUMES).join(volume.to_string()),
            )?;
        }
        f(Log { database: &db, volume, entry, encryption_key: None, now: now_unix_secs() }, &index)
    }

    pub fn bloblog_prepare(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        blobs: &[Vec<u8>],
    ) -> Result<Cursor> {
        bloblog::validate_blobs(blobs)?;
        self.with_bloblog(access, volume, log, Permission::Write, true, |store, _| {
            Ok(store.start_cursor())
        })
    }

    #[cfg(test)]
    pub(crate) fn bloblog_put(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        blobs: &[Vec<u8>],
    ) -> Result<Cursor> {
        let cursor = self.bloblog_prepare(access, volume, log, blobs)?;
        let entries = bloblog::append_entries(access.account, volume, log, Id::generate()?, blobs)?;
        self.bloblog_receive(access, volume, log, &entries)?;
        Ok(cursor)
    }

    pub fn bloblog_get(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        cursor: Option<&Cursor>,
    ) -> Result<Page> {
        self.with_bloblog(access, volume, log, Permission::Read, true, |store, _| {
            store.get_committed(cursor)
        })
    }

    /// Capture one private page and its source cut under the same admission gate.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn private_log_page(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        cursor: Option<&Cursor>,
    ) -> Result<bloblog::ReplicaPage> {
        if access.invitation_peer.is_none() {
            return Err(Error::AccessDenied("private log reader required"));
        }
        self.with_bloblog(access, volume, log, Permission::Read, false, |store, index| {
            Ok(bloblog::ReplicaPage {
                page: store.get_committed(cursor)?,
                source: self.source_snapshot_token(volume)?,
                policy_revision: MetadataPolicy::read(index)?.revision(),
                database_clock: store.database.clock().as_u64(),
            })
        })
    }

    /// A verified service page can initialize a read-only participant's backing.
    pub fn bloblog_receive(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        entries: &[Entry],
    ) -> Result<bool> {
        self.with_bloblog(access, volume, log, Permission::Read, true, |store, index| {
            let changed = store.apply(entries, false)?;
            if changed {
                // Record the durable content commit even if readiness bookkeeping
                // fails below. Receipt metadata alone must not invalidate guests.
                self.changes
                    .observed_database_changed(volume, crate::store::DatabaseId::Entry(log));
            }
            let mut tx = index.begin_write(now_unix_secs())?;
            volume_index::mark_backing_ready(&mut tx, log)?;
            tx.commit()?;
            Ok(changed)
        })
    }

    pub fn bloblog_pending(&self, access: &Access, volume: Id, log: EntryId) -> Result<Vec<Entry>> {
        self.with_bloblog(access, volume, log, Permission::Read, false, |store, _| store.pending())
    }

    pub fn bloblog_checkpoint(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
    ) -> Result<Option<Cursor>> {
        self.with_bloblog(access, volume, log, Permission::Read, true, |store, _| {
            get(store.database, b"remote-cursor")
        })
    }

    pub fn bloblog_set_checkpoint(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        cursor: &Cursor,
    ) -> Result<()> {
        self.bloblog_set_checkpoint_changed(access, volume, log, cursor).map(|_| ())
    }

    pub(crate) fn bloblog_set_checkpoint_changed(
        &self,
        access: &Access,
        volume: Id,
        log: EntryId,
        cursor: &Cursor,
    ) -> Result<bool> {
        self.with_bloblog(access, volume, log, Permission::Read, false, |store, _| {
            store.validate_cursor(cursor)?;
            if get::<Cursor>(store.database, b"remote-cursor")?.as_ref() == Some(cursor) {
                return Ok(false);
            }
            put(store.database, b"remote-cursor", cursor)?;
            #[cfg(any(test, feature = "file-provider"))]
            self.changes.payload_size_changed(volume);
            Ok(true)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    fn setup(share_key_with_service: bool) -> (tempfile::TempDir, LocalStore, Volume, Access) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let owner = Access::owner(store.account.id);
        let volume = store.create_volume("Root", share_key_with_service).unwrap();
        (dir, store, volume, owner)
    }

    #[test]
    fn read_only_participant_can_receive_and_checkpoint_but_not_append() {
        let (_dir, store, volume, owner) = setup(true);
        let reader = Id::generate().unwrap();
        store.grant_volume(&owner, volume.id, reader, false).unwrap();
        let reader_access = Access::owner(reader);

        let log = store.create_bloblog(&owner, volume.id, "edits").unwrap().id;
        store.bloblog_put(&owner, volume.id, log, &[b"hello".to_vec()]).unwrap();
        let page = store.bloblog_get(&owner, volume.id, log, None).unwrap();

        // A read-only participant can apply entries the service already
        // replicated and persist the resulting checkpoint...
        store.bloblog_receive(&reader_access, volume.id, log, &page.entries).unwrap();
        store.bloblog_set_checkpoint(&reader_access, volume.id, log, &page.cursor).unwrap();

        // ...but cannot append entries of their own.
        let error =
            store.bloblog_put(&reader_access, volume.id, log, &[b"nope".to_vec()]).unwrap_err();
        assert!(matches!(error, Error::AccessDenied(_)), "{error:?}");
    }

    #[test]
    fn received_entries_invalidate_only_their_content_scope_without_publication() {
        // Compare scoped delivery fences across a new receipt and its replay;
        // content must wake readers without changing authority or publishing again.
        let (_dir, store, volume, owner) = setup(true);
        let log = store.create_bloblog(&owner, volume.id, "edits").unwrap().id;
        let unrelated = store.create_volume("Other", true).unwrap().id;
        let entries = bloblog::append_entries(
            owner.account,
            volume.id,
            log,
            Id::generate().unwrap(),
            &[b"new entry".to_vec()],
        )
        .unwrap();
        let before = store.delivery_revision();
        let publication = store.publication_generation();
        assert!(store.bloblog_receive(&owner, volume.id, log, &entries).unwrap());
        let after = store.delivery_revision();
        assert_ne!(before, after);
        assert!(store.delivery_scope_revision(before, &[volume.id]).1.is_some());
        assert_eq!(store.delivery_scope_revision(before, &[unrelated]).1, None);
        assert_eq!(store.delivery_snapshot_revision(before, &[volume.id]).1, None);
        assert_eq!(store.publication_generation(), publication);
        assert!(!store.bloblog_receive(&owner, volume.id, log, &entries).unwrap());
        assert!(!store.bloblog_receive(&owner, volume.id, log, &[]).unwrap());
        assert_eq!(store.delivery_revision(), after);
        assert_eq!(store.publication_generation(), publication);
    }

    #[test]
    fn checkpoint_receipts_require_changed_authorized_commits() {
        // Checkpoint storage changes Size without invalidating guest data or
        // publishing again. Rejected and duplicate checkpoints must not notify.
        let (_dir, store, volume, owner) = setup(true);
        let log = store.create_bloblog(&owner, volume.id, "edits").unwrap().id;
        let other = store.create_bloblog(&owner, volume.id, "other").unwrap().id;
        store.bloblog_put(&owner, volume.id, log, &[b"retained".to_vec()]).unwrap();
        let page = store.bloblog_get(&owner, volume.id, log, None).unwrap();
        let source = store.delivery_revision();
        let publication = store.publication_generation();
        let receipt = store.delivery_provider_revision(None, None).0;
        assert!(store.bloblog_set_checkpoint(&owner, volume.id, other, &page.cursor).is_err());
        assert_eq!(store.delivery_provider_revision(None, None).0, receipt);

        store.bloblog_set_checkpoint(&owner, volume.id, log, &page.cursor).unwrap();
        let committed = store.delivery_provider_revision(None, None).0;
        assert_ne!(committed, receipt);
        assert_eq!(store.delivery_revision(), source);
        assert_eq!(store.publication_generation(), publication);
        assert_eq!(store.bloblog_get(&owner, volume.id, log, None).unwrap().entries, page.entries);
        store.bloblog_set_checkpoint(&owner, volume.id, log, &page.cursor).unwrap();
        assert_eq!(store.delivery_provider_revision(None, None).0, committed);

        store
            .set_entry_permissions(
                &owner,
                volume.id,
                log,
                EntryAcl::Replace(crate::store::object_contract::Acl::default()),
            )
            .unwrap();
        let denied = store.delivery_provider_revision(None, None).0;
        let source = store.delivery_revision();
        let publication = store.publication_generation();
        assert!(matches!(
            store.bloblog_set_checkpoint(&owner, volume.id, log, &page.cursor),
            Err(Error::AccessDenied(_))
        ));
        assert_eq!(store.delivery_provider_revision(None, None).0, denied);
        assert_eq!(store.delivery_revision(), source);
        assert_eq!(store.publication_generation(), publication);
    }

    #[test]
    fn named_log_overrides_replace_owner_content_permissions_and_isolate_backings() {
        use crate::store::object_contract::{Acl, Grant, Subject};
        let (_dir, store, volume, owner) = setup(true);
        let participant = Id::generate().unwrap();
        let reader = Access::owner(participant);
        store.grant_volume(&owner, volume.id, participant, true).unwrap();
        let first = store.create_bloblog(&owner, volume.id, "first").unwrap().id;
        let second = store.create_bloblog(&owner, volume.id, "second").unwrap().id;
        store.bloblog_put(&owner, volume.id, first, &[b"first".to_vec()]).unwrap();
        let first_page = store.bloblog_get(&owner, volume.id, first, None).unwrap();
        assert!(store.bloblog_receive(&owner, volume.id, second, &first_page.entries).is_err());
        assert!(store.bloblog_get(&owner, volume.id, second, None).unwrap().entries.is_empty());
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                first,
                EntryAcl::Replace(Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                }),
            )
            .unwrap();
        for access in [&owner, &reader] {
            assert_eq!(store.bloblog_get(access, volume.id, first, None).unwrap().entries.len(), 1);
            assert!(store.bloblog_put(access, volume.id, first, &[vec![1]]).is_err());
            assert!(store.rename_entry(access, volume.id, first, "renamed").is_err());
        }
        store.bloblog_put(&reader, volume.id, second, &[b"second".to_vec()]).unwrap();
        // Managing overrides remains an owner operation under the volume gate.
        assert!(store.set_entry_permissions(&reader, volume.id, first, EntryAcl::Inherit).is_err());
        store.set_entry_permissions(&owner, volume.id, first, EntryAcl::Inherit).unwrap();
        store.rename_entry(&owner, volume.id, first, "renamed").unwrap();
        store.bloblog_set_checkpoint(&reader, volume.id, first, &first_page.cursor).unwrap();
        assert_eq!(
            store.bloblog_checkpoint(&reader, volume.id, first).unwrap(),
            Some(first_page.cursor)
        );
    }

    #[test]
    fn unknown_or_retired_logs_never_publish_empty_backings_or_resurrect_data() {
        let (directory, store, volume, owner) = setup(true);
        let unknown = EntryId::generate().unwrap();
        assert!(store.bloblog_get(&owner, volume.id, unknown, None).is_err());
        assert!(store.bloblog_receive(&owner, volume.id, unknown, &[]).is_err());
        assert!(
            !directory
                .path()
                .join(crate::paths::VOLUMES)
                .join(volume.id.to_string())
                .join(format!("{unknown}.kvdb"))
                .exists()
        );
        let first = store.create_bloblog(&owner, volume.id, "edits").unwrap().id;
        store.bloblog_put(&owner, volume.id, first, &[b"retained".to_vec()]).unwrap();
        let page = store.bloblog_get(&owner, volume.id, first, None).unwrap();
        store.remove_entry(&owner, volume.id, first).unwrap();
        let recreated = store.create_bloblog(&owner, volume.id, "edits").unwrap().id;
        assert_ne!(first, recreated);
        assert!(store.bloblog_receive(&owner, volume.id, first, &page.entries).is_err());
        assert!(store.bloblog_receive(&owner, volume.id, recreated, &page.entries).is_err());
        assert!(store.bloblog_get(&owner, volume.id, recreated, None).unwrap().entries.is_empty());
        let retained = store.entry_backing(volume.id, first, false).unwrap();
        assert_eq!(
            Log {
                database: &retained,
                volume: volume.id,
                entry: first,
                encryption_key: None,
                now: now_unix_secs(),
            }
            .get(None)
            .unwrap()
            .entries,
            page.entries
        );
    }
}
