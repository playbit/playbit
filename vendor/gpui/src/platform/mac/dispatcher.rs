#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use crate::{PlatformDispatcher, TaskLabel};
use async_task::Runnable;
use block::{Block, ConcreteBlock};
use core_foundation::{
    base::CFTypeRef,
    runloop::{CFRunLoopGetMain, CFRunLoopRef, CFRunLoopWakeUp, kCFRunLoopCommonModes},
};
use objc::{
    class, msg_send,
    runtime::{BOOL, YES},
    sel, sel_impl,
};
use std::{
    cell::Cell,
    ffi::c_void,
    ptr::{NonNull, addr_of},
    time::Duration,
};

/// All items in the generated file are marked as pub, so we're gonna wrap it in a separate mod to prevent
/// these pub items from leaking into public API.
pub(crate) mod dispatch_sys {
    include!(concat!(env!("OUT_DIR"), "/dispatch_sys.rs"));
}

use dispatch_sys::*;
pub(crate) fn dispatch_get_main_queue() -> dispatch_queue_t {
    addr_of!(_dispatch_main_q) as *const _ as dispatch_queue_t
}

pub(crate) struct MacDispatcher;

unsafe extern "C" {
    fn CFRunLoopPerformBlock(run_loop: CFRunLoopRef, mode: CFTypeRef, block: *const c_void);
}

/// AppKit's deferred termination runs a nested event loop. Main-queue GCD
/// callbacks cannot run while a main-queue callback is awaiting that loop.
/// Run-loop blocks remain dispatchable there, including a guest's drain reply.
pub(crate) fn dispatch_on_main_loop(callback: impl FnOnce() + Send + 'static) {
    let callback = Cell::new(Some(callback));
    let block = ConcreteBlock::new(move || {
        if let Some(callback) = callback.take() {
            callback();
        }
    })
    .copy();
    // Core Foundation copies the block and invokes it once on the main loop.
    unsafe {
        let run_loop = CFRunLoopGetMain();
        CFRunLoopPerformBlock(
            run_loop,
            kCFRunLoopCommonModes.cast(),
            (&*block as *const Block<(), ()>).cast(),
        );
        CFRunLoopWakeUp(run_loop);
    }
}

impl PlatformDispatcher for MacDispatcher {
    fn is_main_thread(&self) -> bool {
        let is_main_thread: BOOL = unsafe { msg_send![class!(NSThread), isMainThread] };
        is_main_thread == YES
    }

    fn dispatch(&self, runnable: Runnable, _: Option<TaskLabel>) {
        unsafe {
            dispatch_async_f(
                dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH.try_into().unwrap(), 0),
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline),
            );
        }
    }

    fn dispatch_on_main_thread(&self, runnable: Runnable) {
        dispatch_on_main_loop(move || {
            runnable.run();
        });
    }

    fn dispatch_after(&self, duration: Duration, runnable: Runnable) {
        unsafe {
            let queue =
                dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH.try_into().unwrap(), 0);
            let when = dispatch_time(DISPATCH_TIME_NOW as u64, duration.as_nanos() as i64);
            dispatch_after_f(
                when,
                queue,
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline),
            );
        }
    }
}

extern "C" fn trampoline(runnable: *mut c_void) {
    let task = unsafe { Runnable::<()>::from_raw(NonNull::new_unchecked(runnable as *mut ())) };
    task.run();
}
