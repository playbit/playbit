//! Decode wasm32 C records field by field, never as native references or enums.
use super::layout::data::{entry_handle as entry_layout, field_input, value_input};
use super::{Memory, Region};
use crate::runtime::host::{FieldChange, HostResult, Value};
use playbit_api::abi::syscall::Error as HostError;
use playbit_api::abi::{data as abi, syscall as contract};

pub(crate) fn entry_handle(
    memory: &mut Memory<'_>,
    pointer: u32,
    output: Option<Region>,
) -> HostResult<(abi::EntryHandle, Region)> {
    let region = memory.record(pointer, entry_layout::LAYOUT, true, output)?;
    let handle = abi::EntryHandle {
        volume: region.u64(memory.data, entry_layout::VOLUME),
        entry_id: region.read(memory.data, entry_layout::ENTRY_ID),
        reserved: region.read::<1>(memory.data, entry_layout::RESERVED)[0],
        ..abi::EntryHandle::default()
    };
    if !handle.valid() {
        return Err(HostError::InvalidArgument);
    }
    Ok((handle, region))
}

pub(super) fn reserve(remaining: &mut usize, amount: usize) -> HostResult<()> {
    if amount > *remaining {
        return Err(HostError::LimitExceeded);
    }
    *remaining -= amount;
    Ok(())
}

pub(crate) fn bloblog_blobs(
    memory: &mut Memory<'_>,
    pointer: u32,
    count: u32,
    stride: u32,
    output: Option<Region>,
) -> HostResult<Vec<Vec<u8>>> {
    use super::layout::bloblog::blob;
    use playbit_api::abi::bloblog as log;
    let records = memory.record_array(
        pointer,
        count,
        stride,
        blob::LAYOUT,
        true,
        log::MAX_BLOBS as u32,
        output,
    )?;
    log::validate_lengths((0..count as usize).map(|index| {
        records.slot(memory.data, index, stride).u32(memory.data, blob::LENGTH) as usize
    }))?;
    for index in 0..count as usize {
        let record = records.slot(memory.data, index, stride);
        memory.input(
            record.u32(memory.data, blob::DATA),
            record.u32(memory.data, blob::LENGTH),
            1,
            1,
            log::MAX_BLOB_BYTES as u32,
            output,
        )?;
    }
    let mut blobs = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let record = records.slot(memory.data, index, stride);
        let bytes = memory.input(
            record.u32(memory.data, blob::DATA),
            record.u32(memory.data, blob::LENGTH),
            1,
            1,
            log::MAX_BLOB_BYTES as u32,
            output,
        )?;
        blobs.push(bytes.bytes(memory.data).to_vec());
    }
    Ok(blobs)
}

pub(crate) fn changes(
    memory: &mut Memory<'_>,
    pointer: u32,
    count: u32,
    stride: u32,
    output: Option<Region>,
) -> HostResult<Vec<FieldChange>> {
    let fields = memory.record_array(
        pointer,
        count,
        stride,
        field_input::LAYOUT,
        true,
        contract::MAX_FIELDS,
        output,
    )?;
    let mut remaining_values = contract::MAX_VALUES as usize;
    // Admit aggregate cardinality before allocating any vector of values.
    for index in 0..count as usize {
        let field = fields.slot(memory.data, index, stride);
        let count = field.u32(memory.data, field_input::COUNT);
        let present = field.u32(memory.data, field_input::PRESENT);
        if present > 1 || (present == 0 && count != 0) {
            return Err(HostError::InvalidArgument);
        }
        reserve(&mut remaining_values, count as usize)?;
        memory.record_array(
            field.u32(memory.data, field_input::VALUES),
            count,
            field.u32(memory.data, field_input::STRIDE),
            value_input::LAYOUT,
            true,
            contract::MAX_VALUES,
            output,
        )?;
    }
    let mut result = Vec::with_capacity(count as usize);
    let mut remaining_bytes = contract::MAX_DATA_BYTES;
    for index in 0..count as usize {
        let field = fields.slot(memory.data, index, stride);
        let count = field.u32(memory.data, field_input::COUNT);
        let value_stride = field.u32(memory.data, field_input::STRIDE);
        let values = if field.u32(memory.data, field_input::PRESENT) == 0 {
            None
        } else {
            let records = memory.record_array(
                field.u32(memory.data, field_input::VALUES),
                count,
                value_stride,
                value_input::LAYOUT,
                true,
                contract::MAX_VALUES,
                output,
            )?;
            let mut values = Vec::with_capacity(count as usize);
            for index in 0..count as usize {
                values.push(value(
                    memory,
                    records.slot(memory.data, index, value_stride),
                    &mut remaining_bytes,
                    output,
                )?);
            }
            Some(values)
        };
        result.push(FieldChange { field_id: field.read(memory.data, field_input::ID), values });
    }
    Ok(result)
}

fn value(
    memory: &mut Memory<'_>,
    record: Region,
    remaining: &mut usize,
    output: Option<Region>,
) -> HostResult<Value> {
    let kind = record.u32(memory.data, value_input::KIND);
    let length = record.u32(memory.data, value_input::LENGTH);
    let scalar = record.u64(memory.data, value_input::SCALAR);
    let id1 = record.read::<16>(memory.data, value_input::ID1);
    let id2 = record.read::<16>(memory.data, value_input::ID2);
    let entry_id = record.read::<15>(memory.data, value_input::ENTRY_ID);
    let pointer = record.u32(memory.data, value_input::DATA);
    let variable = matches!(kind, abi::TEXT | abi::BYTES);
    let ids = matches!(kind, abi::REFERENCE | abi::FILE);
    if (!variable && (length != 0 || pointer != 0))
        || (variable && scalar != 0)
        || record.read::<1>(memory.data, value_input::RESERVED) != [0]
        || (!ids && (id1 != [0; 16] || id2 != [0; 16] || entry_id != [0; 15]))
        || (ids && (id1 == [0; 16] || entry_id == [0; 15]))
    {
        return Err(HostError::InvalidArgument);
    }
    let size = match kind {
        abi::TEXT | abi::BYTES => length as usize,
        abi::BOOLEAN => 1,
        abi::INTEGER | abi::FLOAT => 8,
        abi::REFERENCE => 47,
        abi::FILE => 31,
        _ => return Err(HostError::InvalidArgument),
    };
    reserve(remaining, size)?;
    Ok(match kind {
        abi::TEXT | abi::BYTES => {
            let bytes = memory
                .input(pointer, length, 1, 1, contract::MAX_DATA_BYTES as u32, output)?
                .bytes(memory.data);
            if kind == abi::TEXT {
                Value::Text(
                    std::str::from_utf8(bytes).map_err(|_| HostError::InvalidArgument)?.into(),
                )
            } else {
                Value::Bytes(bytes.to_vec())
            }
        }
        abi::BOOLEAN if scalar <= 1 => Value::Boolean(scalar != 0),
        abi::INTEGER => Value::Integer(scalar as i64),
        abi::FLOAT if f64::from_bits(scalar).is_finite() => Value::Float(f64::from_bits(scalar)),
        abi::REFERENCE if scalar == 0 && id2 != [0; 16] => {
            Value::Reference { volume_id: id1, database_id: entry_id, object_id: id2 }
        }
        abi::FILE if scalar == 0 && id2 == [0; 16] => Value::File { volume_id: id1, entry_id },
        _ => return Err(HostError::InvalidArgument),
    })
}
