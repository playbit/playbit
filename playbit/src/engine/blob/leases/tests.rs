use super::*;
use crate::store::{
    EntryId,
    object_contract::{Acl, EntryAcl, Grant, Permission, Subject},
};
use std::{sync::mpsc, time::Duration};

fn setup() -> (tempfile::TempDir, Arc<LocalStore>, Access, BlobVersion) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("File fixture", false).unwrap();
    let entry = store.create_blob(&access, volume.id, "file.bin", &[7; 65537]).unwrap();
    let version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
        .unwrap();
    (directory, store, access, version)
}

#[test]
fn pinned_descriptors_bound_admission_and_unpinned_descriptors_can_be_reclaimed() {
    let (_directory, store, access, version) = setup();
    let mut pinned = Vec::new();
    for n in 0..LEASE_LIMIT {
        let mut candidate = version.clone();
        candidate.revision = EntryId::from_bytes([n as u8 + 1; 15]).unwrap();
        let file = Arc::new(store.verify_blob_version(&access, &version).unwrap());
        store.cache_verified_blob(candidate.clone(), access.clone(), file.clone(), 0).unwrap();
        assert!(Arc::ptr_eq(
            &file,
            &store.cached_verified_blob(&candidate, &access).unwrap().unwrap()
        ));
        pinned.push(file);
    }
    let extra = Arc::new(store.verify_blob_version(&access, &version).unwrap());
    assert!(matches!(
        store.cache_verified_blob(version.clone(), access.clone(), extra.clone(), 0),
        Err(Error::LimitExceeded)
    ));
    assert_eq!(store.verified_blob_count(), LEASE_LIMIT);
    pinned.pop();
    store.cache_verified_blob(version.clone(), access.clone(), extra, 0).unwrap();
    assert_eq!(store.verified_blob_count(), LEASE_LIMIT);
    store.invalidate_verified_blob(&version).unwrap();
    assert!(store.cached_verified_blob(&version, &access).unwrap().is_none());
}

#[cfg(feature = "connected-engine")]
#[test]
fn connected_transport_reads_resident_bytes_without_a_live_runtime_or_connection() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport = BlobTransport::new(
        runtime.handle().clone(),
        Arc::new(Mutex::new(None)),
        Arc::new(Default::default()),
        #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
        None,
    );
    drop(runtime);
    assert!(store.cached_verified_blob(&version, &access).unwrap().is_none());
    let (actual, file) = transport.verified_file(store, access, None, version.address).unwrap();
    assert_eq!(actual, version);
    assert_eq!(file.read_range(65534, 8).unwrap(), [7; 3]);
    assert_eq!(transport.transfers.bulk.available_permits(), 2);
}

#[test]
fn owned_reads_share_descriptors_and_recheck_rename_replacement_and_retirement() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let (_, first) =
        transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    store
        .rename_entry(&access, version.address.volume, version.address.entry, "renamed.bin")
        .unwrap();
    let (_, renamed) =
        transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    assert!(Arc::ptr_eq(&first, &renamed));
    assert_eq!(renamed.read_range(65534, 8).unwrap(), [7; 3]);
    store
        .write_blob(&access, version.address.volume, version.address.entry, b"replacement")
        .unwrap();
    let (replacement, next) =
        transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    assert_ne!(version.revision, replacement.revision);
    assert!(!Arc::ptr_eq(&first, &next));
    assert_eq!(next.read_range(0, 8).unwrap(), b"replacem");
    store.remove_entry(&access, version.address.volume, version.address.entry).unwrap();
    assert!(transport.verified_file(store, access, None, version.address).is_err());
}

#[test]
fn cached_descriptor_does_not_bypass_current_entry_override() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    store
        .set_entry_permissions(
            &access,
            version.address.volume,
            version.address.entry,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let denied = transport.verified_file(store, access, None, version.address).err().unwrap();
    assert!(matches!(denied.downcast_ref::<Error>(), Some(Error::AccessDenied(_))));
}

