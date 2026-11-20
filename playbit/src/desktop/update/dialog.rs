//! A native, nonblocking NSAlert. AppKit objects and callbacks stay on main.
#![allow(unsafe_code, reason = "Owned AppKit objects are only accessed on the main thread.")]
use async_channel::Sender;
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Dismiss,
    Install,
    Later,
}

pub struct Dialog {
    alert: *mut Object,
    target: *mut Object,
    _sender: Box<Sender<Choice>>,
}
extern "C" fn clicked(this: &Object, _: Sel, button: *mut Object) {
    // SAFETY: Dialog owns the boxed sender through all native callbacks. Tags
    // are assigned by new(), never taken from the release server.
    unsafe {
        let sender = (*this.get_ivar::<*const std::ffi::c_void>("sender")).cast::<Sender<Choice>>();
        let tag: isize = msg_send![button, tag];
        let choice = match tag {
            1 => Choice::Install,
            2 => Choice::Later,
            _ => Choice::Dismiss,
        };
        let _ = (*sender).try_send(choice);
    }
}
fn target_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut declaration = ClassDecl::new("PlaybitUpdateDialogTarget", class!(NSObject))
            .expect("unique update dialog class");
        declaration.add_ivar::<*const std::ffi::c_void>("sender");
        unsafe {
            declaration
                .add_method(sel!(choose:), clicked as extern "C" fn(&Object, Sel, *mut Object));
        }
        declaration.register()
    })
}
impl Dialog {
    pub fn new(
        title: &str,
        message: &str,
        buttons: &[(&str, Choice)],
        progress: bool,
        minimum_width: f64,
        sender: Sender<Choice>,
    ) -> Self {
        let sender = Box::new(sender);
        unsafe {
            let alert: *mut Object = msg_send![class!(NSAlert), new];
            let _: () = msg_send![alert, setAlertStyle: 1usize];
            let title = NSString::alloc(std::ptr::null_mut()).init_str(title);
            let message = NSString::alloc(std::ptr::null_mut()).init_str(message);
            let _: () = msg_send![alert, setMessageText: title];
            let _: () = msg_send![alert, setInformativeText: message];
            let _: () = msg_send![title, release];
            let _: () = msg_send![message, release];
            let target: *mut Object = msg_send![target_class(), new];
            (*target)
                .set_ivar("sender", (&*sender as *const Sender<Choice>).cast::<std::ffi::c_void>());
            for (label, choice) in buttons {
                let label = NSString::alloc(std::ptr::null_mut()).init_str(label);
                let button: *mut Object = msg_send![alert, addButtonWithTitle: label];
                let _: () = msg_send![label, release];
                let tag = match choice {
                    Choice::Dismiss => 0isize,
                    Choice::Install => 1,
                    Choice::Later => 2,
                };
                let _: () = msg_send![button, setTag: tag];
                let _: () = msg_send![button, setTarget: target];
                let _: () = msg_send![button, setAction: sel!(choose:)];
            }
            if progress {
                let indicator: *mut Object = msg_send![class!(NSProgressIndicator), alloc];
                let indicator: *mut Object = msg_send![indicator, initWithFrame: NSRect::new(NSPoint::new(0., 0.), NSSize::new(minimum_width.max(280.), 16.))];
                let _: () = msg_send![indicator, setIndeterminate: true];
                let _: () = msg_send![indicator, setUsesThreadedAnimation: true];
                let _: () = msg_send![indicator, startAnimation: std::ptr::null_mut::<Object>()];
                let _: () = msg_send![alert, setAccessoryView: indicator];
                let _: () = msg_send![indicator, release];
            } else if minimum_width > 0. {
                let spacer: *mut Object = msg_send![class!(NSView), alloc];
                let spacer: *mut Object = msg_send![spacer, initWithFrame: NSRect::new(NSPoint::new(0., 0.), NSSize::new(minimum_width, 0.))];
                let _: () = msg_send![alert, setAccessoryView: spacer];
                let _: () = msg_send![spacer, release];
            }
            let _: () = msg_send![alert, layout];
            let dialog = Self { alert, target, _sender: sender };
            dialog.focus();
            dialog
        }
    }
    pub fn focus(&self) {
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
            let window: *mut Object = msg_send![self.alert, window];
            let _: () = msg_send![window, makeKeyAndOrderFront: std::ptr::null_mut::<Object>()];
        }
    }
}
impl Drop for Dialog {
    fn drop(&mut self) {
        unsafe {
            let window: *mut Object = msg_send![self.alert, window];
            let _: () = msg_send![window, orderOut: std::ptr::null_mut::<Object>()];
            let _: () = msg_send![self.alert, release];
            let _: () = msg_send![self.target, release];
        }
    }
}

pub fn launch_error(message: &str) {
    crate::diagnostic!("Playbit couldn't open: {message}");
    modal_error("Playbit couldn't open", message);
}

pub fn update_error(message: &str) {
    modal_error("Couldn't update Playbit", message);
}

fn modal_error(title: &str, message: &str) {
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        // The detached installer has not entered the normal application loop.
        let _: bool = msg_send![app, setActivationPolicy: 0isize];
        let _: () = msg_send![app, finishLaunching];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let alert: *mut Object = msg_send![class!(NSAlert), new];
        let title = NSString::alloc(std::ptr::null_mut()).init_str(title);
        let body = NSString::alloc(std::ptr::null_mut()).init_str(message);
        let _: () = msg_send![alert, setMessageText: title];
        let _: () = msg_send![alert, setInformativeText: body];
        let _: isize = msg_send![alert, runModal];
        let _: () = msg_send![title, release];
        let _: () = msg_send![body, release];
        let _: () = msg_send![alert, release];
    }
}
