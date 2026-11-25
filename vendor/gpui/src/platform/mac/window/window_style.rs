//! Apply live AppKit mutations outside GPUI rendering. Changing the style mask
//! can synchronously call back into the window to resize its Metal view.
use super::*;
use crate::{MacWindowSizeConstraints, MacWindowStyle};

pub(super) fn schedule(state: &Arc<Mutex<MacWindowState>>, style: MacWindowStyle) {
    let mut window = state.lock();
    if window.pending_style.or(window.applied_style) == Some(style) {
        return;
    }
    if window.pending_style.replace(style).is_some() {
        return;
    }
    let weak = Arc::downgrade(state);
    window
        .executor
        .spawn(async move {
            if let Some(state) = weak.upgrade() {
                apply(&state);
            }
        })
        .detach();
}

/// Inputs from the display used to fit the last installed normal-window policy.
/// AppKit reports parameter changes for every screen, including color-only changes.
#[derive(Clone, Copy)]
pub(super) struct ConstraintDisplay {
    scale: f32,
    visible: Option<NSRect>,
}

impl ConstraintDisplay {
    fn read(window: &MacWindowState) -> Self {
        let screen = unsafe { window.native_window.screen() };
        Self {
            scale: window.scale_factor(),
            visible: (screen != nil).then(|| unsafe { screen.visibleFrame() }),
        }
    }

    fn matches(self, other: Self) -> bool {
        let geometry =
            |frame: NSRect| (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height);
        self.scale == other.scale && self.visible.map(geometry) == other.visible.map(geometry)
    }
}

pub(super) fn display_changed(state: &Arc<Mutex<MacWindowState>>) {
    let window = state.lock();
    let Some(installed) = window.guest_constraint_display else { return };
    if window.owner_dropped
        || window.is_fullscreen()
        || installed.matches(ConstraintDisplay::read(&window))
    {
        return;
    }
    drop(window);
    reapply(state);
}

pub(super) extern "C" fn screen_parameters_changed(this: &Object, _: Sel, _: id) {
    // NSApplication posts on the main thread; the observer belongs to its live NSWindow.
    let state = unsafe { get_window_state(this) };
    display_changed(&state);
}

pub(super) unsafe fn observe_screen_parameters(native: id, observe: bool) {
    unsafe extern "C" {
        static NSApplicationDidChangeScreenParametersNotification: id;
    }
    unsafe {
        let center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
        let name = NSApplicationDidChangeScreenParametersNotification;
        let app = NSApplication::sharedApplication(nil);
        if observe {
            let _: () = msg_send![center, addObserver: native selector: sel!(playbitScreenParametersChanged:)
                name: name object: app];
        } else {
            let _: () = msg_send![center, removeObserver: native name: name object: app];
        }
    }
}

pub(super) fn reapply(state: &Arc<Mutex<MacWindowState>>) {
    let mut window = state.lock();
    let last = window.applied_style.take();
    let wanted = window.pending_style.or(last);
    drop(window);
    if let Some(style) = wanted {
        schedule(state, style);
    }
}

