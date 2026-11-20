//! Authorized query selection and bounded projected records, shared by native and web hosts.
use crate::store::{
    Error, Id, Result,
    object_contract::{EditStamp, FieldCell, ObjectFields, Value},
};
use playbit_api::abi::syscall::{MAX_DATA_BYTES, MAX_FIELDS, MAX_PAGE_OBJECTS, MAX_VALUES};
use serde::{
    Deserialize,
    de::{SeqAccess, Visitor},
};
use serde_json::value::RawValue;
use std::{fmt, mem::size_of};

// Rust 1.95 BTree nodes hold 11 entries and occupy <1 KiB for Id/FieldCell.
// With at most 256 fields, a split can allocate at most three such nodes.
// Reserve 4 KiB per inserted field for those allocations and the host's
// FieldChange vector, including its overlap while the map is consumed.
const FIELD_SCRATCH_BYTES: usize = 4096;

mod limits;
pub use limits::Limits;
pub(crate) use limits::Scratch;

#[derive(Clone, Debug, serde::Serialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct ObjectPage {
    pub objects: Vec<ObjectFields>,
    pub next: Option<Id>,
}

pub fn validate_projection(fields: &[Id], limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_OBJECTS as usize || fields.len() > MAX_FIELDS as usize {
        return Err(Error::LimitExceeded);
    }
    for (index, field) in fields.iter().enumerate() {
        if fields[..index].contains(field) {
            return Err(Error::InvalidInput("duplicate query field"));
        }
    }
    Ok(())
}

/// Check that a prepared projected record can be decoded within these same
/// query limits. Count serialized bytes before allocating, then reuse the actual
/// decoder/accounting. The caller still owns commit and input admission.
pub fn validate_record(record: &ObjectFields, limits: Limits) -> Result<()> {
    if record_bytes(record)?.checked_add(5).ok_or(Error::LimitExceeded)? > limits.output_bytes {
        return Err(Error::LimitExceeded);
    }
    let mut scratch = limits.start(1, record.fields.len())?;
    let mut values = 0;
    for cell in record.fields.values() {
        let mut count = EncodedLength { bytes: 0, limit: limits.scratch_bytes / 3 };
        serde_json::to_writer(&mut count, cell).map_err(|_| Error::LimitExceeded)?;
        scratch.raw(count.bytes, FIELD_SCRATCH_BYTES)?;
        let mut raw = Vec::with_capacity(count.bytes);
        serde_json::to_writer(&mut raw, cell)
            .map_err(|_| Error::Internal("could not encode an object field cell"))?;
        assert_eq!(raw.len(), count.bytes, "immutable cell encoding has its admitted length");
        decode_cell(&raw, &mut scratch, &mut values)?;
    }
    Ok(())
}
struct EncodedLength {
    bytes: usize,
    limit: usize,
}
impl std::io::Write for EncodedLength {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = encoded_length(self.bytes, bytes.len(), self.limit)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_length(before: usize, added: usize, limit: usize) -> Option<usize> {
    before.checked_add(added).filter(|length| *length <= limit)
}

/// Exact contribution to projected query output: ID prefix plus type and fields.
pub fn record_bytes(object: &ObjectFields) -> Result<usize> {
    let mut bytes = 36usize;
    let mut values = 0;
    if object.fields.len() > MAX_FIELDS as usize {
        return Err(Error::LimitExceeded);
    }
    for cell in object.fields.values() {
        bytes += 17;
        if let Some(items) = &cell.values {
            values += items.len();
            if values > MAX_VALUES as usize {
                return Err(Error::LimitExceeded);
            }
            bytes += 4;
            for value in items {
                let length = match value {
                    Value::Text(value) => value.len(),
                    Value::Bytes(value) => value.len(),
                    Value::Boolean(_) => 1,
                    Value::Integer(_) => 8,
                    Value::Float(value) if value.is_finite() => 8,
                    Value::Float(_) => {
                        return Err(Error::corrupt(
                            "object field contains a nonfinite floating point number",
                        ));
                    }
                    Value::Reference { .. } => 47,
                    Value::File { .. } => 31,
                };
                bytes = bytes.checked_add(5 + length).ok_or(Error::LimitExceeded)?;
                if bytes > MAX_DATA_BYTES {
                    return Err(Error::LimitExceeded);
                }
            }
        }
    }
    Ok(bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCell<'a> {
    stamp: EditStamp,
    #[serde(borrow)]
    values: Option<&'a RawValue>,
}

struct CountValues<'a>(&'a mut bool);
impl<'de> Visitor<'de> for CountValues<'_> {
    type Value = usize;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("field values")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<usize, A::Error> {
        let mut count = 0;
        while sequence.next_element::<&RawValue>()?.is_some() {
            count += 1;
            if count > MAX_VALUES as usize {
                *self.0 = true;
                return Err(serde::de::Error::custom("query value limit"));
            }
        }
        Ok(count)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredValueKind {
    Text,
    Boolean,
    Integer,
    Float,
    Bytes,
    File,
    Reference,
}

#[derive(Deserialize)]
struct RawStoredValue<'a> {
    kind: StoredValueKind,
    #[serde(borrow)]
    value: &'a RawValue,
}

fn decode_value(raw: &RawValue) -> std::result::Result<Value, serde_json::Error> {
    // Decode the envelope without buffering serde Content: JSON member order
    // must not turn a byte array into a large intermediate vector of enums.
    let value: RawStoredValue<'_> = serde_json::from_str(raw.get())?;
    let body = value.value.get();
    Ok(match value.kind {
        StoredValueKind::Text => Value::Text(serde_json::from_str(body)?),
        StoredValueKind::Boolean => Value::Boolean(serde_json::from_str(body)?),
        StoredValueKind::Integer => Value::Integer(serde_json::from_str(body)?),
        StoredValueKind::Float => Value::Float(serde_json::from_str(body)?),
        StoredValueKind::Bytes => Value::Bytes(serde_json::from_str(body)?),
        StoredValueKind::File => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct File {
                volume_id: Id,
                entry_id: crate::store::EntryId,
            }
            let file: File = serde_json::from_str(body)?;
            Value::File { volume_id: file.volume_id, entry_id: file.entry_id }
        }
        StoredValueKind::Reference => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Reference {
                volume_id: Id,
                database_id: crate::store::EntryId,
                object_id: Id,
            }
            let reference: Reference = serde_json::from_str(body)?;
            Value::Reference {
                volume_id: reference.volume_id,
                database_id: reference.database_id,
                object_id: reference.object_id,
            }
        }
    })
}

