#![allow(
    clippy::unwrap_used,
    reason = "Tests assert current entry-copy admission and durable data."
)]
use super::*;
use crate::store::object_contract::{Grant, Subject, Value};

fn fixture(service: bool) -> (tempfile::TempDir, Arc<LocalStore>, Access, Id) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let access = Access::owner(store.account.id);
    let volume = store.create_volume("Copies", service).unwrap().id;
    (directory, store, access, volume)
}

fn address(volume: Id, entry: EntryId) -> EntryAddress {
    EntryAddress { volume, entry }
}

#[test]
fn duplicates_live_subtrees_with_independent_typed_backings_and_inherited_permissions() {
    // Copy nested live data, then reopen and edit the copy to prove durable, independent identities.
    let (directory, store, access, volume) = fixture(true);
    let blob =
        store.create_blob(&access, volume, "Tree/Nested/file.bin", b"file contents").unwrap();
    let kv =
        store.create_database(&access, volume, "Tree/Preferences", EntryKind::KvDb {}).unwrap();
    store.kv_put(&access, address(volume, kv.id), b"theme", b"light").unwrap();
    let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    let crate::store::InitialDatabaseKind::ObjectDb { schema, .. } = &declaration.databases[0].kind
    else {
        panic!("object fixture")
    };
    let (ty, definition) = schema.types.iter().next().unwrap();
    let field = *definition.fields.iter().find(|(_, field)| field.name == "title").unwrap().0;
    let objects = store
        .create_database(
            &access,
            volume,
            "Tree/Nested/Objects",
            EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let record = store
        .create_object_with_fields(
            &access,
            address(volume, objects.id),
            *ty,
            &BTreeMap::from([(field, Some(vec![Value::Text("kept".into())]))]),
        )
        .unwrap();
    let log = store.create_bloblog(&access, volume, "Tree/Log").unwrap();
    store.bloblog_put(&access, volume, log.id, &[b"first".to_vec(), b"second".to_vec()]).unwrap();
    store
        .set_entry_permissions(
            &access,
            volume,
            blob.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(access.account)),
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    let tree = store.resolve_entry(&access, volume, "Tree").unwrap();
    let copy = store.duplicate_entry(&access, address(volume, tree.id)).unwrap();
    assert_eq!(copy.info.name, "Tree Copy");
    assert_ne!(copy.id, tree.id);
    let copied_file = store.resolve_entry(&access, volume, "Tree Copy/Nested/file.bin").unwrap();
    let copied_kv = store.resolve_entry(&access, volume, "Tree Copy/Preferences").unwrap();
    let copied_objects = store.resolve_entry(&access, volume, "Tree Copy/Nested/Objects").unwrap();
    let copied_log = store.resolve_entry(&access, volume, "Tree Copy/Log").unwrap();
    for (source, copied) in [
        (blob.id, copied_file.id),
        (kv.id, copied_kv.id),
        (objects.id, copied_objects.id),
        (log.id, copied_log.id),
    ] {
        assert_ne!(source, copied);
        assert_eq!(
            store.entry_details(&access, volume, copied).unwrap().permissions,
            EntryAcl::Inherit
        );
    }
    assert_eq!(store.read_blob_entry(&access, volume, copied_file.id).unwrap(), b"file contents");
    assert_eq!(
        store.kv_read(&access, address(volume, copied_kv.id), b"theme", 0, 64, None).unwrap().bytes,
        b"light"
    );
    assert_eq!(
        store.read_object(&access, address(volume, copied_objects.id), record.object_id).unwrap(),
        record
    );
    assert!(store.bloblog_get(&access, volume, copied_log.id, None).unwrap().entries.is_empty());
    let mut entries = store.bloblog_pending(&access, volume, copied_log.id).unwrap();
    // Pending entries are indexed by identity, so compare their payloads without that order.
    entries.sort_unstable_by(|a, b| a.data.cmp(&b.data));
    assert_eq!(
        entries.iter().map(|entry| entry.data.as_slice()).collect::<Vec<_>>(),
        [b"first".as_slice(), b"second".as_slice()]
    );
    assert!(entries.iter().all(|entry| entry.log == copied_log.id));
    assert_eq!(store.file_entries(&access, volume, Some(copy.id)).unwrap().len(), 3);
    store.kv_put(&access, address(volume, copied_kv.id), b"theme", b"dark").unwrap();
    assert_eq!(
        store.kv_read(&access, address(volume, kv.id), b"theme", 0, 64, None).unwrap().bytes,
        b"light"
    );
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.resolve_entry(&access, volume, "Tree Copy").unwrap().id, copy.id);
    assert_eq!(store.read_blob_entry(&access, volume, copied_file.id).unwrap(), b"file contents");
    assert_eq!(
        store.kv_read(&access, address(volume, copied_kv.id), b"theme", 0, 64, None).unwrap().bytes,
        b"dark"
    );
}

