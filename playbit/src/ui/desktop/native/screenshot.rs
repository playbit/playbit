//! Capture the selected owner's composited NSWindow. ScreenCaptureKit enumerates only
//! content available to this process without consent, then filters to its own window.
#![allow(
    unsafe_code,
    unexpected_cfgs,
    reason = "ScreenCaptureKit and AppKit's native capture callbacks are Objective-C APIs."
)]
use block::ConcreteBlock;
use cocoa::foundation::NSRect;
use objc::{class, msg_send, rc::autoreleasepool, runtime::Object, sel, sel_impl};
use polling::Poller;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::{Arc, mpsc::SyncSender};

#[link(name = "ScreenCaptureKit", kind = "framework")]
unsafe extern "C" {
    #[link_name = "OBJC_CLASS_$_SCShareableContent"]
    static SHAREABLE_CONTENT: objc::runtime::Class;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageRetain(image: *mut Object) -> *mut Object;
    fn CGImageRelease(image: *mut Object);
    fn CGImageGetWidth(image: *mut Object) -> usize;
    fn CGImageGetHeight(image: *mut Object) -> usize;
}

// Bound ScreenCaptureKit backing surfaces and temporary PNG encoder storage.
// Very large windows are sampled proportionally at up to 2048 pixels per edge.
const MAX_EDGE: f64 = 2048.;

pub(super) fn window_number(window: &gpui::Window) -> Option<u32> {
    let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    // SAFETY: On AppKit's UI thread the GPUI handle borrows this live NSView.
    unsafe {
        let native: *mut Object = msg_send![handle.ns_view.as_ptr() as *mut Object, window];
        if native.is_null() {
            return None;
        }
        let number: isize = msg_send![native, windowNumber];
        u32::try_from(number).ok().filter(|n| *n != 0)
    }
}

pub(super) struct Capture {
    result: SyncSender<Result<Vec<u8>, String>>,
    poller: Arc<Poller>,
}

impl Capture {
    pub(super) fn new(result: SyncSender<Result<Vec<u8>, String>>, poller: Arc<Poller>) -> Self {
        Self { result, poller }
    }

    pub(super) fn complete(&self, result: Result<Vec<u8>, String>) {
        // A caller may disconnect while the OS owns the callback.
        if self.result.try_send(result).is_ok() {
            self.poller.notify().expect("live screenshot readiness notification");
        }
    }

    pub(super) fn start(self: Arc<Self>, number: u32, background: gpui::BackgroundExecutor) {
        let completion = ConcreteBlock::new(move |content: *mut Object, error: *mut Object| {
            if content.is_null() || !error.is_null() {
                return self
                    .complete(Err(platform_error(error, "Could not enumerate the app window")));
            }
            // SAFETY: ScreenCaptureKit lends valid shareable content during this callback.
            let window = unsafe {
                let windows: *mut Object = msg_send![content, windows];
                let count: usize = msg_send![windows, count];
                (0..count).find_map(|i| {
                    let window: *mut Object = msg_send![windows, objectAtIndex: i];
                    let id: u32 = msg_send![window, windowID];
                    if id != number {
                        return None;
                    }
                    let owner: *mut Object = msg_send![window, owningApplication];
                    let pid: libc::pid_t = msg_send![owner, processID];
                    (pid == libc::getpid()).then_some(window)
                })
            };
            if let Some(window) = window {
                capture_window(window, self.clone(), background.clone());
            } else {
                self.complete(Err("The selected app window is unavailable for capture".into()));
            }
        })
        .copy();
        // SAFETY: This current-process API (macOS 14.4+) copies the completion block.
        unsafe {
            let _: () = msg_send![&SHAREABLE_CONTENT,
                getCurrentProcessShareableContentWithCompletionHandler: &*completion];
        }
    }
}

