//! Operation-oriented C layouts. Native pointers are never Wasm offsets.
//! The native adapter owns Context; applications borrow it only during callbacks.
#![allow(
    unsafe_code,
    reason = "C declarations and opaque pointers define the ABI; invocation lives in the SDK and runtime adapters."
)]
use core::ffi::c_void;

#[repr(C)]
pub struct Context {
    _opaque: [u8; 0],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Event {
    pub size: u32,
    pub written: u32,
    pub generation: u64,
    pub kind: u32,
    pub target: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Identity {
    pub size: u32,
    pub written: u32,
    pub generation: u64,
    pub account_id: [u8; 16],
    pub device_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub session_id: [u8; 32],
    pub flags: u32,
    pub reserved: u32,
}

pub type Init = unsafe extern "C" fn(*mut Context, *mut c_void, *mut *mut c_void) -> i32;
pub type Turn = unsafe extern "C" fn(*mut Context, *mut c_void) -> i32;
pub type Destroy = unsafe extern "C" fn(*mut c_void);

#[derive(Clone, Copy)]
#[repr(C)]
pub struct App {
    pub size: u32,
    pub written: u32,
    pub init: Option<Init>,
    pub turn: Option<Turn>,
    pub destroy: Option<Destroy>,
    pub user_data: *mut c_void,
}

/// Declare one immutable Boolean intent; this is local metadata, not I/O.
/// # Safety
/// Context obeys the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed scalar lowering borrows the live callback context.")]
pub unsafe fn pb_quit_on_last_window(context: *mut Context, intent: u32) -> i64 {
    // SAFETY: only the borrowed context and canonical scalar slots are forwarded.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::QuitOnLastWindow as u32,
            [intent as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_log(context: *mut Context, text: *const u8, length: u32) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::Log as u32,
            [text as usize as u64, length as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_view_submit(context: *mut Context, scene: *const u8, length: u32) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ViewSubmit as u32,
            [scene as usize as u64, length as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_event_poll(
    context: *mut Context,
    event: *mut Event,
    data: *mut u8,
    capacity: u32,
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::EventPoll as u32,
            [event as usize as u64, data as usize as u64, capacity as u64, 0, 0, 0],
        )
    }
}

/// Poll one event, waiting until the absolute monotonic deadline in nanoseconds.
/// Zero polls immediately and `u64::MAX` waits indefinitely. An empty queue at
/// the deadline returns `WouldBlock`; a queued event takes precedence.
/// # Safety
/// Context and both exclusive output spans must remain valid until this call returns.
#[inline]
pub unsafe fn pb_event_poll_deadline(
    context: *mut Context,
    event: *mut Event,
    data: *mut u8,
    capacity: u32,
    deadline_ns: u64,
) -> i64 {
    // SAFETY: the caller lends disjoint, valid outputs for the entire poll.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::EventPollDeadline as u32,
            [event as usize as u64, data as usize as u64, capacity as u64, deadline_ns, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_identity(context: *mut Context, identity: *mut Identity) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::Identity as u32,
            [identity as usize as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_wake_after(context: *mut Context, delay_ms: u32) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::WakeAfter as u32,
            [delay_ms as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_update_defer(context: *mut Context, generation: u64) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::UpdateDefer as u32,
            [generation, 0, 0, 0, 0, 0],
        )
    }
}

/// Submit packed trace events with their capture generation and cumulative loss.
/// The host consumes all bytes before return.
/// # Safety
/// Context and bytes obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
pub unsafe fn pb_trace_record(
    context: *mut Context,
    bytes: *const u8,
    length: u32,
    generation: u64,
    lost: u64,
) -> i64 {
    // SAFETY: the caller lends the complete input buffer for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::TraceRecord as u32,
            [bytes as usize as u64, length as u64, generation, lost, 0, 0],
        )
    }
}
/// Read monotonic nanoseconds. This operation always succeeds.
/// # Safety
/// Context must be live for this call.
#[inline]
pub unsafe fn pb_clock_read(context: *mut Context) -> u64 {
    // SAFETY: the caller supplies a live context; this operation borrows no memory.
    unsafe {
        super::invocation::invoke(context, super::syscall::Opcode::ClockRead as u32, [0; 6]) as u64
    }
}

/// Fill at most MAX_RANDOM_BYTES with cryptographically secure platform randomness.
/// Returns zero only after filling the entire buffer. On entropy failure the
/// admitted output is zeroed and Unavailable is returned; never use it on error.
/// # Safety
/// Context and writable output obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
pub unsafe fn pb_random_fill(context: *mut Context, bytes: *mut u8, length: u32) -> i64 {
    // SAFETY: the caller lends the complete output exclusively until return.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::RandomFill as u32,
            [bytes as usize as u64, length as u64, 0, 0, 0, 0],
        )
    }
}

impl Default for Event {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            generation: 0,
            kind: 0,
            target: 0,
        }
    }
}

super::record::record!(Event, 24, { generation: u64, kind: u32, target: u32 });

impl Default for Identity {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            generation: 0,
            account_id: [0; 16],
            device_id: [0; 16],
            instance_id: [0; 16],
            session_id: [0; 32],
            flags: 0,
            reserved: 0,
        }
    }
}

super::record::record!(Identity, 104, { generation: u64, account_id: [u8; 16], device_id: [u8; 16], instance_id: [u8; 16], session_id: [u8; 32], flags: u32, reserved: u32 });

super::record::record!(App, if cfg!(target_pointer_width = "64") { 40 } else { 24 }, { init: Option<Init>, turn: Option<Turn>, destroy: Option<Destroy>, user_data: *mut c_void });
