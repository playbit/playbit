#![allow(clippy::unwrap_used, reason = "Tests assert identity-bound File publication and denial.")]
use super::*;

fn fixture() -> (tempfile::TempDir, Arc<LocalStore>, Access, Id) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Files", false).unwrap().id;
    (directory, store, access, volume)
}

#[test]
fn open_creates_parents_but_discard_does_not_publish_a_file() {
    let (_directory, store, access, volume) = fixture();
    let destination = store.file_destination(&access, volume, None, "a/b/file", false).unwrap();
    let parent = store.resolve_entry(&access, volume, "a/b").unwrap();
    assert_eq!(destination.parent, Some(parent.id));
    assert!(store.resolve_entry(&access, volume, "a/b/file").is_err());
    drop(destination);
    let next = store.file_destination(&access, volume, Some(parent.id), "c/file", false).unwrap();
    store.publish_file(&access, &next, &mut &b"contents"[..], true).unwrap();
    let entry = store.file_entry(&access, volume, Some(parent.id), "c/file").unwrap();
    assert_eq!(store.read_blob_entry(&access, volume, entry.id).unwrap(), b"contents");
    assert_eq!(store.resolve_entry(&access, volume, "a/b/c/file").unwrap().id, entry.id);
}

#[test]
fn writer_follows_parent_move_but_never_delete_and_recreation() {
    let (_directory, store, access, volume) = fixture();
    let destination = store.file_destination(&access, volume, None, "folder/file", false).unwrap();
    let parent = destination.parent.unwrap();
    store.rename_entry(&access, volume, parent, "moved").unwrap();
    store.publish_file(&access, &destination, &mut &b"first"[..], true).unwrap();
    let entry = store.resolve_entry(&access, volume, "moved/file").unwrap();
    let replacement = store.file_destination(&access, volume, Some(parent), "file", false).unwrap();
    store.remove_entry(&access, volume, parent).unwrap();
    let fresh = store.file_destination(&access, volume, None, "moved/file", false).unwrap();
    assert_ne!(fresh.parent, Some(parent));
    let error = store.publish_file(&access, &replacement, &mut &b"lost"[..], true).unwrap_err();
    assert!(!error.publication_unknown);
    assert!(matches!(error.error, Error::NotFound("destination directory")));
    assert_eq!(store.read_blob_entry(&access, volume, entry.id).unwrap(), b"first");
    store.publish_file(&access, &fresh, &mut &b"new identity"[..], true).unwrap();
    assert_ne!(store.resolve_entry(&access, volume, "moved/file").unwrap().id, entry.id);
}

#[test]
fn absent_writers_keep_distinct_identities_and_exclusive_rechecks_at_close() {
    let (_directory, store, access, volume) = fixture();
    let one = store.file_destination(&access, volume, None, "file", false).unwrap();
    let two = store.file_destination(&access, volume, None, "file", false).unwrap();
    let exclusive = store.file_destination(&access, volume, None, "file", true).unwrap();
    let (first, second) = (one.entry, two.entry);
    assert_ne!(first, second);
    store.publish_file(&access, &one, &mut &b"one"[..], true).unwrap();
    store.publish_file(&access, &two, &mut &b"two"[..], true).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume, first).unwrap(), b"one");
    assert_eq!(store.read_blob_entry(&access, volume, second).unwrap(), b"two");
    let database = store.db(volume, crate::store::DatabaseId::Index).unwrap();
    let snapshot = database.begin_read().unwrap();
    let facts: Vec<_> = [first, second]
        .into_iter()
        .map(|id| {
            let (clock, bytes) =
                snapshot.get_with_clock(&volume_index::entry_key(id)).unwrap().unwrap();
            volume_index::namespace::source::Declaration::decode(id, clock, &bytes)
                .unwrap()
                .birth()
                .unwrap()
                .fact
        })
        .collect();
    let admitted = volume_index::namespace::admit(&facts).unwrap();
    assert!(
        admitted
            .events
            .values()
            .all(|state| *state == volume_index::namespace::EventState::Accepted)
    );
    assert!(admitted.view.entries[&first].visible && admitted.view.entries[&second].visible);

    let failure =
        store.publish_file(&access, &exclusive, &mut &b"exclusive"[..], true).unwrap_err();
    assert!(!failure.publication_unknown);
    assert!(matches!(failure.error, Error::AlreadyExists(_)));
    let existing = store.file_destination(&access, volume, None, "file", false).unwrap();
    assert!(!existing.create);
    assert!(existing.entry == first || existing.entry == second);

    // Exclusive creation also succeeds with a nonempty captured namespace context.
    drop(snapshot);
    let fresh = store.file_destination(&access, volume, None, "exclusive", true).unwrap();
    assert!(!fresh.creation_context.as_ref().unwrap().observed.is_empty());
    store.publish_file(&access, &fresh, &mut &b"exclusive"[..], true).unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "exclusive").unwrap().id, fresh.entry);
    assert_eq!(store.read_blob_entry(&access, volume, fresh.entry).unwrap(), b"exclusive");
}

