//! Native HTTP/1.1 WebSocket transport for the shared bounded AP stream client.
//!
//! The host supplies an already certificate/name/time-verified TLS stream whose
//! Read/Write/flush calls are bounded and nonblocking, plus secure platform RNG
//! for tungstenite's handshake nonce and masks. This module never configures or
//! bypasses TLS. Drive only on socket readiness, local work or `continue_now`;
//! host-owned deadlines bound handshake, cancellation and connection liveness.
use super::{Client, PACKET_MAX, Read as StreamRead};
use std::{
    fmt,
    io::{self as stdio, Read, Write},
};
use tungstenite::{
    Message, WebSocket,
    client::IntoClientRequest,
    handshake::{HandshakeError, MidHandshake, client::ClientHandshake},
    protocol::WebSocketConfig,
};
#[cfg(feature = "ap-identity")]
mod authentication;
#[cfg(feature = "ap-identity")]
mod exchange;
mod io;
mod publication;
mod subscription;
pub use publication::Publication;
pub use subscription::{Subscription, SubscriptionItem};
#[cfg(feature = "ap-identity")]
mod registration;
use io::Bounded;
#[cfg(feature = "ap-identity")]
pub use {
    authentication::Authentication, exchange::AuthenticationError,
    registration::RegistrationRequest,
};
const TURNS: usize = 8;
const WRITE_BYTES: usize = 2 * (PACKET_MAX + 14);

#[derive(Debug)]
pub enum Error {
    Closed,
    Authority,
    Subprotocol,
    MessageType,
    Socket(tungstenite::Error),
    Stream(super::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AP WebSocket transport: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<tungstenite::Error> for Error {
    fn from(error: tungstenite::Error) -> Self {
        Self::Socket(error)
    }
}
impl From<super::Error> for Error {
    fn from(error: super::Error) -> Self {
        Self::Stream(error)
    }
}
type Result<T> = std::result::Result<T, Error>;
fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(0)
        .max_write_buffer_size(WRITE_BYTES)
        .max_frame_size(Some(PACKET_MAX))
        .max_message_size(Some(PACKET_MAX))
}
fn blocked(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e) if e.kind() == stdio::ErrorKind::WouldBlock)
}

pub struct Connecting<S: Read + Write> {
    state: Option<MidHandshake<ClientHandshake<Bounded<S>>>>,
    continue_now: bool,
}
impl<S: Read + Write> Connecting<S> {
    /// `authority` is only host[:port], at most 256 bytes. AP's path and requested
    /// subprotocol are fixed. The caller owns TLS trust, name/time and RNG setup.
    pub fn new(stream: S, authority: &str) -> Result<Self> {
        if authority.is_empty()
            || authority.len() > 256
            || authority.contains('@')
            || authority.parse::<tungstenite::http::uri::Authority>().is_err()
        {
            return Err(Error::Authority);
        }
        let mut request = format!("wss://{authority}/").into_client_request()?;
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "playbit-ap-2".parse().expect("static header"));
        let state = ClientHandshake::start(Bounded::new(stream), request, Some(config()))?;
        Ok(Self { state: Some(state), continue_now: true })
    }
    /// Drive host-owned TLS I/O without exposing WebSocket/parser buffers.
    pub fn transport_mut(&mut self) -> Result<&mut S> {
        Ok(&mut self.state.as_mut().ok_or(Error::Closed)?.get_mut().get_mut().stream)
    }
    pub fn continue_now(&self) -> bool {
        self.continue_now
    }
    /// Observe write readiness while the request or its flush is blocked.
    /// The TLS provider must also deliver its own readiness requirements.
    pub fn wants_write(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.get_ref().get_ref().write_blocked)
    }
    /// A pending result needs readiness unless the I/O quantum was exhausted.
    /// Successful upgrade consumes this object; failures are terminal as well.
    pub fn advance(&mut self) -> Result<Option<Connection<S>>> {
        let mut state = self.state.take().ok_or(Error::Closed)?;
        self.continue_now = false;
        state.get_mut().get_mut().begin();
        match state.handshake() {
            Ok((mut socket, response)) => {
                if response.headers().get("sec-websocket-protocol").map(|v| v.as_bytes())
                    != Some(b"playbit-ap-2")
                {
                    return Err(Error::Subprotocol);
                }
                socket.get_mut().upgraded();
                Ok(Some(Connection {
                    socket: Some(socket),
                    streams: Client::new(),
                    packet: vec![0; PACKET_MAX],
                    pending: false,
                }))
            }
            Err(HandshakeError::Interrupted(state)) => {
                self.continue_now = state.get_ref().get_ref().yielded;
                self.state = Some(state);
                Ok(None)
            }
            Err(HandshakeError::Failure(error)) => Err(error.into()),
        }
    }
}

