//! Wasm KV admission copies only bounded input into existing async operations.
use super::{WasmState, call};
use crate::runtime::linear_memory::layout::kv::{query as query_layout, read as read_layout};
use crate::runtime::{
    host::{DataOperation, DataResult, kv_stored_root},
    linear_memory::{Memory, Region, disjoint, input},
    syscall::{HostError, HostResult, Opcode, OperationHandle, pending_data},
};
use playbit_api::abi::kv as abi;
use wasmtime::Caller;

fn root(
    memory: &Memory<'_>,
    query: Region,
    offset: usize,
    present: usize,
) -> HostResult<Option<crate::store::key_value::Root>> {
    kv_stored_root(
        query.u32(memory.data, present),
        abi::Root {
            level: query.u32(memory.data, offset),
            digest: query.read(memory.data, offset + 4),
        },
    )
}

pub(super) fn keys_begin(
    caller: Caller<'_, WasmState>,
    entry: u32,
    query: u32,
    after: u32,
    length: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::KvKeysBegin)?;
        let query = memory.record(query, query_layout::LAYOUT, true, None)?;
        if query.u32(memory.data, query_layout::RESERVED) != 0 {
            return Err(HostError::InvalidArgument);
        }
        let root = root(memory, query, query_layout::ROOT_LEVEL, query_layout::HAS_ROOT)?;
        let after = memory.input(after, length, 1, 1, abi::MAX_KEY_BYTES as u32, None)?;
        let handle = pending_data::begin_entry(
            state,
            entry,
            DataOperation::KvKeys {
                entry_id: entry.entry_id,
                root,
                limit: query.u32(memory.data, query_layout::LIMIT),
                after: (length != 0).then(|| after.bytes(memory.data).to_vec()),
            },
            Opcode::KvKeysBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

pub(super) fn read_begin(
    caller: Caller<'_, WasmState>,
    entry: u32,
    key: u32,
    length: u32,
    query: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::KvReadBegin)?;
        let query = memory.record(query, read_layout::LAYOUT, true, None)?;
        if query.u32(memory.data, read_layout::RESERVED) != 0 {
            return Err(HostError::InvalidArgument);
        }
        let root = root(memory, query, read_layout::ROOT_LEVEL, read_layout::HAS_ROOT)?;
        let key = memory.input(key, length, 1, 1, abi::MAX_KEY_BYTES as u32, None)?;
        let handle = pending_data::begin_entry(
            state,
            entry,
            DataOperation::KvRead {
                entry_id: entry.entry_id,
                key: key.bytes(memory.data).to_vec(),
                root,
                offset: query.u64(memory.data, read_layout::OFFSET),
                limit: query.u32(memory.data, read_layout::LIMIT),
            },
            Opcode::KvReadBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

pub(super) fn put_begin(
    caller: Caller<'_, WasmState>,
    entry: u32,
    key: u32,
    length: u32,
    value: u32,
    value_length: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::KvPutBegin)?;
        let key = memory.input(key, length, 1, 1, abi::MAX_KEY_BYTES as u32, None)?;
        let value = memory.input(value, value_length, 1, 1, abi::MAX_VALUE_BYTES as u32, None)?;
        let handle = pending_data::begin_entry(
            state,
            entry,
            DataOperation::KvPut {
                entry_id: entry.entry_id,
                key: key.bytes(memory.data).to_vec(),
                value: value.bytes(memory.data).to_vec(),
            },
            Opcode::KvPutBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

pub(super) fn delete_begin(
    caller: Caller<'_, WasmState>,
    entry: u32,
    key: u32,
    length: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (entry, _) = input::entry_handle(memory, entry, None)?;
        pending_data::admit_entry(state, entry, Opcode::KvDeleteBegin)?;
        let key = memory.input(key, length, 1, 1, abi::MAX_KEY_BYTES as u32, None)?;
        let handle = pending_data::begin_entry(
            state,
            entry,
            DataOperation::KvDelete {
                entry_id: entry.entry_id,
                key: key.bytes(memory.data).to_vec(),
            },
            Opcode::KvDeleteBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

pub(super) fn bytes_take(
    caller: Caller<'_, WasmState>,
    generation: u32,
    slot: u32,
    opcode: Opcode,
    pointer: u32,
    capacity: u32,
    count: u32,
) -> i64 {
    call(caller, |state, memory| {
        let handle = OperationHandle { generation, slot };
        pending_data::admit_take(state, handle, opcode)?;
        let limit = match opcode {
            Opcode::KvKeysTake => abi::MAX_PAGE_BYTES,
            Opcode::KvReadTake => abi::MAX_CHUNK_BYTES,
            _ => return Err(HostError::Unsupported),
        };
        let count = memory.admit(count, 1, 4, 4, 1)?;
        let bytes = memory.admit(pointer, capacity, 1, 1, limit as u32)?;
        disjoint(&[count, bytes])?;
        pending_data::take(state, handle, opcode, |_, data| {
            let length = data.kv_output_bytes()?;
            count.write(memory.data, 0, &(length as u32).to_le_bytes());
            if length > capacity as usize {
                return Err(HostError::BufferTooSmall);
            }
            let output = bytes.sub(0, length).bytes_mut(memory.data);
            data.encode_kv(output)?;
            Ok(0)
        })
    })
}

pub(super) fn receipt_take(
    caller: Caller<'_, WasmState>,
    generation: u32,
    slot: u32,
    opcode: Opcode,
) -> i64 {
    call(caller, |state, _| {
        pending_data::take(state, OperationHandle { generation, slot }, opcode, |_, data| {
            match (opcode, data) {
                (Opcode::KvPutTake, DataResult::KvWritten)
                | (Opcode::KvDeleteTake, DataResult::KvDeleted) => Ok(0),
                _ => Err(HostError::Internal),
            }
        })
    })
}