#[test]
fn publication_rechecks_authority_after_backing_preparation() {
    let (_directory, store, access, volume) = fixture();
    let original = store.create_blob(&access, volume, "file", b"original").unwrap();
    let destination = store.file_destination(&access, volume, None, "file", false).unwrap();
    let hook_store = store.clone();
    let hook_access = access.clone();
    *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
        hook_store.set_entry_permissions(
            &hook_access,
            volume,
            original.id,
            EntryAcl::Replace(Acl::default()),
        )
    }));
    let failure = store.publish_file(&access, &destination, &mut &b"denied"[..], true).unwrap_err();
    assert!(!failure.publication_unknown);
    assert!(matches!(failure.error, Error::AccessDenied(_)));
    *store.entry_blobs.before_publish.lock().unwrap() = None;
    store.set_entry_permissions(&access, volume, original.id, EntryAcl::Inherit).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume, original.id).unwrap(), b"original");
}

#[test]
fn parent_move_during_backing_preparation_keeps_the_captured_destination() {
    let (_directory, store, access, volume) = fixture();
    let destination = store.file_destination(&access, volume, None, "folder/file", false).unwrap();
    let parent = destination.parent.unwrap();
    let hook_store = store.clone();
    let hook_access = access.clone();
    *store.entry_blobs.before_publish.lock().unwrap() =
        Some(Arc::new(move || hook_store.rename_entry(&hook_access, volume, parent, "moved")));
    store.publish_file(&access, &destination, &mut &b"complete"[..], true).unwrap();
    *store.entry_blobs.before_publish.lock().unwrap() = None;
    assert_eq!(store.resolve_entry(&access, volume, "moved/file").unwrap().info.name, "file");
    assert!(store.resolve_entry(&access, volume, "folder/file").is_err());
}

#[test]
fn publication_and_staging_reject_a_replaced_consent_before_engine_maintenance() {
    let (_directory, store, owner, volume) = fixture();
    let root = store.create_volume("App", true).unwrap();
    let version = crate::identifier::encode(&[0x11; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let grant = PersonalVolumeGrant { volume, write: true };
    store.select_personal_volumes(&session.id, vec![grant.clone()]).unwrap();
    let epoch = store.personal_volume_selections(&session.id).unwrap().unwrap()[0].incarnation;
    let access = Access::session_version(owner.account, &session.id, &version)
        .with_personal_incarnation(volume, epoch);
    let target = store.file_destination(&access, volume, None, "file", false).unwrap();
    let hook_store = store.clone();
    let hook_session = session.id.clone();
    *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
        hook_store.select_personal_volumes(&hook_session, vec![])?;
        hook_store.select_personal_volumes(&hook_session, vec![grant.clone()])
    }));
    // Regrant occurs after preflight and backing preparation, before the final
    // locked publication check. No engine invalidation callback runs here.
    let failure = store.publish_file(&access, &target, &mut &b"denied"[..], false).unwrap_err();
    assert!(!failure.publication_unknown);
    assert!(matches!(failure.error, Error::AccessDenied(_)));
    assert!(matches!(store.file_staging_access(&access, &target), Err(Error::AccessDenied(_))));
    assert!(matches!(store.file_directory(&access, volume, None), Err(Error::AccessDenied(_))));
    assert!(matches!(store.resolve_entry(&owner, volume, "file"), Err(Error::NotFound(_))));
    *store.entry_blobs.before_publish.lock().unwrap() = None;
    let next_epoch = store.personal_volume_selections(&session.id).unwrap().unwrap()[0].incarnation;
    assert_ne!(epoch, next_epoch);
    let fresh = Access::session_version(owner.account, &session.id, &version)
        .with_personal_incarnation(volume, next_epoch);
    let fresh_target = store.file_destination(&fresh, volume, None, "file", false).unwrap();
    store.publish_file(&fresh, &fresh_target, &mut &b"fresh"[..], false).unwrap();
}

