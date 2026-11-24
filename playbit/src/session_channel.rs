//! Shared ephemeral session channel identities and events on all platforms.
use playbit_api::abi::syscall::Error;
use serde::{Deserialize, Serialize};
pub const SESSION: u8 = 1;
pub const DISPLAY_NAMES_FEATURE: &str = "session.display-names.v1";
pub const MAX_NAME_BYTES: usize = 223;
pub const MAX_CHANNELS: usize = 16;
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;
pub const SESSION_QUEUE_BYTES: usize = 64 * 1024;
pub const ACCOUNT_QUEUE_BYTES: usize = 256 * 1024;
pub const MAX_PARTICIPANTS: usize = 64;
pub const SOURCE_BYTES: usize = 80;
pub const PROOF_BYTES: usize = 64;
pub use playbit_api::abi::environment::DISPLAY_NAME_BYTES;
// Even escaping each display-name byte as two JSON bytes fits a 64-entry roster.
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
const _: () = assert!(32 + MAX_NAME_BYTES == u8::MAX as usize);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub account_id: String,
    pub device_id: String,
    pub instance_id: String,
    pub build_id: String,
}
impl Participant {
    pub fn decode(source: &[u8]) -> Result<Self, &'static str> {
        let source: &[u8; SOURCE_BYTES] =
            source.try_into().map_err(|_| "invalid channel source")?;
        Ok(Self {
            account_id: crate::identifier::encode(&source[..16]),
            device_id: crate::identifier::encode(&source[16..32]),
            instance_id: crate::identifier::encode(&source[32..48]),
            build_id: crate::identifier::encode(&source[48..]),
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let mut source = Vec::with_capacity(SOURCE_BYTES);
        source.extend(crate::identifier::decode::<16>(&self.account_id)?);
        source.extend(crate::identifier::decode::<16>(&self.device_id)?);
        source.extend(crate::identifier::decode::<16>(&self.instance_id)?);
        source.extend(crate::identifier::decode::<32>(&self.build_id)?);
        Ok(source)
    }
}
/// Account label attached to a transport-authenticated roster entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(deny_unknown_fields)]
pub struct PresentParticipant {
    #[serde(flatten)]
    pub participant: Participant,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
}

/// Empty denotes an unavailable display name or a live clear.
pub fn display_name(bytes: &[u8]) -> Result<&str, &'static str> {
    if bytes.len() > DISPLAY_NAME_BYTES {
        return Err("display name too long");
    }
    let name = std::str::from_utf8(bytes).map_err(|_| "invalid display name UTF-8")?;
    if !name.is_empty() {
        crate::store::account::validate_display_name(name).map_err(|_| "invalid display name")?;
    }
    Ok(name)
}

/// The fixed-width initial field distinguishes an empty named roster from an ID-only join.
pub fn subscription_fields(body: &[u8]) -> Result<(&[u8], Option<&str>), &'static str> {
    let (app, field) = match body.len() {
        0 | 32 => return Ok((body, None)),
        DISPLAY_NAME_BYTES => (&[][..], body),
        length if length == 32 + DISPLAY_NAME_BYTES => body.split_at(32),
        _ => return Err("invalid membership subscription payload"),
    };
    Ok((app, Some(display_name_field(field)?)))
}

/// A named join or an in-place upgrade carries the same fixed-width field.
pub fn display_name_field(field: &[u8]) -> Result<&str, &'static str> {
    if field.len() != DISPLAY_NAME_BYTES {
        return Err("invalid display name field width");
    }
    let end = field.iter().rposition(|&byte| byte != 0).map_or(0, |index| index + 1);
    display_name(&field[..end])
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Presence { participants: Vec<PresentParticipant> },
    UpdateAvailable,
}

pub fn destination(session: &[u8; 32], name: &str) -> Result<Vec<u8>, &'static str> {
    validate_name(name)?;
    let mut result = Vec::with_capacity(32 + name.len());
    result.extend_from_slice(session);
    result.extend_from_slice(name.as_bytes());
    Ok(result)
}
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err("channel name must contain 1 to 223 UTF-8 bytes");
    }
    Ok(())
}
pub fn command_rights(action: u32) -> Result<crate::capability::Rights, Error> {
    use crate::capability::Rights;
    match action {
        1 => Ok(Rights::WRITE),
        2 | 3 => Ok(Rights::READ),
        4 => Ok(Rights::NONE),
        _ => Err(Error::InvalidArgument),
    }
}
pub fn validate_command(name: &str, action: u32, bytes: usize) -> Result<(), Error> {
    validate_name(name).map_err(|_| Error::InvalidArgument)?;
    command_rights(action)?;
    if action != 1 && bytes != 0 {
        return Err(Error::InvalidArgument);
    }
    if bytes > MAX_MESSAGE_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(())
}

