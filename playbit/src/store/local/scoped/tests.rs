//! A selected cut excludes unrelated facts without changing origin clocks.
#![allow(clippy::unwrap_used, reason = "Tests assert concrete projection fixtures.")]
use super::*;
use crate::store::PLAYBIT_REPLICA_PRINCIPAL;
use crate::store::object_contract::{Grant, Principal, Subject};

#[test]
fn write_or_append_projects_editing_controls_without_history_content_or_catalog() {
    for permission in [Permission::Write, Permission::Append] {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let owner = Access::owner(store.account().id);
        let writer = Access::owner(Id::from_bytes([4; 16]));
        let volume = store.create_volume("Controls", true).unwrap();
        let entry = store
            .create_database(&owner, volume.id, "initial", volume_index::EntryKind::KvDb {})
            .unwrap();
        let sibling = store
            .create_database(&owner, volume.id, "private", volume_index::EntryKind::KvDb {})
            .unwrap();
        for revision in 0..PAGE_ROWS + 1 {
            store.rename_entry(&owner, volume.id, entry.id, &format!("name-{revision}")).unwrap();
        }
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                entry.id,
                EntryAcl::Replace(Acl {
                    grants: vec![Grant {
                        subject: Subject::Principal(Principal::Human(writer.account)),
                        permissions: vec![permission],
                    }],
                }),
            )
            .unwrap();
        let source = store.db(volume.id, DatabaseId::Index).unwrap();
        let policy = DatabasePolicy::read(&source, DatabaseId::Entry(entry.id)).unwrap();
        assert!(!policy.metadata_readable(writer.account).unwrap());
        let projection =
            ScopedSnapshot::new(&source, DatabaseId::Index, &policy, writer.account, None)
                .unwrap()
                .project(&std::env::temp_dir())
                .unwrap();
        let read = projection.database().begin_read().unwrap();
        for row in read.iter() {
            let (key, _) = row.unwrap();
            assert!(!volume_index::is_entry_history(&key));
        }
        let snapshot = volume_index::IndexSnapshot::new(read);
        assert_eq!(snapshot.source_entry(entry.id).unwrap().info.name, format!("name-{PAGE_ROWS}"));
        assert!(snapshot.source_entry(sibling.id).is_err());
        let details = store.entry_details(&writer, volume.id, entry.id).unwrap();
        assert!(!details.readable && !details.retained_readable);
        assert_eq!(details.writable, permission == Permission::Write);
        assert_eq!(details.appendable, permission == Permission::Append);
        assert!(store.entries(&writer, volume.id, None, 64).is_err());
        assert!(store.entry(&writer, volume.id, entry.id).is_err());
        assert!(
            store
                .entry_revisions(
                    &writer,
                    volume_index::EntryAddress { volume: volume.id, entry: entry.id },
                    None,
                    64
                )
                .is_err()
        );
    }
}

#[test]
fn selected_pages_keep_source_indexes_consistent_without_qualifying_app_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let volume = store.create_volume("Paged receipt", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "initial", volume_index::EntryKind::KvDb {})
        .unwrap();
    // More than one source page: the birth arrives before the current name head.
    for revision in 0..PAGE_ROWS + 1 {
        store.rename_entry(&owner, volume.id, entry.id, &format!("name-{revision}")).unwrap();
    }
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&source, DatabaseId::Entry(entry.id)).unwrap();
    let target = empty_detached_replica(&std::env::temp_dir(), &source).unwrap();
    let cut =
        || ScopedSnapshot::new(&source, DatabaseId::Index, &policy, owner.account, None).unwrap();
    let check = || {
        let read = volume_index::IndexSnapshot::new(target.database().begin_read().unwrap());
        // A detached page repairs raw indexes but cannot grant app access.
        // Receipt qualification belongs to the final publication boundary.
        assert!(!read.source_entry(entry.id).unwrap().name_conflict);
        assert!(matches!(read.entry(entry.id), Err(Error::NotFound(_))));
        let rows = read.source_headers(None, 64).unwrap().entries;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, entry.id);
    };
    let mut interrupted = false;
    let result = cut().merge_pages(target.database(), || {
        check();
        interrupted = true;
        Err(Error::System("injected page interruption".into()))
    });
    assert!(interrupted && result.is_err());
    let retained = volume_index::IndexSnapshot::new(target.database().begin_read().unwrap());
    let retained_name = retained.source_entry(entry.id).unwrap().info.name;
    let mut pages = 0;
    cut()
        .merge_pages(target.database(), || {
            check();
            pages += 1;
            Ok(())
        })
        .unwrap();
    assert!(pages > 1);
    ScopedSnapshot::for_reconciliation(&source, DatabaseId::Index, &policy, owner.account, None)
        .unwrap()
        .merge_into(target.database())
        .unwrap();
    check();
    let current = volume_index::IndexSnapshot::new(target.database().begin_read().unwrap());
    assert_eq!(current.source_entry(entry.id).unwrap().info.name, format!("name-{PAGE_ROWS}"));
    assert!(matches!(current.entry(entry.id), Err(Error::NotFound(_))));
    assert_eq!(retained.source_entry(entry.id).unwrap().info.name, retained_name);
}

