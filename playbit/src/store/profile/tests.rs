#![allow(clippy::unwrap_used, reason = "Tests assert fixture setup and expected success.")]
use super::*;
use kvdb::{CreateOptions, storage::mem::MemStorage};

fn database() -> Db<MemStorage> {
    Db::create(MemStorage::new(), &CreateOptions::plain()).unwrap()
}
fn identity() -> (Account, [u8; 32], Id) {
    // Deterministic storage fixture; cryptographic generation belongs to the host.
    (
        Account { id: Id::from_bytes([7; 16]), public_key: "ab".repeat(32) },
        [9; 32],
        Id::from_bytes([10; 16]),
    )
}

#[test]
fn retained_release_candidate_profile_entries_open_without_rewriting() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/profile-rc1.json")).unwrap();
    let descriptor = fixture["descriptor"].as_str().unwrap().as_bytes();
    assert_eq!(descriptor, format::BASELINE);
    let account = fixture["account"].as_str().unwrap().as_bytes();
    let seed = [fixture["account_seed_byte"].as_u64().unwrap() as u8; 32];
    let installation = [fixture["installation_byte"].as_u64().unwrap() as u8; 16];
    let db = database();
    let mut tx = db.begin_write(1).unwrap();
    for (key, value) in [
        (format::KEY, descriptor),
        (b"account", account),
        (b"account-secret", &seed),
        (b"installation", &installation),
    ] {
        tx.put(key, value).unwrap();
    }
    tx.commit().unwrap();
    let entries = || {
        let tx = db.begin_read().unwrap();
        tx.iter_from(b"")
            .map(|entry| {
                let (key, value) = entry.unwrap();
                (key.to_vec(), value.to_vec())
            })
            .collect::<Vec<_>>()
    };
    let before = entries();
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let expected = Account {
        id: Id::from_bytes(
            blake3::hash(pair.public_key().as_ref()).as_bytes()[..16].try_into().unwrap(),
        ),
        public_key: pair.public_key().as_ref().iter().map(|byte| format!("{byte:02x}")).collect(),
    };
    assert_eq!(open(&db, false, 2, unexpected_generation).unwrap(), expected);
    assert_eq!(entries(), before);
    assert_eq!(super::seed(&db).unwrap(), seed);
    assert_eq!(super::installation(&db).unwrap().as_bytes(), &installation);
}
fn unexpected_generation() -> Result<(Account, [u8; 32], Id)> {
    panic!("retained profile must never generate replacement credentials")
}
fn put(db: &Db<MemStorage>, key: &[u8], value: Option<&[u8]>) {
    let mut tx = db.begin_write(1).unwrap();
    if let Some(value) = value {
        tx.put(key, value).unwrap();
    } else {
        tx.del(key).unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn create_reload_and_rebound_account_preserve_credentials() {
    // Reopen with both freshness flags, then rebind only the account ID.
    let db = database();
    let (account, expected_seed, expected_installation) = identity();
    assert_eq!(open(&db, true, 1, || Ok(identity())).unwrap(), account);
    assert_eq!(seed(&db).unwrap(), expected_seed);
    assert_eq!(installation(&db).unwrap(), expected_installation);
    for fresh in [true, false] {
        assert_eq!(open(&db, fresh, 2, unexpected_generation).unwrap(), account);
    }
    let rebound = Account { id: Id::from_bytes([8; 16]), ..account };
    put(&db, b"account", Some(&encode(&rebound).unwrap()));
    assert_eq!(open(&db, false, 2, unexpected_generation).unwrap(), rebound);
    assert_eq!(seed(&db).unwrap(), expected_seed);
    assert_eq!(installation(&db).unwrap(), expected_installation);
}

#[test]
fn missing_identity_is_never_repaired() {
    // Retained emptiness and partial identity must not become new accounts.
    let db = database();
    assert!(matches!(open(&db, false, 1, unexpected_generation), Err(Error::Corrupt(_))));
    for missing in [b"account".as_slice(), b"account-secret", b"installation"] {
        let db = database();
        open(&db, true, 1, || Ok(identity())).unwrap();
        put(&db, missing, None);
        for fresh in [false, true] {
            let error = open(&db, fresh, 2, unexpected_generation).unwrap_err();
            assert!(matches!(error, Error::Corrupt(_)));
            let expected = if missing == b"account" {
                "account record is missing"
            } else if missing == b"installation" {
                "installation identity is missing"
            } else {
                "account signing seed is missing"
            };
            assert!(error.to_string().contains(expected));
        }
        assert!(db.begin_read().unwrap().get(missing).unwrap().is_none());
    }
}

#[test]
fn nonempty_profile_and_changes_during_generation_reject_creation() {
    // Preparation grants no write authority: recheck the writer after callback work.
    let db = database();
    put(&db, b"retained", Some(b"data"));
    assert!(matches!(open(&db, true, 1, unexpected_generation), Err(Error::Corrupt(_))));
    let db = database();
    let result = open(&db, true, 1, || {
        put(&db, b"retained", Some(b"data"));
        Ok(identity())
    });
    assert!(matches!(result, Err(Error::Corrupt(_))));
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.get(b"retained").unwrap().unwrap().as_ref(), b"data");
    assert!(tx.get(b"account").unwrap().is_none());
    assert!(tx.get(b"account-secret").unwrap().is_none());
}

#[test]
fn malformed_and_oversized_records_fail_without_mutation() {
    // Exercise both record bounds and canonical public-key validation on load.
    for (key, bad) in [
        (b"account".as_slice(), vec![b' '; MAX_ACCOUNT_BYTES + 1]),
        (b"account", b"{}".to_vec()),
        (b"account", encode(&Account { public_key: "AB".repeat(32), ..identity().0 }).unwrap()),
        (b"account", encode(&Account { public_key: "g".repeat(64), ..identity().0 }).unwrap()),
        (b"account", encode(&Account { public_key: "a".repeat(63), ..identity().0 }).unwrap()),
        (b"account-secret", vec![0; 31]),
        (b"account-secret", vec![0; 4096]),
        (b"installation", vec![0; 15]),
        (b"installation", vec![0; 4096]),
    ] {
        let db = database();
        open(&db, true, 1, || Ok(identity())).unwrap();
        put(&db, key, Some(&bad));
        assert!(matches!(open(&db, false, 2, unexpected_generation), Err(Error::Corrupt(_))));
        assert_eq!(db.begin_read().unwrap().get(key).unwrap().unwrap().as_ref(), bad);
    }
}

#[test]
fn generator_failure_or_invalid_key_leaves_database_empty() {
    // Host errors must occur before publication of either identity record.
    let db = database();
    assert!(matches!(open(&db, true, 1, || Err(Error::LimitExceeded)), Err(Error::LimitExceeded)));
    let (mut account, seed, installation) = identity();
    account.public_key = "bad key".into();
    assert!(matches!(
        open(&db, true, 1, || Ok((account, seed, installation))),
        Err(Error::Corrupt(_))
    ));
    assert!(db.begin_read().unwrap().next_key(b"", false).unwrap().is_none());
}