#[derive(Debug)]
pub struct Progress {
    pub messages: usize,
    /// The host must observe write readiness while output remains buffered.
    pub wants_write: bool,
    /// Schedule another bounded turn without waiting for a new socket edge.
    pub continue_now: bool,
}
pub struct Connection<S: Read + Write> {
    socket: Option<WebSocket<Bounded<S>>>,
    streams: Client,
    packet: Vec<u8>,
    pending: bool,
}
impl<S: Read + Write> Connection<S> {
    /// Drive host-owned TLS I/O; unavailable after terminal close/failure.
    pub fn transport_mut(&mut self) -> Result<&mut S> {
        Ok(&mut self.socket.as_mut().ok_or(Error::Closed)?.get_mut().stream)
    }
    pub fn open(&mut self) -> Result<u32> {
        self.streams.open().map_err(Into::into)
    }
    pub fn read(&mut self, id: u32, output: &mut [u8]) -> Result<StreamRead> {
        self.streams.read(id, output).map_err(Into::into)
    }
    pub fn finish(&mut self, id: u32) -> Result<()> {
        self.streams.finish(id).map_err(Into::into)
    }
    pub fn release(&mut self, id: u32) -> Result<()> {
        self.streams.release(id).map_err(Into::into)
    }
    /// Terminal abort: drop the TLS/socket owner and all streams. Reconnection
    /// creates fresh state; no buffered operation is silently replayed.
    pub fn close(&mut self) {
        self.socket = None;
        self.streams.close();
        self.pending = false;
    }
    pub fn write(&mut self, id: u32, input: &[u8]) -> Result<usize> {
        self.begin()?;
        let result = self.write_packet(id, input);
        if result.as_ref().is_err_and(|e| matches!(e, Error::Socket(_))) {
            self.close();
        }
        result
    }
    fn write_packet(&mut self, id: u32, input: &[u8]) -> Result<usize> {
        if !self.flush()? {
            return Ok(0);
        }
        let count = self.streams.write(id, input, &mut self.packet)?;
        if count == 0 {
            return Ok(0);
        }
        self.send(count)?;
        Ok(count - 5)
    }
    /// Up to eight messages and eight controls, sharing a 32-KiB / 32-I/O-call
    /// quantum. Even an endless sequence of empty fragments yields to its host.
    pub fn advance(&mut self) -> Result<Progress> {
        self.begin()?;
        let result = self.drive();
        if result.is_err() {
            self.close();
        }
        result
    }
    fn begin(&mut self) -> Result<()> {
        self.socket.as_mut().ok_or(Error::Closed)?.get_mut().begin();
        Ok(())
    }
    fn drive(&mut self) -> Result<Progress> {
        // Ready output precedes input: even empty-fragment floods cannot
        // consume the I/O quantum before OPEN/FIN/CREDIT or a pending pong.
        let mut controls = 0;
        for _ in 0..TURNS {
            if !self.flush()? {
                break;
            }
            let count = self.streams.next_packet(&mut self.packet)?;
            if count == 0 {
                break;
            }
            self.send(count)?;
            controls += 1;
        }
        let mut messages = 0;
        for _ in 0..TURNS {
            match self.socket.as_mut().ok_or(Error::Closed)?.read() {
                Ok(Message::Binary(data)) => self.streams.receive(&data)?,
                Ok(Message::Ping(_) | Message::Pong(_)) => self.pending = true,
                Ok(Message::Close(_)) => {
                    // Acknowledge when writable; never retain a dead AP stream
                    // while waiting for the peer's closing transport.
                    self.pending = true;
                    let _ = self.flush();
                    return Err(Error::Closed);
                }
                Ok(_) => return Err(Error::MessageType),
                Err(error) if blocked(&error) => break,
                Err(error) => return Err(error.into()),
            }
            messages += 1;
        }
        self.flush()?;
        Ok(Progress {
            messages,
            wants_write: self.pending,
            continue_now: messages != 0
                || controls == TURNS
                || self.socket.as_ref().ok_or(Error::Closed)?.get_ref().yielded,
        })
    }
    fn send(&mut self, count: usize) -> Result<()> {
        let message = Message::Binary(self.packet[..count].to_vec().into());
        self.pending = true;
        match self.socket.as_mut().ok_or(Error::Closed)?.write(message) {
            Ok(()) => Ok(()),
            // Tungstenite retained the frame: flush it, never re-submit it.
            Err(error) if blocked(&error) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    fn flush(&mut self) -> Result<bool> {
        if !self.pending {
            return Ok(true);
        }
        match self.socket.as_mut().ok_or(Error::Closed)?.flush() {
            Ok(()) => {
                self.pending = false;
                Ok(true)
            }
            Err(error) if blocked(&error) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
#[path = "transport/tests.rs"]
mod tests;
