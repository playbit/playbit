#![allow(clippy::unwrap_used, reason = "Tests exercise isolated account and volume stores.")]
use super::*;

fn unlock(store: &LocalStore) {
    store
        .save_recovery_state(&RecoveryState {
            secret: [41; 16],
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        })
        .unwrap();
}

fn invite(owner: &LocalStore, recipient: &LocalStore, volume: Id) -> (u64, Vec<u8>) {
    let policy = owner.volume_invitation_policy(volume).unwrap().unwrap();
    let key = account_key::volume_invitation_public_key(recipient.account.id, &[41; 16]).unwrap();
    let envelope =
        owner.seal_volume_invitation(volume, recipient.account.id, policy.clock, &key).unwrap();
    (policy.clock, envelope)
}

#[test]
fn account_decisions_survive_recovery_replay_regrant_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let owner = LocalStore::open(dir.path().join("owner")).unwrap();
    let recipient = LocalStore::open(dir.path().join("recipient")).unwrap();
    let device = dir.path().join("device");
    let mut other = LocalStore::open(&device).unwrap();
    other.bind_authenticated_account(recipient.account.id).unwrap();
    unlock(&recipient);
    unlock(&other);
    let volume = owner.create_volume("Quarterly reports", true).unwrap().id;
    owner
        .grant_volume(&Access::owner(owner.account.id), volume, recipient.account.id, false)
        .unwrap();
    let (clock, envelope) = invite(&owner, &recipient, volume);
    assert!(
        recipient
            .apply_volume_invitation(owner.account.id, volume, clock, false, &envelope, None)
            .is_err()
    );
    assert!(
        recipient
            .apply_volume_invitation(owner.account.id, volume, clock + 1, true, &envelope, None)
            .is_err()
    );
    assert!(
        recipient
            .apply_volume_invitation(
                owner.account.id,
                Id::generate().unwrap(),
                clock,
                true,
                &envelope,
                None
            )
            .is_err()
    );
    assert!(
        recipient
            .apply_volume_invitation(
                owner.account.id,
                volume,
                clock,
                true,
                &envelope,
                Some("Avery")
            )
            .unwrap()
    );
    assert_eq!(recipient.volume_listing().unwrap().invitations, [volume]);
    assert_eq!(
        recipient.volume_listing().unwrap().owners.get(&owner.account.id).map(String::as_str),
        Some("Avery")
    );
    assert!(!recipient.has_volume_index(volume).unwrap());
    assert_eq!(recipient.credentials(volume).unwrap(), owner.credentials(volume).unwrap());
    assert!(recipient.personal_volumes().unwrap().is_empty());
    assert!(
        serde_json::from_slice::<serde_json::Value>(&recipient.recovery_catalog().unwrap())
            .unwrap()["volumes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    recipient.decide_volume_invitation(volume, false).unwrap();
    assert!(recipient.volume_listing().unwrap().invitations.is_empty());
    assert!(recipient.volume_listing().unwrap().volumes.is_empty());
    let decision = recipient.pending_account_record(None).unwrap().unwrap();
    other.stage_account_record(&decision).unwrap().unwrap();
    other.reconcile_account_records().unwrap();
    assert!(
        other
            .apply_volume_invitation(owner.account.id, volume, clock, true, &envelope, None)
            .unwrap()
    );
    assert!(other.volume_listing().unwrap().invitations.is_empty());
    assert!(other.withdraw_volume_invitation(volume, clock).unwrap());
    assert!(
        !other
            .apply_volume_invitation(owner.account.id, volume, clock, true, &envelope, None)
            .unwrap()
    );
    owner
        .grant_volume(&Access::owner(owner.account.id), volume, recipient.account.id, true)
        .unwrap();
    let (newer, envelope) = invite(&owner, &recipient, volume);
    assert!(newer > clock);
    assert!(
        other
            .apply_volume_invitation(owner.account.id, volume, newer, true, &envelope, None)
            .unwrap()
    );
    assert!(!other.withdraw_volume_invitation(volume, clock).unwrap());
    drop(other);
    let other = LocalStore::open(device).unwrap();
    assert!(other.has_direct_volume_grant(volume).unwrap());
    assert!(other.volume_listing().unwrap().volumes.is_empty());
}

#[test]
fn private_grant_checks_both_peers_and_only_commits_authorized_changes() {
    let dir = tempfile::tempdir().unwrap();
    let owner = LocalStore::open(dir.path().join("owner")).unwrap();
    let recipient = LocalStore::open(dir.path().join("recipient")).unwrap();
    unlock(&recipient);
    let volume = owner.create_volume("Peer only", false).unwrap().id;
    let owner_access = Access::owner(owner.account.id);
    let account = recipient.account.id;
    let access = Access::owner(account);
    owner.grant_volume(&owner_access, volume, account, true).unwrap();
    let entry = owner
        .create_database(
            &owner_access,
            volume,
            "data",
            crate::store::volume_index::EntryKind::KvDb {},
        )
        .unwrap();
    let (clock, envelope) = invite(&owner, &recipient, volume);
    recipient
        .apply_volume_invitation(owner.account.id, volume, clock, false, &envelope, None)
        .unwrap();
    recipient.decide_volume_invitation(volume, true).unwrap();
    assert!(recipient.volume_listing().unwrap().invitations.is_empty());
    assert_eq!(recipient.volume_listing().unwrap().volumes[0].id, volume);
    let stage = super::super::tests::received::receive(&owner, &recipient, volume);
    recipient.adopt_received_revocations(&access, &stage).unwrap();
    recipient.adopt_received_volume(&access, &stage).unwrap();
    let database = DatabaseId::Entry(entry.id);
    let remote = Access::private_volume_peer(account, account);
    let sending = Access::private_volume_peer(account, owner.account.id);
    owner.with_authorized_replica(&remote, volume, database, None, |_, _| Ok(())).unwrap();
    recipient.with_authorized_replica(&sending, volume, database, None, |_, _| Ok(())).unwrap();
    let delta = |value: &[u8]| {
        recipient
            .with_replica(volume, database, |db| {
                let mut tx = db.begin_write_with_delta(
                    now_unix_secs(),
                    crate::store::MAX_REPLICA_DELTA_BYTES,
                )?;
                tx.put(b"message", value)?;
                match tx.commit_with_delta()? {
                    kvdb::CommittedDelta::Bytes(bytes) => Ok(bytes),
                    _ => panic!("test update fits the delta"),
                }
            })
            .unwrap()
    };
    let bytes = delta(b"hello");
    let projected =
        recipient.private_invitation_delta(volume, database, owner.account.id, &bytes).unwrap();
    assert!(owner.apply_private_invitation_delta(volume, database, account, &projected).unwrap());
    assert!(!owner.apply_private_invitation_delta(volume, database, account, &projected).unwrap());
    // A grantee can reconnect with an offline commit omitted from the live
    // feed. Relay exactly the owner's accepted changes to other subscribers.
    let mut feed = owner.take_change_receiver().unwrap();
    while !feed.drain().records.is_empty() {}
    delta(b"offline edit");
    let staged = recipient.db(volume, database).unwrap();
    owner.merge_authorized_replica(&remote, volume, database, &staged).unwrap();
    let batch = feed.drain();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(
        batch.records[0].kind(),
        crate::store::change_feed::CommittedChangeKind::Database(database)
    );
    let mut delivered = Vec::new();
    kvdb::sync::visit_delta(
        batch.records[0].bytes(),
        crate::store::MAX_REPLICA_DELTA_BYTES,
        |key, value| {
            if let kvdb::sync::EntState::Present { value, .. } = value {
                delivered.push((key.to_vec(), value.to_vec()));
            }
        },
    )
    .unwrap();
    assert_eq!(delivered, [(b"message".to_vec(), b"offline edit".to_vec())]);
    owner.merge_authorized_replica(&remote, volume, database, &staged).unwrap();
    assert!(feed.drain().records.is_empty());
    let stranger = Id::generate().unwrap();
    assert!(owner.private_invitation_delta(volume, database, stranger, &projected).is_err());
    assert!(owner.apply_private_invitation_delta(volume, database, stranger, &projected).is_err());
    owner.grant_volume(&owner_access, volume, account, false).unwrap();
    assert!(
        owner.apply_private_invitation_delta(volume, database, account, &delta(b"denied")).is_err()
    );
    assert_eq!(
        owner
            .with_replica(volume, database, |db| Ok(db
                .begin_read()?
                .get(b"message")?
                .unwrap()
                .to_vec()))
            .unwrap(),
        b"offline edit"
    );
    // A concurrent Public grant continues to admit ordinary reads. Removing
    // the named grant closes both the private snapshot and detached merge.
    owner
        .set_permissions(
            &owner_access,
            volume,
            Acl {
                grants: vec![crate::store::object_contract::Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Read, Permission::Write],
                }],
            },
        )
        .unwrap();
    assert!(
        owner
            .with_authorized_replica(&remote, volume, database, Some(Permission::Read), |_, _| Ok(
                ()
            ))
            .is_err()
    );
    assert!(owner.merge_authorized_replica(&remote, volume, database, &staged).is_err());
    owner.revoke(&owner_access, volume, account, "Removed").unwrap();
    assert!(owner.volume_invitation_policy(volume).unwrap().unwrap().recipients.is_empty());
    assert!(owner.private_invitation_delta(volume, database, account, &projected).is_err());
    recipient.withdraw_volume_invitation(volume, clock + 1).unwrap();
    assert!(recipient.volume_details(&access, volume).is_err());
}