/// Authenticates the exact destination, authenticated sender and publication ID.
pub fn associated_data(
    destination: &[u8],
    source: &[u8; SOURCE_BYTES],
    message: &[u8; 16],
) -> Result<Vec<u8>, &'static str> {
    validate_destination(destination)?;
    if destination.len() == 32 {
        return Err("own channel cannot carry application messages");
    }
    Ok([
        b"app.playbit.session.channel.v1".as_slice(),
        &(destination.len() as u16).to_le_bytes(),
        destination,
        source,
        message,
    ]
    .concat())
}
pub enum Incoming<'a> {
    Message { sender: Participant, message: [u8; 16], sealed: &'a [u8] },
    Control(Control),
    Overflow,
}
/// Validate the AP envelope once before control parsing or authenticated decryption.
pub fn incoming<'a>(
    expected: &[u8],
    kind: u8,
    destination: &[u8],
    source: &[u8],
    body: &'a [u8],
) -> Result<Incoming<'a>, &'static str> {
    validate_destination(expected)?;
    if destination != expected {
        return Err("channel frame scope mismatch");
    }
    if kind == 200 && source.is_empty() && body == (-9i32).to_le_bytes() {
        return Ok(Incoming::Overflow);
    }
    if kind != 3 || body.len() > MAX_CONTROL_BYTES {
        return Err("invalid channel publication");
    }
    if expected.len() > 32 {
        let sender = Participant::decode(source)?;
        if !(44..=MAX_MESSAGE_BYTES + 44).contains(&body.len()) {
            return Err("invalid channel message length");
        }
        return Ok(Incoming::Message {
            sender,
            message: body[..16].try_into().expect("message width"),
            sealed: &body[16..],
        });
    }
    if !source.is_empty() {
        return Err("untrusted session control message");
    }
    let control: Control = serde_json::from_slice(body).map_err(|_| "invalid session control")?;
    if let Control::Presence { participants } = &control {
        if participants.len() > MAX_PARTICIPANTS {
            return Err("presence capacity exceeded");
        }
        for participant in participants {
            participant.participant.encode()?;
            display_name(participant.display_name.as_bytes())?;
        }
    }
    Ok(Incoming::Control(control))
}
pub fn validate_destination(destination: &[u8]) -> Result<&[u8; 32], &'static str> {
    if !(32..=255).contains(&destination.len()) {
        return Err("invalid session channel destination");
    }
    std::str::from_utf8(&destination[32..]).map_err(|_| "invalid UTF-8 channel name")?;
    destination[..32].try_into().map_err(|_| "invalid session channel identity")
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Presence { participants: Vec<PresentParticipant> },
    Message { channel: String, sender: Participant, message_id: String, data: Vec<u8> },
}

#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
pub(crate) fn validate_events(
    events: &[Event],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if events.len() > 64 {
        return Err("too many channel events".into());
    }
    let mut bytes = 0;
    for event in events {
        bytes += match event {
            Event::Presence { participants } => {
                if participants.len() > MAX_PARTICIPANTS {
                    return Err("too many participants".into());
                }
                let mut size = 0;
                for member in participants {
                    member.participant.encode()?;
                    display_name(member.display_name.as_bytes())?;
                    size += SOURCE_BYTES + member.display_name.len();
                }
                size
            }
            Event::Message { channel: name, sender, message_id, data } => {
                if data.len() > MAX_MESSAGE_BYTES {
                    return Err("channel message too large".into());
                }
                destination(&[0; 32], name)?;
                sender.encode()?;
                crate::store::Id::parse(message_id)?;
                data.len() + name.len() + SOURCE_BYTES + 16
            }
        };
        if bytes > SESSION_QUEUE_BYTES {
            return Err("channel event batch exceeds queue budget".into());
        }
    }
    Ok(())
}
