//! Client-side primitives for the Playbit Access Point protocol.
//!
//! The wire protocol is independent of QUIC: each reliable byte stream carries
//! length-prefixed AP frames. Connection ownership and authentication are added
//! by the client layer on top of this codec. See the adjacent `protocol.md`
//! for framing, authentication, service operations and live replication.
//! Names fit their one-byte lengths; the fixed header and zero padding align the
//! body to 16 bytes. Decode rejects malformed lengths before copying payloads.
//! Payload copying and padding validation are bounded data-plane loops.

use std::fmt;

#[cfg(feature = "ap-identity")]
pub mod authentication;
#[cfg(feature = "network")]
mod client;
#[cfg(feature = "ap-identity")]
mod error;
#[cfg(feature = "ap-identity")]
mod identity;
#[cfg(feature = "ap-identity")]
pub use {authentication::RegistrationChallenge, error::ClientError, identity::ClientIdentity};
pub mod channel;
pub mod hello;
pub mod key_auth;
pub mod operation;
pub mod publisher;
pub mod receive;
pub mod registration;
pub mod replication;
#[cfg(feature = "ap-identity")]
pub mod sealed_identity;
pub mod service;
#[cfg(feature = "network")]
pub mod service_client;
#[cfg(all(feature = "network", feature = "test-support"))]
pub mod testing;
pub mod timing;
#[cfg(all(feature = "ap-tls", not(target_arch = "wasm32")))]
pub mod tls;
pub mod websocket;

#[cfg(feature = "network")]
pub use client::{
    ApConnection, ApReceiveStream, ApSendStream, ApStream, ClientConfig, load_pem_certificates,
};

/// Bounded native transport windows shared by both endpoints. A bulk stream's
/// receive credit is smaller than the connection send budget, leaving room for
/// interactive streams while two bulk transfers are in flight.
#[cfg(feature = "network")]
pub fn quic_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.stream_receive_window((512u32 * 1024).into());
    transport.receive_window((2u32 * 1024 * 1024).into());
    transport.send_window(2 * 1024 * 1024);
    transport
}

pub const MAX_FRAME_LEN: usize = 1024 * 1024;
/// A planned AP shutdown. Native clients may reauthenticate and recover their
/// existing subscriptions through the bounded transport-recovery path.
pub const APPLICATION_ERROR_RESTART: u32 = 0x1000b7f;
const HEADER_LEN: usize = 8;
const BODY_ALIGN: usize = 16;

const _: () = assert!(HEADER_LEN == 4 + 4);
const _: () = assert!(BODY_ALIGN.is_power_of_two());
const _: () = assert!(HEADER_LEN <= BODY_ALIGN);
const _: () = assert!(MAX_FRAME_LEN <= u32::MAX as usize);
const _: () = assert!(MAX_FRAME_LEN >= HEADER_LEN + 2 * u8::MAX as usize + BODY_ALIGN);

pub mod frame_type {
    pub const HELLO: u8 = 0;
    pub const REGISTER: u8 = 1;
    pub const DIRECT: u8 = 2;
    pub const PUBLISH: u8 = 3;
    pub const SUBSCRIBE: u8 = 4;
    pub const KEY_CHALLENGE: u8 = 5;
    pub const KEY_PROOF: u8 = 6;
    pub const ONBOARDING: u8 = 7;
    pub const AGREEMENT: u8 = 8;

    pub const ERROR: u8 = 200;
}

pub mod registration_status {
    pub const IN_PROGRESS: u8 = 0;
    pub const SUCCESS: u8 = 1;
    pub const INVALID_CHALLENGE: u8 = 2;
    pub const ACCOUNT_DISABLED: u8 = 3;
    /// A reserved email challenge has no confirmed delivery. Retry after cooldown.
    pub const DELIVERY_UNCONFIRMED: u8 = 4;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub frame_type: u8,
    pub flags: u8,
    pub source: Vec<u8>,
    pub destination: Vec<u8>,
    pub body: Vec<u8>,
}

