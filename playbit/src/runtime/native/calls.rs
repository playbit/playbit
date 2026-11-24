//! Native pointers are validated structurally here; allocation validity and
//! lifetime are the caller's C contract. No native pointer becomes a Wasm offset.
use super::{Context, Ordering, State, abi};
use crate::runtime::syscall::{HostError, HostResult, control, environment, events};
use playbit_api::abi::syscall as contract;
use std::{mem::align_of, sync::TryLockError};

pub(super) unsafe fn with_context<T: Into<i64>>(
    context: *mut abi::Context,
    call: impl FnOnce(&mut State) -> HostResult<T>,
) -> i64 {
    if context.is_null() || !(context as usize).is_multiple_of(align_of::<Context>()) {
        return HostError::InvalidArgument as i64;
    }
    // SAFETY: callers must borrow a live context issued by NativeInstance.
    let context = unsafe { &*context.cast::<Context>() };
    if context.owner != std::thread::current().id() || !context.active.load(Ordering::Acquire) {
        return HostError::Conflict as i64;
    }
    let mut state = match context.state.try_lock() {
        Ok(state) => state,
        Err(TryLockError::WouldBlock) => return HostError::Conflict as i64,
        Err(TryLockError::Poisoned(_)) => panic!("native instance state is poisoned"),
    };
    state.assert_invariants();
    let result = call(&mut state);
    state.assert_invariants();
    result.map(Into::into).unwrap_or_else(|error| error as i64)
}

pub(super) fn span<T>(pointer: *const T, count: usize) -> HostResult<std::ops::Range<usize>> {
    super::memory::span_at::<T>(pointer as usize, count)
}
pub(super) unsafe fn bytes<'a>(
    pointer: *const u8,
    length: u32,
    limit: usize,
) -> HostResult<&'a [u8]> {
    if length as usize > limit {
        return Err(HostError::LimitExceeded);
    }
    span(pointer, length as usize)?;
    if length == 0 {
        return Ok(&[]);
    }
    // SAFETY: the caller promises a readable stable allocation for the call.
    Ok(unsafe { std::slice::from_raw_parts(pointer, length as usize) })
}

/// # Safety
/// Context and input must obey the borrowing contract in playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_log(
    context: *mut abi::Context,
    text: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: raw inputs are borrowed only for this call under the C contract.
    unsafe {
        with_context(context, |state| {
            let text = bytes(text, length, u32::MAX as usize)?;
            let text = std::str::from_utf8(text).map_err(|_| HostError::InvalidArgument)?;
            control::log(state, text).map(|()| 0)
        })
    }
}

/// # Safety
/// Context and scene must obey the borrowing contract in playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_view_submit(
    context: *mut abi::Context,
    scene: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: the scene is stable and readable for the call, never retained.
    unsafe {
        with_context(context, |state| {
            let scene = bytes(scene, length, state.view_bytes_limit as usize)?;
            control::view_submit(state, scene).map(|()| 0)
        })
    }
}
/// # Safety
/// Context and disjoint writable outputs obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_event_poll(
    context: *mut abi::Context,
    event: *mut abi::Event,
    data: *mut u8,
    capacity: u32,
) -> i64 {
    // SAFETY: all structural output admission precedes event consumption. The
    // caller guarantees actual allocation validity; no output references escape.
    unsafe {
        with_context(context, |state| {
            if capacity as usize > contract::MAX_EVENT_BYTES {
                return Err(HostError::LimitExceeded);
            }
            let header = super::record::region(event, true)?;
            let payload = span(data, capacity as usize)?;
            if capacity != 0 && header.start < payload.end && payload.start < header.end {
                return Err(HostError::InvalidArgument);
            }
            playbit_api::abi::record::clear_native(event)?;
            #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
            crate::runtime::syscall::audio::pump(state);
            let length = events::peek_event(state)?.data.len();
            if length > capacity as usize {
                return Ok(length as i32);
            }
            let ready = events::take_event(state)?;
            if length != 0 {
                std::ptr::copy_nonoverlapping(ready.data.as_ptr(), data, length);
            }
            super::record::write(
                event,
                abi::Event {
                    generation: u64::from(state.generation),
                    kind: ready.kind as u32,
                    target: ready.target,
                    ..abi::Event::default()
                },
            )?;
            Ok(length as i32)
        })
    }
}
/// # Safety
/// Context and writable identity obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_identity(
    context: *mut abi::Context,
    identity: *mut abi::Identity,
) -> i64 {
    // SAFETY: the caller owns the output; all validation precedes its write.
    unsafe {
        with_context(context, |state| {
            super::record::region(identity, true)?;
            playbit_api::abi::record::clear_native(identity)?;
            let (generation, value) = environment::identity(state)?;
            super::record::write(
                identity,
                abi::Identity {
                    generation,
                    account_id: value.account_id,
                    device_id: value.device_id,
                    instance_id: value.instance_id,
                    session_id: value.session_id.unwrap_or([0; 32]),
                    flags: u32::from(value.is_service)
                        | (u32::from(value.session_id.is_some()) << 1),
                    reserved: 0,
                    display_name: environment::display_name_field(&value.display_name),
                    ..abi::Identity::default()
                },
            )?;
            Ok(0)
        })
    }
}
/// # Safety
/// Context and writable session info obey playbit/sys/abi.h.
pub(super) unsafe fn pb_session_info(
    context: *mut abi::Context,
    output: *mut playbit_api::abi::session::SessionInfo,
) -> i64 {
    // SAFETY: the native caller lends an initialized, writable output record.
    unsafe {
        with_context(context, |state| {
            super::record::region(output, true)?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::write(output, environment::session_info(state)?)?;
            Ok(0)
        })
    }
}
/// # Safety
/// Context must be a live callback borrow as specified in playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_wake_after(context: *mut abi::Context, delay_ms: u32) -> i64 {
    // SAFETY: this call borrows only the context, with no caller buffers.
    unsafe {
        with_context(context, |state| {
            control::wake_after(state, delay_ms, std::time::Instant::now()).map(|()| 0)
        })
    }
}
/// # Safety
/// Context must be a live callback borrow as specified in playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_update_defer(
    context: *mut abi::Context,
    generation: u64,
) -> i64 {
    // SAFETY: this call borrows only the context, with no caller buffers.
    unsafe { with_context(context, |state| control::update_defer(state, generation).map(|()| 0)) }
}

