//! Wasm bloblog admission copies bounded inputs into the shared async owner.
use super::{StoreData, call};
use crate::runtime::{
    host::DataOperation,
    linear_memory::{disjoint, input},
    syscall::{HostError, Opcode, OperationHandle, pending_data},
};
use playbit_api::abi::bloblog as abi;
use wasmtime::Caller;

pub(super) fn append_begin(
    caller: Caller<'_, impl StoreData>,
    volume: u64,
    log: u32,
    blobs: u32,
    count: u32,
    stride: u32,
    append_id: u32,
) -> i64 {
    call(caller, |state, memory| {
        let log = memory.input(log, 15, 1, 1, 15, None)?;
        if append_id == 0 {
            return Err(HostError::InvalidArgument);
        }
        let append_id = memory.input(append_id, 16, 1, 1, 16, None)?.read(memory.data, 0);
        let entry = playbit_api::abi::data::EntryHandle {
            volume,
            entry_id: log.read(memory.data, 0),
            ..Default::default()
        };
        pending_data::admit_entry(state, entry, Opcode::BloblogAppendBegin)?;
        let blobs = input::bloblog_blobs(memory, blobs, count, stride, None)?;
        let handle = pending_data::begin_entry(
            state,
            entry,
            DataOperation::BloblogAppend { log: entry.entry_id, append_id, blobs },
            Opcode::BloblogAppendBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

pub(super) fn read_begin(
    caller: Caller<'_, impl StoreData>,
    volume: u64,
    log: u32,
    cursor: u32,
    length: u32,
) -> i64 {
    call(caller, |state, memory| {
        let log = memory.input(log, 15, 1, 1, 15, None)?;
        let entry = playbit_api::abi::data::EntryHandle {
            volume,
            entry_id: log.read(memory.data, 0),
            ..Default::default()
        };
        pending_data::admit_entry(state, entry, Opcode::BloblogReadBegin)?;
        let bytes = memory.input(cursor, length, 1, 1, abi::MAX_CURSOR_BYTES as u32, None)?;
        let cursor = if length == 0 {
            None
        } else {
            Some(
                crate::store::bloblog::Cursor::parse(abi::cursor(bytes.bytes(memory.data))?)
                    .map_err(|_| HostError::InvalidArgument)?,
            )
        };
        let handle = pending_data::begin_entry(
            state,
            entry,
            DataOperation::BloblogRead { log: entry.entry_id, cursor },
            Opcode::BloblogReadBegin,
        )?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}

pub(super) fn take(
    caller: Caller<'_, impl StoreData>,
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
        let count = memory.admit(count, 1, 4, 4, 1)?;
        let limit = match opcode {
            Opcode::BloblogAppendTake => abi::MAX_CURSOR_BYTES,
            Opcode::BloblogReadTake => abi::MAX_PAGE_BYTES,
            _ => return Err(HostError::Unsupported),
        };
        let bytes = memory.admit(pointer, capacity, 1, 1, limit as u32)?;
        disjoint(&[count, bytes])?;
        pending_data::take(state, handle, opcode, |_, result| {
            let length = result.bloblog_output_bytes()?;
            count.write(memory.data, 0, &(length as u32).to_le_bytes());
            if length > capacity as usize {
                return Err(HostError::BufferTooSmall);
            }
            result.encode_bloblog(bytes.sub(0, length).bytes_mut(memory.data))?;
            Ok(0)
        })
    })
}
