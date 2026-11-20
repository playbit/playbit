#![allow(clippy::unwrap_used, reason = "Tests assert bounded transport fixtures.")]
use super::*;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use tungstenite::protocol::Role;
#[derive(Default)]
struct Wire {
    incoming: VecDeque<u8>,
    outgoing: Vec<u8>,
    allowance: usize,
    read_bytes: usize,
}
#[derive(Clone)]
struct Socket(Rc<RefCell<Wire>>);
impl Read for Socket {
    fn read(&mut self, out: &mut [u8]) -> stdio::Result<usize> {
        let mut wire = self.0.borrow_mut();
        if wire.incoming.is_empty() {
            return Err(stdio::ErrorKind::WouldBlock.into());
        }
        let count = out.len().min(wire.incoming.len());
        for byte in &mut out[..count] {
            *byte = wire.incoming.pop_front().unwrap();
        }
        wire.read_bytes += count;
        Ok(count)
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> stdio::Result<usize> {
        let mut wire = self.0.borrow_mut();
        if wire.allowance == 0 {
            return Err(stdio::ErrorKind::WouldBlock.into());
        }
        let count = bytes.len().min(wire.allowance);
        wire.allowance -= count;
        wire.outgoing.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> stdio::Result<()> {
        Ok(())
    }
}
fn wire() -> Socket {
    Socket(Rc::new(RefCell::new(Wire { allowance: usize::MAX, ..Wire::default() })))
}
fn raw(socket: Socket) -> Connection<Socket> {
    let mut bounded = Bounded::new(socket);
    bounded.upgraded();
    Connection {
        socket: Some(WebSocket::from_raw_socket(bounded, Role::Client, Some(config()))),
        streams: Client::new(),
        packet: vec![0; PACKET_MAX],
        pending: false,
    }
}
fn incoming(socket: &Socket, message: Message) {
    let mut server =
        WebSocket::from_raw_socket(std::io::Cursor::new(Vec::new()), Role::Server, Some(config()));
    server.write(message).unwrap();
    socket.0.borrow_mut().incoming.extend(server.get_ref().get_ref());
}
fn outgoing(socket: &Socket) -> WebSocket<std::io::Cursor<Vec<u8>>> {
    WebSocket::from_raw_socket(
        std::io::Cursor::new(std::mem::take(&mut socket.0.borrow_mut().outgoing)),
        Role::Server,
        Some(config()),
    )
}
fn response(socket: &Socket, protocol: &str) {
    let request = String::from_utf8(socket.0.borrow().outgoing.clone()).unwrap();
    let key = request
        .lines()
        .find_map(|line| {
            line.split_once(": ")
                .filter(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
                .map(|(_, v)| v)
        })
        .unwrap();
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let bytes = format!(
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: {protocol}\r\n\r\n"
    );
    socket.0.borrow_mut().incoming.extend(bytes.bytes());
}
#[test]
fn upgrade_is_resumable_checks_subprotocol_and_consumes_its_owner() {
    for protocol in ["playbit-ap-2", "other"] {
        let socket = wire();
        socket.0.borrow_mut().allowance = 7;
        let mut connecting = Connecting::new(socket.clone(), "localhost:443").unwrap();
        assert!(connecting.advance().unwrap().is_none());
        assert!(connecting.wants_write());
        socket.0.borrow_mut().allowance = usize::MAX;
        assert!(connecting.advance().unwrap().is_none());
        assert!(!connecting.wants_write());
        assert!(!connecting.continue_now());
        response(&socket, protocol);
        let result = connecting.advance();
        if protocol == "playbit-ap-2" {
            let connection = result.unwrap().unwrap();
            let config = connection.socket.as_ref().unwrap().get_config();
            assert_eq!(config.max_message_size, Some(PACKET_MAX));
            assert_eq!(config.max_write_buffer_size, WRITE_BYTES);
        } else {
            assert!(result.is_err());
        }
        assert!(matches!(connecting.advance(), Err(Error::Closed)));
    }
    for authority in ["", "a/b", "user@host"] {
        assert!(matches!(Connecting::new(wire(), authority), Err(Error::Authority)));
    }
}
#[test]
fn oversized_upgrade_is_terminal_without_unbounded_header_growth() {
    let socket = wire();
    let mut connecting = Connecting::new(socket.clone(), "localhost").unwrap();
    connecting.advance().unwrap();
    socket.0.borrow_mut().incoming.extend(b"HTTP/1.1 101 Switching Protocols\r\nLong: ");
    socket.0.borrow_mut().incoming.extend(std::iter::repeat_n(b'x', 20_000));
    assert!(connecting.advance().is_err());
    assert_eq!(socket.0.borrow().read_bytes, 8192);
    assert!(matches!(connecting.advance(), Err(Error::Closed)));
}
#[test]
fn partially_written_packet_is_retained_once_and_backpressures_the_caller() {
    let socket = wire();
    let mut connection = raw(socket.clone());
    let id = connection.open().unwrap();
    connection.advance().unwrap();
    socket.0.borrow_mut().allowance = 5;
    assert_eq!(connection.write(id, &[9; 800]).unwrap(), 800);
    assert_eq!(connection.write(id, b"must not be consumed").unwrap(), 0);
    assert!(connection.advance().unwrap().wants_write);
    socket.0.borrow_mut().allowance = usize::MAX;
    assert!(!connection.advance().unwrap().wants_write);
    let mut server = outgoing(&socket);
    assert_eq!(server.read().unwrap(), Message::Binary(vec![0, 1, 0, 0, 0].into()));
    let Message::Binary(data) = server.read().unwrap() else {
        panic!("DATA expected");
    };
    assert_eq!(&data[..5], &[1, 1, 0, 0, 0]);
    assert_eq!(&data[5..], &[9; 800]);
    assert_eq!(server.get_ref().position() as usize, server.get_ref().get_ref().len());
}
#[test]
fn receive_ping_and_credit_use_the_shared_stream_state() {
    let socket = wire();
    let mut connection = raw(socket.clone());
    let id = connection.open().unwrap();
    connection.advance().unwrap();
    socket.0.borrow_mut().outgoing.clear();
    incoming(&socket, Message::Binary(vec![1, 1, 0, 0, 0, 42].into()));
    incoming(&socket, Message::Ping(vec![7].into()));
    let progress = connection.advance().unwrap();
    assert_eq!(progress.messages, 2);
    let mut byte = [0];
    assert_eq!(connection.read(id, &mut byte).unwrap(), StreamRead::Data(1));
    assert_eq!(byte, [42]);
    connection.advance().unwrap();
    let mut server = outgoing(&socket);
    assert_eq!(server.read().unwrap(), Message::Pong(vec![7].into()));
    assert_eq!(server.read().unwrap(), Message::Binary(vec![4, 1, 0, 0, 0, 1, 0, 0, 0].into()));
}
#[test]
fn fragmented_input_and_empty_fragment_flood_have_a_finite_turn() {
    let socket = wire();
    let mut connection = raw(socket.clone());
    let id = connection.open().unwrap();
    connection.advance().unwrap();
    // A fragmented AP DATA message, including an empty non-final continuation.
    socket.0.borrow_mut().incoming.extend([2, 3, 1, 1, 0, 0, 0, 128, 3, 0, 0, 42]);
    assert_eq!(connection.advance().unwrap().messages, 1);
    let mut byte = [0];
    assert_eq!(connection.read(id, &mut byte).unwrap(), StreamRead::Data(1));
    assert_eq!(byte, [42]);
    socket.0.borrow_mut().incoming.extend([2, 0]);
    socket.0.borrow_mut().incoming.extend(std::iter::repeat_n([0, 0], 40_000).flatten());
    socket.0.borrow_mut().outgoing.clear();
    connection.finish(id).unwrap();
    let before = socket.0.borrow().read_bytes;
    let progress = connection.advance().unwrap();
    assert!(progress.continue_now);
    assert_eq!(progress.messages, 0);
    assert!(socket.0.borrow().read_bytes - before <= io::BYTES);
    assert!(!socket.0.borrow().incoming.is_empty());
    // CREDIT from the consumed byte and FIN progress before the flood.
    let mut server = outgoing(&socket);
    assert_eq!(server.read().unwrap(), Message::Binary(vec![4, 1, 0, 0, 0, 1, 0, 0, 0].into()));
    assert_eq!(server.read().unwrap(), Message::Binary(vec![2, 1, 0, 0, 0].into()));
}
#[test]
fn invalid_messages_close_all_streams_and_drop_socket() {
    for frame in [vec![0x81, 1, b'x'], vec![0x82, 126, 0x40, 0x06], vec![0x82, 5, 9, 1, 0, 0, 0]] {
        let socket = wire();
        let mut connection = raw(socket.clone());
        let id = connection.open().unwrap();
        connection.advance().unwrap();
        socket.0.borrow_mut().incoming.extend(frame);
        assert!(connection.advance().is_err());
        assert!(connection.socket.is_none());
        assert!(connection.read(id, &mut [0]).is_err());
        assert!(connection.open().is_err());
        assert!(matches!(connection.advance(), Err(Error::Closed)));
    }
}

#[test]
fn peer_close_is_acknowledged_when_writable_and_terminates_streams() {
    let socket = wire();
    let mut connection = raw(socket.clone());
    let id = connection.open().unwrap();
    connection.advance().unwrap();
    socket.0.borrow_mut().outgoing.clear();
    incoming(&socket, Message::Close(None));
    assert!(matches!(connection.advance(), Err(Error::Closed)));
    assert_eq!(outgoing(&socket).read().unwrap(), Message::Close(None));
    assert!(connection.read(id, &mut [0]).is_err());
    assert!(connection.socket.is_none());
}

#[test]
fn fragmented_messages_cannot_exceed_the_aggregate_packet_limit() {
    let socket = wire();
    let mut connection = raw(socket.clone());
    socket.0.borrow_mut().incoming.extend([2, 126, 0x40, 0x05]);
    socket.0.borrow_mut().incoming.extend(std::iter::repeat_n(0, PACKET_MAX));
    socket.0.borrow_mut().incoming.extend([128, 1, 0]);
    assert!(matches!(connection.advance(), Err(Error::Socket(tungstenite::Error::Capacity(_)))));
    assert!(connection.socket.is_none());
}

#[cfg(feature = "ap-identity")]
#[path = "authentication_tests.rs"]
mod authentication_tests;

#[cfg(feature = "ap-identity")]
#[path = "registration_tests.rs"]
mod registration_tests;

#[path = "publication_tests.rs"]
mod publication_tests;

#[path = "subscription_tests.rs"]
mod subscription_tests;
