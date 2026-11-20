//! Publication, interruption recovery and idempotence through real disk stores.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::store::{MAX_ROOT_INITIAL_OBJECTS, MAX_ROOT_INITIALIZATION_BYTES};
const TYPE: Id = Id::from_bytes([1; 16]);
const TITLE: Id = Id::from_bytes([2; 16]);

#[test]
fn provider_creation_receipt_publishes_once_and_preserves_later_edits_after_interruption() {
    // Interrupt each durable boundary, recover, and replay the original request
    // after renaming/editing. Both credentials and user content must survive.
    for step in 1..=6 {
        let directory = tempfile::tempdir().unwrap();
        let id = Id::generate().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        INTERRUPT.set(step);
        assert!(store.provider_create_volume(id, "New Folder", true).is_err());
        INTERRUPT.set(0);
        assert_eq!(store.volumes().unwrap().len(), usize::from(step == 6));
        assert_eq!(
            store.metadata.begin_read().unwrap().has(provider_creation_key(id).as_bytes()).unwrap(),
            step == 6
        );
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        store.provider_create_volume(id, "New Folder", true).unwrap();
        let credentials = store.credentials(id).unwrap();
        let access = Access::owner(store.account.id);
        let entry = store.create_blob(&access, id, "file.txt", b"Retain my edit").unwrap();
        store.label_volume(&access, id, "Renamed").unwrap();
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.provider_create_volume(id, "New Folder", true).unwrap().name, "Renamed");
        let retained = store.credentials(id).unwrap();
        assert_eq!(retained.database_key, credentials.database_key);
        assert_eq!(retained.blob_key, credentials.blob_key);
        assert_eq!(store.read_blob_entry(&access, id, entry.id).unwrap(), b"Retain my edit");
        assert_eq!(store.pending_operations().unwrap().len(), 1);
        assert!(store.provider_create_volume(id, "Different request", true).is_err());
        store.delete_volume(id).unwrap();
        assert!(store.provider_create_volume(id, "New Folder", true).is_err());
    }
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let existing = store.create_volume("Existing", true).unwrap();
    assert!(store.provider_create_volume(existing.id, "Existing", true).is_err());
    assert!(store.provider_create_volume(Id::from_bytes([0; 16]), "New Folder", true).is_err());
}

#[test]
fn interrupted_independent_blob_seeds_publish_once_and_never_resurrect_retired_ids() {
    let mut declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/shared-note/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    declaration.blobs[0].bytes = b"Seeded note".to_vec();
    let entry = declaration.blobs[0].id;
    for step in 1..=6 {
        let directory = tempfile::tempdir().unwrap();
        let root = Id::generate().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        INTERRUPT.set(step);
        assert!(store.initialize_root(root, "Shared Note", false, &declaration).is_err());
        INTERRUPT.set(0);
        assert_eq!(store.credentials(root).is_ok(), step == 6);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        store.initialize_root(root, "Shared Note", false, &declaration).unwrap();
        let access = Access::owner(store.account().id);
        assert_eq!(store.read_blob_entry(&access, root, entry).unwrap(), b"Seeded note");
        let named = store.resolve_entry(&access, root, "note.txt").unwrap();
        assert!(matches!(
            named.info.kind,
            crate::store::volume_index::EntryKind::Blob { published_at_micros, .. } if published_at_micros > 0
        ));
        store.rename_entry(&access, root, entry, "renamed.txt").unwrap();
        store.write_blob(&access, root, entry, b"My edit").unwrap();
        store.initialize_root(root, "Shared Note", false, &declaration).unwrap();
        assert_eq!(store.read_blob_entry(&access, root, entry).unwrap(), b"My edit");
        assert_eq!(store.resolve_entry(&access, root, "renamed.txt").unwrap().id, entry);
        store.remove_entry(&access, root, entry).unwrap();
        store.initialize_root(root, "Shared Note", false, &declaration).unwrap();
        assert!(store.entry_details(&access, root, entry).unwrap().entry.deleted);
        assert!(matches!(store.resolve_entry(&access, root, "note.txt"), Err(Error::NotFound(_))));
        assert!(matches!(store.read_blob_entry(&access, root, entry), Err(Error::NotFound(_))));
    }
}
fn declaration() -> RootInitialization {
    crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session)
}
fn address(volume: Id) -> crate::store::volume_index::EntryAddress {
    crate::store::volume_index::EntryAddress { volume, entry: declaration().databases[0].id }
}
fn seeds_mut(root: &mut RootInitialization) -> &mut Vec<crate::store::InitialObject> {
    let crate::store::InitialDatabaseKind::ObjectDb { objects, .. } = &mut root.databases[0].kind
    else {
        panic!("fixture has one named object database");
    };
    objects
}
fn seeds(root: &RootInitialization) -> &Vec<crate::store::InitialObject> {
    let crate::store::InitialDatabaseKind::ObjectDb { objects, .. } = &root.databases[0].kind
    else {
        panic!("fixture has one named object database");
    };
    objects
}
fn resize_seeds(root: &mut RootInitialization, count: usize) {
    let seed = seeds(root)[0].clone();
    *seeds_mut(root) = (0..count)
        .map(|n| crate::store::InitialObject {
            id: Id::from_bytes([32 + n as u8; 16]),
            ..seed.clone()
        })
        .collect();
}
fn rows(store: &LocalStore, root: Id) -> Vec<ObjectFields> {
    store
        .query_objects(&Access::owner(store.account().id), address(root), TYPE, None, 64)
        .unwrap()
        .objects
}

