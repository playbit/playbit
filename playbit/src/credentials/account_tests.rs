use super::*;

#[test]
fn discovery_requires_one_valid_account_and_never_crosses_profiles() {
    // A new instance can identify the account without a token or instance.json.
    let first = tempfile::tempdir().expect("first profile");
    let second = tempfile::tempdir().expect("second profile");
    let account = crate::store::Id::from_bytes([7; 16]);
    assert!(find_account(first.path()).expect("empty Keychain").is_none());
    remember_account(first.path(), account, "person@example.com", &[9; 16]).expect("save");
    let saved = find_account(first.path()).expect("lookup").expect("one account");
    assert_eq!(saved.account, account);
    assert_eq!(saved.email, "person@example.com");
    assert_eq!(saved.secret, [9; 16]);
    assert!(find_account(second.path()).expect("other profile").is_none());
    assert!(!first.path().join("instance.json").exists());
    remember_account(
        first.path(),
        crate::store::Id::from_bytes([8; 16]),
        "other@example.com",
        &[8; 16],
    )
    .expect("second account");
    assert!(find_account(first.path()).expect("ambiguous accounts").is_none());
    assert!(
        AccountCredential::parse("not base58!", "person@example.com".into(), &[0; 16]).is_err()
    );
    assert!(
        AccountCredential::parse(&account.to_string(), "person@example.com".into(), &[0; 15])
            .is_err()
    );
    assert!(AccountCredential::parse(&account.to_string(), String::new(), &[0; 16]).is_ok());
}

#[test]
fn saved_keys_reject_conflicts_and_exact_lookup_ignores_other_accounts() {
    let profile = tempfile::tempdir().expect("profile");
    let account = Id::from_bytes([4; 16]);
    remember_account(profile.path(), account, "", &[5; 16]).expect("confirmed without email");
    remember_account(profile.path(), account, "person@example.com", &[5; 16])
        .expect("metadata update");
    assert!(remember_account(profile.path(), account, "person@example.com", &[6; 16]).is_err());
    let found = find_account_for(profile.path(), account, "person@example.com")
        .expect("lookup")
        .expect("account");
    assert_eq!(found.secret, [5; 16]);
    assert!(
        find_account_for(profile.path(), Id::from_bytes([3; 16]), "")
            .expect("exact account")
            .is_none()
    );
}
