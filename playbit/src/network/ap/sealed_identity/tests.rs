#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
const KEY: [u8; 32] = [0x97; 32];
const CONTEXT: [u8; 16] = [0x48; 16];
fn identity() -> ClientIdentity {
    ClientIdentity::from_seed("test@example.invalid", "Test device", [0x19; 32]).unwrap()
}
#[test]
fn prepared_and_enrolled_identities_round_trip() {
    let mut identity = identity();
    for n in 0..2 {
        let encoded = seal(&identity, &KEY, [n; 12], &CONTEXT).unwrap();
        let reopened = open(&encoded, &KEY, &CONTEXT).unwrap();
        assert_eq!(serde_json::to_vec(&reopened).unwrap(), serde_json::to_vec(&identity).unwrap());
        assert!(reopened.has_private_key());
        assert!(!encoded.windows(identity.email.len()).any(|s| s == identity.email.as_bytes()));
        // Complete fixture fields, not an issued identity or authentication proof.
        identity.auth_token = vec![4; 72];
        identity.instance_id = vec![5; 16];
        identity.ca_certificate = vec![6; 1024];
        identity.instance_certificate = vec![7; 1024];
    }
}
#[test]
fn wrong_key_context_and_every_altered_byte_fail() {
    let encoded = seal(&identity(), &KEY, [3; 12], &CONTEXT).unwrap();
    assert!(open(&encoded, &[0; 32], &CONTEXT).is_err());
    assert!(open(&encoded, &KEY, &[0; 16]).is_err());
    for i in 0..encoded.len() {
        let mut changed = encoded.clone();
        changed[i] ^= 1;
        assert!(open(&changed, &KEY, &CONTEXT).is_err(), "accepted changed byte {i}");
    }
    for len in 0..encoded.len() {
        assert!(open(&encoded[..len], &KEY, &CONTEXT).is_err());
    }
    let mut trailing = encoded;
    trailing.push(0);
    assert!(open(&trailing, &KEY, &CONTEXT).is_err());
}
#[test]
fn bounded_serialization_admits_exact_limit_and_rejects_one_more_byte() {
    let mut identity = identity();
    let used = serde_json::to_vec(&identity).unwrap().len();
    identity.instance_name.push_str(&"a".repeat(MAX_PLAINTEXT_BYTES - used));
    let bytes = seal(&identity, &KEY, [4; 12], &CONTEXT).unwrap();
    assert_eq!(bytes.len(), MAX_ENVELOPE_BYTES);
    assert_eq!(open(&bytes, &KEY, &CONTEXT).unwrap().instance_name, identity.instance_name);
    identity.instance_name.push('a');
    assert!(seal(&identity, &KEY, [5; 12], &CONTEXT).is_err());
    assert!(open(&vec![0; MAX_ENVELOPE_BYTES + 1], &KEY, &CONTEXT).is_err());
}
#[test]
fn missing_key_is_not_implicitly_generated_and_bad_json_is_rejected() {
    assert!(seal(&ClientIdentity::default(), &KEY, [6; 12], &CONTEXT).is_err());
    for (n, plaintext) in [b"not json".as_slice(), b"{}".as_slice()].into_iter().enumerate() {
        let nonce = [10 + n as u8; 12];
        let mut bytes = nonce.to_vec();
        let mut body = plaintext.to_vec();
        cipher(&KEY)
            .unwrap()
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(associated_data(&CONTEXT)),
                &mut body,
            )
            .unwrap();
        bytes.extend(body);
        assert!(open(&bytes, &KEY, &CONTEXT).is_err());
    }
}
