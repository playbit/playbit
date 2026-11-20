//! Typed native schema boundary; schema authority remains in shared operations.
use super::{
    calls::{bytes, with_context},
    data::{append_bytes, entry_handle, output_span, reserve},
};
use crate::runtime::{
    host::{DataOperation, DataResult},
    syscall::{self as rt, HostError, HostResult, pending_data},
};
use playbit_api::abi::{data, operations::Context, schema as abi, syscall as contract};
const MAX_TYPES: usize = 256;
const MAX_FIELDS: usize = 4096;

fn value_type(kind: u32, reference: [u8; 16]) -> HostResult<rt::ValueType> {
    if kind != data::REFERENCE && reference != [0; 16] {
        return Err(HostError::InvalidArgument);
    }
    Ok(match kind {
        data::TEXT => rt::ValueType::Text,
        data::BOOLEAN => rt::ValueType::Boolean,
        data::INTEGER => rt::ValueType::Integer,
        data::FLOAT => rt::ValueType::Float,
        data::BYTES => rt::ValueType::Bytes,
        data::REFERENCE => rt::ValueType::Reference(reference),
        data::FILE => rt::ValueType::File,
        _ => return Err(HostError::InvalidArgument),
    })
}
fn kind(value: rt::ValueType) -> (u32, [u8; 16]) {
    (
        match value {
            rt::ValueType::Text => data::TEXT,
            rt::ValueType::Boolean => data::BOOLEAN,
            rt::ValueType::Integer => data::INTEGER,
            rt::ValueType::Float => data::FLOAT,
            rt::ValueType::Bytes => data::BYTES,
            rt::ValueType::Reference(id) => return (data::REFERENCE, id),
            rt::ValueType::File => data::FILE,
        },
        [0; 16],
    )
}
unsafe fn name(pointer: *const u8, length: u32, remaining: &mut usize) -> HostResult<String> {
    if length > 255 {
        return Err(HostError::InvalidArgument);
    }
    reserve(remaining, length as usize)?;
    // SAFETY: names are stable readable allocations under the native contract.
    let bytes = unsafe { bytes(pointer, length, 255)? };
    Ok(std::str::from_utf8(bytes).map_err(|_| HostError::InvalidArgument)?.to_owned())
}
unsafe fn decode(types: *const abi::TypeInput, count: u32, stride: u32) -> HostResult<rt::Schema> {
    // SAFETY: structural bounds are admitted before borrowing each nested span.
    unsafe {
        let types = super::record::inputs(types, count, stride, MAX_TYPES)?;
        let mut fields_left = MAX_FIELDS;
        let mut bytes_left = contract::MAX_DATA_BYTES;
        let mut result = rt::Schema { types: Vec::with_capacity(types.len()) };
        for ty in &types {
            if ty.flags & !1 != 0 {
                return Err(HostError::InvalidArgument);
            }
            reserve(&mut fields_left, ty.count as usize)?;
            let fields = super::record::inputs(
                ty.fields,
                ty.count,
                ty.stride,
                contract::MAX_FIELDS as usize,
            )?;
            let mut declaration = rt::TypeDeclaration {
                id: ty.id,
                name: name(ty.name_data, ty.name_length, &mut bytes_left)?,
                deprecated: ty.flags == 1,
                fields: Vec::with_capacity(fields.len()),
            };
            for field in &fields {
                if field.flags & !3 != 0 {
                    return Err(HostError::InvalidArgument);
                }
                declaration.fields.push(rt::FieldDeclaration {
                    id: field.id,
                    name: name(field.name_data, field.name_length, &mut bytes_left)?,
                    value_type: value_type(field.kind, field.reference_type)?,
                    deprecated: field.flags & 1 != 0,
                    repeated: field.flags & 2 != 0,
                });
            }
            result.types.push(declaration);
        }
        Ok(result)
    }
}
// SAFETY: callers admit every output span before invoking this writer.
unsafe fn write_result(
    schema: &rt::Schema,
    output: *mut abi::Schema,
    mut result: abi::Schema,
) -> HostResult<i32> {
    unsafe {
        result.types_count = schema.types.len() as u32;
        result.fields_count = schema.types.iter().map(|ty| ty.fields.len() as u32).sum();
        result.bytes_count = schema
            .types
            .iter()
            .map(|ty| {
                ty.name.len() as u32 + ty.fields.iter().map(|f| f.name.len() as u32).sum::<u32>()
            })
            .sum();
        if result.types_count > result.types_capacity
            || result.fields_count > result.fields_capacity
            || result.bytes_count > result.bytes_capacity
        {
            super::record::counts(output, result)?;
            return Err(HostError::BufferTooSmall);
        }
        let mut fields = 0;
        let mut bytes = 0;
        for (index, ty) in schema.types.iter().enumerate() {
            let name = append_bytes(result.bytes_data, &mut bytes, ty.name.as_bytes());
            super::record::element(
                result.types_data,
                result.types_stride,
                index,
                abi::Type {
                    size: std::mem::size_of::<abi::Type>() as u32,
                    written: 0,
                    id: ty.id,
                    name_offset: name.offset,
                    name_count: name.count,
                    fields_offset: fields,
                    fields_count: ty.fields.len() as u32,
                    flags: u32::from(ty.deprecated),
                    reserved: 0,
                },
            )?;
            for field in &ty.fields {
                let name = append_bytes(result.bytes_data, &mut bytes, field.name.as_bytes());
                let (kind, reference_type) = kind(field.value_type);
                super::record::element(
                    result.fields_data,
                    result.fields_stride,
                    fields as usize,
                    abi::Field {
                        size: std::mem::size_of::<abi::Field>() as u32,
                        written: 0,
                        id: field.id,
                        name_offset: name.offset,
                        name_count: name.count,
                        kind,
                        reference_type,
                        flags: u32::from(field.deprecated) | (u32::from(field.repeated) << 1),
                    },
                )?;
                fields += 1;
            }
        }
        assert_eq!(fields, result.fields_count);
        assert_eq!(bytes, result.bytes_count);
        super::record::write(output, result)?;
        Ok(0)
    }
}

