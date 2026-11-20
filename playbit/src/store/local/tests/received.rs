#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::store::{DatabaseId, EntryId, volume_index};

fn pair() -> (tempfile::TempDir, LocalStore, tempfile::TempDir, LocalStore, Volume, Access, Access)
{
    let (alice_dir, alice, volume, owner) = setup();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            },
        )
        .unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    alice.provision_version(&owner, volume.id, &version).unwrap();
    let session = alice
        .create_session(
            &crate::package::AppIdentity { publisher: alice.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            volume.id,
        )
        .unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    bob.import_invitation_link(
        &alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        &alice.session_envelope(&session.id).unwrap(),
        crate::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    sync_volume(&alice, &bob, volume.id);
    let participant = Access::session(bob.account().id, &session.id);
    (alice_dir, alice, bob_dir, bob, volume, owner, participant)
}
#[cfg(feature = "connected-engine")]
fn qualify(stage: &mut ReceivedVolume) {
    // This fixture stands in for the authenticated AP full-cut acceptance pass.
    // Retain the same exact-root evidence required by production publication.
    use crate::store::volume_index::namespace::source::cut;
    let accepted = stage
        .with_database(DatabaseId::Index, |index| cut::accepted(&index.begin_read()?))
        .unwrap();
    qualify_accepted(stage, &accepted);
}
#[cfg(feature = "connected-engine")]
fn qualify_accepted(
    stage: &mut ReceivedVolume,
    accepted: &BTreeMap<EntryId, volume_index::namespace::Assignments>,
) {
    let scope = stage.entry_scope();
    stage
        .with_database(DatabaseId::Index, |index| {
            volume_index::namespace::receipt::retain_fixture(index, scope, accepted)
        })
        .unwrap();
    stage.qualify_authority();
}

pub(in crate::store::local) fn receive(
    source: &LocalStore,
    target: &LocalStore,
    volume: Id,
) -> ReceivedVolume {
    let stage = target.prepare_received_volume(volume).unwrap();
    let mut after = None;
    loop {
        let page = source.replication_database_page(volume, after).unwrap();
        for database in page.databases {
            source
                .with_replica(volume, database, |source| {
                    stage.with_database(database, |target| {
                        if database == crate::store::DatabaseId::Index {
                            merge_index_replica(
                                &std::env::temp_dir(),
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            merge_replica(&std::env::temp_dir(), source, target, kvdb::Lww, |_| {
                                Ok(())
                            })
                        }
                    })
                })
                .unwrap();
            stage.finish_database(database).unwrap();
        }
        let Some(next) = page.next else { break };
        after = Some(next);
    }
    let mut after = None;
    loop {
        let page = stage.database_page(after).unwrap();
        for database in page.databases {
            let DatabaseId::Entry(id) = database else { unreachable!() };
            let protocol = stage
                .with_database(DatabaseId::Index, |index| {
                    Ok(DatabasePolicy::read_source(index, database)?.entry.unwrap().protocol)
                })
                .unwrap();
            if protocol != Some(crate::store::volume_index::DatabaseProtocol::BlobLog) {
                continue;
            }
            let mut cursor = None;
            loop {
                let page = source
                    .bloblog_get(&Access::owner(source.account().id), volume, id, cursor.as_ref())
                    .unwrap();
                stage.receive_log_page(id, &page).unwrap();
                if !page.more {
                    break;
                }
                cursor = Some(page.cursor);
            }
            stage.finish_database(database).unwrap();
        }
        let Some(next) = page.next else { break };
        after = Some(next);
    }
    stage
}

#[test]
fn received_unsupported_format_keeps_the_current_volume_unchanged() {
    // Format admission precedes namespace decoding and publication, even when
    // the received index also contains a declaration the client cannot parse.
    let (_alice_dir, alice, _bob_dir, bob, volume, owner, participant) = pair();
    let entry =
        alice.create_database(&owner, volume.id, "kept", volume_index::EntryKind::KvDb {}).unwrap();
    sync_volume(&alice, &bob, volume.id);
    let before = bob.with_replica(volume.id, DatabaseId::Index, replica_root).unwrap();
    let stage = receive(&alice, &bob, volume.id);
    stage
        .with_database(DatabaseId::Index, |db| {
            let mut tx = db.begin_write(now_unix_secs())?;
            tx.del(&crate::store::catalog::FORMAT_KEY)?;
            let mut key = vec![volume_index::ENTRY_ID_BYTE];
            key.extend_from_slice(entry.id.as_bytes());
            tx.put(&key, b"{\"unknown_entry_schema\":true}")?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        bob.adopt_received_volume(&participant, &stage),
        Err(Error::UnsupportedVolumeFormat)
    ));
    assert_eq!(bob.with_replica(volume.id, DatabaseId::Index, replica_root).unwrap(), before);
    assert!(bob.index(volume.id).is_ok());
}

#[cfg(feature = "connected-engine")]
fn receive_selected(
    source: &LocalStore,
    target: &LocalStore,
    address: volume_index::EntryAddress,
    participant: &Access,
) -> ReceivedVolume {
    let stage = target.prepare_received_entry(address).unwrap();
    let source_index = source.db(address.volume, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&source_index, DatabaseId::Entry(address.entry)).unwrap();
    let mut databases = vec![DatabaseId::Index, DatabaseId::Revocations];
    if policy.metadata_readable(participant.account).unwrap() {
        databases.push(DatabaseId::Entry(address.entry));
    }
    for database in databases {
        source
            .with_replica(address.volume, database, |source| {
                stage.with_database(database, |target| {
                    if database == DatabaseId::Entry(address.entry) {
                        merge_replica(&std::env::temp_dir(), source, target, kvdb::Lww, |_| Ok(()))
                    } else {
                        // Recipient snapshots select qualified controls into an
                        // empty outbound replica before ordinary wire merging.
                        let projected = super::super::scoped::ScopedSnapshot::new(
                            source,
                            database,
                            &policy,
                            participant.account,
                            Some(&crate::identifier::encode(&[0xab; 32])),
                        )?
                        .project(&std::env::temp_dir())?;
                        if database == DatabaseId::Index {
                            merge_index_replica(
                                &std::env::temp_dir(),
                                projected.database(),
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            merge_replica(
                                &std::env::temp_dir(),
                                projected.database(),
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        }
                    }
                })
            })
            .unwrap();
        stage.finish_database(database).unwrap();
    }
    stage
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_restores_rejected_name_and_retains_revision() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice
        .create_database(
            &owner,
            volume.id,
            "accepted",
            crate::store::volume_index::EntryKind::KvDb {},
        )
        .unwrap();
    let stage = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    bob.rename_entry(&participant, volume.id, entry.id, "offline-rejected").unwrap();
    let rejected = bob.entry(&participant, volume.id, entry.id).unwrap().name_revision;
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    assert_eq!(bob.entry(&participant, volume.id, entry.id).unwrap().info.name, "accepted");
    assert!(bob.resolve_entry(&participant, volume.id, "offline-rejected").is_err());
    assert_eq!(bob.resolve_entry(&participant, volume.id, "accepted").unwrap().id, entry.id);
    let retained = bob
        .entry_revisions(
            &participant,
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id },
            None,
            64,
        )
        .unwrap();
    assert!(retained.revisions.iter().any(|revision| revision.revision == rejected));
    let address = crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let page = bob.entry_revisions(&participant, address, cursor, 1).unwrap();
        paged.extend(page.revisions);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
        assert!(paged.len() <= retained.revisions.len());
    }
    assert_eq!(paged, retained.revisions);
    assert_eq!(paged[0].revision, rejected);
    let cuts = bob.rejected_entry_cuts(&participant, address).unwrap();
    let archive =
        bob.path.join(crate::paths::RECEIVED).join(cuts[0].to_string()).join("rejected/index.kvdb");
    {
        let archived = open_replica(&archive, 512 * 1024).unwrap();
        let mut tx = archived.begin_write(now_unix_secs()).unwrap();
        tx.local_indexes().put(b"tampered-local-readiness", b"yes").unwrap();
        tx.commit().unwrap();
    }
    assert!(matches!(bob.entry_revisions(&participant, address, None, 64), Err(Error::Corrupt(_))));
    assert!(matches!(bob.rejected_entry_cuts(&participant, address), Err(Error::Corrupt(_))));
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_does_not_publish_rejected_retirement() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice
        .create_database(
            &owner,
            volume.id,
            "accepted",
            crate::store::volume_index::EntryKind::KvDb {},
        )
        .unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    bob.remove_entry(&participant, volume.id, entry.id).unwrap();
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    assert!(!bob.entry(&participant, volume.id, entry.id).unwrap().deleted);
    assert_eq!(bob.resolve_entry(&participant, volume.id, "accepted").unwrap().id, entry.id);
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_restores_backing_after_write_withdrawal() {
    use crate::store::volume_index::{EntryAddress, EntryKind};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "accepted", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    alice.kv_put(&owner, address, b"key", b"accepted").unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    bob.kv_put(&participant, address, b"key", b"rejected").unwrap();
    bob.kv_put(&participant, address, b"offline-only", b"retained for recovery").unwrap();
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    assert_eq!(
        bob.kv_read(&participant, address, b"key", 0, 1024, None).unwrap().bytes,
        b"accepted"
    );
    assert!(matches!(
        bob.kv_read(&participant, address, b"offline-only", 0, 1024, None),
        Err(Error::NotFound(_))
    ));
    let cuts = bob.rejected_entry_cuts(&participant, address).unwrap();
    assert_eq!(cuts.len(), 1);
    assert!(
        bob.recover_rejected_entry(&participant, address, "recovered", cuts[0]).is_err(),
        "A participant cannot duplicate a permission override without owner authority"
    );
    drop(accepted);
    alice.set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    let mut allowed = receive(&alice, &bob, volume.id);
    qualify(&mut allowed);
    bob.adopt_received_volume(&participant, &allowed).unwrap();
    let recovered =
        bob.recover_rejected_entry(&participant, address, "recovered", cuts[0]).unwrap();
    assert_ne!(recovered.id, entry.id);
    let recovered = EntryAddress { volume: volume.id, entry: recovered.id };
    assert_eq!(
        bob.kv_read(&participant, recovered, b"key", 0, 1024, None).unwrap().bytes,
        b"rejected"
    );
    assert_eq!(
        bob.kv_read(&participant, recovered, b"offline-only", 0, 1024, None).unwrap().bytes,
        b"retained for recovery"
    );
    assert_eq!(
        bob.kv_read(&participant, address, b"key", 0, 1024, None).unwrap().bytes,
        b"accepted"
    );
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_metadata_without_read_quarantines_backing_until_canonical_capture() {
    use crate::store::local::received::{PublicationCheckpoint, rejected};
    use crate::store::volume_index::{EntryAddress, EntryKind, IndexSnapshot};
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "accepted", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    alice.kv_put(&owner, address, b"key", b"accepted").unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    bob.kv_put(&participant, address, b"key", b"rejected").unwrap();
    bob.kv_put(&participant, address, b"offline-only", b"recover me").unwrap();
    let control_cut = |target: &LocalStore| {
        let mut stage = target.prepare_received_volume(volume.id).unwrap();
        for database in [DatabaseId::Index, DatabaseId::Revocations] {
            alice
                .with_replica(volume.id, database, |source| {
                    stage.with_database(database, |target| {
                        if database == DatabaseId::Index {
                            merge_index_replica(
                                &std::env::temp_dir(),
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            merge_replica(&std::env::temp_dir(), source, target, kvdb::Lww, |_| {
                                Ok(())
                            })
                        }
                    })
                })
                .unwrap();
            stage.finish_database(database).unwrap();
        }
        qualify(&mut stage);
        stage
    };
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Append],
                }],
            }),
        )
        .unwrap();
    for _ in 0..3 {
        bob.adopt_received_volume(&participant, &control_cut(&bob)).unwrap();
    }
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let cuts = rejected::cuts(&index).unwrap();
    assert_eq!(cuts.len(), 1, "metadata-only retries must not multiply archives");
    assert!(IndexSnapshot::new(index.begin_read().unwrap()).backing_quarantined(entry.id).unwrap());
    assert!(bob.rejected_entry_cuts(&participant, address).is_err(), "current Read still applies");
    drop(index);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    bob.adopt_received_volume(&participant, &control_cut(&bob)).unwrap();
    assert!(
        !bob.replica_upload_allowed(&participant, volume.id, DatabaseId::Entry(entry.id)).unwrap()
    );
    assert!(matches!(bob.kv_put(&participant, address, b"key", b"retry"), Err(Error::NotFound(_))));
    assert!(
        bob.with_outbound_replica(
            volume.id,
            DatabaseId::Entry(entry.id),
            ReplicaReader::Service,
            |_| Ok(())
        )
        .is_err()
    );
    alice.set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Inherit).unwrap();
    assert!(
        bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).is_err(),
        "an unqualified peer cannot release quarantine"
    );
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(move |point| {
        if point == PublicationCheckpoint::Backing(DatabaseId::Entry(entry.id)) {
            Err(Error::System("interrupt canonical backing before readiness".into()))
        } else {
            Ok(())
        }
    }));
    assert!(bob.adopt_received_volume(&participant, &accepted).is_err());
    assert!(bob.has_pending_publication(volume.id).unwrap());
    drop(accepted);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    assert!(!bob.has_pending_publication(volume.id).unwrap());
    assert_eq!(
        bob.kv_read(&participant, address, b"key", 0, 1024, None).unwrap().bytes,
        b"accepted"
    );
    assert!(matches!(
        bob.kv_read(&participant, address, b"offline-only", 0, 1024, None),
        Err(Error::NotFound(_))
    ));
    assert!(
        bob.replica_upload_allowed(&participant, volume.id, DatabaseId::Entry(entry.id)).unwrap()
    );
    let recovered =
        bob.recover_rejected_entry(&participant, address, "recovered", cuts[0].operation).unwrap();
    let recovered = EntryAddress { volume: volume.id, entry: recovered.id };
    assert_eq!(
        bob.kv_read(&participant, recovered, b"key", 0, 1024, None).unwrap().bytes,
        b"rejected"
    );
    assert_eq!(
        bob.kv_read(&participant, recovered, b"offline-only", 0, 1024, None).unwrap().bytes,
        b"recover me"
    );
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_preserves_rejected_creation_for_explicit_new_id_recovery() {
    use crate::store::volume_index::{EntryAddress, EntryKind};
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let entry =
        bob.create_database(&participant, volume.id, "offline", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    bob.kv_put(&participant, address, b"key", b"only local copy").unwrap();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    assert!(bob.resolve_entry(&participant, volume.id, "offline").is_err());
    assert!(bob.entry(&participant, volume.id, entry.id).is_err());
    let cuts = bob.rejected_entry_cuts(&participant, address).unwrap();
    assert_eq!(cuts.len(), 1);
    assert!(bob.recover_rejected_entry(&participant, address, "recovered", cuts[0]).is_err());
    let discovered = bob.rejected_entries(&participant, volume.id, None, 32).unwrap();
    assert_eq!(discovered.entries.len(), 1);
    assert_eq!(discovered.entries[0].entry.id, entry.id);
    assert_eq!(discovered.entries[0].entry.name, "offline");
    assert_eq!(discovered.entries[0].cursor.cut, cuts[0]);
    assert!(bob.rejected_entries(&participant, volume.id, None, 0).is_err());
    assert!(
        bob.rejected_entries(&participant, volume.id, Some(discovered.entries[0].cursor), 32)
            .unwrap()
            .entries
            .is_empty()
    );
    drop(accepted);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    assert_eq!(bob.rejected_entry_cuts(&participant, address).unwrap(), cuts);
    assert_eq!(bob.rejected_entries(&participant, volume.id, None, 32).unwrap(), discovered);
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            },
        )
        .unwrap();
    let mut allowed = receive(&alice, &bob, volume.id);
    qualify(&mut allowed);
    bob.adopt_received_volume(&participant, &allowed).unwrap();
    assert!(
        bob.entry(&participant, volume.id, entry.id).is_err(),
        "Restoring Write cannot republish a rejected birth"
    );
    let recovered =
        bob.recover_rejected_entry(&participant, address, "recovered", cuts[0]).unwrap();
    assert_ne!(recovered.id, entry.id);
    assert_eq!(
        bob.kv_read(
            &participant,
            EntryAddress { volume: volume.id, entry: recovered.id },
            b"key",
            0,
            1024,
            None
        )
        .unwrap()
        .bytes,
        b"only local copy"
    );
}