#[test]
fn source_publication_excludes_catalog_controls_acl_changes_and_siblings_without_read() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let writer = Id::from_bytes([4; 16]);
    let volume = store.create_volume("Publication", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "selected", volume_index::EntryKind::KvDb {})
        .unwrap();
    let other = store
        .create_database(&owner, volume.id, "sibling", volume_index::EntryKind::KvDb {})
        .unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(writer)),
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = MetadataPolicy::read(&source).unwrap();
    assert!(policy.authorize(writer, Permission::Read).is_err());
    let projection = ScopedSnapshot::for_publication(&source, &policy, entry.id, writer).unwrap();
    store.rename_entry(&Access::owner(writer), volume.id, entry.id, "later").unwrap();
    let projection = projection.project(&std::env::temp_dir()).unwrap();
    let snapshot = volume_index::IndexSnapshot::new(projection.database().begin_read().unwrap());
    assert_eq!(snapshot.source_entry(entry.id).unwrap().info.name, "selected");
    assert!(snapshot.source_entry(other.id).is_err());
    assert_eq!(snapshot.permissions(entry.id).unwrap(), EntryAcl::Inherit);
    assert!(MetadataPolicy::read(projection.database()).is_err());
    assert!(
        ScopedSnapshot::for_publication(&source, &policy, entry.id, Id::from_bytes([5; 16]))
            .is_err()
    );
}

