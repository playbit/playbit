//! Wasm File adapters use fixed little-endian records and preadmitted spans.
use super::{StoreData, call, output::Buffer};
use crate::runtime::linear_memory::layout::files::{
    file_close_result, file_list_entry, file_reference, file_status,
};
use crate::runtime::{
    host::files::{Action, Output},
    linear_memory::{Region, disjoint},
    syscall::{self as rt, HostError, HostResult, VolumeHandle, pending_files},
};
use playbit_api::{
    abi::{files as abi, syscall::Opcode},
    ui::FileReference,
};
use wasmtime::Caller;

pub(super) fn reference(memory: &[u8], record: Region) -> HostResult<FileReference> {
    if record.read::<1>(memory, file_reference::RESERVED) != [0]
        || record.read::<15>(memory, file_reference::ENTRY_ID) == [0; 15]
    {
        return Err(HostError::InvalidArgument);
    }
    Ok(FileReference {
        volume: VolumeHandle {
            generation: (record.u64(memory, file_reference::VOLUME) >> 32) as u32,
            slot: record.u64(memory, file_reference::VOLUME) as u32,
        },
        entry_id: record.read(memory, file_reference::ENTRY_ID),
    })
}
fn file(packed: u64) -> abi::FileHandle {
    abi::FileHandle(packed)
}
fn operation(packed: u64) -> rt::OperationHandle {
    let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(packed);
    rt::OperationHandle { generation, slot }
}
pub(super) fn dispatch(caller: Caller<'_, impl StoreData>, opcode: Opcode, slots: [u64; 6]) -> i64 {
    let [a, b, c, d, _, _] = slots;
    match opcode {
        Opcode::FileOpenBegin | Opcode::FileWriteBegin => {
            input_begin(caller, opcode, a, b, c as u32, d as u32)
        }
        Opcode::FileReadBegin => {
            begin(caller, file(a), Action::Read { offset: b, length: c as u32 })
        }
        Opcode::FileStatusBegin => begin(caller, file(a), Action::Status),
        Opcode::FileCloseBegin => begin(caller, file(a), Action::Close { flags: b as u32 }),
        Opcode::FileListOpenBegin if b == 0 => begin(caller, file(a), Action::ListOpen),
        Opcode::FileListOpenBegin => HostError::InvalidArgument as i64,
        Opcode::FileListNextBegin => next_begin(caller, file(a)),
        Opcode::FileReadTake => read_take(caller, operation(a), b as u32, c as u32, d as u32),
        Opcode::FileListNextTake => next_take(caller, operation(a), b as u32),
        Opcode::FileWriteTake => {
            call(caller, |state, _| pending_files::take(state, operation(a), opcode, |_, _| Ok(0)))
        }
        Opcode::FileListClose => call(caller, |state, _| {
            pending_files::list_close(state, abi::FileListHandle(a)).map(|()| 0)
        }),
        Opcode::FileDispose => {
            call(caller, |state, _| pending_files::discard(state, file(a)).map(|()| 0))
        }
        Opcode::OperationDispose => {
            call(caller, |state, _| pending_files::dispose(state, operation(a)).map(|()| 0))
        }
        _ => fixed_take(caller, opcode, operation(a), b as u32),
    }
}
fn begin(caller: Caller<'_, impl StoreData>, file: abi::FileHandle, action: Action) -> i64 {
    call(caller, |state, _memory| {
        let handle = pending_files::begin(state, file, action)?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}
fn input_begin(
    caller: Caller<'_, impl StoreData>,
    opcode: Opcode,
    a: u64,
    b: u64,
    c: u32,
    d: u32,
) -> i64 {
    call(caller, |state, memory| {
        let (pointer, length, limit) = if opcode == Opcode::FileOpenBegin {
            (b as u32, c, 4096)
        } else {
            (c, d, abi::MAX_FILE_WRITE)
        };
        let content = memory.input(pointer, length, 1, 1, limit, None)?.bytes(memory.data).to_vec();
        let action = if opcode == Opcode::FileOpenBegin {
            Action::Open { path: content, mode: d }
        } else {
            Action::Write { offset: b, bytes: content }
        };
        let handle = pending_files::begin(state, file(a), action)?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}
fn next_begin(caller: Caller<'_, impl StoreData>, file: abi::FileHandle) -> i64 {
    call(caller, |state, _memory| {
        let handle = pending_files::list_next(state, abi::FileListHandle(file.0))?;
        Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
    })
}
fn fixed_take(
    caller: Caller<'_, impl StoreData>,
    opcode: Opcode,
    operation: rt::OperationHandle,
    pointer: u32,
) -> i64 {
    call(caller, |state, memory| {
        pending_files::admit_take(state, operation, opcode)?;
        let layout = match opcode {
            Opcode::FileOpenTake | Opcode::FileListOpenTake => {
                return pending_files::take(state, operation, opcode, |_, handle| {
                    Ok(handle.ok_or(HostError::Internal)? as i64)
                });
            }
            Opcode::FileStatusTake => file_status::LAYOUT,
            Opcode::FileCloseTake => file_close_result::LAYOUT,
            _ => return Err(HostError::Unsupported),
        };
        let record = memory.record(pointer, layout, false, None)?;
        record.clear(memory.data);
        pending_files::take(state, operation, opcode, |output, slot| {
            match output {
                Output::Status { kind, flags, size, modification_time } => {
                    record.write(memory.data, file_status::KIND, &kind.to_le_bytes());
                    record.write(memory.data, file_status::FLAGS, &flags.to_le_bytes());
                    record.write(memory.data, file_status::FILE_SIZE, &size.to_le_bytes());
                    record.write(
                        memory.data,
                        file_status::MODIFICATION_TIME,
                        &modification_time.to_le_bytes(),
                    );
                }
                Output::Close { error, publication, .. } => {
                    record.write(memory.data, file_close_result::ERROR, &error.to_le_bytes());
                    record.write(
                        memory.data,
                        file_close_result::PUBLICATION,
                        &publication.to_le_bytes(),
                    );
                    let recovery = slot.unwrap_or(0);
                    record.write(memory.data, file_close_result::RECOVERY, &recovery.to_le_bytes());
                }
                _ => return Err(HostError::Internal),
            }
            record.publish(memory.data, layout);
            Ok(0)
        })
    })
}
fn read_take(
    caller: Caller<'_, impl StoreData>,
    operation: rt::OperationHandle,
    pointer: u32,
    capacity: u32,
    count: u32,
) -> i64 {
    call(caller, |state, memory| {
        pending_files::admit_take(state, operation, Opcode::FileReadTake)?;
        let count = memory.admit(count, 1, 4, 4, 1)?;
        let bytes = memory.admit(pointer, capacity, 1, 1, abi::MAX_FILE_READ)?;
        disjoint(&[count, bytes])?;
        pending_files::take(state, operation, Opcode::FileReadTake, |output, _| {
            let Output::Read { bytes: content } = output else { return Err(HostError::Internal) };
            count.write(memory.data, 0, &(content.len() as u32).to_le_bytes());
            if content.len() > capacity as usize {
                return Err(HostError::BufferTooSmall);
            }
            bytes.write(memory.data, 0, content);
            Ok(0)
        })
    })
}
fn next_take(
    caller: Caller<'_, impl StoreData>,
    operation: rt::OperationHandle,
    pointer: u32,
) -> i64 {
    call(caller, |state, memory| {
        pending_files::admit_take(state, operation, Opcode::FileListNextTake)?;
        let record = memory.record(pointer, file_list_entry::LAYOUT, false, None)?;
        let name = Buffer::admit(memory, record.sub(file_list_entry::NAME_DATA, 12), 1, 1, 255)?;
        disjoint(&[record, name.data])?;
        record.clear(memory.data);
        pending_files::take(state, operation, Opcode::FileListNextTake, |output, _| {
            let (kind, content, outcome) = match output {
                Output::Entry { kind, name } => (*kind, name.as_slice(), abi::FILE_LIST_ENTRY),
                Output::End => (0, &[][..], abi::FILE_LIST_END),
                _ => return Err(HostError::Internal),
            };
            name.count(memory.data, content.len());
            if !name.fits(content.len()) {
                return Err(HostError::BufferTooSmall);
            }
            name.data.write(memory.data, 0, content);
            record.write(memory.data, file_list_entry::OUTCOME, &outcome.to_le_bytes());
            record.write(memory.data, file_list_entry::KIND, &kind.to_le_bytes());
            record.publish(memory.data, file_list_entry::LAYOUT);
            Ok(0)
        })
    })
}
