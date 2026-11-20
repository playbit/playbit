use super::*;
use crate::network::ap::{ClientIdentity, Frame, frame_type};

fn identity() -> ClientIdentity {
    let mut identity = ClientIdentity::default();
    identity.auth_token = vec![7; 72];
    identity.instance_id = vec![9; 32];
    identity
}
fn data(socket: &Socket, bytes: &[u8]) {
    let mut packet = vec![1, 1, 0, 0, 0];
    packet.extend_from_slice(bytes);
    incoming(socket, Message::Binary(packet.into()));
}
fn fin(socket: &Socket) {
    incoming(socket, Message::Binary(vec![2, 1, 0, 0, 0].into()));
}
fn reply() -> Vec<u8> {
    auth_wire(Frame::new(frame_type::HELLO, 0, [], [], [8; 72]).unwrap())
}
#[test]
fn fragmented_reply_waits_for_fin_and_returns_the_same_live_connection() {
    // Feed single bytes and hold FIN: even a complete successful reply must
    // not renew the identity until the stream's framing is fully validated.
    let socket = wire();
    let mut identity = identity();
    let mut exchange = Authentication::new(raw(socket.clone()), &mut identity).unwrap();
    exchange.advance().unwrap();
    for byte in reply() {
        data(&socket, &[byte]);
        assert!(exchange.advance().unwrap().is_none());
        assert_eq!(exchange.token(), [7; 72]);
    }
    assert!(exchange.continue_now()); // FIN queued only after the validated reply.
    assert!(exchange.advance().unwrap().is_none());
    assert!(!exchange.continue_now());
    fin(&socket);
    let mut connection = exchange.advance().unwrap().unwrap();
    assert!(matches!(exchange.advance(), Err(AuthenticationError::Transport(Error::Closed))));
    drop(exchange);
    assert_eq!(identity.auth_token, [8; 72]);
    assert_eq!(connection.open().unwrap(), 2);
    connection.advance().unwrap();
    assert_eq!(Rc::strong_count(&socket.0), 2);
    connection.close();
    assert_eq!(Rc::strong_count(&socket.0), 1);
}
#[test]
fn invalid_replies_close_transport_without_changing_saved_identity() {
    // Malformed framing, oversize admission, trailing bytes and credential
    // rejection all fail atomically, including a valid prefix followed by junk.
    let mut trailing = reply();
    trailing.push(0);
    let mut oversized = reply();
    oversized[..4].copy_from_slice(&4113u32.to_le_bytes());
    let cases = [
        vec![],
        reply()[..7].to_vec(),
        reply()[..20].to_vec(),
        trailing,
        oversized,
        auth_wire(Frame::new(frame_type::REGISTER, 0, [], [], []).unwrap()),
        Frame::new(frame_type::HELLO, 0, [1], [], [8; 72]).unwrap().encode().unwrap(),
        auth_wire(Frame::new(frame_type::HELLO, 2, [], [], [8; 72]).unwrap()),
        auth_wire(Frame::new(frame_type::HELLO, 255, [], [], []).unwrap()),
    ];
    for bytes in cases {
        let socket = wire();
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let mut exchange = Authentication::new(raw(socket.clone()), &mut identity).unwrap();
        exchange.advance().unwrap();
        data(&socket, &bytes);
        fin(&socket);
        let mut failed = false;
        for _ in 0..8 {
            match exchange.advance() {
                Err(_) => {
                    failed = true;
                    break;
                }
                Ok(None) => {}
                Ok(Some(_)) => panic!("invalid authentication accepted"),
            }
        }
        assert!(failed);
        assert_eq!(Rc::strong_count(&socket.0), 1);
        assert!(exchange.transport_mut().is_err());
        assert!(exchange.advance().is_err());
        drop(exchange);
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
}
#[test]
fn blocked_output_resumes_without_replaying_hello_and_drop_cancels() {
    // Stall a partial WebSocket write, then parse the emitted stream exactly.
    let socket = wire();
    let mut identity = identity();
    let mut exchange = Authentication::new(raw(socket.clone()), &mut identity).unwrap();
    socket.0.borrow_mut().allowance = 3;
    assert!(exchange.advance().unwrap().is_none());
    assert!(exchange.wants_write());
    assert!(exchange.advance().unwrap().is_none());
    assert!(!exchange.continue_now());
    socket.0.borrow_mut().allowance = usize::MAX;
    for _ in 0..3 {
        exchange.advance().unwrap();
    }
    let mut messages = outgoing(&socket);
    let mut payload = Vec::new();
    let mut kinds = Vec::new();
    for _ in 0..2 {
        let Message::Binary(packet) = messages.read().unwrap() else { panic!("binary expected") };
        kinds.push(packet[0]);
        if packet[0] == 1 {
            payload.extend_from_slice(&packet[5..]);
        }
    }
    assert!(messages.read().is_err());
    // HELLO rejection leaves the server waiting for registration; sending FIN
    // here would race its response against connection teardown.
    assert_eq!(kinds, [0, 1]);
    assert_eq!(Frame::decode(&payload).unwrap().body, [7; 72]);
    drop(exchange);
    assert_eq!(Rc::strong_count(&socket.0), 1);
    assert_eq!(identity.auth_token, [7; 72]);
}
#[test]
fn connection_failure_is_terminal_and_preserves_identity() {
    // Failure after a complete reply but before FIN cannot adopt the renewal.
    let socket = wire();
    let mut identity = identity();
    let mut exchange = Authentication::new(raw(socket.clone()), &mut identity).unwrap();
    exchange.advance().unwrap();
    data(&socket, &reply());
    for _ in 0..3 {
        assert!(exchange.advance().unwrap().is_none());
    }
    incoming(&socket, Message::Close(None));
    assert!(exchange.advance().is_err());
    assert_eq!(Rc::strong_count(&socket.0), 1);
    drop(exchange);
    assert_eq!(identity.auth_token, [7; 72]);
}
#[test]
fn constructor_rejects_used_connections_and_unbounded_or_missing_credentials() {
    // Admission happens before I/O; rejected ownership drops the connection.
    for (token, instance) in [(4097, 32), (0, 32), (72, 0)] {
        let socket = wire();
        let mut identity = identity();
        identity.auth_token.resize(token, 7);
        identity.instance_id.resize(instance, 9);
        assert!(Authentication::new(raw(socket.clone()), &mut identity).is_err());
        assert!(socket.0.borrow().outgoing.is_empty());
        assert_eq!(Rc::strong_count(&socket.0), 1);
    }
    let socket = wire();
    let mut identity = identity();
    let mut connection = raw(socket.clone());
    connection.open().unwrap();
    assert!(matches!(
        Authentication::new(connection, &mut identity),
        Err(AuthenticationError::UsedConnection)
    ));
    assert_eq!(Rc::strong_count(&socket.0), 1);
}
#[test]
fn maximum_body_is_accepted_and_empty_renewal_retains_token() {
    // Exercise the exact admission boundary and the shared empty-renewal rule.
    for length in [0, 4092] {
        let socket = wire();
        let mut identity = identity();
        identity.auth_token.resize(4096, 7);
        let mut exchange = Authentication::new(raw(socket.clone()), &mut identity).unwrap();
        exchange.advance().unwrap();
        let bytes = auth_wire(Frame::new(frame_type::HELLO, 0, [], [], vec![8; length]).unwrap());
        data(&socket, &bytes);
        fin(&socket);
        let mut connection = None;
        for _ in 0..8 {
            connection = exchange.advance().unwrap();
            if connection.is_some() {
                break;
            }
        }
        assert!(connection.is_some());
        drop(exchange);
        assert_eq!(
            identity.auth_token,
            vec![if length == 0 { 7 } else { 8 }; if length == 0 { 4096 } else { length }]
        );
    }
}

#[test]
fn rejected_reply_does_not_wait_for_remote_fin() {
    // Ordinary AP rejection leaves this stream open for explicit enrollment.
    // Reject promptly, preserve credentials and drop transport ownership.
    for status in [1, 2, 3, 255] {
        let socket = wire();
        let mut identity = identity();
        let before = serde_json::to_vec(&identity).unwrap();
        let mut exchange = Authentication::new(raw(socket.clone()), &mut identity).unwrap();
        exchange.advance().unwrap();
        let reply = auth_wire(Frame::new(frame_type::HELLO, status, [], [], []).unwrap());
        data(&socket, &reply);
        let mut rejected = false;
        for _ in 0..4 {
            match exchange.advance() {
                Err(AuthenticationError::Identity(
                    crate::network::ap::ClientError::AuthenticationRequired,
                )) if status <= 3 => {
                    rejected = true;
                    break;
                }
                Err(AuthenticationError::Identity(
                    crate::network::ap::ClientError::UnexpectedHelloStatus(255),
                )) if status == 255 => {
                    rejected = true;
                    break;
                }
                Ok(None) => {}
                _ => panic!("unexpected authentication result"),
            }
        }
        assert!(rejected);
        assert_eq!(Rc::strong_count(&socket.0), 1);
        drop(exchange);
        assert_eq!(serde_json::to_vec(&identity).unwrap(), before);
    }
}

fn auth_wire(frame: Frame) -> Vec<u8> {
    crate::network::ap::timing::auth::encode(frame, &Default::default()).unwrap().encode().unwrap()
}
