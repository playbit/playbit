#![allow(
    clippy::unwrap_used,
    reason = "Tests retain and compare real profile bytes at admission boundaries."
)]
use super::*;

fn files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            result.extend(files(&entry.path()));
        } else {
            result.insert(entry.path(), fs::read(entry.path()).unwrap());
        }
    }
    result
}
#[test]
fn incompatible_and_markerless_profiles_refuse_before_any_retained_bytes_change() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let store = LocalStore::open(path).unwrap();
    let account = store.account().id;
    let volume = store.create_volume("Retained", true).unwrap();
    let pending = store.pending_operations().unwrap();
    drop(store);
    fs::create_dir(path.join("staging")).unwrap();
    fs::write(path.join("staging/unpublished"), b"retain until compatible admission").unwrap();
    for bytes in [
        b"{\"format\":\"playbit.profile\",\"revision\":2,\"required_read_features\":[],\
          \"required_write_features\":[]}"
            .as_slice(),
        b"{\"format\":\"playbit.profile\",\"revision\":1,\"required_read_features\":[],\
          \"required_write_features\":[\"future.authority\"]}",
        b"{",
    ] {
        fs::write(path.join(FILE), bytes).unwrap();
        let before = files(path);
        assert!(LocalStore::open(path).is_err());
        assert_eq!(files(path), before);
    }
    fs::remove_file(path.join(FILE)).unwrap();
    let before = files(path);
    assert!(LocalStore::open(path).is_err());
    assert_eq!(files(path), before);
    fs::write(path.join(FILE), profile::format::BASELINE).unwrap();
    let reopened = LocalStore::open(path).unwrap();
    assert_eq!(reopened.account().id, account);
    assert_eq!(reopened.volume(volume.id).unwrap(), volume);
    assert_eq!(reopened.pending_operations().unwrap(), pending);
}
#[test]
fn exclusive_lifetime_owner_prevents_an_overlapping_format_writer() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    assert!(matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_))));
    assert!(ProfileLock::open(&directory.path().join(crate::paths::LOCAL_LOCK)).is_err());
    drop(store);
    let lock = ProfileLock::open(&directory.path().join(crate::paths::LOCAL_LOCK)).unwrap();
    let mut next: serde_json::Value = serde_json::from_slice(profile::format::BASELINE).unwrap();
    next["required_read_features"] = serde_json::json!(["future.authority"]);
    fs::write(directory.path().join(FILE), serde_json::to_vec(&next).unwrap()).unwrap();
    drop(lock);
    let before = files(directory.path());
    assert!(LocalStore::open(directory.path()).is_err());
    assert_eq!(files(directory.path()), before);
}
#[test]
fn unsupported_inner_metadata_cannot_authorize_staging_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let mut next: serde_json::Value = serde_json::from_slice(profile::format::BASELINE).unwrap();
    next["required_write_features"] = serde_json::json!(["future.authority"]);
    let mut tx = store.metadata.begin_write(1).unwrap();
    tx.put(profile::format::KEY, &serde_json::to_vec(&next).unwrap()).unwrap();
    tx.commit().unwrap();
    drop(store);
    fs::create_dir(directory.path().join("staging")).unwrap();
    let retained = directory.path().join("staging/pending");
    fs::write(&retained, b"do not delete on failed inner admission").unwrap();
    assert!(LocalStore::open(directory.path()).is_err());
    assert_eq!(fs::read(&retained).unwrap(), b"do not delete on failed inner admission");
}
#[test]
fn fresh_publication_resumes_after_disk_failure_at_each_header_boundary() {
    for boundary in [Boundary::Write, Boundary::Rename, Boundary::DirectorySync] {
        let directory = tempfile::tempdir().unwrap();
        FAILURE.with(|failure| failure.set(Some(boundary)));
        let result = LocalStore::open(directory.path());
        FAILURE.with(|failure| failure.set(None));
        assert!(result.is_err());
        assert!(!directory.path().join(crate::paths::PROFILE_DB).exists());
        let store = LocalStore::open(directory.path()).unwrap();
        let account = store.account().id;
        drop(store);
        assert_eq!(LocalStore::open(directory.path()).unwrap().account().id, account);
    }
}
#[test]
fn pending_identity_and_its_format_resume_without_new_credentials() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("control")).unwrap();
    fs::write(directory.path().join(FILE), profile::format::BASELINE).unwrap();
    let pending = directory.path().join("control/profile-init.kvdb");
    let database = create_db(&pending, None, PROFILE_DATABASE_CACHE_BYTES).unwrap();
    let (account, seed) = generate_account().unwrap();
    profile::open(&database, true, 1, || Ok((account.clone(), seed, Id::from_bytes([7; 16]))))
        .unwrap();
    drop(database);
    let store = LocalStore::open(directory.path()).unwrap();
    assert_eq!(store.account(), &account);
    assert_eq!(store.account_seed().unwrap(), seed);
    assert!(!pending.exists());
    assert_eq!(
        store.metadata.begin_read().unwrap().get(profile::format::KEY).unwrap().unwrap().as_ref(),
        profile::format::BASELINE
    );
}

