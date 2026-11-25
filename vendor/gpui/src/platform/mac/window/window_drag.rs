//! Publish stable native drag geometry for WindowServer. These views neither
//! paint nor receive input; GPUI continues to own controls and event dispatch.
use super::*;
use std::sync::LazyLock;

static NON_DRAGGABLE_VIEW: LazyLock<&'static Class> = LazyLock::new(|| unsafe {
    let mut class = ClassDecl::new("GPUINonDraggableView", class!(NSView)).unwrap();
    class.add_method(sel!(mouseDownCanMoveWindow), no as extern "C" fn(&Object, Sel) -> BOOL);
    class.add_method(sel!(hitTest:), hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id);
    class.add_method(sel!(isAccessibilityElement), no as extern "C" fn(&Object, Sel) -> BOOL);
    // Full-size content views need this AppKit hook to exclude controls in the
    // native titlebar too. mouseDownCanMoveWindow alone only excludes content.
    class.add_method(
        sel!(_opaqueRectForWindowMoveWhenInTitlebar),
        opaque_rect as extern "C" fn(&Object, Sel) -> NSRect,
    );
    class.register()
});

extern "C" fn hit_test(_: &Object, _: Sel, _: NSPoint) -> id {
    nil
}
extern "C" fn opaque_rect(this: &Object, _: Sel) -> NSRect {
    unsafe { msg_send![this, bounds] }
}

#[derive(Default)]
pub(super) struct WindowDrag {
    regions: Vec<Bounds<Pixels>>,
    // Borrowed from the native view hierarchy, which owns their lifetime.
    views: Vec<id>,
    height: Pixels,
}

impl WindowDrag {
    pub(super) fn update(&mut self, window: id, parent: id, regions: &[Bounds<Pixels>]) {
        unsafe {
            let height = px(NSView::bounds(parent).size.height as f32);
            if self.regions == regions && self.height == height {
                return;
            }
            self.height = height;
            self.regions.clear();
            self.regions.extend_from_slice(regions);
            while self.views.len() > regions.len() {
                let view = self.views.pop().unwrap();
                let _: () = msg_send![view, removeFromSuperview];
            }
            for (index, bounds) in regions.iter().enumerate() {
                let frame = NSRect::new(
                    NSPoint::new(bounds.left().to_f64(), (height - bounds.bottom()).to_f64()),
                    NSSize::new(bounds.size.width.to_f64(), bounds.size.height.to_f64()),
                );
                if let Some(view) = self.views.get(index) {
                    let _: () = msg_send![*view, setFrame: frame];
                } else {
                    let view: id = msg_send![*NON_DRAGGABLE_VIEW, alloc];
                    let view = NSView::initWithFrame_(view, frame);
                    parent.addSubview_(view);
                    self.views.push(view);
                    let _: () = msg_send![view, release];
                }
            }
            // Republish only when geometry changes, never on hover or down.
            window.setMovableByWindowBackground_(NO);
            window.setMovableByWindowBackground_(YES);
        }
    }
}

// Intercept before AppKit starts native window-move tracking, which can consume
// the content view's mouse events. Share its drag exclusions so controls keep
// their ordinary double-click behavior, and consume the down to avoid a second
// titlebar action from AppKit.
pub(super) extern "C" fn send_window_event(this: &Object, _: Sel, event: id) {
    unsafe {
        let state = get_window_state(this);
        let mut lock = state.lock();
        let left_mouse_down = event.eventType() == cocoa::appkit::NSEventType::NSLeftMouseDown;
        if left_mouse_down {
            lock.corner_resize = window_style::CornerResize::mouse_down(&lock, event);
        }
        if left_mouse_down
            && event.clickCount() == 2
            && !event.modifierFlags().contains(NSEventModifierFlags::NSControlKeyMask)
            && lock.has_custom_titlebar()
            && {
                let movable: BOOL = msg_send![this, isMovable];
                movable == YES
            }
        {
            let position =
                convert_mouse_position(event.locationInWindow(), lock.content_size().height);
            // Native traffic lights sit above the full-size content view and
            // therefore are not represented by GPUI's exclusion hitboxes.
            let over_native_button = [
                NSWindowButton::NSWindowCloseButton,
                NSWindowButton::NSWindowMiniaturizeButton,
                NSWindowButton::NSWindowZoomButton,
            ]
            .into_iter()
            .any(|kind| {
                let button = lock.native_window.standardWindowButton_(kind);
                if button.is_null() {
                    return false;
                }
                let hidden: BOOL = msg_send![button, isHiddenOrHasHiddenAncestor];
                let point: NSPoint =
                    msg_send![button, convertPoint: event.locationInWindow() fromView: nil];
                let bounds: NSRect = msg_send![button, bounds];
                hidden == NO
                    && point.x >= bounds.origin.x
                    && point.x < bounds.origin.x + bounds.size.width
                    && point.y >= bounds.origin.y
                    && point.y < bounds.origin.y + bounds.size.height
            });
            if !over_native_button && lock.window_drag.contains(position, lock.content_size()) {
                lock.titlebar_double_click();
                return;
            }
        }
        let superclass =
            if lock.kind == WindowKind::PopUp { class!(NSPanel) } else { class!(NSWindow) };
        drop(lock);
        let _: () = msg_send![super(this, superclass), sendEvent: event];
    }
}

impl WindowDrag {
    fn contains(&self, position: Point<Pixels>, content_size: Size<Pixels>) -> bool {
        // No published geometry yet means no custom titlebar action.
        self.height > px(0.)
            && Bounds::new(Point::default(), content_size).contains(&position)
            && !self.regions.iter().any(|region| region.contains(&position))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_click_region_excludes_controls_content_and_outside_window() {
        let content = size(px(600.), px(400.));
        let mut drag = WindowDrag::default();
        assert!(!drag.contains(point(px(200.), px(20.)), content));
        drag.height = content.height;
        drag.regions = vec![
            Bounds::new(point(px(0.), px(44.)), size(px(600.), px(356.))),
            Bounds::new(point(px(450.), px(0.)), size(px(100.), px(44.))),
        ];
        assert!(drag.contains(point(px(200.), px(20.)), content));
        assert!(!drag.contains(point(px(500.), px(20.)), content));
        assert!(!drag.contains(point(px(200.), px(100.)), content));
        assert!(!drag.contains(point(px(-1.), px(20.)), content));
        assert!(!drag.contains(point(px(601.), px(20.)), content));
    }
}
