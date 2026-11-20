#![allow(clippy::unwrap_used, reason = "Recovery fixtures assert admitted durable operations.")]
use super::*;
use crate::store::object_contract::{Grant, Subject, Value};

fn fixture(service: bool) -> (tempfile::TempDir, Arc<LocalStore>, Volume, Access) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let volume = store.create_volume("Recovery", service).unwrap();
    let access = Access::owner(store.account.id);
    (directory, store, volume, access)
}
fn address(volume: &Volume, entry: &VolumeEntry) -> EntryAddress {
    EntryAddress { volume: volume.id, entry: entry.id }
}
fn own_override(account: Id) -> EntryAcl {
    EntryAcl::Replace(Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(account)),
            permissions: vec![Permission::Read, Permission::Write],
        }],
    })
}

#[test]
fn recover_retired_blob_version_preserves_override_and_content_with_new_publication_time() {
    let (directory, store, volume, access) = fixture(false);
    let entry = store.create_blob(&access, volume.id, "file", b"shared").unwrap();
    let shared = store.create_blob(&access, volume.id, "shared", b"shared").unwrap();
    let old = store.blob_version(&access, address(&volume, &entry), None).unwrap();
    let acl = own_override(access.account);
    store.set_entry_permissions(&access, volume.id, entry.id, acl.clone()).unwrap();
    store.write_blob(&access, volume.id, entry.id, b"new").unwrap();
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    assert_eq!(store.read_blob_version(&access, &old).unwrap(), b"shared");
    let history = store.entry_revisions(&access, old.address, None, 64).unwrap().revisions;
    assert_eq!(history.len(), 3);
    assert!(
        history
            .iter()
            .any(|revision| matches!(revision.change, volume_index::EntryChange::Delete {}))
    );
    let recovered =
        store.recover_entry(&access, old.address, "recovered/file", Some(old.revision)).unwrap();
    assert_ne!(recovered.id, entry.id);
    assert_ne!(recovered.info.created_revision, entry.info.created_revision);
    let EntryKind::Blob { digest: old_digest, published_at_micros: old_time, .. } = entry.info.kind
    else {
        unreachable!()
    };
    let EntryKind::Blob { digest: new_digest, published_at_micros: new_time, .. } =
        recovered.info.kind
    else {
        unreachable!()
    };
    assert_eq!(new_digest, old_digest);
    assert!(new_time >= old_time);
    assert_eq!(store.entry_details(&access, volume.id, recovered.id).unwrap().permissions, acl);
    assert!(store.entry_details(&access, volume.id, entry.id).unwrap().entry.deleted);
    store.evict_blobs(0).unwrap();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, recovered.id).unwrap(), b"shared");
    assert_eq!(store.read_blob_entry(&access, volume.id, shared.id).unwrap(), b"shared");
    assert_eq!(store.read_blob_version(&access, &old).unwrap(), b"shared");
}

