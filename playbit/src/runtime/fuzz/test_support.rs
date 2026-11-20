//! Deterministic sandbox state and memory validators for operation fuzzing.
//! No filesystem, network or native presentation is available to this host.
use super::*;
#[cfg(feature = "wasm-runtime")]
mod operations;
#[cfg(feature = "wasm-runtime")]
pub use operations::wasm_operations;

/// Fuzz the replacement scalar ABI's nested input decoder without a VM or host.
/// The first 24 bytes select field offset/count/stride, output offset/presence and fuel;
/// the remaining bytes are linear memory. Successful decoded values stay bounded.
pub fn wasm32_memory(bytes: &[u8]) {
    use crate::runtime::linear_memory::{Memory, input};
    if bytes.len() < 24 || bytes.len() > 2 * abi::MAX_DATA_BYTES {
        return;
    }
    let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("header"));
    let mut data = bytes[24..].to_vec();
    let mut memory = Memory { data: &mut data, fuel: Some(u64::from(word(20))) };
    let output = if word(16) != 0 {
        match memory.admit(word(12), 1, 16, 1, 1) {
            Ok(output) => Some(output),
            Err(_) => return,
        }
    } else {
        None
    };
    if let Ok(fields) = input::changes(&mut memory, word(0), word(4), word(8), output) {
        assert!(fields.len() <= abi::MAX_FIELDS as usize);
        let values = fields.iter().filter_map(|field| field.values.as_ref()).flatten();
        assert!(values.clone().count() <= abi::MAX_VALUES as usize);
        let payload: usize = values
            .map(|value| match value {
                Value::Text(text) => text.len(),
                Value::Bytes(bytes) => bytes.len(),
                Value::Boolean(_) => 1,
                Value::Integer(_) | Value::Float(_) => 8,
                Value::Reference { .. } => 47,
                Value::File { .. } => 31,
            })
            .sum();
        assert!(payload <= abi::MAX_DATA_BYTES);
    }
    assert_eq!(data, bytes[24..], "input decoding never writes guest memory");
}

/// Fuzz the object result descriptor and all three output-array admissions.
/// Output writes consume checked semantic host results, qualified separately.
pub fn wasm32_outputs(bytes: &[u8]) {
    use crate::runtime::linear_memory::{
        Memory, disjoint,
        layout::data::{field, object, value},
        output::Buffer,
    };
    if bytes.len() < 24 || bytes.len() > 2 * abi::MAX_DATA_BYTES {
        return;
    }
    let pointer = u32::from_le_bytes(bytes[..4].try_into().expect("header"));
    let fuel = u32::from_le_bytes(bytes[20..24].try_into().expect("fuel"));
    let mut data = bytes[24..].to_vec();
    let mut memory = Memory { data: &mut data, fuel: Some(u64::from(fuel)) };
    let result = (|| -> HostResult<()> {
        let descriptor = memory.record(pointer, object::LAYOUT, false, None)?;
        let fields = Buffer::records(
            &mut memory,
            descriptor.sub(object::FIELDS_DATA, 16),
            field::LAYOUT,
            abi::MAX_FIELDS,
        )?;
        let values = Buffer::records(
            &mut memory,
            descriptor.sub(object::VALUES_DATA, 16),
            value::LAYOUT,
            abi::MAX_VALUES,
        )?;
        let bytes = Buffer::admit(
            &mut memory,
            descriptor.sub(object::BYTES_DATA, 12),
            1,
            1,
            abi::MAX_DATA_BYTES as u32,
        )?;
        disjoint(&[descriptor, fields.data, values.data, bytes.data])?;
        assert!(fields.capacity <= abi::MAX_FIELDS);
        assert!(values.capacity <= abi::MAX_VALUES);
        assert!(bytes.capacity as usize <= abi::MAX_DATA_BYTES);
        fields.count(memory.data, 1);
        values.count(memory.data, 1);
        bytes.count(memory.data, 1);
        Ok(())
    })();
    let mut expected = bytes[24..].to_vec();
    if result.is_ok() {
        for offset in [object::FIELDS_COUNT, object::VALUES_COUNT, object::BYTES_COUNT] {
            let start = pointer as usize + offset;
            expected[start..start + 4].copy_from_slice(&1_u32.to_le_bytes());
        }
    }
    assert_eq!(data, expected, "only admitted result counts may change");
}

/// Fuzz nested schema arrays, aggregate budgets, flags, tags and UTF-8 names.
pub fn wasm32_schema(bytes: &[u8]) {
    use crate::runtime::linear_memory::{Memory, schema};
    if bytes.len() < 24 || bytes.len() > 2 * abi::MAX_DATA_BYTES {
        return;
    }
    let word =
        |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("header"));
    let mut data = bytes[24..].to_vec();
    let mut memory = Memory { data: &mut data, fuel: Some(u64::from(word(20))) };
    if let Ok(schema) = schema::decode(&mut memory, word(0), word(4), word(8), None) {
        assert!(schema.types.len() <= 256);
        assert!(schema.types.iter().all(|ty| ty.fields.len() <= 256));
        assert!(schema.types.iter().map(|ty| ty.fields.len()).sum::<usize>() <= 4096);
        let size: usize = schema
            .types
            .iter()
            .map(|ty| ty.name.len() + ty.fields.iter().map(|f| f.name.len()).sum::<usize>())
            .sum();
        assert!(size <= abi::MAX_DATA_BYTES);
    }
    assert_eq!(data, bytes[24..], "schema decoding never writes guest memory");
}