#[test]
fn range_corruption_invalidates_a_cached_descriptor_and_fresh_verification_is_required() {
    let (directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let (_, file) =
        transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    let digest: String = version.digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = directory.path().join(crate::paths::BLOBS).join(digest);
    std::fs::write(&path, [42; 65537]).unwrap();
    assert!(matches!(file.read_range(0, 8), Err(Error::Corrupt(_))));
    let error = transport
        .read_range(store.clone(), access.clone(), None, version.address, 0, 8)
        .unwrap_err();
    assert!(matches!(error.downcast_ref::<Error>(), Some(Error::Corrupt(_))));
    assert!(store.verified_blob_count() == 0);
    assert_eq!(std::fs::read(&path).unwrap(), [42; 65537]);
    std::fs::write(&path, [7; 65537]).unwrap();
    let (_, bytes) = transport
        .read_range(store.clone(), access.clone(), None, version.address, 65534, 8)
        .unwrap();
    assert_eq!(bytes, [7; 3]);
    let (_, repaired) = transport.verified_file(store, access, None, version.address).unwrap();
    assert!(!Arc::ptr_eq(&file, &repaired));
    assert_eq!(repaired.read_range(0, 8).unwrap(), [7; 8]);
}

#[test]
fn captured_revision_survives_replacement_and_retirement_but_checks_read_withdrawal() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    store.write_blob(&access, version.address.volume, version.address.entry, b"new").unwrap();
    // A later publication changes selection, not the already captured revision.
    let retained = transport.prepare_file(store.clone(), access.clone(), None, &version).unwrap();
    assert_eq!(retained.read_range(0, 8).unwrap(), [7; 8]);
    let next = store.blob_version(&access, version.address, None).unwrap();
    store
        .set_entry_permissions(
            &access,
            version.address.volume,
            version.address.entry,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    let denied = transport.prepare_file(store.clone(), access.clone(), None, &next).err().unwrap();
    assert!(matches!(denied.downcast_ref::<Error>(), Some(Error::AccessDenied(_))));
    store
        .set_entry_permissions(
            &access,
            version.address.volume,
            version.address.entry,
            EntryAcl::Inherit,
        )
        .unwrap();
    store.remove_entry(&access, version.address.volume, version.address.entry).unwrap();
    assert_eq!(
        transport
            .prepare_file(store.clone(), access.clone(), None, &next)
            .unwrap()
            .read_range(0, 8)
            .unwrap(),
        b"new"
    );
    assert_eq!(
        transport
            .prepare_file(store.clone(), access.clone(), None, &version)
            .unwrap()
            .read_range(0, 8)
            .unwrap(),
        [7; 8]
    );
    assert!(transport.open_read(store, access, None, version.address).is_err());
}

#[test]
fn corrupt_cached_range_cannot_queue_when_both_bulk_credits_are_held() {
    let (directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    let first = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let second = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let digest: String = version.digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = directory.path().join(crate::paths::BLOBS).join(digest);
    std::fs::write(&path, b"corrupt").unwrap();
    let error =
        transport.read_range(store.clone(), access, None, version.address, 0, 8).unwrap_err();
    assert!(matches!(error.downcast_ref::<Error>(), Some(Error::LimitExceeded)));
    assert!(store.verified_blob_count() == 0);
    assert_eq!(std::fs::read(path).unwrap(), b"corrupt");
    assert_eq!(transport.transfers.bulk.available_permits(), 0);
    drop((first, second));
}

#[test]
fn eviction_expires_idle_leases_but_preserves_the_active_verified_handle() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let (_, active) =
        transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    let generation = store.blob_eviction_generation();
    // Pending content remains pinned and its descriptor reusable.
    assert_eq!(store.set_cache_budget(0).unwrap(), version.len_bytes);
    assert_eq!(store.blob_eviction_generation(), generation);
    let (_, pending) =
        transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    assert!(Arc::ptr_eq(&active, &pending));
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
    store.acknowledge_blob_version(&access, &version).unwrap();
    assert_eq!(store.set_cache_budget(0).unwrap(), 0);
    assert!(store.blob_eviction_generation() > generation);
    assert!(store.verified_blob_count() == 0);
    assert_eq!(active.read_range(65534, 8).unwrap(), [7; 3]);
    let error =
        transport.read_range(store.clone(), access, None, version.address, 0, 8).unwrap_err();
    assert!(matches!(error.downcast_ref::<Error>(), Some(Error::NotFound("blob content"))));
    assert!(store.verified_blob_count() == 0);
}

#[test]
fn saturated_bulk_credits_reject_a_cache_miss_without_queuing() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let first = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let second = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let failure = transport
        .verified_file(store.clone(), access.clone(), None, version.address)
        .err()
        .unwrap();
    assert!(matches!(failure.downcast_ref::<Error>(), Some(Error::LimitExceeded)));
    assert!(store.verified_blob_count() == 0);
    drop(first);
    transport.verified_file(store.clone(), access.clone(), None, version.address).unwrap();
    let held = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    // A ready descriptor consumes no bulk permit, but still checks authority.
    assert!(transport.verified_file(store, access, None, version.address).is_ok());
    drop((second, held));
}

