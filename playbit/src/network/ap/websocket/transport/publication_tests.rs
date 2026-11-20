use super::*;
use crate::network::ap::{
    Frame, frame_type,
    operation::{SERVICE_DESTINATION, ServiceRequest},
};
use crate::store::{DatabaseId, EntryId};
fn request() -> ServiceRequest {
    ServiceRequest::PublishReplica {
        operation_id: crate::identifier::encode(&[0x11; 16]),
        volume_id: crate::identifier::encode(&[0x22; 16]),
        database_id: DatabaseId::Entry(EntryId::from_bytes([2; 15]).unwrap()),
        session_id: None,
        build_id: None,
    }
}
fn connection(socket: &Socket) -> Connection<Socket> {
    let mut connection = raw(socket.clone());
    assert_eq!(connection.open().unwrap(), 1); // Reserve authentication in this wire-only fixture.
    connection
}
fn data(socket: &Socket, bytes: &[u8]) {
    let mut packet = vec![1, 2, 0, 0, 0];
    packet.extend_from_slice(bytes);
    incoming(socket, Message::Binary(packet.into()));
}
fn fin(socket: &Socket) {
    incoming(socket, Message::Binary(vec![2, 2, 0, 0, 0].into()));
}
fn reply(operation: &str, clock: u64) -> Vec<u8> {
    Frame::new(frame_type::DIRECT, crate::network::ap::service::PROTOCOL, SERVICE_DESTINATION, b"client", serde_json::to_vec(&serde_json::json!({
        "operation_id": operation, "timing": [], "ok": true, "data": {"database_clock": clock, "policy_revision": 1}
    })).unwrap()).unwrap().encode().unwrap()
}
fn send(publication: &mut Publication<'_, Socket>) {
    for _ in 0..32 {
        assert!(publication.advance().unwrap().is_none());
        if !publication.continue_now() {
            return;
        }
    }
    panic!("small publication did not quiesce");
}
#[test]
fn publication_borrows_payload_emits_shared_frames_and_waits_for_fin() {
    let socket = wire();
    let mut connection = connection(&socket);
    let request = request();
    let payload = vec![7; 257];
    let mut publication = Publication::new(&mut connection, &request, &payload).unwrap();
    send(&mut publication);
    let mut wire_bytes = Vec::new();
    let mut output = outgoing(&socket);
    while let Ok(Message::Binary(packet)) = output.read() {
        if packet[..5] == [1, 2, 0, 0, 0] {
            wire_bytes.extend_from_slice(&packet[5..]);
        }
    }
    let mut expected = crate::network::ap::service::request(&request).unwrap().encode().unwrap();
    expected.extend_from_slice(
        &Frame::new(frame_type::DIRECT, 0, [], SERVICE_DESTINATION, payload.clone())
            .unwrap()
            .encode()
            .unwrap(),
    );
    assert_eq!(wire_bytes, expected);
    for byte in reply(request.operation_id(), 7) {
        data(&socket, &[byte]);
        assert!(publication.advance().unwrap().is_none());
    }
    assert!(publication.advance().unwrap().is_none());
    assert!(!publication.continue_now());
    fin(&socket);
    assert_eq!(publication.advance().unwrap().unwrap().database_clock, 7);
    assert!(publication.advance().is_err());
    drop(publication);
    assert_eq!(connection.open().unwrap(), 3);
}
#[test]
fn invalid_publication_replies_retire_only_their_stream() {
    let request = request();
    let valid = reply(request.operation_id(), 7);
    let mut trailing = valid.clone();
    trailing.push(0);
    let mut oversized = valid.clone();
    oversized[..4].copy_from_slice(&8192u32.to_le_bytes());
    for bytes in [
        vec![],
        valid[..7].to_vec(),
        trailing,
        oversized,
        reply("different", 7),
        reply(request.operation_id(), 0),
    ] {
        let socket = wire();
        let mut connection = connection(&socket);
        let mut publication = Publication::new(&mut connection, &request, &[7; 10]).unwrap();
        send(&mut publication);
        if !bytes.is_empty() {
            data(&socket, &bytes);
        }
        fin(&socket);
        let mut rejected = false;
        for _ in 0..32 {
            match publication.advance() {
                Err(_) => {
                    rejected = true;
                    break;
                }
                Ok(None) => {}
                Ok(Some(_)) => panic!("invalid response acknowledged"),
            }
        }
        assert!(rejected);
        drop(publication);
        assert_eq!(connection.open().unwrap(), 3);
        assert!(connection.socket.is_some());
    }
}
#[test]
fn publication_drop_releases_blocked_stream_and_preflight_does_not_admit_invalid_data() {
    let socket = wire();
    let mut connection = connection(&socket);
    let request = request();
    assert!(Publication::new(&mut connection, &request, &[]).is_err());
    assert!(Publication::new(&mut connection, &request, &vec![0; 65537]).is_err());
    socket.0.borrow_mut().allowance = 0;
    let mut publication = Publication::new(&mut connection, &request, &[7; 10]).unwrap();
    assert!(publication.advance().unwrap().is_none());
    assert!(publication.wants_write());
    drop(publication); // e.g. host deadline or cancellation, while output is blocked.
    assert!(connection.read(2, &mut [0]).is_err());
    assert_eq!(connection.open().unwrap(), 3);
    assert!(connection.socket.is_some());
}

#[test]
fn service_rejection_does_not_wait_for_payload_credit_or_remote_fin() {
    let socket = wire();
    let mut connection = connection(&socket);
    let request = request();
    let payload = vec![7; 65536];
    let mut publication = Publication::new(&mut connection, &request, &payload).unwrap();
    for _ in 0..32 {
        assert!(publication.advance().unwrap().is_none());
    }
    assert!(!publication.continue_now(), "payload is waiting for stream credit");
    let failure = Frame::new(frame_type::DIRECT, crate::network::ap::service::PROTOCOL, SERVICE_DESTINATION, b"client",
        serde_json::to_vec(&serde_json::json!({"operation_id": request.operation_id(), "timing": [], "ok": false, "error": "revoked"})).unwrap(),
    ).unwrap().encode().unwrap();
    data(&socket, &failure); // Deliberately no FIN and no extra CREDIT.
    let mut rejected = false;
    for _ in 0..8 {
        if let Err(error) = publication.advance() {
            assert_eq!(error.to_string(), "revoked");
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    drop(publication);
    assert_eq!(connection.open().unwrap(), 3);
}
