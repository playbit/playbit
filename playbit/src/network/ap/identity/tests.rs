#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
#[test]
fn authenticated_connections_require_a_complete_preissued_identity() {
    let mut identity = ClientIdentity::new("server@example.com", "Hosted instance");
    assert!(matches!(
        identity.validate_authenticated(),
        Err(ClientError::MissingIdentityField("auth token"))
    ));
    identity.auth_token = b"token".to_vec();
    assert!(matches!(
        identity.validate_authenticated(),
        Err(ClientError::MissingIdentityField("instance ID"))
    ));
    identity.instance_id = b"hosted-instance".to_vec();
    identity.private_key_der.clear();
    assert!(matches!(
        identity.validate_authenticated(),
        Err(ClientError::MissingIdentityField("private key"))
    ));
    identity.signing_key().unwrap();
    identity.ca_certificate = b"root".to_vec();
    identity.instance_certificate = b"instance".to_vec();
    identity.validate_authenticated().unwrap();
}

#[test]
fn preissued_identity_verifies_its_ed25519_spki() {
    let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let identity = ClientIdentity::from_preissued(
        "Hosted instance",
        b"instance".to_vec(),
        b"token".to_vec(),
        key.serialize_der(),
        &key.subject_public_key_info(),
        b"root".to_vec(),
        b"certificate".to_vec(),
    )
    .unwrap();
    assert!(identity.email.is_empty());
    assert_eq!(identity.instance_name, "Hosted instance");

    let mut wrong_public = key.subject_public_key_info();
    *wrong_public.last_mut().unwrap() ^= 1;
    assert!(matches!(
        ClientIdentity::from_preissued(
            "Hosted instance",
            b"instance".to_vec(),
            b"token".to_vec(),
            key.serialize_der(),
            &wrong_public,
            b"root".to_vec(),
            b"certificate".to_vec(),
        ),
        Err(ClientError::InvalidPreissuedIdentity)
    ));
}

#[test]
fn imported_seed_survives_registration_retry_and_restart() {
    let mut identity = ClientIdentity::from_seed("a@example.invalid", "Mac", [7; 32]).unwrap();
    let first = identity.certificate_request().unwrap();
    let encoded = serde_json::to_vec(&identity).unwrap();
    let mut loaded: ClientIdentity = serde_json::from_slice(&encoded).unwrap();
    let second = loaded.certificate_request().unwrap();
    assert_eq!(first.der(), second.der());
    assert_eq!(loaded.signing_key().unwrap().public_key_raw().len(), 32);
    assert!(loaded.account_id().is_err());
    loaded.auth_token = vec![0; 72];
    loaded.auth_token[..16].fill(7);
    assert_eq!(loaded.account_id().unwrap(), [7; 16]);
}

#[test]
fn stored_identity_requires_the_complete_current_shape() {
    let identity = ClientIdentity::new("person@example.invalid", "Mac");
    let mut encoded = serde_json::to_value(&identity).unwrap();
    encoded["unknown_credential"] = serde_json::json!("unexpected");
    assert!(serde_json::from_value::<ClientIdentity>(encoded.clone()).is_err());
    encoded.as_object_mut().unwrap().remove("unknown_credential");
    encoded.as_object_mut().unwrap().remove("private_key_der");
    assert!(serde_json::from_value::<ClientIdentity>(encoded).is_err());
    assert!(serde_json::from_str::<ClientIdentity>("{}").is_err());
}

#[test]
fn public_key_inspection_rejects_missing_material_without_generating_a_key() {
    use ring::signature::KeyPair as _;
    for seed in [[0; 32], [9; 32], [255; 32]] {
        let identity = ClientIdentity::from_seed("device@example.invalid", "Device", seed).unwrap();
        let signing = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        assert_eq!(identity.public_key().unwrap(), signing.public_key().as_ref());
    }
    let mut missing = ClientIdentity::default();
    assert!(missing.public_key().is_err());
    assert!(!missing.has_private_key());
    missing.private_key_der = vec![0; 48];
    assert!(missing.public_key().is_err());
    assert_eq!(missing.private_key_der, vec![0; 48]);
}