#[test]
fn cancelled_parent_retains_transfer_credit_until_blocking_child_finishes() {
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(2));
    let (started, entered) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let permits = semaphore.clone();
    let parent = runtime.spawn(async move {
        let credit = Arc::new(permits.acquire_owned().await.unwrap());
        blocking(store, access, version, credit, move |_, _, _| {
            started.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .await
        .unwrap();
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    parent.abort();
    runtime.block_on(async {
        assert!(parent.await.unwrap_err().is_cancelled());
    });
    assert_eq!(semaphore.available_permits(), 1);
    release.send(()).unwrap();
    // Runtime shutdown joins the real child; no detached credit can be reused early.
    drop(runtime);
    assert_eq!(semaphore.available_permits(), 2);
}

#[test]
fn open_captures_metadata_without_hydration_and_reads_only_its_revision() {
    // Hold both bulk credits to prove open performs no hydration, then replace
    // content before the first read and check the old and newly opened views.
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let first = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let second = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let mut view =
        transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    assert_eq!(view.version().unwrap(), &version);
    assert!(view.file.is_none());
    assert!(view.read(&transport, 0, 8).is_err());
    drop((first, second));
    store.write_blob(&access, version.address.volume, version.address.entry, b"new").unwrap();
    assert_eq!(view.read(&transport, 0, 8).unwrap(), [7; 8]);
    let mut new = transport.open_read(store, access, None, version.address).unwrap();
    assert_eq!(new.read(&transport, 0, 8).unwrap(), b"new");
    assert_eq!(view.read(&transport, 65534, 8).unwrap(), [7; 3]);
}

#[test]
fn read_view_admission_accounts_for_unhydrated_views_and_releases_credit() {
    // Metadata-only handles consume the same bounded view pool until dropped.
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let mut views = Vec::new();
    for _ in 0..LEASE_LIMIT {
        views.push(
            transport.open_read(store.clone(), access.clone(), None, version.address).unwrap(),
        );
    }
    assert!(matches!(
        transport
            .open_read(store.clone(), access.clone(), None, version.address)
            .err()
            .unwrap()
            .downcast_ref::<Error>(),
        Some(Error::LimitExceeded)
    ));
    views.pop();
    let view = transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    views.clear();
    drop(view);
    for _ in 0..LEASE_LIMIT {
        views.push(
            transport.open_read(store.clone(), access.clone(), None, version.address).unwrap(),
        );
    }
}

#[test]
fn empty_and_eof_reads_validate_extent_and_authority_without_fetching() {
    // Empty/EOF requests need metadata and authority, but never bulk storage.
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let mut view =
        transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    let first = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    let second = transport.transfers.bulk.clone().try_acquire_owned().unwrap();
    for (offset, length) in [(0, 0), (version.len_bytes, 1), (version.len_bytes + 1, 1)] {
        assert!(view.read(&transport, offset, length).unwrap().is_empty());
    }
    assert!(view.read(&transport, u64::MAX, 0).is_err());
    assert!(view.read(&transport, 0, 65537).is_err());
    store
        .set_entry_permissions(
            &access,
            version.address.volume,
            version.address.entry,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Public,
                    permissions: vec![Permission::Write],
                }],
            }),
        )
        .unwrap();
    assert!(view.version().is_err());
    assert!(view.read(&transport, 0, 0).is_err());
    assert!(view.read(&transport, version.len_bytes, 1).is_err());
    drop((first, second));
}

