//! Authenticated account identity and admission, independent of payload meaning.
use super::{MAX_PLAINTEXT_BYTES, Record};
use crate::store::catalog::record::{self, Codec, Mode, View};
use crate::store::{Error, Result};
use serde::Serialize;

pub const UNSUPPORTED: &str = "This account requires a newer version of Playbit. Its saved data has been kept; account access is paused.";
pub const MAX_FEATURES: usize = 16;
pub const MAX_FEATURE_BYTES: usize = 64;
const MAX_DECODE_CHARGE: usize = 4 * 1024 * 1024;
/// An invitation carries a creator-owned Shared capability, distinct from World.
pub const SHARED_SESSION: &str = "shared-session.v1";
fn supports(feature: &str) -> bool {
    feature == SHARED_SESSION
}

pub fn unsupported() -> Error {
    Error::AccessDenied(UNSUPPORTED)
}
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub fn is_unsupported(error: &Error) -> bool {
    matches!(error, Error::AccessDenied(message) if *message == UNSUPPORTED)
}

struct Payload(Vec<u8>);
impl Codec for Payload {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        record::raw(out, record::BYTES, &self.0)
    }
    fn read(value: View<'_>, _: Mode) -> Result<Self> {
        Ok(Self(value.body(record::BYTES)?.to_vec()))
    }
}

