//! Bounded, explicitly driven TLS for the native AP WebSocket transport.
//!
//! The host supplies a connected nonblocking socket, trusted roots, correct
//! system time, secure platform RNG and deadlines.
//! `advance` alone accesses the socket. Read/Write/flush only access bounded TLS
//! buffers. Drive TLS before WebSocket work, then schedule again if either made
//! progress; otherwise wait for the reported socket readiness or a deadline.
use rustls::{ClientConfig, ClientConnection, RootCertStore, pki_types::ServerName};
use std::{
    io::{self, Read, Write},
    sync::Arc,
};
mod io_limit;
use io_limit::Limited;
const BUFFER_BYTES: usize = 16 * 1024;
const FRAGMENT_BYTES: usize = 4096;
const ROOTS_MAX: usize = 256;

/// Verified TLS profile: no custom verifier, session cache or early application data.
#[derive(Clone)]
pub struct Config(Arc<ClientConfig>);
impl Config {
    pub fn new(roots: RootCertStore) -> io::Result<Self> {
        if roots.is_empty() || roots.len() > ROOTS_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AP TLS requires 1..256 trust roots",
            ));
        }
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(invalid)?
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        config.max_fragment_size = Some(FRAGMENT_BYTES);
        config.resumption = rustls::client::Resumption::disabled();
        Ok(Self(Arc::new(config)))
    }
}
fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
fn blocked() -> io::Error {
    io::ErrorKind::WouldBlock.into()
}
fn closed() -> io::Error {
    io::ErrorKind::NotConnected.into()
}
fn pending(result: io::Result<usize>) -> io::Result<usize> {
    match result {
        Ok(0) => Err(io::ErrorKind::WriteZero.into()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
        result => result,
    }
}

#[derive(Debug)]
pub struct Progress {
    pub ready: bool,
    pub wants_read: bool,
    pub wants_write: bool,
    pub peer_closed: bool,
    /// Socket or plaintext progress needs another bounded turn. Query progress
    /// again after upper-layer Read/Write calls, which may change readiness.
    pub continue_now: bool,
}
struct State<S> {
    socket: S,
    tls: ClientConnection,
    flush_pending: bool,
    peer_closed: bool,
    schedule: bool,
}
pub struct Client<S> {
    state: Option<State<S>>,
}
impl<S: Read + Write> Client<S> {
    /// Construct only while secure platform entropy is available. This may
    /// generate a ClientHello; it performs no socket I/O or name resolution.
    pub fn new(socket: S, config: &Config, server: &str) -> io::Result<Self> {
        if server.is_empty() || server.len() > 253 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid AP TLS server name"));
        }
        let name = ServerName::try_from(server.to_owned()).map_err(invalid)?;
        let mut tls = ClientConnection::new(config.0.clone(), name).map_err(invalid)?;
        tls.set_buffer_limit(Some(BUFFER_BYTES));
        Ok(Self {
            state: Some(State {
                socket,
                tls,
                flush_pending: false,
                peer_closed: false,
                schedule: true,
            }),
        })
    }
    /// One read and one write, each capped at 16 KiB, plus one socket flush.
    /// Certificate processing has the pinned provider's 64-KiB handshake bound;
    /// it belongs on the connection worker, not an interactive UI task.
    pub fn advance(&mut self) -> io::Result<Progress> {
        let result = self.state.as_mut().ok_or_else(closed)?.advance();
        if result.is_err() {
            self.state = None;
        }
        result
    }
    /// Current readiness after all upper-layer work. Consuming buffered plaintext
    /// may re-enable socket reads; accepting plaintext may create pending writes.
    pub fn progress(&self) -> io::Result<Progress> {
        Ok(self.state.as_ref().ok_or_else(closed)?.progress())
    }
    /// Terminal abort; no retained socket or automatically replayed plaintext.
    pub fn close(&mut self) {
        self.state = None;
    }
    fn state(&mut self) -> io::Result<&mut State<S>> {
        self.state.as_mut().ok_or_else(closed)
    }
}
impl<S: Read + Write> State<S> {
    fn advance(&mut self) -> io::Result<Progress> {
        let mut progress = false;
        if self.tls.wants_write() {
            let count = pending(self.tls.write_tls(&mut Limited::new(&mut self.socket)))?;
            self.flush_pending |= count != 0;
            progress |= count != 0;
        }
        if self.flush_pending {
            match self.socket.flush() {
                Ok(()) => {
                    self.flush_pending = false;
                    progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        if self.tls.wants_read() {
            match self.tls.read_tls(&mut Limited::new(&mut self.socket)) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => {
                    let state = self.tls.process_new_packets().map_err(invalid)?;
                    self.peer_closed = state.peer_has_closed();
                    progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        let ready = !self.tls.is_handshaking();
        if ready && self.tls.alpn_protocol() != Some(b"http/1.1".as_slice()) {
            return Err(invalid("AP TLS did not negotiate HTTP/1.1"));
        }
        self.schedule = progress;
        Ok(self.progress())
    }
    fn progress(&self) -> Progress {
        Progress {
            ready: !self.tls.is_handshaking(),
            wants_read: self.tls.wants_read(),
            wants_write: self.tls.wants_write() || self.flush_pending,
            peer_closed: self.peer_closed,
            continue_now: self.schedule,
        }
    }
}
impl<S: Read + Write> Read for Client<S> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let state = self.state()?;
        if state.tls.is_handshaking() {
            return Err(blocked());
        }
        let length = output.len().min(BUFFER_BYTES);
        let result = state.tls.reader().read(&mut output[..length]);
        state.schedule |= result.as_ref().is_ok_and(|count| *count != 0);
        if result.as_ref().is_err_and(|error| error.kind() != io::ErrorKind::WouldBlock) {
            self.state = None;
        }
        result
    }
}
impl<S: Read + Write> Write for Client<S> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        let state = self.state()?;
        if state.tls.is_handshaking() {
            return Err(blocked());
        }
        if state.peer_closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let result = state.tls.writer().write(&input[..input.len().min(FRAGMENT_BYTES)]);
        state.schedule |= result.as_ref().is_ok_and(|count| *count != 0);
        match result {
            Ok(0) => Err(blocked()),
            Err(error) => {
                self.state = None;
                Err(error)
            }
            result => result,
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        let state = self.state()?;
        if state.tls.wants_write() || state.flush_pending { Err(blocked()) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests;
