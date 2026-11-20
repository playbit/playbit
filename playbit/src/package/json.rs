//! Small canonical JSON contract for descriptive package metadata. Identity is
//! checked against the complete value, never a projection into current structs.
use serde::{
    Serialize,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, io::Write};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const MAX_DEPTH: usize = 16;
const MAX_VALUES: usize = 8192;

struct ReadValue<'a> {
    remaining: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for ReadValue<'_> {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        input: D,
    ) -> std::result::Result<Value, D::Error> {
        if *self.remaining == 0 || self.depth > MAX_DEPTH {
            return Err(serde::de::Error::custom("package JSON exceeds structural limits"));
        }
        *self.remaining -= 1;
        input.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for ReadValue<'_> {
    type Value = Value;

    fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("bounded JSON with integer numbers and unique object keys")
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<Value, E> {
        Ok(value.into())
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<Value, E> {
        Ok(value.into())
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(value.into()))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> std::result::Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence
            .next_element_seed(ReadValue { remaining: self.remaining, depth: self.depth + 1 })?
        {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut values = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate package JSON key"));
            }
            let value = map
                .next_value_seed(ReadValue { remaining: self.remaining, depth: self.depth + 1 })?;
            values.insert(key, value);
        }
        // Explicit ordering also holds if serde_json's preserve_order is enabled.
        Ok(Value::Object(values.into_iter().collect()))
    }
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other("package JSON exceeds byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn parse(bytes: &[u8], limit: usize) -> Result<Value> {
    if bytes.len() > limit {
        return Err("package JSON exceeds byte limit".into());
    }
    let mut input = serde_json::Deserializer::from_slice(bytes);
    let mut remaining = MAX_VALUES;
    let value = ReadValue { remaining: &mut remaining, depth: 0 }.deserialize(&mut input)?;
    input.end()?;
    Ok(value)
}
fn serialize(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let mut writer = BoundedBytes { bytes: Vec::new(), limit };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}
pub(super) fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let value = parse(&serialize(value, limit)?, limit)?;
    serialize(&value, limit)
}
pub(super) fn decode(bytes: &[u8], limit: usize) -> Result<Value> {
    let value = parse(bytes, limit)?;
    if serialize(&value, limit)? != bytes {
        return Err("noncanonical package JSON".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_values_include_unknown_fields_and_reject_ambiguous_values() {
        let bytes = br#"{"a":{"future":[null,true,-2,18446744073709551615]},"z":"text"}"#;
        let value = decode(bytes, 4096).expect("complete value admitted");
        assert_eq!(encode(&value, 4096).expect("stable bytes"), bytes);
        for invalid in [
            br#"{"a":1,"a":2}"#.as_slice(),
            br#"{"a":{"b":1,"b":2}}"#,
            br#"{"z":1,"a":2}"#,
            br#"{"a":1.0}"#,
            br#"{"a":1e0}"#,
            br#"{"a":-0}"#,
            br#"{"a": 1}"#,
        ] {
            assert!(decode(invalid, 4096).is_err(), "{invalid:?}");
        }
        assert!(decode(bytes, bytes.len() - 1).is_err());
        let deep = format!("{}0{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(decode(deep.as_bytes(), 4096).is_err());
        let wide = format!("[{}]", vec!["0"; MAX_VALUES].join(","));
        assert!(decode(wide.as_bytes(), 32 * 1024).is_err());
    }
}
