#![allow(clippy::unwrap_used, reason = "Tests assert successful bounded file imports.")]
use super::*;
use std::io::{self, Cursor};

struct BoundedInput {
    remaining: u64,
    calls: usize,
    interrupt: bool,
    fail_after: Option<usize>,
}
impl Read for BoundedInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        assert!(bytes.len() <= crate::store::BLOB_CHUNK_BYTES);
        self.calls += 1;
        if std::mem::take(&mut self.interrupt) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.fail_after.is_some_and(|calls| self.calls > calls) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let count = self.remaining.min(bytes.len() as u64).min(17017) as usize;
        bytes[..count].fill(37);
        self.remaining -= count as u64;
        Ok(count)
    }
}
fn input(length: u64) -> BoundedInput {
    BoundedInput { remaining: length, calls: 0, interrupt: true, fail_after: None }
}
#[test]
fn large_short_read_import_is_bounded_deduplicated_and_records_publication_time() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Imports", false).unwrap();
    let length = 8 * 1024 * 1024;
    let first =
        store.create_blob_from_reader(&access, volume.id, "one", &mut input(length)).unwrap();
    let second =
        store.create_blob_from_reader(&access, volume.id, "two", &mut input(length)).unwrap();
    assert_ne!(first.id, second.id);
    let a = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: first.id }, None)
        .unwrap();
    let b = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: second.id }, None)
        .unwrap();
    assert_eq!(a.digest, b.digest);
    assert_eq!(a.encrypted_key, b.encrypted_key);
    assert_eq!(a.len_bytes, length);
    assert!(
        matches!(first.info.kind, EntryKind::Blob { published_at_micros, .. } if published_at_micros > 0)
    );
    assert!(
        matches!(second.info.kind, EntryKind::Blob { published_at_micros, .. } if published_at_micros > 0)
    );
    assert_eq!(
        store.verify_blob_version(&access, &a).unwrap().read_range(length - 4, 4).unwrap(),
        [37; 4]
    );
    store.remove_entry(&access, volume.id, first.id).unwrap();
    assert_eq!(store.verify_blob_version(&access, &b).unwrap().read_range(0, 4).unwrap(), [37; 4]);
}
#[test]
fn failed_or_oversized_input_never_publishes_an_entry_or_replaces_the_head() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Imports", false).unwrap();
    let first = store.create_blob(&access, volume.id, "one", b"original").unwrap();
    let mut failing = input(1024 * 1024);
    failing.fail_after = Some(3);
    assert!(store.write_blob_from_reader(&access, volume.id, first.id, &mut failing).is_err());
    assert_eq!(store.read_blob_entry(&access, volume.id, first.id).unwrap(), b"original");
    assert!(matches!(
        store.create_blob_from_reader(
            &access,
            volume.id,
            "too big",
            &mut input(MAX_ENTRY_BLOB_BYTES as u64 + 1)
        ),
        Err(Error::LimitExceeded)
    ));
    assert!(matches!(store.resolve_entry(&access, volume.id, "too big"), Err(Error::NotFound(_))));
    assert_eq!(store.entry_blobs.active.load(Ordering::Acquire), 0);
    let empty =
        store.create_blob_from_reader(&access, volume.id, "empty", &mut Cursor::new([])).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, empty.id).unwrap(), b"");
}

