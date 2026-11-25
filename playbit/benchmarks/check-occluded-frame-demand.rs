//! Native correctness fixture: covered, unfocused windows retain developer frame demand.
//! No global input, application activation, engine or user profile.
#![allow(clippy::unwrap_used, reason = "The native fixture checks platform operations.")]
#![allow(unsafe_code, unexpected_cfgs, reason = "Inspect only this fixture's AppKit windows.")]
use gpui::{prelude::*, *};
use objc::{class, msg_send, rc::StrongPtr, runtime::Object, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, rc::Rc, time::Duration};

struct Fixture {
    changed: bool,
}
impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(if self.changed { 0x2277cc } else { 0xeeeeee }))
    }
}
fn native(handle: WindowHandle<Fixture>, cx: &mut App) -> *mut Object {
    handle
        .update(cx, |_, window, _| {
            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("macOS fixture requires an AppKit window")
            };
            unsafe { msg_send![handle.ns_view.as_ptr() as *mut Object, window] }
        })
        .unwrap()
}
fn state(window: *mut Object) -> (bool, bool) {
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let active: bool = msg_send![app, isActive];
        let visible: bool = msg_send![window, isVisible];
        let occlusion: usize = msg_send![window, occlusionState];
        assert!(visible, "fixture target must be ordered onscreen");
        (active, occlusion & 2 == 0)
    }
}
fn main() {
    Application::new().run(|cx| {
        let bounds = Bounds::new(point(px(100.), px(100.)), size(px(160.), px(120.)));
        let options = || WindowOptions {
            focus: false,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        let target =
            cx.open_window(options(), |_, cx| cx.new(|_| Fixture { changed: false })).unwrap();
        let target_native = native(target, cx);
        // The cover uses AppKit's opaque backing, independent of the GPUI
        // presentation path being tested (which may not have presented yet).
        let cover = unsafe {
            let mut frame: cocoa::foundation::NSRect = msg_send![target_native, frame];
            frame.origin.x -= 10.;
            frame.origin.y -= 10.;
            frame.size.width += 20.;
            frame.size.height += 20.;
            let cover: *mut Object = msg_send![class!(NSWindow), alloc];
            let cover: *mut Object = msg_send![cover, initWithContentRect: frame
                styleMask: 0usize backing: 2usize defer: false];
            let color: *mut Object = msg_send![class!(NSColor), windowBackgroundColor];
            let _: () = msg_send![cover, setOpaque: true];
            let _: () = msg_send![cover, setBackgroundColor: color];
            let _: () = msg_send![cover, setReleasedWhenClosed: false];
            let _: () = msg_send![target_native, orderBack: std::ptr::null_mut::<Object>()];
            let _: () = msg_send![cover, orderBack: std::ptr::null_mut::<Object>()];
            let number: i64 = msg_send![target_native, windowNumber];
            let _: () = msg_send![cover, orderWindow: 1isize relativeTo: number];
            StrongPtr::new(cover)
        };
        let callbacks = Rc::new(Cell::new(0));
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(300)).await;
            let (active, occluded) = state(target_native);
            if active || !occluded {
                println!(
                    "{{\"complete\":false,\"setup_failed\":true,\"app_active\":{active},\
                     \"occluded\":{occluded}}}"
                );
                std::process::exit(78);
            }
            let observed = callbacks.clone();
            target
                .update(cx, |fixture, window, cx| {
                    fixture.changed = true;
                    cx.notify();
                    window.on_next_frame(move |window, _| {
                        observed.set(observed.get() + 1);
                        window.on_next_frame(move |_, _| observed.set(observed.get() + 1));
                    });
                })
                .unwrap();
            // A functional deadline, never startup or interaction latency evidence.
            Timer::after(Duration::from_secs(3)).await;
            let expected =
                if cfg!(any(debug_assertions, feature = "dev_always_render")) { 2 } else { 0 };
            let received = callbacks.get();
            let (active, occluded) = state(target_native);
            let complete = received == expected && !active && occluded;
            println!(
                "{{\"complete\":{complete},\"expected_callbacks\":{expected},\
                 \"callbacks\":{received},\"app_active\":{active},\"occluded\":{occluded}}}"
            );
            if !complete {
                std::process::exit(1);
            }
            Timer::after(Duration::from_millis(300)).await;
            assert_eq!(callbacks.get(), expected, "completed callback demand remains idle");
            unsafe {
                let _: () = msg_send![*cover, close];
            }
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
