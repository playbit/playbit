#![no_main]

use libfuzzer_sys::fuzz_target;
use playbit::network::ap::{Frame, MAX_FRAME_LEN, hello, operation::ServiceRequest, replication};

fuzz_target!(|data: &[u8]| {
    // Exercise bounded arbitrary wire bytes and canonical round trips on both endpoints.
    if data.len() > MAX_FRAME_LEN {
        return;
    }
    // Incremental reception has a device-sized allocation budget and preserves
    // the exact boundary when the input contains coalesced stream frames.
    if data.len() <= 4096 {
        let mut reader = playbit::network::ap::receive::FrameReader::new(4096);
        let mut consumed = 0;
        let quantum = 1 + data.first().copied().unwrap_or(0) as usize;
        while consumed < data.len() {
            let buffer = reader.buffer().expect("no previous admission error");
            assert!(buffer.len() <= 4096 + 528);
            let count = buffer.len().min(quantum).min(data.len() - consumed);
            buffer[..count].copy_from_slice(&data[consumed..consumed + count]);
            consumed += count;
            match reader.advance(count) {
                Ok(true) => break,
                Ok(false) => assert!(count > 0),
                Err(_) => {
                    assert!(reader.buffer().is_err());
                    break;
                }
            }
        }
        let result = reader.finish();
        if let Ok(expected) = Frame::decode(data) {
            assert_eq!(result, Ok(expected));
        } else if let Ok(frame) = result {
            assert_eq!(Frame::decode(&data[..consumed]).expect("complete frame"), frame);
        }
    }
    let _ = playbit::network::ap::key_auth::decode(data);
    if data.len() <= playbit::network::ap::websocket::PACKET_MAX {
        let mut client = playbit::network::ap::websocket::Client::new();
        let mut output = vec![0; playbit::network::ap::websocket::PACKET_MAX];
        client.open().expect("fresh stream");
        client.next_packet(&mut output).expect("OPEN");
        let _ = client.receive(data);
    }
    if let Ok(frame) = Frame::decode(data) {
        assert_eq!(frame.encode().expect("decoded frame encodes"), data);
        if frame.body.len() <= playbit::network::ap::timing::auth::MAX_REPLY_BYTES
            && let Ok((plain, timings)) = playbit::network::ap::timing::auth::decode(frame.clone())
        {
            let encoded = playbit::network::ap::timing::auth::encode(plain.clone(), &timings)
                .expect("admitted reply re-encodes");
            let decoded = playbit::network::ap::timing::auth::decode(encoded)
                .expect("canonical auth envelope");
            assert_eq!(decoded, (plain, timings));
        }
        // A rejected authentication reply must not partially replace identity.
        if data.len() <= 4096 {
            use playbit::network::ap::{ClientIdentity, authentication};
            let mut identity = ClientIdentity::default();
            identity.auth_token = vec![1; 72];
            identity.instance_id = vec![b'a'; 32];
            let before = serde_json::to_vec(&identity).expect("fixture serializes");
            let mut candidate = identity.clone();
            if authentication::accept_hello(&mut candidate, frame.clone(), false).is_err() {
                assert_eq!(serde_json::to_vec(&candidate).expect("fixture serializes"), before);
            }
            let mut candidate = identity;
            if authentication::accept_registration(&mut candidate, &frame).is_err() {
                assert_eq!(serde_json::to_vec(&candidate).expect("fixture serializes"), before);
            }
        }

        if data.len() <= 4096 {
            use playbit::network::ap::{frame_type, operation::SERVICE_DESTINATION, service};
            let operation = "12121212121212121212121212121212";
            if let Ok(response) = service::response(frame.clone(), operation) {
                assert_eq!(frame.frame_type, frame_type::DIRECT);
                assert_eq!(frame.source, SERVICE_DESTINATION);
                assert_eq!(response.operation_id, operation);
                assert!(response.ok);
            }
            if let Ok(payload) = service::data(frame.clone()) {
                assert_eq!(frame.frame_type, frame_type::DIRECT);
                assert_eq!(frame.source, SERVICE_DESTINATION);
                assert_eq!(payload, frame.body);
            }
        }

        if let Ok(fields) = playbit::network::ap::registration::Response::parse(&frame) {
            assert_eq!(fields.status, frame.flags);
            assert_eq!(
                4 + fields.auth_token.len()
                    + fields.instance_id.len()
                    + fields.ca_certificate.len()
                    + fields.instance_certificate.len(),
                frame.body.len()
            );
            assert_eq!(fields.auth_token.len(), frame.body[0] as usize);
            assert_eq!(fields.instance_id.len(), frame.body[1] as usize);
            assert_eq!(
                fields.ca_certificate.len(),
                u16::from_le_bytes([frame.body[2], frame.body[3]]) as usize
            );
        }

        if let Ok(decoded) = hello::decode(&frame) {
            let credentials =
                hello::TestingCredentials { key: decoded.key, namespace: decoded.namespace };
            assert_eq!(credentials.hello(decoded.csr).expect("validated HELLO"), frame);
        }
    }
    if let Ok(event) = replication::ReplicaEvent::decode(data) {
        assert_eq!(event.encode().expect("decoded event encodes"), data);
        use replication::{Cursor, ReplicaEventKind, SubscriptionAck};
        let ack = SubscriptionAck { epoch: event.epoch, sequence: 0, policy_revision: 0 };
        let mut cursor = Cursor::new(&ack).expect("decoded nonzero epoch");
        if cursor.accept(&event) == Ok(true)
            && !matches!(event.kind, ReplicaEventKind::Gap | ReplicaEventKind::Terminal { .. })
        {
            assert_eq!(event.sequence, 1);
            cursor.applied(&event);
            assert_eq!(cursor.accept(&event), Ok(false));
        }
    }
    if let Ok(request) = playbit::network::ap::service::admit_request(
        playbit::network::ap::service::PROTOCOL,
        data,
        &[],
    ) {
        assert!(request.validate().is_ok());
        let frame =
            playbit::network::ap::service::request(&request).expect("admitted request encodes");
        // Encoding adds required features for operations such as bloblog reads.
        // The receiving endpoint must support those features for the round trip.
        assert_eq!(
            playbit::network::ap::service::admit_request(
                frame.flags,
                &frame.body,
                playbit::network::ap::service::OPTIONAL_FEATURES,
            )
            .expect("encoded request is admitted with supported features"),
            request
        );
    }
    if let Ok(request) = serde_json::from_slice::<ServiceRequest>(data) {
        let result = request.validate();
        let bytes = serde_json::to_vec(&request).expect("request serializes");
        let copy: ServiceRequest = serde_json::from_slice(&bytes).expect("request round trip");
        assert_eq!(request, copy);
        assert_eq!(result, copy.validate());
    }
    if data.len() <= playbit::network::ap::timing::MAX_ENCODED_BYTES
        && let Ok(timings) = serde_json::from_slice::<playbit::network::ap::timing::Timings>(data)
    {
        let encoded = serde_json::to_vec(&timings).expect("bounded timing projection");
        assert!(encoded.len() <= playbit::network::ap::timing::MAX_ENCODED_BYTES);
        assert_eq!(timings, serde_json::from_slice(&encoded).expect("exact duration round trip"));
    }
    // Also reach frame construction with arbitrary types, names, padding and payload lengths.
    if data.len() >= 4 {
        let source_end = 4 + (data[2] as usize).min(data.len() - 4);
        let destination_end = source_end + (data[3] as usize).min(data.len() - source_end);
        if let Ok(frame) = Frame::new(
            data[0],
            data[1],
            &data[4..source_end],
            &data[source_end..destination_end],
            &data[destination_end..],
        ) {
            let bytes = frame.encode().expect("valid frame encodes");
            assert_eq!(Frame::decode(&bytes).expect("encoded frame decodes"), frame);
        }
    }
});
