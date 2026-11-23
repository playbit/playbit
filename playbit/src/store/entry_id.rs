use crate::store::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;

/// Stable volume entry identity. A name is never an identity.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[cfg_attr(feature = "app-host", rkyv(derive(Eq, PartialEq, Ord, PartialOrd)))]
#[cfg_attr(feature = "app-host", rkyv(bytecheck(verify)))]
pub struct EntryId([u8; 15]);

#[cfg(feature = "app-host")]
impl ArchivedEntryId {
    pub(crate) fn as_bytes(&self) -> &[u8; 15] {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<()> {
        EntryId::from_bytes(self.0).map(|_| ())
    }
}

impl EntryId {
    pub const fn as_bytes(&self) -> &[u8; 15] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; 15]) -> Result<Self> {
        if bytes == [0; 15] {
            return Err(Error::InvalidInput("entry ID must be nonzero"));
        }
        Ok(Self(bytes))
    }

    pub fn parse(value: &str) -> Result<Self> {
        Self::from_bytes(crate::identifier::decode(value).map_err(Error::InvalidInput)?)
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
    pub fn generate() -> Result<Self> {
        // A zero draw is rejected, rather than silently substituting a fixed ID.
        Self::from_bytes(crate::store::random_array::<15>()?)
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::identifier::Text(&self.0).fmt(f)
    }
}

impl fmt::Debug for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntryId({self})")
    }
}

impl Serialize for EntryId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EntryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn entry_ids_have_one_nonzero_spelling_and_exactly_fifteen_bytes() {
        // Check every byte and both text boundaries, including leading
        // zero bytes in a nonzero identity.
        for byte in 0..=u8::MAX {
            let mut bytes = [byte; 15];
            bytes[14] = 1;
            let id = EntryId::from_bytes(bytes).unwrap();
            let expected = crate::identifier::encode(&bytes);
            assert_eq!(id.to_string(), expected);
            assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{expected}\""));
        }
        let id = EntryId::from_bytes([0xab; 15]).unwrap();
        assert_eq!(id.as_bytes().len(), 15);
        assert_eq!(EntryId::parse(&id.to_string()).unwrap(), id);
        assert_eq!(
            serde_json::from_str::<EntryId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
        for invalid in ["0".repeat(30), "ab".repeat(16), "AB".repeat(15), "ab".repeat(14)] {
            assert!(EntryId::parse(&invalid).is_err());
        }
    }

    #[cfg(feature = "app-host")]
    #[test]
    fn archived_entry_identity_rejects_zero_before_deserialization() {
        use crate::engine::{Request, ipc::codec};
        let request = Request::EntryDetails {
            session: None,
            volume: crate::store::Id::from_bytes([1; 16]),
            entry: EntryId([0; 15]),
        };
        let bytes = codec::encode(&request, codec::MAX_ARCHIVE).unwrap();
        assert!(codec::access::<crate::engine::ArchivedRequest>(&bytes).is_err());
        assert!(rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).is_err());
    }
}