impl Frame {
    pub fn new(
        frame_type: u8,
        flags: u8,
        source: impl Into<Vec<u8>>,
        destination: impl Into<Vec<u8>>,
        body: impl Into<Vec<u8>>,
    ) -> Result<Self> {
        let frame = Self {
            frame_type,
            flags,
            source: source.into(),
            destination: destination.into(),
            body: body.into(),
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let body_offset = body_offset(self.source.len(), self.destination.len());
        let frame_len = body_offset.checked_add(self.body.len()).ok_or(Error::FrameTooLarge)?;
        assert!(frame_len <= MAX_FRAME_LEN, "validated frame fits the wire limit");

        let mut encoded = vec![0; frame_len];
        encoded[..4].copy_from_slice(&(frame_len as u32).to_le_bytes());
        encoded[4] = self.frame_type;
        encoded[5] = self.flags;
        encoded[6] = self.source.len() as u8;
        encoded[7] = self.destination.len() as u8;
        encoded[HEADER_LEN..HEADER_LEN + self.source.len()].copy_from_slice(&self.source);
        let destination_start = HEADER_LEN + self.source.len();
        encoded[destination_start..destination_start + self.destination.len()]
            .copy_from_slice(&self.destination);
        encoded[body_offset..].copy_from_slice(&self.body);
        Ok(encoded)
    }

    // fuzz: ap_codec
    pub fn decode(encoded: &[u8]) -> Result<Self> {
        let header = Header::decode(encoded)?;
        if encoded.len() != header.frame_len {
            return Err(Error::InvalidLength);
        }
        let source_start = HEADER_LEN;
        let destination_start = source_start + header.source_len;
        let body_start = body_offset(header.source_len, header.destination_len);
        if body_start > encoded.len() {
            return Err(Error::InvalidHeader);
        }
        if encoded[destination_start + header.destination_len..body_start]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(Error::InvalidPadding);
        }

        Self::new(
            header.frame_type,
            header.flags,
            encoded[source_start..destination_start].to_vec(),
            encoded[destination_start..destination_start + header.destination_len].to_vec(),
            encoded[body_start..].to_vec(),
        )
    }

    fn validate(&self) -> Result<()> {
        if self.source.len() > u8::MAX as usize || self.destination.len() > u8::MAX as usize {
            return Err(Error::NameTooLong);
        }
        match self.frame_type {
            frame_type::HELLO
            | frame_type::REGISTER
            | frame_type::KEY_CHALLENGE
            | frame_type::KEY_PROOF
            | frame_type::ONBOARDING
            | frame_type::AGREEMENT => {}
            frame_type::DIRECT | frame_type::PUBLISH | frame_type::SUBSCRIBE => {
                if self.destination.is_empty() {
                    return Err(Error::MissingDestination);
                }
            }
            frame_type::ERROR..=u8::MAX => {}
            _ => return Err(Error::InvalidType(self.frame_type)),
        }

        let frame_len = body_offset(self.source.len(), self.destination.len())
            .checked_add(self.body.len())
            .ok_or(Error::FrameTooLarge)?;
        if frame_len > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Header {
    frame_len: usize,
    frame_type: u8,
    flags: u8,
    source_len: usize,
    destination_len: usize,
}

impl Header {
    // fuzz: ap_codec
    fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() < HEADER_LEN {
            return Err(Error::UnexpectedEnd);
        }
        let frame_len = u32::from_le_bytes(
            encoded[..4].try_into().expect("checked fixed-width field has the destination width"),
        ) as usize;
        if frame_len < HEADER_LEN {
            return Err(Error::InvalidLength);
        }
        if frame_len > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge);
        }
        let source_len = encoded[6] as usize;
        let destination_len = encoded[7] as usize;
        let names_end = HEADER_LEN
            .checked_add(source_len)
            .and_then(|length| length.checked_add(destination_len))
            .ok_or(Error::InvalidHeader)?;
        if names_end > frame_len {
            return Err(Error::InvalidHeader);
        }
        Ok(Self {
            frame_len,
            frame_type: encoded[4],
            flags: encoded[5],
            source_len,
            destination_len,
        })
    }
}

