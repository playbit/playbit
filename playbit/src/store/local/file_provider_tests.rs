#![allow(clippy::unwrap_used, reason = "Tests assert authorized filesystem projection behavior.")]
use super::*;
use crate::store::EntryType;
use crate::store::object_contract::{Acl, EntryAcl};

#[cfg(feature = "connected-engine")]
#[test]
fn synthetic_folder_uploads_require_the_exact_policy_clock_and_body() {
    // Acknowledge real snapshot/delta cuts around offline renames. Delayed
    // acknowledgments must neither qualify nor obscure a newer label revision.
    use crate::store::{DatabaseId, change_feed::CommittedChangeKind};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.provider_create_volume(Id::generate().unwrap(), "Files", false).unwrap();
    assert!(!store.provider_volume_item(volume.id).unwrap().uploaded);
    let initial = store.replica_root(volume.id, DatabaseId::Index).unwrap();
    store.acknowledge_provider_snapshot(volume.id, initial).unwrap();
    assert!(store.provider_volume_item(volume.id).unwrap().uploaded);
    let mut feed = store.take_change_receiver().unwrap();
    drop(feed.drain());
    let rename = |name| {
        let before = store.provider_volume_item(volume.id).unwrap();
        store.provider_rename_volume(volume.id, name, &before.metadata_version).unwrap();
    };
    rename("First rename");
    let first = feed.drain();
    assert!(!store.provider_volume_item(volume.id).unwrap().uploaded);
    rename("Second rename");
    let second = feed.drain();
    for record in &first.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap();
        }
    }
    assert!(!store.provider_volume_item(volume.id).unwrap().uploaded);
    for record in &second.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap();
        }
    }
    assert!(store.provider_volume_item(volume.id).unwrap().uploaded);
    assert!(!store.provider_volume_uploaded(&volume).unwrap());
    assert!(store.provider_volume_uploaded(&store.volume(volume.id).unwrap()).unwrap());
    for record in &first.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap();
        }
    }
    assert!(store.provider_volume_item(volume.id).unwrap().uploaded);
    rename("Files");
    assert!(!store.provider_volume_item(volume.id).unwrap().uploaded);
    store.acknowledge_provider_snapshot(volume.id, initial).unwrap();
    assert!(!store.provider_volume_item(volume.id).unwrap().uploaded);
}

#[test]
fn synthetic_catalog_checks_persistent_account_authority_and_recovered_folders_stay_visible() {
    // Import only account discovery, then retain its folder while admission
    // waits. A persistent quarantine must block every synthetic operation.
    let directory = tempfile::tempdir().unwrap();
    let source = LocalStore::open(directory.path().join("source")).unwrap();
    let volume = source.create_volume("A/B\0label", true).unwrap();
    let mut recovered = LocalStore::open(directory.path().join("recovered")).unwrap();
    recovered.bind_authenticated_account(source.account.id).unwrap();
    recovered.import_recovery_catalog(&source.recovery_catalog().unwrap(), || true).unwrap();
    assert_eq!(recovered.provider_volumes().unwrap().len(), 1);
    let folder = recovered.provider_volume_item(volume.id).unwrap();
    assert_eq!(folder.role, "volume");
    assert!(folder.unavailable);
    assert!(!folder.filename.contains('/') && !folder.filename.contains('\0'));
    assert!(recovered.provider_entries(volume.id, None, None, 128).is_err());
    // Recheck the persistent fence, not only its cached process flag.
    let mut tx = recovered.metadata.begin_write(now_unix_secs()).unwrap();
    tx.put(b"account-authority-quarantine", b"retained unsupported authority").unwrap();
    tx.commit().unwrap();
    assert!(recovered.provider_account().is_err());
    assert!(recovered.provider_root_item().is_err());
    assert!(recovered.provider_volumes().is_err());
    assert!(recovered.provider_volume_item(volume.id).is_err());
    assert!(recovered.provider_create_volume(Id::from_bytes([9; 16]), "Blocked", true).is_err());
}

