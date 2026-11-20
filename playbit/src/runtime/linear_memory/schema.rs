//! Explicit wasm32 schema records; nested input allocations share finite budgets.
use super::layout::schema::{field_input, type_input};
use super::{Memory, Region, input::reserve};
use crate::runtime::host::{FieldDeclaration, HostResult, Schema, TypeDeclaration, ValueType};
use playbit_api::abi::syscall::Error as HostError;
use playbit_api::abi::{data, syscall as contract};

pub(in crate::runtime) fn value_type(kind: u32, reference: [u8; 16]) -> HostResult<ValueType> {
    if kind != data::REFERENCE && reference != [0; 16] {
        return Err(HostError::InvalidArgument);
    }
    Ok(match kind {
        data::TEXT => ValueType::Text,
        data::BOOLEAN => ValueType::Boolean,
        data::INTEGER => ValueType::Integer,
        data::FLOAT => ValueType::Float,
        data::BYTES => ValueType::Bytes,
        data::REFERENCE => ValueType::Reference(reference),
        data::FILE => ValueType::File,
        _ => return Err(HostError::InvalidArgument),
    })
}
#[cfg(any(test, feature = "wasm-runtime"))]
pub(in crate::runtime) fn kind(value: ValueType) -> (u32, [u8; 16]) {
    (
        match value {
            ValueType::Text => data::TEXT,
            ValueType::Boolean => data::BOOLEAN,
            ValueType::Integer => data::INTEGER,
            ValueType::Float => data::FLOAT,
            ValueType::Bytes => data::BYTES,
            ValueType::Reference(id) => return (data::REFERENCE, id),
            ValueType::File => data::FILE,
        },
        [0; 16],
    )
}
fn name(
    memory: &mut Memory<'_>,
    pointer: u32,
    length: u32,
    remaining: &mut usize,
    excluded: Option<Region>,
) -> HostResult<String> {
    if length > 255 {
        return Err(HostError::InvalidArgument);
    }
    reserve(remaining, length as usize)?;
    let bytes = memory.input(pointer, length, 1, 1, 255, excluded)?;
    Ok(std::str::from_utf8(bytes.bytes(memory.data))
        .map_err(|_| HostError::InvalidArgument)?
        .to_owned())
}
fn admit_fields(
    memory: &mut Memory<'_>,
    types: Region,
    count: u32,
    stride: u32,
    excluded: Option<Region>,
) -> HostResult<()> {
    let mut fields_left = 4096;
    // Admit aggregate field count and every field array before allocating declarations.
    for index in 0..count as usize {
        let ty = types.slot(memory.data, index, stride);
        let count = ty.u32(memory.data, type_input::COUNT);
        if ty.u32(memory.data, type_input::FLAGS) & !1 != 0 {
            return Err(HostError::InvalidArgument);
        }
        reserve(&mut fields_left, count as usize)?;
        memory.record_array(
            ty.u32(memory.data, type_input::FIELDS),
            count,
            ty.u32(memory.data, type_input::STRIDE),
            field_input::LAYOUT,
            true,
            contract::MAX_FIELDS,
            excluded,
        )?;
    }
    Ok(())
}

pub(crate) fn decode(
    memory: &mut Memory<'_>,
    pointer: u32,
    count: u32,
    stride: u32,
    excluded: Option<Region>,
) -> HostResult<Schema> {
    let types =
        memory.record_array(pointer, count, stride, type_input::LAYOUT, true, 256, excluded)?;
    admit_fields(memory, types, count, stride, excluded)?;
    let mut bytes_left = contract::MAX_DATA_BYTES;
    let mut result = Schema { types: Vec::with_capacity(count as usize) };
    for index in 0..count as usize {
        let ty = types.slot(memory.data, index, stride);
        let count = ty.u32(memory.data, type_input::COUNT);
        let field_stride = ty.u32(memory.data, type_input::STRIDE);
        let fields = memory.record_array(
            ty.u32(memory.data, type_input::FIELDS),
            count,
            field_stride,
            field_input::LAYOUT,
            true,
            contract::MAX_FIELDS,
            excluded,
        )?;
        let mut declaration = TypeDeclaration {
            id: ty.read(memory.data, type_input::ID),
            name: name(
                memory,
                ty.u32(memory.data, type_input::NAME_DATA),
                ty.u32(memory.data, type_input::NAME_LENGTH),
                &mut bytes_left,
                excluded,
            )?,
            deprecated: ty.u32(memory.data, type_input::FLAGS) == 1,
            fields: Vec::with_capacity(count as usize),
        };
        for index in 0..count as usize {
            let field = fields.slot(memory.data, index, field_stride);
            let flags = field.u32(memory.data, field_input::FLAGS);
            if flags & !3 != 0 {
                return Err(HostError::InvalidArgument);
            }
            declaration.fields.push(FieldDeclaration {
                id: field.read(memory.data, field_input::ID),
                name: name(
                    memory,
                    field.u32(memory.data, field_input::NAME_DATA),
                    field.u32(memory.data, field_input::NAME_LENGTH),
                    &mut bytes_left,
                    excluded,
                )?,
                value_type: value_type(
                    field.u32(memory.data, field_input::KIND),
                    field.read(memory.data, field_input::REFERENCE_TYPE),
                )?,
                deprecated: flags & 1 != 0,
                repeated: flags & 2 != 0,
            });
        }
        result.types.push(declaration);
    }
    Ok(result)
}
