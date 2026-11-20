use crate::store::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;

/// Stable identity for an Archipelago object.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
#[cfg_attr(feature = "app-host", rkyv(derive(Eq, PartialEq, Ord, PartialOrd)))]
pub struct Id(pub(crate) [u8; 16]);

impl Id {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Id {
        Id(bytes)
    }

    /// Parse the canonical base58 encoding of exactly 16 bytes.
    pub fn parse(text: &str) -> Result<Id> {
        crate::identifier::decode(text).map(Id).map_err(Error::InvalidInput)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::identifier::Text(&self.0).fmt(f)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({self})")
    }
}

impl Serialize for Id {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Id::parse(&value).map_err(de::Error::custom)
    }
}

impl From<[u8; 16]> for Id {
    fn from(bytes: [u8; 16]) -> Self {
        Self::from_bytes(bytes)
    }
}
impl From<&[u8; 16]> for Id {
    fn from(bytes: &[u8; 16]) -> Self {
        Self::from_bytes(*bytes)
    }
}

#[cfg(test)]
mod base58_tests {
    use super::*;

    #[test]
    fn base58_round_trips_every_sixteen_bit_suffix() {
        // Exhaust the small-value range, including every change in leading-zero count.
        for value in 0..=u16::MAX {
            let mut bytes = [0; 16];
            bytes[14..].copy_from_slice(&value.to_be_bytes());
            let id = Id::from_bytes(bytes);
            assert_eq!(Id::parse(&id.to_string()).expect("canonical ID"), id);
        }
    }

    #[test]
    fn canonical_ids_round_trip_including_leading_zero_bytes() {
        // Exercise every leading-zero count and known Bitcoin-alphabet vectors.
        for zeros in 0..=16 {
            let mut bytes = [255; 16];
            bytes[..zeros].fill(0);
            let id = Id::from_bytes(bytes);
            assert_eq!(Id::parse(&id.to_string()).expect("canonical ID"), id);
        }
        assert_eq!(Id::from_bytes([0; 16]).to_string(), "1111111111111111");
        assert_eq!(Id::from_bytes([255; 16]).to_string(), "YcVfxkQb6JRzqk5kF2tNLv");
        for text in ["", "1", "11111111111111111", "0", "O", "I", "l", "zzzzzzzzzzzzzzzzzzzzzz"] {
            assert!(Id::parse(text).is_err(), "{text}");
        }
    }
}
