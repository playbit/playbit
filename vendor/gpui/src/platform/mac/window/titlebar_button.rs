//! AppKit owns the right-hand titlebar button's layout, input, and accessibility.
use super::*;
use crate::MacTitlebarButton;
use cocoa::{
    appkit::{
        NSCompositingOperation::NSCompositeSourceOver, NSImage, NSViewMaxYMargin, NSViewMinYMargin,
    },
    foundation::NSData,
};
use objc::rc::StrongPtr;
use std::sync::OnceLock;

pub(super) struct Accessory {
    controller: StrongPtr,
    button: StrongPtr,
}

fn button_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| unsafe {
        let mut decl = ClassDecl::new("GPUITitlebarButton", class!(NSButton)).unwrap();
        decl.add_ivar::<BOOL>("gpuiHovered");
        decl.add_method(sel!(wantsUpdateLayer), no as extern "C" fn(&Object, Sel) -> BOOL);
        decl.add_method(sel!(drawRect:), draw as extern "C" fn(&Object, Sel, NSRect));
        decl.add_method(sel!(mouseEntered:), hover as extern "C" fn(&mut Object, Sel, id));
        decl.add_method(sel!(mouseExited:), hover as extern "C" fn(&mut Object, Sel, id));
        decl.register()
    })
}

extern "C" fn hover(button: &mut Object, event: Sel, _: id) {
    unsafe {
        button.set_ivar("gpuiHovered", (event == sel!(mouseEntered:)) as BOOL);
        let _: () = msg_send![button, setNeedsDisplay: YES];
    }
}

extern "C" fn draw(button: &Object, _: Sel, _: NSRect) {
    unsafe {
        let bounds: NSRect = msg_send![button, bounds];
        let enabled: BOOL = msg_send![button, isEnabled];
        let cell: id = msg_send![button, cell];
        let pressed: BOOL = msg_send![cell, isHighlighted];
        let hovered = enabled == YES && *button.get_ivar::<BOOL>("gpuiHovered") == YES;
        if pressed == YES || hovered {
            let alpha = if pressed == YES { 0.15_f64 } else { 0.08 };
            let fill: id = msg_send![class!(NSColor), colorWithWhite: 0.0_f64 alpha: alpha];
            let _: () = msg_send![fill, setFill];
            let pill: id = msg_send![class!(NSBezierPath), bezierPathWithRoundedRect: bounds xRadius: 12.0_f64 yRadius: 12.0_f64];
            let _: () = msg_send![pill, fill];
        }
        let layer: id = msg_send![button, layer];
        let ring: f64 = msg_send![layer, borderWidth];
        let opacity = if pressed == YES || hovered || ring > 0. { 0.9 } else { 0.4 };
        let image: id = msg_send![button, image];
        let size: NSSize = msg_send![image, size];
        let frame = NSRect::new(
            NSPoint::new(
                (bounds.size.width - size.width) / 2.,
                (bounds.size.height - size.height) / 2.,
            ),
            size,
        );
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let shadow = StrongPtr::new(msg_send![class!(NSShadow), new]);
        let white: id = msg_send![class!(NSColor), colorWithWhite: 1.0_f64 alpha: 0.4_f64];
        let _: () = msg_send![*shadow, setShadowColor: white];
        let _: () = msg_send![*shadow, setShadowOffset: NSSize::new(0., -1.)];
        let _: () = msg_send![*shadow, setShadowBlurRadius: 0.0_f64];
        let _: () = msg_send![*shadow, set];
        image.drawInRect_fromRect_operation_fraction_respectFlipped_hints_(
            frame,
            NSRect::new(NSPoint::new(0., 0.), size),
            NSCompositeSourceOver,
            opacity,
            YES,
            nil,
        );
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
    }
}

unsafe fn color(value: crate::Rgba) -> id {
    unsafe {
        msg_send![class!(NSColor), colorWithDisplayP3Red: f64::from(value.r)
            green: f64::from(value.g) blue: f64::from(value.b) alpha: f64::from(value.a)]
    }
}