fn receive_namespace_birth(store: &LocalStore, volume: Id, info: EntryInfo) -> EntryId {
    // An independently authored, authorized birth arrives with full-cut AP
    // evidence. No local create API can manufacture a concurrent duplicate.
    use volume_index::namespace::{observations, receipt, source};
    let id = EntryId::generate().unwrap();
    let db = store.db(volume, crate::store::DatabaseId::Index).unwrap();
    let mut tx = db.begin_write(now_unix_secs() + 1).unwrap();
    info.validate().unwrap();
    tx.put(&volume_index::entry_key(id), &crate::store::catalog::record::encode(&info).unwrap())
        .unwrap();
    volume_index::repair(&mut tx.local_indexes(), &[id]).unwrap();
    tx.commit().unwrap();
    let accepted = source::cut::accepted(&db.begin_read().unwrap()).unwrap();
    receipt::retain_fixture(&db, None, &accepted).unwrap();
    let observations = receipt::capture(&db.begin_read().unwrap(), None).unwrap();
    let mut tx = db.begin_write(now_unix_secs() + 1).unwrap();
    let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
    let mut after = before.clone();
    after.apply_receipt(&observations).unwrap();
    after.write_changes(&before, &mut tx.local_indexes()).unwrap();
    source::cut::project(&tx).unwrap().write_current(&mut tx.local_indexes()).unwrap();
    tx.commit().unwrap();
    id
}

#[test]
fn retained_directory_coalescence_preserves_original_acl_and_pending_writer_parent() {
    // Coalescence routes names, but cannot replace a retained handle's original
    // permission target or its private writer's open-time parent evidence.
    let (_directory, store, access, volume) = fixture();
    let pending = store.file_destination(&access, volume, None, "folder/child", false).unwrap();
    let original = pending.parent.unwrap();
    let mut duplicate = store.entry(&access, volume, original).unwrap().info;
    duplicate.created_revision = EntryId::generate().unwrap();
    duplicate.namespace.observed.clear();
    let canonical = receive_namespace_birth(&store, volume, duplicate);
    assert_eq!(store.file_entry(&access, volume, None, "folder").unwrap().id, canonical);
    store
        .set_entry_permissions(&access, volume, original, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(
        store.file_entries(&access, volume, Some(original)),
        Err(Error::AccessDenied(_))
    ));
    assert!(store.file_entries(&access, volume, Some(canonical)).unwrap().is_empty());
    assert!(matches!(
        store.rename_entry(&access, volume, original, "renamed"),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(
        store.publish_file(&access, &pending, &mut &b"denied"[..], true).unwrap_err().error,
        Error::AccessDenied(_)
    ));
    store.set_entry_permissions(&access, volume, original, EntryAcl::Inherit).unwrap();
    store.publish_file(&access, &pending, &mut &b"kept"[..], true).unwrap();
    let child = store.file_entry(&access, volume, Some(original), "child").unwrap();
    assert_eq!(child.id, pending.entry);
    assert_eq!(child.info.namespace.parent, Some(canonical));
    assert_eq!(
        store.entry(&access, volume, child.id).unwrap().info.namespace.parent,
        Some(original)
    );
    let replacement =
        store.file_destination(&access, volume, Some(original), "child", false).unwrap();
    store.publish_file(&access, &replacement, &mut &b"updated"[..], true).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume, child.id).unwrap(), b"updated");
    store.rename_entry(&access, volume, original, "renamed").unwrap();
    assert_eq!(store.file_entry(&access, volume, None, "renamed/child").unwrap().id, child.id);
}