pub(super) struct Frame {
    kind: String,
    logical_key: String,
    authority: bool,
    required_read: Vec<String>,
    required_write: Vec<String>,
    payload: Payload,
    unsupported_header: bool,
}
impl Codec for Frame {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(record::STRUCT);
        record::put(out, 1, &self.kind)?;
        record::put(out, 2, &self.logical_key)?;
        record::put(out, 3, &self.authority)?;
        record::put(out, 4, &self.required_read)?;
        record::put(out, 5, &self.required_write)?;
        record::put(out, 6, &self.payload)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (known, unsupported_header) = header(value)?;
        let fields = View::record(&known)?.fields(mode, &[1, 2, 3, 4, 5, 6])?;
        let kind: String = fields.required(1, mode)?;
        let logical_key: String = fields.required(2, mode)?;
        if !name(kind.as_bytes())
            || !kind
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || logical_key.is_empty()
            || logical_key.len() > 511
            || logical_key.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(Error::InvalidInput("invalid account record identity"));
        }
        Ok(Self {
            kind,
            logical_key,
            authority: fields.required(3, mode)?,
            required_read: features(fields.get(4)?, mode)?,
            required_write: features(fields.get(5)?, mode)?,
            payload: fields.required(6, mode)?,
            unsupported_header,
        })
    }
}
fn header(value: View<'_>) -> Result<(Vec<u8>, bool)> {
    // Identity remains interpretable before unfamiliar requirements. Keep
    // unknown header bodies opaque, including future wire kinds; only their
    // framing and bounded extent are understood here.
    let mut tail = value.body(record::STRUCT)?;
    let mut known = Vec::from(record::MAGIC);
    known.push(record::STRUCT);
    let mut previous = 0;
    let mut extensions = 0;
    let mut count = 0;
    let mut unsupported_header = false;
    while !tail.is_empty() {
        count += 1;
        if count > 64 {
            return Err(Error::LimitExceeded);
        }
        let (tag, _, rest) = record::field(tail)?;
        let id = tag & 0x3fff;
        if id == 0 || id <= previous {
            return Err(Error::corrupt("unordered account header fields"));
        }
        let bytes = &tail[..tail.len() - rest.len()];
        if id <= 6 {
            known.extend_from_slice(bytes);
        } else {
            extensions += bytes.len();
            if extensions > record::MAX_EXTENSION_BYTES {
                return Err(Error::LimitExceeded);
            }
            unsupported_header |= tag & record::REQUIRED != 0;
        }
        previous = id;
        tail = rest;
    }
    if record::charge(&known)?.saturating_add(extensions * 512) > MAX_DECODE_CHARGE {
        return Err(Error::LimitExceeded);
    }
    Ok((known, unsupported_header))
}
fn features(value: View<'_>, mode: Mode) -> Result<Vec<String>> {
    if value.sequence()?.count()? > MAX_FEATURES {
        return Err(Error::LimitExceeded);
    }
    let mut result: Vec<String> = Vec::new();
    let mut sequence = value.sequence()?;
    while let Some(value) = sequence.next_value()? {
        let bytes = value.body(record::TEXT)?;
        if !name(bytes) || result.last().is_some_and(|old| old.as_bytes() >= bytes) {
            return Err(Error::InvalidInput("invalid account feature requirements"));
        }
        result.push(String::read(value, mode)?);
    }
    Ok(result)
}
fn name(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.len() <= MAX_FEATURE_BYTES
        && bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

/// This is returned to WebCrypto before key binding. An unsupported declaration
/// becomes an authority failure only after authentication and HMAC identity checks.
#[derive(Serialize)]
pub struct Decoded {
    pub logical_key: String,
    pub record: Option<Record>,
    pub unsupported: bool,
}
impl Decoded {
    pub fn admit(self) -> Result<Option<Record>> {
        if self.unsupported { Err(unsupported()) } else { Ok(self.record) }
    }
}
impl Frame {
    fn parse(plain: &[u8]) -> Result<Self> {
        if plain.is_empty() || plain.len() > MAX_PLAINTEXT_BYTES {
            return Err(Error::InvalidInput("invalid account record size"));
        }
        record::decode(plain)
    }
    pub(super) fn decode(plain: &[u8]) -> Result<Decoded> {
        let frame = Self::parse(plain)?;
        let expected_authority = match frame.kind.as_str() {
            "display_name" | "app_visibility" => Some(false),
            "volume" | "volume_deleted" | "invitation" | "personal_volumes" | "hosted_volume"
            | "hosted_invitation" => Some(true),
            _ => None,
        };
        if expected_authority.is_some_and(|expected| expected != frame.authority) {
            return Err(Error::InvalidInput("account record authority classification changed"));
        }
        if expected_authority.is_none() && !frame.authority {
            let prefix = format!("optional/{}/", frame.kind);
            if !frame.logical_key.starts_with(&prefix) || frame.logical_key.len() == prefix.len() {
                return Err(Error::InvalidInput(
                    "optional account record uses a reserved identity",
                ));
            }
        }
        let mut unsupported = frame.unsupported_header
            || frame.required_read.iter().any(|feature| !supports(feature))
            || frame.required_write.iter().any(|feature| !supports(feature))
            || expected_authority.is_none() && frame.authority;
        let decoded = if expected_authority.is_some() && !unsupported {
            let decode = record::charge(&frame.payload.0).and_then(|charge| {
                if charge > MAX_DECODE_CHARGE {
                    return Err(Error::LimitExceeded);
                }
                record::decode::<Record>(&frame.payload.0)
            });
            match decode {
                Ok(record) => {
                    record.validate()?;
                    if matches!(&record,
                        Record::Invitation { invitation, .. }
                            | Record::HostedInvitation { invitation, .. }
                        if invitation.session.shared_volume.is_some())
                        && (!frame.required_read.iter().any(|feature| feature == SHARED_SESSION)
                            || !frame
                                .required_write
                                .iter()
                                .any(|feature| feature == SHARED_SESSION))
                    {
                        return Err(Error::InvalidInput("Shared account requirements missing"));
                    }
                    if record.kind() != frame.kind || record.logical_key()? != frame.logical_key {
                        return Err(Error::InvalidInput(
                            "account payload does not match its identity",
                        ));
                    }
                    Some(record)
                }
                Err(Error::InvalidInput(message)) if message.starts_with("unsupported ") => {
                    unsupported = true;
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        Ok(Decoded { logical_key: frame.logical_key, record: decoded, unsupported })
    }
    pub(super) fn encode(value: &Record, original: Option<&[u8]>) -> Result<Vec<u8>> {
        value.validate()?;
        let requirements = if matches!(value,
            Record::Invitation { invitation, .. }
                | Record::HostedInvitation { invitation, .. }
            if invitation.session.shared_volume.is_some())
        {
            vec![SHARED_SESSION.into()]
        } else {
            Vec::new()
        };
        let mut frame = Self {
            kind: value.kind().into(),
            logical_key: value.logical_key()?,
            authority: !matches!(value, Record::DisplayName { .. } | Record::AppVisibility { .. }),
            required_read: requirements.clone(),
            required_write: requirements,
            payload: Payload(record::encode(value)?),
            unsupported_header: false,
        };
        let bytes = if let Some(original) = original {
            let previous = Self::parse(original)?;
            Self::decode(original)?.admit()?;
            if previous.kind != frame.kind || previous.logical_key != frame.logical_key {
                return Err(Error::InvalidInput("account mutation changes record identity"));
            }
            frame.payload = Payload(record::rewrite(&previous.payload.0, value)?);
            record::rewrite(original, &frame)?
        } else {
            record::encode(&frame)?
        };
        if bytes.len() > MAX_PLAINTEXT_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests;
