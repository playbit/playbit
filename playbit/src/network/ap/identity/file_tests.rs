#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::fs;

fn issued_identity() -> ClientIdentity {
    let mut identity = ClientIdentity::new("person@example.com", "My Mac");
    identity.auth_token = vec![1; 72];
    let hash = blake3::hash(&identity.public_key().unwrap());
    identity.auth_token[..16].copy_from_slice(&hash.as_bytes()[..16]);
    identity.instance_id = crate::store::Id::from_bytes([9; 16]).to_string().into_bytes();
    identity.ca_certificate = b"root certificate".to_vec();
    identity.instance_certificate = b"instance certificate".to_vec();
    identity
}

#[test]
fn profile_identity_contains_plain_instance_credentials_and_readable_ids() {
    // Files alone reproduce the identity, with no Keychain or encryption dependency.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("instance.json");
    let identity = issued_identity();
    identity.save(&path).unwrap();
    let loaded = ClientIdentity::load(&path).unwrap();
    assert_eq!(serde_json::to_value(&loaded).unwrap(), serde_json::to_value(&identity).unwrap());
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(metadata["instance_id"], crate::store::Id::from_bytes([9; 16]).to_string());
    assert_eq!(
        STANDARD.decode(metadata["auth_token"].as_str().unwrap()).unwrap(),
        identity.auth_token
    );
    assert_eq!(
        STANDARD.decode(metadata["private_key_der"].as_str().unwrap()).unwrap(),
        identity.private_key_der
    );
    assert!(
        metadata["ca_certificate"].as_str().unwrap().starts_with("-----BEGIN CERTIFICATE-----\n")
    );
    assert!(metadata.get("account_key").is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o077, 0);
    }
    let copied = directory.path().join("copied.json");
    fs::copy(&path, &copied).unwrap();
    assert_eq!(ClientIdentity::load(&copied).unwrap().private_key_der, identity.private_key_der);
}

#[cfg(unix)]
#[test]
fn identity_loading_rejects_permissive_files_and_symlinks() {
    // Existing owner-only file boundaries remain enforced despite trusted storage.
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("instance.json");
    fs::write(&path, b"{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(ClientIdentity::load(&path), Err(ClientError::InsecureIdentityFile)));
    fs::remove_file(&path).unwrap();
    symlink(directory.path().join("absent"), &path).unwrap();
    assert!(matches!(ClientIdentity::load(&path), Err(ClientError::InsecureIdentityFile)));
}

#[test]
fn failed_identity_replacement_preserves_target_and_removes_temporary_file() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("identity.json");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), b"unrelated retained data").unwrap();
    let identity = ClientIdentity::new("fixture@trace.invalid", "Isolated Mac");
    assert!(identity.save(&target).is_err());
    assert_eq!(fs::read(target.join("keep")).unwrap(), b"unrelated retained data");
    let entries: Vec<_> =
        fs::read_dir(directory.path()).unwrap().map(|entry| entry.unwrap().path()).collect();
    assert_eq!(entries, [target], "failed atomic replacement leaves no credential temporary file");
}

#[test]
fn invalid_identity_fields_and_failed_publication_never_replace_saved_credentials() {
    // Malformed fields fail before atomic file replacement; interrupted temp files are ignored.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("instance.json");
    let mut identity = issued_identity();
    identity.save(&path).unwrap();
    let saved = fs::read(&path).unwrap();
    identity.instance_id = b"invalid".to_vec();
    assert!(identity.save(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), saved);
    fs::write(path.with_extension("interrupted.tmp"), b"partial").unwrap();
    assert!(ClientIdentity::load(&path).is_ok());
    let metadata: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    for field in [
        "instance_id",
        "account_id",
        "device_id",
        "ca_certificate",
        "private_key_der",
        "auth_token",
    ] {
        let mut invalid = metadata.clone();
        invalid[field] = serde_json::json!("invalid!");
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(ClientIdentity::load(&path).is_err(), "{field}");
    }
    fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
    assert!(ClientIdentity::load(&path).is_err());
}

#[test]
fn independent_profiles_keep_distinct_credentials_for_the_same_account() {
    // Renewing one installation cannot replace another installation's key or token.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_path = first.path().join("instance.json");
    let second_path = second.path().join("instance.json");
    let mut first_identity = issued_identity();
    let mut second_identity = issued_identity();
    second_identity.auth_token[..16].copy_from_slice(&first_identity.auth_token[..16]);
    second_identity.auth_token[16..].fill(2);
    first_identity.save(&first_path).unwrap();
    second_identity.save(&second_path).unwrap();
    first_identity.auth_token[16..].fill(3);
    first_identity.save(&first_path).unwrap();
    assert_eq!(ClientIdentity::load(&first_path).unwrap().auth_token, first_identity.auth_token);
    let second_loaded = ClientIdentity::load(&second_path).unwrap();
    assert_eq!(second_loaded.auth_token, second_identity.auth_token);
    assert_eq!(second_loaded.private_key_der, second_identity.private_key_der);
}