#[test]
fn delegated_management_publication_includes_only_the_selected_entry_acl() {
    // A management-only source can forward its ACL without receiving unrelated controls or content.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let manager = Id::from_bytes([6; 16]);
    let volume = store.create_volume("Management publication", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "selected", volume_index::EntryKind::KvDb {})
        .unwrap();
    let sibling = store
        .create_database(&owner, volume.id, "sibling", volume_index::EntryKind::KvDb {})
        .unwrap();
    let permissions = EntryAcl::Replace(Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(manager)),
            permissions: vec![Permission::ManagePermissions],
        }],
    });
    store.set_entry_permissions(&owner, volume.id, entry.id, permissions.clone()).unwrap();
    assert!(store.entry_details(&Access::owner(manager), volume.id, entry.id).is_err());
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = MetadataPolicy::read(&source).unwrap();
    let projection = ScopedSnapshot::for_publication(&source, &policy, entry.id, manager)
        .unwrap()
        .project(&std::env::temp_dir())
        .unwrap();
    let snapshot = volume_index::IndexSnapshot::new(projection.database().begin_read().unwrap());
    assert_eq!(snapshot.permissions(entry.id).unwrap(), permissions);
    let keys = projection
        .database()
        .begin_read()
        .unwrap()
        .iter()
        .map(|row| row.unwrap().0.to_vec())
        .collect::<Vec<_>>();
    assert_eq!(keys, vec![volume_index::permission_source_key(entry.id).to_vec()]);
    assert!(snapshot.source_entry(sibling.id).is_err());
    assert!(MetadataPolicy::read(projection.database()).is_err());
    assert!(matches!(
        store.rename_entry(&Access::owner(manager), volume.id, entry.id, "unauthorized"),
        Err(Error::AccessDenied(_))
    ));
    store
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert!(matches!(
        ScopedSnapshot::for_publication(&source, &policy, entry.id, manager),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
fn append_managers_publish_birth_and_acl_without_replaying_unwritable_history() {
    // An entry with existing rename history still bootstraps under Append; demotion keeps only current rights.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let manager = Access::owner(Id::from_bytes([7; 16]));
    let volume = store.create_volume("Append management", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "initial", volume_index::EntryKind::KvDb {})
        .unwrap();
    store.rename_entry(&owner, volume.id, entry.id, "prior").unwrap();
    store.rename_entry(&owner, volume.id, entry.id, "current").unwrap();
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = MetadataPolicy::read(&source).unwrap();
    let acl = |permissions: Vec<Permission>| {
        EntryAcl::Replace(Acl {
            grants: vec![Grant {
                subject: Subject::Principal(Principal::Human(manager.account)),
                permissions,
            }],
        })
    };
    for data_permission in [Permission::Append, Permission::Write] {
        store
            .set_entry_permissions(
                &owner,
                volume.id,
                entry.id,
                acl(vec![Permission::Read, data_permission, Permission::ManagePermissions]),
            )
            .unwrap();
        for demoted in [false, true] {
            if demoted {
                store
                    .set_entry_permissions(
                        &manager,
                        volume.id,
                        entry.id,
                        acl(vec![Permission::Read, data_permission]),
                    )
                    .unwrap();
            }
            let projected =
                ScopedSnapshot::for_publication(&source, &policy, entry.id, manager.account)
                    .unwrap()
                    .project(&std::env::temp_dir())
                    .unwrap();
            let read = projected.database().begin_read().unwrap();
            let keys = read.iter().map(|row| row.unwrap().0.to_vec()).collect::<Vec<_>>();
            assert!(keys.contains(&volume_index::birth_source_key(entry.id).to_vec()));
            assert!(keys.contains(&volume_index::permission_source_key(entry.id).to_vec()));
            if data_permission == Permission::Append {
                assert_eq!(keys.len(), 2, "Append cannot publish independent registers or history");
            } else {
                assert!(keys.iter().any(|key| volume_index::is_entry_history(key)));
            }
            let before = volume_index::IndexSnapshot::new(source.begin_read().unwrap());
            for key in &keys {
                let (clock, bytes) = read.get_with_clock(key).unwrap().unwrap();
                let remote = kvdb::sync::EntState::Present { clock, value: &bytes };
                before
                    .authorize_source(
                        &volume,
                        policy.default_permissions(),
                        manager.account,
                        &kvdb::sync::Delta { key, local: remote, remote },
                    )
                    .unwrap();
            }
        }
    }
}

#[test]
fn retired_append_managers_publish_only_the_acl_without_replaying_the_birth() {
    // Raw retirement blocks Append birth replay while current management still permits ACL publication.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let manager = Id::from_bytes([8; 16]);
    let volume = store.create_volume("Retired management", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "retired", volume_index::EntryKind::KvDb {})
        .unwrap();
    store.remove_entry(&owner, volume.id, entry.id).unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(manager)),
                    permissions: vec![Permission::Append, Permission::ManagePermissions],
                }],
            }),
        )
        .unwrap();
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = MetadataPolicy::read(&source).unwrap();
    let before = volume_index::IndexSnapshot::new(source.begin_read().unwrap());
    assert!(before.source_header(entry.id).unwrap().deleted);
    before
        .authorize_source_writer(&volume, policy.default_permissions(), manager, entry.id)
        .unwrap();
    let projection = ScopedSnapshot::for_publication(&source, &policy, entry.id, manager)
        .unwrap()
        .project(&std::env::temp_dir())
        .unwrap();
    let read = projection.database().begin_read().unwrap();
    let keys = read.iter().map(|row| row.unwrap().0.to_vec()).collect::<Vec<_>>();
    assert_eq!(keys, vec![volume_index::permission_source_key(entry.id).to_vec()]);
    let (clock, bytes) = read.get_with_clock(&keys[0]).unwrap().unwrap();
    let remote = kvdb::sync::EntState::Present { clock, value: &bytes };
    before
        .authorize_source(
            &volume,
            policy.default_permissions(),
            manager,
            &kvdb::sync::Delta { key: &keys[0], local: remote, remote },
        )
        .unwrap();
    let read = source.begin_read().unwrap();
    let key = volume_index::birth_source_key(entry.id);
    let (clock, bytes) = read.get_with_clock(&key).unwrap().unwrap();
    let remote = kvdb::sync::EntState::Present { clock, value: &bytes };
    assert!(matches!(
        before.authorize_source(
            &volume,
            policy.default_permissions(),
            manager,
            &kvdb::sync::Delta { key: &key, local: remote, remote },
        ),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
fn selected_projection_pins_origin_clocks_and_excludes_unrelated_entries_and_builds() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let reader = Id::from_bytes([4; 16]);
    let volume = store.create_volume("Scoped", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "selected", volume_index::EntryKind::KvDb {})
        .unwrap();
    let other = store
        .create_database(&owner, volume.id, "private", volume_index::EntryKind::KvDb {})
        .unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(Principal::Human(reader)),
                        permissions: vec![Permission::Read],
                    },
                    Grant {
                        subject: Subject::Principal(PLAYBIT_REPLICA_PRINCIPAL),
                        permissions: vec![Permission::Read],
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(owner.account)),
                        permissions: vec![Permission::Write],
                    },
                ],
            }),
        )
        .unwrap();
    let build = crate::identifier::encode(&[0xaa; 32]);
    store.provision_version(&owner, volume.id, &build).unwrap();
    store.provision_version(&owner, volume.id, &crate::identifier::encode(&[0xbb; 32])).unwrap();
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&source, DatabaseId::Entry(entry.id)).unwrap();
    assert!(policy.volume.authorize(reader, Permission::Read).is_err());
    let snapshot =
        ScopedSnapshot::new(&source, DatabaseId::Index, &policy, reader, Some(&build)).unwrap();
    let before = source.begin_read().unwrap();
    let expected = before.get_with_clock(&volume_index::entry_key(entry.id)).unwrap().unwrap();
    store.rename_entry(&owner, volume.id, entry.id, "later").unwrap();
    let projected = snapshot.project(&std::env::temp_dir()).unwrap();
    let read = projected.database().begin_read().unwrap();
    let birth = read.get_with_clock(&volume_index::entry_key(entry.id)).unwrap().unwrap();
    assert_eq!(birth.0, expected.0);
    assert_eq!(birth.1.as_ref(), expected.1.as_ref());
    drop(birth);
    let index = volume_index::IndexSnapshot::new(read);
    assert_eq!(index.source_entry(entry.id).unwrap().info.name, "selected");
    assert!(index.source_entry(other.id).is_err());
    assert!(matches!(index.entry(entry.id), Err(Error::NotFound(_))));
    assert_eq!(index.source_headers(None, 64).unwrap().entries.len(), 1);
    let metadata = MetadataPolicy::read(projected.database()).unwrap();
    assert!(metadata.provisioned_versions.contains(&build));
    assert!(!metadata.provisioned_versions.contains(&"bb".repeat(32)));
    assert!(
        ScopedSnapshot::new(&source, DatabaseId::Index, &policy, Id::from_bytes([5; 16]), None)
            .is_err()
    );
}

