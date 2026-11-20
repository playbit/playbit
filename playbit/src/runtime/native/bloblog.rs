//! Bloblog pointer admission precedes owned work; take never repeats storage I/O.
use super::{
    calls::{span, with_context},
    data::{disjoint, input, output_span},
    record,
};
use crate::runtime::{
    host::DataOperation,
    syscall::{self as rt, pending_data},
};
use playbit_api::abi::{bloblog as abi, data, operations::Context, pending::OperationHandle};

/// # Safety
/// Stable descriptors and payloads obey playbit/sys/abi.h.
pub(super) unsafe fn append_begin(
    context: *mut Context,
    volume: data::VolumeHandle,
    log: *const u8,
    blobs: *const abi::Blob,
    count: u32,
    stride: u32,
    append_id: *const u8,
) -> i64 {
    // SAFETY: every input range is admitted before copying or queue admission.
    unsafe {
        with_context(context, |state| {
            span(log, 15)?;
            span(append_id, 16)?;
            let append_id = append_id.cast::<[u8; 16]>().read();
            record::input_span(blobs, count, stride, abi::MAX_BLOBS)?;
            let entry = data::EntryHandle {
                volume: volume.0,
                entry_id: log.cast::<[u8; 15]>().read(),
                ..Default::default()
            };
            pending_data::admit_entry(state, entry, rt::Opcode::BloblogAppendBegin)?;
            let blobs = record::inputs(blobs, count, stride, abi::MAX_BLOBS)?;
            abi::validate_lengths(blobs.iter().map(|blob| blob.length as usize))?;
            for blob in &blobs {
                span(blob.data, blob.length as usize)?;
            }
            let blobs = blobs
                .iter()
                .map(|blob| {
                    input(blob.data, blob.length as usize, abi::MAX_BLOB_BYTES).map(<[u8]>::to_vec)
                })
                .collect::<rt::HostResult<Vec<_>>>()?;
            let handle = pending_data::begin_entry(
                state,
                entry,
                DataOperation::BloblogAppend { log: entry.entry_id, append_id, blobs },
                rt::Opcode::BloblogAppendBegin,
            )?;
            Ok((OperationHandle::new(handle.generation, handle.slot)).0 as i64)
        })
    }
}

/// # Safety
/// Stable log ID and cursor inputs obey playbit/sys/abi.h.
pub(super) unsafe fn read_begin(
    context: *mut Context,
    volume: data::VolumeHandle,
    log: *const u8,
    cursor: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: copied inputs never retain a guest pointer after return.
    unsafe {
        with_context(context, |state| {
            span(log, 15)?;
            span(cursor, length as usize)?;
            let entry = data::EntryHandle {
                volume: volume.0,
                entry_id: log.cast::<[u8; 15]>().read(),
                ..Default::default()
            };
            pending_data::admit_entry(state, entry, rt::Opcode::BloblogReadBegin)?;
            let bytes = input(cursor, length as usize, abi::MAX_CURSOR_BYTES)?;
            let cursor = if bytes.is_empty() {
                None
            } else {
                Some(
                    crate::store::bloblog::Cursor::parse(abi::cursor(bytes)?)
                        .map_err(|_| rt::HostError::InvalidArgument)?,
                )
            };
            let handle = pending_data::begin_entry(
                state,
                entry,
                DataOperation::BloblogRead { log: entry.entry_id, cursor },
                rt::Opcode::BloblogReadBegin,
            )?;
            Ok((OperationHandle::new(handle.generation, handle.slot)).0 as i64)
        })
    }
}

/// # Safety
/// Disjoint caller-owned count and output bytes obey playbit/sys/abi.h.
pub(super) unsafe fn take(
    context: *mut Context,
    pending: OperationHandle,
    opcode: rt::Opcode,
    output: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: admission precedes inspection or consumption of the completion.
    unsafe {
        with_context(context, |state| {
            let handle =
                rt::OperationHandle { generation: pending.generation(), slot: pending.slot() };
            pending_data::admit_take(state, handle, opcode)?;
            let limit = match opcode {
                rt::Opcode::BloblogAppendTake => abi::MAX_CURSOR_BYTES,
                rt::Opcode::BloblogReadTake => abi::MAX_PAGE_BYTES,
                _ => return Err(rt::HostError::Unsupported),
            };
            disjoint(&[
                span(count, 1)?,
                output_span(&data::Buffer::new(output, capacity), limit)?,
            ])?;
            pending_data::take(state, handle, opcode, |_, result| {
                let length = result.bloblog_output_bytes()?;
                count.write(length as u32);
                if length > capacity as usize {
                    return Err(rt::HostError::BufferTooSmall);
                }
                // SAFETY: admitted capacity fits the nonempty cursor/page frame.
                result.encode_bloblog(std::slice::from_raw_parts_mut(output, length))?;
                Ok(0)
            })
        })
    }
}