pub(super) unsafe fn update(
    state: &Mutex<MacWindowState>,
    native: id,
    spec: Option<MacTitlebarButton>,
    previous: Option<MacTitlebarButton>,
) {
    // AppKit can resize the view synchronously when titlebar accessories change.
    let accessory = state.lock().titlebar_accessory.take();
    unsafe {
        let Some(spec) = spec else {
            if let Some(accessory) = accessory {
                let _: () = msg_send![*accessory.controller, removeFromParentViewController];
            }
            return;
        };
        let label = ns_string(spec.label);
        let is_new = accessory.is_none();
        let accessory = accessory.unwrap_or_else(|| {
            let controller: id = msg_send![class!(NSTitlebarAccessoryViewController), new];
            let controller = StrongPtr::new(controller);
            // Pin to the physical right, including in right-to-left locales.
            const LAYOUT_ATTRIBUTE_RIGHT: NSInteger = 2;
            let _: () = msg_send![*controller, setLayoutAttribute: LAYOUT_ATTRIBUTE_RIGHT];
            let button: id = msg_send![button_class(), buttonWithTitle: ns_string("")
                target: native action: sel!(gpuiTitlebarAction:)];
            let _: () = msg_send![button, setImagePosition: 1_usize]; // NSImageOnly
            let _: () = msg_send![button, setBordered: NO];
            // GPUI owns keyboard traversal even when AppKit owns pointer hits.
            let _: () = msg_send![button, setRefusesFirstResponder: YES];
            // AppKit stretches the accessory to the height of the titlebar.
            // Keep the button inset from the rounded window corner.
            let view = StrongPtr::new(NSView::initWithFrame_(
                NSView::alloc(nil), NSRect::new(NSPoint::new(0., 0.), NSSize::new(38., 32.))
            ));
            let _: () = msg_send![button, setFrame: NSRect::new(NSPoint::new(0., 4.), NSSize::new(34., 24.))];
            button.setAutoresizingMask_(NSViewMinYMargin | NSViewHeightSizable | NSViewMaxYMargin);
            button.setWantsLayer(YES);
            let _: () = msg_send![button.layer(), setCornerRadius: 12.0_f64];
            let area: id = msg_send![class!(NSTrackingArea), alloc];
            let area = StrongPtr::new(msg_send![area, initWithRect: NSView::bounds(button)
                options: NSTrackingMouseEnteredAndExited | NSTrackingActiveAlways | NSTrackingInVisibleRect
                owner: button userInfo: nil]);
            let _: () = msg_send![button, addTrackingArea: *area];
            view.addSubview_(button);
            let _: () = msg_send![*controller, setView: *view];
            Accessory { controller, button: StrongPtr::retain(button) }
        });
        let button = *accessory.button;
        if is_new || previous.is_none_or(|old| old.image != spec.image) {
            let data = NSData::dataWithBytes_length_(
                nil,
                spec.image.as_ptr().cast(),
                spec.image.len() as NSUInteger,
            );
            let image = StrongPtr::new(NSImage::initWithData_(NSImage::alloc(nil), data));
            let _: () = msg_send![button, setImage: *image];
        }
        let _: () = msg_send![button, setToolTip: label];
        let _: () = msg_send![button, setAccessibilityLabel: label];
        let _: () = msg_send![button, setEnabled: spec.enabled as BOOL];
        if previous.is_none_or(|old| old.keyboard_pressed != spec.keyboard_pressed) {
            let _: () = msg_send![button, highlight: spec.keyboard_pressed as BOOL];
        }
        let layer = button.layer();
        let background: id = msg_send![color(spec.background), CGColor];
        let _: () = msg_send![layer, setBackgroundColor: background];
        if let Some(ring) = spec.focus_ring {
            let border: id = msg_send![color(ring), CGColor];
            let _: () = msg_send![layer, setBorderColor: border];
        }
        let border_width = if spec.focus_ring.is_some() { 2.0_f64 } else { 0. };
        let _: () = msg_send![layer, setBorderWidth: border_width];
        let _: () = msg_send![button, setNeedsDisplay: YES];
        if is_new {
            // GPUI's override hides AppKit's tab strip. Install this accessory
            // through NSWindow directly so it keeps its native frame.
            let _: () = msg_send![super(native, class!(NSWindow)), addTitlebarAccessoryViewController: *accessory.controller];
        }
        state.lock().titlebar_accessory = Some(accessory);
    }
}

pub(super) extern "C" fn clicked(this: &Object, _: Sel, _: id) {
    let state = unsafe { get_window_state(this) };
    let mut window = state.lock();
    let spec =
        window.pending_style.or(window.applied_style).and_then(|style| style.titlebar_button);
    let Some(spec) = spec.filter(|spec| spec.enabled) else { return };
    if let Some(mut dispatch) = window.titlebar_action.take() {
        drop(window);
        dispatch(spec.action);
        state.lock().titlebar_action = Some(dispatch);
    }
}
