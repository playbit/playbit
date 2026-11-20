use super::output::Buffer;
use crate::runtime::linear_memory::layout::data::{
    field as field_layout, object as object_layout, value as value_layout,
};
use crate::runtime::{
    linear_memory::{Memory, Region},
    syscall::{HostError, HostResult, ObjectRecord, Value},
};
use playbit_api::abi::{data as abi, syscall as contract};
pub(super) struct Output {
    descriptor: Region,
    fields: Buffer,
    values: Buffer,
    bytes: Buffer,
}
impl Output {
    pub(super) fn admit(memory: &mut Memory<'_>, pointer: u32) -> HostResult<Self> {
        let descriptor = memory.record(pointer, object_layout::LAYOUT, false, None)?;
        let fields = Buffer::records(
            memory,
            descriptor.sub(object_layout::FIELDS_DATA, 16),
            field_layout::LAYOUT,
            contract::MAX_FIELDS,
        )?;
        let values = Buffer::records(
            memory,
            descriptor.sub(object_layout::VALUES_DATA, 16),
            value_layout::LAYOUT,
            contract::MAX_VALUES,
        )?;
        let bytes = Buffer::admit(
            memory,
            descriptor.sub(object_layout::BYTES_DATA, 12),
            1,
            1,
            contract::MAX_DATA_BYTES as u32,
        )?;
        crate::runtime::linear_memory::record::outputs(&[
            descriptor,
            fields.data,
            values.data,
            bytes.data,
        ])?;
        descriptor.clear(memory.data);
        fields.clear(memory.data);
        values.clear(memory.data);
        Ok(Self { descriptor, fields, values, bytes })
    }
    pub(super) fn write(&self, memory: &mut [u8], object: &ObjectRecord) -> HostResult<i32> {
        let all_values = object.fields.iter().filter_map(|f| f.values.as_ref()).flatten();
        let value_count = all_values.clone().count();
        let size = all_values
            .map(|v| match v {
                Value::Text(s) => s.len(),
                Value::Bytes(b) => b.len(),
                _ => 0,
            })
            .sum();
        self.fields.count(memory, object.fields.len());
        self.values.count(memory, value_count);
        self.bytes.count(memory, size);
        if !self.fields.fits(object.fields.len())
            || !self.values.fits(value_count)
            || !self.bytes.fits(size)
        {
            return Err(HostError::BufferTooSmall);
        }
        let (used_values, used_bytes) =
            write_fields(memory, &self.fields, &self.values, &self.bytes, object);
        assert_eq!((used_values, used_bytes), (value_count, size));
        self.descriptor.write(memory, object_layout::TYPE_ID, &object.type_id);
        self.descriptor.publish(memory, object_layout::LAYOUT);
        Ok(0)
    }
}

pub(super) fn write_value(
    memory: &mut [u8],
    record: Region,
    bytes: &Buffer,
    used: &mut usize,
    value: &Value,
) {
    record.initialize(memory, value_layout::LAYOUT);
    let kind = match value {
        Value::Text(value) => {
            let range = bytes.append(memory, used, value.as_bytes());
            record.write(memory, value_layout::BYTES_OFFSET, &range);
            abi::TEXT
        }
        Value::Bytes(value) => {
            let range = bytes.append(memory, used, value);
            record.write(memory, value_layout::BYTES_OFFSET, &range);
            abi::BYTES
        }
        Value::Boolean(value) => {
            record.write(memory, value_layout::SCALAR, &u64::from(*value).to_le_bytes());
            abi::BOOLEAN
        }
        Value::Integer(value) => {
            record.write(memory, value_layout::SCALAR, &value.to_le_bytes());
            abi::INTEGER
        }
        Value::Float(value) => {
            record.write(memory, value_layout::SCALAR, &value.to_le_bytes());
            abi::FLOAT
        }
        Value::Reference { volume_id, database_id, object_id } => {
            record.write(memory, value_layout::ID1, volume_id);
            record.write(memory, value_layout::ID2, object_id);
            record.write(memory, value_layout::ENTRY_ID, database_id);
            abi::REFERENCE
        }
        Value::File { volume_id, entry_id } => {
            record.write(memory, value_layout::ID1, volume_id);
            record.write(memory, value_layout::ENTRY_ID, entry_id);
            abi::FILE
        }
    };
    record.write(memory, value_layout::KIND, &kind.to_le_bytes());
    record.publish(memory, value_layout::LAYOUT);
}

fn write_fields(
    memory: &mut [u8],
    fields_output: &Buffer,
    values: &Buffer,
    bytes: &Buffer,
    object: &ObjectRecord,
) -> (usize, usize) {
    let mut used_values = 0;
    let mut used_bytes = 0;
    for (index, field) in object.fields.iter().enumerate() {
        let record = fields_output.element(memory, index);
        let field_values = field.values.as_deref().unwrap_or_default();
        record.write(memory, field_layout::ID, &field.field_id);
        record.write(memory, field_layout::VALUES_OFFSET, &(used_values as u32).to_le_bytes());
        record.write(
            memory,
            field_layout::VALUES_COUNT,
            &(field_values.len() as u32).to_le_bytes(),
        );
        record.write(
            memory,
            field_layout::PRESENT,
            &u32::from(field.values.is_some()).to_le_bytes(),
        );
        record.write(memory, field_layout::RESERVED, &0_u32.to_le_bytes());
        fields_output.publish(memory, index);
        for value in field_values {
            let record = values.element(memory, used_values);
            write_value(memory, record, bytes, &mut used_bytes, value);
            used_values += 1;
        }
    }
    (used_values, used_bytes)
}