#[test]
fn revocation_projection_preserves_only_the_accounts_live_record_or_tombstone() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let volume = store.create_volume("Scoped", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "selected", volume_index::EntryKind::KvDb {})
        .unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&index, DatabaseId::Entry(entry.id)).unwrap();
    let source = store.db(volume.id, DatabaseId::Revocations).unwrap();
    let key = owner.account.to_string();
    for deleted in [false, true] {
        let mut tx = source.begin_write(1).unwrap();
        tx.put(key.as_bytes(), &encode(&"revoked").unwrap()).unwrap();
        tx.put(format!("{key}/private").as_bytes(), b"not an account").unwrap();
        tx.put(
            Id::from_bytes([5; 16]).to_string().as_bytes(),
            &encode(&"private revocation").unwrap(),
        )
        .unwrap();
        if deleted {
            tx.del(key.as_bytes()).unwrap();
        }
        tx.commit().unwrap();
        let snapshot =
            ScopedSnapshot::new(&source, DatabaseId::Revocations, &policy, owner.account, None)
                .unwrap();
        let projected = snapshot.project(&std::env::temp_dir()).unwrap();
        let read = projected.database().begin_read().unwrap();
        assert_eq!(read.has(key.as_bytes()).unwrap(), !deleted);
        assert_eq!(read.is_del(key.as_bytes()).unwrap().is_some(), deleted);
        assert!(read.next_source_key(key.as_bytes(), true).unwrap().is_none());
    }
}