/// # Safety
/// Context and stable entry input obey the C contract.
pub(super) unsafe fn read_begin(context: *mut Context, entry: *const data::EntryHandle) -> i64 {
    // SAFETY: the entry record is admitted before host work.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::SchemaReadBegin)?;
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::SchemaRead { entry_id: entry.entry_id },
                rt::Opcode::SchemaReadBegin,
            )?;
            Ok(
                (playbit_api::abi::pending::OperationHandle::new(pending.generation, pending.slot))
                    .0 as i64,
            )
        })
    }
}
/// # Safety
/// Stable nested schema inputs obey the C contract.
pub(super) unsafe fn establish_begin(
    context: *mut Context,
    entry: *const data::EntryHandle,
    types: *const abi::TypeInput,
    count: u32,
    stride: u32,
) -> i64 {
    // SAFETY: every nested input is admitted and copied before host admission.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::SchemaEstablishBegin)?;
            let schema = decode(types, count, stride)?;
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::SchemaEstablish { entry_id: entry.entry_id, schema },
                rt::Opcode::SchemaEstablishBegin,
            )?;
            Ok(
                (playbit_api::abi::pending::OperationHandle::new(pending.generation, pending.slot))
                    .0 as i64,
            )
        })
    }
}
/// # Safety
/// Caller-owned result descriptor and arrays are valid and mutually disjoint.
pub(super) unsafe fn read_take(
    context: *mut Context,
    pending: playbit_api::abi::pending::OperationHandle,
    output: *mut abi::Schema,
) -> i64 {
    // SAFETY: output admission precedes retained-result inspection and consumption.
    unsafe {
        with_context(context, |state| {
            let pending =
                rt::OperationHandle { generation: pending.generation(), slot: pending.slot() };
            pending_data::admit_take(state, pending, rt::Opcode::SchemaReadTake)?;
            let descriptor = super::record::region(output, true)?;
            let result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                output_span(&result.types(), MAX_TYPES)?,
                output_span(&result.fields(), MAX_FIELDS)?,
                output_span(&result.bytes(), contract::MAX_DATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::clear_array(&result.types());
            super::record::clear_array(&result.fields());
            pending_data::take(state, pending, rt::Opcode::SchemaReadTake, |_, data| {
                let DataResult::Schema(schema) = data else { return Err(HostError::Internal) };
                write_result(schema, output, result)
            })
        })
    }
}
/// # Safety
/// Context obeys the C contract; there is no pointer output.
pub(super) unsafe fn establish_take(
    context: *mut Context,
    pending: playbit_api::abi::pending::OperationHandle,
) -> i64 {
    // SAFETY: only the valid borrowed context is accessed.
    unsafe {
        with_context(context, |state| {
            pending_data::take(
                state,
                rt::OperationHandle { generation: pending.generation(), slot: pending.slot() },
                rt::Opcode::SchemaEstablishTake,
                |_, data| {
                    if matches!(data, DataResult::SchemaEstablished) {
                        Ok(0)
                    } else {
                        Err(HostError::Internal)
                    }
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_names_flags_and_nested_counts_are_rejected() {
        let field = abi::FieldInput {
            size: size_of::<abi::FieldInput>() as u32,
            written: 0,
            id: [2; 16],
            name_data: b"done".as_ptr(),
            name_length: 4,
            kind: data::BOOLEAN,
            flags: 4,
            reference_type: [0; 16],
        };
        let mut ty = abi::TypeInput {
            size: size_of::<abi::TypeInput>() as u32,
            written: 0,
            id: [1; 16],
            name_data: b"Item".as_ptr(),
            name_length: 4,
            fields: &field,
            count: 1,
            stride: size_of::<abi::FieldInput>() as u32,
            flags: 0,
        };
        // SAFETY: all allocations are live; malformed tags/counts fail before any host call.
        assert_eq!(
            unsafe { decode(&ty, 1, size_of::<abi::TypeInput>() as u32) },
            Err(HostError::InvalidArgument)
        );
        ty.count = contract::MAX_FIELDS + 1;
        assert_eq!(
            unsafe { decode(&ty, 1, size_of::<abi::TypeInput>() as u32) },
            Err(HostError::LimitExceeded)
        );
        ty.count = 0;
        ty.flags = 2;
        assert_eq!(
            unsafe { decode(&ty, 1, size_of::<abi::TypeInput>() as u32) },
            Err(HostError::InvalidArgument)
        );
        let long = [b'a'; 256];
        let mut budget = contract::MAX_DATA_BYTES;
        assert_eq!(
            unsafe { name(long.as_ptr(), long.len() as u32, &mut budget) },
            Err(HostError::InvalidArgument)
        );
        assert_eq!(
            unsafe { name([0xff].as_ptr(), 1, &mut budget) },
            Err(HostError::InvalidArgument)
        );
    }
}
