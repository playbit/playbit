//! The active native context restricts sensitive fields to Roman input sources.
//! Policy changes run on the foreground thread after restoring the input handler,
//! outside window locks. AppKit retains responsibility for choosing a Roman layout.

use super::*;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSAllRomanInputSourcesLocaleIdentifier: id;
}

/// Call after focus/frame synchronization and before the first key is passed
/// to NSTextInputContext. The caller must not hold the window lock or a handler.
pub(super) fn synchronize(window: &Arc<Mutex<MacWindowState>>) {
    let (mut handler, native_window) = {
        let mut state = window.lock();
        (state.input_handler.take(), state.native_window)
    };
    let active = unsafe { native_window.isKeyWindow() == YES };
    let sensitive = active && handler.as_mut().is_some_and(|handler| handler.sensitive_input());
    window.lock().input_handler = handler;
    apply(window, sensitive);
}

/// Restore unrestricted input synchronously on window resignation and close.
/// Drop the restriction before disposing of the view or entering a window lock.
pub(super) fn clear(window: &Arc<Mutex<MacWindowState>>) {
    apply(window, false);
}

fn apply(window: &Arc<Mutex<MacWindowState>>, sensitive: bool) {
    let view = {
        let state = window.lock();
        if state.sensitive_input == sensitive {
            return;
        }
        state.native_view.as_ptr()
    };
    unsafe {
        let context: id = msg_send![view, inputContext];
        if context.is_null() {
            return;
        }
        // Store the transition before AppKit can reenter. The editor has
        // already committed its outgoing composition on actual focus loss.
        window.lock().sensitive_input = sensitive;
        let _: () = msg_send![context, discardMarkedText];
        if window.lock().sensitive_input != sensitive {
            return;
        }
        let locales = if sensitive {
            NSArray::arrayWithObject(nil, NSAllRomanInputSourcesLocaleIdentifier)
        } else {
            nil
        };
        let _: () = msg_send![context, setAllowedInputSourceLocales: locales];
    }
}
