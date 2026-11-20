//! Wire vectors and bounded lifecycle tests for the native AP stream client.
use super::*;

fn wire(kind: u8, id: u32, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![kind];
    bytes.extend(id.to_le_bytes());
    bytes.extend(body);
    bytes
}
fn opened(client: &mut Client, output: &mut [u8]) -> u32 {
    let id = client.open().expect("room for stream");
    let count = client.next_packet(output).expect("valid output");
    assert_eq!(&output[..count], wire(OPEN, id, &[]));
    id
}

#[test]
fn credit_returns_only_on_consumption_and_ring_wrap_preserves_order() {
    // Fill a window, consume across its two slices, and inspect exact credits.
    let mut client = Client::new();
    let mut packet = vec![0; PACKET_MAX];
    let id = opened(&mut client, &mut packet);
    client.receive(&wire(DATA, id, &[1; CHUNK])).expect("first half");
    client.receive(&wire(DATA, id, &[2; CHUNK])).expect("second half");
    assert_eq!(client.next_packet(&mut packet), Ok(0));
    let mut output = vec![0; CHUNK + 17];
    assert_eq!(client.read(id, &mut output), Ok(Read::Data(CHUNK + 17)));
    assert_eq!(&output[..CHUNK], &[1; CHUNK]);
    assert_eq!(&output[CHUNK..], &[2; 17]);
    let count = client.next_packet(&mut packet).expect("connection credit");
    assert_eq!(&packet[..count], wire(CREDIT, 0, &((CHUNK + 17) as u32).to_le_bytes()));
    let count = client.next_packet(&mut packet).expect("returned credit");
    assert_eq!(&packet[..count], wire(CREDIT, id, &((CHUNK + 17) as u32).to_le_bytes()));
    client.receive(&wire(DATA, id, &[3; CHUNK])).expect("ring wraps");
    let mut rest = vec![0; WINDOW];
    assert_eq!(client.read(id, &mut rest), Ok(Read::Data(WINDOW - 17)));
    assert_eq!(&rest[..CHUNK - 17], &[2; CHUNK - 17]);
    assert_eq!(&rest[CHUNK - 17..WINDOW - 17], &[3; CHUNK]);
    assert_eq!(client.read(id, &mut rest), Ok(Read::Pending));
}

#[test]
fn blocked_bulk_stream_leaves_sibling_and_control_progress() {
    // Consume one stream's entire send window while a sibling remains usable.
    let mut client = Client::new();
    let mut output = vec![0; PACKET_MAX];
    let bulk = opened(&mut client, &mut output);
    let live = opened(&mut client, &mut output);
    for _ in 0..2 {
        assert_eq!(client.write(bulk, &[7; CHUNK], &mut output), Ok(PACKET_MAX));
    }
    assert_eq!(client.write(bulk, b"blocked", &mut output), Ok(0));
    assert_eq!(client.write(live, b"live", &mut output), Ok(9));
    assert_eq!(&output[..9], wire(DATA, live, b"live"));
    client.receive(&wire(CREDIT, bulk, &3u32.to_le_bytes())).expect("peer consumed");
    assert_eq!(client.write(bulk, b"abcdef", &mut output), Ok(8));
    assert_eq!(&output[..8], wire(DATA, bulk, b"abc"));
    client.receive(&wire(STOP, bulk, &[])).expect("peer cancelled");
    assert_eq!(client.write(bulk, b"x", &mut output), Err(Error::WriteClosed));
    let count = client.next_packet(&mut output).expect("FIN ready");
    assert_eq!(&output[..count], wire(FIN, bulk, &[]));
    assert_eq!(client.write(live, b"still live", &mut output), Ok(15));
}

#[test]
fn admission_release_and_half_close_keep_ids_and_crossing_packets_safe() {
    // Fill all slots, cancel one, then reuse its slot only after both FINs.
    let mut client = Client::new();
    let mut output = vec![0; PACKET_MAX];
    let ids: Vec<_> = (0..STREAM_LIMIT).map(|_| opened(&mut client, &mut output)).collect();
    assert_eq!(client.open(), Err(Error::StreamLimit));
    let id = ids[0];
    client.receive(&wire(DATA, id, b"unread")).expect("input");
    client.release(id).expect("cancel stream");
    assert_eq!(client.read(id, &mut output), Err(Error::ReadClosed));
    assert_eq!(client.open(), Err(Error::StreamLimit));
    let count = client.next_packet(&mut output).expect("discarded connection credit");
    assert_eq!(&output[..count], wire(CREDIT, 0, &6u32.to_le_bytes()));
    let count = client.next_packet(&mut output).expect("STOP ready");
    assert_eq!(&output[..count], wire(STOP, id, &[]));
    client.receive(&wire(DATA, id, b"crossed stop")).expect("in-flight bytes discarded");
    let count = client.next_packet(&mut output).expect("crossed connection credit");
    assert_eq!(&output[..count], wire(CREDIT, 0, &12u32.to_le_bytes()));
    let count = client.next_packet(&mut output).expect("FIN ready");
    assert_eq!(&output[..count], wire(FIN, id, &[]));
    assert_eq!(client.open(), Err(Error::StreamLimit));
    client.receive(&wire(FIN, id, &[])).expect("peer FIN retires");
    assert_eq!(client.read(id, &mut output), Err(Error::UnknownStream));
    assert_eq!(opened(&mut client, &mut output), STREAM_LIMIT as u32 + 1);
    // Retired IDs ignore shaped crossing packets, as the existing AP does.
    client.receive(&wire(CREDIT, id, &0u32.to_le_bytes())).expect("retired packet");
    client.receive(&wire(DATA, id, b"late")).expect("retired data");
    assert_eq!(client.next_packet(&mut output), Ok(9));
    let live = ids[1];
    client.receive(&wire(FIN, live, &[])).expect("read half EOF");
    assert_eq!(client.read(live, &mut output), Ok(Read::Finished));
    assert_eq!(client.write(live, b"reply", &mut output), Ok(10));
    client.finish(live).expect("write half finish");
    assert_eq!(client.write(live, b"x", &mut output), Err(Error::WriteClosed));
    assert_eq!(client.next_packet(&mut output), Ok(5));
    assert_eq!(client.read(live, &mut []), Ok(Read::Data(0)));
    client.release(live).expect("release complete stream");
    assert_eq!(client.read(live, &mut output), Err(Error::UnknownStream));
}