#[test]
fn provider_volume_rename_is_conditional_and_keeps_generated_names_out_of_labels() {
    // Edit a projected label under an exact metadata version and retain an
    // unknown optional policy field through the store's ordinary rewrite path.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("A/B", true).unwrap();
    let db = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
    let optional = [99, 0, 4, 0, 0, 0, 255, 7, 0, 9];
    let mut tx = db.begin_write(now_unix_secs()).unwrap();
    let mut policy = tx.get(&VOLUME_KEY).unwrap().unwrap().to_vec();
    policy.extend_from_slice(&optional);
    tx.put(&VOLUME_KEY, &policy).unwrap();
    tx.commit().unwrap();
    let original = store.provider_volume_item(volume.id).unwrap();
    store
        .provider_rename_volume(volume.id, &original.filename, &original.metadata_version)
        .unwrap();
    assert_eq!(store.volume(volume.id).unwrap().name, "A/B");
    let renamed = original.filename.replacen("A%2FB", "C%2FD", 1);
    store.provider_rename_volume(volume.id, &renamed, &original.metadata_version).unwrap();
    assert_eq!(store.volume(volume.id).unwrap().name, "C/D");
    assert!(store.provider_rename_volume(volume.id, "Stale", &original.metadata_version).is_err());
    assert_eq!(store.volume(volume.id).unwrap().name, "C/D");
    let current = store.provider_volume_item(volume.id).unwrap();
    store.provider_rename_volume(volume.id, "Literal%2F", &current.metadata_version).unwrap();
    assert_eq!(store.volume(volume.id).unwrap().name, "Literal%2F");
    assert!(db.begin_read().unwrap().get(&VOLUME_KEY).unwrap().unwrap().ends_with(&optional));
}

#[test]
fn provider_upload_receipts_follow_exact_namespace_and_body_and_survive_restart() {
    use crate::store::{DatabaseId, volume_index::EntryAddress};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Personal", true).unwrap();
    let access = Access::owner(store.account.id);
    let blob = store.create_blob(&access, volume.id, "file.bin", b"first").unwrap();
    let address = EntryAddress { volume: volume.id, entry: blob.id };
    let first = store.blob_version(&access, address, None).unwrap();
    assert!(!store.provider_item(volume.id, blob.id).unwrap().uploaded);
    let acknowledge_namespace = |store: &LocalStore| {
        let root = store.replica_root(volume.id, DatabaseId::Index).unwrap();
        store.acknowledge_provider_snapshot(volume.id, root).unwrap();
    };
    acknowledge_namespace(&store);
    // A durable declaration cannot substitute for the exact immutable body.
    assert!(!store.provider_item(volume.id, blob.id).unwrap().uploaded);
    let before = store.provider_upload_revision(volume.id).unwrap();
    let source = store.delivery_revision().unwrap();
    let provider = store.delivery_provider_revision(None, Some(&[volume.id])).0;
    store.acknowledge_blob_version(&access, &first).unwrap();
    assert_eq!(store.delivery_revision(), Some(source));
    assert_eq!(store.delivery_scope_revision(Some(source), &[volume.id]).1, None);
    assert!(store.delivery_provider_revision(provider, Some(&[volume.id])).1.is_some());
    let uploaded = store.provider_item(volume.id, blob.id).unwrap();
    assert!(uploaded.uploaded);
    assert!(store.provider_upload_revision(volume.id).unwrap() > before);
    let quiet = store.provider_upload_revision(volume.id).unwrap();
    let quiet_fence = store.delivery_provider_revision(None, None).0;
    acknowledge_namespace(&store);
    store.acknowledge_blob_version(&access, &first).unwrap();
    assert_eq!(store.provider_upload_revision(volume.id).unwrap(), quiet);
    assert_eq!(store.delivery_provider_revision(quiet_fence, None).1, None);

    store.write_blob(&access, volume.id, blob.id, b"second").unwrap();
    let second = store.blob_version(&access, address, None).unwrap();
    acknowledge_namespace(&store);
    let pending = store.provider_item(volume.id, blob.id).unwrap();
    assert!(!pending.uploaded);
    store.acknowledge_blob_version(&access, &first).unwrap();
    assert_eq!(store.provider_item(volume.id, blob.id).unwrap(), pending);
    store.acknowledge_blob_version(&access, &second).unwrap();
    let finished = store.provider_item(volume.id, blob.id).unwrap();
    assert!(finished.uploaded);
    assert_eq!(finished.content_version, pending.content_version);
    assert_eq!(finished.metadata_version, pending.metadata_version);
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.provider_item(volume.id, blob.id).unwrap(), finished);
}

#[cfg(feature = "connected-engine")]
#[test]
fn provider_initialization_receipt_seeds_its_captured_cut_after_a_concurrent_edit() {
    use crate::store::{DatabaseId, change_feed::CommittedChangeKind};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Personal", true).unwrap();
    let access = Access::owner(store.account.id);
    let database = store.create_database(&access, volume.id, "first", EntryKind::KvDb {}).unwrap();
    let index = store.db(volume.id, DatabaseId::Index).unwrap();
    let policy = DatabasePolicy::read(&index, DatabaseId::Index).unwrap();
    let frozen = outbound_replica(
        &store.path.join(crate::paths::STAGING),
        &index,
        &policy,
        ReplicaReader::Service,
    )
    .unwrap();
    let root = replica_root(frozen.database()).unwrap();
    let mut feed = store.take_change_receiver().unwrap();
    drop(feed.drain());
    store.rename_entry(&access, volume.id, database.id, "second").unwrap();
    let edits = feed.drain();
    store.acknowledge_provider_received(volume.id, frozen.database(), root).unwrap();
    assert!(!store.provider_item(volume.id, database.id).unwrap().uploaded);
    for record in &edits.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap();
        }
    }
    assert!(store.provider_item(volume.id, database.id).unwrap().uploaded);
}

