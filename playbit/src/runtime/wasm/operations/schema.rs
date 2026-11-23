use super::{StoreData, call, output::Buffer};
use crate::runtime::linear_memory::layout::schema::{
    field as field_layout, schema as schema_layout, type_record,
};
use crate::runtime::{
    host::{DataOperation, DataResult},
    instance::State,
    linear_memory::{Memory, Region, input, schema},
    syscall::{HostError, HostResult, Opcode, OperationHandle, Schema, pending_data},
};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;

fn output(memory: &mut Memory<'_>, pointer: u32) -> HostResult<(Region, Buffer, Buffer, Buffer)> {
    let descriptor = memory.record(pointer, schema_layout::LAYOUT, false, None)?;
    let types = Buffer::records(
        memory,
        descriptor.sub(schema_layout::TYPES_DATA, 16),
        type_record::LAYOUT,
        256,
    )?;
    let fields = Buffer::records(
        memory,
        descriptor.sub(schema_layout::FIELDS_DATA, 16),
        field_layout::LAYOUT,
        4096,
    )?;
    let bytes = Buffer::admit(
        memory,
        descriptor.sub(schema_layout::BYTES_DATA, 12),
        1,
        1,
        contract::MAX_DATA_BYTES as u32,
    )?;
    crate::runtime::linear_memory::record::outputs(&[
        descriptor,
        types.data,
        fields.data,
        bytes.data,
    ])?;
    descriptor.clear(memory.data);
    types.clear(memory.data);
    fields.clear(memory.data);
    Ok((descriptor, types, fields, bytes))
}
fn write_result(
    memory: &mut Memory<'_>,
    descriptor: Region,
    types: &Buffer,
    fields: &Buffer,
    bytes: &Buffer,
    schema: &Schema,
) -> HostResult<i32> {
    let field_count = schema.types.iter().map(|ty| ty.fields.len()).sum();
    let byte_count = schema
        .types
        .iter()
        .map(|ty| ty.name.len() + ty.fields.iter().map(|f| f.name.len()).sum::<usize>())
        .sum();
    types.count(memory.data, schema.types.len());
    fields.count(memory.data, field_count);
    bytes.count(memory.data, byte_count);
    if !types.fits(schema.types.len()) || !fields.fits(field_count) || !bytes.fits(byte_count) {
        return Err(HostError::BufferTooSmall);
    }
    let used = write(memory.data, types, fields, bytes, schema);
    assert_eq!(used, (field_count, byte_count));
    descriptor.publish(memory.data, schema_layout::LAYOUT);
    Ok(0)
}
pub(super) fn read_begin(caller: Caller<'_, impl StoreData>, entry: u32) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::SchemaReadBegin)?;
        let pending = pending_data::begin_entry(
            state,
            entry,
            DataOperation::SchemaRead { entry_id: entry.entry_id },
            Opcode::SchemaReadBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot) as i64)
    })
}
pub(super) fn establish_begin(
    caller: Caller<'_, impl StoreData>,
    entry: u32,
    pointer: u32,
    count: u32,
    stride: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::SchemaEstablishBegin)?;
        let schema = schema::decode(memory, pointer, count, stride, None)?;
        let pending = pending_data::begin_entry(
            state,
            entry,
            DataOperation::SchemaEstablish { entry_id: entry.entry_id, schema },
            Opcode::SchemaEstablishBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot) as i64)
    })
}
pub(super) fn read_take(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    pointer: u32,
) -> i64 {
    call(caller, |state, memory| {
        let pending = OperationHandle { generation, slot };
        pending_data::admit_take(state, pending, Opcode::SchemaReadTake)?;
        let (descriptor, types, fields, bytes) = output(memory, pointer)?;
        pending_data::take(state, pending, Opcode::SchemaReadTake, |_, result| {
            let DataResult::Schema(schema) = result else { return Err(HostError::Internal) };
            write_result(memory, descriptor, &types, &fields, &bytes, schema)
        })
    })
}
pub(super) fn establish_take(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
) -> i64 {
    call(caller, |state, _| take_establishment(state, OperationHandle { generation, slot }))
}
fn take_establishment(state: &mut State, pending: OperationHandle) -> HostResult<i32> {
    pending_data::take(state, pending, Opcode::SchemaEstablishTake, |_, result| {
        if matches!(result, DataResult::SchemaEstablished) {
            Ok(0)
        } else {
            Err(HostError::Internal)
        }
    })
}
fn write(
    memory: &mut [u8],
    types: &Buffer,
    fields: &Buffer,
    bytes: &Buffer,
    schema: &Schema,
) -> (usize, usize) {
    let mut used_fields = 0;
    let mut used_bytes = 0;
    for (index, ty) in schema.types.iter().enumerate() {
        let record = types.element(memory, index);
        let name = bytes.append(memory, &mut used_bytes, ty.name.as_bytes());
        record.write(memory, type_record::ID, &ty.id);
        record.write(memory, type_record::NAME_OFFSET, &name);
        record.write(memory, type_record::FIELDS_OFFSET, &(used_fields as u32).to_le_bytes());
        record.write(memory, type_record::FIELDS_COUNT, &(ty.fields.len() as u32).to_le_bytes());
        record.write(memory, type_record::FLAGS, &u32::from(ty.deprecated).to_le_bytes());
        record.write(memory, type_record::RESERVED, &0_u32.to_le_bytes());
        types.publish(memory, index);
        for field in &ty.fields {
            let record = fields.element(memory, used_fields);
            let name = bytes.append(memory, &mut used_bytes, field.name.as_bytes());
            let (kind, reference) = schema::kind(field.value_type);
            record.write(memory, field_layout::ID, &field.id);
            record.write(memory, field_layout::NAME_OFFSET, &name);
            record.write(memory, field_layout::KIND, &kind.to_le_bytes());
            let flags = u32::from(field.deprecated) | (u32::from(field.repeated) << 1);
            record.write(memory, field_layout::FLAGS, &flags.to_le_bytes());
            record.write(memory, field_layout::REFERENCE_TYPE, &reference);
            fields.publish(memory, used_fields);
            used_fields += 1;
        }
    }
    (used_fields, used_bytes)
}