#[test]
fn live_copy_names_follow_the_effective_parent_and_preserve_file_extensions() {
    // Renames/moves and occupied siblings determine the fresh copy name, not birth metadata.
    let (_directory, store, access, volume) = fixture(false);
    let file = store.create_blob(&access, volume, "Old/original.txt", b"data").unwrap();
    store.rename_entry(&access, volume, file.id, "Moved/renamed.txt").unwrap();
    store.create_blob(&access, volume, "Moved/renamed Copy.txt", b"occupied").unwrap();
    let copy = store.duplicate_entry(&access, address(volume, file.id)).unwrap();
    assert_eq!(copy.info.name, "renamed Copy 2.txt");
    assert_eq!(
        store.resolve_entry(&access, volume, "Moved/renamed Copy 2.txt").unwrap().id,
        copy.id
    );
    assert!(store.resolve_entry(&access, volume, "Old/renamed Copy 2.txt").is_err());
    assert_eq!(store.read_blob_entry(&access, volume, copy.id).unwrap(), b"data");
    let unicode = format!("{}.bin", "😀".repeat(62));
    let file = store.create_blob(&access, volume, &unicode, b"unicode").unwrap();
    let copy = store.duplicate_entry(&access, address(volume, file.id)).unwrap();
    assert_eq!(copy.info.name, format!("{} Copy.bin", "😀".repeat(61)));
    assert!(copy.info.name.len() <= namespace::MAX_COMPONENT_BYTES);
    assert_eq!(store.read_blob_entry(&access, volume, copy.id).unwrap(), b"unicode");
}