#[test]
#[cfg(feature = "connected-engine")]
#[ignore = "operator-only retained-edit profile for packaged browser qualification"]
fn rejected_browser_fixture() {
    use crate::store::volume_index::{EntryAddress, EntryKind};
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    alice.label_volume(&owner, volume.id, "Archive").unwrap();
    let database = bob
        .create_database(&participant, volume.id, "Drafts/settings", EntryKind::KvDb {})
        .unwrap();
    bob.kv_put(
        &participant,
        EntryAddress { volume: volume.id, entry: database.id },
        b"message",
        b"An offline edit retained for recovery",
    )
    .unwrap();
    bob.create_blob(&participant, volume.id, "Drafts/readme.txt", b"An independent offline file\n")
        .unwrap();
    for permissions in
        [vec![Permission::Read], vec![Permission::Read, Permission::Write, Permission::Append]]
    {
        alice
            .set_permissions(
                &owner,
                volume.id,
                Acl { grants: vec![Grant { subject: Subject::Public, permissions }] },
            )
            .unwrap();
        let mut accepted = receive(&alice, &bob, volume.id);
        qualify(&mut accepted);
        bob.adopt_received_volume(&participant, &accepted).unwrap();
    }
    assert_eq!(
        bob.rejected_entries(&Access::owner(bob.account().id), volume.id, None, 32)
            .unwrap()
            .entries
            .len(),
        2
    );
    drop(bob);
    let profile = bob_dir.keep();
    println!("{}", serde_json::json!({ "profile": profile, "volume": volume.id }));
}

#[test]
#[cfg(feature = "connected-engine")]
fn volume_snapshot_retains_only_readable_rejected_backings_and_blobs() {
    use crate::store::volume_index::{EntryAddress, EntryKind};
    use std::io::Read;
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let first = alice.create_database(&owner, volume.id, "first", EntryKind::KvDb {}).unwrap();
    let second = alice.create_database(&owner, volume.id, "second", EntryKind::KvDb {}).unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    for entry in [first.id, second.id] {
        bob.kv_put(&participant, EntryAddress { volume: volume.id, entry }, b"key", b"offline")
            .unwrap();
    }
    let blob = bob.create_blob(&participant, volume.id, "offline-file", b"retained file").unwrap();
    let version = bob
        .blob_version(&participant, EntryAddress { volume: volume.id, entry: blob.id }, None)
        .unwrap();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    let mut stage = receive(&alice, &bob, volume.id);
    qualify(&mut stage);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    drop(stage);
    let cuts = bob
        .rejected_entry_cuts(&participant, EntryAddress { volume: volume.id, entry: first.id })
        .unwrap();
    assert_eq!(cuts.len(), 1);
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            second.id,
            EntryAcl::Replace(Acl { grants: vec![] }),
        )
        .unwrap();
    let mut stage = receive(&alice, &bob, volume.id);
    qualify(&mut stage);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    drop(stage);
    let snapshot = bob.capture_volume(&participant, volume.id).unwrap();
    assert!(matches!(
        snapshot.with_rejected_database(cuts[0], second.id, |_| Ok(())),
        Err(Error::AccessDenied(_))
    ));
    let snapshot_path = std::fs::read_dir(bob.path.join("staging/snapshots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        !snapshot_path
            .join(crate::paths::RECEIVED)
            .join(cuts[0].to_string())
            .join("rejected")
            .join(DatabaseId::Entry(second.id).file_name())
            .exists(),
        "denied bytes must be omitted, not merely hidden by the accessor"
    );
    // The snapshot owns its copies; changing or deleting originals cannot change it.
    let source = bob.path.join(crate::paths::RECEIVED).join(cuts[0].to_string());
    std::fs::write(
        source.join("rejected").join(format!("{}.blob", hex(&version.digest))),
        b"corrupt",
    )
    .unwrap();
    assert!(
        matches!(bob.capture_volume(&participant, volume.id), Err(Error::Corrupt(_))),
        "a corrupt original must fail the complete capture"
    );
    std::fs::remove_dir_all(&source).unwrap();
    let bytes = snapshot
        .with_rejected_database(cuts[0], first.id, |tx| Ok(tx.get(b"key")?.unwrap().to_vec()))
        .unwrap();
    assert_eq!(bytes, b"offline");
    let mut bytes = Vec::new();
    snapshot.open_rejected_blob(cuts[0], &version).unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"retained file");
    assert!(
        bob.capture_volume(&participant, volume.id).is_err(),
        "missing recovery data must fail capture rather than produce dangling references"
    );
}

