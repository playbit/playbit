//! Transport-independent registration framing. Parsing borrows bounded response
//! fields; the authenticated caller owns status interpretation and credential
//! adoption. Neither encoding nor parsing generates keys, sends mail or persists
//! identity. Wire admission precedes allocation; account/CSR policy stays shared
//! with the existing authentication layer rather than being inferred from bytes.
use super::{BODY_ALIGN, Frame, MAX_FRAME_LEN, frame_type};
const HEADER_BYTES: usize = 4;
const MAX_BODY_BYTES: usize = MAX_FRAME_LEN - BODY_ALIGN;
/// The immutable edition used by the authenticated AGREEMENT exchange.
pub const TERMS_REVISION: u8 = 1;
pub const TERMS_DOCUMENT: &str = include_str!("../../../assets/user-agreement-v1.md");
pub const TERMS_URL: &str = "https://playbit.app/help/user-agreement/1/";

/// Decode the pinned digest at compile time for protocol-only hosts without a crypto backend.
pub const fn terms_digest() -> [u8; 32] {
    const PIN: &[u8; 65] = include_bytes!("../../../assets/user-agreement-v1.sha256");
    const fn nibble(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => panic!("agreement digest is lowercase SHA-256"),
        }
    }
    assert!(PIN[64] == b'\n');
    let mut digest = [0; 32];
    let mut i = 0;
    while i < digest.len() {
        digest[i] = nibble(PIN[2 * i]) << 4 | nibble(PIN[2 * i + 1]);
        i += 1;
    }
    digest
}
pub const EMAIL_CODE_DIGITS: usize = 6;

/// Email verification accepts exactly six ASCII digits, including leading zeroes.
/// Display separators are not part of the wire code.
pub fn valid_email_code(code: &[u8]) -> bool {
    code.len() == EMAIL_CODE_DIGITS && code.iter().all(u8::is_ascii_digit)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidCode,
    FieldTooLong,
    UnexpectedFrame(u8),
    InvalidResponse,
    Protocol(super::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCode => f.write_str("enter the six-digit email sign-in code"),
            Self::FieldTooLong => f.write_str("AP registration field is too long"),
            Self::UnexpectedFrame(kind) => write!(f, "unexpected AP registration frame {kind}"),
            Self::InvalidResponse => f.write_str("invalid AP registration response"),
            Self::Protocol(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;

pub(crate) fn request_length(
    email: usize,
    name: usize,
    challenge: usize,
    csr: usize,
    body_limit: usize,
) -> Result<usize> {
    if email > u16::MAX as usize || name > u8::MAX as usize || challenge > u8::MAX as usize {
        return Err(Error::FieldTooLong);
    }
    (HEADER_BYTES + email + name + challenge)
        .checked_add(csr)
        .filter(|length| *length <= MAX_BODY_BYTES && *length <= body_limit)
        .ok_or(Error::Protocol(super::Error::FrameTooLarge))
}

/// Encode the existing REGISTER request. Empty code/CSR starts registration;
/// Account-key login has its own selector-aware codec in `key_auth`.
pub fn request(email: &str, instance_name: &str, challenge: &str, csr: &[u8]) -> Result<Frame> {
    if !challenge.is_empty() && !valid_email_code(challenge.as_bytes()) {
        return Err(Error::InvalidCode);
    }
    let length = request_length(
        email.len(),
        instance_name.len(),
        challenge.len(),
        csr.len(),
        MAX_BODY_BYTES,
    )?;
    let mut body = Vec::with_capacity(length);
    body.extend_from_slice(&(email.len() as u16).to_le_bytes());
    body.push(instance_name.len() as u8);
    body.push(challenge.len() as u8);
    body.extend_from_slice(email.as_bytes());
    body.extend_from_slice(instance_name.as_bytes());
    body.extend_from_slice(challenge.as_bytes());
    body.extend_from_slice(csr);
    assert_eq!(body.len(), length, "encoded fields match admitted body length");
    Frame::new(frame_type::REGISTER, 0, [], [], body).map_err(Error::Protocol)
}

pub struct Response<'a> {
    pub status: u8,
    pub auth_token: &'a [u8],
    pub instance_id: &'a [u8],
    pub ca_certificate: &'a [u8],
    pub instance_certificate: &'a [u8],
}
impl<'a> Response<'a> {
    /// Structural decoding is not successful authentication. The caller must
    /// check status and credential semantics before adopting or persisting them.
    // fuzz: ap_codec
    pub fn parse(frame: &'a Frame) -> Result<Self> {
        if frame.frame_type != frame_type::REGISTER {
            return Err(Error::UnexpectedFrame(frame.frame_type));
        }
        if !frame.source.is_empty()
            || !frame.destination.is_empty()
            || frame.body.len() < HEADER_BYTES
        {
            return Err(Error::InvalidResponse);
        }
        let [token_end, instance_end, ca_end] = response_ends(
            frame.body[0],
            frame.body[1],
            u16::from_le_bytes([frame.body[2], frame.body[3]]),
            frame.body.len(),
        )
        .ok_or(Error::InvalidResponse)?;
        Ok(Self {
            status: frame.flags,
            auth_token: &frame.body[HEADER_BYTES..token_end],
            instance_id: &frame.body[token_end..instance_end],
            ca_certificate: &frame.body[instance_end..ca_end],
            instance_certificate: &frame.body[ca_end..],
        })
    }
}
fn response_ends(token: u8, instance: u8, ca: u16, length: usize) -> Option<[usize; 3]> {
    if !(HEADER_BYTES..=MAX_BODY_BYTES).contains(&length) {
        return None;
    }
    let token_end = HEADER_BYTES + usize::from(token);
    let instance_end = token_end + usize::from(instance);
    let ca_end = instance_end + usize::from(ca);
    (ca_end <= length).then_some([token_end, instance_end, ca_end])
}

#[cfg(test)]
#[path = "registration/tests.rs"]
mod tests;
