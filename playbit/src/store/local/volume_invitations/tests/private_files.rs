use super::*;
use crate::store::change_feed::CommittedChangeKind;
use crate::store::object_contract::Grant;
use crate::store::volume_index::{BlobVersion, EntryAddress};
use std::io::{Read, Seek};

fn volumes(path: &Path) -> (LocalStore, LocalStore, Id) {
    let owner = LocalStore::open(path.join("owner")).unwrap();
    let writer = LocalStore::open(path.join("writer")).unwrap();
    unlock(&writer);
    let volume = owner.create_volume("Private files", false).unwrap().id;
    owner.grant_volume(&Access::owner(owner.account.id), volume, writer.account.id, true).unwrap();
    let (clock, envelope) = invite(&owner, &writer, volume);
    writer
        .apply_volume_invitation(owner.account.id, volume, clock, false, &envelope, None)
        .unwrap();
    let stage = crate::store::local::tests::received::receive(&owner, &writer, volume);
    let access = Access::owner(writer.account.id);
    writer.adopt_received_revocations(&access, &stage).unwrap();
    writer.adopt_received_volume(&access, &stage).unwrap();
    (owner, writer, volume)
}

fn create_versions(owner: &LocalStore, writer: &LocalStore, volume: Id) -> [BlobVersion; 2] {
    let mut feed = writer.take_change_receiver().unwrap();
    while !feed.drain().records.is_empty() {}
    let access = Access::owner(writer.account.id);
    let entry = writer.create_blob(&access, volume, "edited", b"first").unwrap().id;
    let address = EntryAddress { volume, entry };
    let initial = writer.blob_version(&access, address, None).unwrap();
    writer.write_blob(&access, volume, entry, b"second").unwrap();
    let edited = writer.blob_version(&access, address, None).unwrap();
    let receiver = Access::private_volume_peer(writer.account.id, writer.account.id);
    assert!(owner.validate_peer_blob_upload(&receiver, &initial).is_err());
    deliver_index(writer, owner, volume, &mut feed);
    [initial, edited]
}

#[test]
fn saturated_private_upload_resumes_on_writer_and_owner_preparation_release() {
    use crate::store::local::entry_blobs::Preparation;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    #[derive(Default)]
    struct Released(AtomicBool);
    impl Wake for Released {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }
    fn after_release<T>(store: &LocalStore, work: impl Future<Output = T>) -> T {
        let first = Preparation::enter(&store.entry_blobs).unwrap();
        let _other = Preparation::enter(&store.entry_blobs).unwrap();
        assert!(matches!(Preparation::enter(&store.entry_blobs), Err(Error::LocalContention(_))));
        let released = Arc::new(Released::default());
        let waker = Waker::from(released.clone());
        let mut cx = Context::from_waker(&waker);
        let mut work = std::pin::pin!(work);
        assert!(work.as_mut().poll(&mut cx).is_pending());
        assert!(!released.0.load(Ordering::Acquire));
        drop(first);
        assert!(released.0.load(Ordering::Acquire));
        let Poll::Ready(result) = work.poll(&mut cx) else { panic!("one slot must suffice") };
        result
    }

    let directory = tempfile::tempdir().unwrap();
    let (owner, writer, volume) = volumes(directory.path());
    let [_, version] = create_versions(&owner, &writer, volume);
    let sender = Access::private_volume_peer(writer.account.id, owner.account.id);
    let receiver = Access::private_volume_peer(writer.account.id, writer.account.id);
    let mut body =
        after_release(&writer, writer.encrypt_peer_blob_upload(&sender, &version)).unwrap();
    assert!(
        after_release(&owner, owner.receive_peer_blob_upload(&receiver, &version, &mut body))
            .unwrap()
    );
    writer.acknowledge_peer_blob_upload(&sender, &version).unwrap();
    assert_eq!(
        owner.read_blob_version(&Access::owner(owner.account.id), &version).unwrap(),
        b"second"
    );
    assert!(!writer.peer_blob_upload_pending(&version).unwrap());
}

fn deliver_index(
    source: &LocalStore,
    target: &LocalStore,
    volume: Id,
    feed: &mut crate::store::change_feed::CommittedChangeReceiver,
) {
    for record in feed.drain().records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            let bytes = source
                .private_invitation_delta(
                    volume,
                    DatabaseId::Index,
                    target.account.id,
                    record.bytes(),
                )
                .unwrap();
            target
                .apply_private_invitation_delta(
                    volume,
                    DatabaseId::Index,
                    source.account.id,
                    &bytes,
                )
                .unwrap();
        }
    }
}

