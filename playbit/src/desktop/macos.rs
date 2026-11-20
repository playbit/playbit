#![allow(
    unsafe_code,
    reason = "AppKit owns the main thread and the appearance objects borrowed by these messages."
)]

use objc::{class, msg_send, runtime::Object, sel, sel_impl};

#[cfg(not(test))]
pub fn startup_failure(message: &str, can_reset: bool) -> bool {
    confirm("Playbit couldn’t start", message, can_reset.then_some("Sign in Again"))
}

fn confirm(title: &str, message: &str, action: Option<&str>) -> bool {
    use std::ffi::CString;
    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: 0isize];
        let alert: *mut Object = msg_send![class!(NSAlert), new];
        for (text, role) in [(Some(title), 0), (Some(message), 1), (action, 2), (Some("Quit"), 2)] {
            let Some(text) = text else { continue };
            let text = CString::new(text.replace('\0', "�")).expect("NUL characters removed");
            let string: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()];
            match role {
                0 => {
                    let _: () = msg_send![alert, setMessageText: string];
                }
                1 => {
                    let _: () = msg_send![alert, setInformativeText: string];
                }
                _ => {
                    let _: *mut Object = msg_send![alert, addButtonWithTitle: string];
                }
            }
        }
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let response: isize = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        let _: () = msg_send![pool, drain];
        action.is_some() && response == 1000
    }
}