struct DecodeValues(usize);
impl<'de> Visitor<'de> for DecodeValues {
    type Value = Vec<Value>;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("field values")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<Vec<Value>, A::Error> {
        let mut values = Vec::with_capacity(self.0);
        while let Some(raw) = sequence.next_element::<&RawValue>()? {
            // The immutable raw sequence was counted before reserving storage.
            if values.len() == self.0 {
                return Err(serde::de::Error::custom("changed field values"));
            }
            values.push(decode_value(raw).map_err(serde::de::Error::custom)?);
        }
        Ok(values)
    }
}

pub(crate) fn decode_cell(
    raw: &[u8],
    scratch: &mut Scratch,
    total_values: &mut usize,
) -> Result<FieldCell> {
    use serde::Deserializer;
    // Native ValueRef borrows redb's read guard. Web already caps/copies one
    // storage value. Charge raw bytes three times before parsing: that web copy,
    // retained strings/byte vectors, and temporary JSON escape/copy space.
    // Native guards borrow their bytes; no new raw copy is made here.
    let raw_charge = scratch.raw(raw.len(), FIELD_SCRATCH_BYTES)?;
    let cell: RawCell<'_> = serde_json::from_slice(raw).map_err(Error::decode::<RawCell>)?;
    let values = if let Some(raw_values) = cell.values {
        let mut exceeded = false;
        let count = serde_json::Deserializer::from_str(raw_values.get())
            .deserialize_seq(CountValues(&mut exceeded))
            .map_err(|_| {
                if exceeded {
                    Error::LimitExceeded
                } else {
                    Error::corrupt("invalid object field value sequence")
                }
            })?;
        *total_values += count;
        if *total_values > MAX_VALUES as usize {
            return Err(Error::LimitExceeded);
        }
        // Reserve both store and host vectors while values move between them;
        // the per-field charge includes map/field bookkeeping and parser state.
        let charge = raw_charge + count * size_of::<Value>() * 2;
        scratch.charge(charge)?;
        Some(
            serde_json::Deserializer::from_str(raw_values.get())
                .deserialize_seq(DecodeValues(count))
                .map_err(|_| Error::corrupt("invalid object field values"))?,
        )
    } else {
        scratch.charge(raw_charge)?;
        None
    };
    Ok(FieldCell { stamp: cell.stamp, values })
}

