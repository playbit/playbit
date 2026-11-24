#![allow(clippy::unwrap_used, reason = "Tests exercise durable permission configuration saves.")]
use super::*;
use crate::store::volume_index;

fn custom_grants(count: u16) -> Acl {
    Acl {
        grants: (0..count)
            .map(|index| {
                let mut id = [7; 16];
                id[..2].copy_from_slice(&index.to_be_bytes());
                Grant {
                    subject: Subject::Principal(Principal::Human(Id::from_bytes(id))),
                    permissions: vec![
                        Permission::Read,
                        Permission::Write,
                        Permission::Append,
                        Permission::ManagePermissions,
                    ],
                }
            })
            .collect(),
    }
}

fn account_acl(account: Id, permissions: &[Permission]) -> Acl {
    Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(account)),
            permissions: permissions.to_vec(),
        }],
    }
}

#[test]
fn delegated_management_uses_the_current_entry_acl_without_granting_volume_authority() {
    // Exercise the real setter and details: entry delegation never grants a sibling or volume control.
    let (_directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let other = store
        .create_database(&owner, volume.id, "other", volume_index::EntryKind::KvDb {})
        .unwrap();
    let manager = Access::owner(Id::from_bytes([91; 16]));
    let full = account_acl(
        manager.account,
        &[Permission::Read, Permission::Write, Permission::Append, Permission::ManagePermissions],
    );
    store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(full.clone())).unwrap();
    assert!(store.entry_details(&manager, volume.id, entry).unwrap().manage_permissions);
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, other.id, EntryAcl::Inherit),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(
        store.set_permissions(&manager, volume.id, full.clone()),
        Err(Error::AccessDenied(_))
    ));
    let mut edited = full.clone();
    edited.grants.push(Grant { subject: Subject::Public, permissions: vec![Permission::Read] });
    store
        .set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(edited.clone()))
        .unwrap();
    assert_eq!(
        store.entry_details(&manager, volume.id, entry).unwrap().permissions,
        EntryAcl::Replace(edited)
    );
    let invalid = Acl {
        grants: vec![Grant {
            subject: Subject::Public,
            permissions: vec![Permission::Read, Permission::ManagePermissions],
        }],
    };
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(invalid)),
        Err(Error::InvalidInput(_))
    ));
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry,
            EntryAcl::Replace(account_acl(manager.account, &[Permission::Read, Permission::Write])),
        )
        .unwrap();
    assert!(!store.entry_details(&manager, volume.id, entry).unwrap().manage_permissions);
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(full)),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
fn retained_preferences_are_visible_only_to_current_managers_and_never_authorize_restoration() {
    // Save a delegated draft locally, then withdraw current authority without discarding its bytes.
    let (_directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let manager = Access::owner(Id::from_bytes([92; 16]));
    let configured =
        account_acl(manager.account, &[Permission::Read, Permission::ManagePermissions]);
    store.set_permissions(&owner, volume.id, configured.clone()).unwrap();
    store
        .set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(configured.clone()))
        .unwrap();
    store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Inherit).unwrap();
    let details = store.entry_details(&manager, volume.id, entry).unwrap();
    assert!(details.manage_permissions && !details.writable);
    assert_eq!(details.retained_permissions, Some(configured.clone()));
    assert_eq!(store.retained_entry_permissions(owner.account, volume.id, entry).unwrap(), None);
    store
        .set_permissions(&owner, volume.id, account_acl(manager.account, &[Permission::Read]))
        .unwrap();
    let details = store.entry_details(&manager, volume.id, entry).unwrap();
    assert!(!details.manage_permissions);
    assert_eq!(details.retained_permissions, None);
    assert_eq!(
        store.retained_entry_permissions(manager.account, volume.id, entry).unwrap(),
        Some(configured.clone())
    );
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(configured)),
        Err(Error::AccessDenied(_))
    ));
    assert_eq!(
        store.entry_details(&owner, volume.id, entry).unwrap().permissions,
        EntryAcl::Inherit
    );
}

