//! PNG guests use their artwork directly. SVG icons follow the macOS appearance.
#![allow(unsafe_code, reason = "AppKit images and the observer are owned on the main thread.")]
use cocoa::foundation::NSString;
use objc::{class, msg_send, rc::StrongPtr, runtime::Object, sel, sel_impl};
use std::{marker::PhantomData, path::Path, rc::Rc};

/// PNG-source wrappers use this ICNS for both the bundle and the running Dock icon.
pub(super) const PNG_ICON: &str = "AppPNG.icns";

pub(crate) struct Observer {
    observer: *mut Object,
    // Never move AppKit ownership or observer removal to another thread.
    _main_thread: PhantomData<Rc<()>>,
}

impl Observer {
    /// Called once on AppKit's main thread after initialization. Only generated
    /// SVG wrappers have AppDark.icns and need an appearance observer.
    pub(crate) fn new() -> Option<Self> {
        let executable = std::env::current_exe().ok()?;
        let resources = executable.parent()?.parent()?.join("Resources");
        if !resources.join("AppDark.icns").is_file() {
            set_png_icon(&resources.join(PNG_ICON));
            return None;
        }
        let light = load(&resources.join("App.icns"));
        let dark = load(&resources.join("AppDark.icns"));
        if light.is_null() || dark.is_null() {
            unsafe {
                for image in [light, dark] {
                    if !image.is_null() {
                        let _: () = msg_send![image, release];
                    }
                }
            }
            crate::diagnostic!("The generated app icon could not be loaded");
            return None;
        }
        // The block retains both images, including while a notification is
        // queued. Removing the observer cannot leave a callback with freed images.
        // SAFETY: load returned two owned NSImage references on the main thread.
        let images = Rc::new(unsafe { [StrongPtr::new(light), StrongPtr::new(dark)] });
        set(*images[0], *images[1]);
        let handler =
            block::ConcreteBlock::new(move |_: *mut Object| set(*images[0], *images[1])).copy();
        unsafe {
            let center: *mut Object =
                msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
            let queue: *mut Object = msg_send![class!(NSOperationQueue), mainQueue];
            let name = NSString::alloc(std::ptr::null_mut())
                .init_str("AppleInterfaceThemeChangedNotification");
            let observer: *mut Object = msg_send![center, addObserverForName: name object: std::ptr::null_mut::<Object>() queue: queue usingBlock: handler];
            let _: () = msg_send![name, release];
            Some(Self { observer, _main_thread: PhantomData })
        }
    }
}

fn set_png_icon(path: &Path) {
    if !path.is_file() {
        return;
    }
    let image = load(path);
    if image.is_null() {
        crate::diagnostic!("The generated PNG app icon could not be loaded");
        return;
    }
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
        let _: () = msg_send![image, release];
    }
}

fn load(path: &Path) -> *mut Object {
    let Some(path) = path.to_str() else { return std::ptr::null_mut() };
    unsafe {
        let path = NSString::alloc(std::ptr::null_mut()).init_str(path);
        let image: *mut Object = msg_send![class!(NSImage), alloc];
        let image: *mut Object = msg_send![image, initWithContentsOfFile: path];
        let _: () = msg_send![path, release];
        image
    }
}

fn set(light: *mut Object, dark: *mut Object) {
    unsafe {
        let defaults: *mut Object = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = NSString::alloc(std::ptr::null_mut()).init_str("AppleInterfaceStyle");
        let value: *mut Object = msg_send![defaults, stringForKey: key];
        let is_dark = is_dark_appearance(value);
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: if is_dark { dark } else { light }];
        let _: () = msg_send![key, release];
    }
}

/// `value` must be nil or a live NSString from the appearance preference.
unsafe fn is_dark_appearance(value: *mut Object) -> bool {
    // Light mode normally has no AppleInterfaceStyle value. objc's message
    // macro requires a non-null receiver even though Objective-C accepts nil.
    if value.is_null() {
        return false;
    }
    unsafe {
        let expected = NSString::alloc(std::ptr::null_mut()).init_str("Dark");
        let is_dark: objc::runtime::BOOL = msg_send![value, isEqualToString: expected];
        let _: () = msg_send![expected, release];
        is_dark == objc::runtime::YES
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        unsafe {
            let center: *mut Object =
                msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
            let _: () = msg_send![center, removeObserver: self.observer];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_selects_dark_only_for_dark_preference() {
        unsafe {
            assert!(!is_dark_appearance(std::ptr::null_mut()));
            for (style, expected) in [("Light", false), ("Dark", true), ("", false)] {
                let value = NSString::alloc(std::ptr::null_mut()).init_str(style);
                let actual = is_dark_appearance(value);
                let _: () = msg_send![value, release];
                assert_eq!(actual, expected, "appearance {style:?}");
            }
        }
    }
}
