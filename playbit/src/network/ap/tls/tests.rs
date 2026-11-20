#![allow(
    clippy::unwrap_used,
    reason = "Tests assert verified TLS fixtures and exact failure paths."
)]
use super::*;
use rustls::{ServerConfig, ServerConnection, pki_types::PrivatePkcs8KeyDer};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
};
#[derive(Default)]
struct Trace {
    reads: usize,
    writes: usize,
    flushes: usize,
    read_bytes: usize,
    write_bytes: usize,
}
struct Socket {
    input: Rc<RefCell<VecDeque<u8>>>,
    output: Rc<RefCell<VecDeque<u8>>>,
    trace: Rc<RefCell<Trace>>,
    blocked: Rc<Cell<bool>>,
    zero_write: Rc<Cell<bool>>,
    flush_blocked: Rc<Cell<bool>>,
    eof: Rc<Cell<bool>>,
    dropped: Rc<Cell<bool>>,
    chunk: usize,
}
impl Read for Socket {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.trace.borrow_mut().reads += 1;
        let count = out.len().min(self.chunk).min(self.input.borrow().len());
        if count == 0 {
            return if self.eof.get() { Ok(0) } else { Err(blocked()) };
        }
        for byte in &mut out[..count] {
            *byte = self.input.borrow_mut().pop_front().unwrap();
        }
        self.trace.borrow_mut().read_bytes += count;
        Ok(count)
    }
}
impl Write for Socket {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.trace.borrow_mut().writes += 1;
        if self.zero_write.get() {
            return Ok(0);
        }
        if self.blocked.get() {
            return Err(blocked());
        }
        let count = input.len().min(self.chunk).min(65536 - self.output.borrow().len());
        if count == 0 {
            return Err(blocked());
        }
        self.output.borrow_mut().extend(&input[..count]);
        self.trace.borrow_mut().write_bytes += count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.trace.borrow_mut().flushes += 1;
        if self.flush_blocked.get() { Err(blocked()) } else { Ok(()) }
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
fn sockets(chunk: usize) -> (Socket, Socket) {
    let a = Rc::new(RefCell::new(VecDeque::new()));
    let b = Rc::new(RefCell::new(VecDeque::new()));
    let make = |input, output| Socket {
        input,
        output,
        chunk,
        trace: Rc::default(),
        blocked: Rc::default(),
        zero_write: Rc::default(),
        flush_blocked: Rc::default(),
        eof: Rc::default(),
        dropped: Rc::default(),
    };
    (make(a.clone(), b.clone()), make(b, a))
}
fn fixture(year: i32, alpn: bool) -> (Config, ServerConnection) {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.not_before = rcgen::date_time_ymd(year, 1, 1);
    params.not_after = rcgen::date_time_ymd(year + 10, 1, 1);
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(certificate.der().clone()).unwrap();
    let mut server =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
    server.alpn_protocols = if alpn { vec![b"http/1.1".to_vec()] } else { vec![] };
    (Config::new(roots).unwrap(), ServerConnection::new(Arc::new(server)).unwrap())
}
fn serve(server: &mut ServerConnection, socket: &mut Socket) {
    if server.wants_read() {
        match server.read_tls(socket) {
            Ok(_) => {
                server.process_new_packets().unwrap();
            }
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
        }
    }
    if server.wants_write() {
        pending(server.write_tls(socket)).unwrap();
    }
}
fn advance(client: &mut Client<Socket>, trace: &Rc<RefCell<Trace>>) -> io::Result<Progress> {
    *trace.borrow_mut() = Trace::default();
    let result = client.advance();
    let t = trace.borrow();
    assert!(t.reads <= 1 && t.writes <= 1 && t.flushes <= 1);
    assert!(t.read_bytes <= BUFFER_BYTES && t.write_bytes <= BUFFER_BYTES);
    result
}
fn connect(
    client: &mut Client<Socket>,
    server: &mut ServerConnection,
    socket: &mut Socket,
    trace: &Rc<RefCell<Trace>>,
) -> io::Result<()> {
    for _ in 0..4096 {
        let p = advance(client, trace)?;
        serve(server, socket);
        if p.ready && !server.is_handshaking() {
            return Ok(());
        }
    }
    panic!("handshake failed to make bounded progress");
}
#[test]
fn fragmented_verified_handshake_does_not_expose_early_plaintext_or_hidden_io() {
    let (config, mut server) = fixture(2020, true);
    let (socket, mut peer) = sockets(37);
    let trace = socket.trace.clone();
    let mut client = Client::new(socket, &config, "localhost").unwrap();
    assert_eq!(client.write(b"early").unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(client.read(&mut [0; 16]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(trace.borrow().reads + trace.borrow().writes, 0);
    connect(&mut client, &mut server, &mut peer, &trace).unwrap();
    *trace.borrow_mut() = Trace::default();
    assert_eq!(client.write(b"hello").unwrap(), 5);
    assert_eq!(client.flush().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(trace.borrow().reads + trace.borrow().writes + trace.borrow().flushes, 0);
    for _ in 0..100 {
        advance(&mut client, &trace).unwrap();
        serve(&mut server, &mut peer);
        let mut out = [0; 5];
        match server.reader().read(&mut out) {
            Ok(5) => {
                assert_eq!(&out, b"hello");
                return;
            }
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
            value => panic!("unexpected plaintext: {value:?}"),
        }
    }
    panic!("plaintext not delivered");
}
#[test]
fn rejects_wrong_name_expired_future_untrusted_and_missing_alpn() {
    for (year, name, trusted, alpn) in [
        (2020, "wrong.invalid", true, true),
        (2000, "localhost", true, true),
        (2040, "localhost", true, true),
        (2020, "localhost", false, true),
        (2020, "localhost", true, false),
    ] {
        let (config, mut server) = fixture(year, alpn);
        let (unrelated, _) = fixture(2020, true);
        let (socket, mut peer) = sockets(65536);
        let trace = socket.trace.clone();
        let dropped = socket.dropped.clone();
        let mut client =
            Client::new(socket, if trusted { &config } else { &unrelated }, name).unwrap();
        let error = connect(&mut client, &mut server, &mut peer, &trace).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let reason = error.get_ref().and_then(|error| error.downcast_ref::<rustls::Error>());
        use rustls::{CertificateError as Cert, Error as TlsError};
        match (year, name, trusted, alpn) {
            (_, "wrong.invalid", _, _) => assert!(matches!(
                reason,
                Some(TlsError::InvalidCertificate(Cert::NotValidForNameContext { .. }))
            )),
            (2000, _, _, _) => assert!(matches!(
                reason,
                Some(TlsError::InvalidCertificate(Cert::ExpiredContext { .. }))
            )),
            (2040, _, _, _) => assert!(matches!(
                reason,
                Some(TlsError::InvalidCertificate(Cert::NotValidYetContext { .. }))
            )),
            (_, _, false, _) => {
                // The unrelated fixture has the same issuer DN and a different key.
                assert!(
                    matches!(reason, Some(TlsError::InvalidCertificate(Cert::BadSignature))),
                    "{error:?}"
                )
            }
            (_, _, _, false) => assert!(error.to_string().contains("negotiate HTTP/1.1")),
            _ => unreachable!(),
        }
        assert!(dropped.get());
        assert_eq!(client.advance().unwrap_err().kind(), io::ErrorKind::NotConnected);
        assert_eq!(client.read(&mut [0; 1]).unwrap_err().kind(), io::ErrorKind::NotConnected);
        assert_eq!(client.write(b"x").unwrap_err().kind(), io::ErrorKind::NotConnected);
    }
}
#[test]
fn buffered_partial_writes_survive_backpressure_exactly_once() {
    let (config, mut server) = fixture(2020, true);
    let (socket, mut peer) = sockets(113);
    let trace = socket.trace.clone();
    let blocked = socket.blocked.clone();
    let mut client = Client::new(socket, &config, "localhost").unwrap();
    connect(&mut client, &mut server, &mut peer, &trace).unwrap();
    blocked.set(true);
    let input: Vec<_> = (0..50000).map(|i| (i % 251) as u8).collect();
    let mut sent = 0;
    while let Ok(count) = client.write(&input[sent..]) {
        assert!(count > 0);
        sent += count;
        assert!(sent <= BUFFER_BYTES);
    }
    let p = advance(&mut client, &trace).unwrap();
    assert!(p.wants_write);
    assert_eq!(client.flush().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    blocked.set(false);
    let mut output = Vec::new();
    for _ in 0..10000 {
        if sent < input.len() {
            match client.write(&input[sent..]) {
                Ok(n) => sent += n,
                Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
            }
        }
        advance(&mut client, &trace).unwrap();
        serve(&mut server, &mut peer);
        let mut bytes = [0; 4096];
        match server.reader().read(&mut bytes) {
            Ok(n) => output.extend_from_slice(&bytes[..n]),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
        }
        assert!(output.len() <= input.len());
        if output.len() == input.len() {
            assert_eq!(output, input);
            return;
        }
    }
    panic!("partial output never completed");
}
#[test]
fn flush_readiness_and_clean_close_differ_from_truncated_tcp() {
    for clean in [false, true] {
        let (config, mut server) = fixture(2020, true);
        let (socket, mut peer) = sockets(65536);
        let trace = socket.trace.clone();
        let eof = socket.eof.clone();
        let flush_blocked = socket.flush_blocked.clone();
        let mut client = Client::new(socket, &config, "localhost").unwrap();
        connect(&mut client, &mut server, &mut peer, &trace).unwrap();
        assert_eq!(client.write(b"a").unwrap(), 1);
        flush_blocked.set(true);
        assert!(advance(&mut client, &trace).unwrap().wants_write);
        assert_eq!(client.flush().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        flush_blocked.set(false);
        advance(&mut client, &trace).unwrap();
        serve(&mut server, &mut peer);
        if clean {
            server.send_close_notify();
            serve(&mut server, &mut peer);
            assert!(advance(&mut client, &trace).unwrap().peer_closed);
            assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
            assert_eq!(client.write(b"b").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        } else {
            eof.set(true);
            assert_eq!(
                advance(&mut client, &trace).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
            assert_eq!(client.advance().unwrap_err().kind(), io::ErrorKind::NotConnected);
        }
        client.close();
        assert_eq!(client.advance().unwrap_err().kind(), io::ErrorKind::NotConnected);
    }
}
#[test]
fn trust_name_and_socket_admission_are_bounded() {
    assert!(Config::new(RootCertStore::empty()).is_err());
    let (config, _) = fixture(2020, true);
    for name in ["".to_owned(), "a".repeat(254), "bad/name".to_owned()] {
        let (socket, _) = sockets(65536);
        let trace = socket.trace.clone();
        assert!(Client::new(socket, &config, &name).is_err());
        assert_eq!(trace.borrow().reads + trace.borrow().writes, 0);
    }
    let (mut socket, _) = sockets(65536);
    let trace = socket.trace.clone();
    let mut limited = Limited::new(&mut socket);
    assert_eq!(limited.write(&vec![0; 32768]).unwrap(), 16384);
    assert_eq!(limited.write(b"x").unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(trace.borrow().writes, 1);
}

#[test]
fn plaintext_progress_rearms_readiness_without_idle_self_wakes() {
    let (config, mut server) = fixture(2020, true);
    let (socket, mut peer) = sockets(65536);
    let trace = socket.trace.clone();
    let mut client = Client::new(socket, &config, "localhost").unwrap();
    connect(&mut client, &mut server, &mut peer, &trace).unwrap();
    for _ in 0..4 {
        advance(&mut client, &trace).unwrap();
        serve(&mut server, &mut peer);
    }
    assert!(!client.progress().unwrap().continue_now);
    server.writer().write_all(b"ab").unwrap();
    serve(&mut server, &mut peer);
    assert!(!advance(&mut client, &trace).unwrap().wants_read);
    assert!(!advance(&mut client, &trace).unwrap().continue_now);
    *trace.borrow_mut() = Trace::default();
    let mut out = [0; 1];
    assert_eq!(client.read(&mut out).unwrap(), 1);
    assert_eq!(&out, b"a");
    assert!(client.progress().unwrap().continue_now);
    assert!(!client.progress().unwrap().wants_read);
    assert_eq!(client.read(&mut out).unwrap(), 1);
    assert_eq!(&out, b"b");
    assert!(client.progress().unwrap().wants_read);
    assert!(client.progress().unwrap().continue_now);
    assert_eq!(trace.borrow().reads + trace.borrow().writes, 0);
    assert!(!advance(&mut client, &trace).unwrap().continue_now);
}
#[test]
fn zero_socket_write_is_terminal_instead_of_a_busy_loop() {
    let (config, _) = fixture(2020, true);
    let (socket, _) = sockets(65536);
    socket.zero_write.set(true);
    let dropped = socket.dropped.clone();
    let mut client = Client::new(socket, &config, "localhost").unwrap();
    assert_eq!(client.advance().unwrap_err().kind(), io::ErrorKind::WriteZero);
    assert!(dropped.get());
    assert_eq!(client.progress().unwrap_err().kind(), io::ErrorKind::NotConnected);
}

#[test]
fn oversized_record_and_trust_store_are_rejected() {
    let params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let mut roots = RootCertStore::empty();
    for _ in 0..ROOTS_MAX {
        roots.add(certificate.der().clone()).unwrap();
    }
    assert!(Config::new(roots.clone()).is_ok());
    roots.add(certificate.der().clone()).unwrap();
    assert!(Config::new(roots).is_err());
    let (config, _) = fixture(2020, true);
    let (socket, _) = sockets(65536);
    socket.input.borrow_mut().extend([22, 3, 3, 255, 255]); // A 65,535-byte TLS record.
    let trace = socket.trace.clone();
    let dropped = socket.dropped.clone();
    let mut client = Client::new(socket, &config, "localhost").unwrap();
    assert_eq!(advance(&mut client, &trace).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(dropped.get());
}