#[test]
fn reopening_preserves_edits_and_registers_once() {
    // Reopen real files after editing; initialization must retain the exact
    // object identity and edit instead of appending or resetting seed records.
    let directory = tempfile::tempdir().unwrap();
    let root = Id::generate().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let declaration = declaration();
    store.initialize_root(root, "TODO", false, &declaration).unwrap();
    let object = rows(&store, root).remove(0);
    let access = Access::owner(store.account().id);
    store
        .edit_fields(
            &access,
            address(root),
            object.object_id,
            &BTreeMap::from([(TITLE, Some(vec![Value::Text("Keep my edit".into())]))]),
        )
        .unwrap();
    let edited = store.read_object(&access, address(root), object.object_id).unwrap();
    store.label_volume(&access, root, "My tasks").unwrap();
    store.set_permissions(&access, root, Acl::default()).unwrap();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.initialize_root(root, "TODO", false, &declaration).unwrap();
    assert_eq!(volume.name, "My tasks");
    assert_eq!(rows(&store, root), vec![edited]);
    assert_eq!(store.index(root).unwrap().permissions, Acl::default());
    assert_eq!(store.pending_operations().unwrap().len(), 1);
    assert!(!store.metadata.begin_read().unwrap().has(INTENT).unwrap());
}

#[test]
fn interruption_never_exposes_partial_root_and_retry_is_idempotent() {
    // Stop after each durable boundary, inspect visibility, then reopen and
    // retry. Only the final catalog commit exposes any credentials or records.
    for step in 1..=6 {
        let directory = tempfile::tempdir().unwrap();
        let root = Id::generate().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        INTERRUPT.set(step);
        assert!(store.initialize_root(root, "TODO", false, &declaration()).is_err());
        INTERRUPT.set(0);
        assert_eq!(store.volumes().unwrap().len(), usize::from(step == 6));
        assert_eq!(store.credentials(root).is_ok(), step == 6);
        assert_eq!(store.pending_operations().unwrap().len(), usize::from(step == 6));
        if step < 6 {
            assert!(store.db(root, crate::store::DatabaseId::Index).is_err());
            assert!(store.db(root, crate::store::DatabaseId::Entry(address(root).entry)).is_err());
        }
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        if step < 6 {
            assert!(!directory.path().join(crate::paths::VOLUMES).join(root.to_string()).exists());
        }
        store.initialize_root(root, "TODO", false, &declaration()).unwrap();
        assert_eq!(rows(&store, root).len(), 1);
        assert_eq!(store.pending_operations().unwrap().len(), 1);
    }
}

#[test]
fn retry_without_reopening_cleans_abandoned_work() {
    // A failed caller need not close the embedding host before trying again.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let abandoned = Id::generate().unwrap();
    INTERRUPT.set(4);
    assert!(store.initialize_root(abandoned, "TODO", false, &declaration()).is_err());
    INTERRUPT.set(0);
    let root = Id::generate().unwrap();
    store.initialize_root(root, "TODO", false, &declaration()).unwrap();
    assert!(!directory.path().join(abandoned.to_string()).exists());
    assert_eq!(store.volumes().unwrap().len(), 1);
}

