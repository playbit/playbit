//! Match native window chrome to the shared renderer's current light theme.
#![allow(
    unsafe_code,
    unexpected_cfgs,
    reason = "AppKit appearance objects are borrowed on its main thread."
)]
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSAppearanceNameAqua: *mut Object;
}

/// Called by the light-themed desktop shell before creating its windows.
pub fn set_light_appearance() {
    let _timing = crate::trace::Span::new("ui.set_light_appearance");
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setAppearance: aqua()];
    }
}

pub(super) fn aqua() -> *mut Object {
    // AppKit owns this named appearance for the lifetime of the process.
    let appearance: *mut Object =
        unsafe { msg_send![class!(NSAppearance), appearanceNamed: NSAppearanceNameAqua] };
    assert!(!appearance.is_null(), "AppKit must provide the Aqua appearance");
    appearance
}

/// The shared sign-in panel has a light design even in a guest process that
/// follows system appearance. AppKit may reenter GPUI when appearance changes.
#[cfg(feature = "connected-engine")]
pub(super) fn set_light_panel(window: &gpui::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
    unsafe {
        let native: *mut Object = msg_send![handle.ns_view.as_ptr() as *mut Object, window];
        let _: () = msg_send![native, performSelector: sel!(setAppearance:)
            withObject: aqua() afterDelay: 0.0_f64];
    }
}
/// Read on the UI thread when beginning a decorative transition.
pub fn reduce_motion() -> bool {
    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let reduce: objc::runtime::BOOL =
            msg_send![workspace, accessibilityDisplayShouldReduceMotion];
        reduce == objc::runtime::YES
    }
}

/// Disable native titlebar buttons while the window's custom dialog owns input.
/// GPUI's inert subtree covers content; AppKit owns these three controls.
pub fn set_content_modal(window: &gpui::Window, active: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
    unsafe {
        let native: *mut Object = msg_send![handle.ns_view.as_ptr() as *mut Object, window];
        for kind in [0_usize, 1, 2] {
            let button: *mut Object = msg_send![native, standardWindowButton: kind];
            if !button.is_null() {
                let enabled = if active { objc::runtime::NO } else { objc::runtime::YES };
                let _: () = msg_send![button, setEnabled: enabled];
            }
        }
    }
}