#[test]
fn retained_read_is_current_authority_and_manage_alone_never_reads_content() {
    let (_directory, store, volume, access) = fixture(false);
    let entry = store.create_blob(&access, volume.id, "file", b"private").unwrap();
    let address = address(&volume, &entry);
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    store
        .set_entry_permissions(&access, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(
        store.entry_revisions(&access, address, None, 64),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(
        store.recover_entry(&access, address, "recovered", None),
        Err(Error::AccessDenied(_))
    ));
    store.set_entry_permissions(&access, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    store.recover_entry(&access, address, "recovered", None).unwrap();
}

#[test]
fn recovery_refuses_live_entries_and_occupied_names_without_usable_new_entries() {
    let (_directory, store, volume, access) = fixture(false);
    let entry = store.create_blob(&access, volume.id, "file", b"content").unwrap();
    let address = address(&volume, &entry);
    assert!(matches!(
        store.recover_entry(&access, address, "copy", None),
        Err(Error::InvalidInput(_))
    ));
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    let occupied = store.create_database(&access, volume.id, "file", EntryKind::KvDb {}).unwrap();
    assert!(matches!(
        store.recover_entry(&access, address, "file", None),
        Err(Error::AlreadyExists(_))
    ));
    assert_eq!(store.resolve_entry(&access, volume.id, "file").unwrap().id, occupied.id);
    assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 2);
}

#[test]
fn recovery_read_withdrawal_and_prepublication_interrupt_never_publish_a_birth() {
    for withdraw in [false, true] {
        let (directory, store, volume, access) = fixture(false);
        let entry = store.create_blob(&access, volume.id, "file", b"retained").unwrap();
        store.remove_entry(&access, volume.id, entry.id).unwrap();
        let weak = Arc::downgrade(&store);
        let target = address(&volume, &entry);
        let access2 = access.clone();
        *store.entry_blobs.recovery_before_publish.lock().unwrap() = Some(Arc::new(move || {
            if withdraw {
                weak.upgrade().unwrap().set_entry_permissions(
                    &access2,
                    target.volume,
                    target.entry,
                    EntryAcl::Replace(Acl::default()),
                )?;
                Ok(())
            } else {
                Err(Error::System("interrupted before publication".into()))
            }
        }));
        assert!(store.recover_entry(&access, target, "recovered", None).is_err());
        assert_eq!(store.entry_blobs.active.load(std::sync::atomic::Ordering::Acquire), 0);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert!(store.resolve_entry(&access, volume.id, "recovered").is_err());
        assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 1);
        assert!(store.entry_details(&access, volume.id, entry.id).unwrap().entry.deleted);
    }
}

#[test]
fn missing_or_corrupt_retained_blob_cannot_publish_recovery() {
    for corrupt in [false, true] {
        let (_directory, store, volume, access) = fixture(false);
        let entry = store.create_blob(&access, volume.id, "file", b"retained").unwrap();
        let EntryKind::Blob { digest, .. } = entry.info.kind else { unreachable!() };
        store.remove_entry(&access, volume.id, entry.id).unwrap();
        if corrupt {
            fs::write(store.blob_path(&digest, false), b"corrupt!").unwrap();
        } else {
            fs::remove_file(store.blob_path(&digest, false)).unwrap();
        }
        assert!(store.recover_entry(&access, address(&volume, &entry), "recovered", None).is_err());
        assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 1);
    }
}

