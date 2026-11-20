//! Profile ownership ends after database teardown, even if another process
//! inherited a descriptor during fork before exec.
use super::*;

#[test]
fn retired_profile_releases_ownership_while_a_duplicate_survives() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let account = store.account().id;
    let duplicate = store._lock.0.try_clone().unwrap();
    assert!(matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_))));
    drop(store);
    let reopened = LocalStore::open(directory.path()).unwrap();
    assert_eq!(reopened.account().id, account);
    assert!(duplicate.metadata().unwrap().is_file());
    assert!(matches!(LocalStore::open(directory.path()), Err(Error::LocalContention(_))));
}
