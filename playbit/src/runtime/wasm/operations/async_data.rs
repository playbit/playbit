//! Checked typed begin/take lowering. Every result is retained across short buffers.
use super::{StoreData, call, query, read};
use crate::runtime::linear_memory::layout::data::query as query_layout;
use crate::runtime::{
    host::{DataOperation, DataResult},
    linear_memory::input,
    syscall::{HostError, Opcode, OperationHandle, pending_data},
};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;

pub(super) fn read_begin(
    caller: Caller<'_, impl StoreData>,
    entry: u32,
    object: u32,
    fields: u32,
    count: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::ObjectReadBegin)?;
        let object = memory.input(object, 1, 16, 1, 1, None)?;
        let fields = memory.input(fields, count, 16, 1, contract::MAX_FIELDS, None)?;
        let fields =
            (0..count as usize).map(|index| fields.read(memory.data, index * 16)).collect();
        let pending = pending_data::begin_entry(
            state,
            entry,
            DataOperation::Read {
                entry_id: entry.entry_id,
                object_id: object.read(memory.data, 0),
                fields,
            },
            Opcode::ObjectReadBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot) as i64)
    })
}

pub(super) fn edit_begin(
    caller: Caller<'_, impl StoreData>,
    entry: u32,
    object: u32,
    fields: u32,
    count: u32,
    stride: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::ObjectEditBegin)?;
        let object = memory.input(object, 1, 16, 1, 1, None)?;
        let fields = input::changes(memory, fields, count, stride, None)?;
        let pending = pending_data::begin_entry(
            state,
            entry,
            DataOperation::Edit {
                entry_id: entry.entry_id,
                object_id: object.read(memory.data, 0),
                fields,
            },
            Opcode::ObjectEditBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot) as i64)
    })
}

pub(super) fn query_begin(caller: Caller<'_, impl StoreData>, entry: u32, query: u32) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::ObjectQueryBegin)?;
        let query = memory.record(query, query_layout::LAYOUT, true, None)?;
        let flags = query.u32(memory.data, query_layout::FLAGS);
        let after = query.read(memory.data, query_layout::AFTER);
        let count = query.u32(memory.data, query_layout::COUNT);
        if query.u32(memory.data, query_layout::RESERVED) != 0
            || flags & !3 != 0
            || flags & 1 == 0 && after != [0; 16]
            || flags & 2 == 0 && count != 0
        {
            return Err(HostError::InvalidArgument);
        }
        let projection = memory.admit(
            query.u32(memory.data, query_layout::FIELDS),
            count,
            16,
            1,
            contract::MAX_FIELDS,
        )?;
        let fields =
            (0..count as usize).map(|index| projection.read(memory.data, index * 16)).collect();
        let pending = pending_data::begin_entry(
            state,
            entry,
            DataOperation::Query {
                entry_id: entry.entry_id,
                type_id: query.read(memory.data, query_layout::TYPE_ID),
                after: (flags & 1 != 0).then_some(after),
                limit: query.u32(memory.data, query_layout::LIMIT),
                projection: (flags & 2 != 0).then_some(fields),
            },
            Opcode::ObjectQueryBegin,
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
        pending_data::admit_take(
            state,
            OperationHandle { generation, slot },
            Opcode::ObjectReadTake,
        )?;
        let output = read::Output::admit(memory, pointer)?;
        pending_data::take(
            state,
            OperationHandle { generation, slot },
            Opcode::ObjectReadTake,
            |_, data| {
                let DataResult::Object(object) = data else { return Err(HostError::Internal) };
                output.write(memory.data, object)
            },
        )
    })
}

pub(super) fn edit_take(caller: Caller<'_, impl StoreData>, generation: u32, slot: u32) -> i64 {
    call(caller, |state, _| {
        pending_data::admit_take(
            state,
            OperationHandle { generation, slot },
            Opcode::ObjectEditTake,
        )?;
        pending_data::take(
            state,
            OperationHandle { generation, slot },
            Opcode::ObjectEditTake,
            |_, data| {
                if matches!(data, DataResult::Edited) { Ok(0) } else { Err(HostError::Internal) }
            },
        )
    })
}

pub(super) fn query_take(
    caller: Caller<'_, impl StoreData>,
    generation: u32,
    slot: u32,
    pointer: u32,
) -> i64 {
    call(caller, |state, memory| {
        pending_data::admit_take(
            state,
            OperationHandle { generation, slot },
            Opcode::ObjectQueryTake,
        )?;
        let output = query::Output::admit(memory, pointer)?;
        pending_data::take(
            state,
            OperationHandle { generation, slot },
            Opcode::ObjectQueryTake,
            |call, data| {
                let (DataOperation::Query { type_id, .. }, DataResult::Page(page)) =
                    (&call.operation, data)
                else {
                    return Err(HostError::Internal);
                };
                let counts = output.counts(memory.data, page)?;
                assert_eq!(output.write(memory.data, page, *type_id), counts);
                Ok(0)
            },
        )
    })
}