#[test]
fn delegated_management_rechecks_replace_overrides_revocations_and_current_authority() {
    // Current entry policy supersedes defaults; a persisted authority fence and revocation each veto writes.
    use crate::store::authority::{Barrier, barrier_key};
    let (_directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let manager = Access::owner(Id::from_bytes([93; 16]));
    let delegated =
        account_acl(manager.account, &[Permission::Read, Permission::ManagePermissions]);
    store.set_permissions(&owner, volume.id, delegated).unwrap();
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry,
            EntryAcl::Replace(account_acl(manager.account, &[Permission::Read])),
        )
        .unwrap();
    assert!(!store.entry_details(&manager, volume.id, entry).unwrap().manage_permissions);
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Inherit),
        Err(Error::AccessDenied(_))
    ));
    store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Inherit).unwrap();
    let index = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let mut tx = index.begin_write(now_unix_secs()).unwrap();
    tx.local_indexes()
        .put(&barrier_key(manager.account), &Barrier::default().invalidate().encode())
        .unwrap();
    tx.commit().unwrap();
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Inherit),
        Err(Error::AccessDenied(_))
    ));
    let mut tx = index.begin_write(now_unix_secs()).unwrap();
    tx.local_indexes().del(&barrier_key(manager.account)).unwrap();
    tx.commit().unwrap();
    assert!(store.entry_details(&manager, volume.id, entry).unwrap().manage_permissions);
    store.revoke(&owner, volume.id, manager.account, "withdraw management").unwrap();
    assert!(matches!(
        store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Inherit),
        Err(Error::ParticipantRevoked { .. })
    ));
    assert!(matches!(
        store.entry_details(&manager, volume.id, entry),
        Err(Error::ParticipantRevoked { .. })
    ));
}

#[test]
fn delegated_management_keeps_session_denials_and_personal_volume_consent_boundaries() {
    // A root session can use current management; neither a denied session nor personal file consent can.
    let (_directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let manager = Id::from_bytes([94; 16]);
    store
        .set_permissions(
            &owner,
            volume.id,
            account_acl(manager, &[Permission::Read, Permission::ManagePermissions]),
        )
        .unwrap();
    let build = crate::identifier::encode(&[41; 32]);
    store.provision_version(&owner, volume.id, &build).unwrap();
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: owner.account, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            volume.id,
        )
        .unwrap();
    let personal = store.create_volume("Personal", false).unwrap();
    let personal_entry = store
        .create_database(&owner, personal.id, "private", volume_index::EntryKind::KvDb {})
        .unwrap();
    for write in [false, true] {
        store
            .select_personal_volumes(
                &session.id,
                vec![PersonalVolumeGrant { volume: personal.id, write }],
            )
            .unwrap();
        let personal_access = Access::session(owner.account, &session.id);
        assert!(matches!(
            store.set_entry_permissions(
                &personal_access,
                personal.id,
                personal_entry.id,
                EntryAcl::Inherit,
            ),
            Err(Error::AccessDenied(_))
        ));
        assert!(
            !store
                .entry_details(&personal_access, personal.id, personal_entry.id)
                .unwrap()
                .manage_permissions
        );
    }
    let access = Access::session(manager, &session.id);
    store.set_entry_permissions(&access, volume.id, entry, EntryAcl::Inherit).unwrap();
    assert!(store.entry_details(&access, volume.id, entry).unwrap().manage_permissions);
    store.record_session_denial(&session, "session revoked").unwrap();
    assert!(matches!(
        store.set_entry_permissions(&access, volume.id, entry, EntryAcl::Inherit),
        Err(Error::AccessDenied(_))
    ));
    assert!(matches!(store.entry_details(&access, volume.id, entry), Err(Error::AccessDenied(_))));
}

