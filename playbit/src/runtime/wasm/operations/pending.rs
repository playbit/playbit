//! Admit fixed outputs before starting work or consuming a completion.
use super::{StoreData, call, files::reference};
use crate::runtime::linear_memory::layout::{files::file_reference, pending::selection};
use crate::runtime::{
    linear_memory::input,
    syscall::{Opcode, OperationHandle, file_ops, handles, pending_data},
};
use wasmtime::Caller;

pub(super) fn create_begin(
    caller: Caller<'_, impl StoreData>,
    entry: u32,
    type_id: u32,
    fields: u32,
    count: u32,
    stride: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::ObjectCreateBegin)?;
        let type_id = memory.input(type_id, 1, 16, 1, 1, None)?;
        let changes =
            crate::runtime::linear_memory::input::changes(memory, fields, count, stride, None)?;
        let handle = pending_data::begin_entry(
            state,
            entry,
            crate::runtime::host::DataOperation::Create {
                entry_id: entry.entry_id,
                type_id: type_id.read(memory.data, 0),
                fields: changes,
            },
            Opcode::ObjectCreateBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}
pub(super) fn copy_begin(caller: Caller<'_, impl StoreData>, source: u32, destination: u32) -> i64 {
    call(caller, |state, memory| {
        let source = memory.record(source, file_reference::LAYOUT, true, None)?;
        let destination = memory.record(destination, file_reference::LAYOUT, true, None)?;
        let handle = file_ops::copy_begin(
            state,
            reference(memory.data, source)?,
            reference(memory.data, destination)?,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}
pub(super) fn select_begin(caller: Caller<'_, impl StoreData>) -> i64 {
    call(caller, |state, _memory| {
        let handle = file_ops::select_begin(state)?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

fn poll(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    output: u32,
    opcode: Opcode,
) -> i64 {
    call(caller, |state, memory| {
        let output = memory.admit(output, 1, 16, 1, 1)?;
        let id = handles::poll_operation(state, OperationHandle { generation, slot }, opcode)?;
        output.write(memory.data, 0, &id);
        Ok(0)
    })
}
pub(super) fn create_poll(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    output: u32,
) -> i64 {
    poll(caller, generation, slot, output, Opcode::ObjectCreatePoll)
}
pub(super) fn copy_poll(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    output: u32,
) -> i64 {
    call(caller, |state, memory| {
        let output = memory.admit(output, 1, 15, 1, 1)?;
        let id = file_ops::copy_poll(state, OperationHandle { generation, slot })?;
        output.write(memory.data, 0, &id);
        Ok(0)
    })
}

pub(super) fn select_poll(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    output: u32,
) -> i64 {
    call(caller, |state, memory| {
        let output = memory.record(output, selection::LAYOUT, false, None)?;
        output.clear(memory.data);
        let selected = file_ops::select_poll(state, OperationHandle { generation, slot })?;
        output.initialize(memory.data, selection::LAYOUT);
        if let Some(file) = selected {
            output.write(memory.data, selection::PRESENT, &1_u32.to_le_bytes());
            let volume =
                playbit_api::abi::dispatch::pack_handle(file.volume.generation, file.volume.slot);
            output.write(memory.data, selection::VOLUME, &volume.to_le_bytes());
            output.write(memory.data, selection::ENTRY_ID, &file.entry_id);
        }
        output.publish(memory.data, selection::LAYOUT);
        Ok(0)
    })
}
