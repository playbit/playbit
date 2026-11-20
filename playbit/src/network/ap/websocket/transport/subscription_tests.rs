use super::*;
use crate::network::ap::{
    self, Frame, frame_type,
    operation::{SERVICE_DESTINATION, ServiceRequest},
    replication::{self, ReplicaEvent, ReplicaEventKind},
    service,
};
use crate::store::{DatabaseId, EntryId};
fn database() -> DatabaseId {
    DatabaseId::Entry(EntryId::from_bytes([3; 15]).unwrap())
}
fn request() -> ServiceRequest {
    ServiceRequest::SubscribeReplica {
        entry_scope: None,
        operation_id: crate::identifier::encode(&[0x11; 16]),
        volume_id: crate::identifier::encode(&[0x22; 16]),
        session_id: None,
        build_id: None,
    }
}
fn connection(socket: &Socket) -> Connection<Socket> {
    let mut connection = raw(socket.clone());
    assert_eq!(connection.open().unwrap(), 1);
    connection
}
fn data(socket: &Socket, bytes: &[u8]) {
    let mut packet = vec![1, 2, 0, 0, 0];
    packet.extend_from_slice(bytes);
    incoming(socket, Message::Binary(packet.into()));
}
fn ack(operation: &str) -> Vec<u8> {
    Frame::new(
        frame_type::DIRECT,
        crate::network::ap::service::PROTOCOL,
        SERVICE_DESTINATION,
        b"client",
        serde_json::to_vec(&serde_json::json!({
            "operation_id": operation, "timing": [], "ok": true,
            "data": {"epoch": vec![7; 16], "sequence": 0, "policy_revision": 1}
        }))
        .unwrap(),
    )
    .unwrap()
    .encode()
    .unwrap()
}
fn envelope(kind: ReplicaEventKind<'_>) -> Vec<u8> {
    let body =
        ReplicaEvent { epoch: [7; 16], sequence: 1, policy_revision: 1, kind }.encode().unwrap();
    Frame::new(frame_type::DIRECT, 0, SERVICE_DESTINATION, b"client", body)
        .unwrap()
        .encode()
        .unwrap()
}
fn settle(subscription: &mut Subscription<'_, Socket>) {
    for _ in 0..128 {
        assert!(subscription.advance().unwrap().is_none());
        if !subscription.continue_now() {
            return;
        }
    }
    panic!("subscription did not quiesce");
}
fn receive(
    subscription: &mut Subscription<'_, Socket>,
    socket: &Socket,
    bytes: &[u8],
) -> SubscriptionItem {
    let mut item = None;
    for chunk in bytes.chunks(512) {
        data(socket, chunk);
        for _ in 0..128 {
            if let Some(next) = subscription.advance().unwrap() {
                assert!(item.is_none());
                item = Some(next);
                break;
            }
            if !subscription.continue_now() {
                break;
            }
        }
    }
    item.expect("complete frame")
}
#[test]
fn subscription_streams_large_events_and_explicit_checkpoints_without_fin() {
    // Receive beyond one stream credit window, retain no whole-stream buffer,
    // and send one explicit barrier while keeping the stream writable.
    let socket = wire();
    let mut connection = connection(&socket);
    let request = request();
    let mut subscription =
        Subscription::new(&mut connection, &request, replication::MAX_EVENT_BYTES).unwrap();
    assert!(subscription.checkpoint().is_err());
    settle(&mut subscription);
    assert!(matches!(
        receive(&mut subscription, &socket, &ack(request.operation_id())),
        SubscriptionItem::Acknowledged(_)
    ));
    let delta = vec![0x55; replication::MAX_DELTA_BYTES];
    let SubscriptionItem::Event(bytes) = receive(
        &mut subscription,
        &socket,
        &envelope(ReplicaEventKind::Changes { database: database(), delta: &delta }),
    ) else {
        panic!()
    };
    assert_eq!(
        ReplicaEvent::decode(&bytes).unwrap().kind,
        ReplicaEventKind::Changes { database: database(), delta: &delta }
    );
    drop(bytes);
    subscription.checkpoint().unwrap();
    assert!(subscription.checkpoint().is_err());
    settle(&mut subscription);
    let SubscriptionItem::Event(bytes) =
        receive(&mut subscription, &socket, &envelope(ReplicaEventKind::CaughtUp))
    else {
        panic!()
    };
    assert_eq!(ReplicaEvent::decode(&bytes).unwrap().kind, ReplicaEventKind::CaughtUp);
    subscription.checkpoint().unwrap();
    settle(&mut subscription);
    let mut sent = Vec::new();
    let mut output = outgoing(&socket);
    while let Ok(Message::Binary(packet)) = output.read() {
        if packet[..5] == [1, 2, 0, 0, 0] {
            sent.extend_from_slice(&packet[5..]);
        }
    }
    let control =
        Frame::new(frame_type::DIRECT, 0, [], SERVICE_DESTINATION, replication::CHECKPOINT)
            .unwrap()
            .encode()
            .unwrap();
    let mut expected = service::request(&request).unwrap().encode().unwrap();
    expected.extend_from_slice(&control);
    expected.extend_from_slice(&control);
    assert_eq!(sent, expected);
    assert!(!subscription.continue_now());
    drop(subscription);
    assert_eq!(connection.open().unwrap(), 3);
}
#[test]
fn invalid_ack_and_truncated_stream_retire_without_closing_connection() {
    // A mismatched operation or incomplete reply cannot become a subscription.
    for bytes in [
        ack(&crate::identifier::encode(&[0x33; 16])),
        ack(&crate::identifier::encode(&[0x11; 16]))[..12].to_vec(),
    ] {
        let socket = wire();
        let mut connection = connection(&socket);
        let request = request();
        let mut subscription = Subscription::new(&mut connection, &request, 4096).unwrap();
        settle(&mut subscription);
        data(&socket, &bytes);
        incoming(&socket, Message::Binary(vec![2, 2, 0, 0, 0].into()));
        let mut failed = false;
        for _ in 0..128 {
            if subscription.advance().is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(subscription.advance().is_err());
        assert!(subscription.checkpoint().is_err());
        drop(subscription);
        assert_eq!(connection.open().unwrap(), 3);
    }
}
#[test]
fn event_limit_is_enforced_from_header_and_terminal_delivery_closes_stream() {
    // Admission rejects oversized frames before their body arrives. Terminals
    // are delivered once for host recovery/revocation handling, never as EOF.
    for kind in [
        None,
        Some(ReplicaEventKind::Gap),
        Some(ReplicaEventKind::Terminal {
            volume: [7; 16],
            cause: crate::network::ap::replication::TerminalCause::AuthorityChanged,
        }),
    ] {
        let socket = wire();
        let mut connection = connection(&socket);
        let request = request();
        let limit = if matches!(kind, Some(ReplicaEventKind::Terminal { .. })) {
            replication::MAX_EVENT_BYTES
        } else {
            replication::EVENT_HEADER_BYTES
        };
        let mut subscription = Subscription::new(&mut connection, &request, limit).unwrap();
        settle(&mut subscription);
        receive(&mut subscription, &socket, &ack(request.operation_id()));
        if let Some(kind) = kind {
            assert!(matches!(
                receive(&mut subscription, &socket, &envelope(kind)),
                SubscriptionItem::Event(_)
            ));
        } else {
            let bytes = envelope(ReplicaEventKind::Changes { database: database(), delta: &[1] });
            data(&socket, &bytes[..8]);
            let error = subscription.advance().unwrap_err();
            assert_eq!(error.to_string(), ap::Error::FrameTooLarge.to_string());
        }
        assert!(subscription.advance().is_err());
        drop(subscription);
        assert_eq!(connection.open().unwrap(), 3);
    }
}

#[test]
fn invalid_subscription_admission_does_not_consume_a_stream() {
    // Limits and request kind are validated before allocating a stream. An
    // unauthenticated first stream is rejected and its connection is closed.
    let socket = wire();
    let mut connection = connection(&socket);
    let request = request();
    for limit in
        [0, replication::EVENT_HEADER_BYTES - 1, replication::MAX_EVENT_BYTES + 1, usize::MAX]
    {
        assert!(Subscription::new(&mut connection, &request, limit).is_err());
    }
    let wrong = ServiceRequest::RegisterVolume {
        operation_id: crate::identifier::encode(&[0x11; 16]),
        volume_id: crate::identifier::encode(&[0x22; 16]),
        owner_account_id: crate::identifier::encode(&[0x33; 16]),
    };
    assert!(Subscription::new(&mut connection, &wrong, 4096).is_err());
    assert_eq!(connection.open().unwrap(), 2);
    let mut fresh = raw(socket);
    assert!(Subscription::new(&mut fresh, &request, 4096).is_err());
    assert!(fresh.open().is_err());
}
#[test]
fn malformed_event_retires_subscription_and_preserves_other_streams() {
    // Valid outer framing does not make an unknown replica envelope acceptable.
    let socket = wire();
    let mut connection = connection(&socket);
    let request = request();
    let mut subscription = Subscription::new(&mut connection, &request, 4096).unwrap();
    settle(&mut subscription);
    receive(&mut subscription, &socket, &ack(request.operation_id()));
    let frame = Frame::new(
        frame_type::DIRECT,
        0,
        SERVICE_DESTINATION,
        b"client",
        [0; replication::EVENT_HEADER_BYTES],
    )
    .unwrap()
    .encode()
    .unwrap();
    data(&socket, &frame);
    let mut failed = false;
    for _ in 0..128 {
        if subscription.advance().is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    drop(subscription);
    assert_eq!(connection.open().unwrap(), 3);
}