#[test]
fn selected_live_receive_rejects_unrelated_facts_before_any_commit_and_needs_no_catalog_read() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let reader = Id::from_bytes([4; 16]);
    let access = Access::owner(reader);
    let volume = store.create_volume("Scoped", true).unwrap();
    let selected = store
        .create_database(&owner, volume.id, "selected", volume_index::EntryKind::KvDb {})
        .unwrap();
    let other = store
        .create_database(&owner, volume.id, "private", volume_index::EntryKind::KvDb {})
        .unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            selected.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(reader)),
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let source = empty_detached_replica(&std::env::temp_dir(), &index).unwrap();
    crate::store::merge_index_replica(
        &std::env::temp_dir(),
        &index,
        source.database(),
        kvdb::Lww,
        |_| Ok(()),
    )
    .unwrap();
    let rename = |entry, name| {
        let mut tx = source.database().begin_write_with_delta(now_unix_secs(), 65_536).unwrap();
        volume_index::rename(
            &mut tx,
            entry,
            name,
            EntryId::generate().unwrap(),
            &crate::store::volume_index::namespace::Origin {
                installation: Id::from_bytes([42; 16]),
                label: None,
            },
        )
        .unwrap();
        let kvdb::CommittedDelta::Bytes(bytes) = tx.commit_with_delta().unwrap() else {
            panic!("bounded rename");
        };
        bytes
    };
    let selected_delta = rename(selected.id, "renamed");
    let private_delta = rename(other.id, "private-renamed");
    let mixed = kvdb::sync::combine_deltas(&selected_delta, &private_delta, 65_536).unwrap();
    let address = volume_index::EntryAddress { volume: volume.id, entry: selected.id };
    let before = index.begin_read().unwrap().root_hash().unwrap();
    assert!(store.merge_received_entry_delta(&access, address, DatabaseId::Index, &mixed).is_err());
    assert_eq!(index.begin_read().unwrap().root_hash().unwrap(), before);
    assert!(store.entries(&access, volume.id, None, 64).is_err());
    let result = store
        .merge_received_entry_delta(&access, address, DatabaseId::Index, &selected_delta)
        .unwrap();
    assert!(result.applied > 0);
    assert_eq!(store.entry(&access, volume.id, selected.id).unwrap().info.name, "renamed");
    assert_eq!(
        store
            .merge_received_entry_delta(&access, address, DatabaseId::Index, &selected_delta)
            .unwrap()
            .applied,
        0
    );
    assert!(store.entry(&access, volume.id, other.id).is_err());
    assert!(store.entries(&access, volume.id, None, 64).is_err());
}

#[test]
fn competing_name_projection_preserves_known_id_without_granting_name_lookup() {
    // Two offline owner copies rename different IDs to one name. A selected
    // reader can retain the losing entry while the winning sibling stays hidden;
    // public name lookup must reject catalog admission before consulting a map.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let reader = Access::owner(Id::from_bytes([4; 16]));
    let volume = store.create_volume("Competing names", true).unwrap();
    let first =
        store.create_database(&owner, volume.id, "one", volume_index::EntryKind::KvDb {}).unwrap();
    let second =
        store.create_database(&owner, volume.id, "two", volume_index::EntryKind::KvDb {}).unwrap();
    let (loser, winner) = (first.id.min(second.id), first.id.max(second.id));
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            loser,
            EntryAcl::Replace(Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(Principal::Human(reader.account)),
                        permissions: vec![Permission::Read],
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(owner.account)),
                        permissions: vec![Permission::Write],
                    },
                ],
            }),
        )
        .unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let a = empty_detached_replica(&std::env::temp_dir(), &index).unwrap();
    let b = empty_detached_replica(&std::env::temp_dir(), &index).unwrap();
    for replica in [a.database(), b.database()] {
        crate::store::merge_index_replica(
            &std::env::temp_dir(),
            &index,
            replica,
            kvdb::Lww,
            |_| Ok(()),
        )
        .unwrap();
    }
    for (replica, entry) in [(a.database(), loser), (b.database(), winner)] {
        let mut tx = replica.begin_write(now_unix_secs()).unwrap();
        volume_index::rename(
            &mut tx,
            entry,
            "shared",
            EntryId::generate().unwrap(),
            &crate::store::volume_index::namespace::Origin {
                installation: Id::from_bytes([42; 16]),
                label: None,
            },
        )
        .unwrap();
        tx.commit().unwrap();
    }
    // Reverse the contributions; the full catalog still picks the greatest ID.
    for replica in [b.database(), a.database()] {
        store.merge_authorized_replica(&owner, volume.id, DatabaseId::Index, replica).unwrap();
    }
    assert_eq!(store.resolve_entry(&owner, volume.id, "shared").unwrap().id, winner);
    let full = store.entry(&reader, volume.id, loser).unwrap();
    assert!(full.name_conflict);
    let policy = DatabasePolicy::read(&index, DatabaseId::Entry(loser)).unwrap();
    let projected = ScopedSnapshot::new(&index, DatabaseId::Index, &policy, reader.account, None)
        .unwrap()
        .project(&std::env::temp_dir())
        .unwrap();
    let snapshot = volume_index::IndexSnapshot::new(projected.database().begin_read().unwrap());
    let selected = snapshot.source_entry(loser).unwrap();
    assert_eq!(selected.info.name, "shared");
    assert_eq!(selected.name_revision, full.name_revision);
    assert!(snapshot.source_entry(winner).is_err());
    assert!(matches!(
        store.resolve_entry(&reader, volume.id, "shared"),
        Err(Error::AccessDenied(_))
    ));
    assert!(store.entries(&reader, volume.id, None, 64).is_err());
}