#[test]
fn local_scope_promotion_publishes_the_outer_barrier_before_metadata_and_resumes() {
    for boundary in
        [Boundary::Write, Boundary::Rename, Boundary::DirectorySync, Boundary::MetadataCommit]
    {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let store = LocalStore::open(path).unwrap();
        let account = store.account().clone();
        let volume = store.create_volume("Ordinary retained data", true).unwrap();
        let pending = store.pending_operations().unwrap();
        let descriptor =
            profile::format::promote_local_app_scopes(profile::format::BASELINE).unwrap();
        assert_eq!(fs::read(path.join(FILE)).unwrap(), profile::format::BASELINE);
        let guard = store.lock_profile().unwrap();
        FAILURE.with(|failure| failure.set(Some(boundary)));
        let result = enable_local_app_scopes(&store);
        FAILURE.with(|failure| failure.set(None));
        drop(guard);
        assert!(result.is_err());
        assert_eq!(
            store
                .metadata
                .begin_read()
                .unwrap()
                .get(profile::format::KEY)
                .unwrap()
                .unwrap()
                .as_ref(),
            profile::format::BASELINE
        );
        let header = fs::read(path.join(FILE)).unwrap();
        if matches!(boundary, Boundary::DirectorySync | Boundary::MetadataCommit) {
            assert_eq!(header, descriptor);
        } else {
            assert_eq!(header, profile::format::BASELINE);
        }
        drop(store);
        // New readers accept the header-promoted / database-baseline crash
        // state. Old readers reject its nonempty requirements before DB open.
        let reopened = LocalStore::open(path).unwrap();
        assert_eq!(reopened.account(), &account);
        assert_eq!(reopened.volume(volume.id).unwrap(), volume);
        assert_eq!(reopened.pending_operations().unwrap(), pending);
        {
            let _guard = reopened.lock_profile().unwrap();
            enable_local_app_scopes(&reopened).unwrap();
        }
        assert_eq!(fs::read(path.join(FILE)).unwrap(), descriptor);
        assert_eq!(
            reopened
                .metadata
                .begin_read()
                .unwrap()
                .get(profile::format::KEY)
                .unwrap()
                .unwrap()
                .as_ref(),
            descriptor
        );
        let before = files(path);
        {
            let _guard = reopened.lock_profile().unwrap();
            enable_local_app_scopes(&reopened).unwrap();
        }
        assert_eq!(files(path), before, "completed promotion makes no further writes");
        drop(reopened);
        let reopened = LocalStore::open(path).unwrap();
        assert_eq!(reopened.account(), &account);
        assert_eq!(reopened.volume(volume.id).unwrap(), volume);
        assert_eq!(reopened.pending_operations().unwrap(), pending);
    }
}

#[test]
fn incompatible_inner_requirements_cannot_change_the_outer_barrier() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let mut descriptor: serde_json::Value =
        serde_json::from_slice(profile::format::BASELINE).unwrap();
    descriptor["required_write_features"] = serde_json::json!(["future.authority"]);
    let mut tx = store.metadata.begin_write(1).unwrap();
    tx.put(profile::format::KEY, &serde_json::to_vec(&descriptor).unwrap()).unwrap();
    tx.commit().unwrap();
    let before = files(directory.path());
    let _guard = store.lock_profile().unwrap();
    assert!(enable_local_app_scopes(&store).is_err());
    assert_eq!(files(directory.path()), before);
}