#[test]
#[cfg(feature = "connected-engine")]
fn rejected_discovery_requires_current_entry_read_even_with_catalog_read() {
    use crate::store::volume_index::{EntryAddress, EntryKind};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "private", EntryKind::KvDb {}).unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    bob.rename_entry(&participant, volume.id, entry.id, "offline private name").unwrap();
    alice
        .set_entry_permissions(&owner, volume.id, entry.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    assert!(bob.entries(&participant, volume.id, None, 32).is_ok());
    assert!(bob.rejected_entries(&participant, volume.id, None, 32).unwrap().entries.is_empty());
    assert!(matches!(
        bob.rejected_entry_cuts(&participant, EntryAddress { volume: volume.id, entry: entry.id }),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
#[cfg(feature = "connected-engine")]
fn rejected_discovery_pages_and_rechecks_catalog_read() {
    use crate::store::volume_index::EntryKind;
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let first =
        bob.create_database(&participant, volume.id, "offline-a", EntryKind::KvDb {}).unwrap();
    let second =
        bob.create_database(&participant, volume.id, "offline-b", EntryKind::KvDb {}).unwrap();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    let page = bob.rejected_entries(&participant, volume.id, None, 1).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert!(page.next.is_some());
    let next = bob.rejected_entries(&participant, volume.id, page.next, 1).unwrap();
    assert_eq!(next.entries.len(), 1);
    assert!(next.next.is_none());
    let mut found = vec![page.entries[0].entry.id, next.entries[0].entry.id];
    found.sort();
    let mut expected = vec![first.id, second.id];
    expected.sort();
    assert_eq!(found, expected);
    alice.set_permissions(&owner, volume.id, Acl::default()).unwrap();
    sync_volume(&alice, &bob, volume.id);
    assert!(matches!(
        bob.rejected_entries(&participant, volume.id, None, 32),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_preserves_append_birth_beside_rejected_rename() {
    use crate::store::volume_index::EntryKind;
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry =
        bob.create_database(&participant, volume.id, "initial", EntryKind::KvDb {}).unwrap();
    bob.rename_entry(&participant, volume.id, entry.id, "rejected rename").unwrap();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Append],
                }],
            },
        )
        .unwrap();
    let mut accepted = receive(&alice, &bob, volume.id);
    qualify(&mut accepted);
    bob.adopt_received_volume(&participant, &accepted).unwrap();
    assert_eq!(bob.resolve_entry(&participant, volume.id, "initial").unwrap().id, entry.id);
    assert!(bob.resolve_entry(&participant, volume.id, "rejected rename").is_err());
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_recovers_every_durable_boundary() {
    use crate::store::local::received::PublicationCheckpoint;
    use crate::store::volume_index::{EntryAddress, EntryKind};
    for (boundary, rename) in (0..4).flat_map(|boundary| [(boundary, false), (boundary, true)]) {
        let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
        let entry =
            alice.create_database(&owner, volume.id, "accepted", EntryKind::KvDb {}).unwrap();
        let address = EntryAddress { volume: volume.id, entry: entry.id };
        alice.kv_put(&owner, address, b"key", b"accepted").unwrap();
        bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
        bob.kv_put(&participant, address, b"key", b"rejected").unwrap();
        let rejected_event = if rename {
            bob.rename_entry(&participant, volume.id, entry.id, "offline").unwrap();
            use volume_index::namespace::source::cut;
            let index = bob.db(volume.id, DatabaseId::Index).unwrap();
            let snapshot = index.begin_read().unwrap();
            let accepted = cut::accepted(&snapshot).unwrap();
            Some(
                cut::delivered(&snapshot, &accepted)
                    .unwrap()
                    .into_iter()
                    .find(|source| source.entry == entry.id && !source.birth)
                    .unwrap(),
            )
        } else {
            None
        };
        alice
            .set_permissions(
                &owner,
                volume.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                },
            )
            .unwrap();
        if !rename {
            // Controls may arrive before the qualified backing capture. The
            // index now agrees with AP while the rejected backing remains local.
            alice
                .with_replica(volume.id, DatabaseId::Index, |source| {
                    bob.with_replica(volume.id, DatabaseId::Index, |target| {
                        merge_index_replica(
                            &std::env::temp_dir(),
                            source,
                            target,
                            kvdb::Lww,
                            |_| Ok(()),
                        )
                    })
                })
                .unwrap();
        }
        let mut accepted = receive(&alice, &bob, volume.id);
        if !rename {
            assert_eq!(
                accepted.with_database(DatabaseId::Index, replica_root).unwrap(),
                replica_root(&bob.db(volume.id, DatabaseId::Index).unwrap()).unwrap()
            );
        }
        qualify(&mut accepted);
        let checkpoint = [
            PublicationCheckpoint::Intent,
            PublicationCheckpoint::Backing(DatabaseId::Entry(entry.id)),
            PublicationCheckpoint::Index,
            PublicationCheckpoint::Readiness,
        ][boundary];
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(move |observed| {
            if observed == checkpoint {
                Err(Error::System("injected rejected-cut interruption".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &accepted).is_err());
        assert!(bob.has_pending_publication(volume.id).unwrap());
        let staged = accepted.with_database(DatabaseId::Index, |index| {
            Ok((
                MetadataPolicy::read(index)?.default_permissions().clone(),
                crate::store::volume_index::IndexSnapshot::new(index.begin_read()?).entry(entry.id),
            ))
        });
        let retained = fs::read_to_string(accepted.staged_directory().join("rejected.json"));
        drop(accepted);
        drop(bob);
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        assert!(!bob.has_pending_publication(volume.id).unwrap());
        if let Some(source) = &rejected_event {
            use volume_index::namespace::observations::{self, Ledger};
            let index = bob.db(volume.id, DatabaseId::Index).unwrap();
            let snapshot = index.begin_read().unwrap();
            assert!(!observations::is_provisional(&snapshot, source, 0).unwrap());
            assert!(
                !Ledger::load_read(&snapshot)
                    .unwrap()
                    .context()
                    .unwrap()
                    .contains(&source.id().unwrap())
            );
        }
        let restored =
            bob.resolve_entry(&participant, volume.id, "accepted").unwrap_or_else(|error| {
                let source = bob.with_replica(volume.id, DatabaseId::Index, |index| {
                    Ok(crate::store::volume_index::IndexSnapshot::new(index.begin_read()?)
                        .entry(entry.id))
                });
                panic!("recovery at {checkpoint:?}, rename={rename}: {error:?}; source: {source:?}; staged: {staged:?}; retained: {retained:?}");
            });
        assert_eq!(restored.id, entry.id);
        assert!(bob.resolve_entry(&participant, volume.id, "offline").is_err());
        assert_eq!(
            bob.kv_read(&participant, address, b"key", 0, 1024, None).unwrap().bytes,
            b"accepted"
        );
        let cuts = bob.rejected_entry_cuts(&participant, address).unwrap();
        assert_eq!(cuts.len(), 1);
        let mut repeated = receive(&alice, &bob, volume.id);
        qualify(&mut repeated);
        bob.adopt_received_volume(&participant, &repeated).unwrap();
        assert_eq!(bob.rejected_entry_cuts(&participant, address).unwrap(), cuts);
        alice
            .set_permissions(
                &owner,
                volume.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read, Permission::Write],
                    }],
                },
            )
            .unwrap();
        let mut allowed = receive(&alice, &bob, volume.id);
        qualify(&mut allowed);
        bob.adopt_received_volume(&participant, &allowed).unwrap();
        let recovered =
            bob.recover_rejected_entry(&participant, address, "recovered", cuts[0]).unwrap();
        assert_ne!(recovered.id, entry.id);
        assert_eq!(
            bob.kv_read(
                &participant,
                EntryAddress { volume: volume.id, entry: recovered.id },
                b"key",
                0,
                1024,
                None
            )
            .unwrap()
            .bytes,
            b"rejected"
        );
    }
}

