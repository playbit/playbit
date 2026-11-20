//! Example-owned C bridge to typed engine services; this is not the app ABI.
//! AppKit owns both profiles on its main thread. Every pointer is private to
//! that host, and the UI disables queued callbacks before close. Errors are
//! reported explicitly; extern C panics abort instead of unwinding into AppKit.
use std::{
    ffi::{CStr, c_char, c_void},
    path::Path,
};
// Reuse the independently tested fixture and its exact store/notification rules.
// Its CLI entry points are unused in this library composition.
#[allow(dead_code)]
#[path = "../../../tests/embedding_host.rs"]
mod fixture;
use fixture::Host;

/// # Safety
/// path must be a live NUL-terminated string. wake must remain callable through
/// close, only schedule later owner-thread work, and never unwind or reenter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fixture_open(path: *const c_char, wake: extern "C" fn()) -> *mut c_void {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the example passes a live NUL-terminated argv-derived path.
    let Ok(path) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        return std::ptr::null_mut();
    };
    match Host::open_with_wake(Path::new(path), move || wake()) {
        Ok(host) => Box::into_raw(Box::new(host)).cast(),
        Err(error) => {
            eprintln!("open: {error}");
            std::ptr::null_mut()
        }
    }
}
/// # Safety
/// host must be the live, unaliased owner-thread handle returned by open.
/// No close or other fixture call may run concurrently or reentrantly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fixture_toggle(host: *mut c_void) -> i32 {
    // SAFETY: the example retains this owner-thread handle until close.
    match unsafe { &*host.cast::<Host>() }.toggle() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("edit: {error}");
            -1
        }
    }
}
/// # Safety
/// host must be a live owner-thread handle, with no concurrent close or call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fixture_value(host: *mut c_void) -> i32 {
    // SAFETY: same live owner-thread handle contract as fixture_toggle.
    match unsafe { &*host.cast::<Host>() }.value() {
        Ok(value) => i32::from(value),
        Err(error) => {
            eprintln!("read: {error}");
            -1
        }
    }
}
/// # Safety
/// host must be a live, exclusively borrowed owner-thread handle. The wake
/// callback must only schedule later work and cannot reenter this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fixture_drain(host: *mut c_void) -> i32 {
    // SAFETY: callbacks only enqueue this later turn; there is no reentrancy.
    match unsafe { &mut *host.cast::<Host>() }.advance() {
        Ok(count) => i32::try_from(count).expect("bounded change feed"),
        Err(error) => {
            eprintln!("changes: {error}");
            -1
        }
    }
}
/// # Safety
/// host must be null or the unique live owner-thread handle. Disable queued
/// users first and discard the pointer after this call; it cannot be reused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fixture_close(host: *mut c_void) {
    if !host.is_null() {
        // SAFETY: consume the unique handle; the caller immediately clears it.
        drop(unsafe { Box::from_raw(host.cast::<Host>()) });
    }
}