#[test]
fn unopened_contents_remain_charged_and_pinned_until_view_release() {
    // Open pins resident bytes before verification. Eviction reports their
    // charge and cannot remove them until the view releases its storage lease.
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let mut view =
        transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    assert!(view.file.is_none());
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
    store.acknowledge_blob_version(&access, &version).unwrap();
    assert_eq!(store.set_cache_budget(0).unwrap(), version.len_bytes);
    store.remove_entry(&access, version.address.volume, version.address.entry).unwrap();
    assert_eq!(view.read(&transport, 0, 8).unwrap(), [7; 8]);
    assert_eq!(view.read(&transport, 65534, 8).unwrap(), [7; 3]);
    let sibling =
        transport.open_read_version(store.clone(), access.clone(), None, version.clone()).unwrap();
    view.close().unwrap();
    assert!(
        store.verify_blob_version(&access, &version).is_ok(),
        "sibling keeps the revision pinned"
    );
    drop(sibling);
    assert!(
        matches!(
            store.verify_blob_version(&access, &version),
            Err(Error::NotFound("blob content"))
        ),
        "last disposal enforces the disk budget without another cache operation"
    );
    assert!(
        store.verified_blob_count() == 0,
        "retirement also releases idle descriptors of evicted contents"
    );
}

#[test]
fn independent_transports_share_the_store_read_view_limit() {
    // A second transport cannot evade aggregate admission for this profile.
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let first = BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let second = BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let mut views = Vec::new();
    for _ in 0..LEASE_LIMIT {
        views.push(first.open_read(store.clone(), access.clone(), None, version.address).unwrap());
    }
    assert!(matches!(
        second
            .open_read(store.clone(), access.clone(), None, version.address)
            .err()
            .unwrap()
            .downcast_ref::<Error>(),
        Some(Error::LimitExceeded)
    ));
    views.pop();
    let extra = second.open_read(store, access, None, version.address).unwrap();
    assert_eq!(extra.version().unwrap(), &version);
}

#[test]
#[ignore = "local blob view latency qualification; not a CI timing assertion"]
fn benchmark_blob_views_during_concurrent_publication() {
    // Worker-path timings include admission, storage reads and authorization.
    // Packaged frame/readiness qualification remains a separate measurement.
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_directory, store, access, version) = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport =
        BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
    let other = store.create_volume("Concurrent publication", false).unwrap();
    let entry = store.create_blob(&access, other.id, "other", b"initial").unwrap();
    let done = AtomicBool::new(false);
    let mut opens = Vec::with_capacity(1000);
    let mut reads = Vec::with_capacity(1000);
    let mut retained = Vec::with_capacity(1000);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut count = 0;
            for n in 0u64..10_000 {
                if done.load(Ordering::Acquire) {
                    break;
                }
                store.write_blob(&access, other.id, entry.id, &n.to_le_bytes()).unwrap();
                count += 1;
            }
            count
        });
        for _ in 0..1000 {
            let start = std::time::Instant::now();
            let mut view =
                transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
            opens.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            assert_eq!(view.read(&transport, 0, 65536).unwrap(), vec![7; 65536]);
            reads.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let mut view =
            transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
        for _ in 0..1000 {
            let start = std::time::Instant::now();
            assert_eq!(view.read(&transport, 0, 65536).unwrap(), vec![7; 65536]);
            retained.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        done.store(true, Ordering::Release);
        eprintln!("concurrent publications: {}", writer.join().unwrap());
    });
    for (name, mut times) in [
        ("blob-view-open", opens),
        ("blob-view-read-64KiB", reads),
        ("blob-view-retained-read-64KiB", retained),
    ] {
        times.sort_by(f64::total_cmp);
        eprintln!(
            "{name} n={} p50={:.6} p99={:.6} max={:.6} ms",
            times.len(),
            times[499],
            times[989],
            times[999]
        );
    }
}