#[cfg(feature = "connected-engine")]
#[test]
fn provider_late_namespace_ack_does_not_clear_a_newer_rename() {
    use crate::store::{DatabaseId, change_feed::CommittedChangeKind};
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Personal", true).unwrap();
    let access = Access::owner(store.account.id);
    let database = store.create_database(&access, volume.id, "first", EntryKind::KvDb {}).unwrap();
    let root = store.replica_root(volume.id, DatabaseId::Index).unwrap();
    store.acknowledge_provider_snapshot(volume.id, root).unwrap();
    assert!(store.provider_item(volume.id, database.id).unwrap().uploaded);
    let mut feed = store.take_change_receiver().unwrap();
    drop(feed.drain());
    store.rename_entry(&access, volume.id, database.id, "second").unwrap();
    let first = feed.drain();
    store.rename_entry(&access, volume.id, database.id, "third").unwrap();
    let second = feed.drain();
    for record in &first.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap();
        }
    }
    assert!(!store.provider_item(volume.id, database.id).unwrap().uploaded);
    for record in &second.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap();
        }
    }
    let current = store.provider_item(volume.id, database.id).unwrap();
    assert_eq!(current.filename, "third.pbdb");
    assert!(current.uploaded);
    let revision = store.provider_upload_revision(volume.id).unwrap();
    for record in &first.records {
        if record.kind() == CommittedChangeKind::Database(DatabaseId::Index) {
            assert!(!store.acknowledge_provider_delta(volume.id, record.bytes()).unwrap());
        }
    }
    assert_eq!(store.provider_upload_revision(volume.id).unwrap(), revision);
}

#[test]
fn recovered_provider_volume_waits_for_metadata_before_enumeration_or_anchor() {
    // Recovered credentials establish a location, but not an empty directory or
    // an unsupported format. Its first index must arrive before enumeration.
    let directory = tempfile::tempdir().unwrap();
    let source = LocalStore::open(directory.path().join("source")).unwrap();
    let volume = source.create_volume("Personal", true).unwrap();
    let mut recovered = LocalStore::open(directory.path().join("recovered")).unwrap();
    recovered.bind_authenticated_account(source.account().id).unwrap();
    recovered.import_recovery_catalog(&source.recovery_catalog().unwrap(), || true).unwrap();
    let error = recovered.provider_volume(volume.id).unwrap_err();
    assert!(error.to_string().contains("waiting for initial synchronization"));
    assert!(recovered.provider_entries(volume.id, None, None, 128).is_err());
}

#[test]
fn provider_projects_directories_databases_and_blobs_under_current_authority() {
    // Enumerate a nested personal namespace; database edits leave reference bytes/version unchanged.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let volume = store.create_volume("Personal", false).unwrap();
    let access = Access::owner(store.account.id);
    let db =
        store.create_database(&access, volume.id, "folder/settings", EntryKind::KvDb {}).unwrap();
    let blob = store.create_blob(&access, volume.id, "folder/photo.jpg", b"image").unwrap();
    let root = store.provider_entries(volume.id, None, None, 128).unwrap();
    assert_eq!(root.items.len(), 1);
    let folder = root.items[0].id;
    assert_eq!(root.items[0].kind, EntryType::Directory);
    let page = store.provider_entries(volume.id, Some(folder), None, 128).unwrap();
    assert_eq!(page.items.len(), 2);
    let original = page.items.iter().find(|item| item.id == db.id).unwrap().clone();
    assert_eq!(original.filename, "settings.pbdb");
    let reference = original.database_reference(volume.id).unwrap().to_string();
    assert_eq!(reference.len(), original.len_bytes as usize);
    assert_eq!(page.items.iter().find(|item| item.id == blob.id).unwrap().len_bytes, 5);
    store
        .kv_put(
            &access,
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: db.id },
            b"key",
            b"value",
        )
        .unwrap();
    let current = store.provider_entries(volume.id, Some(folder), None, 128).unwrap();
    assert_eq!(current.items.iter().find(|item| item.id == db.id).unwrap(), &original);
    store.rename_entry(&access, volume.id, db.id, "folder/renamed").unwrap();
    let renamed = store.provider_entries(volume.id, Some(folder), None, 128).unwrap();
    let renamed = renamed.items.iter().find(|item| item.id == db.id).unwrap();
    assert_eq!(renamed.id, original.id);
    assert_eq!(renamed.content_version, original.content_version);
    assert_ne!(renamed.metadata_version, original.metadata_version);
    store
        .set_entry_permissions(&access, volume.id, db.id, EntryAcl::Replace(Acl::default()))
        .unwrap();
    assert_eq!(store.provider_entries(volume.id, Some(folder), None, 128).unwrap().items.len(), 1);
    store.delete_volume(volume.id).unwrap();
    assert!(store.provider_entries(volume.id, None, None, 128).is_err());
    assert_eq!(store.provider_retired_volumes(&[volume.id]).unwrap(), [volume.id]);
}