#[test]
#[cfg(feature = "connected-engine")]
fn selected_equal_root_reconciliation_retains_backing_edits_at_every_durable_boundary() {
    use crate::store::local::received::PublicationCheckpoint;
    for boundary in 0..4 {
        let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
        // Select the existing sole entry so a valid scoped projection can equal
        // the complete local Index root, including the required build policy.
        let address = data(&alice, volume.id);
        let object =
            alice.create_object_with_fields(&owner, address, id(1), &title("accepted")).unwrap();
        bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
        bob.edit_fields(&participant, address, object.object_id, &title("offline edit")).unwrap();
        alice
            .set_permissions(
                &owner,
                volume.id,
                Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Read],
                    }],
                },
            )
            .unwrap();
        alice
            .with_replica(volume.id, DatabaseId::Index, |source| {
                bob.with_replica(volume.id, DatabaseId::Index, |target| {
                    merge_index_replica(
                        &std::env::temp_dir(),
                        source,
                        target,
                        kvdb::Lww,
                        |_| Ok(()),
                    )
                })
            })
            .unwrap();
        let mut accepted = receive_selected(&alice, &bob, address, &participant);
        assert_eq!(
            accepted.with_database(DatabaseId::Index, replica_root).unwrap(),
            replica_root(&bob.db(volume.id, DatabaseId::Index).unwrap()).unwrap()
        );
        qualify(&mut accepted);
        let checkpoint = [
            PublicationCheckpoint::Intent,
            PublicationCheckpoint::Backing(DatabaseId::Entry(address.entry)),
            PublicationCheckpoint::Index,
            PublicationCheckpoint::Readiness,
        ][boundary];
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(move |observed| {
            if observed == checkpoint {
                Err(Error::System("interrupted selected cut".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &accepted).is_err());
        drop(accepted);
        drop(bob);
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        assert!(!bob.has_pending_publication(volume.id).unwrap());
        assert_eq!(
            bob.read_object(&participant, address, object.object_id).unwrap(),
            alice.read_object(&owner, address, object.object_id).unwrap()
        );
        assert_eq!(bob.rejected_entry_cuts(&participant, address).unwrap().len(), 1);
    }
}

#[test]
#[cfg(feature = "connected-engine")]
fn selected_accepted_head_confirms_its_omitted_provisional_history() {
    use crate::store::volume_index::{
        EntryAddress, EntryKind,
        namespace::{observations, source::cut},
    };
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "initial", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    bob.rename_entry(&participant, volume.id, entry.id, "accepted edit").unwrap();
    let local_index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let canonical = alice.db(volume.id, DatabaseId::Index).unwrap();
    super::super::merge_index_with_publication(
        &std::env::temp_dir(),
        &local_index,
        &canonical,
        kvdb::Lww,
        |_| Ok(()),
        cut::retain_accepted,
    )
    .unwrap();
    let accepted = cut::accepted(&canonical.begin_read().unwrap()).unwrap();
    let witnesses = cut::delivered(&local_index.begin_read().unwrap(), &accepted).unwrap();
    let edit = witnesses.iter().find(|source| source.entry == entry.id && !source.birth).unwrap();
    assert!(observations::is_provisional(&local_index.begin_read().unwrap(), edit, 0).unwrap());
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Append],
                }],
            }),
        )
        .unwrap();
    let mut stage = receive_selected(&alice, &bob, address, &participant);
    stage
        .with_database(DatabaseId::Index, |index| {
            for row in index.begin_read()?.iter() {
                assert!(!volume_index::is_entry_history(&row?.0));
            }
            Ok(())
        })
        .unwrap();
    qualify_accepted(&mut stage, &accepted);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    let retained =
        volume_index::namespace::payloads::read(&local_index.begin_read().unwrap(), edit)
            .unwrap()
            .unwrap();
    assert_eq!(*blake3::hash(&retained).as_bytes(), edit.source_digest);
    assert!(!observations::is_provisional(&local_index.begin_read().unwrap(), edit, 0).unwrap());
    let current =
        cut::current::Record::read(&local_index.begin_read().unwrap(), entry.id).unwrap().unwrap();
    assert_eq!(current.name.unwrap().revision, edit.revision);
    assert_eq!(current.name.unwrap().read(&local_index.begin_read().unwrap()).unwrap().1, retained);
    assert!(
        observations::Ledger::load_read(&local_index.begin_read().unwrap())
            .unwrap()
            .context()
            .unwrap()
            .contains(&edit.id().unwrap())
    );
    drop(stage);
    drop(local_index);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    assert_eq!(
        cut::current::Record::read(&index.begin_read().unwrap(), entry.id).unwrap(),
        Some(current)
    );
    assert_eq!(
        volume_index::namespace::payloads::read(&index.begin_read().unwrap(), edit).unwrap(),
        Some(retained)
    );
    assert!(
        observations::Ledger::load_read(&index.begin_read().unwrap())
            .unwrap()
            .context()
            .unwrap()
            .contains(&edit.id().unwrap())
    );
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_receipt_retains_large_schema_birth_within_shared_decoder_budget() {
    // A large supported declaration borrows unused plan capacity; receipt
    // retention must not impose a smaller per-record decoding limit.
    use crate::store::object_contract::{FieldDeclaration, Schema, TypeDeclaration, ValueType};
    use volume_index::namespace::{payloads, receipt};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let fields = (1..=200u64)
        .map(|number| {
            let mut bytes = [1; 16];
            bytes[8..].copy_from_slice(&number.to_be_bytes());
            (
                Id::from_bytes(bytes),
                FieldDeclaration {
                    name: format!("field-{number}"),
                    value_type: ValueType::Text,
                    repeated: false,
                    deprecated: false,
                },
            )
        })
        .collect();
    let schema = Schema {
        types: BTreeMap::from([(
            Id::from_bytes([4; 16]),
            TypeDeclaration { name: "Row".into(), deprecated: false, fields },
        )]),
    };
    let entry = alice
        .create_database(
            &owner,
            volume.id,
            "large-schema",
            volume_index::EntryKind::ObjectDb { schema },
        )
        .unwrap();
    let mut stage = receive(&alice, &bob, volume.id);
    qualify(&mut stage);
    let witnesses = stage
        .with_database(DatabaseId::Index, |index| receipt::capture(&index.begin_read()?, None))
        .unwrap();
    let birth = &witnesses
        .iter()
        .find(|row| row.source.entry == entry.id && row.source.birth)
        .unwrap()
        .source;
    bob.adopt_received_volume(&participant, &stage).unwrap();
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let bytes = payloads::read(&index.begin_read().unwrap(), birth).unwrap().unwrap();
    assert!(crate::store::catalog::index_record_charge(&bytes).unwrap() > 512 * 1024);
    assert!(bytes.len() < volume_index::MAX_ENTRY_RECORD_BYTES);
    drop(stage);
    let mut repeat = receive(&alice, &bob, volume.id);
    qualify(&mut repeat);
    assert!(!bob.adopt_received_volume(&participant, &repeat).unwrap().metadata_changed);
    drop(repeat);
    alice.rename_entry(&owner, volume.id, entry.id, "renamed-schema").unwrap();
    let mut changed = receive(&alice, &bob, volume.id);
    qualify(&mut changed);
    bob.adopt_received_volume(&participant, &changed).unwrap();
    assert_eq!(bob.entry(&participant, volume.id, entry.id).unwrap().info.name, "renamed-schema");
}

#[test]
#[cfg(feature = "connected-engine")]
fn selected_accepted_payload_survives_newer_local_head_and_interrupted_merge() {
    // The selected writer gets no history. Its newer offline head wins source
    // merge; restart must still recover the exact earlier confirmed payload.
    use crate::store::local::received::PublicationCheckpoint;
    use volume_index::namespace::{payloads, receipt, source::cut};
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let entry = alice
        .create_database(&owner, volume.id, "initial", volume_index::EntryKind::KvDb {})
        .unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    alice.rename_entry(&owner, volume.id, entry.id, "accepted earlier").unwrap();
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            entry.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let canonical = alice.db(volume.id, DatabaseId::Index).unwrap();
    let accepted = cut::accepted(&canonical.begin_read().unwrap()).unwrap();
    let address = volume_index::EntryAddress { volume: volume.id, entry: entry.id };
    let mut stage = receive_selected(&alice, &bob, address, &participant);
    qualify_accepted(&mut stage, &accepted);
    let witnesses = stage
        .with_database(DatabaseId::Index, |index| {
            for row in index.begin_read()?.iter() {
                assert!(!volume_index::is_entry_history(&row?.0));
            }
            receipt::capture(&index.begin_read()?, Some(entry.id))
        })
        .unwrap();
    let accepted_edit = &witnesses.iter().find(|row| !row.source.birth).unwrap().source;
    // Two local edits advance past the unseen accepted rename without relying
    // on the operating system clock crossing a second during the fixture.
    bob.rename_entry(&participant, volume.id, entry.id, "intermediate offline").unwrap();
    bob.rename_entry(&participant, volume.id, entry.id, "newer offline").unwrap();
    *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|point| {
        if point == PublicationCheckpoint::Index {
            Err(Error::System("interrupt after overwriting accepted head".into()))
        } else {
            Ok(())
        }
    }));
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    let merged = stage
        .with_database(DatabaseId::Index, |index| {
            volume_index::IndexSnapshot::new(index.begin_read()?).source_entry(entry.id)
        })
        .unwrap();
    assert_eq!(merged.info.name, "newer offline");
    drop(stage);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    assert!(index.begin_read().unwrap().iter().all(|row| {
        let (key, bytes) = row.unwrap();
        !volume_index::is_entry_history(&key)
            || blake3::hash(&bytes).as_bytes() != &accepted_edit.source_digest
    }));
    let row_key =
        [b"namespace-observation/".as_slice(), accepted_edit.id().unwrap().as_bytes()].concat();
    let observation: volume_index::namespace::observations::Observation = {
        let snapshot = index.begin_read().unwrap();
        let row = snapshot.local_get(&row_key).unwrap().unwrap();
        crate::store::catalog::decode(&row).unwrap()
    };
    assert_eq!(observation.source, *accepted_edit);
    assert_eq!(observation.status, volume_index::namespace::observations::Status::Confirmed);
    assert_eq!(
        observation.assignment,
        Some(volume_index::namespace::Assignments { name: true, parent: false })
    );
    let bytes = payloads::read(&index.begin_read().unwrap(), accepted_edit).unwrap().unwrap();
    let revision: volume_index::EntryRevision =
        crate::store::catalog::record::decode(&bytes).unwrap();
    assert!(
        matches!(revision.change, volume_index::EntryChange::Name { ref name } if name == "accepted earlier")
    );
    assert_eq!(*blake3::hash(&bytes).as_bytes(), accepted_edit.source_digest);
    assert_eq!(
        volume_index::IndexSnapshot::new(index.begin_read().unwrap())
            .revision(entry.id, accepted_edit.revision)
            .unwrap(),
        revision
    );
    assert!(!bob.has_pending_publication(volume.id).unwrap());
}

