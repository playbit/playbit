//! Canonical packed causal IDs. Encoding the closed set as one unpadded
//! base64url string avoids per-ID JSON objects without dropping any history.
use super::{EntryId, MAX_FACTS, validate_context};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserializer, Serializer, de};

pub fn serialize<S: Serializer>(ids: &[EntryId], serializer: S) -> Result<S::Ok, S::Error> {
    validate_context(ids).map_err(serde::ser::Error::custom)?;
    let mut bytes = Vec::with_capacity(ids.len() * 15);
    for id in ids {
        bytes.extend_from_slice(id.as_bytes());
    }
    serializer.serialize_str(&URL_SAFE_NO_PAD.encode(&bytes))
}
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<EntryId>, D::Error> {
    struct Context;
    impl de::Visitor<'_> for Context {
        type Value = Vec<EntryId>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded canonical packed namespace context")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            // Each 15-byte ID encodes to20 characters with no partial quantum.
            // Bound the count before allocating the decoded bytes or ID vector.
            if !value.len().is_multiple_of(20) || value.len() / 20 >= MAX_FACTS {
                return Err(E::custom("invalid namespace context length"));
            }
            let bytes = URL_SAFE_NO_PAD.decode(value).map_err(E::custom)?;
            if bytes.len() != value.len() / 20 * 15 {
                return Err(E::custom("invalid namespace context encoding"));
            }
            let ids = bytes
                .as_chunks::<15>()
                .0
                .iter()
                .map(|bytes| EntryId::from_bytes(*bytes).map_err(E::custom))
                .collect::<Result<Vec<_>, _>>()?;
            validate_context(&ids).map_err(E::custom)?;
            Ok(ids)
        }
    }
    deserializer.deserialize_str(Context)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert canonical context round trips.")]
mod tests {
    use super::*;
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Record(#[serde(with = "super")] Vec<EntryId>);
    #[test]
    fn compact_context_is_exact_bounded_and_canonical() {
        // Exercise full supported count and malformed encodings at the decoder,
        // independently of the outer source record's smaller byte ceiling.
        let ids: Vec<_> = (1..MAX_FACTS as u64)
            .map(|number| {
                let mut bytes = [0; 15];
                bytes[7..].copy_from_slice(&number.to_be_bytes());
                EntryId::from_bytes(bytes).unwrap()
            })
            .collect();
        let encoded = serde_json::to_string(&Record(ids.clone())).unwrap();
        assert_eq!(encoded.len(), ids.len() * 20 + 2);
        assert_eq!(serde_json::from_str::<Record>(&encoded).unwrap().0, ids);
        for invalid in ["[]", "\"A\"", "\"====================\""] {
            assert!(serde_json::from_str::<Record>(invalid).is_err());
        }
        let duplicate =
            URL_SAFE_NO_PAD.encode([ids[0].as_bytes().as_slice(), ids[0].as_bytes()].concat());
        assert!(serde_json::from_str::<Record>(&format!("\"{duplicate}\"")).is_err());
        let oversized = format!("\"{}\"", "A".repeat(MAX_FACTS * 20));
        assert!(serde_json::from_str::<Record>(&oversized).is_err());
    }
}
