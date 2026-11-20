//! Trusted C local-engine lifecycle. No store Arc or mutable reference escapes.
//! One owner thread serializes operations; reentry is rejected before state access.
#![cfg_attr(
    all(target_os = "macos", feature = "macos-shared-runtime"),
    allow(
        dead_code,
        reason = "The shared GUI omits separate SDK/provider C exports; their unused Rust support is removed by the linker."
    )
)]
#![allow(unsafe_code, reason = "C management handles and callbacks follow playbit/engine.h.")]
use super::Services;
use crate::store::{Id, change_feed::CommittedChangeReceiver};
use playbit_api::abi::syscall::Error;
use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    thread::ThreadId,
};
#[cfg(feature = "native-runtime")]
mod apps;
mod changes;
#[cfg(feature = "native-runtime")]
pub use apps::{AppOutput, AppStep};
pub(crate) mod memory;
mod volumes;
pub use changes::{Change, Changes};
use memory::{disjoint, fixed, input};
pub use volumes::Volume;
type Result<T> = std::result::Result<T, Error>;
pub type Wake = unsafe extern "C" fn(*mut c_void);

/// Opaque C owner. Construct and destroy only through the exported functions.
pub struct Engine {
    owner: ThreadId,
    inner: Mutex<Inner>,
    wake: Arc<Notification>,
}
struct Inner {
    #[cfg(feature = "native-runtime")]
    apps: super::native::Apps,
    services: Services,
    changes: CommittedChangeReceiver,
}
struct Notification {
    callback: Wake,
    data: usize,
    enabled: AtomicBool,
}
impl Notification {
    fn signal(&self) {
        if self.enabled.load(Ordering::Acquire) {
            // SAFETY: C retains callback/data through close; no store/worker escapes.
            unsafe { (self.callback)(self.data as *mut c_void) }
        }
    }
}
pub(crate) fn store_error(error: crate::store::Error) -> Error {
    use crate::store::Error as E;
    match error {
        E::AccessDenied(_) => Error::AccessDenied,
        E::NotFound(_) => Error::NotFound,
        E::AlreadyExists(_) | E::LocalContention(_) => Error::Conflict,
        E::InvalidInput(_) => Error::InvalidArgument,
        E::LimitExceeded => Error::LimitExceeded,
        _ => Error::Internal,
    }
}
unsafe fn with_engine(pointer: *mut Engine, call: impl FnOnce(&mut Inner) -> Result<()>) -> i32 {
    let run = || {
        fixed(pointer)?;
        // SAFETY: caller lends a live handle and synchronizes its destruction.
        let engine = unsafe { &*pointer };
        if engine.owner != std::thread::current().id() {
            return Err(Error::Conflict);
        }
        let mut inner = match engine.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::WouldBlock) => return Err(Error::Conflict),
            Err(TryLockError::Poisoned(_)) => panic!("C engine state is poisoned"),
        };
        call(&mut inner)
    };
    run().map_or_else(|error| error as i32, |()| 0)
}

/// # Safety
/// Pointers, callback and lifetime must obey playbit/engine.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_open(
    path: *const u8,
    path_length: u32,
    wake: Option<Wake>,
    data: *mut c_void,
    output: *mut *mut Engine,
) -> i32 {
    let run = || {
        let destination = fixed(output)?;
        let source = input(path, path_length, 4096)?;
        disjoint(&destination, &source)?;
        let callback = wake.ok_or(Error::InvalidArgument)?;
        // SAFETY: input was admitted, C promises stable readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(path, path_length as usize) };
        let path = std::str::from_utf8(bytes).map_err(|_| Error::InvalidArgument)?;
        if path.is_empty() || path.contains('\0') || !std::path::Path::new(path).is_absolute() {
            return Err(Error::InvalidArgument);
        }
        let wake = Arc::new(Notification {
            callback,
            data: data as usize,
            enabled: AtomicBool::new(true),
        });
        let notify = wake.clone();
        let services = Services::open(path, move || notify.signal()).map_err(store_error)?;
        let changes =
            services.store.take_change_receiver().expect("new services have one change receiver");
        assert!(changes.readiness().is_none(), "C embedding uses host readiness");
        let engine = Box::new(Engine {
            owner: std::thread::current().id(),
            inner: Mutex::new(Inner {
                #[cfg(feature = "native-runtime")]
                apps: {
                    let notify = wake.clone();
                    super::native::Apps::new(Arc::new(move || notify.signal()))
                },
                services,
                changes,
            }),
            wake,
        });
        // SAFETY: admitted disjoint output; ownership transfers after successful open.
        unsafe { output.write(Box::into_raw(engine)) };
        Ok(())
    };
    run().map_or_else(|error| error as i32, |()| 0)
}

/// # Safety
/// `pointer` owns a live engine handle and obeys playbit/engine.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_close(pointer: *mut *mut Engine) -> i32 {
    let run = || {
        let slot = fixed(pointer)?;
        // SAFETY: C lends an initialized writable handle slot.
        let raw = unsafe { pointer.read() };
        let handle = fixed(raw)?;
        disjoint(&slot, &handle)?;
        // SAFETY: live handle, lifetime synchronized by the caller.
        let engine = unsafe { &*raw };
        if engine.owner != std::thread::current().id() {
            return Err(Error::Conflict);
        }
        let guard = match engine.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::WouldBlock) => return Err(Error::Conflict),
            Err(TryLockError::Poisoned(_)) => panic!("C engine state is poisoned"),
        };
        engine.wake.enabled.store(false, Ordering::Release);
        // Destroy foreign app state while reentry is still excluded by the lock.
        #[cfg(feature = "native-runtime")]
        let guard = {
            let mut guard = guard;
            guard.apps.clear();
            guard
        };
        drop(guard);
        // SAFETY: owner is idle, no Arc/worker escapes, and callback delivery is retired.
        unsafe {
            drop(Box::from_raw(raw));
            pointer.write(std::ptr::null_mut());
        }
        Ok(())
    };
    run().map_or_else(|error| error as i32, |()| 0)
}

/// # Safety
/// Live handle and writable output obey playbit/engine.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_account(engine: *mut Engine, output: *mut [u8; 16]) -> i32 {
    // SAFETY: the live engine and output are lent for this synchronous call.
    unsafe {
        with_engine(engine, |inner| {
            fixed(output)?;
            output.write(*inner.services.store.account().id.as_bytes());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
