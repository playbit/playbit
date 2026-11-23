use super::{StoreData, call, output::Buffer, read::write_value};
use crate::runtime::host::{DataOperation, DataResult};
use crate::runtime::linear_memory::layout::{
    authenticated::{field, metadata as metadata_layout},
    data::value as value_layout,
};
use crate::runtime::{
    linear_memory::{authenticated::metadata, input},
    syscall::{HostError, Opcode, OperationHandle, Value, pending_data},
};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;
pub(super) fn begin(
    caller: Caller<'_, impl StoreData>,
    entry: u32,
    object: u32,
    field: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::ObjectFieldReadAuthenticatedBegin)?;
        let object = memory.input(object, 1, 16, 1, 1, None)?;
        let field = memory.input(field, 1, 16, 1, 1, None)?;
        let pending = pending_data::begin_entry(
            state,
            entry,
            DataOperation::AuthenticatedField {
                entry_id: entry.entry_id,
                object_id: object.read(memory.data, 0),
                field_id: field.read(memory.data, 0),
            },
            Opcode::ObjectFieldReadAuthenticatedBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot) as i64)
    })
}

fn admit(
    memory: &mut crate::runtime::linear_memory::Memory<'_>,
    output: u32,
) -> crate::runtime::host::HostResult<(
    crate::runtime::linear_memory::Region,
    crate::runtime::linear_memory::Region,
    Buffer,
    Buffer,
)> {
    let descriptor = memory.record(output, field::LAYOUT, false, None)?;
    let metadata_record = memory.record(
        descriptor.u32(memory.data, field::METADATA),
        metadata_layout::LAYOUT,
        false,
        None,
    )?;
    let values = Buffer::records(
        memory,
        descriptor.sub(field::VALUES_DATA, 16),
        value_layout::LAYOUT,
        contract::MAX_VALUES,
    )?;
    let bytes = Buffer::admit(
        memory,
        descriptor.sub(field::BYTES_DATA, 12),
        1,
        1,
        contract::MAX_DATA_BYTES as u32,
    )?;
    crate::runtime::linear_memory::record::outputs(&[
        descriptor,
        metadata_record,
        values.data,
        bytes.data,
    ])?;
    descriptor.clear(memory.data);
    metadata_record.clear(memory.data);
    values.clear(memory.data);
    Ok((descriptor, metadata_record, values, bytes))
}

pub(super) fn take(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    output: u32,
) -> i64 {
    call(caller, |state, memory| {
        let operation = OperationHandle { generation, slot };
        pending_data::admit_take(state, operation, Opcode::ObjectFieldReadAuthenticatedTake)?;
        let (descriptor, metadata_record, values, bytes) = admit(memory, output)?;
        pending_data::take(
            state,
            operation,
            Opcode::ObjectFieldReadAuthenticatedTake,
            |_, record| {
                let DataResult::AuthenticatedField(record) = record else {
                    return Err(HostError::Internal);
                };
                let items = record.as_ref().and_then(|r| r.values.as_deref()).unwrap_or_default();
                let size = items
                    .iter()
                    .map(|v| match v {
                        Value::Text(s) => s.len(),
                        Value::Bytes(b) => b.len(),
                        _ => 0,
                    })
                    .sum();
                values.count(memory.data, items.len());
                bytes.count(memory.data, size);
                if !values.fits(items.len()) || !bytes.fits(size) {
                    return Err(HostError::BufferTooSmall);
                }
                let mut used = 0;
                for (index, value) in items.iter().enumerate() {
                    let output = values.element(memory.data, index);
                    write_value(memory.data, output, &bytes, &mut used, value);
                }
                assert_eq!(used, size);
                metadata_record.write(
                    memory.data,
                    metadata_layout::VOLUME_ID,
                    &metadata(record.as_ref()),
                );
                metadata_record.publish(memory.data, metadata_layout::LAYOUT);
                descriptor.publish(memory.data, field::LAYOUT);
                Ok(0)
            },
        )
    })
}