#[test]
fn short_output_and_id_exhaustion_do_not_consume_state() {
    // Caller mistakes precede effects, and monotonically allocated IDs never wrap.
    let mut client = Client::new();
    let id = client.open().expect("stream");
    let mut output = vec![0; PACKET_MAX];
    assert_eq!(client.write(id, b"wait for OPEN", &mut output), Ok(0));
    assert_eq!(client.next_packet(&mut output[..4]), Err(Error::OutputTooSmall));
    assert_eq!(client.next_packet(&mut output), Ok(5));
    assert_eq!(&output[..5], wire(OPEN, id, &[]));
    assert_eq!(client.write(id, b"x", &mut output[..6]), Err(Error::OutputTooSmall));
    for _ in 0..2 {
        assert_eq!(client.write(id, &[0; CHUNK], &mut output), Ok(PACKET_MAX));
    }
    client.last_id = u32::MAX;
    assert_eq!(client.open(), Err(Error::IdExhausted));
    assert_eq!(client.last_id, u32::MAX);
    assert_eq!(client.streams.iter().flatten().count(), 1);
}

#[test]
fn malformed_packets_close_all_streams() {
    // Shape, unknown ID, credit, direction, FIN and maximum-size violations
    // all fail closed; no surviving sibling can resume on poisoned state.
    let bad = [
        vec![],
        vec![0; 4],
        wire(OPEN, 1, &[]),
        wire(9, 1, &[]),
        wire(DATA, 0, b"x"),
        wire(DATA, 3, b"x"),
        wire(DATA, 1, &[]),
        wire(FIN, 1, b"x"),
        wire(STOP, 1, b"x"),
        wire(CREDIT, 1, &[1]),
        wire(CREDIT, 1, &0u32.to_le_bytes()),
        wire(CREDIT, 1, &1u32.to_le_bytes()),
        wire(CREDIT, 1, &u32::MAX.to_le_bytes()),
        wire(DATA, 1, &vec![0; CHUNK + 1]),
    ];
    for bytes in bad {
        let mut client = Client::new();
        let mut output = vec![0; PACKET_MAX];
        opened(&mut client, &mut output);
        opened(&mut client, &mut output);
        assert!(client.receive(&bytes).is_err(), "{bytes:?}");
        assert_eq!(client.open(), Err(Error::Closed));
        assert_eq!(client.read(2, &mut output), Err(Error::Closed));
        assert_eq!(client.next_packet(&mut output), Err(Error::Closed));
        assert!(client.streams.iter().all(Option::is_none));
    }
    for kind in [DATA, FIN] {
        let mut client = Client::new();
        let mut output = vec![0; PACKET_MAX];
        let id = opened(&mut client, &mut output);
        client.receive(&wire(FIN, id, &[])).expect("first FIN");
        let body: &[u8] = if kind == DATA { b"x" } else { b"" };
        assert_eq!(client.receive(&wire(kind, id, body)), Err(Error::AfterFin));
    }
}

#[test]
fn tiny_fragments_coalesce_and_unadvertised_credit_cannot_be_spent() {
    // No per-packet allocation/list: a full window of one-byte messages fits,
    // but a read cannot authorize peer traffic until CREDIT is emitted.
    let mut client = Client::new();
    let mut output = vec![0; PACKET_MAX];
    let id = opened(&mut client, &mut output);
    let bytes = wire(DATA, id, b"a");
    for _ in 0..WINDOW {
        client.receive(&bytes).expect("one byte within window");
    }
    assert_eq!(client.stream(id).expect("live").incoming.capacity(), WINDOW);
    assert_eq!(client.read(id, &mut [0]), Ok(Read::Data(1)));
    assert_eq!(client.receive(&bytes), Err(Error::FlowControl));
}

#[test]
fn controls_rotate_and_unsent_open_cannot_receive() {
    // A stream with several pending controls cannot starve sibling OPEN/FIN.
    let mut client = Client::new();
    let mut output = vec![0; PACKET_MAX];
    for _ in 0..STREAM_LIMIT {
        let id = client.open().expect("room");
        client.release(id).expect("cancel before OPEN");
    }
    for kind in [OPEN, STOP, FIN] {
        for id in 1..=STREAM_LIMIT as u32 {
            let count = client.next_packet(&mut output).expect("control ready");
            assert_eq!(&output[..count], wire(kind, id, &[]));
        }
    }
    assert_eq!(client.next_packet(&mut output), Ok(0));
    let mut client = Client::new();
    let id = client.open().expect("room");
    assert_eq!(client.receive(&wire(FIN, id, &[])), Err(Error::MalformedPacket));
}
