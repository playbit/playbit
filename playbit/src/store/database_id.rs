//! Database transport identities. A volume supplies the addressing context;
//! named database identities are the complete stable 15-byte entry ID.
use super::{EntryId, Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum DatabaseId {
    Index,
    Revocations,
    Entry(EntryId),
}

impl DatabaseId {
    pub const WIRE_BYTES: usize = 16;

    /// Public Merkle parameters for the hosted participant view. These never
    /// authenticate requests or encrypt any World or peer-transfer contents.
    pub fn world_projection_key(volume: super::Id) -> [u8; 32] {
        blake3::derive_key("app.playbit.world-replica.v1", volume.as_bytes())
    }

    pub fn hash_key(self, volume_key: &[u8; 32], volume: super::Id) -> [u8; 32] {
        let mut hash = blake3::Hasher::new_keyed(volume_key);
        hash.update(b"app.playbit.database.v1\0");
        hash.update(volume.as_bytes());
        hash.update(&self.to_bytes());
        *hash.finalize().as_bytes()
    }

    pub fn to_bytes(self) -> [u8; Self::WIRE_BYTES] {
        let mut bytes = [0; Self::WIRE_BYTES];
        match self {
            Self::Index => {}
            Self::Revocations => bytes[0] = 1,
            Self::Entry(id) => {
                bytes[0] = 2;
                bytes[1..].copy_from_slice(id.as_bytes());
            }
        }
        bytes
    }

    pub fn from_bytes(bytes: [u8; Self::WIRE_BYTES]) -> Result<Self> {
        match bytes[0] {
            0 if bytes[1..].iter().all(|byte| *byte == 0) => Ok(Self::Index),
            1 if bytes[1..].iter().all(|byte| *byte == 0) => Ok(Self::Revocations),
            2 => Ok(Self::Entry(EntryId::from_bytes(
                bytes[1..].try_into().expect("fixed-width entry ID suffix"),
            )?)),
            _ => Err(Error::InvalidInput("invalid database identity")),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "index" => Ok(Self::Index),
            "revocations" => Ok(Self::Revocations),
            _ => Ok(Self::Entry(EntryId::parse(text)?)),
        }
    }

    /// Persistent identity-derived file names, independent of entry paths.
    pub fn file_name(self) -> String {
        match self {
            Self::Index => ".index.kvdb".into(),
            Self::Revocations => ".revocations.kvdb".into(),
            Self::Entry(id) => format!("{id}.kvdb"),
        }
    }
}

impl fmt::Display for DatabaseId {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index => output.write_str("index"),
            Self::Revocations => output.write_str("revocations"),
            Self::Entry(id) => id.fmt(output),
        }
    }
}
impl Serialize for DatabaseId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for DatabaseId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert valid identity fixtures.")]
mod tests {
    use super::*;

    #[test]
    fn complete_entry_identity_round_trips_without_numeric_aliases() {
        for id in [
            DatabaseId::Index,
            DatabaseId::Revocations,
            DatabaseId::Entry(EntryId::from_bytes([7; 15]).unwrap()),
        ] {
            assert_eq!(DatabaseId::from_bytes(id.to_bytes()).unwrap(), id);
            assert_eq!(DatabaseId::parse(&id.to_string()).unwrap(), id);
            assert_eq!(
                serde_json::from_str::<DatabaseId>(&serde_json::to_string(&id).unwrap()).unwrap(),
                id
            );
        }
        for invalid in [
            "1",
            "2",
            "3",
            "4",
            "INDEX",
            "000000000000000000000000000000",
            "77777777777777777777777777777A",
        ] {
            assert!(DatabaseId::parse(invalid).is_err());
        }
        assert!(serde_json::from_str::<DatabaseId>("1").is_err());
        let mut invalid = DatabaseId::Index.to_bytes();
        invalid[15] = 1;
        assert!(DatabaseId::from_bytes(invalid).is_err());
        assert!(DatabaseId::from_bytes([2; 16]).is_ok());
        let mut zero_entry = [0; 16];
        zero_entry[0] = 2;
        assert!(DatabaseId::from_bytes(zero_entry).is_err());
    }
}
