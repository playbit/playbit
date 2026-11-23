//! One exported syscall boundary. Scalar admission precedes all pointer access.
//! Typed handlers retain ownership/authorization checks and return in tail position.
use super::*;
use playbit_api::abi::{data as data_abi, dispatch, pending as pending_abi};

/// # Safety
/// Context and all argument pointers obey playbit/sys/abi.h. Native callers own
/// allocation validity, alignment, nonoverlap and stable storage for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_syscall(
    context: *mut abi::Context,
    operation: u32,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    e: u64,
    f: u64,
) -> i64 {
    let opcode = match dispatch::admit(operation, &[a, b, c, d, e, f], usize::BITS) {
        Ok(opcode) => opcode,
        Err(error) => return error as i64,
    };
    // SAFETY: scalar admission above precedes every typed pointer handler.
    unsafe { dispatch_admitted(context, opcode, [a, b, c, d, e, f]) }
}

// Always inline this explicit group selector, preserving the exported boundary's
// existing tail branches while keeping admission separate from pointer dispatch.
#[inline(always)]
unsafe fn dispatch_admitted(context: *mut abi::Context, opcode: Opcode, slots: [u64; 6]) -> i64 {
    // SAFETY: admission prevents truncation; each typed handler enforces the
    // operation's structural pointer and context contract before effects.
    unsafe {
        match opcode {
            Opcode::AudioOpenBegin
            | Opcode::AudioOpenTake
            | Opcode::AudioClose
            | Opcode::AudioSampleRate
            | Opcode::AudioVolume
            | Opcode::AudioSetVolume
            | Opcode::SoundOpenPcm
            | Opcode::SoundClose
            | Opcode::SoundPlay
            | Opcode::SoundTell
            | Opcode::SoundSeek
            | Opcode::SoundSetGain
            | Opcode::SoundSetBalance
            | Opcode::SoundStop => audio::dispatch(context, opcode, slots),
            Opcode::BloblogAppendBegin
            | Opcode::BloblogAppendTake
            | Opcode::BloblogReadBegin
            | Opcode::BloblogReadTake => bloblog_call(context, opcode, slots),
            Opcode::FileOpenBegin
            | Opcode::FileOpenTake
            | Opcode::FileReadBegin
            | Opcode::FileReadTake
            | Opcode::FileWriteBegin
            | Opcode::FileWriteTake
            | Opcode::FileStatusBegin
            | Opcode::FileStatusTake
            | Opcode::FileCloseBegin
            | Opcode::FileCloseTake
            | Opcode::FileListOpenBegin
            | Opcode::FileListOpenTake
            | Opcode::FileListNextBegin
            | Opcode::FileListNextTake
            | Opcode::FileListClose
            | Opcode::FileDispose
            | Opcode::OperationDispose => files::dispatch(context, opcode, slots),
            _ => dispatch_non_file(context, opcode, slots),
        }
    }
}

#[inline(always)]
unsafe fn bloblog_call(context: *mut abi::Context, opcode: Opcode, slots: [u64; 6]) -> i64 {
    let [a, b, c, d, e, f] = slots;
    // SAFETY: scalar admission precedes these typed pointer handlers.
    unsafe {
        match opcode {
            Opcode::BloblogAppendBegin => bloblog::append_begin(
                context,
                data_abi::VolumeHandle(a),
                b as usize as _,
                c as usize as _,
                d as u32,
                e as u32,
                f as usize as _,
            ),
            Opcode::BloblogReadBegin => bloblog::read_begin(
                context,
                data_abi::VolumeHandle(a),
                b as usize as _,
                c as usize as _,
                d as u32,
            ),
            Opcode::BloblogAppendTake | Opcode::BloblogReadTake => bloblog::take(
                context,
                pending_abi::OperationHandle(a),
                opcode,
                b as usize as _,
                c as u32,
                d as usize as _,
            ),
            _ => unreachable!("bloblog opcode group"),
        }
    }
}

