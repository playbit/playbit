use super::*;

fn enrolled_profile() -> (tempfile::TempDir, Engine) {
    let directory = tempfile::tempdir().expect("profile");
    let store = LocalStore::open(directory.path()).expect("local store");
    let account = store.account().id;
    let mut identity = crate::network::ap::ClientIdentity::from_seed(
        "person@example.com",
        "Test installation",
        store.account_seed().expect("device seed"),
    )
    .expect("identity");
    identity.auth_token = vec![1; 72];
    identity.auth_token[..16].copy_from_slice(account.as_bytes());
    identity.instance_id = Id::from_bytes([5; 16]).to_string().into_bytes();
    identity.save(&directory.path().join("instance.json")).expect("identity file");
    crate::credentials::remember_account(directory.path(), account, "person@example.com", &[9; 16])
        .expect("account key");
    drop(store);
    let engine = Engine::open(directory.path()).expect("reopened engine");
    (directory, engine)
}

#[test]
fn saved_account_lookup_uses_the_authenticated_account_without_signing_in() {
    let (directory, mut engine) = enrolled_profile();
    let account = engine.services.store.account().id;
    crate::credentials::remember_account(
        directory.path(),
        Id::from_bytes([4; 16]),
        "other@example.com",
        &[8; 16],
    )
    .expect("other account");
    let outcome = enrollment::Action::SavedAccount
        .execute(engine.remote.as_mut().expect("remote"), &engine.services.store)
        .expect("exact lookup");
    assert!(
        matches!(outcome, enrollment::Outcome::SavedAccount(Some(value)) if value.email == "person@example.com" && value.account_key == crate::store::account_key::format(&[9; 16]))
    );
    assert_eq!(engine.services.store.account().id, account);
    assert!(engine.services.store.recovery_state().expect("state").is_none());
    let remote = engine.remote.as_mut().expect("remote");
    remote.require_replacement();
    let error = enrollment::Action::SavedAccount
        .execute(remote, &engine.services.store)
        .err()
        .expect("replacement decision must precede recovery lookup");
    assert!(error.to_string().contains("Confirm replacing existing data"));
}

#[test]
fn disconnect_rejects_a_late_saved_account_lookup() {
    let (_directory, mut engine) = enrolled_profile();
    engine.disconnect_network().expect("disconnect");
    engine.network_request_waiter = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    engine
        .complete_enrollment(
            Ok(NetworkResult::Enrollment(enrollment::Outcome::SavedAccount(Some(
                reply::SavedAccount {
                    email: "person@example.com".into(),
                    account_key: crate::store::account_key::format(&[9; 16]),
                },
            )))),
            true,
            true,
        )
        .expect("late lookup");
    assert!(engine.network_response.take().expect("lookup reply").is_err());
    assert!(!engine.bootstrap_pending);
}

#[test]
fn failed_backup_keeps_local_credentials_and_requires_explicit_retry() {
    use crate::store::{AccountKeyBackup, RecoveryState};
    let (directory, mut engine) = enrolled_profile();
    let store = &engine.services.store;
    let account = store.account().id;
    store
        .save_recovery_state(&RecoveryState {
            secret: [9; 16],
            acknowledged: true,
            revision: 1,
            synchronized_digest: None,
            pending: None,
        })
        .expect("local commit");
    let remote = engine.remote.as_mut().expect("remote");
    assert!(remote.account_backup_pending(store).expect("pending"));
    crate::credentials::keychain::testing::fail(&account.to_string(), Some(true));
    let outcome = enrollment::Action::Backup { retry: false }.execute(remote, store);
    crate::credentials::keychain::testing::fail(&account.to_string(), None);
    assert!(matches!(
        outcome.expect("local readiness survives denial"),
        enrollment::Outcome::Backup(AccountKeyBackup::Failed)
    ));
    assert_eq!(store.recovery_state().expect("state").expect("key").secret, [9; 16]);
    assert!(!remote.account_backup_pending(store).expect("no polling"));
    assert!(matches!(
        enrollment::Action::Backup { retry: true }.execute(remote, store).expect("explicit retry"),
        enrollment::Outcome::Backup(AccountKeyBackup::Saved)
    ));
    drop(engine);
    let reopened = LocalStore::open(directory.path()).expect("offline reopen");
    assert_eq!(reopened.recovery_state().expect("state").expect("key").secret, [9; 16]);
    assert_eq!(reopened.account_key_backup().expect("backup"), AccountKeyBackup::Saved);
}