#[test]
fn concurrent_acl_winners_receive_a_new_directory_revision() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let owner = Access::owner(store.account.id);
    let volume = store.create_volume("Concurrent permissions", true).unwrap().id;
    assert!(store.volume_invitation_policy(volume).unwrap().is_none());
    store.grant_volume(&owner, volume, Id::generate().unwrap(), false).unwrap();
    let previous = store.volume_invitation_policy(volume).unwrap().unwrap();
    let source = kvdb::Db::create(
        kvdb::storage::mem::MemStorage::new(),
        &kvdb::CreateOptions::syncable(
            store.db(volume, DatabaseId::Index).unwrap().meta().hash_key,
            1,
        ),
    )
    .unwrap();
    let mut tx = source
        .begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)
        .unwrap();
    tx.put_with_clock(
        &DEFAULT_PERMISSIONS_KEY,
        &record::encode(&Acl::default()).unwrap(),
        kvdb::Clock::from_u64(previous.clock),
    )
    .unwrap();
    let kvdb::CommittedDelta::Bytes(delta) = tx.commit_with_delta().unwrap() else {
        panic!("one ACL fits a delta")
    };
    store.merge_received_delta(&owner, volume, DatabaseId::Index, &delta).unwrap();
    let winner = store.volume_invitation_policy(volume).unwrap().unwrap();
    assert!(winner.clock > previous.clock);
    store.merge_received_delta(&owner, volume, DatabaseId::Index, &delta).unwrap();
    assert_eq!(store.volume_invitation_policy(volume).unwrap().unwrap().clock, winner.clock);
    store.set_permissions(&owner, volume, Acl::default()).unwrap();
    let withdrawal = store.volume_invitation_policy(volume).unwrap().unwrap();
    assert!(withdrawal.recipients.is_empty());
    assert!(withdrawal.clock > previous.clock);
}

