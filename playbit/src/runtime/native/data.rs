//! Native object records: bounded typed translation, with no request envelope.
use super::calls::{bytes, span, with_context};
use crate::runtime::syscall::{self as rt, HostError, HostResult, environment};
use playbit_api::abi::{data as abi, operations::Context, syscall as contract};
use std::ops::Range;

pub(super) fn handle(value: abi::VolumeHandle) -> HostResult<rt::VolumeHandle> {
    Ok(rt::VolumeHandle { generation: value.generation(), slot: value.slot() })
}

pub(super) unsafe fn entry_handle(pointer: *const abi::EntryHandle) -> HostResult<rt::EntryHandle> {
    // SAFETY: the C caller retains a stable, readable handle for this call.
    let value = unsafe { super::record::read(pointer, true)? };
    if !value.valid() {
        return Err(HostError::InvalidArgument);
    }
    Ok(value)
}

/// # Safety
/// The stable UTF-8 input follow the native C contract.
pub(super) unsafe extern "C" fn pb_entry_resolve_begin(
    context: *mut Context,
    volume: abi::VolumeHandle,
    name: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: all regions are admitted before borrowing input or starting work.
    unsafe {
        with_context(context, |state| {
            rt::pending_data::admit_begin(state, handle(volume)?, rt::Opcode::EntryResolveBegin)?;
            span(name, length as usize)?;
            let name = std::str::from_utf8(bytes(
                name,
                length,
                playbit_api::abi::files::MAX_FILE_PATH_BYTES,
            )?)
            .map_err(|_| HostError::InvalidArgument)?
            .to_owned();
            let pending = rt::pending_data::begin(
                state,
                handle(volume)?,
                crate::runtime::host::DataOperation::ResolveEntry { name },
                rt::Opcode::EntryResolveBegin,
            )?;
            Ok(
                (playbit_api::abi::pending::OperationHandle::new(pending.generation, pending.slot))
                    .0 as i64,
            )
        })
    }
}

/// # Safety
/// The fixed writable entry capability obeys the native C contract.
pub(super) unsafe extern "C" fn pb_entry_resolve_take(
    context: *mut Context,
    pending: playbit_api::abi::pending::OperationHandle,
    output: *mut abi::EntryHandle,
) -> i64 {
    // SAFETY: output admission precedes completion consumption.
    unsafe {
        with_context(context, |state| {
            let pending =
                rt::OperationHandle { generation: pending.generation(), slot: pending.slot() };
            rt::pending_data::admit_take(state, pending, rt::Opcode::EntryResolveTake)?;
            super::record::region(output, true)?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::write(output, rt::pending_data::resolve_entry_take(state, pending)?)?;
            Ok(0)
        })
    }
}

pub(super) fn disjoint(regions: &[Range<usize>]) -> HostResult<()> {
    assert!(regions.len() <= 8, "fixed native argument region bound");
    for (index, a) in regions.iter().enumerate() {
        for b in &regions[index + 1..] {
            if !a.is_empty() && !b.is_empty() && a.start < b.end && b.start < a.end {
                return Err(HostError::InvalidArgument);
            }
        }
    }
    Ok(())
}
pub(super) unsafe fn output_span<T: playbit_api::abi::record::Element>(
    buffer: &abi::Buffer<T>,
    limit: usize,
) -> HostResult<Range<usize>> {
    // SAFETY: callers lend initialized output slots for the duration of the call.
    unsafe { super::record::array(buffer.data, buffer.capacity, buffer.stride, limit) }
}
pub(super) unsafe fn input<'a, T>(
    data: *const T,
    count: usize,
    limit: usize,
) -> HostResult<&'a [T]> {
    if count > limit {
        return Err(HostError::LimitExceeded);
    }
    span(data, count)?;
    if count == 0 {
        return Ok(&[]);
    }
    // SAFETY: the caller supplies a stable readable allocation for the call.
    Ok(unsafe { std::slice::from_raw_parts(data, count) })
}
pub(super) unsafe fn append_bytes(output: *mut u8, used: &mut u32, bytes: &[u8]) -> abi::Range {
    let range = abi::Range::new(*used, bytes.len() as u32);
    if !bytes.is_empty() {
        // SAFETY: complete output capacity and disjointness were admitted before writing.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), output.add(*used as usize), bytes.len());
        }
    }
    *used += bytes.len() as u32;
    range
}