#[tokio::test]
async fn private_uploads_retain_each_version_and_only_retire_the_acknowledged_reference() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, writer, volume) = volumes(directory.path());
    let [first, latest] = create_versions(&owner, &writer, volume);
    let sending = Access::private_volume_peer(writer.account.id, owner.account.id);
    let receiving = Access::private_volume_peer(writer.account.id, writer.account.id);
    assert!(writer.has_pending_blob_uploads().unwrap());
    let page = writer.next_private_file_upload(volume, None).unwrap();
    assert_eq!(page.versions.len(), 1);
    let remaining = writer.next_private_file_upload(volume, page.next).unwrap();
    assert_eq!(remaining.versions.len(), 1);
    assert_ne!(page.versions, remaining.versions);
    assert!(remaining.next.is_none());
    // An initialized backing can still lack these bytes. Its first physical
    // arrival must wake a file reader independently of any readiness change.
    {
        let index = owner.db(volume, DatabaseId::Index).unwrap();
        let mut tx = index.begin_write(now_unix_secs()).unwrap();
        crate::store::volume_index::mark_backing_ready(&mut tx, latest.address.entry).unwrap();
        tx.commit().unwrap();
    }
    for version in [&latest, &first] {
        writer.validate_peer_blob_upload(&sending, version).unwrap();
        let mut body = writer.encrypt_peer_blob_upload(&sending, version).await.unwrap();
        // Lost acknowledgement: the receiver can durably accept the same body twice.
        for changed in [true, false] {
            body.rewind().unwrap();
            let before = owner.delivery_revision();
            assert_eq!(
                owner.receive_peer_blob_upload(&receiving, version, &mut body).await.unwrap(),
                changed
            );
            assert_eq!(owner.delivery_scope_revision(before, &[volume]).1.is_some(), changed);
        }
        assert!(writer.peer_blob_upload_pending(version).unwrap());
        writer.acknowledge_peer_blob_upload(&sending, version).unwrap();
        writer.acknowledge_peer_blob_upload(&sending, version).unwrap();
        assert!(!writer.peer_blob_upload_pending(version).unwrap());
        if version == &latest {
            assert!(writer.peer_blob_upload_pending(&first).unwrap());
            fs::remove_file(owner.blob_path(&version.digest, false)).unwrap();
            body.rewind().unwrap();
            assert!(owner.receive_peer_blob_upload(&receiving, version, &mut body).await.unwrap());
        }
    }
    assert!(!writer.has_pending_blob_uploads().unwrap());
    assert!(
        owner
            .pending_entry_blobs(&Access::owner(owner.account.id), volume, None)
            .unwrap()
            .versions
            .is_empty()
    );
    drop(owner);
    drop(writer);
    for device in ["owner", "writer"] {
        let store = LocalStore::open(directory.path().join(device)).unwrap();
        let access = Access::owner(store.account.id);
        // A hosted receipt for the same plaintext cannot release the private copy.
        let hosted = store.create_volume("Hosted copy", true).unwrap().id;
        let entry = store.create_blob(&access, hosted, "match", b"second").unwrap().id;
        let shared =
            store.blob_version(&access, EntryAddress { volume: hosted, entry }, None).unwrap();
        store.acknowledge_blob_version(&access, &shared).unwrap();
        store.evict_blobs(0).unwrap();
        assert_eq!(store.read_blob_version(&access, &first).unwrap(), b"first");
        assert_eq!(
            store.read_blob_entry(&access, volume, latest.address.entry).unwrap(),
            b"second"
        );
        assert!(!store.blob_version_durable(&access, &latest).unwrap());
        assert!(!store.peer_blob_upload_pending(&latest).unwrap());
    }
}