#[test]
fn selected_writer_root_intents_normalize_without_ancestor_payloads() {
    for (initial, prior_name, destination) in [
        ("folder/file", "folder/current", "moved"),
        ("folder/file", "folder/current", "current"),
        ("file", "current", "current"),
    ] {
        selected_writer_root_intent(initial, prior_name, destination);
    }
}

fn selected_writer_root_intent(initial: &str, prior_name: &str, destination: &str) {
    // A Write-only receiver gets exact controls and authenticated opaque causal
    // evidence. Its root/name assignment must not require omitted ancestry.
    use volume_index::namespace::{observations, receipt, source};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let writer = Id::from_bytes([4; 16]);
    let volume = store.create_volume("Selected intent", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, initial, volume_index::EntryKind::KvDb {})
        .unwrap();
    store.rename_entry(&owner, volume.id, entry.id, prior_name).unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(writer)),
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&index, DatabaseId::Entry(entry.id)).unwrap();
    let projected = ScopedSnapshot::new(&index, DatabaseId::Index, &policy, writer, None)
        .unwrap()
        .project(&std::env::temp_dir())
        .unwrap();
    let selected = projected.database();
    let original_view = source::cut::project(&index.begin_read().unwrap()).unwrap().admission.view;
    let accepted = source::cut::accepted(&index.begin_read().unwrap()).unwrap();
    receipt::retain_fixture(selected, Some(entry.id), &accepted).unwrap();
    let rows = receipt::capture(&selected.begin_read().unwrap(), Some(entry.id)).unwrap();
    let mut tx = selected.begin_write(now_unix_secs() + 1).unwrap();
    let before = observations::Ledger::load(&tx.local_indexes()).unwrap();
    let mut after = before.clone();
    after.apply_receipt(&rows).unwrap();
    after.write_changes(&before, &mut tx.local_indexes()).unwrap();
    for row in &rows {
        let read = selected.begin_read().unwrap();
        let bytes = if row.source.birth {
            read.get(&volume_index::entry_key(row.source.entry)).unwrap().unwrap().to_vec()
        } else {
            read.iter()
                .map(|item| item.unwrap().1.to_vec())
                .find(|bytes| blake3::hash(bytes).as_bytes() == &row.source.source_digest)
                .unwrap()
        };
        volume_index::namespace::payloads::retain(&mut tx.local_indexes(), row, &bytes, 0).unwrap();
    }
    source::cut::project(&tx).unwrap().write_current(&mut tx.local_indexes()).unwrap();
    tx.commit().unwrap();
    let read = selected.begin_read().unwrap();
    if let Some(parent) = entry.info.namespace.parent {
        assert!(!read.has(&volume_index::entry_key(parent)).unwrap());
    }
    assert!(read.iter().all(|item| !volume_index::is_entry_history(&item.unwrap().0)));
    let prior = observations::Publication::capture(&read).unwrap();
    if entry.info.namespace.parent.is_some() {
        assert!(prior.observed.len() > rows.len());
    }
    let snapshot = volume_index::IndexSnapshot::new(read);
    let metadata = MetadataPolicy::read(&index).unwrap();
    assert!(metadata.authorize(writer, Permission::Read).is_err());
    snapshot
        .authorize(
            metadata.volume(),
            metadata.default_permissions(),
            Principal::Human(writer),
            entry.id,
            Permission::Write,
        )
        .unwrap();
    drop(snapshot);
    let revision = EntryId::generate().unwrap();
    let mut tx = selected.begin_write_with_delta(now_unix_secs() + 2, 65_536).unwrap();
    volume_index::assign_entry_root(
        &mut tx,
        entry.id,
        destination,
        revision,
        &volume_index::namespace::Origin { installation: store.installation(), label: None },
    )
    .unwrap();
    let kvdb::CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else {
        panic!("bounded selected source assignment");
    };
    let snapshot = volume_index::IndexSnapshot::new(selected.begin_read().unwrap());
    let event = snapshot.revision(entry.id, revision).unwrap();
    assert!(matches!(event.change, volume_index::EntryChange::Move { parent: None, .. }));
    assert_eq!(event.observed, prior.observed);
    volume_index::apply_delta(&index, &delta, now_unix_secs() + 3, |prior, delta| {
        prior.authorize_source(metadata.volume(), metadata.default_permissions(), writer, delta)
    })
    .unwrap();
    let admission = source::cut::project(&index.begin_read().unwrap()).unwrap().admission;
    assert!(
        admission
            .events
            .values()
            .all(|state| *state == volume_index::namespace::EventState::Accepted)
    );
    assert_eq!(admission.view.entries[&entry.id].parent, None);
    let resolved = &admission.view.entries[&entry.id];
    assert_eq!(resolved.name, destination);
    if destination == "current" {
        assert_eq!(resolved.name_event, original_view.entries[&entry.id].name_event);
    }
    if entry.info.namespace.parent.is_none() {
        assert_eq!(resolved.parent_event, original_view.entries[&entry.id].parent_event);
    }
    assert!(store.resolve_entry(&Access::owner(writer), volume.id, "moved").is_err());
    assert!(
        store.rename_entry(&Access::owner(writer), volume.id, entry.id, "secret/path").is_err()
    );
}