#[test]
fn verified_receipt_repairs_shared_corrupt_content_and_retains_old_bytes_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Repair", false).unwrap();
    let first = store.create_blob(&access, volume.id, "one", b"shared").unwrap();
    let second = store.create_blob(&access, volume.id, "two", b"shared").unwrap();
    let version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: first.id }, None)
        .unwrap();
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    let retained = store.blob_path(&version.digest, false);
    let cached = store.blob_path(&version.digest, true);
    fs::create_dir_all(cached.parent().unwrap()).unwrap();
    fs::write(&cached, b"badbad").unwrap();
    fs::write(&retained, b"broken").unwrap();
    assert!(matches!(store.verify_blob_version(&access, &version), Err(Error::Corrupt(_))));
    store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
    assert_eq!(fs::read(&retained).unwrap(), b"shared");
    assert_eq!(fs::read(&cached).unwrap(), b"shared");
    let mut quarantine: Vec<_> = fs::read_dir(store.path.join(crate::paths::CORRUPT_BLOBS))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    quarantine.sort();
    assert_eq!(quarantine, [b"badbad".to_vec(), b"broken".to_vec()]);
    assert_eq!(store.read_blob_entry(&access, volume.id, second.id).unwrap(), b"shared");
    assert_eq!(
        store.pending_entry_blobs(&access, volume.id, None).unwrap().versions.len(),
        1,
        "the other entry's pending shared reference still pins its durable copy"
    );
    store.evict_blobs(0).unwrap();
    assert!(retained.exists());
    drop(store);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.read_blob_entry(&access, volume.id, first.id).unwrap(), b"shared");
    assert_eq!(store.read_blob_entry(&access, volume.id, second.id).unwrap(), b"shared");
    assert_eq!(fs::read_dir(store.path.join(crate::paths::CORRUPT_BLOBS)).unwrap().count(), 2);
}

#[test]
fn received_repair_rechecks_read_and_rejects_corrupt_ciphertext_without_publication() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Repair", false).unwrap();
    let entry = store.create_blob(&access, volume.id, "one", b"original").unwrap();
    let version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
        .unwrap();
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    let mut bytes = Vec::new();
    encrypted.read_to_end(&mut bytes).unwrap();
    fs::write(store.blob_path(&version.digest, false), b"broken").unwrap();
    let mut corrupt = bytes.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert!(store.receive_blob_version(&access, &version, &mut &corrupt[..], true).is_err());
    assert!(!store.path.join(crate::paths::CORRUPT_BLOBS).exists());
    let denied = store.clone();
    store
        .set_blob_publication_hook(Some(Arc::new(move || {
            denied.set_entry_permissions(
                &access,
                volume.id,
                entry.id,
                EntryAcl::Replace(Acl::default()),
            )
        })))
        .unwrap();
    assert!(matches!(
        store.receive_blob_version(
            &Access::owner(store.account().id),
            &version,
            &mut &bytes[..],
            true
        ),
        Err(Error::AccessDenied(_))
    ));
    store.set_blob_publication_hook(None).unwrap();
    assert_eq!(fs::read(store.blob_path(&version.digest, false)).unwrap(), b"broken");
    assert!(!store.path.join(crate::paths::CORRUPT_BLOBS).exists());
    assert_eq!(store.entry_blobs.active.load(Ordering::Acquire), 0);
}

#[test]
fn repair_does_not_replace_a_target_changed_after_inspection() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Repair", false).unwrap();
    let entry = store.create_blob(&access, volume.id, "one", b"original").unwrap();
    let version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
        .unwrap();
    let target = store.blob_path(&version.digest, false);
    fs::write(&target, b"broken").unwrap();
    let inspected = store.inspect_blob(&version, &target).unwrap();
    let mut stage = blob_files::BlobStage::new(&store.path.join("staging/repair-test")).unwrap();
    stage.file.write_all(b"original").unwrap();
    stage.sync().unwrap();
    let other = store.path.join("different");
    fs::write(&other, b"changed").unwrap();
    fs::rename(other, &target).unwrap();
    assert!(!inspected.publish(&stage, &target, true).unwrap());
    assert_eq!(fs::read(target).unwrap(), b"changed");
    assert!(!store.path.join(crate::paths::CORRUPT_BLOBS).exists());
}

#[test]
fn corrupt_repair_crash_child() {
    let Ok(profile) = std::env::var("ARCHIPELAGO_BLOB_REPAIR_CRASH_PROFILE") else { return };
    let store = LocalStore::open(Path::new(&profile)).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Repair", false).unwrap();
    let entry = store.create_blob(&access, volume.id, "one", b"original").unwrap();
    let version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
        .unwrap();
    let mut encrypted = store.encrypt_blob_version(&access, &version).unwrap();
    fs::write(store.blob_path(&version.digest, false), b"broken").unwrap();
    blob_files::CRASH_AFTER_QUARANTINE.set(true);
    store.receive_blob_version(&access, &version, &mut encrypted, true).unwrap();
    panic!("the repair checkpoint must terminate without running destructors");
}

