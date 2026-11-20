//! Native pending operations copy inputs before admission and validate poll outputs.
use super::{
    calls::{span, with_context},
    data::entry_handle,
    files::reference,
};
use crate::runtime::syscall::{self as rt, HostResult, file_ops, handles, pending_data};
use playbit_api::abi::{data, files, operations::Context, pending as abi};
fn operation(handle: abi::OperationHandle) -> HostResult<rt::OperationHandle> {
    Ok(rt::OperationHandle { generation: handle.generation(), slot: handle.slot() })
}
fn result(handle: rt::OperationHandle) -> abi::OperationHandle {
    abi::OperationHandle::new(handle.generation, handle.slot)
}
/// # Safety
/// Stable type and initial field inputs obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_object_create_begin(
    context: *mut Context,
    entry: *const data::EntryHandle,
    type_id: *const [u8; 16],
    changes: *const data::FieldInput,
    count: u32,
    stride: u32,
) -> i64 {
    // SAFETY: input admission and copying finish before starting host work.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::ObjectCreateBegin)?;
            span(type_id, 1)?;
            let changes = super::data::decode_changes(changes, count, stride)?;
            let pending = pending_data::begin_entry(
                state,
                entry,
                crate::runtime::host::DataOperation::Create {
                    entry_id: entry.entry_id,
                    type_id: type_id.read(),
                    fields: changes,
                },
                rt::Opcode::ObjectCreateBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}
/// # Safety
/// Stable file inputs obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_file_copy_begin(
    context: *mut Context,
    source: *const files::FileReference,
    destination: *const files::FileReference,
) -> i64 {
    // SAFETY: record spans are admitted before copying stable file references.
    unsafe {
        with_context(context, |state| {
            super::record::region(source, false)?;
            super::record::region(destination, false)?;
            let pending = file_ops::copy_begin(
                state,
                reference(super::record::read(source, true)?)?,
                reference(super::record::read(destination, true)?)?,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}
/// # Safety
/// Context lifetime and thread ownership obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_file_select_begin(context: *mut Context) -> i64 {
    // SAFETY: the current-thread context is validated before requesting selection.
    unsafe {
        with_context(context, |state| {
            let pending = file_ops::select_begin(state)?;
            Ok((result(pending)).0 as i64)
        })
    }
}

unsafe fn poll(
    context: *mut Context,
    handle: abi::OperationHandle,
    output: *mut [u8; 16],
    opcode: rt::Opcode,
) -> i64 {
    // SAFETY: fixed output is writable and admitted before any result consumption.
    unsafe {
        with_context(context, |state| {
            span(output, 1)?;
            let value = handles::poll_operation(state, operation(handle)?, opcode)?;
            output.write(value);
            Ok(0)
        })
    }
}
/// # Safety
/// Writable ID output obeys playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_object_create_poll(
    context: *mut Context,
    handle: abi::OperationHandle,
    output: *mut [u8; 16],
) -> i64 {
    // SAFETY: forwards the same fixed-output contract.
    unsafe { poll(context, handle, output, rt::Opcode::ObjectCreatePoll) }
}
/// # Safety
/// Writable ID output obeys playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_file_copy_poll(
    context: *mut Context,
    handle: abi::OperationHandle,
    output: *mut [u8; 15],
) -> i64 {
    // SAFETY: fixed output is admitted before consuming any result.
    unsafe {
        with_context(context, |state| {
            span(output, 1)?;
            output.write(file_ops::copy_poll(state, operation(handle)?)?);
            Ok(0)
        })
    }
}

/// # Safety
/// Writable selection output obeys playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_file_select_poll(
    context: *mut Context,
    handle: abi::OperationHandle,
    output: *mut abi::Selection,
) -> i64 {
    // SAFETY: admit the entire result even when selection ultimately reports cancellation.
    unsafe {
        with_context(context, |state| {
            super::record::region(output, true)?;
            playbit_api::abi::record::clear_native(output)?;
            let selected = file_ops::select_poll(state, operation(handle)?)?;
            let mut value = abi::Selection::default();
            if let Some(file) = selected {
                value.present = 1;
                value.volume = playbit_api::abi::dispatch::pack_handle(
                    file.volume.generation,
                    file.volume.slot,
                );
                value.entry_id = file.entry_id;
            }
            super::record::write(output, value)?;
            Ok(0)
        })
    }
}