/// # Safety
/// The caller lends the complete packed input until return.
pub(super) unsafe fn pb_trace_record(
    context: *mut abi::Context,
    pointer: *const u8,
    length: u32,
    generation: u64,
    lost: u64,
) -> i64 {
    // SAFETY: scalar admission precedes bounded input admission; no pointer is retained.
    unsafe {
        with_context(context, |state| {
            let input = bytes(pointer, length, playbit_api::abi::trace::MAX_BATCH_BYTES)?;
            crate::runtime::syscall::tracing::record(state, input, generation, lost).map(|()| 0)
        })
    }
}

/// # Safety
/// Context must be a live callback borrow as specified in playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_quit_on_last_window(
    context: *mut abi::Context,
    intent: u32,
) -> i64 {
    // SAFETY: the local scalar choice borrows only the live instance context.
    unsafe {
        with_context(context, |state| control::quit_on_last_window(state, intent).map(|()| 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_random_fill_uses_the_sdk_and_checks_borrows_and_output_bounds() {
        // Use the real OS source through the safe SDK and raw native dispatcher.
        let (state, _, _) = crate::runtime::syscall::pending_data::tests::state();
        let context = super::super::Context {
            state: std::sync::Mutex::new(state),
            owner: std::thread::current().id(),
            active: std::sync::atomic::AtomicBool::new(true),
            poll_yielder: Default::default(),
        };
        let raw = (&context as *const super::super::Context).cast_mut().cast();
        let mut sdk_state = playbit_api::context::ContextState::default();
        // SAFETY: context and all buffers remain on the active owner thread.
        unsafe {
            let (mut sdk, _) = playbit_api::context::Context::from_raw(raw, &mut sdk_state)
                .expect("valid native context");
            let mut bytes = vec![42; contract::MAX_RANDOM_BYTES];
            sdk.random_fill(&mut bytes).expect("OS entropy");
            assert!(bytes.iter().any(|&byte| byte != 42));
            sdk.random_fill(&mut []).expect("empty output");
            bytes.push(42);
            let previous = bytes.clone();
            assert_eq!(sdk.random_fill(&mut bytes), Err(HostError::LimitExceeded));
            assert_eq!(bytes, previous);
            assert_eq!(
                playbit_api::abi::operations::pb_random_fill(raw, std::ptr::null_mut(), 0),
                0
            );
            assert_eq!(
                playbit_api::abi::operations::pb_random_fill(raw, std::ptr::null_mut(), 1),
                HostError::InvalidArgument as i64
            );
            assert_eq!(
                playbit_api::abi::operations::pb_random_fill(
                    raw,
                    bytes.as_mut_ptr(),
                    bytes.len() as u32
                ),
                HostError::LimitExceeded as i64
            );
            assert_eq!(bytes, previous);
            context.active.store(false, Ordering::Release);
            assert_eq!(
                playbit_api::abi::operations::pb_random_fill(raw, bytes.as_mut_ptr(), 32),
                HostError::Conflict as i64
            );
            assert_eq!(bytes, previous);
        }
    }

    #[test]
    fn native_sdk_logs_preserve_bursts_and_large_utf8_messages() {
        // Exercise the SDK and native pointer boundary within one active turn.
        let (state, _, _) = crate::runtime::syscall::pending_data::tests::state();
        let context = super::super::Context {
            state: std::sync::Mutex::new(state),
            owner: std::thread::current().id(),
            active: std::sync::atomic::AtomicBool::new(true),
            poll_yielder: Default::default(),
        };
        let raw = (&context as *const super::super::Context).cast_mut().cast();
        let mut sdk_state = playbit_api::context::ContextState::default();
        // SAFETY: the context remains active on its owner thread for this test.
        let (mut sdk, _) = unsafe { playbit_api::context::Context::from_raw(raw, &mut sdk_state) }
            .expect("valid native context");
        let message = "🌱".repeat(24 * 1024);
        let mut expected = Vec::new();
        for text in [&message, &message] {
            sdk.log(text);
            expected.push(text.clone());
        }
        for index in 0..1000 {
            let message = index.to_string();
            sdk.log(&message);
            expected.push(message);
        }
        // SAFETY: invalid UTF-8 has readable storage; a null nonempty span is
        // rejected structurally before any pointer can be dereferenced.
        unsafe {
            assert_eq!(pb_log(raw, [255].as_ptr(), 1), HostError::InvalidArgument as i64);
            assert_eq!(pb_log(raw, std::ptr::null(), 1), HostError::InvalidArgument as i64);
        }
        let frame = context.state.lock().expect("context lock").finish_frame();
        assert_eq!(frame.logs(), expected);
    }
}