/// Binary storage projection charges raw scratch and value containers before
/// allocating. Unselected cells stay borrowed in their original record.
pub(crate) fn decode_record_cell(
    raw: crate::store::catalog::record::View<'_>,
    scratch: &mut Scratch,
    total_values: &mut usize,
) -> Result<FieldCell> {
    use crate::store::catalog::record::{self, Codec, Mode};
    let raw_charge = scratch.raw(raw.0.len(), FIELD_SCRATCH_BYTES)?;
    let fields = raw.fields(Mode::Write, &[1, 2])?;
    let values = fields.get(2)?;
    let count = if values.0 == [record::NULL] { 0 } else { values.sequence()?.count()? };
    *total_values = total_values.checked_add(count).ok_or(Error::LimitExceeded)?;
    if *total_values > MAX_VALUES as usize {
        return Err(Error::LimitExceeded);
    }
    scratch.charge(
        raw_charge.checked_add(count * size_of::<Value>() * 2).ok_or(Error::LimitExceeded)?,
    )?;
    let values = if values.0 == [record::NULL] {
        None
    } else {
        let mut result = Vec::with_capacity(count);
        let mut sequence = values.sequence()?;
        while let Some(value) = sequence.next_value()? {
            result.push(Value::read(value, Mode::Write)?);
        }
        Some(result)
    };
    Ok(FieldCell { stamp: fields.required(1, Mode::Write)?, values })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded decoder behavior.")]
mod tests {
    use super::*;
    fn id(byte: u8) -> Id {
        Id::from_bytes([byte; 16])
    }
    #[test]
    fn projected_values_accept_either_json_member_order_without_content_buffering() {
        let values = [
            Value::Text("escaped\ntext".into()),
            Value::Boolean(true),
            Value::Integer(-7),
            Value::Float(0.25),
            Value::Bytes(vec![0; 200_000]),
            Value::File {
                volume_id: id(9),
                entry_id: crate::store::EntryId::from_bytes([7; 15]).unwrap(),
            },
            Value::Reference {
                volume_id: id(9),
                database_id: crate::store::EntryId::from_bytes([7; 15]).unwrap(),
                object_id: id(8),
            },
        ];
        for value in values {
            let encoded = serde_json::to_value(&value).unwrap();
            let reversed =
                format!(r#"{{"value":{},"kind":{}}}"#, encoded["value"], encoded["kind"]);
            let raw = RawValue::from_string(reversed).unwrap();
            assert_eq!(decode_value(&raw).unwrap(), value);
        }
        let invalid = RawValue::from_string(r#"{"value":1e999,"kind":"float"}"#.into()).unwrap();
        assert!(decode_value(&invalid).is_err());
    }

    #[test]
    fn projection_and_framing_limits_reject_duplicates_and_overflow() {
        assert!(validate_projection(&[id(1), id(1)], 1).is_err());
        assert!(validate_projection(&[], 0).is_err());
        assert!(validate_projection(&[], 65).is_err());
        assert!(Limits::ABI.start(usize::MAX, 1).is_err());
        assert!(Limits::ABI.start(1, usize::MAX).is_err());
        for limits in [
            Limits { output_bytes: 4, ..Limits::ABI },
            Limits { output_bytes: MAX_DATA_BYTES + 1, ..Limits::ABI },
            Limits { scratch_bytes: 0, ..Limits::ABI },
            Limits {
                scratch_bytes: playbit_api::abi::syscall::MAX_QUERY_SCRATCH_BYTES + 1,
                ..Limits::ABI
            },
        ] {
            assert!(limits.start(1, 0).is_err());
        }
    }
    #[test]
    fn decoder_checks_raw_and_value_bounds_before_allocating_content() {
        let stamp = EditStamp { clock: 1, device_id: id(9) };
        let raw = serde_json::to_vec(&FieldCell {
            stamp,
            values: Some(vec![Value::Text(String::new()); MAX_VALUES as usize + 1]),
        })
        .unwrap();
        assert!(matches!(
            decode_cell(&raw, &mut Limits::ABI.start(1, 1).unwrap(), &mut 0),
            Err(Error::LimitExceeded)
        ));
        let raw = vec![b' '; playbit_api::abi::syscall::MAX_QUERY_SCRATCH_BYTES / 2];
        assert!(matches!(
            decode_cell(&raw, &mut Limits::ABI.start(1, 1).unwrap(), &mut 0),
            Err(Error::LimitExceeded)
        ));
    }
    #[test]
    fn complete_reference_addresses_have_exact_output_charges() {
        for (value, length) in [
            (
                Value::File {
                    volume_id: id(9),
                    entry_id: crate::store::EntryId::from_bytes([7; 15]).unwrap(),
                },
                31,
            ),
            (
                Value::Reference {
                    volume_id: id(9),
                    database_id: crate::store::EntryId::from_bytes([7; 15]).unwrap(),
                    object_id: id(8),
                },
                47,
            ),
        ] {
            let record = ObjectFields {
                object_id: id(1),
                type_id: id(2),
                fields: std::collections::BTreeMap::from([(
                    id(3),
                    FieldCell {
                        stamp: EditStamp { clock: 1, device_id: id(9) },
                        values: Some(vec![value]),
                    },
                )]),
            };
            assert_eq!(record_bytes(&record).unwrap(), 36 + 17 + 4 + 5 + length);
        }
    }
}
