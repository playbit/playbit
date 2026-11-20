#![allow(clippy::unwrap_used, reason = "Tests assert successful snapshot fixtures.")]
use super::*;
use crate::store::EntryId;
use crate::store::volume_index::EntryKind;

fn setup() -> (tempfile::TempDir, LocalStore, Volume, Access) {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Archive", true).unwrap();
    (directory, store, volume, access)
}

fn write(store: &LocalStore, volume: Id, entry: EntryId, bytes: &[u8]) {
    store
        .with_replica(volume, DatabaseId::Entry(entry), |db| {
            let mut tx = db.begin_write(now_unix_secs())?;
            tx.put(b"payload", bytes)?;
            tx.local_indexes().put(b"local-checkpoint", b"retained")?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
}

fn read(snapshot: &VolumeSnapshot, entry: EntryId) -> Vec<u8> {
    snapshot
        .with_database(DatabaseId::Entry(entry), |tx| {
            assert_eq!(&*tx.local_get(b"local-checkpoint")?.unwrap(), b"retained");
            Ok(tx.get(b"payload")?.unwrap().to_vec())
        })
        .unwrap()
}

#[test]
fn captures_more_backings_than_cache_and_preserves_names_local_records_and_old_bytes() {
    let (_directory, store, volume, access) = setup();
    let mut entries = Vec::new();
    for number in 0u8..40 {
        let entry = store
            .create_database(&access, volume.id, &format!("folder/{number}"), EntryKind::KvDb {})
            .unwrap();
        write(&store, volume.id, entry.id, &[number]);
        entries.push(entry.id);
    }
    // Retired recovery backings count against the same bounded cache as live ones.
    for entry in entries.iter().skip(1).step_by(2) {
        store.remove_entry(&access, volume.id, *entry).unwrap();
    }
    let snapshot = store.capture_volume(&access, volume.id).unwrap();
    store.rename_entry(&access, volume.id, entries[0], "new/name").unwrap();
    write(&store, volume.id, entries[0], b"replacement");
    snapshot
        .with_database(DatabaseId::Index, |tx| {
            let index = IndexSnapshot::new(tx);
            assert_eq!(index.lookup("folder/0")?.id, entries[0]);
            assert!(matches!(index.lookup("new/name"), Err(Error::NotFound(_))));
            Ok(())
        })
        .unwrap();
    for (number, entry) in entries.into_iter().enumerate() {
        assert_eq!(read(&snapshot, entry), [number as u8]);
    }
    let path = snapshot.directory.0.clone();
    drop(snapshot);
    assert!(!path.exists());
}

#[test]
fn index_backing_race_retries_once_and_continuous_mutation_terminates() {
    let (_directory, store, volume, access) = setup();
    let entry = store.create_database(&access, volume.id, "data", EntryKind::KvDb {}).unwrap();
    write(&store, volume.id, entry.id, b"old");
    let store = Arc::new(store);
    let weak = Arc::downgrade(&store);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_hook = calls.clone();
    *store.snapshots.after_copy.lock().unwrap() = Some(Arc::new(move |db| {
        if db == DatabaseId::Index && calls_hook.fetch_add(1, Ordering::AcqRel) == 0 {
            let store = weak.upgrade().unwrap();
            store.rename_entry(&access, volume.id, entry.id, "renamed")?;
            write(&store, volume.id, entry.id, b"new");
        }
        Ok(())
    }));
    let access = Access::owner(store.account().id);
    let snapshot = store.capture_volume(&access, volume.id).unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(read(&snapshot, entry.id), b"new");
    snapshot
        .with_database(DatabaseId::Index, |tx| {
            assert_eq!(IndexSnapshot::new(tx).lookup("renamed")?.id, entry.id);
            Ok(())
        })
        .unwrap();
    let weak = Arc::downgrade(&store);
    *store.snapshots.after_copy.lock().unwrap() = Some(Arc::new(move |db| {
        if db == DatabaseId::Index {
            write(&weak.upgrade().unwrap(), volume.id, entry.id, b"again");
        }
        Ok(())
    }));
    assert!(matches!(
        store.capture_volume(&access, volume.id),
        Err(Error::LocalContention(CHANGED))
    ));
    assert_eq!(store.snapshots.active.load(Ordering::Acquire), 0);
}

#[test]
fn read_only_traffic_does_not_invalidate_capture_and_backend_fences_cannot_be_rebound() {
    let (_directory, store, volume, access) = setup();
    let entry = store.create_database(&access, volume.id, "data", EntryKind::KvDb {}).unwrap();
    write(&store, volume.id, entry.id, b"stable");
    let store = Arc::new(store);
    let backend = store.entry_backing(volume.id, entry.id, false).unwrap();
    assert!(matches!(
        backend.bind_write_epoch(Arc::new(kvdb::redb::WriteEpoch::default())),
        Err(kvdb::Error::InvalidArgument)
    ));
    let weak = Arc::downgrade(&store);
    *store.snapshots.after_copy.lock().unwrap() = Some(Arc::new(move |_| {
        weak.upgrade().unwrap().with_replica(volume.id, DatabaseId::Entry(entry.id), |db| {
            assert_eq!(&*db.begin_read()?.get(b"payload")?.unwrap(), b"stable");
            Ok(())
        })
    }));
    let snapshot = store.capture_volume(&access, volume.id).unwrap();
    assert_eq!(read(&snapshot, entry.id), b"stable");
}

// An attached backend can have an active writer outside the ordinary volume
// operation gate. Its admission epoch alone cannot qualify committed snapshots.
#[test]
fn active_attached_backend_writer_cannot_qualify_a_local_volume_snapshot() {
    let (_directory, store, volume, access) = setup();
    let entry = store.create_database(&access, volume.id, "data", EntryKind::KvDb {}).unwrap();
    write(&store, volume.id, entry.id, b"committed");
    let backend = store.entry_backing(volume.id, entry.id, false).unwrap();
    let mut writer = backend.begin_write(now_unix_secs()).unwrap();
    writer.put(b"payload", b"pending").unwrap();
    assert!(matches!(
        store.capture_volume(&access, volume.id),
        Err(Error::LocalContention(CHANGED))
    ));
    drop(writer);
    assert_eq!(read(&store.capture_volume(&access, volume.id).unwrap(), entry.id), b"committed");
}

#[test]
fn shared_blob_history_survives_original_cache_eviction_and_snapshot_drop() {
    // Exercise both live capture and explicit retained data after retirement.
    for retired in [false, true] {
        let (_directory, store, volume, access) = setup();
        let first = store.create_blob(&access, volume.id, "photos/first", b"shared").unwrap();
        let second = store.create_blob(&access, volume.id, "photos/second", b"shared").unwrap();
        let address = EntryAddress { volume: volume.id, entry: first.id };
        let old = store.blob_version(&access, address, None).unwrap();
        let other = store
            .blob_version(&access, EntryAddress { volume: volume.id, entry: second.id }, None)
            .unwrap();
        store.write_blob(&access, volume.id, first.id, b"updated").unwrap();
        store.rename_entry(&access, volume.id, first.id, "photos/renamed").unwrap();
        let current = store.blob_version(&access, address, None).unwrap();
        for version in [&old, &other, &current] {
            store.acknowledge_blob_version(&access, version).unwrap();
        }
        if retired {
            store.remove_entry(&access, volume.id, first.id).unwrap();
            store.remove_entry(&access, volume.id, second.id).unwrap();
        }
        let snapshot = store.capture_volume(&access, volume.id).unwrap();
        assert_eq!(
            fs::read_dir(snapshot.directory.0.join(crate::paths::BLOBS)).unwrap().count(),
            2
        );
        store.evict_blobs(0).unwrap();
        assert!(matches!(store.read_blob_version(&access, &old), Err(Error::NotFound(_))));
        for (version, expected) in
            [(&old, &b"shared"[..]), (&other, &b"shared"[..]), (&current, &b"updated"[..])]
        {
            let mut content = Vec::new();
            snapshot.open_blob(version).unwrap().read_to_end(&mut content).unwrap();
            assert_eq!(content, expected);
        }
    }
}

#[test]
fn snapshot_omits_excluded_content_but_preserves_metadata_and_retirement() {
    // Retained Read preserves recovery data without making the ID live again.
    let (_directory, store, volume, access) = setup();
    let private = store.create_database(&access, volume.id, "private", EntryKind::KvDb {}).unwrap();
    let retired = store.create_database(&access, volume.id, "retired", EntryKind::KvDb {}).unwrap();
    write(&store, volume.id, retired.id, b"recoverable");
    store
        .set_entry_permissions(&access, volume.id, private.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    store.remove_entry(&access, volume.id, retired.id).unwrap();
    let snapshot = store.capture_volume(&access, volume.id).unwrap();
    assert!(matches!(
        snapshot.with_database(DatabaseId::Entry(private.id), |_| Ok(())),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(read(&snapshot, retired.id), b"recoverable");
    assert!(matches!(
        store.kv_read(
            &access,
            EntryAddress { volume: volume.id, entry: retired.id },
            b"payload",
            0,
            32,
            None
        ),
        Err(Error::NotFound("retired entry"))
    ));
    snapshot
        .with_database(DatabaseId::Index, |tx| {
            let index = IndexSnapshot::new(tx);
            assert!(index.entry(retired.id)?.deleted);
            assert_eq!(index.lookup("private")?.id, private.id);
            Ok(())
        })
        .unwrap();
}

#[test]
fn captures_opaque_append_log_checkpoint_and_pending_bytes() {
    // Exercise both live capture and explicit retained data after retirement.
    for retired in [false, true] {
        let (_directory, store, volume, access) = setup();
        let entry = store.create_bloblog(&access, volume.id, "events").unwrap();
        store.bloblog_put(&access, volume.id, entry.id, &[b"opaque".to_vec()]).unwrap();
        let page = store.bloblog_get(&access, volume.id, entry.id, None).unwrap();
        store.bloblog_set_checkpoint(&access, volume.id, entry.id, &page.cursor).unwrap();
        if retired {
            store.remove_entry(&access, volume.id, entry.id).unwrap();
            assert!(matches!(
                store.bloblog_get(&access, volume.id, entry.id, None),
                Err(Error::NotFound("retired entry"))
            ));
        }
        let snapshot = store.capture_volume(&access, volume.id).unwrap();
        snapshot
            .with_database(DatabaseId::Entry(entry.id), |tx| {
                let saved: crate::store::bloblog::Cursor =
                    decode(&tx.get(b"remote-cursor")?.unwrap())?;
                assert_eq!(saved, page.cursor);
                assert!(tx.iter().count() > 1, "log content and pending markers remain present");
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn contraction_during_copy_discards_prepared_content_and_retries_with_current_acl() {
    // Exercise the same safety boundary for live and retired retained data.
    for retired in [false, true] {
        let (_directory, store, volume, access) = setup();
        let entry =
            store.create_database(&access, volume.id, "private", EntryKind::KvDb {}).unwrap();
        write(&store, volume.id, entry.id, b"restricted");
        if retired {
            store.remove_entry(&access, volume.id, entry.id).unwrap();
        }
        let store = Arc::new(store);
        let weak = Arc::downgrade(&store);
        let once = AtomicUsize::new(0);
        *store.snapshots.after_copy.lock().unwrap() = Some(Arc::new(move |database| {
            if database == DatabaseId::Index && once.fetch_add(1, Ordering::AcqRel) == 0 {
                let store = weak.upgrade().unwrap();
                store.set_entry_permissions(
                    &Access::owner(store.account().id),
                    volume.id,
                    entry.id,
                    EntryAcl::Replace(Acl::default()),
                )?;
            }
            Ok(())
        }));
        let snapshot = store.capture_volume(&access, volume.id).unwrap();
        assert!(matches!(
            snapshot.with_database(DatabaseId::Entry(entry.id), |_| Ok(())),
            Err(Error::AccessDenied(_))
        ));
        assert!(!snapshot.directory.0.join(DatabaseId::Entry(entry.id).file_name()).exists());
        assert_eq!(fs::read_dir(store.path.join("staging/snapshots")).unwrap().count(), 1);
    }
}

#[test]
fn current_revocation_and_profile_denial_fence_final_snapshot_publication() {
    for profile_denial in [false, true] {
        let (_directory, store, volume, owner) = setup();
        let version = crate::identifier::encode(&[0xab; 32]);
        store.provision_version(&owner, volume.id, &version).unwrap();
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "test".into(),
                },
                BuildSelector::Exact { build_id: version.clone() },
                &version,
                volume.id,
            )
            .unwrap();
        let denied_volume = session.root_volume;
        let participant = Access::session(store.account().id, &session.id);
        let store = Arc::new(store);
        let weak = Arc::downgrade(&store);
        *store.snapshots.after_copy.lock().unwrap() = Some(Arc::new(move |database| {
            if database == DatabaseId::Index {
                let store = weak.upgrade().unwrap();
                if profile_denial {
                    store.record_session_denial(&session, "denied during capture")?;
                } else {
                    store.revoke(
                        &owner,
                        session.root_volume,
                        store.account().id,
                        "revoked during capture",
                    )?;
                }
            }
            Ok(())
        }));
        let result = store.capture_volume(&participant, volume.id);
        if profile_denial {
            assert!(matches!(result, Err(Error::AccessDenied(_))));
        } else {
            assert!(matches!(result, Err(Error::ParticipantRevoked { volume, account })
                if volume == denied_volume && account == participant.account));
        }
        assert_eq!(store.snapshots.active.load(Ordering::Acquire), 0);
        assert_eq!(fs::read_dir(store.path.join("staging/snapshots")).unwrap().count(), 0);
    }
}

#[test]
fn incomplete_manifest_is_unreadable_and_restart_removes_interrupted_capture_files() {
    let (directory, store, volume, access) = setup();
    let capture = store.begin_capture(&access, volume.id).unwrap();
    store.capture_database(&capture, DatabaseId::Index).unwrap();
    let Capture { snapshot, manifest, cut } = capture;
    drop(manifest);
    drop(cut);
    assert!(matches!(
        snapshot.with_database(DatabaseId::Index, |_| Ok(())),
        Err(Error::NotFound("complete volume snapshot"))
    ));
    drop(snapshot);
    let interrupted = store.path.join("staging/snapshots/interrupted");
    fs::create_dir(&interrupted).unwrap();
    fs::write(interrupted.join(MANIFEST_FILE), b"unpublished bytes").unwrap();
    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    assert!(!interrupted.exists());
    assert_eq!(reopened.volume(volume.id).unwrap(), volume);
}

#[test]
fn missing_and_corrupt_blob_bytes_do_not_publish_a_snapshot_or_leak_credits() {
    // Exercise the same safety boundary for live and retired retained data.
    for retired in [false, true] {
        let (_directory, store, volume, access) = setup();
        let entry = store.create_blob(&access, volume.id, "file", b"content").unwrap();
        let version = store
            .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
            .unwrap();
        if retired {
            store.remove_entry(&access, volume.id, entry.id).unwrap();
        }
        let path = store.blob_path(&version.digest, false);
        fs::write(&path, b"corrupt").unwrap();
        assert!(matches!(store.capture_volume(&access, volume.id), Err(Error::Corrupt(_))));
        fs::remove_file(path).unwrap();
        assert!(matches!(
            store.capture_volume(&access, volume.id),
            Err(Error::NotFound("snapshot blob content is unavailable"))
        ));
        assert_eq!(store.snapshots.active.load(Ordering::Acquire), 0);
        store.snapshots.active.store(MAX_CAPTURES, Ordering::Release);
        assert!(matches!(
            store.capture_volume(&access, volume.id),
            Err(Error::LocalContention("both volume snapshot slots are in use"))
        ));
        assert_eq!(store.snapshots.active.load(Ordering::Acquire), MAX_CAPTURES);
    }
}

#[test]
fn backend_epoch_pins_prevent_registry_eviction_and_fence_reuse() {
    let (_directory, store, volume, _access) = setup();
    let database = store.db(volume.id, DatabaseId::Index).unwrap();
    let gate = store.volume_operation(volume.id).unwrap();
    let epoch = Arc::downgrade(&gate.write_epoch);
    drop(gate);
    {
        let mut registry = store.volume_operations.lock().unwrap();
        for number in 0..MAX_VOLUME_OPERATIONS {
            if registry[number].is_none() {
                let mut bytes = [0; 16];
                bytes[..8].copy_from_slice(&(number as u64 + 1).to_le_bytes());
                let id = Id::from_bytes(bytes);
                registry[number] = Some((id, Arc::new(operation_gate::OperationGate::default())));
            }
        }
    }
    for _ in 0..MAX_VOLUME_OPERATIONS + 1 {
        drop(store.operations(&[Id::generate().unwrap()]).unwrap());
    }
    let current = store.volume_operation(volume.id).unwrap();
    let epoch = epoch.upgrade().expect("the attached backend retains its epoch");
    assert!(Arc::ptr_eq(&epoch, &current.write_epoch));
    let before = epoch.current();
    drop(database.begin_write(now_unix_secs()).unwrap());
    assert!(epoch.current() > before);
}

#[test]
fn retired_object_schema_and_content_survive_but_denied_retained_content_is_omitted() {
    // Preserve a schema-owning backing and refuse another retired ID's content
    // under current ACLs, without resurrecting either catalog name.
    let (_directory, store, volume, access) = setup();
    let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    let crate::store::initialization::InitialDatabaseKind::ObjectDb { schema, .. } =
        declaration.databases[0].kind.clone()
    else {
        panic!()
    };
    let entry = store
        .create_database(
            &access,
            volume.id,
            "objects",
            EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    let record = store
        .create_object_with_fields(
            &access,
            address,
            Id::from_bytes([1; 16]),
            &BTreeMap::from([(
                Id::from_bytes([2; 16]),
                Some(vec![Value::Text("retained".into())]),
            )]),
        )
        .unwrap();
    let denied = store.create_blob(&access, volume.id, "denied", b"private").unwrap();
    let denied_version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: denied.id }, None)
        .unwrap();
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    store.remove_entry(&access, volume.id, denied.id).unwrap();
    store
        .set_entry_permissions(&access, volume.id, denied.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    let snapshot = store.capture_volume(&access, volume.id).unwrap();
    snapshot
        .with_database(DatabaseId::Index, |tx| {
            let index = IndexSnapshot::new(tx);
            let retained = index.entry(entry.id)?;
            assert!(retained.deleted);
            assert_eq!(retained.info.kind, EntryKind::ObjectDb { schema: schema.clone() });
            assert!(index.entry(denied.id)?.deleted);
            assert!(matches!(index.lookup("objects"), Err(Error::NotFound(_))));
            Ok(())
        })
        .unwrap();
    snapshot
        .with_database(DatabaseId::Entry(entry.id), |tx| {
            assert_eq!(
                crate::store::object_database::Snapshot::new(tx).read(record.object_id, None)?,
                record
            );
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        store.read_object(&access, address, record.object_id),
        Err(Error::NotFound("retired entry"))
    ));
    assert!(snapshot.open_blob(&denied_version).is_err());
    assert_eq!(fs::read_dir(snapshot.directory.0.join(crate::paths::BLOBS)).unwrap().count(), 0);
}