#[test]
fn changed_declaration_and_existing_unmanaged_root_are_rejected() {
    // Neither a different initializer nor an ordinary existing volume may be
    // silently adopted. Both must retain their previously committed data.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let root = Id::generate().unwrap();
    store.initialize_root(root, "TODO", false, &declaration()).unwrap();
    let before = rows(&store, root);
    let mut changed = declaration();
    seeds_mut(&mut changed).clear();
    assert!(matches!(
        store.initialize_root(root, "TODO", false, &changed),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(rows(&store, root), before);
    let empty = store.create_volume("Empty", false).unwrap();
    assert!(matches!(
        store.initialize_root(empty.id, "TODO", false, &declaration()),
        Err(Error::AlreadyExists(_))
    ));
    assert!(
        store
            .entries(&Access::owner(store.account().id), empty.id, None, 64)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn invalid_field_and_oversized_declarations_leave_no_visible_root() {
    // Exercise admission through both JSON and typed inputs, then reject an
    // invalid later seed before creating any backing or initialization intent.
    assert!(matches!(
        RootInitialization::from_json(&vec![b' '; MAX_ROOT_INITIALIZATION_BYTES + 1]),
        Err(Error::LimitExceeded)
    ));
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let root = Id::generate().unwrap();
    let mut invalid = declaration();
    resize_seeds(&mut invalid, MAX_ROOT_INITIAL_OBJECTS + 1);
    assert!(matches!(
        store.initialize_root(root, "TODO", false, &invalid),
        Err(Error::LimitExceeded)
    ));
    invalid = declaration();
    seeds_mut(&mut invalid)[0]
        .fields
        .insert(TITLE, vec![Value::Text("a".repeat(MAX_ROOT_INITIALIZATION_BYTES))]);
    assert!(matches!(
        store.initialize_root(root, "TODO", false, &invalid),
        Err(Error::LimitExceeded)
    ));
    assert!(!store.metadata.begin_read().unwrap().has(INTENT).unwrap());
    invalid = declaration();
    let mut seed = seeds_mut(&mut invalid)[0].clone();
    seed.fields.insert(TITLE, vec![Value::Boolean(false)]);
    seed.id = Id::from_bytes([88; 16]);
    seeds_mut(&mut invalid).push(seed);
    assert!(store.initialize_root(root, "TODO", false, &invalid).is_err());
    assert!(store.volumes().unwrap().is_empty());
    assert!(!store.metadata.begin_read().unwrap().has(INTENT).unwrap());
    assert!(!directory.path().join(crate::paths::VOLUMES).join(root.to_string()).exists());
    store.initialize_root(root, "TODO", false, &declaration()).unwrap();
    assert_eq!(rows(&store, root).len(), 1);
}

#[test]
fn empty_and_maximum_roots_are_supported() {
    // Empty roots need no application database; the largest fixture publishes
    // every record together with one registration operation.
    for count in [0, MAX_ROOT_INITIAL_OBJECTS] {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let root = Id::generate().unwrap();
        let mut declaration = declaration();
        resize_seeds(&mut declaration, count);
        store.initialize_root(root, "TODO", false, &declaration).unwrap();
        assert_eq!(rows(&store, root).len(), count);
        assert_eq!(
            store
                .entries(&Access::owner(store.account().id), root, None, 64)
                .unwrap()
                .entries
                .len(),
            1
        );
        assert_eq!(store.pending_operations().unwrap().len(), 1);
    }
}

#[test]
fn concurrent_initialization_of_one_id_publishes_once() {
    // Two embedding callers with the same saved root ID serialize admission;
    // neither duplicates starter records or replaces the first credentials.
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let root = Id::generate().unwrap();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| store.initialize_root(root, "TODO", false, &declaration()));
        let second = scope.spawn(|| store.initialize_root(root, "TODO", false, &declaration()));
        assert_eq!(first.join().unwrap().unwrap(), second.join().unwrap().unwrap());
    });
    assert_eq!(rows(&store, root).len(), 1);
    assert_eq!(store.pending_operations().unwrap().len(), 1);
}

#[test]
fn corrupt_intent_does_not_erase_published_files() {
    // A contradictory recovery record must report corruption. It must never
    // use cleanup to conceal damage to a root already in the catalog.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let root = store.create_volume("Preserve", false).unwrap().id;
    let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
    tx.put(INTENT, &encode(&root).unwrap()).unwrap();
    tx.commit().unwrap();
    drop(store);
    assert!(matches!(LocalStore::open(directory.path()), Err(Error::Corrupt(_))));
    assert!(
        directory
            .path()
            .join(crate::paths::VOLUMES)
            .join(root.to_string())
            .join(crate::store::DatabaseId::Index.file_name())
            .exists()
    );
}

#[test]
#[ignore = "Run on the reference Mac in release mode; prints local preparation timing only."]
fn initialization_performance() {
    // Compare private batched seed publication with individual named database setters.
    // Profile opening, the UI, and scaffolding are outside this measured boundary.
    for count in [1, 32, 64] {
        let mut declaration = declaration();
        resize_seeds(&mut declaration, count);
        let mut samples = [Vec::new(), Vec::new(), Vec::new()];
        for _ in 0..100 {
            for mode in 0..2 {
                let directory = tempfile::tempdir().unwrap();
                let store = LocalStore::open(directory.path()).unwrap();
                let started = std::time::Instant::now();
                if mode == 0 {
                    let volume = store.create_volume("TODO", false).unwrap().id;
                    let access = Access::owner(store.account().id);
                    store
                        .set_permissions(&access, volume, declaration.permissions.clone())
                        .unwrap();
                    for declaration in &declaration.databases {
                        let crate::store::InitialDatabaseKind::ObjectDb { schema, objects } =
                            &declaration.kind
                        else {
                            panic!("TODO database fixture")
                        };
                        let entry = store
                            .create_database(
                                &access,
                                volume,
                                &declaration.name,
                                crate::store::volume_index::EntryKind::ObjectDb {
                                    schema: schema.clone(),
                                },
                            )
                            .unwrap();
                        let address =
                            crate::store::volume_index::EntryAddress { volume, entry: entry.id };
                        for seed in objects {
                            let fields = seed
                                .fields
                                .iter()
                                .map(|(id, values)| (*id, Some(values.clone())))
                                .collect();
                            store
                                .create_object_with_fields(&access, address, seed.type_id, &fields)
                                .unwrap();
                        }
                    }
                } else {
                    let root = Id::generate().unwrap();
                    store.initialize_root(root, "TODO", false, &declaration).unwrap();
                    samples[mode].push(started.elapsed().as_secs_f64() * 1000.0);
                    let reopened = std::time::Instant::now();
                    store.initialize_root(root, "TODO", false, &declaration).unwrap();
                    samples[2].push(reopened.elapsed().as_secs_f64() * 1000.0);
                    continue;
                }
                samples[mode].push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        for (mode, values) in samples.iter_mut().enumerate() {
            values.sort_by(f64::total_cmp);
            println!(
                "root rows={count} mode={mode} n=100 ms p50={} p95={} p99={} max={}",
                values[49], values[94], values[98], values[99]
            );
        }
    }
}

#[test]
fn publication_notifies_discovery_without_invalidating_other_replicas() {
    // New roots require initial discovery, not a lost-change signal for every
    // existing replica. Idempotent reopening must not add another feed record.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let mut receiver = store.take_change_receiver().unwrap();
    let before = receiver.drain().status;
    let root = Id::generate().unwrap();
    store.initialize_root(root, "TODO", false, &declaration()).unwrap();
    let batch = receiver.drain();
    assert_eq!(batch.status, before);
    assert_eq!(batch.records.len(), 1);
    assert_eq!(batch.records[0].volume(), root);
    assert_eq!(
        batch.records[0].kind(),
        crate::store::change_feed::CommittedChangeKind::VolumeAvailable
    );
    assert!(batch.records[0].bytes().is_empty());
    store.initialize_root(root, "TODO", false, &declaration()).unwrap();
    assert!(receiver.drain().records.is_empty());
}

#[test]
fn dedicated_app_root_binding_survives_interrupted_first_launch() {
    // The app profile saves its own new root ID before initial data. Restart
    // must reuse that ID and publish only one seed, including after interruption.
    for step in 1..=6 {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        INTERRUPT.set(step);
        assert!(store.prepare_app_root("TODO", &declaration()).is_err());
        INTERRUPT.set(0);
        let binding: AppRootBinding = required(&store.metadata, "local-app-root").unwrap();
        assert_eq!(binding.created, step == 6);
        let bound = binding.id;
        assert_eq!(store.volumes().unwrap().len(), usize::from(step == 6));
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.prepare_app_root("TODO", &declaration()).unwrap().id, bound);
        assert_eq!(store.prepare_app_root("TODO", &declaration()).unwrap().id, bound);
        assert_eq!(rows(&store, bound).len(), 1);
        let mut changed = declaration();
        seeds_mut(&mut changed).clear();
        assert_eq!(store.prepare_app_root("TODO", &changed).unwrap().id, bound);
        assert_eq!(rows(&store, bound).len(), 1);
    }
}
#[test]
fn dedicated_app_profiles_are_independent_and_refuse_unrelated_data() {
    // The same source installed in separate profiles creates distinct volume
    // identities with service keys shared. Never adopt an unrelated profile.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let a = LocalStore::open(first.path()).unwrap();
    let b = LocalStore::open(second.path()).unwrap();
    let ar = a.prepare_app_root("TODO", &declaration()).unwrap();
    let br = b.prepare_app_root("TODO", &declaration()).unwrap();
    assert_ne!(ar.id, br.id);
    assert_ne!(ar.owner, br.owner);
    assert!(ar.share_key_with_service);
    assert!(br.share_key_with_service);
    let unrelated = tempfile::tempdir().unwrap();
    let store = LocalStore::open(unrelated.path()).unwrap();
    store.create_volume("Existing", false).unwrap();
    assert!(store.prepare_app_root("TODO", &declaration()).is_err());
    assert!(!store.metadata.begin_read().unwrap().has(b"local-app-root").unwrap());
}
#[test]
fn concurrent_app_binding_selects_one_root() {
    // Both callers must first observe the same durable association, even before
    // either caller has published the volume it identifies.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| store.prepare_app_root("TODO", &declaration()));
        let second = scope.spawn(|| store.prepare_app_root("TODO", &declaration()));
        assert_eq!(first.join().unwrap().unwrap(), second.join().unwrap().unwrap());
    });
    assert_eq!(store.volumes().unwrap().len(), 1);
}