#[test]
fn shared_transports_release_deferred_contents_after_the_last_late_reader() {
    // Cache ownership follows the store, including dormant transports. A late
    // reader retains the same charge and cannot lose its backing on another close.
    let (_directory, store, access, version) = setup();
    let first_transport = BlobTransport::local();
    let second_transport = BlobTransport::local();
    let mut first =
        first_transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    first.read(&first_transport, 0, 8).unwrap();
    let second =
        second_transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    assert!(Arc::ptr_eq(first.file.as_ref().unwrap(), second.file.as_ref().unwrap()));
    let descriptor = Arc::downgrade(first.file.as_ref().unwrap());
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
    store.acknowledge_blob_version(&access, &version).unwrap();
    assert_eq!(store.set_cache_budget(0).unwrap(), version.len_bytes);
    first.close().unwrap();
    let mut late =
        first_transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    drop(second);
    assert_eq!(late.read(&first_transport, 65534, 8).unwrap(), [7; 3]);
    late.close().unwrap();
    assert!(descriptor.upgrade().is_none(), "no dormant transport retains evicted backing");
    assert_eq!(store.verified_blob_count(), 0);
    assert!(matches!(
        store.verify_blob_version(&access, &version),
        Err(Error::NotFound("blob content"))
    ));
}

#[test]
fn cached_read_close_skips_eviction_and_deferred_close_reports_failure() {
    // A malformed cache entry makes a scan fail. Ordinary warmed reads must not
    // scan; the last deferred close must report that exact dependency failure.
    let (directory, store, access, version) = setup();
    let transport = BlobTransport::local();
    let mut view =
        transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    view.read(&transport, 0, 8).unwrap();
    let malformed = directory.path().join("blobs/not-a-digest");
    std::fs::write(&malformed, b"bad cache entry").unwrap();
    view.close().unwrap();
    let mut view =
        transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    assert_eq!(view.read(&transport, 0, 8).unwrap(), [7; 8]);
    view.close().unwrap();
    std::fs::remove_file(&malformed).unwrap();
    let view = transport.open_read(store.clone(), access.clone(), None, version.address).unwrap();
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
    store.acknowledge_blob_version(&access, &version).unwrap();
    assert_eq!(store.set_cache_budget(0).unwrap(), version.len_bytes);
    std::fs::write(&malformed, b"bad cache entry").unwrap();
    let error = view.close().unwrap_err();
    assert!(matches!(error.downcast_ref::<Error>(), Some(Error::Corrupt(_))));
    std::fs::remove_file(&malformed).unwrap();
    assert_eq!(
        store.enforce_cache_budget().unwrap(),
        0,
        "failed cleanup still releases the reader pin"
    );
}

#[test]
fn failed_eviction_preserves_other_readers_deferred_cleanup() {
    // A retry failing before it visits another digest cannot erase that digest's
    // pending cleanup. Repairing the failure needs no extra explicit cache pass.
    let (directory, store, access, first_version) = setup();
    let second_entry =
        store.create_blob(&access, first_version.address.volume, "second.bin", b"second").unwrap();
    let second_version = store
        .blob_version(
            &access,
            EntryAddress { volume: first_version.address.volume, entry: second_entry.id },
            None,
        )
        .unwrap();
    let transport = BlobTransport::local();
    let first = transport
        .open_read_version(store.clone(), access.clone(), None, first_version.clone())
        .unwrap();
    let second = transport
        .open_read_version(store.clone(), access.clone(), None, second_version.clone())
        .unwrap();
    for version in [&first_version, &second_version] {
        let mut encrypted = store.encrypt_blob_version(&access, version).unwrap();
        store.receive_blob_version(&access, version, &mut encrypted, true).unwrap();
        store.acknowledge_blob_version(&access, version).unwrap();
    }
    assert_eq!(
        store.set_cache_budget(0).unwrap(),
        first_version.len_bytes + second_version.len_bytes
    );
    let malformed = directory.path().join("cache/blobs/not-a-bucket");
    std::fs::write(&malformed, b"invalid").unwrap();
    assert!(first.close().is_err());
    std::fs::remove_file(&malformed).unwrap();
    second.close().unwrap();
    for version in [&first_version, &second_version] {
        assert!(matches!(
            store.verify_blob_version(&access, version),
            Err(Error::NotFound("blob content"))
        ));
    }
}