#[inline(always)]
unsafe fn dispatch_non_file(context: *mut abi::Context, opcode: Opcode, slots: [u64; 6]) -> i64 {
    let [a, b, c, d, e, f] = slots;
    // SAFETY: the admitted dispatcher preserves the original pointer contract.
    unsafe {
        match opcode {
            Opcode::Session | Opcode::ChannelOpen | Opcode::ChannelBegin | Opcode::ChannelTake => {
                channel_call(context, opcode, [a, b, c, d, e, f])
            }
            Opcode::KvKeysBegin
            | Opcode::KvKeysTake
            | Opcode::KvReadBegin
            | Opcode::KvReadTake
            | Opcode::KvPutBegin
            | Opcode::KvPutTake
            | Opcode::KvDeleteBegin
            | Opcode::KvDeleteTake => kv_call(context, opcode, [a, b, c, d, e, f]),
            Opcode::EntryResolveBegin
            | Opcode::EntryResolveTake
            | Opcode::SchemaReadBegin
            | Opcode::SchemaReadTake
            | Opcode::SchemaEstablishBegin
            | Opcode::SchemaEstablishTake
            | Opcode::ObjectReadBegin
            | Opcode::ObjectReadTake
            | Opcode::ObjectEditBegin
            | Opcode::ObjectEditTake
            | Opcode::ObjectQueryBegin
            | Opcode::ObjectQueryTake
            | Opcode::ObjectCreateBegin
            | Opcode::ObjectCreatePoll
            | Opcode::FileCopyBegin
            | Opcode::FileCopyPoll
            | Opcode::FileSelectBegin
            | Opcode::FileSelectPoll => pending_call(context, opcode, a, b, c, d, e, f),
            Opcode::ObjectFieldReadAuthenticatedBegin
            | Opcode::ObjectFieldReadAuthenticatedTake => {
                authenticated_call(context, opcode, a, b, c, d)
            }
            Opcode::AvailableVolumes => data::pb_volumes(context, a as usize as _),
            Opcode::HostPlatform
            | Opcode::HostLimit
            | Opcode::TraceRecord
            | Opcode::ClockRead
            | Opcode::RandomFill
            | Opcode::VolumesObserve
            | Opcode::QuitOnLastWindow
            | Opcode::Capabilities
            | Opcode::StartupArguments
            | Opcode::Presence
            | Opcode::Log
            | Opcode::ViewSubmit
            | Opcode::EventPoll
            | Opcode::EventPollDeadline
            | Opcode::Identity
            | Opcode::SessionInfo
            | Opcode::WakeAfter
            | Opcode::UpdateDefer => platform_call(context, opcode, a, b, c, d, e),
            _ => unreachable!("File opcodes were dispatched separately"),
        }
    }
}

#[inline(always)]
unsafe fn authenticated_call(
    context: *mut abi::Context,
    opcode: Opcode,
    a: u64,
    b: u64,
    c: u64,
    _d: u64,
) -> i64 {
    // SAFETY: pb_syscall admitted every scalar before selecting this typed group.
    unsafe {
        match opcode {
            Opcode::ObjectFieldReadAuthenticatedBegin => {
                authenticated::begin(context, a as usize as _, b as usize as _, c as usize as _)
            }
            Opcode::ObjectFieldReadAuthenticatedTake => {
                authenticated::take(context, pending_abi::OperationHandle(a), b as usize as _)
            }
            _ => unreachable!("authenticated opcode group"),
        }
    }
}