fn schema_mut(root: &mut RootInitialization, index: usize) -> &mut Schema {
    let crate::store::InitialDatabaseKind::ObjectDb { schema, .. } =
        &mut root.databases[index].kind
    else {
        panic!("fixture object database");
    };
    schema
}

fn add_field(root: &mut RootInitialization, index: usize) -> Id {
    let id = Id::from_bytes([91; 16]);
    schema_mut(root, index).types.get_mut(&TYPE).unwrap().fields.insert(
        id,
        crate::store::object_contract::FieldDeclaration {
            name: "annotation".into(),
            value_type: crate::store::object_contract::ValueType::Text,
            repeated: false,
            deprecated: false,
        },
    );
    id
}

#[test]
fn app_reopen_establishes_schema_without_replaying_creation_or_reversing_deprecation() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let mut original = declaration();
    resize_seeds(&mut original, 2);
    let root = store.prepare_app_root("TODO", &original).unwrap().id;
    let access = Access::owner(store.account().id);
    let initial = rows(&store, root);
    store
        .edit_fields(
            &access,
            address(root),
            initial[0].object_id,
            &BTreeMap::from([(TITLE, Some(vec![Value::Text("My task".into())]))]),
        )
        .unwrap();
    store.remove_object(&access, address(root), initial[1].object_id).unwrap();
    store.rename_entry(&access, root, address(root).entry, "my-tasks").unwrap();
    store.label_volume(&access, root, "My volume").unwrap();
    store.set_permissions(&access, root, Acl::default()).unwrap();
    let retained = rows(&store, root);
    let receipt = store
        .metadata
        .begin_read()
        .unwrap()
        .get(receipt_key(root).as_bytes())
        .unwrap()
        .unwrap()
        .to_vec();
    let mut updated = original.clone();
    let field = add_field(&mut updated, 0);
    schema_mut(&mut updated, 0)
        .types
        .get_mut(&TYPE)
        .unwrap()
        .fields
        .get_mut(&TITLE)
        .unwrap()
        .deprecated = true;
    seeds_mut(&mut updated).clear();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.prepare_app_root("New title", &updated).unwrap().name, "My volume");
    assert_eq!(rows(&store, root), retained);
    assert_eq!(store.resolve_entry(&access, root, "my-tasks").unwrap().id, address(root).entry);
    assert_eq!(store.index(root).unwrap().permissions, Acl::default());
    let established = store.schema(&access, address(root)).unwrap();
    assert!(established.types[&TYPE].fields.contains_key(&field));
    assert!(established.types[&TYPE].fields[&TITLE].deprecated);
    store.prepare_app_root("Old app", &original).unwrap();
    assert_eq!(store.schema(&access, address(root)).unwrap(), established);
    assert_eq!(rows(&store, root), retained);
    assert_eq!(
        store
            .metadata
            .begin_read()
            .unwrap()
            .get(receipt_key(root).as_bytes())
            .unwrap()
            .unwrap()
            .to_vec(),
        receipt
    );
    assert_eq!(store.pending_operations().unwrap().len(), 1);
}

