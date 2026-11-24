#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(5);

// Pause only the first edit after its authorized read and real redb write
// transaction begin. Channels have deadlines so a regressed global mutex
// produces a failed assertion, not a hung suite.
fn pause_first(store: &LocalStore) -> (mpsc::Receiver<Id>, mpsc::SyncSender<()>) {
    let (entered, entry) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let wait = Mutex::new(wait);
    let calls = AtomicUsize::new(0);
    *store.after_edit_read.lock().unwrap() = Some(Arc::new(move |volume| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            entered.send(volume).unwrap();
            wait.lock().unwrap().recv_timeout(DEADLINE).unwrap();
        }
    }));
    (entry, release)
}

#[test]
fn independent_volumes_have_overlapping_authorized_write_transactions() {
    let (_dir, store, first, access) = setup();
    let second = store.create_volume("Second", false).unwrap();
    store
        .create_database(
            &access,
            second.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
        )
        .unwrap();
    let a = store
        .create_object_with_fields(&access, data(&store, first.id), id(1), &title("old A"))
        .unwrap();
    let b = store
        .create_object_with_fields(&access, data(&store, second.id), id(1), &title("old B"))
        .unwrap();
    let (entered, entries) = mpsc::sync_channel(2);
    let (release_a, wait_a) = mpsc::sync_channel(1);
    let (release_b, wait_b) = mpsc::sync_channel(1);
    let waits = BTreeMap::from([(first.id, Mutex::new(wait_a)), (second.id, Mutex::new(wait_b))]);
    *store.after_edit_read.lock().unwrap() = Some(Arc::new(move |volume| {
        entered.send(volume).unwrap();
        waits[&volume].lock().unwrap().recv_timeout(DEADLINE).unwrap();
    }));
    std::thread::scope(|scope| {
        let one = scope.spawn(|| {
            store.edit_fields(&access, data(&store, first.id), a.object_id, &title("new A"))
        });
        let two = scope.spawn(|| {
            store.edit_fields(&access, data(&store, second.id), b.object_id, &title("new B"))
        });
        let entered_one = entries.recv_timeout(DEADLINE);
        let entered_two = entries.recv_timeout(DEADLINE);
        let _ = release_a.send(());
        let _ = release_b.send(());
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
        assert_eq!(
            BTreeSet::from([entered_one.unwrap(), entered_two.unwrap()]),
            BTreeSet::from([first.id, second.id])
        );
    });
    *store.after_edit_read.lock().unwrap() = None;
    assert_eq!(
        store.read_object(&access, data(&store, first.id), a.object_id).unwrap().fields[&id(2)]
            .values,
        title("new A")[&id(2)]
    );
    assert_eq!(
        store.read_object(&access, data(&store, second.id), b.object_id).unwrap().fields[&id(2)]
            .values,
        title("new B")[&id(2)]
    );
}