#[test]
fn projected_resurrection_reopens_the_same_directory_for_new_child_publication() {
    // A concurrent child restores the deleted parent's identity. Original ACLs
    // still apply, and the next local creation observes the restoring child.
    let (_directory, store, access, volume) = fixture();
    let pending = store.file_destination(&access, volume, None, "folder/old", false).unwrap();
    store.publish_file(&access, &pending, &mut &b"old"[..], true).unwrap();
    let parent = pending.parent.unwrap();
    let mut late = store.entry(&access, volume, parent).unwrap().info;
    late.namespace.parent = Some(parent);
    late.namespace.automatic = false;
    late.name = "late".into();
    late.created_revision = EntryId::generate().unwrap();
    let db = store.db(volume, crate::store::DatabaseId::Index).unwrap();
    let read = db.begin_read().unwrap();
    let (clock, bytes) = read.get_with_clock(&volume_index::entry_key(parent)).unwrap().unwrap();
    let birth =
        volume_index::namespace::source::Declaration::decode(parent, clock, &bytes).unwrap();
    late.namespace.observed.push(birth.witness().id().unwrap());
    late.namespace.observed.sort();
    drop(bytes);
    drop(read);
    store.remove_entry(&access, volume, parent).unwrap();
    assert!(store.file_directory(&access, volume, Some(parent)).is_err());
    let late_id = receive_namespace_birth(&store, volume, late);
    store.file_directory(&access, volume, Some(parent)).unwrap();
    let details = store.entry_details(&access, volume, parent).unwrap();
    assert!(!details.entry.deleted);
    assert!(details.readable && details.writable);
    assert!(!details.recoverable);
    assert!(store.file_entry(&access, volume, Some(parent), "old").is_err());
    let next = store.file_destination(&access, volume, Some(parent), "new", false).unwrap();
    store.publish_file(&access, &next, &mut &b"new"[..], true).unwrap();
    assert_eq!(store.file_entry(&access, volume, Some(parent), "new").unwrap().id, next.entry);
    let list = store.file_entries(&access, volume, Some(parent)).unwrap();
    assert_eq!(list.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), ["late", "new"]);
    store.rename_entry(&access, volume, parent, "restored").unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "restored").unwrap().id, parent);
    assert_eq!(store.entry_details(&access, volume, parent).unwrap().entry.info.name, "restored");
    assert_eq!(store.file_entry(&access, volume, None, "restored/new").unwrap().id, next.entry);
    assert!(store.file_entry(&access, volume, None, "restored/old").is_err());
    // Resurrection does not clear prior deletions or replace the original ACL.
    // A subsequent deletion observes and removes the surviving children.
    store
        .set_entry_permissions(&access, volume, parent, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(store.remove_entry(&access, volume, parent), Err(Error::AccessDenied(_))));
    let details = store.entry_details(&access, volume, parent).unwrap();
    assert!(!details.entry.deleted);
    assert!(!details.readable && !details.writable);
    store.set_entry_permissions(&access, volume, parent, EntryAcl::Inherit).unwrap();
    store.remove_entry(&access, volume, parent).unwrap();
    assert!(matches!(store.file_directory(&access, volume, Some(parent)), Err(Error::NotFound(_))));
    let details = store.entry_details(&access, volume, parent).unwrap();
    assert!(details.entry.deleted);
    assert!(!details.readable && !details.writable);
    let hidden_child = store.entry_details(&access, volume, late_id).unwrap();
    assert!(hidden_child.entry.deleted);
    assert!(!hidden_child.recoverable);
    assert!(matches!(
        store.file_entry(&access, volume, None, "restored/new"),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        store.file_entry(&access, volume, None, "restored/late"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn nested_pending_writer_keeps_the_original_directory_acl() {
    // Resolving or creating a deeper parent cannot drop the original retained
    // directory's permission constraint while private bytes remain open.
    let (_directory, store, access, volume) = fixture();
    let initial = store.file_destination(&access, volume, None, "folder/file", false).unwrap();
    let original = initial.parent.unwrap();
    let nested =
        store.file_destination(&access, volume, Some(original), "nested/file", false).unwrap();
    assert_ne!(nested.parent, Some(original));
    store
        .set_entry_permissions(&access, volume, original, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(store.file_staging_access(&access, &nested), Err(Error::AccessDenied(_))));
    assert!(matches!(
        store.publish_file(&access, &nested, &mut &b"private"[..], true).unwrap_err().error,
        Error::AccessDenied(_)
    ));
    store.set_entry_permissions(&access, volume, original, EntryAcl::Inherit).unwrap();
    // The retained authority survives deletion of the original directory when
    // the captured destination parent has moved out and remains visible.
    let db = store.db(volume, crate::store::DatabaseId::Index).unwrap();
    let mut tx = db.begin_write(now_unix_secs()).unwrap();
    volume_index::move_entry(
        &mut tx,
        nested.parent.unwrap(),
        None,
        None,
        EntryId::generate().unwrap(),
        &volume_index::namespace::Origin { installation: store.installation(), label: None },
    )
    .unwrap();
    tx.commit().unwrap();
    store.remove_entry(&access, volume, original).unwrap();
    store.file_staging_access(&access, &nested).unwrap();
    store.publish_file(&access, &nested, &mut &b"allowed"[..], true).unwrap();
    assert_eq!(store.file_entry(&access, volume, None, "nested/file").unwrap().id, nested.entry);
}

#[test]
fn existing_writer_survives_generated_conflict_name_reclamation() {
    // A conflict label changing back to the requested name is not a rename.
    // The opened writer still targets the exact file and its attachment events.
    let (_directory, store, access, volume) = fixture();
    let first = store.file_destination(&access, volume, None, "file", false).unwrap();
    let second = store.file_destination(&access, volume, None, "file", false).unwrap();
    store.publish_file(&access, &first, &mut &b"first"[..], true).unwrap();
    store.publish_file(&access, &second, &mut &b"second"[..], true).unwrap();
    let entries = store.file_entries(&access, volume, None).unwrap();
    assert_eq!(entries.len(), 2);
    let loser = entries.iter().find(|entry| entry.name != "file").unwrap();
    let winner = entries.iter().find(|entry| entry.name == "file").unwrap();
    let writer = store.file_destination(&access, volume, None, &loser.name, false).unwrap();
    store.remove_entry(&access, volume, winner.id).unwrap();
    assert_eq!(store.file_entry(&access, volume, None, "file").unwrap().id, loser.id);
    store.publish_file(&access, &writer, &mut &b"retained"[..], true).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume, loser.id).unwrap(), b"retained");
    let committed = store.file_entry(&access, volume, None, "file").unwrap();
    assert_eq!(committed, store.resolve_entry(&access, volume, "file").unwrap());
    store.rename_entry(&access, volume, loser.id, "renamed").unwrap();
    assert!(store.file_entry(&access, volume, None, "file").is_err());
    let renamed = store.file_entry(&access, volume, None, "renamed").unwrap();
    assert_eq!(renamed.id, committed.id);
    assert_eq!(renamed.content_revision, committed.content_revision);
}

#[test]
fn file_read_uses_accepted_bytes_while_content_and_deletion_are_pending() {
    use volume_index::{BlobVersion, EntryAddress, EntryChange, EntryRevision};
    let (_directory, store, access, volume) = fixture();
    let original = store.create_blob(&access, volume, "file", b"accepted bytes").unwrap();
    let database = store.db(volume, crate::store::DatabaseId::Index).unwrap();
    let birth = {
        let read = database.begin_read().unwrap();
        let (clock, bytes) =
            read.get_with_clock(&volume_index::entry_key(original.id)).unwrap().unwrap();
        volume_index::namespace::source::Declaration::decode(original.id, clock, &bytes)
            .unwrap()
            .witness()
    };
    let mut observed = vec![birth.id().unwrap(), EntryId::generate().unwrap()];
    observed.sort_unstable();
    // Encoded index register types: content=8, retirement=1, immutable revision=3.
    // These are received source records, deliberately without qualified receipt
    // observations; a well-shaped payload is not causal admission.
    for (register, change) in [
        (
            8,
            EntryChange::Content {
                kind: EntryKind::Blob {
                    len_bytes: 999,
                    digest: [99; 32],
                    encrypted_key: [99; 32],
                    published_at_micros: 99,
                },
            },
        ),
        (1, EntryChange::Delete {}),
    ] {
        let mut tx = database.begin_write(now_unix_secs() + 2).unwrap();
        let revision = EntryId::generate().unwrap();
        let event = EntryRevision {
            revision,
            creation: birth.source_digest,
            origin_clock: tx.clock().as_u64(),
            origin: volume_index::namespace::Origin {
                installation: store.installation(),
                label: None,
            },
            observed: observed.clone(),
            change,
        };
        let bytes = crate::store::catalog::record::encode(&event).unwrap();
        let head = [&[volume_index::META_BYTE, register][..], original.id.as_bytes()].concat();
        let history =
            [&[volume_index::META_BYTE, 3][..], original.id.as_bytes(), revision.as_bytes()]
                .concat();
        volume_index::validate_record(&head, Some(&bytes)).unwrap();
        volume_index::validate_record(&history, Some(&bytes)).unwrap();
        tx.put(&head, &bytes).unwrap();
        tx.put(&history, &bytes).unwrap();
        volume_index::repair(&mut tx.local_indexes(), &[original.id]).unwrap();
        tx.commit().unwrap();
    }
    let file = store.file_entry(&access, volume, None, "file").unwrap();
    assert_eq!(file.content_revision, original.content_revision);
    let EntryKind::Blob { len_bytes, digest, encrypted_key, .. } = file.info.kind else { panic!() };
    let version = BlobVersion {
        address: EntryAddress { volume, entry: file.id },
        revision: file.content_revision,
        len_bytes,
        digest,
        encrypted_key,
    };
    let _lease = store.lease_blob_version(&access, &version).unwrap();
    assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"accepted bytes");
    let destination = store.file_destination(&access, volume, None, "file", false).unwrap();
    store.publish_file(&access, &destination, &mut &b"new accepted bytes"[..], true).unwrap();
    let current = store.file_entry(&access, volume, None, "file").unwrap();
    assert_ne!(current.content_revision, version.revision);
    let current =
        store.blob_version(&access, version.address, Some(current.content_revision)).unwrap();
    assert_eq!(store.read_blob_version(&access, &current).unwrap(), b"new accepted bytes");
    assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"accepted bytes");
    store
        .set_entry_permissions(&access, volume, original.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(store.file_entry(&access, volume, None, "file"), Err(Error::AccessDenied(_))));
    assert!(matches!(store.read_blob_version(&access, &version), Err(Error::AccessDenied(_))));
}