#[test]
fn multi_database_schema_evolution_is_one_transaction_and_retryable() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let mut original = declaration();
    let mut second = original.databases[0].clone();
    second.id = EntryId::from_bytes([92; 15]).unwrap();
    second.name = "other".into();
    original.databases.push(second);
    let root = store.prepare_app_root("TODO", &original).unwrap().id;
    let access = Access::owner(store.account().id);
    let mut updated = original.clone();
    let field = add_field(&mut updated, 0);
    schema_mut(&mut updated, 1).types.get_mut(&TYPE).unwrap().name = "Renamed".into();
    let before = store.schema(&access, address(root)).unwrap();
    assert!(store.open_initialized_root(&access, root, &updated).is_err());
    assert_eq!(store.schema(&access, address(root)).unwrap(), before);
    updated.databases[1] = original.databases[1].clone();
    store.open_initialized_root(&access, root, &updated).unwrap();
    let after = store.schema(&access, address(root)).unwrap();
    assert!(after.types[&TYPE].fields.contains_key(&field));
    store.open_initialized_root(&access, root, &updated).unwrap();
    assert_eq!(store.schema(&access, address(root)).unwrap(), after);
}

#[test]
fn reopen_requires_current_authority_and_never_creates_a_template_addition() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let original = declaration();
    let root = store.prepare_app_root("TODO", &original).unwrap().id;
    let access = Access::owner(store.account().id);
    store.set_permissions(&access, root, Acl::default()).unwrap();
    let before = store.schema(&access, address(root)).unwrap();
    let mut updated = original.clone();
    add_field(&mut updated, 0);
    assert!(matches!(
        store.open_initialized_root(&Access::owner(Id::from_bytes([93; 16])), root, &updated),
        Err(Error::AccessDenied(_))
    ));
    let mut added = original.databases[0].clone();
    added.id = EntryId::from_bytes([94; 15]).unwrap();
    added.name = "new".into();
    updated.databases.push(added);
    assert!(matches!(
        store.open_initialized_root(&access, root, &updated),
        Err(Error::NotFound(_))
    ));
    assert_eq!(store.schema(&access, address(root)).unwrap(), before);
    assert!(matches!(store.resolve_entry(&access, root, "new"), Err(Error::NotFound(_))));
    store.remove_entry(&access, root, address(root).entry).unwrap();
    assert!(matches!(store.prepare_app_root("TODO", &original), Err(Error::NotFound(_))));
    assert!(store.entry_details(&access, root, address(root).entry).unwrap().entry.deleted);
}

#[test]
fn published_app_binding_refuses_missing_volume_or_receipt_without_reseeding() {
    for key in ["volume", "root-initialization"] {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let root = store.prepare_app_root("TODO", &declaration()).unwrap().id;
        let binding: AppRootBinding = required(&store.metadata, "local-app-root").unwrap();
        assert!(binding.created);
        let mut tx = store.metadata.begin_write(now_unix_secs()).unwrap();
        let key = if key == "volume" { format!("volume/{root}") } else { receipt_key(root) };
        tx.del(key.as_bytes()).unwrap();
        tx.commit().unwrap();
        assert!(matches!(store.prepare_app_root("TODO", &declaration()), Err(Error::NotFound(_))));
        assert!(!store.metadata.begin_read().unwrap().has(key.as_bytes()).unwrap());
    }
}