#[test]
fn delegated_self_removal_keeps_only_its_exact_acl_publication_after_restart() {
    // Commit self-demotion/removal, reopen, and forward only the admitted ACL while new edits remain denied.
    use crate::store::{DatabaseId, DatabasePolicy, ScopedSnapshot};
    for keep_read in [false, true] {
        let (directory, store, volume, owner) = setup();
        let entry = data(&store, volume.id).entry;
        let manager = Access::owner(Id::from_bytes([95; 16]));
        let full = account_acl(
            manager.account,
            &[
                Permission::Read,
                Permission::Write,
                Permission::Append,
                Permission::ManagePermissions,
            ],
        );
        store
            .set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(full.clone()))
            .unwrap();
        let next = if keep_read {
            account_acl(manager.account, &[Permission::Read])
        } else {
            Acl::default()
        };
        store
            .set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(next.clone()))
            .unwrap();
        assert!(matches!(
            store.set_entry_permissions(&manager, volume.id, entry, EntryAcl::Replace(full)),
            Err(Error::AccessDenied(_))
        ));
        drop(store);

        let store = LocalStore::open(directory.path()).unwrap();
        let index = store.db(volume.id, DatabaseId::Index).unwrap();
        let key = volume_index::permission_source_key(entry);
        let snapshot = volume_index::IndexSnapshot::new(index.begin_read().unwrap());
        assert!(snapshot.permission_publication(manager.account, entry).unwrap());
        assert!(!snapshot.permission_publication(owner.account, entry).unwrap());
        assert!(
            snapshot
                .authorize_source_writer(&volume, &Acl::default(), manager.account, entry)
                .is_err(),
            "outgoing receipt cannot grant incoming stream admission"
        );
        let kvdb::CommittedDelta::Bytes(bytes) = index
            .begin_read()
            .unwrap()
            .delta_for_key(&key, crate::store::MAX_REPLICA_DELTA_BYTES)
            .unwrap()
            .unwrap()
        else {
            panic!()
        };
        let policy = DatabasePolicy::read(&index, DatabaseId::Index).unwrap();
        assert_eq!(
            crate::store::authorize_publication_delta(&index, &policy, manager.account, &bytes)
                .unwrap(),
            vec![entry]
        );
        #[cfg(feature = "connected-engine")]
        assert!(
            store
                .publication_delta(&manager, volume.id, DatabaseId::Index, &bytes)
                .unwrap()
                .is_some_and(|delta| !delta.is_empty())
        );
        let projected = ScopedSnapshot::for_publication(
            &index,
            &store.index(volume.id).unwrap(),
            entry,
            manager.account,
        )
        .unwrap()
        .project(&std::env::temp_dir())
        .unwrap();
        let read = projected.database().begin_read().unwrap();
        let keys = read.iter().map(|row| row.unwrap().0.to_vec()).collect::<Vec<_>>();
        assert_eq!(keys, vec![key.to_vec()], "receipt projection carries only its one ACL fact");
        let projected = volume_index::IndexSnapshot::new(read);
        assert_eq!(projected.permissions(entry).unwrap(), EntryAcl::Replace(next));
        assert!(!projected.permission_publication(manager.account, entry).unwrap());

        #[cfg(feature = "connected-engine")]
        {
            store.revoke(&owner, volume.id, manager.account, "permission manager revoked").unwrap();
            assert!(matches!(
                store.publication_delta(&manager, volume.id, DatabaseId::Index, &bytes),
                Err(Error::ParticipantRevoked { .. })
            ));
        }
    }
}

#[test]
fn disabling_larger_existing_acls_keeps_legacy_source_bytes_and_is_device_local() {
    // The previous valid ACL capacity remains supported; only ordinary Inherit bytes enter replication.
    let (directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let configured = custom_grants(300);
    store
        .set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(configured.clone()))
        .unwrap();
    assert!(!store.entry_details(&owner, volume.id, entry).unwrap().readable);
    store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Inherit).unwrap();
    let index = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let mut key = vec![3, 2];
    key.extend_from_slice(entry.as_bytes());
    assert_eq!(
        index.begin_read().unwrap().get(&key).unwrap().unwrap().as_ref(),
        b"PBR\x01\x0a\x01\x00\x08"
    );
    let source_keys =
        index.begin_read().unwrap().iter().map(|item| item.unwrap().0.to_vec()).collect::<Vec<_>>();
    assert!(!source_keys.iter().any(|key| key.starts_with(b"settings/")));
    let disabled = store.entry_details(&owner, volume.id, entry).unwrap();
    assert!(disabled.readable);
    assert_eq!(disabled.retained_permissions, Some(configured.clone()));
    assert_eq!(
        store.retained_entry_permissions(Id::from_bytes([99; 16]), volume.id, entry).unwrap(),
        None
    );
    assert_eq!(
        store.retained_entry_permissions(owner.account, Id::from_bytes([99; 16]), entry).unwrap(),
        None
    );
    drop(index);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(
        store.entry_details(&owner, volume.id, entry).unwrap().retained_permissions,
        Some(configured)
    );
}

#[test]
fn failed_local_persistence_leaves_active_permissions_unchanged() {
    // A busy preference writer prevents the disable before its authoritative ACL transaction begins.
    let (_directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let configured = custom_grants(65);
    store
        .set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(configured.clone()))
        .unwrap();
    let pending = store.metadata.begin_write(now_unix_secs()).unwrap();
    assert!(store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Inherit).is_err());
    drop(pending);
    let details = store.entry_details(&owner, volume.id, entry).unwrap();
    assert_eq!(details.permissions, EntryAcl::Replace(configured));
    assert_eq!(store.retained_entry_permissions(owner.account, volume.id, entry).unwrap(), None);
}

