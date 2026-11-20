//! Pointer admission and owned KV begin/take; no storage runs on this thread.
use super::{
    calls::{span, with_context},
    data::{disjoint, entry_handle, input, output_span},
};
use crate::runtime::{
    host::{DataOperation, DataResult, kv_stored_root},
    syscall::{self as rt, pending_data},
};
use playbit_api::abi::{
    data as data_abi, kv as abi, operations::Context, pending::OperationHandle,
};

fn handle(value: OperationHandle) -> rt::OperationHandle {
    rt::OperationHandle { generation: value.generation(), slot: value.slot() }
}
fn result(value: rt::OperationHandle) -> OperationHandle {
    OperationHandle::new(value.generation, value.slot)
}

/// # Safety
/// Stable input descriptors and bytes obey playbit/sys/abi.h.
pub(super) unsafe fn keys_begin(
    context: *mut Context,
    entry: *const data_abi::EntryHandle,
    query: *const abi::Query,
    after: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: admit each pointer before copying or admitting an operation.
    unsafe {
        with_context(context, |state| {
            super::record::region(entry, false)?;
            super::record::region(query, false)?;
            span(after, length as usize)?;
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::KvKeysBegin)?;
            let query = super::record::read(query, true)?;
            if query.reserved != 0 {
                return Err(rt::HostError::InvalidArgument);
            }
            let root = kv_stored_root(
                query.has_root,
                abi::Root { level: query.root_level, digest: query.root_digest },
            )?;
            let after = input(after, length as usize, abi::MAX_KEY_BYTES)?;
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::KvKeys {
                    entry_id: entry.entry_id,
                    after: (!after.is_empty()).then(|| after.to_vec()),
                    root,
                    limit: query.limit,
                },
                rt::Opcode::KvKeysBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// All stable input spans obey playbit/sys/abi.h.
pub(super) unsafe fn read_begin(
    context: *mut Context,
    entry: *const data_abi::EntryHandle,
    key: *const u8,
    length: u32,
    query: *const abi::Read,
) -> i64 {
    // SAFETY: query and key spans are admitted before copying.
    unsafe {
        with_context(context, |state| {
            super::record::region(entry, false)?;
            super::record::region(query, false)?;
            span(key, length as usize)?;
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::KvReadBegin)?;
            let query = super::record::read(query, true)?;
            if query.reserved != 0 {
                return Err(rt::HostError::InvalidArgument);
            }
            let root = kv_stored_root(
                query.has_root,
                abi::Root { level: query.root_level, digest: query.root_digest },
            )?;
            let key = input(key, length as usize, abi::MAX_KEY_BYTES)?.to_vec();
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::KvRead {
                    entry_id: entry.entry_id,
                    key,
                    root,
                    offset: query.offset,
                    limit: query.limit,
                },
                rt::Opcode::KvReadBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// Stable key and value inputs obey playbit/sys/abi.h.
pub(super) unsafe fn put_begin(
    context: *mut Context,
    entry: *const data_abi::EntryHandle,
    key: *const u8,
    length: u32,
    value: *const u8,
    value_length: u32,
) -> i64 {
    // SAFETY: every borrowed region is admitted before owned copying.
    unsafe {
        with_context(context, |state| {
            super::record::region(entry, false)?;
            span(key, length as usize)?;
            span(value, value_length as usize)?;
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::KvPutBegin)?;
            let key = input(key, length as usize, abi::MAX_KEY_BYTES)?.to_vec();
            let value = input(value, value_length as usize, abi::MAX_VALUE_BYTES)?.to_vec();
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::KvPut { entry_id: entry.entry_id, key, value },
                rt::Opcode::KvPutBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// Stable key and entry inputs obey playbit/sys/abi.h.
pub(super) unsafe fn delete_begin(
    context: *mut Context,
    entry: *const data_abi::EntryHandle,
    key: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: input spans are admitted before copying and creating an operation.
    unsafe {
        with_context(context, |state| {
            super::record::region(entry, false)?;
            span(key, length as usize)?;
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::KvDeleteBegin)?;
            let key = input(key, length as usize, abi::MAX_KEY_BYTES)?.to_vec();
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::KvDelete { entry_id: entry.entry_id, key },
                rt::Opcode::KvDeleteBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// Initialized descriptor and disjoint caller bytes obey playbit/sys/abi.h.
pub(super) unsafe fn bytes_take(
    context: *mut Context,
    pending: OperationHandle,
    opcode: rt::Opcode,
    output: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: pointer admission precedes completion inspection or consumption.
    unsafe {
        with_context(context, |state| {
            let handle = handle(pending);
            pending_data::admit_take(state, handle, opcode)?;
            let limit = match opcode {
                rt::Opcode::KvKeysTake => abi::MAX_PAGE_BYTES,
                rt::Opcode::KvReadTake => abi::MAX_CHUNK_BYTES,
                _ => return Err(rt::HostError::Unsupported),
            };
            disjoint(&[
                span(count, 1)?,
                output_span(&data_abi::Buffer::new(output, capacity), limit)?,
            ])?;
            pending_data::take(state, handle, opcode, |_, data| {
                let length = data.kv_output_bytes()?;
                count.write(length as u32);
                if length > capacity as usize {
                    return Err(rt::HostError::BufferTooSmall);
                }
                // SAFETY: output_span admitted this disjoint mutable range. Null is
                // allowed for zero capacity only; successful KV frames are nonempty.
                let bytes = std::slice::from_raw_parts_mut(output, length);
                data.encode_kv(bytes)?;
                Ok(0)
            })
        })
    }
}

/// # Safety
/// Context is the borrowed owner-thread instance in playbit/sys/abi.h.
pub(super) unsafe fn receipt_take(
    context: *mut Context,
    pending: OperationHandle,
    opcode: rt::Opcode,
) -> i64 {
    // SAFETY: no caller storage is accessed by an acknowledgment.
    unsafe {
        with_context(context, |state| {
            pending_data::take(state, handle(pending), opcode, |_, data| match (opcode, data) {
                (rt::Opcode::KvPutTake, DataResult::KvWritten)
                | (rt::Opcode::KvDeleteTake, DataResult::KvDeleted) => Ok(0),
                _ => Err(rt::HostError::Internal),
            })
        })
    }
}