fn capture_window(
    window: *mut Object,
    capture: Arc<Capture>,
    background: gpui::BackgroundExecutor,
) {
    // SAFETY: The caller holds the SCWindow for this call. The framework retains
    // our filter, configuration and copied completion until asynchronous capture.
    unsafe {
        let filter: *mut Object = msg_send![class!(SCContentFilter), alloc];
        let filter: *mut Object = msg_send![filter, initWithDesktopIndependentWindow: window];
        if filter.is_null() {
            return capture.complete(Err("The selected window could not be captured".into()));
        }
        let rect: NSRect = msg_send![filter, contentRect];
        let scale: f32 = msg_send![filter, pointPixelScale];
        let width = rect.size.width * f64::from(scale);
        let height = rect.size.height * f64::from(scale);
        if !width.is_finite() || !height.is_finite() || width < 1. || height < 1. {
            let _: () = msg_send![filter, release];
            return capture.complete(Err("The selected window has no drawable area".into()));
        }
        let ratio = (MAX_EDGE / width.max(height)).min(1.);
        let config: *mut Object = msg_send![class!(SCStreamConfiguration), new];
        let _: () = msg_send![config, setWidth: (width * ratio).round().max(1.) as usize];
        let _: () = msg_send![config, setHeight: (height * ratio).round().max(1.) as usize];
        let _: () = msg_send![config, setIgnoreShadowsSingleWindow: objc::runtime::YES];
        let _: () = msg_send![config, setShowsCursor: objc::runtime::NO];
        let _: () = msg_send![config, setIncludeChildWindows: objc::runtime::NO];
        let completion = ConcreteBlock::new(move |image: *mut Object, error: *mut Object| {
            if image.is_null() || !error.is_null() {
                return capture
                    .complete(Err(platform_error(error, "Could not capture the app window")));
            }
            let image = CapturedImage(CGImageRetain(image));
            let capture = capture.clone();
            background
                .spawn(async move {
                    capture.complete(autoreleasepool(|| image.png()));
                })
                .detach();
        })
        .copy();
        let _: () = msg_send![class!(SCScreenshotManager),
            captureImageWithFilter: filter configuration: config completionHandler: &*completion];
        let _: () = msg_send![config, release];
        let _: () = msg_send![filter, release];
    }
}

struct CapturedImage(*mut Object);
// SAFETY: A CGImage is immutable, and the screenshot callback retained this
// reference for the background encoder. AppKit's UI thread never encodes PNGs.
unsafe impl Send for CapturedImage {}
impl Drop for CapturedImage {
    fn drop(&mut self) {
        // SAFETY: This balances the successful image callback's CGImageRetain.
        unsafe { CGImageRelease(self.0) };
    }
}

impl CapturedImage {
    fn png(&self) -> Result<Vec<u8>, String> {
        let image = self.0;
        // SAFETY: The CGImage and the autoreleased bitmap representation and data
        // all outlive the copy into the bounded control response.
        unsafe {
            if CGImageGetWidth(image) > MAX_EDGE as usize
                || CGImageGetHeight(image) > MAX_EDGE as usize
            {
                return Err("The captured window exceeds the screenshot pixel limit".into());
            }
            let rep: *mut Object = msg_send![class!(NSBitmapImageRep), alloc];
            let rep: *mut Object = msg_send![rep, initWithCGImage: image];
            let options: *mut Object = msg_send![class!(NSDictionary), dictionary];
            let png: *mut Object =
                msg_send![rep, representationUsingType: 4usize properties: options];
            let _: *mut Object = msg_send![rep, autorelease];
            let length: usize = msg_send![png, length];
            if length == 0 {
                return Err("The app window could not be encoded as PNG".into());
            }
            if length > crate::ui::app_control::MAX_SCREENSHOT_BYTES {
                return Err("Window PNG exceeds the 2 MiB screenshot limit".into());
            }
            let bytes: *const u8 = msg_send![png, bytes];
            Ok(std::slice::from_raw_parts(bytes, length).to_vec())
        }
    }
}

fn platform_error(error: *mut Object, fallback: &str) -> String {
    if error.is_null() {
        return fallback.into();
    }
    // SAFETY: The framework lends an NSError in its callback. Bounded copying
    // keeps native errors within the shared app control reply contract.
    unsafe {
        let message: *mut Object = msg_send![error, localizedDescription];
        let bytes: *const std::ffi::c_char = msg_send![message, UTF8String];
        if bytes.is_null() {
            return fallback.into();
        }
        let bytes = std::slice::from_raw_parts(bytes.cast(), libc::strnlen(bytes, 512));
        format!("{fallback}: {}", String::from_utf8_lossy(bytes))
    }
}