// Inline the groups even without LTO so the shared entry can tail-branch to the
// typed C-convention handlers. These helpers are private, never ABI symbols.
#[inline(always)]
#[allow(clippy::too_many_arguments, reason = "Direct lowering of the six scalar syscall slots.")]
unsafe fn pending_call(
    context: *mut abi::Context,
    opcode: Opcode,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    e: u64,
    _f: u64,
) -> i64 {
    let handle = pending_abi::OperationHandle(a);
    // SAFETY: only pb_syscall calls this group, after scalar admission.
    unsafe {
        match opcode {
            Opcode::EntryResolveBegin => data::pb_entry_resolve_begin(
                context,
                playbit_api::abi::data::VolumeHandle(a),
                b as usize as _,
                c as u32,
            ),
            Opcode::EntryResolveTake => {
                data::pb_entry_resolve_take(context, handle, b as usize as _)
            }
            Opcode::ObjectReadBegin => async_data::read_begin(
                context,
                a as usize as _,
                b as usize as _,
                c as usize as _,
                d as u32,
            ),
            Opcode::ObjectEditBegin => async_data::edit_begin(
                context,
                a as usize as _,
                b as usize as _,
                c as usize as _,
                d as u32,
                e as u32,
            ),
            Opcode::ObjectQueryBegin => {
                async_data::query_begin(context, a as usize as _, b as usize as _)
            }
            Opcode::ObjectReadTake => async_data::read_take(context, handle, b as usize as _),
            Opcode::ObjectEditTake => async_data::edit_take(context, handle),
            Opcode::ObjectQueryTake => async_data::query_take(context, handle, b as usize as _),
            Opcode::ObjectCreateBegin => pending::pb_object_create_begin(
                context,
                a as usize as _,
                b as usize as _,
                c as usize as _,
                d as u32,
                e as u32,
            ),
            Opcode::ObjectCreatePoll => {
                pending::pb_object_create_poll(context, handle, b as usize as _)
            }
            _ => file_pending_call(context, opcode, a, b, c, d, e),
        }
    }
}
#[inline(always)]
unsafe fn file_pending_call(
    context: *mut abi::Context,
    opcode: Opcode,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    _e: u64,
) -> i64 {
    let handle = pending_abi::OperationHandle(a);
    // SAFETY: pb_syscall admitted scalars before selecting this typed file/schema group.
    unsafe {
        match opcode {
            Opcode::SchemaReadBegin => schema::read_begin(context, a as usize as _),
            Opcode::SchemaReadTake => schema::read_take(context, handle, b as usize as _),
            Opcode::SchemaEstablishBegin => schema::establish_begin(
                context,
                a as usize as _,
                b as usize as _,
                c as u32,
                d as u32,
            ),
            Opcode::SchemaEstablishTake => schema::establish_take(context, handle),
            Opcode::FileCopyBegin => {
                pending::pb_file_copy_begin(context, a as usize as _, b as usize as _)
            }
            Opcode::FileCopyPoll => pending::pb_file_copy_poll(context, handle, b as usize as _),
            Opcode::FileSelectBegin => pending::pb_file_select_begin(context),
            Opcode::FileSelectPoll => {
                pending::pb_file_select_poll(context, handle, b as usize as _)
            }
            _ => unreachable!("file/schema pending opcode group"),
        }
    }
}
#[inline(always)]
unsafe fn platform_call(
    context: *mut abi::Context,
    opcode: Opcode,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    _e: u64,
) -> i64 {
    // SAFETY: only pb_syscall calls this group, after scalar admission.
    unsafe {
        match opcode {
            Opcode::HostPlatform => environment::pb_host_platform(context),
            Opcode::HostLimit => environment::pb_host_limit(context, a as u32),
            Opcode::TraceRecord => calls::pb_trace_record(context, a as usize as _, b as u32, c, d),
            Opcode::ClockRead => crate::trace::clock_ns() as i64,
            Opcode::RandomFill => environment::pb_random_fill(context, a as usize as _, b as u32),
            Opcode::VolumesObserve => calls::with_context(context, |state| {
                crate::runtime::syscall::control::observe_volumes(state, a).map(|()| 0)
            }),
            Opcode::QuitOnLastWindow => calls::pb_quit_on_last_window(context, a as u32),
            Opcode::Capabilities => environment::pb_capabilities(context, a as usize as _),
            Opcode::StartupArguments => environment::pb_startup_arguments(context, a as usize as _),
            Opcode::Presence => environment::pb_presence(
                context,
                a as usize as _,
                b as u32,
                c as u32,
                d as usize as _,
            ),

            Opcode::Log => calls::pb_log(context, a as usize as _, b as u32),
            Opcode::ViewSubmit => calls::pb_view_submit(context, a as usize as _, b as u32),
            Opcode::EventPoll => {
                calls::pb_event_poll(context, a as usize as _, b as usize as _, c as u32)
            }
            Opcode::EventPollDeadline => {
                super::deadline::poll(context, a as usize as _, b as usize as _, c as u32, d)
            }
            Opcode::Identity => calls::pb_identity(context, a as usize as _),
            Opcode::SessionInfo => calls::pb_session_info(context, a as usize as _),
            Opcode::WakeAfter => calls::pb_wake_after(context, a as u32),
            Opcode::UpdateDefer => calls::pb_update_defer(context, a),
            _ => unreachable!("platform opcode group"),
        }
    }
}