fn confirm_current_sources(store: &LocalStore, volume: crate::store::Id) {
    use crate::store::volume_index::namespace::{
        self as ns,
        observations::{Ledger, Observation, Status},
    };
    store
        .with_replica(volume, crate::store::DatabaseId::Index, |index| {
            let snapshot = index.begin_read()?;
            let accepted = ns::source::cut::accepted(&snapshot)?;
            let witnesses = ns::source::cut::delivered(&snapshot, &accepted)?;
            let mut scan = ns::source::cut::Scan::materializing(0)?;
            let rows = witnesses
                .into_iter()
                .map(|source| {
                    let bytes = ns::payloads::capture(&snapshot, &source, &mut scan, 0)?;
                    let row = Observation {
                        assignment: Some(accepted[&source.id()?]),
                        source,
                        status: Status::Confirmed,
                    };
                    Ok((row, bytes))
                })
                .collect::<crate::store::Result<Vec<_>>>()?;
            drop(snapshot);
            let mut tx = index.begin_write(0)?;
            let before = Ledger::load(&tx.local_indexes())?;
            let mut after = before.clone();
            after.apply_receipt(&rows.iter().map(|(row, _)| row.clone()).collect::<Vec<_>>())?;
            after.write_changes(&before, &mut tx.local_indexes())?;
            for (row, bytes) in rows {
                ns::payloads::retain(&mut tx.local_indexes(), &row, &bytes, 0)?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();
}

fn replace_same_revision(store: &LocalStore, access: &Access, version: &BlobVersion) {
    use crate::store::{
        DatabaseId,
        volume_index::{self, EntryChange, IndexSnapshot},
    };
    // Model a concurrently authored source: same original observed context,
    // different payload/clock. Its canonical key wins; it never observed A.
    let backing =
        store.create_blob(access, version.address.volume, "other", b"replacement").unwrap();
    store
        .with_replica(version.address.volume, DatabaseId::Index, |index| {
            let snapshot = index.begin_read()?;
            let mut record = IndexSnapshot::new(index.begin_read()?)
                .revision(version.address.entry, version.revision)?;
            let old = crate::store::catalog::record::encode(&record)?;
            let keys = snapshot
                .iter()
                .filter_map(|row| match row {
                    Ok((key, value)) if value.as_ref() == old => Some(Ok(key.to_vec())),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            assert_eq!(keys.len(), 2, "content head and history copies");
            drop(snapshot);
            let mut tx = index.begin_write(0)?;
            record.origin_clock = tx.clock().as_u64();
            record.change = EntryChange::Content { kind: backing.info.kind.clone() };
            let bytes = crate::store::catalog::record::encode(&record)?;
            for key in keys {
                tx.put(&key, &bytes)?;
            }
            volume_index::repair(&mut tx.local_indexes(), &[version.address.entry])?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    confirm_current_sources(store, version.address.volume);
}

#[test]
fn captured_readers_survive_same_revision_replacement_and_recheck_current_read() {
    // Both unconfirmed local and confirmed captures survive a winning exact
    // revision collision, with and without a descriptor acquired before receipt.
    for confirmed in [false, true] {
        let (directory, store, access, initial) = setup();
        store
            .write_blob(&access, initial.address.volume, initial.address.entry, b"captured")
            .unwrap();
        let version = store.blob_version(&access, initial.address, None).unwrap();
        if confirmed {
            confirm_current_sources(&store, version.address.volume);
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let transport =
            BlobTransport::new_local(runtime.handle().clone(), Arc::new(Default::default()));
        let mut warm = transport
            .open_read_version(store.clone(), access.clone(), None, version.clone())
            .unwrap();
        let mut cold = transport
            .open_read_version(store.clone(), access.clone(), None, version.clone())
            .unwrap();
        assert_eq!(warm.read(&transport, 0, 64).unwrap(), b"captured");
        replace_same_revision(&store, &access, &version);
        assert_ne!(
            store.blob_version(&access, version.address, Some(version.revision)).unwrap(),
            version
        );
        assert_eq!(warm.read(&transport, 0, 64).unwrap(), b"captured");
        assert_eq!(cold.read(&transport, 0, 64).unwrap(), b"captured");
        let mut forged = version.clone();
        forged.encrypted_key[0] ^= 1;
        assert!(store.validate_blob_version(&access, &forged).is_err());
        store
            .set_entry_permissions(
                &access,
                version.address.volume,
                version.address.entry,
                EntryAcl::Replace(Acl {
                    grants: vec![Grant {
                        subject: Subject::Public,
                        permissions: vec![Permission::Write],
                    }],
                }),
            )
            .unwrap();
        assert!(warm.read(&transport, 0, 64).is_err());
        assert!(cold.version().is_err());
        store
            .set_entry_permissions(
                &access,
                version.address.volume,
                version.address.entry,
                EntryAcl::Inherit,
            )
            .unwrap();
        warm.close().unwrap();
        cold.close().unwrap();
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        if confirmed {
            assert_eq!(store.read_blob_version(&access, &version).unwrap(), b"captured");
        } else {
            // Temporary capture evidence expires with its File leases.
            assert!(store.validate_blob_version(&access, &version).is_err());
        }
    }
}

#[test]
fn captured_readers_reject_replaced_birth_with_current_read_permission() {
    // Replacing a birth at the same ID must invalidate both warm and cold views,
    // even when the replacement grants Read and reuses the exact body identity.
    use crate::store::{
        DatabaseId,
        volume_index::{self, IndexSnapshot},
    };
    let (_directory, store, access, version) = setup();
    let transport = BlobTransport::local();
    let mut warm =
        transport.open_read_version(store.clone(), access.clone(), None, version.clone()).unwrap();
    let cold =
        transport.open_read_version(store.clone(), access.clone(), None, version.clone()).unwrap();
    warm.read(&transport, 0, 8).unwrap();
    store
        .with_replica(version.address.volume, DatabaseId::Index, |index| {
            let snapshot = index.begin_read()?;
            let original =
                IndexSnapshot::new(index.begin_read()?).entry(version.address.entry)?.info;
            let original_bytes = crate::store::catalog::record::encode(&original)?;
            let keys = snapshot
                .iter()
                .filter_map(|row| match row {
                    Ok((key, value)) if value.as_ref() == original_bytes => Some(Ok(key.to_vec())),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            assert_eq!(keys.len(), 1);
            drop(snapshot);
            let mut replacement = original;
            replacement.origin.label = Some("Different creation".into());
            let bytes = crate::store::catalog::record::encode(&replacement)?;
            let mut tx = index.begin_write(0)?;
            tx.put(&keys[0], &bytes)?;
            volume_index::repair(&mut tx.local_indexes(), &[version.address.entry])?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    // A fresh view of this permitted new birth succeeds but cannot rescue an
    // old lease via the shared exact-version capture registry.
    let fresh =
        transport.open_read_version(store.clone(), access.clone(), None, version.clone()).unwrap();
    assert!(fresh.version().is_ok());
    assert!(warm.read(&transport, 0, 8).is_err());
    assert!(cold.version().is_err());
    warm.close().unwrap();
    cold.close().unwrap();
    assert!(fresh.version().is_ok());
    fresh.close().unwrap();
}
