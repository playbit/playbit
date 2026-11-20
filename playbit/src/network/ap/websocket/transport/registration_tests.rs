use super::*;
use crate::network::ap::{ClientError, ClientIdentity, Frame, frame_type, key_auth};
fn identity() -> ClientIdentity {
    ClientIdentity::from_seed("enroll@example.invalid", "Device", [23; 32]).unwrap()
}
fn data(socket: &Socket, bytes: &[u8]) {
    let mut packet = vec![1, 1, 0, 0, 0];
    packet.extend_from_slice(bytes);
    incoming(socket, Message::Binary(packet.into()));
}
fn response(status: u8) -> Vec<u8> {
    let mut body = vec![72, 32, 2, 0];
    body.extend([8; 72]);
    body.extend([9; 32]);
    body.extend([10; 2]);
    body.extend([11; 2]);
    auth_wire(Frame::new(frame_type::REGISTER, status, [], [], body).unwrap())
}
#[test]
fn delivery_confirmation_releases_connection_without_authenticating() {
    // Production returns no code. Development delivery can explicitly include
    // one; neither form mutates identity or waits for the remote stream's FIN.
    for code in ["", "123456"] {
        let socket = wire();
        let identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let mut request = RegistrationRequest::new(raw(socket.clone()), &identity).unwrap();
        assert!(request.advance().unwrap().is_none());
        let mut server = outgoing(&socket);
        server.read().unwrap(); // OPEN.
        let Message::Binary(packet) = server.read().unwrap() else { panic!("DATA") };
        let frame = Frame::decode(&packet[5..]).unwrap();
        let fields = key_auth::decode(&frame.body);
        // key_auth requires a CSR; a delivery-only REGISTER intentionally has none.
        assert!(fields.is_err());
        assert_eq!(frame.frame_type, frame_type::REGISTER);
        assert_eq!(&frame.body[..4], &[22, 0, 6, 0]);
        let mut body = vec![0, code.len() as u8, 0, 0];
        body.extend(code.as_bytes());
        let reply = auth_wire(Frame::new(frame_type::REGISTER, 0, [], [], body).unwrap());
        data(&socket, &reply);
        let mut completed = None;
        for _ in 0..4 {
            completed = request.advance().unwrap();
            if completed.is_some() {
                break;
            }
        }
        assert_eq!(
            completed.unwrap().automatic_challenge,
            (!code.is_empty()).then(|| code.to_string())
        );
        assert_eq!(Rc::strong_count(&socket.0), 1);
        assert!(request.advance().is_err());
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
}
#[test]
fn delivery_failure_is_terminal_and_cancellation_drops_transport() {
    // Declared delivery failures and bad envelopes retain no
    // transport. A dropped request must also cancel a stalled peer.
    for (kind, status, body) in
        [(1, 3, vec![0; 4]), (1, 4, vec![0; 4]), (1, 2, vec![0; 4]), (0, 0, vec![])]
    {
        let socket = wire();
        let identity = identity();
        let mut request = RegistrationRequest::new(raw(socket.clone()), &identity).unwrap();
        request.advance().unwrap();
        data(&socket, &auth_wire(Frame::new(kind, status, [], [], body).unwrap()));
        let mut failed = false;
        for _ in 0..4 {
            match request.advance() {
                Err(_) => {
                    failed = true;
                    break;
                }
                Ok(None) => {}
                _ => panic!("delivery accepted"),
            }
        }
        assert!(failed);
        assert_eq!(Rc::strong_count(&socket.0), 1);
    }
    let socket = wire();
    let mut request = RegistrationRequest::new(raw(socket.clone()), &identity()).unwrap();
    request.advance().unwrap();
    drop(request);
    assert_eq!(Rc::strong_count(&socket.0), 1);
}
#[test]
fn code_confirmation_rejects_without_fin_and_preserves_the_prepared_key() {
    // A wrong code must be retryable with the exact same key and credentials.
    for status in [0, 2, 3, 255] {
        let socket = wire();
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let mut exchange =
            Authentication::register(raw(socket.clone()), &mut identity, "123456").unwrap();
        exchange.advance().unwrap();
        data(&socket, &response(status));
        let mut failed = false;
        for _ in 0..4 {
            match exchange.advance() {
                Err(AuthenticationError::Identity(_)) => {
                    failed = true;
                    break;
                }
                Ok(None) => {}
                _ => panic!("code accepted"),
            }
        }
        assert!(failed);
        assert_eq!(Rc::strong_count(&socket.0), 1);
        drop(exchange);
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
}
#[test]
fn credentials_are_adopted_only_after_fin_and_connection_remains_usable() {
    // Capture the real CSR/code request; delay FIN and also test valid-prefix
    // plus trailing junk. Neither a partial reply nor junk may adopt credentials.
    for junk in [false, true] {
        let socket = wire();
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let mut exchange =
            Authentication::register(raw(socket.clone()), &mut identity, "123456").unwrap();
        exchange.advance().unwrap();
        let mut server = outgoing(&socket);
        server.read().unwrap();
        let Message::Binary(packet) = server.read().unwrap() else { panic!("DATA") };
        let frame = Frame::decode(&packet[5..]).unwrap();
        assert_eq!(frame.body[3], 6);
        assert!(frame.body.len() > 100); // Includes the signed CSR.
        for byte in response(1) {
            data(&socket, &[byte]);
            assert!(exchange.advance().unwrap().is_none());
        }
        assert!(exchange.token().is_empty());
        if junk {
            data(&socket, &[1]);
        }
        incoming(&socket, Message::Binary(vec![2, 1, 0, 0, 0].into()));
        let result = exchange.advance();
        if junk {
            assert!(result.is_err());
            drop(exchange);
            assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
        } else {
            let mut connection = result.unwrap().unwrap();
            drop(exchange);
            assert_eq!(identity.auth_token, [8; 72]);
            assert_eq!(identity.instance_id, [9; 32]);
            assert_eq!(identity.ca_certificate, [10; 2]);
            assert_eq!(identity.instance_certificate, [11; 2]);
            assert_eq!(connection.open().unwrap(), 2);
        }
    }
}
#[test]
fn missing_keys_and_oversize_fields_are_rejected_before_sending() {
    // Enrollment cannot silently generate an unpersisted replacement key.
    let mut missing = ClientIdentity::default();
    missing.email = "a@b.co".into();
    missing.instance_name = "Device".into();
    let before = serde_json::to_vec(&missing).unwrap();
    let socket = wire();
    assert!(matches!(
        Authentication::register(raw(socket.clone()), &mut missing, "123456"),
        Err(AuthenticationError::Identity(ClientError::MissingIdentityField("private key")))
    ));
    assert!(RegistrationRequest::new(raw(socket.clone()), &missing).is_err());
    assert_eq!(serde_json::to_vec(&missing).unwrap(), before);
    assert!(socket.0.borrow().outgoing.is_empty());
    let mut identity = identity();
    identity.email = "a".repeat(4096);
    assert!(RegistrationRequest::new(raw(socket.clone()), &identity).is_err());
    assert!(Authentication::register(raw(socket.clone()), &mut identity, "123456").is_err());
    identity.email = "a@b.co".into();
    for code in ["".to_string(), "a".repeat(256)] {
        assert!(Authentication::register(raw(socket.clone()), &mut identity, &code).is_err());
    }
    assert_eq!(Rc::strong_count(&socket.0), 1);
}

#[test]
fn delivery_admission_bounds_requests_and_rejects_oversized_or_truncated_replies() {
    // Check the exact operation bound independently of the service's email
    // policy; rejecting a reply header must not wait for its advertised payload.
    let socket = wire();
    let mut identity = identity();
    identity.email = "a".repeat(4096 - 4 - identity.instance_name.len());
    let request = RegistrationRequest::new(raw(socket.clone()), &identity).unwrap();
    drop(request);
    identity.email.push('a');
    assert!(RegistrationRequest::new(raw(socket.clone()), &identity).is_err());
    identity.email = "enroll@example.invalid".into();
    for oversized in [false, true] {
        let mut request = RegistrationRequest::new(raw(socket.clone()), &identity).unwrap();
        request.advance().unwrap();
        if oversized {
            data(&socket, &[0x11, 0x10, 0, 0, 1, 0, 0, 0]); // 4097-byte body.
        } else {
            data(&socket, &[20, 0, 0]);
            incoming(&socket, Message::Binary(vec![2, 1, 0, 0, 0].into()));
        }
        let mut failed = false;
        for _ in 0..4 {
            match request.advance() {
                Err(AuthenticationError::Frame(_)) => {
                    failed = true;
                    break;
                }
                Ok(None) => {}
                _ => panic!("invalid frame accepted"),
            }
        }
        assert!(failed);
        assert_eq!(Rc::strong_count(&socket.0), 1);
    }
}

fn auth_wire(frame: Frame) -> Vec<u8> {
    crate::network::ap::timing::auth::encode(frame, &Default::default()).unwrap().encode().unwrap()
}