unsafe fn write_volumes(
    state: &crate::runtime::instance::State,
    result: &abi::Volumes,
    files: &[playbit_api::abi::files::FileHandle],
    metadata: &[([u8; 16], u32)],
) -> HostResult<()> {
    // SAFETY: caller admitted every descriptor slot and full payload capacity.
    unsafe {
        let mut used_bytes = 0;
        let mut used_tags = 0;
        for (index, (handle, volume)) in environment::available_volumes(state)?.enumerate() {
            let name = append_bytes(result.bytes_data, &mut used_bytes, volume.name.as_bytes());
            let tags = abi::Range::new(used_tags, volume.tags.len() as u32);
            for tag in &volume.tags {
                let range = append_bytes(result.bytes_data, &mut used_bytes, tag.as_bytes());
                super::record::element(
                    result.tags_data,
                    result.tags_stride,
                    used_tags as usize,
                    range,
                )?;
                used_tags += 1;
            }
            super::record::element(
                result.volumes_data,
                result.volumes_stride,
                index,
                abi::Volume {
                    size: std::mem::size_of::<abi::Volume>() as u32,
                    written: 0,
                    handle: playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot),
                    id: volume.id,
                    file: files[index].0,
                    name_offset: name.offset,
                    name_count: name.count,
                    tags_offset: tags.offset,
                    tags_count: tags.count,
                    rights: volume.rights.0,
                    flags: u32::from(volume.is_root) | metadata[index].1,
                    grant_id: metadata[index].0,
                    role: volume.role,
                },
            )?;
        }
        assert_eq!(used_bytes, result.bytes_count);
        assert_eq!(used_tags, result.tags_count);
        Ok(())
    }
}