#[test]
fn recovered_object_database_has_its_schema_ids_fields_and_new_backing_identity() {
    let (directory, store, volume, access) = fixture(false);
    let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    let crate::store::initialization::InitialDatabaseKind::ObjectDb { mut schema, .. } =
        declaration.databases[0].kind.clone()
    else {
        unreachable!()
    };
    use crate::store::object_contract::{FieldDeclaration, ValueType};
    let fields = &mut schema.types.get_mut(&Id::from_bytes([1; 16])).unwrap().fields;
    fields.insert(
        Id::from_bytes([4; 16]),
        FieldDeclaration {
            name: "file".into(),
            value_type: ValueType::File,
            repeated: false,
            deprecated: false,
        },
    );
    fields.insert(
        Id::from_bytes([5; 16]),
        FieldDeclaration {
            name: "reference".into(),
            value_type: ValueType::Reference(Id::from_bytes([1; 16])),
            repeated: false,
            deprecated: false,
        },
    );
    let blob = store.create_blob(&access, volume.id, "file", b"independent").unwrap();
    let entry = store
        .create_database(
            &access,
            volume.id,
            "objects",
            EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let target = address(&volume, &entry);
    let record = store
        .create_object_with_fields(
            &access,
            target,
            Id::from_bytes([1; 16]),
            &BTreeMap::from([
                (Id::from_bytes([2; 16]), Some(vec![Value::Text("retained".into())])),
                (
                    Id::from_bytes([4; 16]),
                    Some(vec![Value::File { volume_id: volume.id, entry_id: blob.id }]),
                ),
                (
                    Id::from_bytes([5; 16]),
                    Some(vec![Value::Reference {
                        volume_id: volume.id,
                        database_id: entry.id,
                        object_id: Id::from_bytes([9; 16]),
                    }]),
                ),
            ]),
        )
        .unwrap();
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    let recovered = store.recover_entry(&access, target, "recovered", None).unwrap();
    let next = address(&volume, &recovered);
    assert_eq!(store.schema(&access, next).unwrap(), schema);
    assert_eq!(store.read_object(&access, next, record.object_id).unwrap(), record);
    assert!(store.read_object(&access, target, record.object_id).is_err());
    assert_eq!(store.read_blob_entry(&access, volume.id, blob.id).unwrap(), b"independent");
    store.remove_entry(&access, volume.id, blob.id).unwrap();
    assert_eq!(
        store.read_object(&access, next, record.object_id).unwrap(),
        record,
        "dangling file and object references stay unchanged, never retargeted to recovery"
    );
    store
        .with_replica(volume.id, DatabaseId::Entry(next.entry), |db| {
            assert_eq!(
                db.meta().hash_key,
                DatabaseId::Entry(next.entry)
                    .hash_key(&store.credentials(volume.id)?.database_key, volume.id)
            );
            Ok(())
        })
        .unwrap();
    drop(store);
    assert_eq!(
        LocalStore::open(directory.path())
            .unwrap()
            .read_object(&access, next, record.object_id)
            .unwrap(),
        record
    );
}

#[test]
fn recovered_append_log_changes_identity_domain_and_retains_unconfirmed_payloads() {
    let (_directory, store, volume, access) = fixture(true);
    let entry = store.create_bloblog(&access, volume.id, "log").unwrap();
    store.bloblog_put(&access, volume.id, entry.id, &[b"one".to_vec(), b"two".to_vec()]).unwrap();
    let before = store.bloblog_get(&access, volume.id, entry.id, None).unwrap();
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    let recovered =
        store.recover_entry(&access, address(&volume, &entry), "recovered-log", None).unwrap();
    assert!(store.bloblog_get(&access, volume.id, recovered.id, None).unwrap().entries.is_empty());
    let after = store.bloblog_pending(&access, volume.id, recovered.id).unwrap();
    assert_eq!(before.entries.len(), after.len());
    for a in &before.entries {
        let b = after.iter().find(|entry| entry.nonce == a.nonce).unwrap();
        assert_eq!((&a.data, a.clock, a.nonce), (&b.data, b.clock, b.nonce));
        assert_ne!(a.id, b.id);
        assert_eq!(b.log, recovered.id);
    }
    let db = store.entry_backing(volume.id, recovered.id, false).unwrap();
    let log = crate::store::bloblog::Log {
        database: &db,
        volume: volume.id,
        entry: recovered.id,
        encryption_key: None,
        now: now_unix_secs(),
    };
    let pending = log
        .pending()
        .unwrap()
        .into_iter()
        .map(|entry| (entry.id, entry))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(pending, after.into_iter().map(|entry| (entry.id, entry)).collect());
    assert!(log.validate_cursor(&before.cursor).is_err());
    log.validate().unwrap();
}

#[test]
fn log_recovery_requires_destination_write_and_rechecks_it_before_publication() {
    let (_directory, store, volume, owner) = fixture(true);
    let entry = store.create_bloblog(&owner, volume.id, "log").unwrap();
    store.bloblog_put(&owner, volume.id, entry.id, &[b"retained".to_vec()]).unwrap();
    store.remove_entry(&owner, volume.id, entry.id).unwrap();
    let participant = Access::owner(Id::generate().unwrap());
    let permissions = |write| Acl {
        grants: vec![Grant {
            subject: Subject::Public,
            permissions: if write {
                vec![Permission::Read, Permission::Write, Permission::Append]
            } else {
                vec![Permission::Read, Permission::Append]
            },
        }],
    };
    store.set_permissions(&owner, volume.id, permissions(false)).unwrap();
    let original = address(&volume, &entry);
    assert!(store.entry_revisions(&participant, original, None, 64).is_ok());
    assert!(matches!(
        store.recover_entry(&participant, original, "recovered-log", None),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(
        store.resolve_entry(&owner, volume.id, "recovered-log"),
        Err(Error::NotFound(_))
    ));
    store.set_permissions(&owner, volume.id, permissions(true)).unwrap();
    let weak = Arc::downgrade(&store);
    let current_owner = owner.clone();
    *store.entry_blobs.recovery_before_publish.lock().unwrap() = Some(Arc::new(move || {
        weak.upgrade().unwrap().set_permissions(&current_owner, volume.id, permissions(false))
    }));
    assert!(matches!(
        store.recover_entry(&participant, original, "recovered-log", None),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(
        store.resolve_entry(&owner, volume.id, "recovered-log"),
        Err(Error::NotFound(_))
    ));
    *store.entry_blobs.recovery_before_publish.lock().unwrap() = None;
    store.set_permissions(&owner, volume.id, permissions(true)).unwrap();
    let recovered = store.recover_entry(&participant, original, "recovered-log", None).unwrap();
    assert_ne!(recovered.id, entry.id);
    let pending = store.bloblog_pending(&participant, volume.id, recovered.id).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].data, b"retained");
}

#[test]
fn backing_published_without_index_birth_remains_an_unusable_retained_orphan_after_restart() {
    let (directory, store, volume, access) = fixture(false);
    let old = store.create_database(&access, volume.id, "old", EntryKind::KvDb {}).unwrap();
    store
        .with_replica(volume.id, DatabaseId::Entry(old.id), |db| {
            let mut tx = db.begin_write(now_unix_secs())?;
            tx.put(b"retained", b"data")?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    store.remove_entry(&access, volume.id, old.id).unwrap();
    let address = address(&volume, &old);
    let source = store.recovery_source(&access, address, "recovered", None, None).unwrap();
    let id = EntryId::generate().unwrap();
    let prepared = store
        .prepare_recovered_database(
            &access,
            address,
            EntryAddress { volume: address.volume, entry: id },
            &source,
        )
        .unwrap();
    prepared
        .publish(&store.path.join(crate::paths::VOLUMES).join(volume.id.to_string()), id)
        .unwrap();
    let backing = store
        .path
        .join(crate::paths::VOLUMES)
        .join(volume.id.to_string())
        .join(DatabaseId::Entry(id).file_name());
    assert!(backing.is_file());
    drop(prepared);
    drop(source);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(backing.is_file(), "interruption never destroys retained content");
    assert!(matches!(store.entry(&access, volume.id, id), Err(Error::NotFound(_))));
    assert!(matches!(
        store.resolve_entry(&access, volume.id, "recovered"),
        Err(Error::NotFound(_))
    ));
    assert!(store.with_replica(volume.id, DatabaseId::Entry(id), |_| Ok(())).is_err());
    assert_eq!(store.entries(&access, volume.id, None, 64).unwrap().entries.len(), 1);
}

#[test]
fn recovered_kv_database_streams_ten_thousand_records_and_largest_legal_value() {
    use kvdb::ValueRead;
    let (directory, store, volume, access) = fixture(false);
    let old = store.create_database(&access, volume.id, "old", EntryKind::KvDb {}).unwrap();
    let large = vec![0x57; kvdb::VALUE_SIZE_MAX];
    let clock = store
        .with_replica(volume.id, DatabaseId::Entry(old.id), |db| {
            let mut tx = db.begin_write(now_unix_secs())?;
            for number in 0..10_000u32 {
                tx.put(&number.to_be_bytes(), &number.to_le_bytes())?;
            }
            tx.put(b"large", &large)?;
            tx.put(b"removed", b"old")?;
            tx.del(b"removed")?;
            let clock =
                tx.get_with_clock(b"large")?.ok_or(Error::corrupt("fixture clock is missing"))?.0;
            tx.commit()?;
            Ok(clock)
        })
        .unwrap();
    store.remove_entry(&access, volume.id, old.id).unwrap();
    let recovered =
        store.recover_entry(&access, address(&volume, &old), "recovered", None).unwrap();
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    store
        .with_replica(volume.id, DatabaseId::Entry(recovered.id), |db| {
            let tx = db.begin_read()?;
            for number in 0..10_000u32 {
                assert_eq!(
                    tx.get(&number.to_be_bytes())?.as_deref(),
                    Some(number.to_le_bytes().as_slice())
                );
            }
            assert!(!tx.has(b"removed")?);
            let reader =
                tx.value_reader(b"large")?.ok_or(Error::corrupt("fixture value is missing"))?;
            assert_eq!(reader.clock(), Some(clock));
            assert_eq!(reader.len(), large.len());
            let mut chunk = [0; 4096];
            let mut offset = 0;
            while offset < reader.len() {
                let length = chunk.len().min(reader.len() - offset);
                reader.read_exact_at(offset, &mut chunk[..length])?;
                assert!(chunk[..length].iter().all(|byte| *byte == 0x57));
                offset += length;
            }
            let mut count = 0;
            let mut after = Vec::new();
            while let Some(key) = tx.next_key(&after, !after.is_empty())? {
                count += 1;
                after = key;
            }
            assert_eq!(count, 10_001);
            Ok(())
        })
        .unwrap();
}

#[test]
fn recovery_respects_personal_read_only_consent_and_rechecks_withdrawal_before_publication() {
    let (_directory, store, volume, owner) = fixture(false);
    let root = store.create_volume("App root", false).unwrap();
    let version = crate::identifier::encode(&[0x11; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let access = Access::session_version(owner.account, &session.id, &version);
    let old = store.create_blob(&owner, volume.id, "file", b"retained").unwrap();
    store.remove_entry(&owner, volume.id, old.id).unwrap();
    let address = address(&volume, &old);
    let grant = |write| vec![PersonalVolumeGrant { volume: volume.id, write }];
    store.select_personal_volumes(&session.id, grant(false)).unwrap();
    assert!(store.entry_revisions(&access, address, None, 64).is_ok());
    assert!(matches!(
        store.recover_entry(&access, address, "recovered", None),
        Err(Error::AccessDenied(_))
    ));
    store.select_personal_volumes(&session.id, grant(true)).unwrap();
    let weak = Arc::downgrade(&store);
    let current_session = session.id.clone();
    *store.entry_blobs.recovery_before_publish.lock().unwrap() = Some(Arc::new(move || {
        weak.upgrade().unwrap().select_personal_volumes(
            &current_session,
            vec![PersonalVolumeGrant { volume: address.volume, write: false }],
        )
    }));
    assert!(matches!(
        store.recover_entry(&access, address, "recovered", None),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(store.resolve_entry(&owner, volume.id, "recovered"), Err(Error::NotFound(_))));
    *store.entry_blobs.recovery_before_publish.lock().unwrap() = None;
    store.select_personal_volumes(&session.id, grant(true)).unwrap();
    store.recover_entry(&access, address, "recovered", None).unwrap();
}

#[test]
fn recovery_availability_tracks_retained_database_backing() {
    let (_directory, store, volume, access) = fixture(false);
    let entry = store.create_database(&access, volume.id, "data2", EntryKind::KvDb {}).unwrap();
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    assert!(store.entry_details(&access, volume.id, entry.id).unwrap().recoverable);
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let mut tx = index.begin_write(now_unix_secs()).unwrap();
    volume_index::quarantine_backing(&mut tx.local_indexes(), entry.id).unwrap();
    tx.commit().unwrap();
    let details = store.entry_details(&access, volume.id, entry.id).unwrap();
    assert!(!details.backing_ready);
    assert!(!details.recoverable, "unavailable retained backing cannot be recovered");
    assert!(matches!(
        store.recover_entry(&access, address(&volume, &entry), "data2", None),
        Err(Error::NotFound("retained entry backing is unavailable"))
    ));
    let mut tx = index.begin_write(now_unix_secs()).unwrap();
    volume_index::mark_source_backing_ready(&mut tx, entry.id).unwrap();
    tx.commit().unwrap();
    assert!(store.entry_details(&access, volume.id, entry.id).unwrap().recoverable);
    store.recover_entry(&access, address(&volume, &entry), "data2", None).unwrap();
}

#[test]
fn recovery_availability_waits_for_schema_dependencies() {
    let (directory, store, volume, access) = fixture(false);
    let entry = store
        .create_database(
            &access,
            volume.id,
            "data2",
            EntryKind::ObjectDb { schema: Schema::default() },
        )
        .unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let source = create_db(
        &directory.path().join("schema-source.kvdb"),
        Some(index.meta().hash_key),
        512 * 1024,
    )
    .unwrap();
    merge_index_replica(directory.path(), &index, &source, kvdb::Lww, |_| Ok(())).unwrap();
    let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    let crate::store::InitialDatabaseKind::ObjectDb { schema, .. } =
        declaration.databases[0].kind.clone()
    else {
        panic!("object database fixture")
    };
    let mut tx = source.begin_write(now_unix_secs()).unwrap();
    volume_index::establish_schema(
        &mut tx,
        entry.id,
        &schema,
        EntryId::generate().unwrap(),
        &entry.info.origin,
    )
    .unwrap();
    tx.commit().unwrap();
    let snapshot = source.begin_read().unwrap();
    // Deliver fields before their type, as an out-of-order replica may do.
    // Field facts use [META_BYTE, 10, entry ID, type ID, field ID] source keys.
    let mut prefix = vec![volume_index::META_BYTE, 10];
    prefix.extend_from_slice(entry.id.as_bytes());
    let expected_fields = schema.types.values().map(|item| item.fields.len()).sum::<usize>();
    assert!(expected_fields > 0, "schema fixture must contain field dependencies");
    let mut delivered_fields = 0;
    for item in snapshot.iter() {
        let (key, _) = item.unwrap();
        if !key.starts_with(&prefix) {
            continue;
        }
        assert_eq!(key.len(), 49, "field fact source key width");
        let kvdb::CommittedDelta::Bytes(delta) =
            snapshot.delta_for_key(&key, 64 * 1024).unwrap().unwrap()
        else {
            panic!("bounded schema fixture")
        };
        let mut receiver =
            kvdb::DeltaReceiver::new(&index, kvdb::Lww, now_unix_secs(), 64 * 1024).unwrap();
        receiver.process(&delta).unwrap();
        receiver.finish().unwrap();
        delivered_fields += 1;
    }
    assert_eq!(delivered_fields, expected_fields, "all field dependencies arrive before types");
    store.remove_entry(&access, volume.id, entry.id).unwrap();
    let details = store.entry_details(&access, volume.id, entry.id).unwrap();
    assert!(details.backing_ready);
    assert!(details.schema_pending);
    assert!(!details.recoverable, "incomplete retained schema cannot be recovered");
    assert!(matches!(
        store.recover_entry(&access, address(&volume, &entry), "data2", None),
        Err(Error::NotFound("retained entry backing is unavailable"))
    ));
    drop(snapshot);
    merge_index_replica(directory.path(), &source, &index, kvdb::Lww, |_| Ok(())).unwrap();
    let details = store.entry_details(&access, volume.id, entry.id).unwrap();
    assert!(!details.schema_pending);
    assert!(details.recoverable);
    let recovered = store.recover_entry(&access, address(&volume, &entry), "data2", None).unwrap();
    assert_eq!(store.schema(&access, address(&volume, &recovered)).unwrap(), schema);
}

#[test]
fn history_merge_preserves_an_unread_source_prefix() {
    let (_directory, store, volume, access) = fixture(false);
    let entry = store.create_blob(&access, volume.id, "file", b"one").unwrap();
    for value in [b"two", b"tri", b"end"] {
        store.write_blob(&access, volume.id, entry.id, value).unwrap();
    }
    let all = store.entry_revisions(&access, address(&volume, &entry), None, 64).unwrap();
    assert_eq!(all.revisions.len(), 4);
    // One source stopped early due to its byte budget; the other has older
    // rows. The unread row at index 1 must precede those older rows next time.
    let first = all.revisions[0].clone();
    let merged = merge_retained_revisions(
        HistoryPage { revisions: vec![first.clone()], next: Some(HistoryCursor::from(&first)) },
        HistoryPage { revisions: all.revisions[2..].to_vec(), next: None },
        64,
    )
    .unwrap();
    assert_eq!(merged.revisions, vec![first.clone()]);
    assert_eq!(merged.next, Some(HistoryCursor::from(&first)));
    // Duplicate records across retained sources are emitted only once.
    let merged = merge_retained_revisions(all.clone(), all.clone(), 64).unwrap();
    assert_eq!(merged, all);
}
