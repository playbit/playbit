//! Reply decisions use public fixture credentials, never a live identity.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture setup and decisions.")]
use super::*;
fn identity() -> ClientIdentity {
    let mut identity = ClientIdentity::default();
    identity.auth_token = b"saved-token".to_vec();
    identity.instance_id = b"saved-instance".to_vec();
    identity.ca_certificate = b"saved-ca".to_vec();
    identity.instance_certificate = b"saved-certificate".to_vec();
    identity
}
#[test]
fn hello_status_matrix_changes_identity_only_after_admission() {
    // Exercise every wire status, both host policies and saved/missing instance.
    for status in 0..=u8::MAX {
        for allow in [false, true] {
            for saved in [false, true] {
                let mut identity = identity();
                if !saved {
                    identity.instance_id.clear();
                }
                let before = serde_json::to_vec(&identity).unwrap();
                let response =
                    Frame::new(frame_type::HELLO, status, [], [], b"renewed-token").unwrap();
                let result = accept_hello(&mut identity, response, allow);
                let expected = if status == hello::VALID && saved {
                    Some(Hello::Authenticated)
                } else if (hello::MISSING..=hello::REVOKED).contains(&status) && allow {
                    Some(Hello::RegistrationRequired)
                } else {
                    None
                };
                assert_eq!(result.as_ref().ok().copied(), expected);

                match result {
                    Ok(Hello::Authenticated) => {
                        assert_eq!(status, hello::VALID);
                        assert!(saved);
                        assert_eq!(identity.auth_token, b"renewed-token");
                        assert_eq!(identity.instance_id, b"saved-instance");
                    }
                    Ok(Hello::RegistrationRequired) => {
                        assert!(allow);
                        assert!((hello::MISSING..=hello::REVOKED).contains(&status));
                        assert!(identity.auth_token.is_empty());
                        assert!(identity.instance_id.is_empty());
                    }
                    Err(_) => assert_eq!(serde_json::to_vec(&identity).unwrap(), before),
                }
                assert_eq!(identity.ca_certificate, b"saved-ca");
                assert_eq!(identity.instance_certificate, b"saved-certificate");
            }
        }
    }
}
#[test]
fn invalid_hello_envelopes_preserve_identity_and_empty_renewal_keeps_token() {
    // Names/type cannot smuggle credential mutation; a valid empty reply retains
    // the saved token exactly as the native protocol previously allowed.
    for (kind, source, destination) in
        [(1, vec![], vec![]), (0, vec![1], vec![]), (0, vec![], vec![1])]
    {
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let response = Frame::new(kind, hello::VALID, source, destination, b"renewed").unwrap();
        assert!(accept_hello(&mut identity, response, true).is_err());
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
    let mut identity = identity();
    assert_eq!(hello_request(&identity).unwrap().body, identity.auth_token);
    assert_eq!(
        accept_hello(&mut identity, Frame::new(0, 0, [], [], []).unwrap(), false).unwrap(),
        Hello::Authenticated
    );
    assert_eq!(identity.auth_token, b"saved-token");
    identity.auth_token.clear();
    assert!(matches!(hello_request(&identity), Err(ClientError::AuthenticationRequired)));
}
fn issued(kind: u8, status: u8) -> Frame {
    let instance = crate::store::Id::from_bytes([0xaa; 16]).to_string();
    let mut body = vec![72, instance.len() as u8, 2, 0];
    body.extend_from_slice(&[1; 72]);
    body.extend_from_slice(instance.as_bytes());
    body.extend_from_slice(b"cacert");
    Frame::new(kind, status, [], [], body).unwrap()
}
#[test]
fn registration_rejection_never_adopts_partial_fields() {
    // Status failures, bad ranges and missing credentials leave all saved fields.
    for status in 0..=u8::MAX {
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let result = accept_registration(&mut identity, &issued(frame_type::REGISTER, status));
        if status == registration_status::SUCCESS {
            result.unwrap();
            assert_eq!(identity.auth_token, [1; 72]);
            assert_eq!(
                identity.instance_id,
                crate::store::Id::from_bytes([0xaa; 16]).to_string().as_bytes()
            );
            assert_eq!(identity.ca_certificate, b"ca");
            assert_eq!(identity.instance_certificate, b"cert");
        } else {
            assert!(result.is_err());
            assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
        }
    }
    for body in [vec![], vec![0; 4], vec![255; 4]] {
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let bad = Frame::new(1, registration_status::SUCCESS, [], [], body).unwrap();
        assert!(accept_registration(&mut identity, &bad).is_err());
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
}
#[test]
fn testing_hello_preserves_its_issued_shape_checks() {
    // Testing is explicit; ordinary tokens do not pass its credential envelope.
    let mut identity = identity();
    accept_testing_hello(&mut identity, issued(0, hello::VALID)).unwrap();
    assert_eq!(identity.auth_token, [1; 72]);
    for (index, length) in [(0, 71), (1, 31), (2, 0)] {
        let mut bad = issued(0, hello::VALID);
        bad.body[index] = length;
        let before = serde_json::to_vec(&identity).unwrap();
        assert!(accept_testing_hello(&mut identity, bad).is_err());
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
}
#[test]
fn registration_challenge_keeps_delivery_failure_explicit() {
    // Delivery-unconfirmed must not be interpreted as successful code delivery.
    assert!(matches!(
        registration_challenge(&Frame::new(1, 4, [], [], [0; 4]).unwrap()),
        Err(ClientError::DeliveryUnconfirmed)
    ));
    assert_eq!(registration_challenge(&Frame::new(1, 0, [], [], [0; 4]).unwrap()).unwrap(), None);
    let mut body = vec![0, 3, 0, 0];
    body.extend_from_slice(b"123");
    assert_eq!(
        registration_challenge(&Frame::new(1, 0, [], [], body).unwrap()).unwrap().as_deref(),
        Some("123")
    );
    assert!(registration_challenge(&Frame::new(1, 0, [], [], [0, 1, 0, 0, 255]).unwrap()).is_err());
}