#[test]
#[cfg(feature = "connected-engine")]
fn selected_file_revision_survives_head_replacement_without_granting_read() {
    use volume_index::namespace::source::cut;
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let file = alice.create_blob(&owner, volume.id, "file", b"initial").unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    alice.write_blob(&owner, volume.id, file.id, b"accepted content").unwrap();
    let address = volume_index::EntryAddress { volume: volume.id, entry: file.id };
    let accepted_version = alice.blob_version(&owner, address, None).unwrap();
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            file.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let accepted = alice
        .with_replica(volume.id, DatabaseId::Index, |index| cut::accepted(&index.begin_read()?))
        .unwrap();
    let mut stage = receive_selected(&alice, &bob, address, &participant);
    qualify_accepted(&mut stage, &accepted);
    // The staged cut is not yet observed by Bob. Choose a strictly later
    // publication clock so this concurrent write deterministically owns the head.
    let incoming_clock = alice.db(volume.id, DatabaseId::Index).unwrap().clock();
    let later_wall = kvdb::clock::EPOCH_UNIX_SECS + u64::from(incoming_clock.time) + 1;
    bob.db(volume.id, DatabaseId::Index)
        .unwrap()
        .begin_write(later_wall)
        .unwrap()
        .commit()
        .unwrap();
    let newer = bob.write_blob(&participant, volume.id, file.id, b"newer offline content").unwrap();
    assert!(bob.db(volume.id, DatabaseId::Index).unwrap().clock() > incoming_clock);
    assert_ne!(newer.content_revision, file.content_revision);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    drop(stage);
    drop(bob);
    let bob = LocalStore::open(bob_dir.path()).unwrap();
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let snapshot = volume_index::IndexSnapshot::new(index.begin_read().unwrap());
    assert_ne!(snapshot.entry(file.id).unwrap().content_revision, accepted_version.revision);
    assert_eq!(snapshot.entry(file.id).unwrap().content_revision, newer.content_revision);
    assert_eq!(
        snapshot.blob_version(address, Some(accepted_version.revision)).unwrap(),
        accepted_version
    );
    assert!(matches!(
        bob.read_blob_version(&participant, &accepted_version),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
#[cfg(feature = "connected-engine")]
fn selected_pending_blob_publication_does_not_certify_an_uncached_current_body() {
    // A Write-only selected receipt omits its parent and content bytes. Publishing
    // a different body with that incomplete context cannot make the old body ready.
    use volume_index::namespace::source::cut;
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let file = alice.create_blob(&owner, volume.id, "folder/file", b"uncached current").unwrap();
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            file.id,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let accepted = alice
        .with_replica(volume.id, DatabaseId::Index, |index| cut::accepted(&index.begin_read()?))
        .unwrap();
    let address = volume_index::EntryAddress { volume: volume.id, entry: file.id };
    let mut stage = receive_selected(&alice, &bob, address, &participant);
    qualify_accepted(&mut stage, &accepted);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let snapshot = volume_index::IndexSnapshot::new(index.begin_read().unwrap());
    assert_eq!(snapshot.entry(file.id).unwrap().content_revision, file.content_revision);
    assert!(!snapshot.backing_ready(file.id).unwrap());
    assert!(snapshot.entry(file.info.namespace.parent.unwrap()).is_err());
    drop(snapshot);
    let returned =
        bob.write_blob(&participant, volume.id, file.id, b"pending publication").unwrap();
    assert_eq!(returned.content_revision, file.content_revision);
    let snapshot = volume_index::IndexSnapshot::new(index.begin_read().unwrap());
    assert_eq!(snapshot.entry(file.id).unwrap().info.kind, file.info.kind);
    assert_ne!(snapshot.source_entry(file.id).unwrap().content_revision, file.content_revision);
    assert!(!snapshot.backing_ready(file.id).unwrap());
    assert!(matches!(
        bob.read_blob_entry(&participant, volume.id, file.id),
        Err(Error::AccessDenied(_))
    ));
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_namespace_receipt_replays_when_the_source_root_is_unchanged() {
    use crate::store::local::received::PublicationCheckpoint;
    use crate::store::volume_index::namespace::{observations::Ledger, source::cut};
    for checkpoint in [
        PublicationCheckpoint::Intent,
        PublicationCheckpoint::Index,
        PublicationCheckpoint::Readiness,
    ] {
        let (_a, alice, bob_dir, bob, volume, _owner, participant) = pair();
        let index = bob.db(volume.id, DatabaseId::Index).unwrap();
        assert!(
            Ledger::load_read(&index.begin_read().unwrap()).unwrap().context().unwrap().is_empty()
        );
        let before = replica_root(&index).unwrap();
        let expected = cut::accepted(&index.begin_read().unwrap()).unwrap();
        let witnesses = cut::delivered(&index.begin_read().unwrap(), &expected).unwrap();
        assert!(!expected.is_empty());
        let mut stage = receive(&alice, &bob, volume.id);
        qualify(&mut stage);
        assert_eq!(stage.with_database(DatabaseId::Index, replica_root).unwrap(), before);
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(move |point| {
            if point == checkpoint {
                Err(Error::System("interrupt namespace publication".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        assert!(bob.has_pending_publication(volume.id).unwrap());
        assert_eq!(replica_root(&index).unwrap(), before);
        drop(stage);
        drop(index);
        drop(bob);
        let bob = LocalStore::open(bob_dir.path()).unwrap();
        let index = bob.db(volume.id, DatabaseId::Index).unwrap();
        let context = Ledger::load_read(&index.begin_read().unwrap()).unwrap().context().unwrap();
        assert!(expected.keys().all(|id| context.contains(id)), "{checkpoint:?}");
        for witness in &witnesses {
            if witness.birth {
                let current =
                    cut::current::Record::read(&index.begin_read().unwrap(), witness.entry)
                        .unwrap()
                        .unwrap();
                assert_eq!(current.birth.revision, witness.revision);
                current.birth.read(&index.begin_read().unwrap()).unwrap();
            }
            assert!(
                volume_index::namespace::payloads::read(&index.begin_read().unwrap(), witness)
                    .unwrap()
                    .is_some(),
                "{checkpoint:?}"
            );
        }
        assert_eq!(replica_root(&index).unwrap(), before);
        assert!(!bob.has_pending_publication(volume.id).unwrap());
        // A second exact receipt is idempotent, including its local head.
        let mut repeat = receive(&alice, &bob, volume.id);
        qualify(&mut repeat);
        assert!(!bob.adopt_received_volume(&participant, &repeat).unwrap().metadata_changed);
        assert_eq!(
            Ledger::load_read(&index.begin_read().unwrap()).unwrap().context().unwrap(),
            context
        );
    }
}

#[test]
#[cfg(feature = "connected-engine")]
fn selected_control_preflight_includes_other_entries_before_durable_intent() {
    use volume_index::{EntryAddress, EntryKind, namespace::source::cut};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry = alice.create_database(&owner, volume.id, "selected", EntryKind::KvDb {}).unwrap();
    let sibling = alice.create_database(&owner, volume.id, "sibling", EntryKind::KvDb {}).unwrap();
    bob.adopt_received_volume(&participant, &receive(&alice, &bob, volume.id)).unwrap();
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let before = replica_root(&index).unwrap();
    let current = cut::current::Record::read(&index.begin_read().unwrap(), entry.id).unwrap();
    assert!(current.is_some());
    let mut tx = index.begin_write(now_unix_secs()).unwrap();
    tx.local_indexes()
        .put(&[b"namespace-current/".as_slice(), sibling.id.as_bytes()].concat(), b"invalid")
        .unwrap();
    tx.commit().unwrap();
    alice.rename_entry(&owner, volume.id, entry.id, "changed").unwrap();
    let accepted = alice
        .with_replica(volume.id, DatabaseId::Index, |index| cut::accepted(&index.begin_read()?))
        .unwrap();
    let mut stage = receive_selected(
        &alice,
        &bob,
        EntryAddress { volume: volume.id, entry: entry.id },
        &participant,
    );
    qualify_accepted(&mut stage, &accepted);
    assert!(matches!(bob.adopt_received_volume(&participant, &stage), Err(Error::Corrupt(_))));
    assert!(!bob.has_pending_publication(volume.id).unwrap());
    assert_eq!(replica_root(&index).unwrap(), before);
    assert_eq!(
        cut::current::Record::read(&index.begin_read().unwrap(), entry.id).unwrap(),
        current
    );
}

#[test]
#[cfg(feature = "connected-engine")]
fn interrupted_namespace_replay_rejects_missing_payload_accounting() {
    // Existing exact payloads must not let replay skip validation of the
    // retained-byte counter and release a volume with corrupt accounting.
    use crate::store::local::received::PublicationCheckpoint;
    let (_a, alice, bob_dir, bob, volume, _owner, participant) = pair();
    let mut first = receive(&alice, &bob, volume.id);
    qualify(&mut first);
    bob.adopt_received_volume(&participant, &first).unwrap();
    drop(first);
    let mut stage = receive(&alice, &bob, volume.id);
    qualify(&mut stage);
    *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|point| {
        if point == PublicationCheckpoint::Index {
            Err(Error::System("interrupt before retained-payload replay".into()))
        } else {
            Ok(())
        }
    }));
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    let index = bob.db(volume.id, DatabaseId::Index).unwrap();
    let mut tx = index.begin_write(now_unix_secs()).unwrap();
    assert!(tx.local_indexes().del(b"namespace-confirmed-payload-usage").unwrap());
    tx.commit().unwrap();
    *bob.received.after_checkpoint.lock().unwrap() = None;
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    assert!(bob.has_pending_publication(volume.id).unwrap());
    assert!(bob.entries(&participant, volume.id, None, 64).is_err());
    drop(index);
    drop(stage);
    drop(bob);
    assert!(LocalStore::open(bob_dir.path()).is_err());
}

#[test]
#[cfg(feature = "connected-engine")]
fn sealed_namespace_plan_damage_keeps_publication_pending() {
    use crate::store::local::received::PublicationCheckpoint;
    for missing in [true, false] {
        let (_a, alice, bob_dir, bob, volume, _owner, participant) = pair();
        let mut stage = receive(&alice, &bob, volume.id);
        qualify(&mut stage);
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|point| {
            if point == PublicationCheckpoint::Index {
                Err(Error::System("interrupt before namespace apply".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        assert!(bob.has_pending_publication(volume.id).unwrap());
        let path = stage.staged_directory().join(".namespace-plan");
        if missing {
            fs::remove_file(&path).unwrap();
        } else {
            let mut bytes = fs::read(&path).unwrap();
            *bytes.last_mut().unwrap() ^= 1;
            fs::write(&path, bytes).unwrap();
        }
        *bob.received.after_checkpoint.lock().unwrap() = None;
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        assert!(bob.has_pending_publication(volume.id).unwrap());
        assert!(bob.entries(&participant, volume.id, None, 64).is_err());
        drop(stage);
        drop(bob);
        assert!(LocalStore::open(bob_dir.path()).is_err());
    }
}

#[test]
#[cfg(feature = "connected-engine")]
fn qualified_ap_reconciliation_does_not_archive_ordinary_read_only_catch_up() {
    use crate::store::volume_index::{EntryAddress, EntryKind};
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let entry =
        alice.create_database(&owner, volume.id, "read-only-kv", EntryKind::KvDb {}).unwrap();
    let address = EntryAddress { volume: volume.id, entry: entry.id };
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![
                    Grant {
                        subject: Subject::Principal(
                            crate::store::object_contract::Principal::Human(alice.account().id),
                        ),
                        permissions: vec![Permission::Read, Permission::Write],
                    },
                    Grant {
                        subject: Subject::Principal(
                            crate::store::object_contract::Principal::Human(bob.account().id),
                        ),
                        permissions: vec![Permission::Read],
                    },
                ],
            },
        )
        .unwrap();
    for value in 0..40u8 {
        alice.kv_put(&owner, address, b"key", &[value]).unwrap();
        let mut stage = receive(&alice, &bob, volume.id);
        qualify(&mut stage);
        bob.adopt_received_volume(&participant, &stage).unwrap();
        assert_eq!(
            bob.kv_read(&participant, address, b"key", 0, 1024, None).unwrap().bytes,
            [value]
        );
        assert!(bob.rejected_entry_cuts(&participant, address).unwrap().is_empty());
    }
}

// Replayed publication must preserve a receiver's pending offline appends and
// rebuild its own arrival index, including recovery after the log commit.
#[test]
fn received_append_log_replays_preserving_pending_local_entries_and_cursor() {
    use crate::store::local::received::PublicationCheckpoint;
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let log = alice.create_bloblog(&owner, volume.id, "edits").unwrap().id;
    alice.bloblog_put(&owner, volume.id, log, &[b"first".to_vec()]).unwrap();
    let stage = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    let database = bob.entry_backing(volume.id, log, false).unwrap();
    crate::store::bloblog::Log {
        database: &database,
        volume: volume.id,
        entry: log,
        encryption_key: None,
        now: 1,
    }
    .put(&[b"offline".to_vec()])
    .unwrap();
    drop(database);
    let pending = bob.bloblog_pending(&participant, volume.id, log).unwrap();
    assert_eq!(pending.len(), 1);
    alice.bloblog_put(&owner, volume.id, log, &[b"second".to_vec()]).unwrap();
    alice.rename_entry(&owner, volume.id, log, "renamed-edits").unwrap();
    let stage = receive(&alice, &bob, volume.id);
    let expected_cursor = alice.bloblog_get(&owner, volume.id, log, None).unwrap().cursor;
    *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(move |point| {
        if point == PublicationCheckpoint::Backing(DatabaseId::Entry(log)) {
            Err(Error::System("stop after log publication".into()))
        } else {
            Ok(())
        }
    }));
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    drop(stage);
    drop(bob);
    let reopened = LocalStore::open(bob_dir.path()).unwrap();
    let page = reopened.bloblog_get(&participant, volume.id, log, None).unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(pending[0].data, b"offline");
    assert_eq!(reopened.bloblog_pending(&participant, volume.id, log).unwrap(), pending);
    assert_eq!(
        reopened.bloblog_checkpoint(&participant, volume.id, log).unwrap(),
        Some(expected_cursor)
    );
    assert_eq!(reopened.resolve_entry(&participant, volume.id, "renamed-edits").unwrap().id, log);
    let repeated = receive(&alice, &reopened, volume.id);
    assert!(!reopened.adopt_received_volume(&participant, &repeated).unwrap().changed());
}

// A read-only participant must keep receiving policy/data. Upload selection
// preserves blocked local appends and honors a remaining entry override.
#[cfg(feature = "connected-engine")]
#[test]
fn write_withdrawal_selects_no_upload_but_entry_override_can_keep_one_writer() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let log = alice.create_bloblog(&owner, volume.id, "edits").unwrap().id;
    let stage = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    drop(stage);
    let database = bob.entry_backing(volume.id, log, false).unwrap();
    crate::store::bloblog::Log {
        database: &database,
        volume: volume.id,
        entry: log,
        encryption_key: None,
        now: 1,
    }
    .put(&[b"offline".to_vec()])
    .unwrap();
    drop(database);
    let pending = bob.bloblog_pending(&participant, volume.id, log).unwrap();
    alice
        .set_permissions(
            &owner,
            volume.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    let stage = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    drop(stage);
    for database in [DatabaseId::Index, DatabaseId::Entry(log), DatabaseId::Revocations] {
        assert!(!bob.replica_upload_allowed(&participant, volume.id, database).unwrap());
    }
    assert_eq!(bob.bloblog_pending(&participant, volume.id, log).unwrap(), pending);
    assert!(bob.bloblog_get(&participant, volume.id, log, None).unwrap().entries.is_empty());
    alice
        .set_entry_permissions(
            &owner,
            volume.id,
            log,
            crate::store::object_contract::EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            }),
        )
        .unwrap();
    let stage = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    assert!(bob.replica_upload_allowed(&participant, volume.id, DatabaseId::Index).unwrap());
    assert!(bob.replica_upload_allowed(&participant, volume.id, DatabaseId::Entry(log)).unwrap());
    assert!(!bob.replica_upload_allowed(&participant, volume.id, DatabaseId::Revocations).unwrap());
}
#[test]
fn named_index_publishes_after_complete_backing_and_repeat_is_unchanged() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let (address, object) = super::live::new_named_object(&alice, &owner, volume.id, "new");
    let stage = receive(&alice, &bob, volume.id);
    assert!(bob.read_object(&participant, address, object.object_id).is_err());
    let outcome = bob.adopt_received_volume(&participant, &stage).unwrap();
    assert!(outcome.metadata_changed && outcome.databases_changed);
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
    assert!(!bob.adopt_received_volume(&participant, &stage).unwrap().changed());
}

#[test]
fn metadata_failure_retains_old_catalog_and_the_same_stage_can_retry() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let (address, object) = super::live::new_named_object(&alice, &owner, volume.id, "new");
    let old = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    let stage = receive(&alice, &bob, volume.id);
    *bob.before_creation_commit.lock().unwrap() = Some(Arc::new(|db| {
        if db == crate::store::DatabaseId::Index {
            Err(Error::System("injected index commit failure".into()))
        } else {
            Ok(())
        }
    }));
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    assert_eq!(
        replica_root(&bob.db(volume.id, crate::store::DatabaseId::Index).unwrap()).unwrap(),
        old
    );
    assert!(
        bob.read_object(&participant, address, object.object_id)
            .unwrap_err()
            .to_string()
            .contains("publication requires recovery")
    );
    *bob.before_creation_commit.lock().unwrap() = None;
    let outcome = bob.adopt_received_volume(&participant, &stage).unwrap();
    assert!(outcome.metadata_changed);
    assert!(!outcome.databases_changed);
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
}

// Interrupt real backing/index commits at each durable boundary. Reopening the
// same profile must finish the accepted cut before any content API is exposed.
#[test]
fn received_publication_recovers_every_durable_boundary() {
    use crate::store::DatabaseId;
    use crate::store::local::received::PublicationCheckpoint;
    for boundary in [0, 1, 2, 3] {
        let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
        let first = data(&alice, volume.id);
        let (second, second_object) =
            super::live::new_named_object(&alice, &owner, volume.id, "second");
        let first_object =
            alice.create_object_with_fields(&owner, first, id(1), &title("Initial")).unwrap();
        sync_volume(&alice, &bob, volume.id);
        alice.edit_fields(&owner, first, first_object.object_id, &title("First updated")).unwrap();
        alice
            .edit_fields(&owner, second, second_object.object_id, &title("Second updated"))
            .unwrap();
        alice.rename_entry(&owner, volume.id, first.entry, "renamed").unwrap();
        let stage = receive(&alice, &bob, volume.id);
        let first_backing = DatabaseId::Entry(first.entry.min(second.entry));
        let checkpoint = [
            PublicationCheckpoint::Intent,
            PublicationCheckpoint::Backing(first_backing),
            PublicationCheckpoint::Index,
            PublicationCheckpoint::Readiness,
        ][boundary];
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(move |observed| {
            if observed == checkpoint {
                Err(Error::System("injected publication interruption".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        assert!(bob.has_pending_publication(volume.id).unwrap());
        assert!(
            bob.read_object(&participant, first, first_object.object_id)
                .unwrap_err()
                .to_string()
                .contains("publication requires recovery")
        );
        let neighbor = bob.create_volume("Unrelated volume", false).unwrap();
        assert_eq!(bob.index(neighbor.id).unwrap().volume, neighbor);
        drop(stage);
        drop(bob);
        let reopened = LocalStore::open(bob_dir.path()).unwrap();
        assert!(!reopened.has_pending_publication(volume.id).unwrap());
        for (address, object) in [(first, first_object), (second, second_object)] {
            assert_eq!(
                reopened.read_object(&participant, address, object.object_id).unwrap(),
                alice.read_object(&owner, address, object.object_id).unwrap()
            );
        }
        assert_eq!(
            reopened.resolve_entry(&participant, volume.id, "renamed").unwrap().id,
            first.entry
        );
        let repeated = receive(&alice, &reopened, volume.id);
        assert!(!reopened.adopt_received_volume(&participant, &repeated).unwrap().changed());
    }
}

// Crash child processes without running Rust destructors, then recover their
// actual backing/index commits from the durable accepted stage.
#[test]
fn received_publication_survives_process_exit_at_each_durable_boundary() {
    use crate::store::DatabaseId;
    use crate::store::local::received::PublicationCheckpoint;
    for boundary in 0..4 {
        let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
        let first = data(&alice, volume.id);
        let (second, second_object) =
            super::live::new_named_object(&alice, &owner, volume.id, "second");
        let first_object =
            alice.create_object_with_fields(&owner, first, id(1), &title("Initial")).unwrap();
        sync_volume(&alice, &bob, volume.id);
        alice.edit_fields(&owner, first, first_object.object_id, &title("First updated")).unwrap();
        alice
            .edit_fields(&owner, second, second_object.object_id, &title("Second updated"))
            .unwrap();
        let stage = receive(&alice, &bob, volume.id);
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|checkpoint| {
            if checkpoint == PublicationCheckpoint::Intent {
                Err(Error::System("prepared child crash input".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        drop(stage);
        drop(bob);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "store::local::tests::received::received_publication_crash_child",
                "--nocapture",
            ])
            .env("PLAYBIT_TEST_RECEIVED_PROFILE", bob_dir.path())
            .env("PLAYBIT_TEST_RECEIVED_BOUNDARY", boundary.to_string())
            .env(
                "PLAYBIT_TEST_RECEIVED_BACKING",
                DatabaseId::Entry(first.entry.min(second.entry)).to_string(),
            )
            .output()
            .unwrap();
        assert_eq!(child.status.code(), Some(73), "{}", String::from_utf8_lossy(&child.stderr));
        let reopened = LocalStore::open(bob_dir.path()).unwrap();
        assert!(!reopened.has_pending_publication(volume.id).unwrap());
        for (address, object) in [(first, first_object), (second, second_object)] {
            assert_eq!(
                reopened.read_object(&participant, address, object.object_id).unwrap(),
                alice.read_object(&owner, address, object.object_id).unwrap()
            );
        }
    }
}

// Called only by the parent regression with a dedicated fixture profile. The
// checkpoint is set before open, so recovery itself crosses the crash boundary.
#[test]
#[ignore = "Crash subprocess; run by received_publication_survives_process_exit_at_each_durable_boundary"]
fn received_publication_crash_child() {
    use crate::store::local::received::{PublicationCheckpoint, crash_at};
    let profile = std::env::var_os("PLAYBIT_TEST_RECEIVED_PROFILE").unwrap();
    let boundary: usize = std::env::var("PLAYBIT_TEST_RECEIVED_BOUNDARY").unwrap().parse().unwrap();
    let backing =
        crate::store::DatabaseId::parse(&std::env::var("PLAYBIT_TEST_RECEIVED_BACKING").unwrap())
            .unwrap();
    crash_at(
        [
            PublicationCheckpoint::Intent,
            PublicationCheckpoint::Backing(backing),
            PublicationCheckpoint::Index,
            PublicationCheckpoint::Readiness,
        ][boundary],
    );
    let _store = LocalStore::open(PathBuf::from(profile)).unwrap();
    panic!("recovery did not cross its requested crash boundary");
}

// A sealed cut cannot be replaced after some live backings have committed.
// Reopen refuses missing/tampered inputs instead of exposing a mixed volume.
#[test]
fn pending_publication_rejects_replacement_missing_and_tampered_stages() {
    use crate::store::DatabaseId;
    use crate::store::local::received::PublicationCheckpoint;
    for damage in ["missing", "tampered"] {
        let (_a, alice, bob_dir, bob, volume, _owner, participant) = pair();
        let stage = receive(&alice, &bob, volume.id);
        *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|checkpoint| {
            if checkpoint == PublicationCheckpoint::Intent {
                Err(Error::System("stop after intent".into()))
            } else {
                Ok(())
            }
        }));
        assert!(bob.adopt_received_volume(&participant, &stage).is_err());
        *bob.received.after_checkpoint.lock().unwrap() = None;
        let replacement = receive(&alice, &bob, volume.id);
        assert!(matches!(
            bob.adopt_received_volume(&participant, &replacement),
            Err(Error::AccessDenied("received publication binding"))
        ));
        drop(replacement);
        let stage_path = stage.staged_directory().to_owned();
        if damage == "tampered" {
            stage
                .with_database(DatabaseId::Index, |db| {
                    let mut tx = db.begin_write(now_unix_secs())?;
                    tx.put(b"unknown mutation", b"changed after sealing")?;
                    tx.commit()?;
                    Ok(())
                })
                .unwrap();
        }
        drop(stage);
        drop(bob);
        if damage == "missing" {
            fs::remove_dir_all(stage_path).unwrap();
        }
        assert!(LocalStore::open(bob_dir.path()).is_err(), "{damage}");
    }
}

// An intent is an already admitted commit. Complete it after a later denial,
// but never make that retained data readable through the revoked context.
#[test]
fn journal_recovery_does_not_grant_access_to_a_later_revoked_participant() {
    use crate::store::local::received::PublicationCheckpoint;
    let (_a, alice, bob_dir, bob, volume, owner, participant) = pair();
    let address = data(&alice, volume.id);
    let object = alice
        .create_object_with_fields(&owner, address, id(1), &title("Accepted earlier"))
        .unwrap();
    let stage = receive(&alice, &bob, volume.id);
    *bob.received.after_checkpoint.lock().unwrap() = Some(Arc::new(|checkpoint| {
        if checkpoint == PublicationCheckpoint::Intent {
            Err(Error::System("stop after intent".into()))
        } else {
            Ok(())
        }
    }));
    assert!(bob.adopt_received_volume(&participant, &stage).is_err());
    bob.record_session_denial(
        &bob.session(participant.session_id.as_ref().unwrap()).unwrap(),
        "Revoked after acceptance",
    )
    .unwrap();
    #[cfg(feature = "connected-engine")]
    {
        let binding = bob.session(participant.session_id.as_ref().unwrap()).unwrap();
        let denial = crate::network::ap::operation::ServiceDenial::ParticipantRevoked {
            account_id: bob.account().id,
            volume_id: volume.id,
            session_id: Some(binding.id.clone()),
            build_id: Some(binding.resolved_build_id.clone()),
        };
        assert!(bob.receive_ap_session_denial(&denial, &binding).unwrap());
    }
    drop(stage);
    drop(bob);
    let reopened = LocalStore::open(bob_dir.path()).unwrap();
    assert!(!reopened.has_pending_publication(volume.id).unwrap());
    assert!(matches!(
        reopened.read_object(&participant, address, object.object_id),
        Err(Error::AccessDenied(_))
    ));
    let retained = crate::store::object_database::Snapshot::new(
        reopened
            .db(volume.id, crate::store::DatabaseId::Entry(address.entry))
            .unwrap()
            .begin_read()
            .unwrap(),
    )
    .read(object.object_id, None)
    .unwrap();
    assert_eq!(retained, object, "recovery completes previously accepted data without granting it");
}

// Credits charge the lifetime of reserved stage handles, including completed
// stages still held for receipts. Dropping an owner makes exactly one slot free.
#[test]
fn received_preparation_bounds_reserved_handles_and_releases_credits() {
    let (_directory, store, volume, _owner) = setup();
    let first = store.prepare_received_volume(volume.id).unwrap();
    let second = store.prepare_received_volume(volume.id).unwrap();
    assert!(matches!(
        store.prepare_received_volume(volume.id),
        Err(Error::LocalContention("received volume preparation limit"))
    ));
    drop(first);
    let third = store.prepare_received_volume(volume.id).unwrap();
    drop(second);
    drop(third);
    let again = store.prepare_received_volume(volume.id).unwrap();
    let also = store.prepare_received_volume(volume.id).unwrap();
    drop(again);
    drop(also);
}

#[test]
fn index_only_receipt_exposes_pending_entry_without_empty_backing() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let (address, object) = super::live::new_named_object(&alice, &owner, volume.id, "new");
    let stage = bob.prepare_received_volume(volume.id).unwrap();
    alice
        .with_replica(volume.id, crate::store::DatabaseId::Index, |source| {
            stage.with_database(crate::store::DatabaseId::Index, |target| {
                merge_index_replica(&std::env::temp_dir(), source, target, kvdb::Lww, |_| Ok(()))
            })
        })
        .unwrap();
    let outcome = bob.adopt_received_volume(&participant, &stage).unwrap();
    assert!(outcome.metadata_changed && !outcome.databases_changed);
    assert!(matches!(
        bob.read_object(&participant, address, object.object_id),
        Err(Error::NotFound(_))
    ));
    assert!(!bob.entry_details(&participant, volume.id, address.entry).unwrap().backing_ready);
    let complete = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &complete).unwrap();
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
}

#[test]
fn current_local_metadata_and_values_win_against_an_older_stage() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let address = data(&alice, volume.id);
    let object =
        alice.create_object_with_fields(&owner, address, id(1), &title("Initial")).unwrap();
    sync_volume(&alice, &bob, volume.id);
    let stage = receive(&alice, &bob, volume.id);
    bob.edit_fields(&participant, address, object.object_id, &title("Concurrent local")).unwrap();
    let retained = bob.read_object(&participant, address, object.object_id).unwrap();
    let local = bob
        .create_object_with_fields(&participant, address, id(1), &title("Local creation"))
        .unwrap();
    bob.rename_entry(&participant, volume.id, address.entry, "renamed during transfer").unwrap();
    bob.adopt_received_volume(&participant, &stage).unwrap();
    assert_eq!(
        bob.resolve_entry(&participant, volume.id, "renamed during transfer").unwrap().id,
        address.entry
    );
    assert!(bob.resolve_entry(&participant, volume.id, "data").is_err());
    assert_eq!(bob.read_object(&participant, address, local.object_id).unwrap(), local);
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), retained);
}

#[test]
fn denial_recorded_after_receipt_prevents_all_adoption() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    alice
        .create_object_with_fields(&owner, data(&alice, volume.id), id(1), &title("Denied"))
        .unwrap();
    let stage = receive(&alice, &bob, volume.id);
    let before = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    bob.record_session_denial(
        &bob.session(participant.session_id.as_ref().unwrap()).unwrap(),
        "revoked",
    )
    .unwrap();
    assert!(matches!(bob.adopt_received_volume(&participant, &stage), Err(Error::AccessDenied(_))));
    assert_eq!(bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
}
#[test]
fn local_permission_contraction_is_not_overridden_by_staged_grant() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    alice
        .create_object_with_fields(&owner, data(&alice, volume.id), id(1), &title("Denied"))
        .unwrap();
    let stage = receive(&alice, &bob, volume.id);
    bob.set_permissions(&owner, volume.id, Acl::default()).unwrap();
    let before = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    assert!(matches!(bob.adopt_received_volume(&participant, &stage), Err(Error::AccessDenied(_))));
    assert_eq!(bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
}

#[test]
fn initial_join_checks_received_root_revocations_before_admission() {
    let (_a, alice, root, owner) = setup();
    alice
        .set_permissions(
            &owner,
            root.id,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read],
                }],
            },
        )
        .unwrap();
    let version = crate::identifier::encode(&[0xab; 32]);
    alice.provision_version(&owner, root.id, &version).unwrap();
    let session = alice
        .create_session(
            &crate::package::AppIdentity { publisher: alice.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    let _b = tempfile::tempdir().unwrap();
    let bob = LocalStore::open(_b.path()).unwrap();
    bob.import_invitation_link(
        &alice.invitation(&session.id, crate::release_channel::ReleaseChannel::Public).unwrap(),
        &alice.session_envelope(&session.id).unwrap(),
        crate::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    let access = Access::session(bob.account().id, &session.id);
    let root_stage = receive(&alice, &bob, root.id);
    assert!(!bob.adopt_received_revocations(&access, &root_stage).unwrap());
    assert!(bob.adopt_received_volume(&access, &root_stage).unwrap().metadata_changed);
    assert!(!bob.adopt_received_revocations(&access, &root_stage).unwrap());
    let created = bob
        .create_session(
            &crate::package::AppIdentity { publisher: bob.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: version.clone() },
            &version,
            root.id,
        )
        .unwrap();
    assert_ne!(created.creator, root.owner);
    let creator_access = Access::session(bob.account().id, &created.id);
    assert!(!bob.adopt_received_revocations(&creator_access, &root_stage).unwrap());
    drop(root_stage);
    alice.revoke(&owner, session.root_volume, bob.account().id, "Removed").unwrap();
    let denied_stage = receive(&alice, &bob, session.root_volume);
    for _ in 0..2 {
        assert!(matches!(
            bob.adopt_received_revocations(&access, &denied_stage),
            Err(Error::AccessDenied(_))
        ));
        assert!(matches!(
            bob.adopt_received_revocations(&creator_access, &denied_stage),
            Err(Error::AccessDenied(_))
        ));
        assert!(
            bob.db(session.root_volume, crate::store::DatabaseId::Revocations)
                .unwrap()
                .begin_read()
                .unwrap()
                .has(bob.account().id.to_string().as_bytes())
                .unwrap()
        );
    }
}
#[test]
fn candidate_requires_exact_r1_provision_and_does_not_adopt_the_session() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let id = participant.session_id.as_ref().unwrap();
    let before = bob.session(id).unwrap();
    let candidate = crate::identifier::encode(&[0xcd; 32]);
    let access = Access::session_version(bob.account().id, id, &candidate);
    let unprovisioned = receive(&alice, &bob, volume.id);
    assert!(matches!(
        bob.adopt_received_volume(&access, &unprovisioned),
        Err(Error::AccessDenied(_))
    ));
    alice.provision_version(&owner, volume.id, &candidate).unwrap();
    let provisioned = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&access, &provisioned).unwrap();
    assert_eq!(bob.session(id).unwrap(), before);
}

#[test]
fn revocation_preflight_defers_owner_tombstones_until_full_adoption() {
    let (_a, alice, _b, bob, _volume, owner, participant) = pair();
    let id = participant.session_id.as_ref().unwrap();
    let session = alice.session(id).unwrap();
    let removed = super::id(99);
    alice.revoke(&owner, alice.session(id).unwrap().root_volume, removed, "Removed").unwrap();
    let stage = receive(&alice, &bob, session.root_volume);
    assert!(bob.adopt_received_revocations(&participant, &stage).unwrap());
    alice
        .with_replica(session.root_volume, crate::store::DatabaseId::Revocations, |db| {
            let mut tx = db.begin_write(now_unix_secs())?;
            tx.del(removed.to_string().as_bytes())?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    let tombstone = receive(&alice, &bob, session.root_volume);
    assert!(!bob.adopt_received_revocations(&participant, &tombstone).unwrap());
    assert!(
        bob.db(session.root_volume, crate::store::DatabaseId::Revocations)
            .unwrap()
            .begin_read()
            .unwrap()
            .has(removed.to_string().as_bytes())
            .unwrap()
    );
    assert!(bob.adopt_received_volume(&participant, &tombstone).unwrap().revocations_changed);
    assert!(
        !bob.db(session.root_volume, crate::store::DatabaseId::Revocations)
            .unwrap()
            .begin_read()
            .unwrap()
            .has(removed.to_string().as_bytes())
            .unwrap()
    );
    assert!(!bob.adopt_received_volume(&participant, &tombstone).unwrap().changed());
}
#[test]
fn stage_binding_rejects_another_volumes_metadata() {
    let (_a, alice, _b, bob, volume, _owner, participant) = pair();
    let stage = receive(&alice, &bob, volume.id);
    let mut forged = volume.clone();
    forged.id = id(99);
    stage
        .with_database(crate::store::DatabaseId::Index, |db| put_source(db, &VOLUME_KEY, &forged))
        .unwrap();
    let before = bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap();
    bob.record_session_denial(
        &bob.session(participant.session_id.as_ref().unwrap()).unwrap(),
        "locally denied",
    )
    .unwrap();
    // Remote shape validation remains first even when current admission would
    // also deny the operation. No reconciliation may precede either check.
    assert!(matches!(
        bob.adopt_received_volume(&participant, &stage),
        Err(Error::AccessDenied("replica volume mismatch"))
    ));
    assert_eq!(bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
    stage
        .with_database(crate::store::DatabaseId::Index, |db| put_source(db, &VOLUME_KEY, &volume))
        .unwrap();
    let staged = stage.with_database(crate::store::DatabaseId::Index, replica_root).unwrap();
    assert!(matches!(
        bob.adopt_received_volume(&participant, &stage),
        Err(Error::AccessDenied("session participant revoked"))
    ));
    assert_eq!(bob.replica_root(volume.id, crate::store::DatabaseId::Index).unwrap(), before);
    assert_eq!(stage.with_database(crate::store::DatabaseId::Index, replica_root).unwrap(), staged);
}

#[test]
fn local_first_object_after_an_empty_remote_snapshot_remains_adoptable() {
    let (_a, alice, _b, bob, volume, _owner, participant) = pair();
    let stage = receive(&alice, &bob, volume.id);
    let address = data(&bob, volume.id);
    stage
        .with_database(crate::store::DatabaseId::Entry(address.entry), |database| {
            assert!(replica_root(database)?.is_none());
            Ok(())
        })
        .unwrap();
    let local = bob
        .create_object_with_fields(&participant, address, id(1), &title("Created during receipt"))
        .unwrap();
    assert!(!bob.adopt_received_volume(&participant, &stage).unwrap().changed());
    assert_eq!(bob.read_object(&participant, address, local.object_id).unwrap(), local);
}

#[test]
fn staged_volume_adopts_an_existing_large_value_across_bounded_fragments() {
    let (_a, alice, _b, bob, volume, owner, participant) = pair();
    let text = "x".repeat(600 * 1024);
    let address = data(&alice, volume.id);
    let object = alice.create_object_with_fields(&owner, address, id(1), &title(&text)).unwrap();
    let stage = receive(&alice, &bob, volume.id);
    bob.adopt_received_volume(&participant, &stage).unwrap();
    assert_eq!(bob.read_object(&participant, address, object.object_id).unwrap(), object);
    let database = crate::store::DatabaseId::Entry(address.entry);
    assert_eq!(
        bob.replica_root(volume.id, database).unwrap(),
        alice.replica_root(volume.id, database).unwrap()
    );
}

#[test]
fn receipt_rechecks_withdrawn_session_mounts() {
    let (_dir, store, root, _owner) = setup();
    let personal = store.create_volume("Private personal data", false).unwrap();
    store
        .create_database(
            &Access::owner(store.account.id),
            personal.id,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema() },
        )
        .unwrap();
    let v1 = crate::identifier::encode(&[0x11; 32]);
    let v2 = crate::identifier::encode(&[0x22; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: v1.clone() },
            &v1,
            root.id,
        )
        .unwrap();
    let access = Access::session_version(store.account.id, &session.id, &v1);
    store
        .select_personal_volumes(
            &session.id,
            vec![PersonalVolumeGrant { volume: personal.id, write: false }],
        )
        .unwrap();
    let stage = receive(&store, &store, personal.id);
    store.select_personal_volumes(&session.id, vec![]).unwrap();
    assert!(matches!(store.adopt_received_volume(&access, &stage), Err(Error::AccessDenied(_))));
    store
        .select_personal_volumes(
            &session.id,
            vec![PersonalVolumeGrant { volume: personal.id, write: false }],
        )
        .unwrap();
    let candidate = Access::session_version(store.account.id, &session.id, &v2);
    assert!(store.adopt_received_volume(&candidate, &stage).is_ok());
    assert!(
        store.schema(&access, data(&store, personal.id)).is_ok(),
        "candidate refusal keeps retained consent usable"
    );
    assert!(
        !store.adopt_received_volume(&access, &stage).unwrap().changed(),
        "private peer-readable state remains admissible"
    );
}

#[test]
fn malformed_revocation_keys_abort_present_and_tombstone_batches() {
    let (_a, alice, _b, bob, _volume, _owner, participant) = pair();
    let session = bob.session(participant.session_id.as_ref().unwrap()).unwrap();
    for tombstone in [false, true] {
        let stage = receive(&alice, &bob, session.root_volume);
        stage
            .with_database(crate::store::DatabaseId::Revocations, |database| {
                let mut tx = database.begin_write(now_unix_secs())?;
                tx.put(id(99).to_string().as_bytes(), b"\"valid reason\"").unwrap();
                tx.put(b"zzzz-invalid-account", b"\"invalid key\"").unwrap();
                if tombstone {
                    tx.del(b"zzzz-invalid-account").unwrap();
                }
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        let before =
            bob.replica_root(session.root_volume, crate::store::DatabaseId::Revocations).unwrap();
        assert!(bob.adopt_received_revocations(&participant, &stage).is_err());
        assert_eq!(
            bob.replica_root(session.root_volume, crate::store::DatabaseId::Revocations).unwrap(),
            before
        );
    }
}