#[test]
fn same_volume_holds_one_operation_guard_through_read_modify_commit() {
    let (_dir, store, volume, access) = setup();
    let object = store
        .create_object_with_fields(&access, data(&store, volume.id), id(1), &title("old"))
        .unwrap();
    let (entered, release) = pause_first(&store);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            store.edit_fields(&access, data(&store, volume.id), object.object_id, &title("new"))
        });
        entered.recv_timeout(DEADLINE).unwrap();
        let lock = store.volume_operation(volume.id).unwrap();
        assert!(
            matches!(lock.try_lock(), Err(operation_gate::TryLockError::WouldBlock)),
            "the edit must retain its volume lock while its transaction is open"
        );
        let second = scope.spawn(|| {
            store.edit_fields(
                &access,
                data(&store, volume.id),
                object.object_id,
                &BTreeMap::from([(id(3), Some(vec![Value::Boolean(true)]))]),
            )
        });
        release.send(()).unwrap();
        first.join().unwrap().unwrap();
        let receipt = second.join().unwrap().unwrap();
        assert_eq!(receipt.fields.len(), 1, "receipt contains only the edited field");
        let updated =
            store.read_object(&access, data(&store, volume.id), object.object_id).unwrap();
        assert_eq!(
            updated.fields[&id(2)].values,
            title("new")[&id(2)],
            "second edit reads the first committed result"
        );
        assert_eq!(updated.fields[&id(3)].values, Some(vec![Value::Boolean(true)]));
    });
    *store.after_edit_read.lock().unwrap() = None;
    let value = store.read_object(&access, data(&store, volume.id), object.object_id).unwrap();
    assert_eq!(value.fields[&id(2)].values, title("new")[&id(2)]);
    assert_eq!(value.fields[&id(3)].values, Some(vec![Value::Boolean(true)]));
    assert!(store.volume_operation(volume.id).unwrap().try_lock().is_ok());
    // Idle locks remain in the bounded working set rather than being allocated
    // again when a different session takes its next turn.
    drop(store.volume_operation(id(200)).unwrap());
    let _last = store.volume_operation(id(201)).unwrap();
    assert!(
        store.volume_operations.lock().unwrap().iter().flatten().count() <= MAX_VOLUME_OPERATIONS
    );
}

#[test]
fn revocation_waits_for_admitted_commit_then_denies_the_next_operation() {
    let (_dir, store, root, owner) = setup();
    let object = store
        .create_object_with_fields(&owner, data(&store, root.id), id(1), &title("old"))
        .unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let access = Access::session_version(id(99), &session.id, &version);
    store.provision_version(&owner, root.id, &version).unwrap();
    store
        .set_permissions(
            &owner,
            root.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            },
        )
        .unwrap();
    let (entered, release) = pause_first(&store);
    std::thread::scope(|scope| {
        let edit = scope.spawn(|| {
            store.edit_fields(&access, data(&store, root.id), object.object_id, &title("admitted"))
        });
        entered.recv_timeout(DEADLINE).unwrap();
        assert!(matches!(
            store.volume_operation(session.root_volume).unwrap().try_lock(),
            Err(operation_gate::TryLockError::WouldBlock)
        ));
        let revoke =
            scope.spawn(|| store.revoke(&owner, session.root_volume, access.account, "revoked"));
        release.send(()).unwrap();
        edit.join().unwrap().unwrap();
        revoke.join().unwrap().unwrap();
    });
    *store.after_edit_read.lock().unwrap() = None;
    assert!(matches!(
        store.edit_fields(&access, data(&store, root.id), object.object_id, &title("denied")),
        Err(Error::ParticipantRevoked { volume, account })
            if volume == session.root_volume && account == access.account
    ));
    assert!(matches!(
        store.query_object_fields(&access, data(&store, root.id), id(1), None, 64, &[id(2)]),
        Err(Error::ParticipantRevoked { volume, account })
            if volume == session.root_volume && account == access.account
    ));
    assert_eq!(
        store.read_object(&owner, data(&store, root.id), object.object_id).unwrap().fields[&id(2)]
            .values,
        title("admitted")[&id(2)]
    );
}