#[test]
fn provider_pages_by_identity_and_never_exposes_session_roots() {
    // Page without losing entries, and ensure a session root cannot become a cloud-drive domain.
    let directory = tempfile::tempdir().unwrap();
    let services = crate::engine::Services::open(directory.path(), || {}).unwrap();
    let store = &services.store;
    let volume = store.create_volume("Personal", false).unwrap();
    let access = Access::owner(store.account.id);
    for index in 0..5 {
        store.create_blob(&access, volume.id, &format!("{index}.txt"), b"bytes").unwrap();
    }
    let mut after = None;
    let mut ids = BTreeSet::new();
    loop {
        let page = store.provider_entries(volume.id, None, after, 2).unwrap();
        for item in page.items {
            assert!(ids.insert(item.id));
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 5);
    assert!(store.provider_entries(volume.id, None, None, 129).is_err());
    let root = store.create_volume("App root", false).unwrap();
    let build = crate::identifier::encode(&[0xab; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    assert!(store.provider_entries(root.id, None, None, 128).is_err());
    assert!(store.provider_entries(session.root_volume, None, None, 128).is_err());
    store.forget_session(&session.id).unwrap();
    assert!(store.provider_entries(root.id, None, None, 128).is_err());
}

#[test]
#[ignore = "operator-run optimized File Provider metadata qualification"]
fn provider_metadata_latency_at_representative_namespace_sizes() {
    use crate::store::catalog::record;
    use crate::store::volume_index::{self, EntryInfo};
    use std::time::Instant;
    for (volume_count, count) in [(1, 32), (32, 32), (4, 250), (40, 250), (128, 1)] {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let mut volumes = Vec::new();
        for number in 0..volume_count {
            let volume = store.create_volume(&format!("Metadata {number}"), true).unwrap();
            let db = store.db(volume.id, crate::store::DatabaseId::Index).unwrap();
            let mut tx = db.begin_write(now_unix_secs()).unwrap();
            for index in 0..count {
                // Independent canonical births keep setup outside the timing boundary.
                let id = EntryId::generate().unwrap();
                let info = EntryInfo {
                    namespace: Default::default(),
                    origin: volume_index::namespace::Origin {
                        installation: store.installation(),
                        label: None,
                    },
                    initial_permissions: EntryAcl::Inherit,
                    created_revision: EntryId::generate().unwrap(),
                    name: format!("Document {index:05}"),
                    kind: EntryKind::KvDb {},
                    protocol: Some(volume_index::DatabaseProtocol::Kvdb),
                };
                tx.put(&volume_index::entry_key(id), &record::encode(&info).unwrap()).unwrap();
            }
            volume_index::repair_all(&mut tx.local_indexes()).unwrap();
            let projection = volume_index::namespace::source::cut::project(&tx).unwrap();
            projection.write_current(&mut tx.local_indexes()).unwrap();
            tx.commit().unwrap();
            // Exercise the complete uploaded-receipt lookup path, including
            // duplicates shared by birth/name/parent, before cached samples.
            store.acknowledge_provider_snapshot(volume.id, replica_root(&db).unwrap()).unwrap();
            let primed = store.provider_entries(volume.id, None, None, 128).unwrap();
            assert!(primed.items.iter().all(|item| item.uploaded));
            volumes.push(volume.id);
        }
        let mut times = Vec::new();
        for sample in 0..100 {
            let start = Instant::now();
            let page =
                store.provider_entries(volumes[sample % volumes.len()], None, None, 128).unwrap();
            assert_eq!(page.items.len(), count.min(128));
            times.push(start.elapsed());
        }
        times.sort();
        let scan = Instant::now();
        let mut seen = 0;
        for volume in volumes {
            let mut after = None;
            loop {
                let page = store.provider_scan(volume, after).unwrap();
                seen += page.items.len();
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        }
        assert_eq!(seen, count * volume_count);
        eprintln!(
            "provider volumes={volume_count} entries_each={count} page128 p99={:?} all_working_sets={:?}",
            times[98],
            scan.elapsed()
        );
        assert!(times[98] < std::time::Duration::from_millis(100));
    }
}