/// # Safety
/// All native records/buffers obey playbit/sys/abi.h; no output aliases input.
pub(super) unsafe extern "C" fn pb_volumes(
    context: *mut Context,
    output: *mut abi::Volumes,
) -> i64 {
    // SAFETY: pointer structure is checked before borrowing; allocation validity is the C contract.
    unsafe {
        with_context(context, |state| {
            let descriptor = super::record::region(output, true)?;
            let mut result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                output_span(&result.volumes(), contract::MAX_VOLUMES)?,
                output_span(&result.tags(), contract::MAX_VOLUMES * 64)?,
                output_span(&result.bytes(), contract::MAX_DATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::clear_array(&result.volumes());
            super::record::clear_array(&result.tags());
            let mut tags = 0;
            let mut bytes = 0;
            let count = state.context.volumes.len();
            for (_, volume) in environment::available_volumes(state)? {
                tags += volume.tags.len();
                bytes += volume.name.len() + volume.tags.iter().map(String::len).sum::<usize>();
            }
            result.revision = state.volume_revision;
            result.volumes_count = count as u32;
            result.tags_count = tags as u32;
            result.bytes_count = bytes as u32;
            if result.volumes_capacity < result.volumes_count
                || result.tags_capacity < result.tags_count
                || result.bytes_capacity < result.bytes_count
            {
                super::record::counts(output, result)?;
                return Err(HostError::BufferTooSmall);
            }
            let grants = state.context.volumes.iter().map(|volume| volume.id).collect::<Vec<_>>();
            let metadata = grants
                .iter()
                .map(|id| state.files.root_metadata(*id))
                .collect::<HostResult<Vec<_>>>()?;
            let files = state.files.enumerate(&mut state.capabilities, &grants)?;
            write_volumes(state, &result, &files, &metadata)?;
            super::record::write(output, result)?;
            Ok(0)
        })
    }
}

pub(super) fn reserve(remaining: &mut usize, amount: usize) -> HostResult<()> {
    if amount > *remaining {
        return Err(HostError::LimitExceeded);
    }
    *remaining -= amount;
    Ok(())
}

pub(super) unsafe fn decode_changes(
    pointer: *const abi::FieldInput,
    count: u32,
    stride: u32,
) -> HostResult<Vec<rt::FieldChange>> {
    // SAFETY: nested allocations are stable for the call under the native input contract.
    unsafe {
        let fields = super::record::inputs(pointer, count, stride, contract::MAX_FIELDS as usize)?;
        let mut remaining_values = contract::MAX_VALUES as usize;
        for field in &fields {
            if field.present > 1 || (field.present == 0 && field.count != 0) {
                return Err(HostError::InvalidArgument);
            }
            reserve(&mut remaining_values, field.count as usize)?;
            super::record::inputs(
                field.values,
                field.count,
                field.stride,
                contract::MAX_VALUES as usize,
            )?;
        }
        let mut output = Vec::with_capacity(fields.len());
        let mut remaining = contract::MAX_DATA_BYTES;
        for field in &fields {
            let values = if field.present == 0 {
                None
            } else {
                let inputs = super::record::inputs(
                    field.values,
                    field.count,
                    field.stride,
                    contract::MAX_VALUES as usize,
                )?;
                let mut values = Vec::with_capacity(inputs.len());
                for value in &inputs {
                    values.push(decode_value(value, &mut remaining)?);
                }
                Some(values)
            };
            output.push(rt::FieldChange { field_id: field.id, values });
        }
        Ok(output)
    }
}
unsafe fn decode_value(value: &abi::ValueInput, remaining: &mut usize) -> HostResult<rt::Value> {
    let variable = matches!(value.kind, abi::TEXT | abi::BYTES);
    let ids = matches!(value.kind, abi::REFERENCE | abi::FILE);
    if (!variable && (value.length != 0 || !value.data.is_null()))
        || (variable && value.scalar != 0)
        || value.reserved != 0
        || (!ids && (value.id1 != [0; 16] || value.id2 != [0; 16] || value.entry_id != [0; 15]))
        || (ids && (value.id1 == [0; 16] || value.entry_id == [0; 15]))
    {
        return Err(HostError::InvalidArgument);
    }
    let size = match value.kind {
        abi::TEXT | abi::BYTES => value.length as usize,
        abi::BOOLEAN => 1,
        abi::INTEGER | abi::FLOAT => 8,
        abi::REFERENCE => 47,
        abi::FILE => 31,
        _ => return Err(HostError::InvalidArgument),
    };
    reserve(remaining, size)?;
    Ok(match value.kind {
        abi::TEXT | abi::BYTES => {
            // SAFETY: the input allocation remains readable until this call returns.
            let data = unsafe { bytes(value.data, value.length, contract::MAX_DATA_BYTES)? };
            if value.kind == abi::TEXT {
                rt::Value::Text(
                    std::str::from_utf8(data).map_err(|_| HostError::InvalidArgument)?.into(),
                )
            } else {
                rt::Value::Bytes(data.to_vec())
            }
        }
        abi::BOOLEAN if value.scalar <= 1 => rt::Value::Boolean(value.scalar != 0),
        abi::INTEGER => rt::Value::Integer(value.scalar as i64),
        abi::FLOAT if f64::from_bits(value.scalar).is_finite() => {
            rt::Value::Float(f64::from_bits(value.scalar))
        }
        abi::REFERENCE if value.scalar == 0 && value.id2 != [0; 16] => rt::Value::Reference {
            volume_id: value.id1,
            database_id: value.entry_id,
            object_id: value.id2,
        },
        abi::FILE if value.scalar == 0 && value.id2 == [0; 16] => {
            rt::Value::File { volume_id: value.id1, entry_id: value.entry_id }
        }
        _ => return Err(HostError::InvalidArgument),
    })
}

/// # Safety
/// All output spans are already admitted and mutually disjoint.
pub(super) unsafe fn write_object(
    object: &rt::ObjectRecord,
    output: *mut abi::Object,
    mut result: abi::Object,
) -> HostResult<i32> {
    // SAFETY: the adapter admits complete capacities before this translation.
    unsafe {
        let values = object.fields.iter().filter_map(|f| f.values.as_ref()).flatten();
        let value_count = values.clone().count();
        let byte_count: usize = values
            .map(|v| match v {
                rt::Value::Text(v) => v.len(),
                rt::Value::Bytes(v) => v.len(),
                _ => 0,
            })
            .sum();
        result.fields_count = object.fields.len() as u32;
        result.values_count = value_count as u32;
        result.bytes_count = byte_count as u32;
        if result.fields_capacity < result.fields_count
            || result.values_capacity < result.values_count
            || result.bytes_capacity < result.bytes_count
        {
            super::record::counts(output, result)?;
            return Err(HostError::BufferTooSmall);
        }
        let mut used_values = 0;
        let mut used_bytes = 0;
        for (index, field) in object.fields.iter().enumerate() {
            let values = field.values.as_deref().unwrap_or_default();
            super::record::element(
                result.fields_data,
                result.fields_stride,
                index,
                abi::Field {
                    size: std::mem::size_of::<abi::Field>() as u32,
                    written: 0,
                    id: field.field_id,
                    present: u32::from(field.values.is_some()),
                    reserved: 0,
                    values_offset: used_values,
                    values_count: values.len() as u32,
                },
            )?;
            for value in values {
                let (mut record, bytes) = output_value(value);
                if let Some(bytes) = bytes {
                    record.set_bytes(append_bytes(result.bytes_data, &mut used_bytes, bytes));
                }
                super::record::element(
                    result.values_data,
                    result.values_stride,
                    used_values as usize,
                    record,
                )?;
                used_values += 1;
            }
        }
        assert_eq!(used_values, result.values_count);
        assert_eq!(used_bytes, result.bytes_count);
        result.type_id = object.type_id;
        super::record::write(output, result)?;
        Ok(0)
    }
}

pub(super) fn output_value(value: &rt::Value) -> (abi::Value, Option<&[u8]>) {
    let mut record = abi::Value::default();
    let mut bytes = None;
    match value {
        rt::Value::Text(value) => {
            record.kind = abi::TEXT;
            bytes = Some(value.as_bytes());
        }
        rt::Value::Bytes(value) => {
            record.kind = abi::BYTES;
            bytes = Some(value.as_slice());
        }
        rt::Value::Boolean(value) => {
            record.kind = abi::BOOLEAN;
            record.scalar = u64::from(*value);
        }
        rt::Value::Integer(value) => {
            record.kind = abi::INTEGER;
            record.scalar = *value as u64;
        }
        rt::Value::Float(value) => {
            record.kind = abi::FLOAT;
            record.scalar = value.to_bits();
        }
        rt::Value::Reference { volume_id, database_id, object_id } => {
            record.kind = abi::REFERENCE;
            record.id1 = *volume_id;
            record.id2 = *object_id;
            record.entry_id = *database_id;
        }
        rt::Value::File { volume_id, entry_id } => {
            record.kind = abi::FILE;
            record.id1 = *volume_id;
            record.entry_id = *entry_id;
        }
    }
    (record, bytes)
}

#[cfg(test)]
#[path = "data_tests.rs"]
mod tests;