#[test]
fn withdrawing_personal_consent_cannot_race_an_admitted_commit() {
    let (_dir, store, root, owner) = setup();
    let personal = store.create_volume("Personal", false).unwrap();
    store
        .create_database(
            &owner,
            personal.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
        )
        .unwrap();
    let object = store
        .create_object_with_fields(&owner, data(&store, personal.id), id(1), &title("old"))
        .unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    store
        .select_personal_volumes(
            &session.id,
            vec![PersonalVolumeGrant { volume: personal.id, write: true }],
        )
        .unwrap();
    let access = Access::session_version(owner.account, &session.id, &version);
    let (entered, release) = pause_first(&store);
    std::thread::scope(|scope| {
        let edit = scope.spawn(|| {
            store.edit_fields(
                &access,
                data(&store, personal.id),
                object.object_id,
                &title("admitted"),
            )
        });
        entered.recv_timeout(DEADLINE).unwrap();
        assert!(matches!(
            store.personal_consent.try_write(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        let consent = scope.spawn(|| store.select_personal_volumes(&session.id, vec![]));
        release.send(()).unwrap();
        edit.join().unwrap().unwrap();
        consent.join().unwrap().unwrap();
    });
    *store.after_edit_read.lock().unwrap() = None;
    assert!(matches!(
        store.edit_fields(&access, data(&store, personal.id), object.object_id, &title("denied")),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(
        store.schema(&access, data(&store, personal.id)),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(
        store.read_object(&owner, data(&store, personal.id), object.object_id).unwrap().fields
            [&id(2)]
            .values,
        title("admitted")[&id(2)]
    );
}

#[test]
fn concurrent_creation_retains_every_index_entry_alongside_shared_edits_and_queries() {
    let (_dir, store, volume, access) = setup();
    let original = store
        .create_object_with_fields(&access, data(&store, volume.id), id(1), &title("old"))
        .unwrap();
    let (entered, release) = pause_first(&store);
    std::thread::scope(|scope| {
        let edit = scope.spawn(|| {
            store.edit_fields(
                &access,
                data(&store, volume.id),
                original.object_id,
                &title("edited"),
            )
        });
        entered.recv_timeout(DEADLINE).unwrap();
        assert!(matches!(
            store.volume_operation(volume.id).unwrap().try_lock(),
            Err(operation_gate::TryLockError::WouldBlock)
        ));
        let create = |prefix: &str| {
            (0..8)
                .map(|index| {
                    store
                        .create_object_with_fields(
                            &access,
                            data(&store, volume.id),
                            id(1),
                            &title(&format!("{prefix}-{index}")),
                        )
                        .unwrap()
                        .object_id
                })
                .collect::<Vec<_>>()
        };
        let first = scope.spawn(move || create("first"));
        let second = scope.spawn(move || create("second"));
        let queries = scope.spawn(|| {
            for _ in 0..8 {
                let page = store
                    .query_object_fields(
                        &access,
                        data(&store, volume.id),
                        id(1),
                        None,
                        64,
                        &[id(2)],
                    )
                    .unwrap();
                let found = page
                    .objects
                    .iter()
                    .find(|object| object.object_id == original.object_id)
                    .unwrap();
                assert_eq!(found.fields[&id(2)].values, title("edited")[&id(2)]);
                assert!(page.next.is_none());
            }
        });
        release.send(()).unwrap();
        edit.join().unwrap().unwrap();
        let mut expected = first.join().unwrap();
        expected.extend(second.join().unwrap());
        expected.push(original.object_id);
        expected.sort();
        queries.join().unwrap();
        let actual: Vec<_> = store
            .query_objects(&access, data(&store, volume.id), id(1), None, 64)
            .unwrap()
            .objects
            .into_iter()
            .map(|object| object.object_id)
            .collect();
        assert_eq!(actual, expected);
    });
    *store.after_edit_read.lock().unwrap() = None;
}

#[test]
fn a_poisoned_volume_lock_is_not_replaced_by_registry_cleanup() {
    let (_dir, store, volume, _) = setup();
    std::thread::scope(|scope| {
        let poisoned = scope.spawn(|| {
            let operation = store.volume_operation(volume.id).unwrap();
            let _guard = operation.lock().unwrap();
            panic!("test interrupted local operation");
        });
        assert!(poisoned.join().is_err());
    });
    assert!(matches!(store.volume_operation(id(200)), Err(Error::Internal(_))));
    assert!(matches!(store.volume_operation(volume.id), Err(Error::Internal(_))));
}

#[test]
fn contended_cache_gate_waits_then_reuses_the_same_backend() {
    let (_dir, store, volume, _) = setup();
    let expected = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let path = store
        .path
        .join(crate::paths::VOLUMES)
        .join(volume.id.to_string())
        .join(crate::store::DatabaseId::Index.file_name());
    let held = store.open.lock().unwrap();
    let (started, start) = mpsc::sync_channel(1);
    let (completed, completion) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            started.send(()).unwrap();
            completed
                .send(store.cached_db(
                    DatabaseKey::Volume(volume.id, crate::store::DatabaseId::Index),
                    &path,
                    None,
                ))
                .unwrap();
        });
        start.recv_timeout(DEADLINE).unwrap();
        assert!(
            completion.recv_timeout(Duration::from_millis(50)).is_err(),
            "cache admission cannot finish while another thread holds its gate"
        );
        drop(held);
        let reopened = completion.recv_timeout(DEADLINE).unwrap().unwrap();
        assert!(Arc::ptr_eq(&expected, &reopened));
    });
}

#[test]
fn independent_session_progress_does_not_wait_for_a_profile_writer() {
    let (_dir, store, root, owner) = setup();
    let other = store.create_volume("Independent", false).unwrap();
    store
        .create_database(
            &owner,
            other.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
        )
        .unwrap();
    let a =
        store.create_object_with_fields(&owner, data(&store, root.id), id(1), &title("A")).unwrap();
    let b = store
        .create_object_with_fields(&owner, data(&store, other.id), id(1), &title("B"))
        .unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    let first = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let second = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            other.id,
        )
        .unwrap();
    let first_access = Access::session(owner.account, &first.id);
    let second_access = Access::session(owner.account, &second.id);
    let (entered, release) = pause_first(&store);
    let store = &store;
    std::thread::scope(|scope| {
        let edit = scope.spawn(|| {
            store.edit_fields(&first_access, data(store, root.id), a.object_id, &title("first"))
        });
        entered.recv_timeout(DEADLINE).unwrap();
        let profile = store.lock_profile().unwrap();
        let (started, start) = mpsc::sync_channel(1);
        let writer = scope.spawn(move || {
            started.send(()).unwrap();
            store.create_service_identity("Concurrent profile write").map(|_| ())
        });
        start.recv_timeout(DEADLINE).unwrap();
        let (completed, completion) = mpsc::sync_channel(1);
        let unrelated = scope.spawn(move || {
            let result = store
                .edit_fields(&second_access, data(store, other.id), b.object_id, &title("second"))
                .and_then(|_| {
                    store.query_object_fields(
                        &second_access,
                        data(store, other.id),
                        id(1),
                        None,
                        64,
                        &[id(2)],
                    )
                });
            completed.send(result).unwrap();
        });
        let result = completion.recv_timeout(DEADLINE);
        drop(profile);
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        edit.join().unwrap().unwrap();
        unrelated.join().unwrap();
        let page = result.expect("unrelated input/query waited for bulk or profile lock").unwrap();
        assert_eq!(page.objects[0].fields[&id(2)].values, title("second")[&id(2)]);
    });
}

#[test]
fn dependency_ids_are_sorted_deduplicated_and_retained_through_waiters() {
    let (_dir, store, _, _) = setup();
    let operations = store.operations(&[id(20), id(10), id(20)]).unwrap();
    assert_eq!(operations.handles.iter().flatten().count(), 2);
    let first = store.volume_operation(id(10)).unwrap();
    let second = store.volume_operation(id(20)).unwrap();
    assert!(Arc::ptr_eq(operations.handles[0].as_ref().unwrap(), &first));
    assert!(Arc::ptr_eq(operations.handles[1].as_ref().unwrap(), &second));
    let store = &store;
    std::thread::scope(|scope| {
        let guards = operations.lock().unwrap();
        let (started, start) = mpsc::sync_channel(1);
        let (finished, finish) = mpsc::sync_channel(1);
        let waiter = scope.spawn(move || {
            let reverse = store.operations(&[id(20), id(10)]).unwrap();
            started.send(()).unwrap();
            let _guards = reverse.lock().unwrap();
            finished.send(()).unwrap();
        });
        start.recv_timeout(DEADLINE).unwrap();
        assert!(finish.try_recv().is_err());
        drop(guards);
        finish.recv_timeout(DEADLINE).unwrap();
        waiter.join().unwrap();
    });
    assert!(matches!(store.operations(&[id(1), id(2), id(3), id(4)]), Err(Error::LimitExceeded)));
}

#[test]
fn retained_handles_consume_registry_capacity_until_released() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let identity = |n: u16| {
        let mut bytes = [0; 16];
        bytes[..2].copy_from_slice(&n.to_le_bytes());
        Id::from_bytes(bytes)
    };
    let mut retained = (0..MAX_VOLUME_OPERATIONS)
        .map(|n| store.volume_operation(identity(n as u16)).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        store.volume_operations.lock().unwrap().iter().flatten().count(),
        MAX_VOLUME_OPERATIONS
    );
    assert!(matches!(store.volume_operation(identity(300)), Err(Error::LocalContention(_))));
    // Existing handles remain available when the registry is full.
    assert!(Arc::ptr_eq(&store.volume_operation(identity(0)).unwrap(), &retained[0]));
    retained.pop();
    let replacement = store.volume_operation(identity(300)).unwrap();
    assert!(replacement.try_lock().is_ok());
}