#[test]
fn failed_active_write_cannot_activate_remembered_grants() {
    // A preference may commit before a failed active write; current Replace still wins presentation and access.
    let (_directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let active = custom_grants(65);
    let remembered = custom_grants(66);
    store
        .set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(active.clone()))
        .unwrap();
    let index = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let pending = index.begin_write(now_unix_secs()).unwrap();
    assert!(
        store
            .set_entry_permissions_with_retained(
                &owner,
                volume.id,
                entry,
                EntryAcl::Inherit,
                Some(remembered.clone())
            )
            .is_err()
    );
    drop(pending);
    let details = store.entry_details(&owner, volume.id, entry).unwrap();
    assert_eq!(details.permissions, EntryAcl::Replace(active.clone()));
    assert_eq!(details.retained_permissions, Some(active));
    assert!(!details.readable);
    assert_eq!(
        store.retained_entry_permissions(owner.account, volume.id, entry).unwrap(),
        Some(remembered)
    );
}

#[test]
fn configured_grants_survive_disable_save_reopen_and_explicit_replacement() {
    // Reopen the actual store without a surviving handle, then restore only its saved custom configuration.
    let (directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    let configured = Acl {
        grants: vec![Grant {
            subject: Subject::Principal(Principal::Human(owner.account)),
            permissions: vec![Permission::Read],
        }],
    };
    store
        .set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(configured.clone()))
        .unwrap();
    store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Inherit).unwrap();
    let disabled = store.entry_details(&owner, volume.id, entry).unwrap();
    assert_eq!(disabled.permissions, EntryAcl::Inherit);
    assert_eq!(disabled.retained_permissions, Some(configured.clone()));
    assert_ne!(disabled.created_clock, 0);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    let reopened = store.entry_details(&owner, volume.id, entry).unwrap();
    assert_eq!(reopened.permissions, EntryAcl::Inherit);
    assert_eq!(reopened.retained_permissions, Some(configured.clone()));
    assert_eq!(reopened.created_clock, disabled.created_clock);
    let invalid = Acl {
        grants: vec![Grant {
            subject: Subject::Public,
            permissions: vec![Permission::ManagePermissions],
        }],
    };
    assert!(
        store
            .set_entry_permissions_with_retained(
                &owner,
                volume.id,
                entry,
                EntryAcl::Inherit,
                Some(invalid)
            )
            .is_err()
    );
    assert!(
        store
            .set_entry_permissions_with_retained(
                &owner,
                volume.id,
                entry,
                EntryAcl::Replace(configured.clone()),
                Some(Acl::default())
            )
            .is_err()
    );
    assert_eq!(store.entry_details(&owner, volume.id, entry).unwrap(), reopened);
    let restored = reopened.retained_permissions.unwrap();
    store
        .set_entry_permissions(&owner, volume.id, entry, EntryAcl::Replace(restored.clone()))
        .unwrap();
    assert_eq!(
        store.entry_details(&owner, volume.id, entry).unwrap().permissions,
        EntryAcl::Replace(configured)
    );
    store
        .set_entry_permissions_with_retained(
            &owner,
            volume.id,
            entry,
            EntryAcl::Inherit,
            Some(Acl::default()),
        )
        .unwrap();
    assert_eq!(
        store.entry_details(&owner, volume.id, entry).unwrap().retained_permissions,
        Some(Acl::default())
    );
    store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Inherit).unwrap();
    assert_eq!(
        store.entry_details(&owner, volume.id, entry).unwrap().retained_permissions,
        Some(Acl::default())
    );
}