#[inline(always)]
unsafe fn kv_call(context: *mut abi::Context, opcode: Opcode, args: [u64; 6]) -> i64 {
    let [a, b, c, d, e, _f] = args;
    let handle = pending_abi::OperationHandle(a);
    // SAFETY: the scalar dispatcher admitted the complete selected KV layout.
    unsafe {
        match opcode {
            Opcode::KvKeysBegin => {
                kv::keys_begin(context, a as usize as _, b as usize as _, c as usize as _, d as u32)
            }
            Opcode::KvReadBegin => {
                kv::read_begin(context, a as usize as _, b as usize as _, c as u32, d as usize as _)
            }
            Opcode::KvPutBegin => kv::put_begin(
                context,
                a as usize as _,
                b as usize as _,
                c as u32,
                d as usize as _,
                e as u32,
            ),
            Opcode::KvDeleteBegin => {
                kv::delete_begin(context, a as usize as _, b as usize as _, c as u32)
            }
            Opcode::KvKeysTake | Opcode::KvReadTake => {
                kv::bytes_take(context, handle, opcode, b as usize as _, c as u32, d as usize as _)
            }
            Opcode::KvPutTake | Opcode::KvDeleteTake => kv::receipt_take(context, handle, opcode),
            _ => unreachable!("KV opcode group"),
        }
    }
}

/// # Safety
/// Inputs and disjoint writable outputs obey the native borrowed-call contract.
unsafe fn channel_call(context: *mut abi::Context, opcode: Opcode, slots: [u64; 6]) -> i64 {
    use super::calls::{bytes, span, with_context};
    use crate::runtime::syscall::{OperationHandle, channels};
    let [a, b, c, d, _e, _] = slots;
    // SAFETY: spans are admitted before reading inputs or writing results.
    unsafe {
        with_context(context, |state| match opcode {
            Opcode::Session => Ok(i64::from(channels::session(state)?)),
            Opcode::ChannelOpen => {
                span(a as usize as *const u8, b as usize)?;
                let name = std::str::from_utf8(bytes(a as usize as *const u8, b as u32, 223)?)
                    .map_err(|_| HostError::InvalidArgument)?;
                let handle = channels::open(state, name)?;
                Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
            }
            Opcode::ChannelBegin => {
                span(c as usize as *const u8, d as usize)?;
                let data = bytes(c as usize as *const u8, d as u32, 32 * 1024)?.to_vec();
                let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
                let handle =
                    channels::begin(state, OperationHandle { generation, slot }, b as u32, data)?;
                Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
            }
            Opcode::ChannelTake => {
                let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
                channels::take(state, OperationHandle { generation, slot }).map(|_| 0)
            }
            _ => Err(HostError::Unsupported),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_registered_operation_obeys_its_null_context_contract() {
        // Exercise every admitted opcode with a null context to catch misrouted groups.
        for &(opcode, _) in playbit_api::abi::syscall::REGISTRY {
            let a = if dispatch::layout(opcode).handle { dispatch::pack_handle(1, 1) } else { 0 };
            assert_eq!(
                dispatch::admit(opcode as u32, &[a, 0, 0, 0, 0, 0], usize::BITS),
                Ok(opcode)
            );
            let before = crate::trace::clock_ns();
            let result =
                unsafe { pb_syscall(core::ptr::null_mut(), opcode as u32, a, 0, 0, 0, 0, 0) };
            if opcode == Opcode::ClockRead {
                // Clock reads always succeed and need no guest state.
                assert!((before..=crate::trace::clock_ns()).contains(&(result as u64)));
            } else {
                // Every stateful handler rejects the null context before
                // touching any argument pointer.
                assert_eq!(result, HostError::InvalidArgument as i64, "{opcode:?}");
            }
        }
    }
    #[test]
    fn unknown_opcodes_and_noncanonical_slots_fail_before_context_access() {
        // Deliberately invalid pointers establish that scalar admission precedes
        // any host state or caller memory access, including on newer app calls.
        let context = std::ptr::dangling_mut::<abi::Context>();
        unsafe {
            assert_eq!(
                pb_syscall(context, u32::MAX, u64::MAX, 0, 0, 0, 0, 0),
                HostError::Unsupported as i64
            );
            assert_eq!(
                pb_syscall(context, Opcode::WakeAfter as u32, 1 << 32, 0, 0, 0, 0, 0),
                HostError::InvalidArgument as i64
            );
            assert_eq!(
                pb_syscall(context, Opcode::HostPlatform as u32, 0, 0, 0, 0, 0, 1),
                HostError::InvalidArgument as i64
            );
        }
    }
}