#[test]
fn crash_after_quarantine_keeps_original_pending_content_and_retained_copy() {
    let directory = tempfile::tempdir().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "store::local::entry_blobs::import_tests::corrupt_repair_crash_child"])
        .env("ARCHIPELAGO_BLOB_REPAIR_CRASH_PROFILE", directory.path())
        .output()
        .unwrap();
    assert_eq!(child.status.code(), Some(73), "{}", String::from_utf8_lossy(&child.stderr));
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.volumes().unwrap().remove(0);
    let entry = store.resolve_entry(&access, volume.id, "one").unwrap();
    let version = store
        .blob_version(&access, EntryAddress { volume: volume.id, entry: entry.id }, None)
        .unwrap();
    assert_eq!(fs::read(store.blob_path(&version.digest, false)).unwrap(), b"broken");
    let retained: Vec<_> = fs::read_dir(store.path.join(crate::paths::CORRUPT_BLOBS))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(retained, [b"broken".to_vec()]);
    assert_eq!(store.pending_entry_blobs(&access, volume.id, None).unwrap().versions.len(), 1);
    assert!(!store.blob_version_durable(&access, &version).unwrap());
}

#[test]
fn publication_clock_is_sampled_after_preparation_and_retained_across_rename() {
    use std::sync::atomic::AtomicI64;
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let access = Access::owner(store.account().id);
    let volume = store.create_volume("Timestamps", false).unwrap();
    let lower = Arc::new(AtomicI64::new(0));
    let observed = lower.clone();
    *store.entry_blobs.before_publish.lock().unwrap() = Some(Arc::new(move || {
        observed.store(publication_time()?, Ordering::SeqCst);
        Ok(())
    }));
    let first = store.create_blob(&access, volume.id, "file", b"unchanged").unwrap();
    let EntryKind::Blob { published_at_micros: first_time, digest: first_digest, .. } =
        first.info.kind
    else {
        unreachable!()
    };
    assert!(first_time >= lower.load(Ordering::SeqCst));
    assert!(first_time <= publication_time().unwrap());
    store.rename_entry(&access, volume.id, first.id, "renamed").unwrap();
    let renamed = store.entry(&access, volume.id, first.id).unwrap();
    assert_eq!(renamed.info.kind, first.info.kind);
    let second = store.write_blob(&access, volume.id, first.id, b"unchanged").unwrap();
    let EntryKind::Blob { published_at_micros: second_time, digest: second_digest, .. } =
        second.info.kind
    else {
        unreachable!()
    };
    assert_ne!(second.content_revision, first.content_revision);
    assert_eq!(first_digest, second_digest);
    assert!(second_time >= lower.load(Ordering::SeqCst));
    let address = EntryAddress { volume: volume.id, entry: first.id };
    let retained = store.entry_revisions(&access, address, None, 64).unwrap();
    assert!(retained.revisions.iter().any(|revision| {
        matches!(&revision.change, volume_index::EntryChange::Created { info }
            if info.kind == first.info.kind)
    }));
}

#[test]
fn timestamp_conversion_floors_submicrosecond_precision_before_epoch() {
    use std::time::Duration;
    assert_eq!(timestamp_micros(UNIX_EPOCH).unwrap(), 0);
    assert_eq!(timestamp_micros(UNIX_EPOCH + Duration::from_nanos(1999)).unwrap(), 1);
    assert_eq!(timestamp_micros(UNIX_EPOCH - Duration::from_nanos(1)).unwrap(), -1);
    assert_eq!(timestamp_micros(UNIX_EPOCH - Duration::from_nanos(1001)).unwrap(), -2);
}