#[cfg(feature = "connected-engine")]
#[test]
fn owner_control_repair_with_only_a_receipt_leaves_initialization_to_the_capture() {
    use crate::store::DatabaseId;
    let (directory, store, volume, _) = setup();
    let local = store.db(volume.id, DatabaseId::Index).unwrap();
    let key = local.meta().hash_key;
    let captured =
        create_replica(&directory.path().join("captured.kvdb"), key, 512 * 1024).unwrap();
    pull(&local, &captured);
    let receipt = DurableRevision {
        revision: captured.clock().as_u64(),
        policy_revision: MetadataPolicy::read(&captured).unwrap().revision(),
        root: captured.begin_read().unwrap().root_hash().unwrap(),
    };
    store.acknowledge_replica(volume.id, DatabaseId::Index, &receipt).unwrap();
    drop(local);
    drop(store);
    let path = directory
        .path()
        .join(crate::paths::VOLUMES)
        .join(volume.id.to_string())
        .join(DatabaseId::Index.file_name());
    fs::rename(&path, directory.path().join("prior-index.kvdb")).unwrap();
    drop(create_replica(&path, key, 512 * 1024).unwrap());
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(
        !store
            .restore_owner_service_read(volume.id, &captured, |_| panic!(
                "empty index has no controls"
            ))
            .unwrap()
    );
}

#[cfg(feature = "connected-engine")]
#[test]
fn owner_service_restoration_requires_retained_identity_and_never_qualifies_reads() {
    // Exercise owner admission against captured restrictive controls and callback
    // failures; ACL-only publication must keep the read fence and Index receipt.
    use crate::store::DatabaseId;
    let (directory, store, volume, owner) = setup();
    let entry = data(&store, volume.id).entry;
    store
        .set_entry_permissions(
            &owner,
            volume.id,
            entry,
            EntryAcl::Replace(account_acl(owner.account, &[Permission::Read])),
        )
        .unwrap();
    let local = store.db(volume.id, DatabaseId::Index).unwrap();
    let remote =
        create_replica(&directory.path().join("captured.kvdb"), local.meta().hash_key, 512 * 1024)
            .unwrap();
    pull(&local, &remote);
    store.set_entry_permissions(&owner, volume.id, entry, EntryAcl::Inherit).unwrap();
    let mut publications = 0;
    assert!(
        !store
            .restore_owner_service_read(volume.id, &remote, |_| {
                publications += 1;
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(
        publications, 0,
        "an owner without a hosted Index receipt cannot publish recovery controls"
    );
    let receipt = DurableRevision {
        revision: remote.clock().as_u64(),
        policy_revision: MetadataPolicy::read(&remote).unwrap().revision(),
        root: remote.begin_read().unwrap().root_hash().unwrap(),
    };
    store.acknowledge_replica(volume.id, DatabaseId::Index, &receipt).unwrap();
    store.invalidate_authority(&owner, volume.id).unwrap();
    assert!(matches!(
        store.restore_owner_service_read(volume.id, &remote, |_| Err(Error::System(
            "publication failed".into()
        ))),
        Err(Error::System(_))
    ));
    assert!(store.authority_refresh_required(volume.id).unwrap());
    assert_eq!(
        store.durable_revision(volume.id, DatabaseId::Index).unwrap(),
        Some(receipt.clone())
    );
    let mut keys = Vec::new();
    assert!(
        store
            .restore_owner_service_read(volume.id, &remote, |bytes| {
                kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, _| {
                    keys.push(key.to_vec())
                })?;
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(keys, vec![volume_index::permission_source_key(entry).to_vec()]);
    assert!(
        store.authority_refresh_required(volume.id).unwrap(),
        "control acknowledgment does not admit retained app data"
    );
    assert_eq!(store.durable_revision(volume.id, DatabaseId::Index).unwrap(), Some(receipt));

    let mut changed_identity = MetadataPolicy::read(&remote).unwrap();
    changed_identity.volume.owner = Id::from_bytes([98; 16]);
    changed_identity.write(&remote, now_unix_secs()).unwrap();
    assert!(matches!(
        store.restore_owner_service_read(volume.id, &remote, |_| {
            publications += 1;
            Ok(())
        }),
        Err(Error::AccessDenied(_))
    ));
    changed_identity.volume.owner = owner.account;
    changed_identity.write(&remote, now_unix_secs()).unwrap();
    pull(&local, &remote);
    assert!(
        !store
            .restore_owner_service_read(volume.id, &remote, |_| {
                publications += 1;
                Ok(())
            })
            .unwrap(),
        "already-published permissions are not replayed"
    );
    let mut participant_volume = volume.clone();
    participant_volume.owner = Id::from_bytes([98; 16]);
    put(&store.metadata, format!("volume/{}", volume.id).as_bytes(), &participant_volume).unwrap();
    assert!(
        !store
            .restore_owner_service_read(volume.id, &remote, |_| {
                publications += 1;
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(publications, 0);
}