fn body_offset(source_len: usize, destination_len: usize) -> usize {
    assert!(source_len <= u8::MAX as usize, "source length fits the header");
    assert!(destination_len <= u8::MAX as usize, "destination length fits the header");
    let names_end = HEADER_LEN + source_len + destination_len;
    (names_end + BODY_ALIGN - 1) & !(BODY_ALIGN - 1)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    UnexpectedEnd,
    InvalidLength,
    InvalidHeader,
    InvalidPadding,
    InvalidTiming,
    InvalidType(u8),
    MissingDestination,
    NameTooLong,
    FrameTooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(formatter, "AP frame ended before its header"),
            Self::InvalidLength => write!(formatter, "AP frame length is invalid"),
            Self::InvalidHeader => write!(formatter, "AP frame header is invalid"),
            Self::InvalidTiming => {
                write!(formatter, "AP authentication timing envelope is invalid")
            }
            Self::InvalidPadding => write!(formatter, "AP frame padding is not zero"),
            Self::InvalidType(frame_type) => {
                write!(formatter, "AP frame type {frame_type} is invalid")
            }
            Self::MissingDestination => write!(formatter, "AP frame destination is empty"),
            Self::NameTooLong => write!(formatter, "AP frame source or destination is too long"),
            Self::FrameTooLarge => write!(formatter, "AP frame is too large"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn maximum_names_and_frame_sizes_round_trip() {
        // Probe both sides of alignment and wire bounds, including direct public-field mutation.
        for source_len in [0, 1, 7, 8, 15, 16, 254, 255] {
            for destination_len in [1, 7, 8, 15, 16, 254, 255] {
                let offset = body_offset(source_len, destination_len);
                assert_eq!(offset % BODY_ALIGN, 0);
                let mut frame = Frame::new(
                    frame_type::DIRECT,
                    0,
                    vec![1; source_len],
                    vec![2; destination_len],
                    vec![3; MAX_FRAME_LEN - offset],
                )
                .unwrap();
                let bytes = frame.encode().unwrap();
                assert_eq!(bytes.len(), MAX_FRAME_LEN);
                assert_eq!(Frame::decode(&bytes).unwrap(), frame);
                frame.body.push(0);
                assert_eq!(frame.encode(), Err(Error::FrameTooLarge));
            }
        }
        let mut frame = Frame::new(frame_type::HELLO, 0, [], [], []).unwrap();
        frame.source.resize(256, 0);
        assert_eq!(frame.encode(), Err(Error::NameTooLong));
    }

    #[test]
    fn header_rejects_names_without_their_alignment_padding() {
        // A complete declared header can still lack the padding needed before its body.
        for length in 8..16 {
            let mut bytes = vec![0; length];
            bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
            assert_eq!(Frame::decode(&bytes), Err(Error::InvalidHeader));
        }
    }

    #[test]
    fn frame_round_trip_preserves_fields_and_alignment() {
        let frame = Frame::new(
            frame_type::PUBLISH,
            1,
            b"sender".to_vec(),
            b"recipes".to_vec(),
            b"hello".to_vec(),
        )
        .unwrap();

        let encoded = frame.encode().unwrap();
        assert_eq!(encoded.len(), 37);
        assert_eq!(&encoded[..4], &37_u32.to_le_bytes());
        assert_eq!(&encoded[8..14], b"sender");
        assert_eq!(&encoded[14..21], b"recipes");
        assert!(encoded[21..32].iter().all(|byte| *byte == 0));
        assert_eq!(&encoded[32..], b"hello");
        assert_eq!(Frame::decode(&encoded).unwrap(), frame);
    }

    #[test]
    fn decoder_rejects_partial_and_nonzero_padding() {
        let frame =
            Frame::new(frame_type::DIRECT, 0, Vec::new(), b"peer".to_vec(), b"hello".to_vec())
                .unwrap();
        let mut encoded = frame.encode().unwrap();
        assert_eq!(Frame::decode(&encoded[..encoded.len() - 1]), Err(Error::InvalidLength));

        encoded[12] = 1;
        assert_eq!(Frame::decode(&encoded), Err(Error::InvalidPadding));
    }

    #[test]
    fn destination_is_required_for_routing_frames() {
        assert_eq!(
            Frame::new(frame_type::SUBSCRIBE, 0, Vec::new(), Vec::new(), Vec::new()),
            Err(Error::MissingDestination)
        );
        assert!(Frame::new(frame_type::HELLO, 0, Vec::new(), Vec::new(), Vec::new()).is_ok());
    }

    #[test]
    fn unknown_application_types_are_rejected_but_server_errors_are_preserved() {
        assert_eq!(
            Frame::new(42, 0, Vec::new(), Vec::new(), Vec::new()),
            Err(Error::InvalidType(42))
        );
        assert!(Frame::new(250, 0, Vec::new(), Vec::new(), Vec::new()).is_ok());
    }
}