#[test]
fn file_close_rejects_missing_payloads_in_qualified_publication_context() {
    // An authenticated selected receipt can establish causal observations without
    // delivering ancestry. A visible unrelated File must not report successful
    // close when its newly authored content depends on those absent payloads.
    use volume_index::namespace::{observations, receipt, source};
    let (_directory, store, access, volume) = fixture();
    let original = store.create_blob(&access, volume, "file", b"accepted").unwrap();
    let destination = store.file_destination(&access, volume, None, "file", false).unwrap();
    let (_remote_directory, remote, remote_access, remote_volume) = fixture();
    let remote_entry = remote
        .create_database(&remote_access, remote_volume, "folder/selected", EntryKind::KvDb {})
        .unwrap();
    let remote_db = remote.db(remote_volume, crate::store::DatabaseId::Index).unwrap();
    let accepted = source::cut::accepted(&remote_db.begin_read().unwrap()).unwrap();
    let read = remote_db.begin_read().unwrap();
    let key = volume_index::entry_key(remote_entry.id);
    let (clock, bytes) = read.get_with_clock(&key).unwrap().unwrap();
    let selected = kvdb::Db::create(
        kvdb::storage::mem::MemStorage::new(),
        &kvdb::CreateOptions::syncable([7; 32], 0),
    )
    .unwrap();
    let mut tx = selected.begin_write(now_unix_secs()).unwrap();
    tx.put_with_clock(&key, &bytes, clock).unwrap();
    tx.commit().unwrap();
    receipt::retain_fixture(&selected, Some(remote_entry.id), &accepted).unwrap();
    let rows = receipt::capture(&selected.begin_read().unwrap(), Some(remote_entry.id)).unwrap();
    let database = store.db(volume, crate::store::DatabaseId::Index).unwrap();
    let mut tx = database.begin_write(now_unix_secs() + 1).unwrap();
    tx.put_with_clock(&key, &bytes, clock).unwrap();
    let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
    let mut after = before.clone();
    after.apply_receipt(&rows).unwrap();
    after.write_changes(&before, &mut tx.local_indexes()).unwrap();
    volume_index::repair(&mut tx.local_indexes(), &[remote_entry.id]).unwrap();
    tx.commit().unwrap();
    let read = database.begin_read().unwrap();
    assert!(
        !read.has(&volume_index::entry_key(remote_entry.info.namespace.parent.unwrap())).unwrap()
    );
    let root = read.root_hash().unwrap();
    let context = observations::Publication::capture(&read).unwrap();
    drop(read);
    assert_eq!(
        store.file_entry(&access, volume, None, "file").unwrap().content_revision,
        original.content_revision
    );
    let failure =
        store.publish_file(&access, &destination, &mut &b"pending"[..], true).unwrap_err();
    assert!(!failure.publication_unknown);
    assert!(matches!(
        failure.error,
        Error::InvalidInput("new namespace edit is not causally admitted")
    ));
    let read = database.begin_read().unwrap();
    assert_eq!(read.root_hash().unwrap(), root);
    assert_eq!(observations::Publication::capture(&read).unwrap(), context);
    assert_eq!(
        store.file_entry(&access, volume, None, "file").unwrap().content_revision,
        original.content_revision
    );
    assert_eq!(store.read_blob_entry(&access, volume, original.id).unwrap(), b"accepted");
}