fn apply(state: &Arc<Mutex<MacWindowState>>) {
    let mut window = state.lock();
    let Some(style) = window.pending_style.take() else { return };
    if window.owner_dropped || window.applied_style == Some(style) {
        return;
    }
    let previous = window.applied_style.replace(style);
    let constrained = style.size_constraints != MacWindowSizeConstraints::default();
    let update_constraints = if constrained {
        previous.is_none_or(|old| {
            old.size_constraints != style.size_constraints
                || old.titled != style.titled
                || old.full_size_content != style.full_size_content
        })
    } else {
        window.guest_constraint_display.is_some()
    } && !window.is_fullscreen();
    let display = update_constraints.then(|| ConstraintDisplay::read(&window));
    let observer_changed =
        display.is_some() && window.guest_constraint_display.is_some() != constrained;
    if let Some(display) = display {
        window.guest_constraint_display = constrained.then_some(display);
    }
    let native = window.native_window;
    let view = window.native_view.as_ptr();
    let old_titlebar_height = window.titlebar_height();
    let old_controls_right = window.traffic_light_right;
    let stop_custom_drag = previous.map_or(window.transparent_titlebar, |old| {
        old.transparent_titlebar && old.full_size_content
    }) && !(style.transparent_titlebar && style.full_size_content);
    let restore_buttons =
        window.traffic_light_position.is_some() && style.traffic_light_position.is_none();
    window.traffic_light_position = style.traffic_light_position;
    if restore_buttons {
        window.move_traffic_light();
    }
    window.transparent_titlebar = style.transparent_titlebar;
    if previous.is_none_or(|old| old.background.a != style.background.a) {
        window.renderer.update_transparency(style.background.a < 1.);
    }
    if stop_custom_drag {
        window.window_drag.update(native, view, &[]);
    }
    drop(window);

    // The run loop owns the window and view for this synchronous operation.
    // Do not hold the state mutex across AppKit resize or appearance callbacks.
    unsafe {
        if stop_custom_drag {
            native.setMovableByWindowBackground_(NO);
        }
        if observer_changed {
            observe_screen_parameters(native, constrained);
        }
        set_mask(native, view, style);
        if previous.is_none_or(|old| old.titlebar_button != style.titlebar_button) {
            titlebar_button::update(
                state,
                native,
                style.titlebar_button,
                previous.and_then(|s| s.titlebar_button),
            );
        }
        if let Some(display) = display {
            apply_size_constraints(state, native, style.size_constraints, display);
        }
        if previous.is_none_or(|old| {
            old.transparent_titlebar != style.transparent_titlebar || old.titled != style.titled
        }) {
            native.setTitlebarAppearsTransparent_(style.transparent_titlebar as BOOL);
        }
        if previous
            .is_none_or(|old| old.hidden_title != style.hidden_title || old.titled != style.titled)
        {
            native.setTitleVisibility_(if style.hidden_title {
                NSWindowTitleVisibility::NSWindowTitleHidden
            } else {
                NSWindowTitleVisibility::NSWindowTitleVisible
            });
        }
        if previous.is_none_or(|old| old.appearance != style.appearance) {
            use super::super::window_appearance::{NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
            use cocoa::appkit::{
                NSAppearance, NSAppearanceNameVibrantDark, NSAppearanceNameVibrantLight,
            };
            let appearance = match style.appearance {
                Some(WindowAppearance::Light) => NSAppearance(NSAppearanceNameAqua),
                Some(WindowAppearance::Dark) => NSAppearance(NSAppearanceNameDarkAqua),
                Some(WindowAppearance::VibrantLight) => NSAppearance(NSAppearanceNameVibrantLight),
                Some(WindowAppearance::VibrantDark) => NSAppearance(NSAppearanceNameVibrantDark),
                None => nil,
            };
            let _: () = msg_send![native, setAppearance: appearance];
        }
        if previous.is_none_or(|old| old.background != style.background) {
            let color = style.background;
            let background: id = msg_send![class!(NSColor),
                colorWithDisplayP3Red: f64::from(color.r)
                green: f64::from(color.g)
                blue: f64::from(color.b)
                alpha: f64::from(color.a)];
            native.setOpaque_((color.a == 1.) as BOOL);
            native.setBackgroundColor_(background);
        }
    }
    let mut window = state.lock();
    window.move_traffic_light();
    // A chrome-only change can move the drag region or the system controls
    // without resizing the content view.
    if (old_titlebar_height != window.titlebar_height()
        || old_controls_right != window.traffic_light_right)
        && let Some(mut resize) = window.resize_callback.take()
    {
        let size = window.content_size();
        let scale = window.scale_factor();
        drop(window);
        resize(size, scale);
        state.lock().resize_callback = Some(resize);
    }
}

unsafe fn set_mask(native: id, view: id, style: MacWindowStyle) {
    use NSWindowStyleMask as Mask;
    unsafe {
        // Preserve unrelated AppKit bits, including fullscreen during transitions.
        let current: NSUInteger = msg_send![native, styleMask];
        let mut mask = Mask::from_bits_retain(current);
        if mask.contains(Mask::NSFullScreenWindowMask) {
            return;
        }
        mask.set(Mask::NSTitledWindowMask, style.titled);
        mask.set(Mask::NSClosableWindowMask, style.closable);
        mask.set(Mask::NSMiniaturizableWindowMask, style.minimizable);
        mask.set(Mask::NSResizableWindowMask, style.resizable);
        mask.set(Mask::NSFullSizeContentViewWindowMask, style.full_size_content);
        if mask.bits() == current {
            return;
        }
        let frame = NSWindow::frame(native);
        let title: id = msg_send![native.title(), copy];
        let responder: id = msg_send![native, firstResponder];
        native.setStyleMask_(mask);
        native.setTitle_(title);
        let _: () = msg_send![title, release];
        if responder == view {
            native.makeFirstResponder_(view);
        }
        // Keep the outside frame stable when the content gains or loses a titlebar.
        native.setFrame_display_(frame, YES);
    }
}

#[derive(Default)]
pub(super) enum CornerResize {
    #[default]
    Idle,
    Armed,
    Live(f64),
}

impl CornerResize {
    pub(super) fn mouse_down(window: &MacWindowState, event: id) -> Self {
        if window.applied_style.is_none_or(|style| style.size_constraints.aspect_ratio == 0.) {
            return Self::Idle;
        }
        let frame = unsafe { NSWindow::frame(window.native_window) };
        let point = unsafe { event.locationInWindow() };
        // AppKit confirms the resize in windowWillStartLiveResize. Use its
        // 16-point corner reach to keep ordinary edge drags on the native path.
        const CORNER_REACH: f64 = 16.;
        if (frame.size.width - point.x).abs() <= CORNER_REACH && point.y.abs() <= CORNER_REACH {
            Self::Armed
        } else {
            Self::Idle
        }
    }
}

pub(super) extern "C" fn window_will_start_live_resize(this: &Object, _: Sel, _: id) {
    let state = unsafe { get_window_state(this) };
    let mut window = state.lock();
    if !matches!(window.corner_resize, CornerResize::Armed) || window.is_fullscreen() {
        return;
    }
    let native = window.native_window;
    let aspect = unsafe { native.contentAspectRatio() };
    if aspect.width <= 0. || aspect.height <= 0. {
        return;
    }
    window.corner_resize = CornerResize::Live(aspect.width / aspect.height);
    drop(window);
    // AppKit's aspect lock can suppress a drag that only changes one dimension.
    // Suspend it while AppKit tracks this corner and supplies both dimensions.
    unsafe { native.setContentResizeIncrements_(NSSize::new(1., 1.)) };
}

pub(super) extern "C" fn window_will_resize(
    this: &Object,
    _: Sel,
    native: id,
    proposed: NSSize,
) -> NSSize {
    if unsafe { native.inLiveResize() } == NO {
        return proposed;
    }
    let state = unsafe { get_window_state(this) };
    let CornerResize::Live(ratio) = state.lock().corner_resize else { return proposed };
    if ratio == 0. {
        return proposed;
    }
    let origin = NSPoint::new(0., 0.);
    let content = unsafe { native.contentRectForFrameRect_(NSRect::new(origin, proposed)) };
    let size = corner_resize_size(content.size, ratio);
    unsafe { native.frameRectForContentRect_(NSRect::new(origin, size)) }.size
}

pub(super) extern "C" fn window_did_end_live_resize(this: &Object, _: Sel, _: id) {
    let state = unsafe { get_window_state(this) };
    let mut window = state.lock();
    let resize = mem::take(&mut window.corner_resize);
    let native = window.native_window;
    drop(window);
    if let CornerResize::Live(ratio) = resize
        && ratio > 0.
    {
        unsafe { native.setContentAspectRatio_(NSSize::new(ratio, 1.)) };
    }
}

fn corner_resize_size(proposed: NSSize, ratio: f64) -> NSSize {
    // Extend the frame to the pointer on the axis that needs more space at this ratio.
    let width = proposed.width.max(proposed.height * ratio);
    NSSize::new(width, width / ratio)
}

/// Changing AppKit's properties does not resize a window that already violates
/// the new constraints.
unsafe fn apply_size_constraints(
    state: &Arc<Mutex<MacWindowState>>,
    native: id,
    requested: MacWindowSizeConstraints,
    display: ConstraintDisplay,
) {
    let frame = unsafe { NSWindow::frame(native) };
    let current = unsafe { native.contentRectForFrameRect_(frame) }.size;
    let visible = display.visible;
    let available =
        visible.map_or(current, |visible| unsafe { native.contentRectForFrameRect_(visible) }.size);
    let pixel = 1. / f64::from(display.scale);
    let (minimum, maximum, ratio, target) = content_sizes(requested, current, available, pixel);
    let use_native_ratio = if let CornerResize::Live(live_ratio) = &mut state.lock().corner_resize {
        *live_ratio = ratio;
        false
    } else {
        true
    };
    unsafe {
        native.setContentMinSize_(minimum);
        native.setContentMaxSize_(maximum);
        if ratio > 0. && use_native_ratio {
            native.setContentAspectRatio_(NSSize::new(ratio, 1.));
        } else {
            // AppKit's resize increments and aspect ratio are mutually exclusive.
            native.setContentResizeIncrements_(NSSize::new(1., 1.));
        }
        if target.width != current.width || target.height != current.height {
            native.setContentSize_(target);
            // AppKit keeps the top-left corner and rounds the result to its pixel grid.
            // Fit that actual frame to the original screen only if the window changed size.
            if let Some(visible) = visible {
                let resized = NSWindow::frame(native);
                if resized.size.width != frame.size.width
                    || resized.size.height != frame.size.height
                {
                    let origin = visible_frame_origin(resized, visible);
                    if origin.x != resized.origin.x || origin.y != resized.origin.y {
                        native.setFrameOrigin_(origin);
                    }
                }
            }
        }
    }
}

fn visible_frame_origin(frame: NSRect, visible: NSRect) -> NSPoint {
    // If native chrome is too large to fit, keep the left edge and titlebar accessible.
    let rightmost = visible.origin.x + (visible.size.width - frame.size.width).max(0.);
    let highest = visible.origin.y + visible.size.height - frame.size.height;
    NSPoint::new(
        frame.origin.x.clamp(visible.origin.x, rightmost),
        frame.origin.y.clamp(visible.origin.y.min(highest), highest),
    )
}

fn content_sizes(
    requested: MacWindowSizeConstraints,
    current: NSSize,
    available: NSSize,
    pixel: f64,
) -> (NSSize, NSSize, f64, NSSize) {
    // FLT_MAX is AppKit's unconstrained content maximum.
    let maximum = |edge: f64| if edge == 0. { f64::from(f32::MAX) } else { edge.max(pixel) };
    let mut maximum = NSSize::new(maximum(requested.max.width), maximum(requested.max.height));
    // A guest-driven enlargement must fit on the display, even if its requested
    // minimum or aspect ratio is impossible there. User drags still go to AppKit.
    let room = NSSize::new(
        available.width.max(pixel).min(maximum.width),
        available.height.max(pixel).min(maximum.height),
    );
    let mut minimum =
        NSSize::new(requested.min.width.min(room.width), requested.min.height.min(room.height));
    let mut target = NSSize::new(
        current.width.clamp(minimum.width, maximum.width),
        current.height.clamp(minimum.height, maximum.height),
    );
    let mut ratio = f64::from(requested.aspect_ratio);
    if ratio > 0. {
        ratio = ratio.clamp(pixel / room.height, room.width / pixel);
        // Give AppKit the intersection with the aspect line on both axes. A drag
        // on either edge then reaches the same endpoint.
        maximum.width = maximum.width.min(maximum.height * ratio);
        maximum.height = maximum.height.min(maximum.width / ratio);
        let largest_width = room.width.min(room.height * ratio);
        minimum.width = minimum.width.max(minimum.height * ratio).min(largest_width);
        minimum.height = minimum.width / ratio;
        target.width = current
            .width
            .clamp(minimum.width.max(pixel).max(ratio * pixel).min(largest_width), largest_width);
        target.height = target.width / ratio;
    }
    (minimum, maximum, ratio, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_drag_follows_the_pointer_on_the_dominant_axis() {
        // Start at 600 by 400 and check outward and inward drags on both sides of the aspect line.
        for (width, height, expected) in [
            (690., 410., (690., 460.)),
            (610., 460., (690., 460.)),
            (570., 350., (570., 380.)),
            (510., 380., (570., 380.)),
            (600., 400., (600., 400.)),
        ] {
            let size = corner_resize_size(NSSize::new(width, height), 1.5);
            assert_eq!((size.width, size.height), expected);
        }
    }

    #[test]
    fn resized_frame_moves_only_as_far_as_the_visible_screen_requires() {
        // Include fractional point geometry and a second display left of the primary.
        let visible = NSRect::new(NSPoint::new(-1512., 31.), NSSize::new(1512., 950.));
        let origin = |x, y, width, height| {
            let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
            let point = visible_frame_origin(frame, visible);
            (point.x, point.y)
        };
        assert_eq!(origin(-1076., -22., 1286.5, 808.), (-1286.5, 31.));
        assert_eq!(origin(-1500., 80., 1286.5, 808.), (-1500., 80.));
        assert_eq!(origin(-1600., 910., 640., 560.), (-1512., 421.));
        assert_eq!(origin(-1400., 32., 1513., 951.), (-1512., 30.));
    }

    #[test]
    fn display_width_changes_recover_and_reclip_the_guest_minimum_at_the_same_scale() {
        // Refit the same 1200-point policy as one display expands and shrinks.
        let policy = MacWindowSizeConstraints { min: size(1200., 0.), ..Default::default() };
        let screen = |width| ConstraintDisplay {
            scale: 2.,
            visible: Some(NSRect::new(NSPoint::new(0., 0.), NSSize::new(width, 800.))),
        };
        let mut installed = screen(1024.);
        let fitted_min = |display: ConstraintDisplay| {
            let available = display.visible.unwrap().size;
            content_sizes(policy, available, available, 1. / f64::from(display.scale)).0.width
        };
        assert_eq!(fitted_min(installed), 1024.);
        for (width, expected) in [(1440., 1200.), (900., 900.), (1280., 1200.)] {
            let changed = screen(width);
            assert!(!installed.matches(changed));
            installed = changed;
            assert_eq!(fitted_min(installed), expected);
            assert!(installed.matches(screen(width)));
        }
    }

    #[test]
    fn fractional_limits_use_the_displays_backing_pixel_floor() {
        // A half-point on a 2× display can satisfy this ratio exactly.
        let requested = MacWindowSizeConstraints {
            min: size(0., 0.),
            max: size(320.5, 0.5),
            aspect_ratio: 641.,
        };
        let current = NSSize::new(640., 560.);
        let (_, max, ratio, target) = content_sizes(requested, current, current, 0.5);
        assert_eq!((max.width, max.height), (320.5, 0.5));
        assert_eq!((ratio, target.width, target.height), (641., 320.5, 0.5));
        let (_, _, ratio, target) = content_sizes(requested, current, current, 1.);
        assert_eq!((ratio, target.width, target.height), (320.5, 320.5, 1.));
    }

    #[test]
    fn content_limits_fit_the_current_window_and_the_display() {
        // Tighten a ratio from opposite edges, then move to a smaller screen.
        let requested = MacWindowSizeConstraints {
            min: size(0., 400.),
            max: size(600., 0.),
            aspect_ratio: 1.5,
        };
        let current = NSSize::new(640., 560.);
        let (min, max, ratio, target) =
            content_sizes(requested, current, NSSize::new(1200., 900.), 1.);
        assert_eq!((min.width, min.height), (600., 400.));
        assert_eq!((max.width, max.height), (600., 400.));
        assert_eq!((ratio, target.width, target.height), (1.5, 600., 400.));
        let (min, _, ratio, target) =
            content_sizes(requested, current, NSSize::new(900., 300.), 1.);
        assert_eq!((min.width, min.height), (450., 300.));
        assert_eq!((ratio, target.width, target.height), (1.5, 450., 300.));

        let unlocked = MacWindowSizeConstraints { aspect_ratio: 0., ..requested };
        let (min, max, ratio, target) =
            content_sizes(unlocked, current, NSSize::new(1200., 900.), 1.);
        assert_eq!((min.width, min.height), (0., 400.));
        assert_eq!((max.width, max.height), (600., f64::from(f32::MAX)));
        assert_eq!((ratio, target.width, target.height), (0., 600., 560.));
        let (_, _, ratio, target) = content_sizes(Default::default(), current, current, 1.);
        assert_eq!((ratio, target.width, target.height), (0., current.width, current.height));
    }
}