#[test]
fn denied_descendant_and_destination_parent_fail_before_creating_a_copy() {
    // Preflight requires Read for every child and Write on the effective destination parent.
    let (_directory, store, access, volume) = fixture(false);
    let file = store.create_blob(&access, volume, "Outer/Tree/private", b"private").unwrap();
    let tree = store.resolve_entry(&access, volume, "Outer/Tree").unwrap();
    let outer = store.resolve_entry(&access, volume, "Outer").unwrap();
    store
        .set_entry_permissions(&access, volume, file.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(
        store.duplicate_entry(&access, address(volume, tree.id)),
        Err(Error::AccessDenied(_))
    ));
    assert!(store.resolve_entry(&access, volume, "Outer/Tree Copy").is_err());
    store.set_entry_permissions(&access, volume, file.id, EntryAcl::Inherit).unwrap();
    store
        .set_entry_permissions(
            &access,
            volume,
            outer.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(access.account)),
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    assert!(matches!(
        store.duplicate_entry(&access, address(volume, tree.id)),
        Err(Error::AccessDenied(_))
    ));
    assert!(store.resolve_entry(&access, volume, "Outer/Tree Copy").is_err());
    assert!(matches!(
        store.duplicate_entry(
            &Access::session(access.account, "untrusted"),
            address(volume, tree.id)
        ),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
fn copy_rechecks_read_after_preparation_and_reports_later_partial_directory_failure() {
    // Revoke at publication: leaves have no birth, while a committed directory is reported partial.
    for (directory, database) in [(false, false), (false, true), (true, false)] {
        let (_temporary, store, access, volume) = fixture(false);
        let path = if directory { "Tree/file" } else { "file" };
        let file = if database {
            store.create_database(&access, volume, path, EntryKind::KvDb {}).unwrap()
        } else {
            store.create_blob(&access, volume, path, b"private").unwrap()
        };
        let source = if directory {
            store.resolve_entry(&access, volume, "Tree").unwrap().id
        } else {
            file.id
        };
        let weak = Arc::downgrade(&store);
        let current_access = access.clone();
        let publications = std::sync::atomic::AtomicUsize::new(0);
        *store.entry_blobs.recovery_before_publish.lock().unwrap() = Some(Arc::new(move || {
            // A volume write also invalidates the root's preparation epoch.
            // Revoke only after its birth, while the first child is prepared.
            if directory && publications.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                return Ok(());
            }
            weak.upgrade().unwrap().set_entry_permissions(
                &current_access,
                volume,
                file.id,
                EntryAcl::Replace(Acl::default()),
            )
        }));
        let error = store.duplicate_entry(&access, address(volume, source)).unwrap_err();
        *store.entry_blobs.recovery_before_publish.lock().unwrap() = None;
        if directory {
            let partial = store.resolve_entry(&access, volume, "Tree Copy").unwrap();
            let message = error.to_string();
            assert!(message.contains("Copy partially completed as “Tree Copy”"));
            assert!(message.contains(&partial.id.to_string()));
            assert!(message.contains("1/2 entries copied"));
            assert!(store.file_entries(&access, volume, Some(partial.id)).unwrap().is_empty());
        } else {
            assert!(matches!(error, Error::AccessDenied(_)));
            assert!(store.resolve_entry(&access, volume, "file Copy").is_err());
        }
        assert_eq!(store.entry_blobs.active.load(std::sync::atomic::Ordering::Acquire), 0);
    }
}

#[test]
fn copy_preflight_rejects_an_overlong_descendant_path_without_a_partial_root() {
    // A valid full-length source path must not create an incomplete tree when the Copy suffix exceeds it.
    let (_directory, store, access, volume) = fixture(false);
    let component = "x".repeat(240);
    let path = vec![component.as_str(); 17].join("/");
    assert_eq!(path.len(), namespace::MAX_PATH_BYTES);
    store.create_blob(&access, volume, &path, b"deep").unwrap();
    let source = store.resolve_entry(&access, volume, &component).unwrap();
    assert!(matches!(
        store.duplicate_entry(&access, address(volume, source.id)),
        Err(Error::LimitExceeded)
    ));
    assert!(store.resolve_entry(&access, volume, &format!("{component} Copy")).is_err());
    assert_eq!(
        store
            .read_blob_entry(
                &access,
                volume,
                store.resolve_entry(&access, volume, &path).unwrap().id
            )
            .unwrap(),
        b"deep"
    );
}

#[test]
fn unavailable_blob_and_retired_source_cannot_be_reported_as_copied() {
    // Missing offline content and retired identities fail without publishing a usable copy.
    for retired in [false, true] {
        let (_directory, store, access, volume) = fixture(false);
        let file = store.create_blob(&access, volume, "file.bin", b"local").unwrap();
        if retired {
            store.remove_entry(&access, volume, file.id).unwrap();
        } else {
            let EntryKind::Blob { digest, .. } = file.info.kind else { panic!("blob fixture") };
            fs::remove_file(store.blob_path(&digest, false)).unwrap();
        }
        assert!(store.duplicate_entry(&access, address(volume, file.id)).is_err());
        assert!(store.resolve_entry(&access, volume, "file Copy.bin").is_err());
    }
}

#[test]
fn copy_name_collision_and_unicode_limits_are_bounded() {
    // Candidate fitting preserves valid UTF-8/extensions and terminates at the namespace bound.
    let mut candidates = Vec::new();
    let result = copy_name("archive.tar.gz", true, |candidate| {
        candidates.push(candidate.to_owned());
        Ok(candidates.len() < 3)
    })
    .unwrap();
    assert_eq!(
        candidates,
        ["archive.tar Copy.gz", "archive.tar Copy 2.gz", "archive.tar Copy 3.gz"]
    );
    assert_eq!(result, "archive.tar Copy 3.gz");
    assert_eq!(copy_name("Folder.ext", false, |_| Ok(false)).unwrap(), "Folder.ext Copy");
    assert_eq!(copy_name(".hidden", true, |_| Ok(false)).unwrap(), ".hidden Copy");
    assert_eq!(
        copy_name(&"é".repeat(127), true, |_| Ok(false)).unwrap(),
        format!("{} Copy", "é".repeat(125))
    );
    let mut attempts = 0;
    assert!(matches!(
        copy_name("name", true, |_| {
            attempts += 1;
            Ok(true)
        }),
        Err(Error::LimitExceeded)
    ));
    assert_eq!(attempts, namespace::MAX_ENTRIES + 1);
    assert!(matches!(
        copy_name(&format!("a.{}", "x".repeat(253)), true, |_| Ok(false)),
        Err(Error::LimitExceeded)
    ));
}
