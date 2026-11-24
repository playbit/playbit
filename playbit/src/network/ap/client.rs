use std::{
    io::BufReader,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use quinn::{Endpoint, RecvStream, SendStream, crypto::rustls::QuicClientConfig};
use rustls::pki_types::CertificateDer;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::time::timeout;

use crate::network::ap::{
    ClientError, ClientIdentity, Frame, MAX_FRAME_LEN, RegistrationChallenge, authentication,
    frame_type, receive::FrameReader, registration,
};

const APPLICATION_ERROR_BYE: u32 = 0x1000b7e;

#[derive(Clone)]
pub struct ClientConfig {
    pub testing: Option<super::hello::TestingCredentials>,
    pub server_address: SocketAddr,
    pub server_name: String,
    trust: ServerTrust,
    pub connect_timeout: Duration,
    /// How long the authentication or registration exchange on a fresh
    /// connection may take before the attempt is abandoned. QUIC's idle
    /// timeout does not cover this: a peer that acknowledges packets but
    /// never answers the exchange keeps the connection alive indefinitely.
    pub authentication_timeout: Duration,
    /// How long a service request may go unanswered before the caller gives
    /// up. Same hazard as the authentication exchange: an AP that answers
    /// keepalives but never the request would otherwise hold a caller forever.
    pub response_timeout: Duration,
    /// QUIC inactivity bound; protocol recovery may extend it to three probe
    /// timeouts on a high-latency path.
    pub idle_timeout: Duration,
    pub keep_alive_interval: Duration,
}

#[derive(Clone)]
enum ServerTrust {
    Platform,
    Certificates(Vec<CertificateDer<'static>>),
}

impl ClientConfig {
    /// Use the operating system's certificate verifier, including Apple's
    /// Security.framework on iOS where no PEM trust bundle is exposed.
    pub fn platform(server_address: SocketAddr, server_name: impl Into<String>) -> Self {
        let mut config = Self::new(server_address, server_name, Vec::new());
        config.trust = ServerTrust::Platform;
        config
    }

    /// Explicit roots for a caller-owned trust domain, such as an isolated AP
    /// integration test. An empty list remains an empty trust store.
    pub fn new(
        server_address: SocketAddr,
        server_name: impl Into<String>,
        root_certificates: Vec<CertificateDer<'static>>,
    ) -> Self {
        Self {
            testing: None,
            server_address,
            server_name: server_name.into(),
            trust: ServerTrust::Certificates(root_certificates),
            connect_timeout: Duration::from_secs(5),
            authentication_timeout: Duration::from_secs(15),
            response_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(5),
            keep_alive_interval: Duration::from_secs(1),
        }
    }
}

pub struct ApConnection {
    endpoint: Endpoint,
    connection: quinn::Connection,
    instance_id: Vec<u8>,
    #[cfg(feature = "app-host")]
    publisher_issuer: Option<[u8; 32]>,
    response_timeout: Duration,
    discovery: Discovery,
}

#[derive(Default)]
struct Discovery {
    description: tokio::sync::OnceCell<super::service::ServiceDescription>,
    #[cfg(feature = "connected-engine")]
    ready: tokio::sync::Notify,
    #[cfg(feature = "connected-engine")]
    session_names: tokio::sync::OnceCell<bool>,
}

struct AuthenticationConnection {
    endpoint: Endpoint,
    connection: quinn::Connection,
    stream: ApStream,
}

impl ApConnection {
    /// Verify the ordinary AP transport before presenting native enrollment.
    /// No identity or authentication frame is sent; registration itself remains
    /// stateless between the user's email and code submissions.
    pub async fn prepare_registration(config: &ClientConfig) -> Result<(), ClientError> {
        let authentication = open_authentication_connection(config, None).await?;
        authentication.connection.close(quinn::VarInt::from_u32(APPLICATION_ERROR_BYE), b"bye");
        Ok(())
    }

    pub async fn connect(
        config: &ClientConfig,
        identity: &mut ClientIdentity,
    ) -> Result<Self, ClientError> {
        Self::connect_inner(config, identity, true, None).await
    }

    /// Connects only with an already-issued AP instance identity.
    ///
    /// Missing, invalid, or revoked tokens fail instead of entering the email
    /// challenge registration flow. Hosted and other unattended processes use
    /// this boundary so they cannot accidentally register as a user's device.
    pub async fn connect_authenticated(
        config: &ClientConfig,
        identity: &mut ClientIdentity,
    ) -> Result<Self, ClientError> {
        if config.testing.is_none() {
            identity.validate_preissued()?;
        }
        Self::connect_inner(config, identity, false, None).await
    }

    /// Reuses the UDP endpoint so it can reject packets for expired connections
    /// while the relay catches up after an outage.
    pub async fn reconnect_authenticated(
        &self,
        config: &ClientConfig,
        identity: &mut ClientIdentity,
    ) -> Result<Self, ClientError> {
        if config.testing.is_none() {
            identity.validate_preissued()?;
        }
        Self::connect_inner(config, identity, false, Some(self.endpoint.clone())).await
    }

    /// Starts registration and asks the AP to send a sign-in code.
    ///
    /// AP registration is stateless between requests, so the caller can wait
    /// for the user and submit the code later on a new QUIC connection.
    pub async fn request_registration(
        config: &ClientConfig,
        identity: &ClientIdentity,
    ) -> Result<RegistrationChallenge, ClientError> {
        authentication::validate_registration_identity(identity)?;
        let mut authentication = open_authentication_connection(config, None).await?;
        let challenge =
            bounded(config, request_registration(&mut authentication.stream, identity)).await?;
        authentication.stream.finish()?;
        Ok(RegistrationChallenge { automatic_challenge: challenge })
    }

    /// Completes registration with the out-of-band code and returns an
    /// authenticated connection.
    pub async fn finish_registration(
        config: &ClientConfig,
        identity: &mut ClientIdentity,
        challenge: &str,
    ) -> Result<Self, ClientError> {
        authentication::validate_registration_identity(identity)?;
        if challenge.is_empty() {
            return Err(ClientError::MissingIdentityField("registration challenge"));
        }
        let mut authentication = open_authentication_connection(config, None).await?;
        bounded(config, finish_registration(&mut authentication.stream, identity, challenge))
            .await?;
        authentication.stream.finish()?;
        Ok(Self {
            endpoint: authentication.endpoint,
            connection: authentication.connection,
            discovery: Discovery::default(),
            instance_id: identity.instance_id.clone(),
            #[cfg(feature = "app-host")]
            publisher_issuer: super::publisher::issuer_key_from_certificate(
                &identity.ca_certificate,
            )
            .ok(),
            response_timeout: config.response_timeout,
        })
    }

    /// Proves account-key possession on the same bounded authentication stream.
    pub async fn sign_in_with_key(
        config: &ClientConfig,
        identity: &mut ClientIdentity,
        seed: &[u8; 16],
        account: Option<crate::store::Id>,
    ) -> Result<Self, ClientError> {
        if account.is_none() {
            authentication::validate_registration_identity(identity)?;
        }
        let mut authentication = open_authentication_connection(config, None).await?;
        bounded(
            config,
            finish_key_registration(&mut authentication.stream, identity, seed, account),
        )
        .await?;
        authentication.stream.finish()?;
        Ok(Self {
            endpoint: authentication.endpoint,
            connection: authentication.connection,
            discovery: Discovery::default(),
            instance_id: identity.instance_id.clone(),
            #[cfg(feature = "app-host")]
            publisher_issuer: super::publisher::issuer_key_from_certificate(
                &identity.ca_certificate,
            )
            .ok(),
            response_timeout: config.response_timeout,
        })
    }

    async fn connect_inner(
        config: &ClientConfig,
        identity: &mut ClientIdentity,
        allow_registration: bool,
        endpoint: Option<Endpoint>,
    ) -> Result<Self, ClientError> {
        if config.testing.is_none() && allow_registration && identity.email.is_empty() {
            return Err(ClientError::MissingIdentityField("email"));
        }
        if identity.instance_name.is_empty() {
            return Err(ClientError::MissingIdentityField("instance name"));
        }

        let mut authentication = open_authentication_connection(config, endpoint).await?;
        bounded(
            config,
            authenticate(
                &mut authentication.stream,
                identity,
                allow_registration,
                config.testing.as_ref(),
            ),
        )
        .await?;
        authentication.stream.finish()?;

        Ok(Self {
            endpoint: authentication.endpoint,
            connection: authentication.connection,
            discovery: Discovery::default(),
            instance_id: identity.instance_id.clone(),
            #[cfg(feature = "app-host")]
            publisher_issuer: super::publisher::issuer_key_from_certificate(
                &identity.ca_certificate,
            )
            .ok(),
            response_timeout: config.response_timeout,
        })
    }

    /// Lazily discover optional support on this authenticated connection only.
    /// Baseline operations and local rendering never depend on this request.
    pub async fn describe(&self) -> super::service::Result<&super::service::ServiceDescription> {
        if let Some(description) = self.discovery.description.get() {
            return Ok(description);
        }
        let result = self
            .discovery
            .description
            .get_or_try_init(|| async {
                tokio::time::timeout(self.response_timeout(), async {
                    let request = super::operation::ServiceRequest::Describe {
                        operation_id: crate::store::Id::generate()?.to_string(),
                    };
                    let mut stream = self.open_stream().await?;
                    stream.send(&super::service::request(&request)?).await?;
                    stream.finish()?;
                    let response = super::service::response(
                        stream.receive_with_body_limit(super::service::DESCRIPTION_LIMIT).await?,
                        request.operation_id(),
                    )?;
                    let description: super::service::ServiceDescription = serde_json::from_value(
                        response.data.ok_or("AP discovery response lacks data")?,
                    )?;
                    description.validate()?;
                    Ok(description)
                })
                .await
                .map_err(|_| ClientError::ResponseTimeout)?
            })
            .await;
        #[cfg(feature = "connected-engine")]
        if result.is_ok() {
            self.discovery.ready.notify_waiters();
        }
        result
    }

    /// A routing join can select its first-frame shape without waiting for ordinary admission.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn cached_session_names(&self) -> Option<bool> {
        self.discovery
            .description
            .get()
            .map(|d| d.features.iter().any(|f| f == super::channel::DISPLAY_NAMES_FEATURE))
            .or_else(|| self.discovery.session_names.get().copied())
    }

    /// All active members on this connection share a single pending discovery and retry delay.
    /// Dropping the last waiting membership cancels that work along with its control stream.
    #[cfg(feature = "connected-engine")]
    pub(crate) async fn discover_session_names(&self) -> bool {
        *self
            .discovery
            .session_names
            .get_or_init(|| async {
                let mut delay = Duration::from_millis(250);
                loop {
                    match self.describe().await {
                        Ok(d) => {
                            return d
                                .features
                                .iter()
                                .any(|f| f == super::channel::DISPLAY_NAMES_FEATURE);
                        }
                        Err(error) => {
                            let retry = error
                                .downcast_ref::<super::service::ServiceFailure>()
                                .is_some_and(super::service::ServiceFailure::is_capacity_exhausted)
                                || matches!(
                                    error.downcast_ref::<ClientError>(),
                                    Some(
                                        ClientError::ResponseTimeout
                                            | ClientError::Read(_)
                                            | ClientError::Write(_)
                                            | ClientError::ClosedStream(_)
                                    )
                                );
                            if !retry || self.is_closed() {
                                return false;
                            }
                        }
                    }
                    // Another operation may finish discovery while a member is backing off.
                    let ready = self.discovery.ready.notified();
                    if self.discovery.description.get().is_none() {
                        tokio::select! {
                            _ = ready => {},
                            _ = tokio::time::sleep(delay) => {},
                        }
                    }
                    delay = (delay * 2).min(Duration::from_secs(8));
                }
            })
            .await
    }

    pub fn instance_id(&self) -> &[u8] {
        &self.instance_id
    }

    /// Issuer pinned by the identity retained after successful AP authentication.
    #[cfg(feature = "app-host")]
    pub(crate) fn publisher_issuer(&self) -> Option<[u8; 32]> {
        self.publisher_issuer
    }

    pub(crate) fn response_timeout(&self) -> Duration {
        self.response_timeout
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn channel_device(
        &self,
    ) -> Result<[u8; 16], Box<dyn std::error::Error + Send + Sync>> {
        Ok(crate::identifier::decode(std::str::from_utf8(&self.instance_id)?)?)
    }
    /// Query the authenticated account or record its affirmative acceptance of
    /// the exact document bundled with this client. Validate the echoed digest.
    pub async fn agreement(&self, accept: bool) -> Result<bool, ClientError> {
        let digest = registration::terms_digest();
        let revision = registration::TERMS_REVISION;
        let frame =
            Frame::new(frame_type::AGREEMENT, if accept { revision } else { 0 }, [], [], digest)?;
        let reply = timeout(self.response_timeout, async {
            let mut stream = self.open_stream().await?;
            stream.send(&frame).await?;
            stream.finish()?;
            stream.answer_with_body_limit(digest.len()).await
        })
        .await
        .map_err(|_| ClientError::ResponseTimeout)??;
        if reply.frame_type != frame_type::AGREEMENT
            || !reply.source.is_empty()
            || !reply.destination.is_empty()
            || reply.body.as_slice() != digest
            || (reply.flags != revision && (accept || reply.flags != 0))
        {
            return Err(ClientError::InvalidAgreementResponse);
        }
        Ok(reply.flags == revision)
    }

    pub async fn open_stream(&self) -> Result<ApStream, ClientError> {
        let (send, receive) = self.connection.open_bi().await?;
        Ok(ApStream { send, receive, response_timeout: self.response_timeout })
    }

    pub async fn closed(&self) -> quinn::ConnectionError {
        self.connection.closed().await
    }

    pub fn is_closed(&self) -> bool {
        self.connection.close_reason().is_some()
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn close_reason(&self) -> Option<quinn::ConnectionError> {
        self.connection.close_reason()
    }

    /// Close the account transport even while background tasks hold shared
    /// handles. It takes effect at once: every shared handle refuses new
    /// operations from here on. Sending the CONNECTION_CLOSE frame it queues
    /// still needs the endpoint polled, which is what `wait_closed` does.
    pub fn shutdown(&self) {
        self.connection.close(quinn::VarInt::from_u32(APPLICATION_ERROR_BYE), b"bye");
    }

    /// Give the endpoint a bounded chance to send the CONNECTION_CLOSE frame
    /// that `shutdown` queued. A caller whose runtime disappears right after
    /// `shutdown` would otherwise sometimes drop that frame unsent, leaving
    /// the AP to notice the disconnect only at its own idle timeout. The wait
    /// is capped so a dead network cannot stall the caller.
    pub async fn wait_closed(&self, bound: Duration) {
        #[cfg(feature = "test-support")]
        crate::network::ap::testing::held_close_flush(self.close_flush_key()).await;
        let _ = timeout(bound, self.endpoint.wait_idle()).await;
    }

    /// Identifies this connection to the test-only close-flush gate.
    #[cfg(feature = "test-support")]
    pub(crate) fn close_flush_key(&self) -> usize {
        self.connection.stable_id()
    }

    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }

    /// Drain the close packet before a short-lived registration executor exits.
    pub async fn close(self) {
        self.connection.close(quinn::VarInt::from_u32(APPLICATION_ERROR_BYE), b"bye");
        self.endpoint.wait_idle().await;
    }
}

async fn open_authentication_connection(
    config: &ClientConfig,
    endpoint: Option<Endpoint>,
) -> Result<AuthenticationConnection, ClientError> {
    let endpoint = match endpoint {
        Some(endpoint) => endpoint,
        None => create_endpoint(config)?,
    };
    let connecting = endpoint.connect(config.server_address, &config.server_name)?;
    let connection = timeout(config.connect_timeout, connecting)
        .await
        .map_err(|_| ClientError::ConnectTimeout)??;
    let (send, receive) = timeout(config.connect_timeout, connection.open_bi())
        .await
        .map_err(|_| ClientError::OpenStreamTimeout)??;
    Ok(AuthenticationConnection {
        endpoint,
        connection,
        stream: ApStream { send, receive, response_timeout: config.authentication_timeout },
    })
}

/// Run one authentication or registration exchange under the configured
/// bound. Dropping the exchange on timeout leaves the stream unanswered; the
/// caller closes the connection with it.
async fn bounded<T>(
    config: &ClientConfig,
    exchange: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    timeout(config.authentication_timeout, exchange)
        .await
        .map_err(|_| ClientError::AuthenticationTimeout)?
}

fn create_endpoint(config: &ClientConfig) -> Result<Endpoint, ClientError> {
    let crypto = tls_client_config(config)?;
    let quic_crypto = QuicClientConfig::try_from(crypto)
        .map_err(|error| ClientError::TlsConfiguration(error.to_string()))?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    let mut transport = super::quic_transport_config();
    transport.keep_alive_interval(Some(config.keep_alive_interval));
    transport.max_idle_timeout(Some(
        config.idle_timeout.try_into().map_err(|_| ClientError::InvalidIdleTimeout)?,
    ));
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

fn tls_client_config(config: &ClientConfig) -> Result<rustls::ClientConfig, ClientError> {
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?;
    let builder = match &config.trust {
        ServerTrust::Platform => builder.with_platform_verifier()?,
        ServerTrust::Certificates(certificates) => {
            let mut roots = rustls::RootCertStore::empty();
            for certificate in certificates {
                roots.add(certificate.clone())?;
            }
            builder.with_root_certificates(roots)
        }
    };
    let mut crypto = builder.with_no_client_auth();
    crypto.alpn_protocols = vec![b"pb.ap".to_vec()];
    Ok(crypto)
}

impl Drop for ApConnection {
    fn drop(&mut self) {
        self.connection.close(quinn::VarInt::from_u32(APPLICATION_ERROR_BYE), b"bye");
    }
}

pub struct ApStream {
    send: SendStream,
    receive: RecvStream,
    response_timeout: Duration,
}

impl ApStream {
    pub fn response_timeout(&self) -> Duration {
        self.response_timeout
    }

    /// Abandon only this operation and release queued transport data. Dropping
    /// an unfinished SendStream alone can retain data while attempting FIN.
    pub(crate) fn cancel(&mut self) {
        let _ = self.send.reset(quinn::VarInt::from_u32(0));
        let _ = self.receive.stop(quinn::VarInt::from_u32(0));
    }

    pub fn split(self) -> (ApSendStream, ApReceiveStream) {
        (
            ApSendStream { send: self.send },
            ApReceiveStream { receive: self.receive, response_timeout: self.response_timeout },
        )
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<(), ClientError> {
        self.send.write_all(&frame.encode()?).await?;
        Ok(())
    }

    pub async fn receive(&mut self) -> Result<Frame, ClientError> {
        receive_frame(&mut self.receive).await
    }

    /// Reject an oversized body from its header before allocating or reading it.
    pub async fn receive_with_body_limit(&mut self, max_body: usize) -> Result<Frame, ClientError> {
        receive_frame_with_limit(&mut self.receive, max_body).await
    }

    /// Wait for the answer to a request already sent on this stream. Unlike
    /// `receive`, which a listener may sit in indefinitely, an answer that
    /// never arrives has to give up.
    pub async fn answer(&mut self) -> Result<Frame, ClientError> {
        timeout(self.response_timeout, self.receive())
            .await
            .map_err(|_| ClientError::ResponseTimeout)?
    }

    /// A bounded answer retains the normal inter-frame deadline and rejects
    /// oversized control/evidence payloads before allocation.
    pub async fn answer_with_body_limit(&mut self, max_body: usize) -> Result<Frame, ClientError> {
        timeout(self.response_timeout, self.receive_with_body_limit(max_body))
            .await
            .map_err(|_| ClientError::ResponseTimeout)?
    }

    pub fn finish(&mut self) -> Result<(), ClientError> {
        self.send.finish()?;
        Ok(())
    }
}

pub struct ApSendStream {
    send: SendStream,
}

impl ApSendStream {
    /// Terminate this operation without retaining queued bytes for FIN.
    pub fn cancel(&mut self) {
        let _ = self.send.reset(quinn::VarInt::from_u32(0));
    }

    /// Streams content bytes after a service upload request frame. The request
    /// declares their exact length; finish this stream after the final byte.
    pub async fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ClientError> {
        self.send.write_all(bytes).await?;
        Ok(())
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<(), ClientError> {
        self.send.write_all(&frame.encode()?).await?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), ClientError> {
        self.send.finish()?;
        Ok(())
    }
}

pub struct ApReceiveStream {
    receive: RecvStream,
    response_timeout: Duration,
}

impl ApReceiveStream {
    /// Stop this operation; a partially received frame must never be resumed.
    pub fn cancel(&mut self) {
        let _ = self.receive.stop(quinn::VarInt::from_u32(0));
    }

    pub async fn receive(&mut self) -> Result<Frame, ClientError> {
        receive_frame(&mut self.receive).await
    }

    /// Reject an oversized body before allocation. Keep this future alive across
    /// other stream controls: dropping it mid-frame discards framing progress.
    pub async fn receive_with_body_limit(&mut self, max_body: usize) -> Result<Frame, ClientError> {
        receive_frame_with_limit(&mut self.receive, max_body).await
    }

    /// Wait for a request's answer using the original stream's deadline.
    pub async fn answer(&mut self) -> Result<Frame, ClientError> {
        timeout(self.response_timeout, self.receive())
            .await
            .map_err(|_| ClientError::ResponseTimeout)?
    }
}

async fn receive_frame(receive: &mut RecvStream) -> Result<Frame, ClientError> {
    receive_frame_with_limit(receive, MAX_FRAME_LEN).await
}

async fn receive_frame_with_limit(
    receive: &mut RecvStream,
    max_body: usize,
) -> Result<Frame, ClientError> {
    let mut reader = FrameReader::new(max_body);
    loop {
        let buffer = reader.buffer()?;
        let count = buffer.len();
        receive.read_exact(buffer).await?;
        if reader.advance(count)? {
            return Ok(reader.finish()?);
        }
    }
}

async fn authentication_exchange(
    stream: &mut ApStream,
    request: &Frame,
    name: &'static str,
) -> Result<Frame, ClientError> {
    let _interval = crate::trace::Interval::new(name);
    stream.send(request).await?;
    let reply = stream.receive_with_body_limit(super::timing::auth::MAX_REPLY_BYTES).await?;
    let (reply, timings) = super::timing::auth::decode(reply)?;
    for timing in timings.entries() {
        crate::trace::mark_value(timing.stage.name(), timing.duration_ns);
    }
    Ok(reply)
}

async fn authenticate(
    stream: &mut ApStream,
    identity: &mut ClientIdentity,
    allow_registration: bool,
    testing: Option<&super::hello::TestingCredentials>,
) -> Result<(), ClientError> {
    if let Some(testing) = testing {
        let csr = identity.certificate_request()?;
        let request =
            testing.hello(csr.der()).map_err(|_| ClientError::InvalidAuthenticationToken)?;
        let reply = authentication_exchange(stream, &request, "ap.testing_hello").await?;
        return authentication::accept_testing_hello(identity, reply);
    }
    if !identity.auth_token.is_empty() {
        let reply = authentication_exchange(
            stream,
            &authentication::hello_request(identity)?,
            "ap.saved_identity",
        )
        .await?;
        if authentication::accept_hello(identity, reply, allow_registration)?
            == authentication::Hello::Authenticated
        {
            return Ok(());
        }
    }

    if !allow_registration {
        return Err(ClientError::AuthenticationRequired);
    }
    register(stream, identity).await
}

async fn register(stream: &mut ApStream, identity: &mut ClientIdentity) -> Result<(), ClientError> {
    let Some(challenge) = request_registration(stream, identity).await? else {
        return Err(ClientError::ChallengeRequired);
    };
    finish_registration(stream, identity, &challenge).await
}

async fn request_registration(
    stream: &mut ApStream,
    identity: &ClientIdentity,
) -> Result<Option<String>, ClientError> {
    let request = registration::request(&identity.email, &identity.instance_name, "", &[])?;
    let reply = authentication_exchange(stream, &request, "ap.request_code").await?;
    authentication::registration_challenge(&reply)
}

async fn finish_key_registration(
    stream: &mut ApStream,
    identity: &mut ClientIdentity,
    seed: &[u8; 16],
    account: Option<crate::store::Id>,
) -> Result<(), ClientError> {
    use crate::{network::ap::key_auth, store::account_key};
    let csr = identity.certificate_request()?;
    let selector =
        account.map(key_auth::Account::Id).unwrap_or(key_auth::Account::Email(&identity.email));
    let request = key_auth::request(selector, &identity.instance_name, csr.der())?;
    let response = authentication_exchange(stream, &request, "ap.key_challenge").await?;
    if response.frame_type != frame_type::KEY_CHALLENGE
        || response.flags != 0
        || !response.source.is_empty()
        || !response.destination.is_empty()
        || response.body.len() != 32
    {
        return Err(ClientError::InvalidRegistrationResponse);
    }
    let nonce = response
        .body
        .as_slice()
        .try_into()
        .map_err(|_| ClientError::InvalidRegistrationResponse)?;
    let proof = account_key::sign(seed, &account_key::login_message(nonce, &request.body))
        .map_err(|_| ClientError::InvalidRegistrationResponse)?;
    let request = Frame::new(frame_type::KEY_PROOF, 0, [], [], proof)?;
    let reply = authentication_exchange(stream, &request, "ap.key_sign_in").await?;
    authentication::accept_registration(identity, &reply)
}

async fn finish_registration(
    stream: &mut ApStream,
    identity: &mut ClientIdentity,
    challenge: &str,
) -> Result<(), ClientError> {
    let csr = identity.certificate_request()?;
    let request =
        registration::request(&identity.email, &identity.instance_name, challenge, csr.der())?;
    let reply = authentication_exchange(stream, &request, "ap.code_sign_in").await?;
    authentication::accept_registration(identity, &reply)
}

pub fn load_pem_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    let mut reader = BufReader::new(pem);
    let certificates =
        rustls_pemfile::certs(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err(ClientError::NoCertificates);
    }
    Ok(certificates)
}

impl ClientError {
    /// Only establishment failures authorize retry; never replay an authenticated operation.
    #[cfg(any(test, feature = "connected-engine"))]
    pub(crate) fn is_connection_retryable(&self) -> bool {
        self.is_transport_lost()
            || self.is_connection_refused()
            || matches!(self, Self::ConnectTimeout | Self::AddressLookup(_))
    }

    /// A new QUIC connection was refused before AP authentication. A draining
    /// endpoint can send this while its replacement is starting. Only callers
    /// establishing a connection may retry it; it says nothing about an
    /// authenticated operation's outcome.
    pub fn is_connection_refused(&self) -> bool {
        matches!(self, Self::Connection(quinn::ConnectionError::ConnectionClosed(close))
            if close.error_code == quinn::TransportErrorCode::CONNECTION_REFUSED)
    }

    /// The peer stopped reading this stream. Its receive direction may still
    /// contain an application terminal message; this is not transport loss.
    pub fn is_write_stopped(&self) -> bool {
        matches!(self, Self::Write(quinn::WriteError::Stopped(_)))
    }

    /// Positive evidence of lost transport or an explicit planned AP restart.
    /// Other peer closes, stream resets, uncertain replies, protocol rejection
    /// and local cancellation do not authorize reconnect.
    pub fn is_transport_lost(&self) -> bool {
        let connection = match self {
            Self::Connection(error)
            | Self::Write(quinn::WriteError::ConnectionLost(error))
            | Self::Read(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(
                error,
            ))) => error,
            _ => return false,
        };
        matches!(connection, quinn::ConnectionError::Reset | quinn::ConnectionError::TimedOut)
            || matches!(connection, quinn::ConnectionError::ApplicationClosed(close)
                if close.error_code == quinn::VarInt::from_u32(super::APPLICATION_ERROR_RESTART))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::network::ap::Error as ProtocolError;
    use rcgen::{KeyPair, PKCS_ED25519, PublicKeyData};
    use std::io;

    #[test]
    fn connection_establishment_retries_dns_but_not_identity_or_protocol_failures() {
        for error in [
            ClientError::AddressLookup(io::Error::from(io::ErrorKind::NotFound)),
            ClientError::AddressLookup(io::Error::from(io::ErrorKind::AddrNotAvailable)),
            ClientError::ConnectTimeout,
        ] {
            assert!(error.is_connection_retryable());
            assert!(!error.is_transport_lost());
        }
        for error in [
            ClientError::AuthenticationRequired,
            ClientError::AccountDisabled,
            ClientError::AuthenticationTimeout,
            ClientError::InvalidAuthenticationToken,
            ClientError::TlsConfiguration("invalid configuration".into()),
        ] {
            assert!(!error.is_connection_retryable());
        }
    }

    #[test]
    fn write_stop_only_identifies_the_peer_stopping_its_read_half() {
        for code in [0, 42] {
            let error =
                ClientError::Write(quinn::WriteError::Stopped(quinn::VarInt::from_u32(code)));
            assert!(error.is_write_stopped());
            assert!(!error.is_transport_lost());
        }
        for error in [
            ClientError::Write(quinn::WriteError::ConnectionLost(quinn::ConnectionError::Reset)),
            ClientError::Write(quinn::WriteError::ClosedStream),
            ClientError::Read(quinn::ReadExactError::ReadError(quinn::ReadError::Reset(
                quinn::VarInt::from_u32(0),
            ))),
            ClientError::ResponseTimeout,
        ] {
            assert!(!error.is_write_stopped());
        }
    }

    #[test]
    fn transport_loss_is_distinct_from_protocol_failure_and_cancellation() {
        for error in [
            quinn::ConnectionError::Reset,
            quinn::ConnectionError::TimedOut,
            quinn::ConnectionError::ApplicationClosed(quinn::ApplicationClose {
                error_code: quinn::VarInt::from_u32(super::super::APPLICATION_ERROR_RESTART),
                reason: Default::default(),
            }),
        ] {
            assert!(ClientError::Connection(error.clone()).is_transport_lost());
            assert!(
                ClientError::Write(quinn::WriteError::ConnectionLost(error.clone()))
                    .is_transport_lost()
            );
            assert!(
                ClientError::Read(quinn::ReadExactError::ReadError(
                    quinn::ReadError::ConnectionLost(error)
                ))
                .is_transport_lost()
            );
        }
        for error in [
            ClientError::Connection(quinn::ConnectionError::LocallyClosed),
            ClientError::Connection(quinn::ConnectionError::ApplicationClosed(
                quinn::ApplicationClose {
                    error_code: quinn::VarInt::from_u32(0),
                    reason: b"service restarting".as_slice().into(),
                },
            )),
            ClientError::Connection(quinn::ConnectionError::ApplicationClosed(
                quinn::ApplicationClose {
                    error_code: quinn::VarInt::from_u32(APPLICATION_ERROR_BYE),
                    reason: Default::default(),
                },
            )),
            ClientError::Write(quinn::WriteError::Stopped(quinn::VarInt::from_u32(0))),
            ClientError::Read(quinn::ReadExactError::FinishedEarly(0)),
            ClientError::Read(quinn::ReadExactError::ReadError(quinn::ReadError::Reset(
                quinn::VarInt::from_u32(0),
            ))),
            ClientError::Protocol(ProtocolError::InvalidHeader),
            ClientError::AuthenticationRequired,
            ClientError::ResponseTimeout,
            ClientError::Io(io::Error::from(io::ErrorKind::ConnectionReset)),
        ] {
            assert!(!error.is_transport_lost(), "{error:?}");
        }
    }

    /// A self-signed QUIC endpoint speaking the AP's ALPN, for the test servers.
    fn server_endpoint(runtime: &tokio::runtime::Runtime) -> (Endpoint, CertificateDer<'static>) {
        server_endpoint_with_transport(runtime, None)
    }
    fn server_endpoint_with_transport(
        runtime: &tokio::runtime::Runtime,
        transport: Option<quinn::TransportConfig>,
    ) -> (Endpoint, CertificateDer<'static>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = certified.cert.der().clone();
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .unwrap();
        crypto.alpn_protocols = vec![b"pb.ap".to_vec()];
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(crypto).unwrap(),
        ));
        if let Some(transport) = transport {
            server.transport = Arc::new(transport);
        }
        let endpoint = runtime
            .block_on(async { Endpoint::server(server, "127.0.0.1:0".parse().unwrap()) })
            .unwrap();
        (endpoint, certificate)
    }

    #[test]
    fn quic_keepalive_preserves_an_idle_connection_without_application_requests() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(Duration::from_millis(250).try_into().unwrap()));
        let (endpoint, certificate) = server_endpoint_with_transport(&runtime, Some(transport));
        let mut config =
            ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![certificate]);
        config.idle_timeout = Duration::from_millis(250);
        config.keep_alive_interval = Duration::from_millis(50);
        runtime.block_on(async {
            timeout(Duration::from_secs(5), async {
                let accepted_endpoint = endpoint.clone();
                let accepted = tokio::spawn(async move {
                    accepted_endpoint.accept().await.unwrap().await.unwrap()
                });
                let authentication = open_authentication_connection(&config, None).await.unwrap();
                let server = accepted.await.unwrap();
                // Several negotiated idle windows pass with no AP requests.
                tokio::time::sleep(Duration::from_secs(1)).await;
                assert!(authentication.connection.close_reason().is_none());
                assert!(server.close_reason().is_none());
                assert!(authentication.connection.stats().frame_tx.ping >= 3);
                assert!(server.stats().frame_rx.ping >= 3);
                assert_eq!(server.stats().frame_rx.stream, 0);
                assert_eq!(endpoint.open_connections(), 1);
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn default_transport_detects_a_blackholed_path_without_application_requests() {
        // Drop both directions after a real QUIC handshake. The socket remains
        // bound, so only protocol liveness, not ICMP or a peer close, detects loss.
        use std::sync::atomic::{AtomicBool, Ordering};
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&runtime);
        let server_address = endpoint.local_addr().unwrap();
        let proxy = runtime.block_on(tokio::net::UdpSocket::bind("127.0.0.1:0")).unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let dropping = Arc::new(AtomicBool::new(false));
        let gate = dropping.clone();
        let forwarding = runtime.spawn(async move {
            let mut client_address = None;
            let mut packet = [0; 65_536];
            loop {
                let (length, sender) = proxy.recv_from(&mut packet).await.unwrap();
                if gate.load(Ordering::Acquire) {
                    continue;
                }
                let destination = if sender == server_address {
                    client_address.expect("the client initiated this connection")
                } else {
                    client_address = Some(sender);
                    server_address
                };
                proxy.send_to(&packet[..length], destination).await.unwrap();
            }
        });
        let config = ClientConfig::new(proxy_address, "localhost", vec![certificate]);
        let accepted_endpoint = endpoint.clone();
        let accepting =
            runtime.spawn(async move { accepted_endpoint.accept().await.unwrap().await.unwrap() });
        let connection = connect_for_close_tests(&runtime, &config);
        runtime.block_on(async {
            let peer = accepting.await.unwrap();
            assert!(connection.close_reason().is_none());
            assert!(peer.close_reason().is_none());
            dropping.store(true, Ordering::Release);
            let started = std::time::Instant::now();
            let error = timeout(Duration::from_secs(40), connection.closed()).await.unwrap();
            let elapsed = started.elapsed();
            eprintln!("blackholed default QUIC transport detected after {elapsed:?}");
            assert!(matches!(error, quinn::ConnectionError::TimedOut), "{error:?}");
            assert!(ClientError::Connection(error).is_transport_lost());
            // Scheduling margin around the five-second idle timeout on loopback.
            // QUIC may extend the timeout to three PTOs on a high-latency path.
            assert!(elapsed < Duration::from_secs(8), "transport remained open for {elapsed:?}");
            forwarding.abort();
        });
    }

    #[test]
    fn registration_preparation_checks_transport_without_authentication() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&runtime);
        let config =
            ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![certificate]);
        runtime.block_on(async {
            let server = tokio::spawn(async move {
                let incoming = endpoint.accept().await.unwrap();
                if let Ok(connection) = incoming.await {
                    assert!(
                        connection.accept_bi().await.is_err(),
                        "preparation sent application data"
                    );
                }
            });
            ApConnection::prepare_registration(&config).await.unwrap();
            timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
        });
    }

    #[test]
    fn registration_preparation_rejects_wrong_certificate_name_and_bounds_offline_wait() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&runtime);
        let mut config =
            ClientConfig::new(endpoint.local_addr().unwrap(), "wrong.invalid", vec![certificate]);
        runtime.block_on(async {
            let server = tokio::spawn(async move {
                let _ = endpoint.accept().await.unwrap().await;
            });
            assert!(ApConnection::prepare_registration(&config).await.is_err());
            server.await.unwrap();
            let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            config.server_address = blackhole.local_addr().unwrap();
            config.server_name = "localhost".into();
            config.connect_timeout = Duration::from_millis(20);
            assert!(matches!(
                ApConnection::prepare_registration(&config).await,
                Err(ClientError::ConnectTimeout)
            ));
        });
    }

    #[cfg(feature = "connected-engine")]
    #[test]
    fn membership_joins_while_an_older_ap_withholds_discovery() {
        use crate::{
            engine::session_channels::{Event, SessionChannels},
            network::ap::{channel, operation, service},
            store::{BuildSelector, LocalStore},
        };

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&runtime);
        let config = silent_config(endpoint.local_addr().unwrap(), certificate);
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(dir.path()).unwrap());
        let root = store.create_volume("Root", false).unwrap();
        let build = crate::identifier::encode(&[7; 32]);
        let session = store
            .create_session(
                &crate::package::AppIdentity {
                    publisher: store.account().id,
                    app_id: "stall".into(),
                },
                BuildSelector::Exact { build_id: build.clone() },
                &build,
                root.id,
            )
            .unwrap();
        runtime.block_on(async {
            timeout(Duration::from_secs(5), async {
                let client_endpoint = create_endpoint(&config).unwrap();
                let dial = client_endpoint
                    .connect(config.server_address, &config.server_name)
                    .unwrap();
                let accept_peer = async { endpoint.accept().await.unwrap().await.unwrap() };
                let (client, peer) = tokio::join!(dial, accept_peer);
                let connection = Arc::new(ApConnection {
                    endpoint: client_endpoint,
                    connection: client.unwrap(),
                    discovery: Discovery::default(),
                    instance_id: b"device".to_vec(),
                    response_timeout: config.response_timeout,
                    #[cfg(feature = "app-host")]
                    publisher_issuer: None,
                });
                let accept = || async {
                    let (send, receive) = peer.accept_bi().await.unwrap();
                    ApStream { send, receive, response_timeout: config.response_timeout }
                };

                // The ordinary stream has received Describe and sends no response until after join.
                let discovery = connection.describe();
                tokio::pin!(discovery);
                let mut ordinary = tokio::select! {
                    stream = accept() => stream,
                    result = &mut discovery => panic!("unexpected discovery result {result:?}"),
                };
                let request = ordinary.receive().await.unwrap();
                let request: operation::ServiceRequest =
                    serde_json::from_slice(&request.body).unwrap();

                let quota = Arc::new(tokio::sync::Semaphore::new(channel::ACCOUNT_QUEUE_BYTES));
                let join = SessionChannels::join(
                    connection.clone(), store, session, Some(([1; 16], [7; 32])), quota, Arc::new(|| {}),
                );
                let baseline = async {
                    let mut control = accept().await;
                    let join = control.receive().await.unwrap();
                    assert_eq!(join.frame_type, frame_type::SUBSCRIBE);
                    assert_eq!(join.body.len(), channel::PROOF_BYTES);
                    let ack = Frame::new(frame_type::SUBSCRIBE, 0, [], &join.destination[..], []).unwrap();
                    control.send(&ack).await.unwrap();
                    (control, join.destination)
                };
                let (joined, (mut control, destination)) = tokio::join!(join, baseline);
                let mut channels = joined.unwrap();
                let mut description = service::ServiceDescription::baseline();
                description.features.retain(|f| f != channel::DISPLAY_NAMES_FEATURE);
                let response = serde_json::json!({
                    "timing": [], "ok": true, "operation_id": request.operation_id(), "data": description,
                });
                let answer = Frame::new(
                    frame_type::DIRECT, service::PROTOCOL, operation::SERVICE_DESTINATION,
                    connection.instance_id(), serde_json::to_vec(&response).unwrap(),
                ).unwrap();
                ordinary.send(&answer).await.unwrap();
                discovery.await.unwrap();
                assert!(!connection.discover_session_names().await);

                // The original stream continues delivering the original roster after feature absence.
                let roster = channel::Control::Presence { participants: vec![] };
                let body = serde_json::to_vec(&roster).unwrap();
                control.send(&Frame::new(frame_type::PUBLISH, 0, [], destination, body).unwrap()).await.unwrap();
                assert_eq!(channels.next().await.unwrap(), Event::Presence { participants: vec![] });
            }).await.unwrap();
        });
    }

    /// Connects a bare `ApConnection` (bypassing the AP authentication
    /// handshake, which is irrelevant to closing a transport) to a server
    /// that never answers anything, modeling a connected, idle AP peer.
    fn connect_for_close_tests(
        runtime: &tokio::runtime::Runtime,
        config: &ClientConfig,
    ) -> ApConnection {
        runtime.block_on(async {
            let authentication = open_authentication_connection(config, None).await.unwrap();
            ApConnection {
                endpoint: authentication.endpoint,
                connection: authentication.connection,
                discovery: Discovery::default(),
                instance_id: vec![],
                response_timeout: config.response_timeout,
                #[cfg(feature = "app-host")]
                publisher_issuer: None,
            }
        })
    }

    #[test]
    fn actual_peer_close_recovers_only_the_planned_restart_code() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for code in [super::super::APPLICATION_ERROR_RESTART, 0, APPLICATION_ERROR_BYE] {
            let (endpoint, certificate) = server_endpoint(&runtime);
            let config =
                ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![certificate]);
            let accepting = runtime.spawn(async move {
                let peer = timeout(Duration::from_secs(2), endpoint.accept())
                    .await
                    .unwrap()
                    .unwrap()
                    .await
                    .unwrap();
                (endpoint, peer)
            });
            let connection = connect_for_close_tests(&runtime, &config);
            runtime.block_on(async {
                let (_endpoint, peer) = accepting.await.unwrap();
                peer.close(quinn::VarInt::from_u32(code), b"service restarting");
                let error = ClientError::Connection(
                    timeout(Duration::from_secs(2), connection.closed()).await.unwrap(),
                );
                assert_eq!(
                    error.is_transport_lost(),
                    code == super::super::APPLICATION_ERROR_RESTART
                );
            });
        }
    }

    #[test]
    fn actual_connection_refusal_is_retryable_only_during_establishment() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&runtime);
        let config =
            ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![certificate]);
        runtime.block_on(async {
            let client = open_authentication_connection(&config, None);
            let server = async {
                endpoint.accept().await.unwrap().refuse();
            };
            let (result, ()) = tokio::join!(client, server);
            let error = result.err().expect("the endpoint refused the connection");
            assert!(error.is_connection_refused(), "{error:?}");
            assert!(
                !error.is_transport_lost(),
                "refusal does not classify an established operation"
            );
        });
        for error in [
            ClientError::AuthenticationRequired,
            ClientError::ResponseTimeout,
            ClientError::Protocol(ProtocolError::InvalidHeader),
            ClientError::Connection(quinn::ConnectionError::LocallyClosed),
            ClientError::Connection(quinn::ConnectionError::ConnectionClosed(
                quinn::ConnectionClose {
                    error_code: quinn::TransportErrorCode::PROTOCOL_VIOLATION,
                    frame_type: None,
                    reason: Default::default(),
                },
            )),
        ] {
            assert!(!error.is_connection_refused(), "{error:?}");
        }
    }

    /// `Connection::close` marks the connection closed at once, but actually
    /// sending CONNECTION_CLOSE still needs the endpoint polled. That is why
    /// closing is split: `Remote::disconnect` runs on synchronous callers,
    /// including the owner thread and iOS suspend while it holds the bridge's
    /// mutex, so it calls `shutdown` inline and leaves only `wait_closed` to
    /// a task on the network runtime. Holding that drain shows the split:
    /// shared handles are already refused while the drain is still pending,
    /// and the peer sees the close once the drain is allowed to finish.
    #[test]
    #[cfg(feature = "test-support")]
    fn shutdown_refuses_operations_at_once_and_the_held_drain_flushes_once_released() {
        let server_runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&server_runtime);
        let config =
            ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![certificate]);

        // The server never answers anything, and only reports whether it
        // eventually saw its peer close. The five-second window is a
        // correctness bound, not a latency target: it only needs to clear
        // scheduling noise on a slow or loaded CI host.
        let (results, observed) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            server_runtime.block_on(async {
                let Some(incoming) = endpoint.accept().await else {
                    return;
                };
                let Ok(connection) = incoming.await else {
                    return;
                };
                let closed = timeout(Duration::from_secs(5), connection.closed()).await.is_ok();
                let _ = results.send(closed);
            });
        });

        let client_runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let connection = Arc::new(connect_for_close_tests(&client_runtime, &config));
        let held = crate::network::ap::testing::hold_close_flush(&connection);

        // This mirrors `Remote::disconnect`: close locally, then spawn the
        // bounded drain instead of awaiting it inline.
        connection.shutdown();
        let drain = client_runtime.spawn({
            let connection = connection.clone();
            async move {
                connection.wait_closed(Duration::from_millis(250)).await;
            }
        });

        // The close is already in force for a handle a background task could
        // still be holding, while the drain itself cannot have finished.
        let shared = connection.clone();
        assert!(shared.is_closed());
        assert!(
            client_runtime.block_on(shared.open_stream()).is_err(),
            "a closed connection cannot admit a new stream"
        );
        assert!(!drain.is_finished(), "the held drain cannot have finished");

        drop(held);
        client_runtime.block_on(drain).unwrap();
        assert!(
            observed.recv_timeout(Duration::from_secs(5)).expect("the server must report"),
            "the released drain must flush the close"
        );
    }

    /// The reverse of the above: if the runtime is dropped immediately after
    /// a bare `shutdown()`, with no background task ever given a chance to
    /// run, the close is lost. This is deterministic, not a race: dropping
    /// the client runtime tears down its UDP socket before anything can be
    /// sent, so the server can never observe a close from that connection
    /// however long it waits. It documents why `Remote::disconnect` cannot
    /// rely on the caller's process staying alive; there is no orderly
    /// process-exit hook in this codebase to attach a synchronous drain to
    /// (see the change description), so an instant process exit right after
    /// disconnect remains a known, unresolved gap rather than a regression
    /// introduced here.
    #[test]
    fn bare_shutdown_without_a_kept_alive_runtime_never_flushes_the_close() {
        let server_runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&server_runtime);
        let config =
            ClientConfig::new(endpoint.local_addr().unwrap(), "localhost", vec![certificate]);

        let (results, observed) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            server_runtime.block_on(async {
                let Some(incoming) = endpoint.accept().await else {
                    return;
                };
                let Ok(connection) = incoming.await else {
                    return;
                };
                let closed = timeout(Duration::from_secs(1), connection.closed()).await.is_ok();
                let _ = results.send(closed);
            });
        });

        let client_runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let connection = connect_for_close_tests(&client_runtime, &config);
        connection.shutdown();
        drop(connection);
        // Dropped immediately after the bare `shutdown()`, with no task ever
        // given a chance to run, exactly like a process exiting right away.
        drop(client_runtime);

        assert!(
            !observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "the socket is gone once the runtime is dropped; the close can never arrive"
        );
    }

    /// A server that completes the QUIC handshake and accepts the client's
    /// stream and request, then keeps the connection alive without ever
    /// answering. Returns the address to connect to and the certificate to
    /// trust.
    fn silent_server(runtime: &tokio::runtime::Runtime) -> (SocketAddr, CertificateDer<'static>) {
        let (endpoint, certificate) = server_endpoint(runtime);
        let address = endpoint.local_addr().unwrap();
        runtime.spawn(async move {
            loop {
                let Some(incoming) = endpoint.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    let Ok((_send, mut receive)) = connection.accept_bi().await else {
                        return;
                    };
                    let mut header = [0; 8];
                    let _ = receive.read_exact(&mut header).await;
                    // Hold the connection, acknowledging keepalives, forever.
                    std::future::pending::<()>().await;
                });
            }
        });
        (address, certificate)
    }

    /// A server that answers the first request on a stream and then sends
    /// nothing more, holding the connection open. Models an AP that
    /// acknowledges an operation and stalls partway through its transfer.
    fn answering_then_silent_server(
        runtime: &tokio::runtime::Runtime,
    ) -> (SocketAddr, CertificateDer<'static>) {
        let (endpoint, certificate) = server_endpoint(runtime);
        let address = endpoint.local_addr().unwrap();
        runtime.spawn(async move {
            loop {
                let Some(incoming) = endpoint.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    while let Ok((mut send, mut receive)) = connection.accept_bi().await {
                        tokio::spawn(async move {
                            if receive_frame(&mut receive).await.is_err() {
                                return;
                            }
                            let answer = crate::network::ap::operation::ServiceResponse {
                                timing: Default::default(),
                                ok: true,
                                operation_id: TEST_OPERATION.into(),
                                error: None,
                                code: None,
                                denial: None,
                                data: None,
                            };
                            let frame = Frame::new(
                                frame_type::DIRECT,
                                crate::network::ap::service::PROTOCOL,
                                crate::network::ap::operation::SERVICE_DESTINATION,
                                b"instance".to_vec(),
                                serde_json::to_vec(&answer).unwrap(),
                            )
                            .unwrap();
                            let _ = send.write_all(&frame.encode().unwrap()).await;
                            // The transfer never continues.
                            std::future::pending::<()>().await;
                        });
                    }
                });
            }
        });
        (address, certificate)
    }

    const TEST_OPERATION: &str = "operation";

    fn silent_config(address: SocketAddr, certificate: CertificateDer<'static>) -> ClientConfig {
        let mut config = ClientConfig::new(address, "localhost", vec![certificate]);
        config.authentication_timeout = Duration::from_millis(300);
        config.keep_alive_interval = Duration::from_millis(50);
        config
    }

    #[test]
    fn stopped_checkpoint_write_does_not_discard_queued_terminal_event() {
        use crate::network::ap::replication::{ReplicaEvent, ReplicaEventKind};
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (endpoint, certificate) = server_endpoint(&runtime);
        let config = silent_config(endpoint.local_addr().unwrap(), certificate);
        runtime.block_on(async {
            timeout(Duration::from_secs(5), async {
                let accepted_endpoint = endpoint.clone();
                let accepted = tokio::spawn(async move {
                    accepted_endpoint.accept().await.unwrap().await.unwrap()
                });
                let authentication = open_authentication_connection(&config, None).await.unwrap();
                let server = accepted.await.unwrap();
                let mut first_stream = Some(authentication.stream);
                for kind in [
                    ReplicaEventKind::Gap,
                    ReplicaEventKind::Terminal {
                        volume: [7; 16],
                        cause: crate::network::ap::replication::TerminalCause::AuthorityChanged,
                    },
                ] {
                    let stream = if let Some(stream) = first_stream.take() {
                        stream
                    } else {
                        let (send, receive) = authentication.connection.open_bi().await.unwrap();
                        ApStream { send, receive, response_timeout: config.response_timeout }
                    };
                    let (mut send, mut receive) = stream.split();
                    crate::network::ap::service_client::replica_checkpoint(&mut send)
                        .await
                        .unwrap();
                    let (mut server_send, mut server_receive) = server.accept_bi().await.unwrap();
                    let _request = receive_frame(&mut server_receive).await.unwrap();
                    let terminal =
                        ReplicaEvent { epoch: [1; 16], sequence: 1, policy_revision: 0, kind };
                    let frame = Frame::new(
                        frame_type::DIRECT,
                        0,
                        crate::network::ap::operation::SERVICE_DESTINATION,
                        b"instance",
                        terminal.encode().unwrap(),
                    )
                    .unwrap();
                    server_send.write_all(&frame.encode().unwrap()).await.unwrap();
                    server_send.finish().unwrap();
                    server_receive.stop(quinn::VarInt::from_u32(0)).unwrap();
                    // A transport signal, not a sleep, establishes the exact
                    // ordering that occurs when bootstrap delays the receiver.
                    assert_eq!(
                        send.send.stopped().await.unwrap(),
                        Some(quinn::VarInt::from_u32(0))
                    );
                    let error = crate::network::ap::service_client::replica_checkpoint(&mut send)
                        .await
                        .unwrap_err();
                    assert!(error.downcast_ref::<ClientError>().unwrap().is_write_stopped());
                    let bytes =
                        crate::network::ap::service_client::receive_replica_event(&mut receive)
                            .await
                            .unwrap();
                    assert_eq!(ReplicaEvent::decode(&bytes).unwrap().kind, kind);
                }
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn a_transfer_that_stalls_after_its_answer_gives_up_without_closing_the_connection() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (address, certificate) = answering_then_silent_server(&runtime);
        let mut config = silent_config(address, certificate);
        config.response_timeout = Duration::from_millis(300);
        // Bounded from outside, so a transfer that never ends fails the test
        // instead of hanging it.
        runtime.block_on(async {
            timeout(Duration::from_secs(10), async {
                let authentication = open_authentication_connection(&config, None).await.unwrap();
                let (send, receive) = authentication.connection.open_bi().await.unwrap();
                let mut stream =
                    ApStream { send, receive, response_timeout: config.response_timeout };
                stream
                    .send(
                        &Frame::new(
                            frame_type::DIRECT,
                            0,
                            [],
                            crate::network::ap::operation::SERVICE_DESTINATION,
                            b"request".to_vec(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                // The AP answers the request…
                crate::network::ap::service_client::response(&mut stream, TEST_OPERATION)
                    .await
                    .expect("the request is answered");
                // …and then sends nothing more.
                let error = crate::network::ap::service_client::receive_transfer(&mut stream)
                    .await
                    .expect_err("a stalled transfer must fail")
                    .to_string();
                assert!(error.contains("did not answer the request in time"), "{error}");
                // Only this operation is abandoned; the account connection stays up.
                assert!(authentication.connection.close_reason().is_none());
            })
            .await
            .expect("a stalled transfer must not wait forever");
        });
    }

    #[test]
    fn publication_deadline_covers_stream_admission_and_blocked_payload_writes() {
        use crate::network::ap::operation::ServiceRequest;
        for block_open in [true, false] {
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut transport = quinn::TransportConfig::default();
            transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(if block_open {
                1
            } else {
                16
            }));
            transport.stream_receive_window(quinn::VarInt::from_u32(1024));
            transport.receive_window(quinn::VarInt::from_u32(1024));
            let (server_endpoint, certificate) =
                server_endpoint_with_transport(&runtime, Some(transport));
            let address = server_endpoint.local_addr().unwrap();
            let mut config = silent_config(address, certificate);
            config.response_timeout = Duration::from_millis(100);
            runtime.block_on(async {
                timeout(Duration::from_secs(5), async {
                    let server_accept = tokio::spawn(async move {
                        let server = server_endpoint.accept().await.unwrap().await.unwrap();
                        (server_endpoint, server)
                    });
                    let endpoint = create_endpoint(&config).unwrap();
                    let transport =
                        endpoint.connect(address, &config.server_name).unwrap().await.unwrap();
                    let (_server_endpoint, server) = server_accept.await.unwrap();
                    let connection = ApConnection {
                        endpoint,
                        connection: transport,
                        discovery: Discovery::default(),
                        instance_id: vec![],
                        response_timeout: config.response_timeout,
                        #[cfg(feature = "app-host")]
                        publisher_issuer: None,
                    };
                    let mut occupied = connection.open_stream().await.unwrap();
                    if block_open {
                        assert!(
                            timeout(Duration::from_millis(25), connection.open_stream())
                                .await
                                .is_err()
                        );
                    } else {
                        // Establish that this transport really blocks writes,
                        // rather than merely withholding an acknowledgment.
                        let frame = Frame::new(
                            frame_type::DIRECT,
                            0,
                            [],
                            crate::network::ap::operation::SERVICE_DESTINATION,
                            vec![1; 64 * 1024],
                        )
                        .unwrap();
                        assert!(
                            timeout(Duration::from_millis(25), occupied.send(&frame))
                                .await
                                .is_err()
                        );
                        occupied.cancel();
                        let (_send, mut receive) = server.accept_bi().await.unwrap();
                        let error = receive.read_to_end(128 * 1024).await.unwrap_err();
                        assert!(matches!(
                            error,
                            quinn::ReadToEndError::Read(quinn::ReadError::Reset(_))
                        ));
                    }
                    for database_id in [
                        crate::store::DatabaseId::Index,
                        crate::store::DatabaseId::Entry(
                            crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                        ),
                    ] {
                        let request = ServiceRequest::PublishReplica {
                            operation_id: crate::identifier::encode(&[0x11; 16]),
                            session_id: None,
                            build_id: None,
                            volume_id: crate::identifier::encode(&[0x22; 16]),
                            database_id,
                        };
                        let payload = vec![3; 64 * 1024];
                        let result = crate::network::ap::service_client::publish_replica(
                            &connection,
                            &request,
                            &payload,
                        )
                        .await;
                        let error = result.unwrap_err();
                        assert!(
                            matches!(
                                error.downcast_ref::<ClientError>(),
                                Some(ClientError::ResponseTimeout)
                            ),
                            "{error}"
                        );
                        assert!(connection.connection.close_reason().is_none());
                        if !block_open {
                            // RAII cancellation resets the unfinished send. It
                            // must not remain queued forever behind remote flow
                            // control after the caller's timeout has returned.
                            let (_send, mut receive) = server.accept_bi().await.unwrap();
                            let error = receive.read_to_end(128 * 1024).await.unwrap_err();
                            assert!(
                                matches!(
                                    error,
                                    quinn::ReadToEndError::Read(quinn::ReadError::Reset(_))
                                ),
                                "{error}"
                            );
                        }
                    }
                    occupied.cancel();
                    connection.connection.close(quinn::VarInt::from_u32(0), b"test complete");
                })
                .await
                .expect("publication admission/send deadlines must end independently");
            });
        }
    }

    #[test]
    fn an_unanswered_service_request_gives_up_instead_of_waiting_forever() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (address, certificate) = silent_server(&runtime);
        let mut config = silent_config(address, certificate);
        config.response_timeout = Duration::from_millis(300);
        // Bounded from outside, so a request that never ends fails the test
        // instead of hanging it.
        runtime.block_on(async {
            timeout(Duration::from_secs(10), async {
                let authentication = open_authentication_connection(&config, None).await.unwrap();
                // A second stream the silent server never reads or answers.
                let (send, receive) = authentication.connection.open_bi().await.unwrap();
                let mut stream =
                    ApStream { send, receive, response_timeout: config.response_timeout };
                let result =
                    crate::network::ap::service_client::response(&mut stream, "operation").await;
                let error = result.expect_err("an unanswered request must fail").to_string();
                assert!(error.contains("did not answer the request in time"), "{error}");
            })
            .await
            .expect("an unanswered service request must not wait forever");
        });
    }

    #[test]
    fn an_unanswered_split_upload_uses_its_response_deadline_without_closing_the_connection() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (address, certificate) = silent_server(&runtime);
        let mut config = silent_config(address, certificate);
        config.response_timeout = Duration::from_millis(300);
        runtime.block_on(async {
            timeout(Duration::from_secs(10), async {
                let authentication = open_authentication_connection(&config, None).await.unwrap();
                let (send, receive) = authentication.connection.open_bi().await.unwrap();
                let mut stream =
                    ApStream { send, receive, response_timeout: config.response_timeout };
                let bytes = b"uploaded app bytes";
                let request = crate::network::ap::operation::ServiceRequest::UploadBuild {
                    operation_id: TEST_OPERATION.into(),

                    build_id: crate::identifier::encode(&[0x02; 32]),
                    content_length: bytes.len() as u64,
                };
                stream
                    .send(
                        &Frame::new(
                            frame_type::DIRECT,
                            0,
                            [],
                            crate::network::ap::operation::SERVICE_DESTINATION,
                            serde_json::to_vec(&request).unwrap(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                let (mut send, mut receive) = stream.split();
                send.write_bytes(bytes).await.unwrap();
                send.finish().unwrap();
                assert!(matches!(receive.answer().await, Err(ClientError::ResponseTimeout)));
                drop(receive);
                drop(send);
                // Only the failed upload is abandoned. A retry can use a new
                // stream on the same account connection.
                assert!(authentication.connection.close_reason().is_none());
                authentication.connection.open_bi().await.unwrap();
            })
            .await
            .expect("a split upload acknowledgment must not wait forever");
        });
    }

    #[test]
    fn an_ap_that_never_answers_cannot_hold_registration_or_authentication_open() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (address, certificate) = silent_server(&runtime);
        let config = silent_config(address, certificate);
        // Bounded from outside, so an exchange that never ends fails the test
        // instead of hanging it.
        runtime.block_on(async {
            timeout(Duration::from_secs(10), async {
                let started = std::time::Instant::now();
                let identity = ClientIdentity::new("person@example.com", "My Mac");
                let result = ApConnection::request_registration(&config, &identity).await;
                assert!(matches!(result, Err(ClientError::AuthenticationTimeout)), "{result:?}");
                let mut identity = ClientIdentity::new("person@example.com", "My Mac");
                let result = ApConnection::finish_registration(&config, &mut identity, "000123")
                    .await
                    .map(drop);
                assert!(matches!(result, Err(ClientError::AuthenticationTimeout)), "{result:?}");
                // The existing-identity path sends HELLO and waits the same way.
                let key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
                let mut identity = ClientIdentity::from_preissued(
                    "Hosted instance",
                    b"instance".to_vec(),
                    b"token".to_vec(),
                    key.serialize_der(),
                    &key.subject_public_key_info(),
                    b"ca certificate".to_vec(),
                    b"instance certificate".to_vec(),
                )
                .unwrap();
                let result =
                    ApConnection::connect_authenticated(&config, &mut identity).await.map(drop);
                assert!(matches!(result, Err(ClientError::AuthenticationTimeout)), "{result:?}");
                assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
            })
            .await
            .expect("an exchange the AP never answers must end on its own");
        });
    }

    #[test]
    fn tls_config_uses_an_explicit_provider() {
        let config = ClientConfig::new("127.0.0.1:4443".parse().unwrap(), "localhost", Vec::new());
        let crypto = tls_client_config(&config).unwrap();
        assert_eq!(crypto.alpn_protocols, vec![b"pb.ap".to_vec()]);
    }

    #[test]
    fn platform_trust_rejects_untrusted_servers_and_explicit_trust_checks_names() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (address, certificate) = silent_server(&runtime);
        for config in [
            ClientConfig::platform(address, "localhost"),
            ClientConfig::new(address, "wrong.example", vec![certificate.clone()]),
            ClientConfig::new(address, "localhost", vec![]),
        ] {
            let result = runtime
                .block_on(async {
                    let endpoint = create_endpoint(&config).unwrap();
                    timeout(
                        Duration::from_secs(5),
                        endpoint.connect(address, &config.server_name).unwrap(),
                    )
                    .await
                })
                .expect("certificate rejection must not wait for a timeout");
            assert!(
                matches!(result, Err(quinn::ConnectionError::TransportError(_))),
                "invalid server certificate was not rejected: {result:?}"
            );
        }
        // The same isolated server succeeds with its explicit root and matching
        // DNS name, so the negative cases prove verification rather than outage.
        let config = ClientConfig::new(address, "localhost", vec![certificate]);
        runtime.block_on(async {
            let endpoint = create_endpoint(&config).unwrap();
            let connection =
                timeout(Duration::from_secs(5), endpoint.connect(address, "localhost").unwrap())
                    .await
                    .unwrap()
                    .unwrap();
            connection.close(0u32.into(), b"verification test complete");
        });
    }

    #[test]
    #[ignore = "read-only TLS handshake against the shared stable AP endpoint"]
    fn platform_trust_accepts_stable_ap_without_a_pem_bundle() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let address = tokio::net::lookup_host(("ap.playbit.app", 443))
                .await
                .unwrap()
                .find(SocketAddr::is_ipv4)
                .unwrap();
            let config = ClientConfig::platform(address, "ap.playbit.app");
            let endpoint = create_endpoint(&config).unwrap();
            let connection = timeout(
                Duration::from_secs(10),
                endpoint.connect(address, &config.server_name).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
            connection.close(0u32.into(), b"platform trust verified");
        });
    }
}
