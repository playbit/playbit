//! Transport fixtures for this crate's and dependent crates' tests: a real
//! (loopback) AP server, and a gate that holds back a connection's close flush.
//!
//! Gated behind the `test-support` feature so none of it reaches a production
//! build; a dependent enables the feature only in its own `[dev-dependencies]`.
use crate::network::ap::{ApConnection, ClientConfig, Frame, frame_type};
use rustls::pki_types::PrivateKeyDer;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

pub(crate) fn authentication_wire(frame: Frame) -> Vec<u8> {
    super::timing::auth::encode(frame, &super::timing::Timings::default())
        .expect("bounded authentication fixture envelope")
        .encode()
        .expect("fixture frame has valid bounded fields")
}

/// The connections whose close flush is currently held, by gate key.
static HELD: Mutex<BTreeMap<usize, Arc<tokio::sync::Semaphore>>> = Mutex::new(BTreeMap::new());

/// Hold back the bounded drain of `ApConnection::wait_closed` for one
/// connection until the returned guard is dropped. A test uses this to show
/// ordering — that a synchronous caller returned, and that other work ran,
/// while the drain could not finish — instead of measuring elapsed time.
/// Only the given connection is held, so other tests in the same binary keep
/// closing their own connections normally.
pub fn hold_close_flush(connection: &ApConnection) -> CloseFlushHold {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let key = connection.close_flush_key();
    HELD.lock().expect("test close-flush registry is not poisoned").insert(key, gate.clone());
    CloseFlushHold { key, gate }
}

/// Releases the held close flush when dropped, including on a panic.
pub struct CloseFlushHold {
    key: usize,
    gate: Arc<tokio::sync::Semaphore>,
}
impl Drop for CloseFlushHold {
    fn drop(&mut self) {
        HELD.lock().expect("test close-flush registry is not poisoned").remove(&self.key);
        // Closing wakes the waiting drain with an error; that is the release.
        self.gate.close();
    }
}

pub(crate) async fn held_close_flush(key: usize) {
    let gate = HELD.lock().expect("test close-flush registry is not poisoned").get(&key).cloned();
    if let Some(gate) = gate {
        let _ = gate.acquire().await;
    }
}

/// The fixture returned by `silent_authenticating_server`.
pub struct SilentAp {
    /// Ready to connect to the fixture.
    pub config: ClientConfig,
    /// Reports once a post-authentication request is held without a reply.
    pub request_held: mpsc::Receiver<()>,
    /// Reports whether the server then observed its peer's connection close.
    pub closed: mpsc::Receiver<bool>,
}

/// A loopback QUIC server speaking the AP's ALPN, with a client configuration
/// that trusts its self-signed certificate.
pub(crate) fn loopback_ap() -> (tokio::runtime::Runtime, quinn::Endpoint, ClientConfig) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime construction succeeds");
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("localhost certificate generation succeeds");
    let certificate = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![certificate.clone()], key)
    .expect("generated certificate matches its private key");
    crypto.alpn_protocols = vec![b"pb.ap".to_vec()];
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
            .expect("default TLS versions include TLS 1.3"),
    ));
    let endpoint = runtime
        .block_on(async {
            quinn::Endpoint::server(
                server_config,
                "127.0.0.1:0".parse().expect("literal loopback socket address"),
            )
        })
        .expect("loopback fixture can bind an ephemeral UDP port");
    let config = ClientConfig::new(
        endpoint.local_addr().expect("bound endpoint has a local address"),
        "localhost",
        vec![certificate],
    );
    (runtime, endpoint, config)
}

