//! Explicit offline signing fixture for the packaged native correctness journey.
//! This does not exercise AP enrollment or production account recovery.
#![cfg(all(
    not(target_arch = "wasm32"),
    feature = "local-store",
    feature = "ap-identity",
    feature = "test-support"
))]

use playbit::{package::Package, store::LocalStore};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
};

#[test]
#[ignore = "Invoked explicitly by test-packaged-workflow.py with its fresh private profile path."]
fn prepare_offline_signer_profile() {
    let path = std::path::PathBuf::from(
        std::env::var_os("PLAYBIT_PACKAGED_WORKFLOW_PROFILE")
            .expect("packaged workflow supplies its new isolated profile path"),
    );
    assert!(path.is_absolute(), "fixture profile path must be absolute");
    let parent = path.parent().expect("fixture profile has a private parent");
    assert_eq!(parent.canonicalize().expect("fixture parent exists"), parent);
    let owner = tempfile::tempdir().expect("isolated ownership probe");
    let metadata = fs::metadata(parent).expect("fixture parent metadata");
    assert!(metadata.is_dir());
    assert_eq!(metadata.uid(), fs::metadata(owner.path()).expect("probe metadata").uid());
    assert_eq!(metadata.mode() & 0o077, 0, "fixture parent must be private");
    // Atomic creation rejects an existing profile, file or symlink, even if empty.
    fs::DirBuilder::new().mode(0o700).create(&path).expect("fixture profile must not exist");
    let store = LocalStore::open(&path).expect("new isolated profile opens");
    store.set_work_offline(true).expect("fixture remains explicitly offline");
    let package = Package::new(
        "offline-fixture".into(),
        "Offline signer fixture".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::new(),
    )
    .expect("minimal fixture package is structurally valid");
    let package = playbit::package::signature::testing::signed(&store, package)
        .expect("explicit fixture issuer signs and authenticates this profile's real signer");
    let build = package.id().expect("signed fixture has an exact build ID");
    let builds = path.join("builds");
    fs::create_dir_all(&builds).expect("fixture build cache exists");
    let package_path = builds.join(&build);
    fs::write(&package_path, package.to_bytes().expect("signed fixture encodes"))
        .expect("retain actual fixture package bytes");
    assert!(store.sessions().expect("fixture session catalog").is_empty());
    assert!(store.volumes().expect("fixture volume catalog").is_empty());
    assert!(!path.join("instance.json").exists(), "fixture does not claim AP enrollment");
    drop(store);
    let reopened = LocalStore::open(&path).expect("fixture signer survives profile reopen");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("fixture has a wall clock")
        .as_secs();
    let retained = reopened
        .authenticate_package(
            Package::open(&package_path).expect("retained signed fixture loads"),
            now,
        )
        .expect("production package admission accepts the retained fixture authority offline");
    assert_eq!(retained.id().expect("retained exact build"), build);
}
