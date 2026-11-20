//! Native File adapters copy inputs and admit result storage before consumption.
use super::{
    calls::{bytes, span, with_context},
    data::{append_bytes, disjoint, handle, output_span},
};
use crate::runtime::{
    host::files::{Action, Output},
    syscall::{self as rt, HostError, HostResult, pending_files},
};
use playbit_api::{
    abi::{data::Buffer, files as abi, operations::Context, syscall::Opcode},
    ui::FileReference,
};

pub(super) fn reference(file: abi::FileReference) -> HostResult<FileReference> {
    if file.reserved != 0 || file.entry_id == [0; 15] {
        return Err(HostError::InvalidArgument);
    }
    Ok(FileReference {
        volume: handle(playbit_api::abi::data::VolumeHandle(file.volume))?,
        entry_id: file.entry_id,
    })
}
fn file(packed: u64) -> abi::FileHandle {
    abi::FileHandle(packed)
}
fn operation(packed: u64) -> rt::OperationHandle {
    let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(packed);
    rt::OperationHandle { generation, slot }
}
/// # Safety
/// Scalar admission and the native C pointer contract precede this dispatcher.
pub(super) unsafe fn dispatch(context: *mut Context, opcode: Opcode, slots: [u64; 6]) -> i64 {
    let [a, b, c, d, _, _] = slots;
    // SAFETY: every typed branch checks its spans before reading or writing.
    unsafe {
        match opcode {
            Opcode::FileOpenBegin | Opcode::FileWriteBegin => {
                input_begin(context, opcode, a, b, c, d)
            }
            Opcode::FileReadBegin => {
                plain_begin(context, file(a), Action::Read { offset: b, length: c as u32 })
            }
            Opcode::FileStatusBegin => plain_begin(context, file(a), Action::Status),
            Opcode::FileCloseBegin => {
                plain_begin(context, file(a), Action::Close { flags: b as u32 })
            }
            Opcode::FileListOpenBegin if b == 0 => plain_begin(context, file(a), Action::ListOpen),
            Opcode::FileListOpenBegin => HostError::InvalidArgument as i64,
            Opcode::FileListNextBegin => next_begin(context, file(a)),
            Opcode::FileReadTake => {
                read_take(context, operation(a), b as usize as _, c as u32, d as usize as _)
            }
            Opcode::FileListNextTake => next_take(context, operation(a), b as usize as _),
            Opcode::FileWriteTake => with_context(context, |state| {
                pending_files::take(state, operation(a), opcode, |_, _| Ok(0))
            }),
            Opcode::FileListClose => with_context(context, |state| {
                pending_files::list_close(state, abi::FileListHandle(a)).map(|()| 0)
            }),
            Opcode::FileDispose => {
                with_context(context, |state| pending_files::discard(state, file(a)).map(|()| 0))
            }
            Opcode::OperationDispose => with_context(context, |state| {
                pending_files::dispose(state, operation(a)).map(|()| 0)
            }),
            _ => fixed_take(context, opcode, operation(a), b as usize),
        }
    }
}
unsafe fn plain_begin(context: *mut Context, file: abi::FileHandle, action: Action) -> i64 {
    // SAFETY: the current-thread context is validated before admitting the operation.
    unsafe {
        with_context(context, |state| {
            let handle = pending_files::begin(state, file, action)?;
            Ok((playbit_api::abi::pending::OperationHandle::new(handle.generation, handle.slot)).0
                as i64)
        })
    }
}
unsafe fn input_begin(
    context: *mut Context,
    opcode: Opcode,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
) -> i64 {
    // SAFETY: input spans are admitted and bounded before copying.
    unsafe {
        with_context(context, |state| {
            let (pointer, length, limit) = if opcode == Opcode::FileOpenBegin {
                (b as usize as *const u8, c as u32, 4096)
            } else {
                (c as usize as *const u8, d as u32, abi::MAX_FILE_WRITE as usize)
            };
            span(pointer, length as usize)?;
            let content = bytes(pointer, length, limit)?.to_vec();
            let action = if opcode == Opcode::FileOpenBegin {
                Action::Open { path: content, mode: d as u32 }
            } else {
                Action::Write { offset: b, bytes: content }
            };
            let handle = pending_files::begin(state, file(a), action)?;
            Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
        })
    }
}
unsafe fn next_begin(context: *mut Context, file: abi::FileHandle) -> i64 {
    // SAFETY: the current-thread context is validated before cursor reservation.
    unsafe {
        with_context(context, |state| {
            let handle = pending_files::list_next(state, abi::FileListHandle(file.0))?;
            Ok((playbit_api::abi::pending::OperationHandle::new(handle.generation, handle.slot)).0
                as i64)
        })
    }
}
unsafe fn fixed_take(
    context: *mut Context,
    opcode: Opcode,
    operation: rt::OperationHandle,
    pointer: usize,
) -> i64 {
    // SAFETY: admit the complete fixed result before peeking at the completion.
    unsafe {
        with_context(context, |state| {
            pending_files::admit_take(state, operation, opcode)?;
            match opcode {
                Opcode::FileOpenTake | Opcode::FileListOpenTake => {
                    return pending_files::take(state, operation, opcode, |_, handle| {
                        Ok(handle.ok_or(HostError::Internal)? as i64)
                    });
                }
                Opcode::FileStatusTake => {
                    super::record::region(pointer as *mut abi::FileStatus, true)?;
                    playbit_api::abi::record::clear_native(pointer as *mut abi::FileStatus)?;
                }
                Opcode::FileCloseTake => {
                    super::record::region(pointer as *mut abi::FileCloseResult, true)?;
                    playbit_api::abi::record::clear_native(pointer as *mut abi::FileCloseResult)?;
                }
                _ => return Err(HostError::Unsupported),
            }
            pending_files::take(state, operation, opcode, |output, slot| {
                match output {
                    Output::Status { kind, flags, size, modification_time } => {
                        super::record::write(
                            pointer as *mut abi::FileStatus,
                            abi::FileStatus {
                                kind: *kind,
                                flags: *flags,
                                file_size: *size,
                                modification_time: *modification_time,
                                ..abi::FileStatus::default()
                            },
                        )?;
                    }
                    Output::Close { error, publication, .. } => {
                        super::record::write(
                            pointer as *mut abi::FileCloseResult,
                            abi::FileCloseResult {
                                error: *error,
                                publication: *publication,
                                recovery: slot.unwrap_or_default(),
                                ..abi::FileCloseResult::default()
                            },
                        )?;
                    }
                    _ => return Err(HostError::Internal),
                }
                Ok(0)
            })
        })
    }
}
unsafe fn read_take(
    context: *mut Context,
    operation: rt::OperationHandle,
    pointer: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: descriptor and payload spans are checked before retained delivery.
    unsafe {
        with_context(context, |state| {
            pending_files::admit_take(state, operation, Opcode::FileReadTake)?;
            disjoint(&[
                span(count, 1)?,
                output_span(&Buffer::new(pointer, capacity), abi::MAX_FILE_READ as usize)?,
            ])?;
            pending_files::take(state, operation, Opcode::FileReadTake, |output, _| {
                let Output::Read { bytes } = output else { return Err(HostError::Internal) };
                count.write(bytes.len() as u32);
                if bytes.len() > capacity as usize {
                    return Err(HostError::BufferTooSmall);
                }
                append_bytes(pointer, &mut 0, bytes);
                Ok(0)
            })
        })
    }
}
unsafe fn next_take(
    context: *mut Context,
    operation: rt::OperationHandle,
    pointer: *mut abi::FileListEntry,
) -> i64 {
    // SAFETY: a short buffer writes only the required count, without consuming.
    unsafe {
        with_context(context, |state| {
            pending_files::admit_take(state, operation, Opcode::FileListNextTake)?;
            let descriptor = super::record::region(pointer, true)?;
            let mut entry = super::record::read(pointer, false)?;
            disjoint(&[
                descriptor,
                output_span(&Buffer::new(entry.name_data, entry.name_capacity), 255)?,
            ])?;
            playbit_api::abi::record::clear_native(pointer)?;
            pending_files::take(state, operation, Opcode::FileListNextTake, |output, _| {
                let (kind, name, outcome) = match output {
                    Output::Entry { kind, name } => (*kind, name.as_slice(), abi::FILE_LIST_ENTRY),
                    Output::End => (0, &[][..], abi::FILE_LIST_END),
                    _ => return Err(HostError::Internal),
                };
                entry.name_count = name.len() as u32;
                if entry.name_count > entry.name_capacity {
                    super::record::counts(pointer, entry)?;
                    return Err(HostError::BufferTooSmall);
                }
                append_bytes(entry.name_data, &mut 0, name);
                entry.kind = kind;
                entry.outcome = outcome;
                super::record::write(pointer, entry)?;
                Ok(0)
            })
        })
    }
}