/// Authenticates a fixture account with a recorded agreement, then holds its
/// first data request until the client disconnects.
pub fn silent_authenticating_server() -> SilentAp {
    let (runtime, endpoint, mut config) = loopback_ap();
    let (results, closed) = mpsc::channel();
    let (served, request_held) = mpsc::channel();
    std::thread::spawn(move || {
        runtime.block_on(async move {
            let Some(incoming) = endpoint.accept().await else {
                return;
            };
            let Ok(connection) = incoming.await else {
                return;
            };
            let Ok((mut send, _receive)) = connection.accept_bi().await else {
                return;
            };
            // The client already sent its HELLO by the time it opened this
            // stream and is only waiting on our reply; nothing needs reading.
            // Flags 0 is the client's HELLO_VALID: authenticate unconditionally.
            let hello_valid = Frame::new(frame_type::HELLO, 0, Vec::new(), Vec::new(), Vec::new())
                .expect("empty HELLO frame is valid");
            if send.write_all(&authentication_wire(hello_valid)).await.is_err() {
                return;
            }
            let observed = tokio::time::timeout(Duration::from_secs(5), async {
                if let Some((send, receive, _)) = next_request_with_receipt(&connection).await {
                    let _ = served.send(());
                    connection.closed().await;
                    // Keep the unanswered request alive until the peer closes.
                    drop((send, receive));
                } else {
                    connection.closed().await;
                }
            })
            .await
            .is_ok();
            let _ = results.send(observed);
        });
    });

    config.authentication_timeout = Duration::from_secs(2);
    SilentAp { config, request_held, closed }
}

/// Whether a held server may answer the frames it has read. Dropping it opens
/// the gate, so a test that fails early never leaves a server task waiting.
pub struct ReplyGate(tokio::sync::watch::Sender<bool>);
impl ReplyGate {
    pub fn open(&self) {
        self.0.send_replace(true);
    }
    pub fn close(&self) {
        self.0.send_replace(false);
    }
}
impl Drop for ReplyGate {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// An AP that accepts connections and reads each client's HELLO, but answers
/// it only while the returned gate is open. A test uses this to hold a client
/// inside authentication — the one exchange a cancellation cannot reach by
/// closing a connection, because the connection is not established yet — and
/// the channel reports each HELLO the server has read, so the test acts on
/// the server's own observation instead of on elapsed time. The gate starts
/// closed. The authentication timeout is left long on purpose: a client that
/// returns from a held exchange did so because it was canceled.
pub fn held_authentication_server() -> (ClientConfig, mpsc::Receiver<()>, ReplyGate) {
    let (runtime, endpoint, mut config) = loopback_ap();
    let (results, observed) = mpsc::channel();
    let gate = tokio::sync::watch::channel(false).0;
    let served = gate.clone();
    std::thread::spawn(move || {
        runtime.block_on(async move {
            while let Some(incoming) = endpoint.accept().await {
                let results = results.clone();
                let mut open = served.subscribe();
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    let Ok((mut send, mut receive)) = connection.accept_bi().await else {
                        return;
                    };
                    // Read the HELLO itself, so the report means the exchange
                    // reached this server and is waiting only on its reply.
                    let mut hello = [0; 4096];
                    let Ok(Some(_)) = receive.read(&mut hello).await else {
                        return;
                    };
                    // A test that has stopped listening still gets served.
                    let _ = results.send(());
                    if open.wait_for(|open| *open).await.is_err() {
                        return;
                    }
                    // Flags 0 is the client's HELLO_VALID: authenticate unconditionally.
                    let valid =
                        Frame::new(frame_type::HELLO, 0, Vec::new(), Vec::new(), Vec::new())
                            .expect("empty HELLO frame is valid");
                    if send.write_all(&authentication_wire(valid)).await.is_err() {
                        return;
                    }
                    connection.closed().await;
                });
            }
        });
    });

    config.authentication_timeout = Duration::from_secs(30);
    (config, observed, ReplyGate(gate))
}

/// What a `held_registration_server` answers a sign-in code with: exactly the
/// durable credentials a real AP mints for a new device.
#[derive(Clone)]
pub struct IssuedCredentials {
    /// Exactly 72 bytes, the first 16 of which are the account id.
    pub auth_token: Vec<u8>,
    pub instance_id: Vec<u8>,
    pub ca_certificate: Vec<u8>,
    pub instance_certificate: Vec<u8>,
}
impl IssuedCredentials {
    fn body(&self) -> Vec<u8> {
        let mut body = vec![self.auth_token.len() as u8, self.instance_id.len() as u8];
        body.extend_from_slice(&(self.ca_certificate.len() as u16).to_le_bytes());
        body.extend_from_slice(&self.auth_token);
        body.extend_from_slice(&self.instance_id);
        body.extend_from_slice(&self.ca_certificate);
        body.extend_from_slice(&self.instance_certificate);
        body
    }
}