#[tokio::test]
async fn private_upload_admits_only_exact_linked_bytes_and_rechecks_writer_before_commit() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, writer, volume) = volumes(directory.path());
    let [first, latest] = create_versions(&owner, &writer, volume);
    let owner = Arc::new(owner);
    let sending = Access::private_volume_peer(writer.account.id, owner.account.id);
    let receiving = Access::private_volume_peer(writer.account.id, writer.account.id);
    let access = Access::owner(owner.account.id);
    let mut body = Vec::new();
    writer
        .encrypt_peer_blob_upload(&sending, &latest)
        .await
        .unwrap()
        .read_to_end(&mut body)
        .unwrap();
    let mut forged = latest.clone();
    forged.digest[0] ^= 1;
    assert!(owner.validate_peer_blob_upload(&receiving, &forged).is_err());
    assert!(!writer.peer_blob_upload_pending(&forged).unwrap());
    assert!(writer.acknowledge_peer_blob_upload(&sending, &forged).is_err());
    assert!(
        owner
            .receive_peer_blob_upload(&receiving, &latest, &mut &body[..body.len() - 1])
            .await
            .is_err()
    );
    let changed = owner.clone();
    owner
        .set_blob_publication_hook(Some(Arc::new(move || {
            changed.set_entry_permissions(
                &Access::owner(changed.account.id),
                volume,
                latest.address.entry,
                EntryAcl::Replace(Acl::default()),
            )
        })))
        .unwrap();
    assert!(owner.receive_peer_blob_upload(&receiving, &latest, &mut &body[..]).await.is_err());
    owner.set_blob_publication_hook(None).unwrap();
    assert!(!owner.blob_path(&latest.digest, false).exists());
    assert!(writer.peer_blob_upload_pending(&latest).unwrap());

    let append = Grant {
        subject: Subject::Principal(Principal::Human(writer.account.id)),
        permissions: vec![Permission::Append],
    };
    owner
        .set_entry_permissions(
            &access,
            volume,
            latest.address.entry,
            EntryAcl::Replace(Acl {
                grants: vec![
                    append,
                    Grant {
                        subject: Subject::Principal(Principal::Human(owner.account.id)),
                        permissions: vec![Permission::Read],
                    },
                ],
            }),
        )
        .unwrap();
    let mut initial = writer.encrypt_peer_blob_upload(&sending, &first).await.unwrap();
    owner.receive_peer_blob_upload(&receiving, &first, &mut initial).await.unwrap();
    assert!(owner.validate_peer_blob_upload(&receiving, &latest).is_err());
    owner.set_entry_permissions(&access, volume, latest.address.entry, EntryAcl::Inherit).unwrap();
    owner
        .set_permissions(
            &access,
            volume,
            Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            },
        )
        .unwrap();
    assert!(owner.validate_peer_blob_upload(&receiving, &latest).is_err());
    writer.withdraw_volume_invitation(volume, u64::MAX).unwrap();
    assert!(writer.acknowledge_peer_blob_upload(&sending, &first).is_err());
    assert!(writer.peer_blob_upload_pending(&first).unwrap());
    assert!(!writer.has_pending_blob_uploads().unwrap());
}

#[tokio::test]
async fn private_writer_keeps_owner_excluded_revisions_until_read_is_restored() {
    let directory = tempfile::tempdir().unwrap();
    let (owner, writer, volume) = volumes(directory.path());
    let [_first, edited] = create_versions(&owner, &writer, volume);
    let sender = Access::private_volume_peer(writer.account.id, owner.account.id);
    let receiver = Access::private_volume_peer(writer.account.id, writer.account.id);
    let owner_access = Access::owner(owner.account.id);
    let writer_access = Access::owner(writer.account.id);
    let mut events = owner.take_change_receiver().unwrap();
    while !events.drain().records.is_empty() {}
    let writer_only = Grant {
        subject: Subject::Principal(Principal::Human(writer.account.id)),
        permissions: vec![Permission::Read, Permission::Write],
    };
    owner
        .set_entry_permissions(
            &owner_access,
            volume,
            edited.address.entry,
            EntryAcl::Replace(Acl { grants: vec![writer_only] }),
        )
        .unwrap();
    assert!(matches!(
        owner.validate_peer_blob_upload(&receiver, &edited),
        Err(Error::AccessDenied(Error::PRIVATE_FILE_OWNER_READ))
    ));
    deliver_index(&owner, &writer, volume, &mut events);
    assert!(matches!(
        writer.validate_peer_blob_upload(&sender, &edited),
        Err(Error::AccessDenied(Error::PRIVATE_FILE_OWNER_READ))
    ));
    assert!(!writer.peer_blob_upload_eligible(&edited).unwrap());
    assert!(writer.peer_blob_upload_pending(&edited).unwrap());
    assert!(!writer.has_pending_blob_uploads().unwrap());
    let allowed = writer.create_blob(&writer_access, volume, "allowed", b"other").unwrap();
    let eligible = writer.pending_entry_blobs(&writer_access, volume, None).unwrap().versions;
    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0].address.entry, allowed.id);

    owner
        .set_entry_permissions(&owner_access, volume, edited.address.entry, EntryAcl::Inherit)
        .unwrap();
    deliver_index(&owner, &writer, volume, &mut events);
    assert!(writer.peer_blob_upload_eligible(&edited).unwrap());
    let mut body = writer.encrypt_peer_blob_upload(&sender, &edited).await.unwrap();
    assert!(owner.receive_peer_blob_upload(&receiver, &edited, &mut body).await.unwrap());
    writer.acknowledge_peer_blob_upload(&sender, &edited).unwrap();
    assert!(!writer.peer_blob_upload_pending(&edited).unwrap());
    assert_eq!(owner.read_blob_version(&owner_access, &edited).unwrap(), b"second");
}
