//! Wall-clock scheduling and process-to-process readiness notifications.
#![allow(
    unsafe_code,
    reason = "Owned notification observers stay on main; GCD owns one timer callback."
)]
use async_channel::Sender;
use cocoa::foundation::NSString;
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
use std::{ffi::c_void, time::Duration};

const READY: &str = "playbit.engine.updateReady";

pub struct Observer(*mut Object);
impl Observer {
    pub fn new(sender: Sender<()>) -> Self {
        let handler = block::ConcreteBlock::new(move |_: *mut Object| {
            let _ = sender.try_send(());
        })
        .copy();
        unsafe {
            let center: *mut Object =
                msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
            let name = NSString::alloc(std::ptr::null_mut()).init_str(READY);
            let observer: *mut Object = msg_send![center, addObserverForName: name object: std::ptr::null_mut::<Object>() queue: std::ptr::null_mut::<Object>() usingBlock: handler];
            let _: () = msg_send![name, release];
            Self(observer)
        }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        unsafe {
            let center: *mut Object =
                msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
            let _: () = msg_send![center, removeObserver: self.0];
        }
    }
}

pub fn prepared() {
    unsafe {
        let center: *mut Object = msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
        let name = NSString::alloc(std::ptr::null_mut()).init_str(READY);
        let _: () = msg_send![center, postNotificationName: name object: std::ptr::null_mut::<Object>() userInfo: std::ptr::null_mut::<Object>() deliverImmediately: true];
        let _: () = msg_send![name, release];
    }
}

unsafe extern "C" {
    fn dispatch_walltime(when: *const libc::timespec, delta: i64) -> u64;
    fn dispatch_get_global_queue(identifier: libc::c_long, flags: libc::c_ulong) -> *mut c_void;
    fn dispatch_after_f(
        when: u64,
        queue: *mut c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}
extern "C" fn elapsed(context: *mut c_void) {
    // SAFETY: after_f calls this exactly once with the box transferred below.
    let sender = unsafe { Box::from_raw(context.cast::<Sender<()>>()) };
    let _ = sender.try_send(());
}
pub async fn after(duration: Duration) {
    let (send, receive) = async_channel::bounded::<()>(1);
    let context = Box::into_raw(Box::new(send)).cast();
    unsafe {
        let when = dispatch_walltime(
            std::ptr::null(),
            duration.as_nanos().try_into().expect("bounded hourly deadline"),
        );
        dispatch_after_f(when, dispatch_get_global_queue(-2, 0), context, elapsed);
    }
    // Wall deadlines pass during sleep, but never request a system wake. Each
    // delivered check schedules one new deadline, so missed intervals coalesce.
    let _ = receive.recv().await;
}