#[test]
fn denial_and_adoption_serialize_with_the_same_session_dependency() {
    for deny in [false, true] {
        let (_dir, store, root, owner) = setup();
        let object = store
            .create_object_with_fields(&owner, data(&store, root.id), id(1), &title("old"))
            .unwrap();
        let version = crate::identifier::encode(&[0xab; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                root.id,
            )
            .unwrap();
        let access = Access::session(owner.account, &session.id);
        let mut candidate = session.clone();
        candidate.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
        candidate.selector = BuildSelector::Exact { build_id: candidate.resolved_build_id.clone() };
        let (entered, release) = pause_first(&store);
        std::thread::scope(|scope| {
            let edit = scope.spawn(|| {
                store.edit_fields(
                    &access,
                    data(&store, root.id),
                    object.object_id,
                    &title("admitted"),
                )
            });
            entered.recv_timeout(DEADLINE).unwrap();
            let session_lock = store.volume_operation(session.root_volume).unwrap();
            assert!(session_lock.try_lock().is_err());
            let mutation = scope.spawn(|| {
                if deny {
                    store.record_session_denial(&session, "revoked").map(|_| ())
                } else {
                    store.adopt_session(&session, &candidate)
                }
            });
            release.send(()).unwrap();
            edit.join().unwrap().unwrap();
            mutation.join().unwrap().unwrap();
        });
        *store.after_edit_read.lock().unwrap() = None;
        if deny {
            assert!(matches!(
                store.query_objects(&access, data(&store, root.id), id(1), None, 64),
                Err(Error::AccessDenied(_))
            ));
        } else {
            assert_eq!(
                store.session(&session.id).unwrap().resolved_build_id,
                candidate.resolved_build_id
            );
            assert_eq!(
                store.read_object(&access, data(&store, root.id), object.object_id).unwrap().fields
                    [&id(2)]
                    .values,
                title("admitted")[&id(2)]
            );
        }
    }
}

#[test]
#[cfg(target_vendor = "apple")]
fn native_dependency_lock_allocations_fit_the_store_budgets() {
    use std::mem::size_of;
    // Rust 1.97.1 sys/sync/mutex/pthread.rs owns one OnceBox<pal::Mutex>;
    // pal::Mutex contains pthread_mutex_t. ArcInner has strong/weak usize counts.
    // The queue RwLock owns no heap allocation; its wait node is on the stack.
    // Initialize native mutexes before publishing them, avoiding racing OnceBox
    // candidates. Include native allocations, not only the pointer wrappers.
    let native_mutex = size_of::<libc::pthread_mutex_t>();
    let arc_allocation = 2 * size_of::<usize>() + size_of::<Mutex<()>>();
    let registry = size_of::<Mutex<VolumeRegistry>>()
        + native_mutex
        + MAX_VOLUME_OPERATIONS * (arc_allocation + native_mutex);
    assert!(registry <= 32 * 1024, "registry and retained native locks: {registry}");
    let gates = 2 * (size_of::<Mutex<()>>() + native_mutex) + size_of::<RwLock<()>>();
    assert!(gates <= 256, "profile, cache and consent gates: {gates}");
    let scratch = size_of::<OperationLocks<'_>>()
        + size_of::<[Option<std::sync::MutexGuard<'_, ()>>; MAX_OPERATION_VOLUMES]>()
        + size_of::<[Option<Id>; MAX_OPERATION_VOLUMES]>();
    assert!(scratch <= 256, "fixed dependency scratch: {scratch}");
}

#[test]
fn repeated_operation_reuses_its_initialized_native_mutex() {
    let (_dir, store, volume, _) = setup();
    let lock = store.volume_operation(volume.id).unwrap();
    let previous = Arc::downgrade(&lock);
    drop(lock);
    let next = store.volume_operation(volume.id).unwrap();
    assert!(Arc::ptr_eq(&previous.upgrade().unwrap(), &next));
    drop(next);
    drop(store.volume_operation(id(200)).unwrap());
    assert!(previous.upgrade().is_some(), "another session must not evict the idle working set");
}

#[test]
fn alternating_roots_reuse_the_bounded_working_set() {
    let (_dir, store, _, _) = setup();
    let roots = [id(10), id(20), id(30), id(40)];
    let previous = roots.map(|root| Arc::downgrade(&store.volume_operation(root).unwrap()));
    for _ in 0..8 {
        for (root, prior) in roots.iter().zip(&previous) {
            let next = store.volume_operation(*root).unwrap();
            assert!(Arc::ptr_eq(&prior.upgrade().unwrap(), &next));
        }
    }
}

#[test]
fn registry_capacity_failure_does_not_partially_evict_or_acquire() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let identity = |n: u16| {
        let mut bytes = [0; 16];
        bytes[..2].copy_from_slice(&n.to_le_bytes());
        Id::from_bytes(bytes)
    };
    let mut retained = (0..MAX_VOLUME_OPERATIONS)
        .map(|n| store.volume_operation(identity(n as u16)).unwrap())
        .collect::<Vec<_>>();
    let idle = Arc::downgrade(retained.last().unwrap());
    retained.pop();
    assert!(matches!(
        store.operations(&[identity(300), identity(301)]),
        Err(Error::LocalContention(_))
    ));
    assert!(idle.upgrade().is_some(), "failed multi-volume admission must not evict a prefix");
    let next = store.volume_operation(identity(300)).unwrap();
    assert!(idle.upgrade().is_none(), "successful capacity admission evicts an unborrowed entry");
    assert!(next.try_lock().is_ok());
}

// Publishing a retained build upload and recording the pending operation that
// names it share the build publication lock, so a writer that finds the file already on
// disk cannot record work against an entry the publisher has renamed into place
// but not yet fsynced.
#[test]
fn a_reused_build_upload_waits_for_the_publisher_to_make_its_entry_durable() {
    let (dir, store, _, _) = setup();
    let bundle = dir.path().join("app.bundle");
    fs::write(&bundle, b"wasm").unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    let (renamed, publishing) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let wait = Mutex::new(wait);
    *store.before_build_fsync.lock().unwrap() = Some(Arc::new(move || {
        renamed.send(()).unwrap();
        wait.lock().unwrap().recv_timeout(DEADLINE).unwrap();
    }));
    let (recorded, records) = mpsc::sync_channel(1);
    let (reached, arrives) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let publisher = scope.spawn(|| store.queue_build_upload(&version, &bundle));
        publishing.recv_timeout(DEADLINE).unwrap();
        assert!(store.blob_cache.try_lock().is_ok(), "file publications can proceed");
        assert!(
            dir.path().join(crate::paths::BUILDS).join(&version).exists(),
            "renamed, not yet published"
        );
        scope.spawn(|| {
            reached.send(()).unwrap();
            recorded.send(store.queue_build_upload(&version, &bundle)).unwrap();
        });
        arrives.recv_timeout(DEADLINE).unwrap();
        assert!(
            records.recv_timeout(Duration::from_millis(250)).is_err(),
            "a reused retained file must not be recorded before its entry is durable"
        );
        release.send(()).unwrap();
        let first = publisher.join().unwrap().unwrap();
        let second = records.recv_timeout(DEADLINE).unwrap().unwrap();
        assert_eq!(first, second, "the second writer reuses the published record");
    });
    *store.before_build_fsync.lock().unwrap() = None;
    assert_eq!(
        store
            .pending_operations()
            .unwrap()
            .iter()
            .filter(|op| matches!(op.operation, PendingKind::UploadBuild { .. }))
            .count(),
        1
    );
}

