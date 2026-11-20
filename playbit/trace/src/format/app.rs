//! App-controlled metadata is private data. Decode its bounded labels separately
//! from the fixed native catalog; absence never supplies an invented name.
use super::{DecodeError, integer, read_integer};
pub const APP_NAMES: usize = 256;
pub const APP_LABEL_BYTES: usize = 256;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AppNameKind {
    Span = 1,
    Mark = 2,
}
impl TryFrom<u64> for AppNameKind {
    type Error = DecodeError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Span),
            2 => Ok(Self::Mark),
            _ => Err(DecodeError::Invalid),
        }
    }
}
pub fn valid_app_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.".contains(&b))
}
fn text<'a>(bytes: &mut &'a [u8], maximum: usize) -> Result<&'a str, DecodeError> {
    let length = read_integer(bytes)?;
    if length == 0 || length > maximum as u64 || length > bytes.len() as u64 {
        return Err(DecodeError::Invalid);
    }
    let value = std::str::from_utf8(&bytes[..length as usize]).map_err(|_| DecodeError::Invalid)?;
    *bytes = &bytes[length as usize..];
    Ok(value)
}
/// Private host-supplied identities. Presence is explicit; all-zero bytes do
/// not stand in for unavailable detail and these IDs never confer authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppIdentity {
    pub session: Option<[u8; 32]>,
    pub build: Option<[u8; 32]>,
}
impl AppIdentity {
    pub(crate) fn decode(bytes: &mut &[u8]) -> Result<Self, DecodeError> {
        let flags = read_integer(bytes)?;
        if flags > 3 {
            return Err(DecodeError::Invalid);
        }
        let mut values = [None; 2];
        for (index, value) in values.iter_mut().enumerate() {
            if flags & (1 << index) != 0 {
                let prefix = bytes.get(..32).ok_or(DecodeError::Invalid)?;
                *value = Some(prefix.try_into().map_err(|_| DecodeError::Invalid)?);
                *bytes = &bytes[32..];
            }
        }
        Ok(Self { session: values[0], build: values[1] })
    }
}
pub struct AppDefinition<'a> {
    pub scope: u32,
    pub identity: AppIdentity,
    pub instance: [u8; 16],
    pub label: &'a str,
    pub names: Vec<(AppNameKind, &'a str)>,
}
impl<'a> AppDefinition<'a> {
    pub fn decode(mut bytes: &'a [u8]) -> Result<Self, DecodeError> {
        if bytes.len() > super::MAX_PAYLOAD_BYTES {
            return Err(DecodeError::Invalid);
        }
        let scope = read_integer(&mut bytes)?;
        if scope == 0 || scope > u32::MAX as u64 {
            return Err(DecodeError::Invalid);
        }
        let identity = AppIdentity::decode(&mut bytes)?;
        let instance = bytes
            .get(..16)
            .ok_or(DecodeError::Invalid)?
            .try_into()
            .map_err(|_| DecodeError::Invalid)?;
        bytes = &bytes[16..];
        let label = text(&mut bytes, APP_LABEL_BYTES)?;
        let count = read_integer(&mut bytes)?;
        if count > APP_NAMES as u64 {
            return Err(DecodeError::Invalid);
        }
        let mut names = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let kind = AppNameKind::try_from(read_integer(&mut bytes)?)?;
            let name = text(&mut bytes, 48)?;
            if !valid_app_name(name) || names.iter().any(|(_, old)| *old == name) {
                return Err(DecodeError::Invalid);
            }
            names.push((kind, name));
        }
        if !bytes.is_empty() {
            return Err(DecodeError::Invalid);
        }
        Ok(Self { scope: scope as u32, identity, instance, label, names })
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppState {
    pub scope: u32,
    pub refused: u64,
    /// 0 active, 1 owner retired, 2 owner unavailable at confirmed source exit.
    pub status: u8,
}
impl AppState {
    pub fn decode(mut bytes: &[u8]) -> Result<Self, DecodeError> {
        let scope = read_integer(&mut bytes)?;
        let refused = read_integer(&mut bytes)?;
        let status = read_integer(&mut bytes)?;
        if !bytes.is_empty() || scope == 0 || scope > u32::MAX as u64 || status > 2 {
            return Err(DecodeError::Invalid);
        }
        Ok(Self { scope: scope as u32, refused, status: status as u8 })
    }
    pub fn encode(self, output: &mut Vec<u8>) -> Result<(), DecodeError> {
        if self.scope == 0 || self.status > 2 {
            return Err(DecodeError::Invalid);
        }
        for value in [u64::from(self.scope), self.refused, u64::from(self.status)] {
            integer(value, output)?;
        }
        Ok(())
    }
}