#[test]
fn deleted_owner_grants_remain_publishable_until_ack_even_when_recovered_before_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owner");
    let source = LocalStore::open(&path).unwrap();
    unlock(&source);
    let owner = source.account.id;
    let volume = source.create_volume("Shared", false).unwrap().id;
    let creation = source.pending_account_record(None).unwrap().unwrap();
    source.acknowledge_account_record(&creation).unwrap();
    let access = Access::owner(owner);
    source.grant_volume(&access, volume, Id::from_bytes([9; 16]), true).unwrap();
    let prior = source.volume_invitation_policy(volume).unwrap().unwrap();
    source.set_permissions(&access, volume, Acl::default()).unwrap();
    source.delete_volume(volume).unwrap();
    let deletion = source.pending_account_record(None).unwrap().unwrap();
    drop(source);
    let source = LocalStore::open(&path).unwrap();
    let mut restored = LocalStore::open(directory.path().join("restored")).unwrap();
    restored.bind_authenticated_account(owner).unwrap();
    unlock(&restored);
    restored.stage_account_record(&deletion).unwrap().unwrap();
    restored.reconcile_account_records().unwrap();

    for store in [&source, &restored] {
        assert!(store.volumes().unwrap().is_empty());
        assert!(matches!(
            store.volume_invitation_publication(volume).unwrap(),
            Some(VolumeInvitationPublication::Withdrawal)
        ));
        store.acknowledge_volume_invitation_withdrawal(volume, prior.clock).unwrap();
        assert_eq!(store.volume_invitation_publications().unwrap(), [volume]);
        store
            .acknowledge_volume_invitation_withdrawal(
                volume,
                VolumeInvitationPublication::WITHDRAWAL_CLOCK,
            )
            .unwrap();
        assert!(store.volume_invitation_publications().unwrap().is_empty());
        assert!(store.volume_invitation_publication(volume).unwrap().is_none());
    }

    restored.stage_account_record(&creation).unwrap();
    restored.stage_account_record(&deletion).unwrap();
    restored.reconcile_account_records().unwrap();
    assert!(restored.volume_invitation_publications().unwrap().is_empty());
    assert!(restored.volume(volume).is_err());
}

#[test]
fn recipient_deletion_does_not_publish_an_owner_withdrawal() {
    let dir = tempfile::tempdir().unwrap();
    let owner = LocalStore::open(dir.path().join("owner")).unwrap();
    let recipient = LocalStore::open(dir.path().join("recipient")).unwrap();
    unlock(&recipient);
    let volume = owner.create_volume("Club", true).unwrap().id;
    owner
        .grant_volume(&Access::owner(owner.account.id), volume, recipient.account.id, false)
        .unwrap();
    let (clock, envelope) = invite(&owner, &recipient, volume);
    recipient
        .apply_volume_invitation(owner.account.id, volume, clock, true, &envelope, None)
        .unwrap();
    let mut tx = recipient.metadata.begin_write(now_unix_secs()).unwrap();
    assert!(recipient.stage_volume_deletion(&mut tx, volume, Some(recipient.account.id)).is_err());
    assert!(recipient.stage_volume_deletion(&mut tx, volume, Some(owner.account.id)).is_err());
    drop(tx);
    recipient.delete_volume(volume).unwrap();
    assert!(recipient.volume_invitation_publications().unwrap().is_empty());
    assert!(recipient.volume_invitation_publication(volume).unwrap().is_none());
}