#[test]
fn selected_writer_cannot_commit_a_known_directory_path_overflow() {
    // Entry Write does not grant catalog Read, but physically available
    // evidence must still reject a mutation that exceeds the path bound.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let writer = Id::from_bytes([4; 16]);
    let volume = store.create_volume("Bounded intent", true).unwrap();
    let path = format!("a/{}/file", vec!["b".repeat(250); 16].join("/"));
    let child =
        store.create_database(&owner, volume.id, &path, volume_index::EntryKind::KvDb {}).unwrap();
    let root = store.resolve_entry(&owner, volume.id, "a").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            root.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(writer)),
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let before = index.begin_read().unwrap().root_hash().unwrap();
    assert!(matches!(
        store.rename_entry(&Access::owner(writer), volume.id, root.id, &"r".repeat(255)),
        Err(Error::LimitExceeded)
    ));
    assert_eq!(index.begin_read().unwrap().root_hash().unwrap(), before);
    assert_eq!(store.resolve_entry(&owner, volume.id, &path).unwrap().id, child.id);
}

#[test]
fn selected_recovery_transfers_qualified_controls_behind_a_pending_raw_head() {
    // Both Read and Write-only recovery must attest the accepted current name,
    // even when a newer raw head waits on an unread dependency. Write-only
    // projection still omits history and the original clocks remain exact.
    use volume_index::namespace::{self as ns, source::cut};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account().id);
    let writer = Id::from_bytes([4; 16]);
    let volume = store.create_volume("Qualified controls", true).unwrap();
    let entry = store
        .create_database(&owner, volume.id, "initial", volume_index::EntryKind::KvDb {})
        .unwrap();
    store.rename_entry(&owner, volume.id, entry.id, "accepted").unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(Principal::Human(writer)),
                        permissions: vec![Permission::Write],
                    },
                    Grant {
                        subject: Subject::Principal(Principal::Human(owner.account)),
                        permissions: vec![Permission::Read, Permission::Write],
                    },
                ],
            }),
        )
        .unwrap();
    let source = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&source, DatabaseId::Entry(entry.id)).unwrap();
    let older = ScopedSnapshot::for_reconciliation(
        &source,
        DatabaseId::Index,
        &policy,
        owner.account,
        None,
    )
    .unwrap();
    let read = source.begin_read().unwrap();
    let current = cut::current::Record::read(&read, entry.id).unwrap().unwrap();
    let name = current.name.unwrap();
    let name_tag = current
        .fields()
        .into_iter()
        .find(|(_, value)| value.is_some_and(|value| value.revision == name.revision))
        .unwrap()
        .0;
    let name_key = [vec![volume_index::META_BYTE, name_tag], entry.id.as_bytes().to_vec()].concat();
    let accepted = read.get_with_clock(&name_key).unwrap().unwrap();
    let accepted_clock = accepted.0;
    let accepted_bytes = accepted.1.to_vec();
    let name_event =
        ns::event_id(entry.id, name.revision, *blake3::hash(&accepted_bytes).as_bytes()).unwrap();
    drop(accepted);
    let mut pending =
        volume_index::IndexSnapshot::new(read).revision(entry.id, name.revision).unwrap();
    pending.revision = EntryId::from_bytes([249; 15]).unwrap();
    pending.observed.push(EntryId::from_bytes([248; 15]).unwrap());
    pending.observed.sort_unstable();
    pending.change = volume_index::EntryChange::Name { name: "pending".into() };
    let mut tx = source.begin_write(now_unix_secs() + 1).unwrap();
    pending.origin_clock = tx.clock().as_u64();
    let pending_bytes = record::encode(&pending).unwrap();
    tx.put(&name_key, &pending_bytes).unwrap();
    let mut history_key = volume_index::entry_source_prefixes(entry.id, true)
        .into_iter()
        .find(|key| volume_index::is_entry_history(key))
        .unwrap();
    history_key.extend_from_slice(pending.revision.as_bytes());
    tx.put(&history_key, &pending_bytes).unwrap();
    cut::retain_accepted(&mut tx.local_indexes()).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        volume_index::IndexSnapshot::new(source.begin_read().unwrap())
            .entry(entry.id)
            .unwrap()
            .info
            .name,
        "accepted"
    );
    assert_eq!(
        source.begin_read().unwrap().get(&name_key).unwrap().unwrap().as_ref(),
        pending_bytes
    );
    let policy = DatabasePolicy::read(&source, DatabaseId::Entry(entry.id)).unwrap();
    // Raw reconciliation cannot downgrade a newer remote head or depend on
    // qualified local rows in a detached stage.
    let stage = empty_detached_replica(&std::env::temp_dir(), &source).unwrap();
    ScopedSnapshot::for_reconciliation(&source, DatabaseId::Index, &policy, owner.account, None)
        .unwrap()
        .merge_into(stage.database())
        .unwrap();
    older.merge_into(stage.database()).unwrap();
    assert_eq!(
        volume_index::IndexSnapshot::new(stage.database().begin_read().unwrap())
            .source_entry(entry.id)
            .unwrap()
            .info
            .name,
        "pending"
    );
    let target = empty_detached_replica(&std::env::temp_dir(), &source).unwrap();
    ScopedSnapshot::for_reconciliation(
        stage.database(),
        DatabaseId::Index,
        &policy,
        owner.account,
        None,
    )
    .unwrap()
    .merge_into(target.database())
    .unwrap();
    let accepted = cut::accepted(&source.begin_read().unwrap()).unwrap();
    for account in [owner.account, writer] {
        let projected = ScopedSnapshot::new(&source, DatabaseId::Index, &policy, account, None)
            .unwrap()
            .project(&std::env::temp_dir())
            .unwrap();
        let read = projected.database().begin_read().unwrap();
        let selected_name = read.get_with_clock(&name_key).unwrap().unwrap();
        assert_eq!(selected_name.0, accepted_clock);
        assert_eq!(selected_name.1.as_ref(), accepted_bytes);
        drop(selected_name);
        if account == writer {
            assert!(read.iter().all(|row| !volume_index::is_entry_history(&row.unwrap().0)));
        }
        let delivered = cut::delivered_ids(&read, &accepted, Some(entry.id)).unwrap();
        assert!(delivered.contains_key(&name_event));
        drop(read);
        ns::receipt::retain_fixture(projected.database(), Some(entry.id), &accepted).unwrap();
        let rows =
            ns::receipt::capture(&projected.database().begin_read().unwrap(), Some(entry.id))
                .unwrap();
        assert!(rows.iter().any(|row| row.source.id().unwrap() == name_event));
    }
}