/// Read exactly one AP frame, leaving any following upload bytes in the stream.
pub(crate) async fn read_frame(receive: &mut quinn::RecvStream) -> Option<Frame> {
    let mut reader = super::receive::FrameReader::new(super::MAX_FRAME_LEN);
    loop {
        let buffer = reader.buffer().ok()?;
        let count = buffer.len();
        receive.read_exact(buffer).await.ok()?;
        if reader.advance(count).ok()? {
            return reader.finish().ok();
        }
    }
}

/// These data-transfer fixtures represent accounts with a receipt for the current document.
pub(crate) async fn next_request_with_receipt(
    connection: &quinn::Connection,
) -> Option<(quinn::SendStream, quinn::RecvStream, Frame)> {
    let revision = super::registration::TERMS_REVISION;
    loop {
        let (mut send, mut receive) = connection.accept_bi().await.ok()?;
        let mut request = read_frame(&mut receive).await?;
        if request.frame_type != frame_type::AGREEMENT {
            return Some((send, receive, request));
        }
        assert!(request.source.is_empty() && request.destination.is_empty());
        assert!(request.flags == 0 || request.flags == revision);
        assert_eq!(request.body.as_slice(), super::registration::terms_digest());
        request.flags = revision;
        send.write_all(&request.encode().expect("fixture agreement reply fits")).await.ok()?;
        send.finish().ok()?;
    }
}

/// An AP that runs the email-code registration: it answers the email request
/// at once, and holds the answer to the code submission — the exchange that
/// mints `issued` — until the returned gate is opened. The channel reports
/// when the server has read that submission, so a test acts on the server's
/// own observation instead of on elapsed time, and knows the AP has by then
/// committed credentials the device has not saved yet. The gate starts closed.
pub fn held_registration_server(
    issued: IssuedCredentials,
) -> (ClientConfig, mpsc::Receiver<()>, ReplyGate) {
    let (runtime, endpoint, mut config) = loopback_ap();
    let (results, observed) = mpsc::channel();
    let gate = tokio::sync::watch::channel(false).0;
    let served = gate.clone();
    std::thread::spawn(move || {
        runtime.block_on(async move {
            while let Some(incoming) = endpoint.accept().await {
                let (results, issued) = (results.clone(), issued.clone());
                let mut open = served.subscribe();
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    let Ok((mut send, mut receive)) = connection.accept_bi().await else {
                        return;
                    };
                    let Some(request) = read_frame(&mut receive).await else {
                        return;
                    };
                    // A registration body is the email, instance name and
                    // challenge lengths, then those fields and the CSR. A
                    // challenge present means this is the code submission.
                    let reply = if request.body.get(3).is_some_and(|length| *length > 0) {
                        // A test that has stopped listening still gets served.
                        let _ = results.send(());
                        if open.wait_for(|open| *open).await.is_err() {
                            return;
                        }
                        // Flags 1 is the client's REGISTER_SUCCESS.
                        Frame::new(frame_type::REGISTER, 1, Vec::new(), Vec::new(), issued.body())
                    } else {
                        // Flags 0 is REGISTER_IN_PROGRESS, and an empty
                        // instance id means the code arrives out of band.
                        Frame::new(frame_type::REGISTER, 0, Vec::new(), Vec::new(), vec![0; 4])
                    };
                    if send
                        .write_all(&authentication_wire(
                            reply.expect("registration fixture fields fit the frame"),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    connection.closed().await;
                });
            }
        });
    });

    // Long on purpose: the held exchange must outlive a whole orderly exit.
    config.authentication_timeout = Duration::from_secs(60);
    (config, observed, ReplyGate(gate))
}