#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
#[test]
fn async_backend_notifies_readers_after_the_edit_reaches_commit() {
    use crate::runtime::host::{FieldChange, Host, Value as RuntimeValue};
    use std::sync::atomic::AtomicBool;
    let (_dir, store, volume, access) = setup();
    let address = data(&store, volume.id);
    let object = store.create_object_with_fields(&access, address, id(1), &title("old")).unwrap();
    let (entered, release) = pause_first(&store);
    let store = Arc::new(store);
    let changed = Arc::new(AtomicBool::new(false));
    let mut backend = crate::engine::data_host::DataHost {
        store: store.clone(),
        access: access.clone(),
        changed: changed.clone(),
    };
    let edit = std::thread::spawn(move || {
        backend.edit_fields(
            crate::runtime::host::EntryAddress {
                volume_id: *address.volume.as_bytes(),
                entry_id: *address.entry.as_bytes(),
            },
            *object.object_id.as_bytes(),
            &[FieldChange {
                field_id: *id(2).as_bytes(),
                values: Some(vec![RuntimeValue::Text("committed".into())]),
            }],
        )
    });
    entered.recv_timeout(DEADLINE).unwrap();
    let premature = changed.load(Ordering::Acquire);
    // Release before asserting so a failure cannot strand the storage worker.
    release.send(()).unwrap();
    edit.join().unwrap().unwrap();
    assert!(!premature, "pre-commit invalidation can make a reader miss the durable edit");
    assert!(changed.load(Ordering::Acquire));
    assert_eq!(
        store.read_object(&access, address, object.object_id).unwrap().fields[&id(2)].values,
        title("committed")[&id(2)]
    );
}

#[test]
fn stale_session_candidate_cannot_replace_a_newer_committed_selection() {
    let (_dir, store, root, _) = setup();
    let first = crate::identifier::encode(&[0xab; 32]);
    let previous = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: first.clone() },
            &first,
            root.id,
        )
        .unwrap();
    let mut candidate = previous.clone();
    candidate.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
    candidate.selector = BuildSelector::Exact { build_id: candidate.resolved_build_id.clone() };
    store.adopt_session(&previous, &candidate).unwrap();
    let mut stale = previous.clone();
    stale.resolved_build_id = crate::identifier::encode(&[0xef; 32]);
    stale.selector = BuildSelector::Exact { build_id: stale.resolved_build_id.clone() };
    assert!(matches!(store.adopt_session(&previous, &stale), Err(Error::InvalidInput(_))));
    assert_eq!(store.session(&previous.id).unwrap(), candidate);
    store.record_session_denial(&candidate, "revoked before adoption").unwrap();
    assert!(matches!(store.adopt_session(&candidate, &stale), Err(Error::AccessDenied(_))));
    assert_eq!(store.session(&previous.id).unwrap(), candidate);
}
