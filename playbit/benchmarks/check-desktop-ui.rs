//! Fixed visual regression fixture, using real GPUI/Metal windows without a guest
//! engine or a user profile. Events stay in these windows; no global input is sent.
#![allow(clippy::unwrap_used, reason = "The visual fixture asserts expected platform operations.")]
#![allow(
    unsafe_code,
    unexpected_cfgs,
    reason = "The diagnostic reads its own AppKit window number for capture."
)]
use gpui::{prelude::*, *};
use objc::{msg_send, runtime::Object, sel, sel_impl};
use playbit::runtime::MenuEntry;
use playbit::ui::desktop::{
    controls::{self, Control, Kind, Properties},
    popup, zoom,
};
use playbit_api::ui::{ButtonStyle, Theme};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{borrow::Cow, rc::Rc, time::Duration};
struct Fixture {
    buttons: Vec<Entity<Control>>,
    session: Entity<Control>,
}
impl Render for Fixture {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        zoom::apply(window, cx);
        let ui = zoom::length;
        div()
            .size_full()
            .relative()
            .bg(rgba(0xecececff))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(ui(100.))
                    .h(ui(16.))
                    .window_control_area(WindowControlArea::Drag),
            )
            .p(ui(20.))
            .flex()
            .flex_col()
            .gap(ui(12.))
            .children(self.buttons.clone())
            .child(div().absolute().right(ui(4.)).top(ui(4.)).child(self.session.clone()))
    }
}
fn action(id: u32, label: &str, disabled: bool) -> MenuEntry {
    MenuEntry::Action {
        icon: Some(playbit_api::ui::MenuIcon::Copy.into()),
        id,
        label: label.into(),
        disabled,
        checked: None,
        destructive: false,
    }
}
fn native_id(handle: AnyWindowHandle, cx: &mut App) -> i64 {
    handle
        .update(cx, |_, window, _| {
            let RawWindowHandle::AppKit(h) =
                HasWindowHandle::window_handle(window).unwrap().as_raw()
            else {
                panic!()
            };
            unsafe {
                let view = h.ns_view.as_ptr() as *mut Object;
                let window: *mut Object = msg_send![view, window];
                let id: i64 = msg_send![window, windowNumber];
                id
            }
        })
        .unwrap()
}
fn native_window(handle: AnyWindowHandle, cx: &mut App) -> *mut Object {
    handle
        .update(cx, |_, window, _| {
            let RawWindowHandle::AppKit(h) =
                HasWindowHandle::window_handle(window).unwrap().as_raw()
            else {
                panic!()
            };
            unsafe { msg_send![h.ns_view.as_ptr() as *mut Object, window] }
        })
        .unwrap()
}
fn check_menu_focus(owner: *mut Object, menu: *mut Object, expected_key: bool) {
    unsafe {
        let parent: *mut Object = msg_send![menu, parentWindow];
        let level: isize = msg_send![menu, level];
        let owner_level: isize = msg_send![owner, level];
        assert_eq!(parent, owner, "menu must be attached to its current owner");
        assert_eq!(level, owner_level, "menu must share its owner's window level");
        let owner_key: bool = msg_send![owner, isKeyWindow];
        let can_key: bool = msg_send![menu, canBecomeKeyWindow];
        let can_main: bool = msg_send![menu, canBecomeMainWindow];
        let style: usize = msg_send![menu, styleMask];
        assert_eq!(owner_key, expected_key, "menu must preserve owner key status");
        assert!(!can_key && !can_main, "menu must reject key/main status");
        println!("Native panel style={style:#x}, canBecomeKey={can_key}, canBecomeMain={can_main}");
        let _: () = msg_send![menu, makeKeyWindow];
        let owner_key: bool = msg_send![owner, isKeyWindow];
        assert_eq!(
            owner_key, expected_key,
            "even a menu click must preserve the owner's key status"
        );
    }
}
fn check_menu_detached(menu: *mut Object) {
    unsafe {
        let parent: *mut Object = msg_send![menu, parentWindow];
        let visible: bool = msg_send![menu, isVisible];
        assert!(parent.is_null() && !visible, "dismissed menu must be hidden and detached");
    }
}
struct OtherOwner;
impl Render for OtherOwner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgba(0xecececff))
    }
}
fn native_key(owner: *mut Object, characters: &str, code: u16) {
    use cocoa::foundation::NSString;
    unsafe {
        let string = NSString::alloc(std::ptr::null_mut()).init_str(characters);
        let number: i64 = msg_send![owner, windowNumber];
        let event: *mut Object = msg_send![objc::class!(NSEvent), keyEventWithType: 10usize
            location: cocoa::foundation::NSPoint::new(0.,0.) modifierFlags: (if (123..=126).contains(&code) { (1usize << 21) | (1usize << 23) } else { 0usize }) timestamp: 0f64
            windowNumber: number context: std::ptr::null_mut::<Object>() characters: string
            charactersIgnoringModifiers: string isARepeat: false keyCode: code];
        let app: *mut Object = msg_send![objc::class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, sendEvent: event];
        let _: () = msg_send![string, release];
    }
}
fn native_drag_allowed(view: *mut Object, x: f64, y: f64) -> bool {
    // Inspect the native exclusions that AppKit publishes to WindowServer,
    // without relying on the current pointer position or dispatching an event.
    unsafe {
        let children: *mut Object = msg_send![view, subviews];
        let count: usize = msg_send![children, count];
        for index in 0..count {
            let child: *mut Object = msg_send![children, objectAtIndex: index];
            let exclusion: bool =
                msg_send![child, isKindOfClass: objc::class!(GPUINonDraggableView)];
            if exclusion {
                let frame: cocoa::foundation::NSRect = msg_send![child, frame];
                let position = cocoa::foundation::NSPoint::new(x, 160. - y);
                if position.x >= frame.origin.x
                    && position.x < frame.origin.x + frame.size.width
                    && position.y >= frame.origin.y
                    && position.y < frame.origin.y + frame.size.height
                {
                    return false;
                }
            }
        }
        true
    }
}
fn native_mouse(window: *mut Object, kind: usize, x: f64, y: f64) {
    unsafe {
        let number: i64 = msg_send![window, windowNumber];
        let event: *mut Object = msg_send![objc::class!(NSEvent), mouseEventWithType: kind
            location: cocoa::foundation::NSPoint::new(x, 160. - y)
            modifierFlags: 1usize << 20 timestamp: 0f64 windowNumber: number
            context: std::ptr::null_mut::<Object>() eventNumber: 1i64 clickCount: 1i64 pressure: 1f32];
        let _: () = msg_send![window, sendEvent: event];
    }
}
fn capture(handle: AnyWindowHandle, name: &str, cx: &mut App) {
    let id = native_id(handle, cx);
    let path = format!("/private/tmp/desktop-ui-render-probe/{name}.png");
    assert!(
        std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-l", &id.to_string(), &path])
            .status()
            .unwrap()
            .success()
    );
    println!("captured {path}");
}
fn move_to(menu: AnyWindowHandle, x: f32, y: f32, cx: &mut App) {
    menu.update(cx, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: point(px(x), px(y)),
                pressed_button: None,
                modifiers: Modifiers::default(),
            }),
            cx,
        );
    })
    .unwrap();
}
fn key(menu: AnyWindowHandle, key: &str, cx: &mut App) {
    menu.update(cx, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke: Keystroke::parse(key).unwrap(),
                is_held: false,
            }),
            cx,
        );
    })
    .unwrap();
}
fn pointer(menu: AnyWindowHandle, y: f32, down: bool, cx: &mut App) {
    menu.update(cx, |_, window, cx| {
        let position = point(px(40.), px(y));
        let event = if down {
            PlatformInput::MouseDown(MouseDownEvent {
                position,
                button: MouseButton::Left,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            })
        } else {
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                button: MouseButton::Left,
                modifiers: Modifiers::default(),
                click_count: 1,
            })
        };
        window.dispatch_event(event, cx);
    })
    .unwrap();
}
fn check_pressed_pixels() {
    let actual =
        image::open("/private/tmp/desktop-ui-render-probe/pressed.png").unwrap().to_rgba8();
    assert_eq!(actual.width(), 440, "this pixel fixture requires a 2× display");
    // Sample clear interior strips, away from text/icons and antialiased contours.
    // The revised design has uniform pressed fills with no inset shading.
    for (name, x, top, bottom) in [("button", 100, 42, 47), ("session", 219, 258, 262)] {
        let fill = actual.get_pixel(x, top);
        for y in top..=bottom {
            assert_eq!(actual.get_pixel(x, y), fill, "{name} has inset shading at y={y}");
        }
    }
    println!("2× pressed button and session fills have no inner shadow");
}
fn main() {
    std::fs::create_dir_all("/private/tmp/desktop-ui-render-probe").unwrap();
    Application::new().with_assets(controls::Assets).run(|cx| {
        zoom::install(cx);
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../assets/InterVariable.ttf"))])
            .unwrap();
        let session_actions = Rc::new(std::cell::Cell::new(0));
        let session_received = session_actions.clone();
        let owner = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(100.), px(100.)),
                        size(px(220.), px(160.)),
                    ))),
                    focus: false,
                    titlebar: None,
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|cx| {
                        let mut buttons: Vec<Entity<Control>> =
                            [ButtonStyle::Default, ButtonStyle::Primary, ButtonStyle::Destructive]
                                .into_iter()
                                .map(|style| {
                                    cx.new(|cx| {
                                        let mut control = Control::new(
                                            1,
                                            Properties {
                                                kind: Kind::Button(style),
                                                label: "Pressed",
                                                disabled: false,
                                                theme: &Theme::light(),
                                            },
                                            Rc::new(|_, _| {}),
                                            cx,
                                        );
                                        control.set_popup_open(true, cx);
                                        control
                                    })
                                })
                                .collect();
                        buttons.push(cx.new(|cx| {
                            let mut control = Control::new(
                                2,
                                Properties {
                                    kind: Kind::Button(ButtonStyle::Default),
                                    label: "",
                                    disabled: false,
                                    theme: &Theme::light(),
                                },
                                Rc::new(|_, _| {}),
                                cx,
                            )
                            .toolbar_icon("session-icon");
                            control.set_popup_open(true, cx);
                            control
                        }));
                        let session = cx.new(|cx| {
                            Control::new(
                                3,
                                Properties {
                                    kind: Kind::Button(ButtonStyle::Default),
                                    label: "",
                                    disabled: false,
                                    theme: &Theme::light(),
                                },
                                Rc::new(move |_, _| {
                                    session_received.set(session_received.get() + 1)
                                }),
                                cx,
                            )
                            .toolbar_icon("session-icon")
                        });
                        Fixture { buttons, session }
                    })
                },
            )
            .unwrap();
        let actions = Rc::new(std::cell::Cell::new(0u32));
        let received = actions.clone();
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(250)).await;
            for (x, y, expected) in [(20., 8., true), (180., 8., false), (40., 34., false)] {
                let view = cx
                    .update(|cx| {
                        move_to(owner.into(), x, y, cx);
                        owner
                            .update(cx, |_, window, _| {
                                let RawWindowHandle::AppKit(h) =
                                    HasWindowHandle::window_handle(window).unwrap().as_raw()
                                else {
                                    panic!()
                                };
                                h.ns_view.as_ptr() as *mut Object
                            })
                            .unwrap()
                    })
                    .unwrap();
                let can_drag = native_drag_allowed(view, x as f64, y as f64);
                assert_eq!(can_drag, expected, "native drag permission at ({x},{y})");
            }
            println!(
                "Native WindowServer drag geometry: titlebar=true, session slot=false, \
                 control=false"
            );
            cx.update(|cx| capture(owner.into(), "pressed", cx)).unwrap();
            check_pressed_pixels();
            let native_owner = cx.update(|cx| native_window(owner.into(), cx)).unwrap();
            // Leave the cached hover over the draggable titlebar, then press the
            // actual titlebar button without a preceding mouse-move event.
            cx.update(|cx| move_to(owner.into(), 20., 8., cx)).unwrap();
            let frame_before: cocoa::foundation::NSRect = unsafe { msg_send![native_owner, frame] };
            native_mouse(native_owner, 1, 199., 16.);
            let movable: bool = unsafe { msg_send![native_owner, isMovable] };
            assert!(movable, "button exclusion must not toggle window movability on mouse-down");
            native_mouse(native_owner, 6, 140., 60.);
            native_mouse(native_owner, 2, 140., 60.);
            Timer::after(Duration::from_millis(50)).await;
            let frame_after: cocoa::foundation::NSRect = unsafe { msg_send![native_owner, frame] };
            assert_eq!(
                (
                    frame_before.origin.x,
                    frame_before.origin.y,
                    frame_before.size.width,
                    frame_before.size.height
                ),
                (
                    frame_after.origin.x,
                    frame_after.origin.y,
                    frame_after.size.width,
                    frame_after.size.height
                ),
                "canceling a session press must not move the native window"
            );
            assert_eq!(session_actions.get(), 0, "dragging out cancels the action");
            let movable: bool = unsafe { msg_send![native_owner, isMovable] };
            assert!(movable, "release preserves configured window movability");
            native_mouse(native_owner, 1, 199., 16.);
            native_mouse(native_owner, 2, 199., 16.);
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(session_actions.get(), 1, "ordinary session click still dispatches once");
            println!(
                "Native titlebar session down/drag-out/up: frame unchanged, action canceled; \
                 ordinary click dispatched once"
            );
            let owner_handle: AnyWindowHandle = owner.into();
            // Menu-only runs keep unrelated titlebar timing out of the cohort.
            if !std::env::args().any(|argument| argument == "--menus-only") {
                let mut drag_samples = Vec::with_capacity(1000);
                for _ in 0..1000 {
                    let start = std::time::Instant::now();
                    native_mouse(native_owner, 1, 199., 16.);
                    native_mouse(native_owner, 6, 140., 60.);
                    native_mouse(native_owner, 2, 140., 60.);
                    owner_handle
                        .update(cx, |_, window, cx| {
                            let _ = window.draw(cx);
                        })
                        .unwrap();
                    drag_samples.push(start.elapsed().as_secs_f64() * 1000.);
                }
                assert_eq!(session_actions.get(), 1, "all canceling gestures must stay canceled");
                drag_samples.sort_by(f64::total_cmp);
                println!(
                    "native titlebar cancellation + CPU draw ms n=1000 p50={} p95={} p99={} max={}",
                    drag_samples[499], drag_samples[949], drag_samples[989], drag_samples[999]
                );
            }

            // Preserve whichever key status this application has, without activating it.
            let expected_key: bool = unsafe { msg_send![native_owner, isKeyWindow] };
            println!(
                "Native owner key status before opening: {expected_key} (application not activated)"
            );
            let request = owner
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(70.), px(60.)),
                        vec![
                            action(1, "First", false),
                            MenuEntry::Submenu {
                                icon: None,
                                label: "More".into(),
                                disabled: false,
                                items: vec![action(2, "Child", false), action(3, "Disabled", true)],
                            },
                            action(4, "Last", false),
                        ],
                        Rc::new(move |id, _| {
                            println!("action {id}");
                            received.set(received.get() + 1);
                        }),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| popup::open(request, cx)).unwrap();
            Timer::after(Duration::from_millis(150)).await;
            let menu = cx
                .update(|cx| {
                    *cx.windows()
                        .iter()
                        .find(|h| **h != owner.into())
                        .expect("menu stays open without activating the app")
                })
                .unwrap();
            let native_menu = cx.update(|cx| native_window(menu, cx)).unwrap();
            check_menu_focus(native_owner, native_menu, expected_key);
            println!("New child menu preserves owner key window and rejects key/main status");
            cx.update(|cx| capture(owner.into(), "context-with-owner", cx)).unwrap();
            cx.update(|cx| {
                capture(menu, "context", cx);
                move_to(menu, 40., 54., cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(300)).await;
            cx.update(|cx| capture(menu, "hover-cascade", cx)).unwrap();
            native_key(native_owner, "\u{f703}", 124);
            Timer::after(Duration::from_millis(100)).await;
            cx.update(|cx| capture(menu, "keyboard-child", cx)).unwrap();
            native_key(native_owner, "\u{f702}", 123);
            Timer::after(Duration::from_millis(100)).await;
            cx.update(|cx| capture(menu, "keyboard-parent", cx)).unwrap();
            cx.update(|cx| key(menu, "right", cx)).unwrap();
            Timer::after(Duration::from_millis(100)).await;
            let id = cx.update(|cx| native_id(menu, cx)).unwrap();
            let recording_path = "/private/tmp/desktop-ui-render-probe/submenu-resize.mov";
            if std::path::Path::new(recording_path).exists() {
                std::fs::remove_file(recording_path).unwrap();
            }
            let mut recording = std::process::Command::new("/usr/sbin/screencapture")
                .args(["-x", "-o", "-v", "-V", "7", "-l", &id.to_string(), recording_path])
                .spawn()
                .unwrap();
            Timer::after(Duration::from_millis(500)).await;
            for _ in 0..20 {
                cx.update(|cx| key(menu, "left", cx)).unwrap();
                Timer::after(Duration::from_millis(150)).await;
                cx.update(|cx| key(menu, "right", cx)).unwrap();
                Timer::after(Duration::from_millis(150)).await;
            }
            Timer::after(Duration::from_millis(700)).await;
            assert!(recording.wait().unwrap().success());
            println!("recorded 20 submenu open/close cycles");
            cx.update(|cx| key(menu, "left", cx)).unwrap();
            Timer::after(Duration::from_millis(100)).await;

            cx.update(|cx| {
                let mut samples = Vec::with_capacity(1000);
                for index in 0..1000 {
                    let start = std::time::Instant::now();
                    move_to(menu, 40., if index % 2 == 0 { 34. } else { 74. }, cx);
                    menu.update(cx, |_, window, cx| {
                        let _ = window.draw(cx);
                    })
                    .unwrap();
                    samples.push(start.elapsed().as_secs_f64() * 1000.);
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "row dispatch + synchronous CPU draw ms n=1000 p50={} p95={} p99={} max={}",
                    samples[499], samples[949], samples[989], samples[999]
                );
                pointer(menu, 34., true, cx);
                pointer(menu, 74., false, cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(actions.get(), 0, "release on a different row must cancel");
            cx.update(|cx| {
                pointer(menu, 34., true, cx);
                pointer(menu, 34., false, cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(50)).await;
            assert_eq!(actions.get(), 1, "same enabled row dispatches exactly once");
            check_menu_detached(native_menu);
            println!("pointer cancellation and matching down/up passed");
            let native_actions = Rc::new(std::cell::Cell::new(0));
            let received = native_actions.clone();
            let request = owner
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(70.), px(60.)),
                        vec![action(1, "First", false), action(2, "Second", false)],
                        Rc::new(move |id, _| received.set(id)),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| popup::open(request, cx)).unwrap();
            Timer::after(Duration::from_millis(150)).await;
            check_menu_focus(native_owner, native_menu, expected_key);
            native_key(native_owner, "s", 1);
            Timer::after(Duration::from_millis(30)).await;
            assert_eq!(native_actions.get(), 0, "type-ahead must not activate");
            let mut samples = Vec::with_capacity(1000);
            for i in 0..1000 {
                let start = std::time::Instant::now();
                let (character, code) =
                    if i % 2 == 0 { ("\u{f700}", 126) } else { ("\u{f701}", 125) };
                native_key(native_owner, character, code);
                menu.update(cx, |_, window, cx| {
                    let _ = window.draw(cx);
                })
                .unwrap();
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "native menu arrow + CPU draw ms n=1000 p50={} p95={} p99={} max={}",
                samples[499], samples[949], samples[989], samples[999]
            );
            native_key(native_owner, "\u{f700}", 126);
            Timer::after(Duration::from_millis(30)).await;
            native_key(native_owner, "\r", 36);
            Timer::after(Duration::from_millis(100)).await;
            assert_eq!(
                native_actions.get(),
                1,
                "type-ahead selects Second, Up selects First, Return activates First"
            );
            unsafe {
                let key: bool = msg_send![native_owner, isKeyWindow];
                assert_eq!(key, expected_key);
            }
            check_menu_detached(native_menu);
            println!(
                "Recycled child menu preserves owner key status; \
                 native type-ahead/Up/Return dispatch once through modal monitor"
            );
            // Exercise every navigation key through NSApplication and the modal
            // monitor, with real arrow modifier flags, rather than GPUI injection.
            let received = native_actions.clone();
            let request = owner
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(70.), px(60.)),
                        vec![
                            action(1, "First", false),
                            MenuEntry::Submenu {
                                icon: None,
                                label: "More".into(),
                                disabled: false,
                                items: vec![action(2, "Child", false)],
                            },
                            action(3, "Last", false),
                        ],
                        Rc::new(move |id, _| received.set(id)),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| popup::open(request, cx)).unwrap();
            Timer::after(Duration::from_millis(100)).await;
            for (characters, code) in [
                ("\u{f701}", 125),
                ("\u{f701}", 125),
                ("\u{f703}", 124),
                ("\u{f702}", 123),
                ("\u{f703}", 124),
                ("\r", 36),
            ] {
                native_key(native_owner, characters, code);
                Timer::after(Duration::from_millis(30)).await;
            }
            assert_eq!(
                native_actions.get(),
                2,
                "Down/Down/Right/Left/Right/Return activates Child"
            );
            let request = owner
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(70.), px(60.)),
                        vec![action(1, "First", false)],
                        Rc::new(|_, _| panic!("Escape must not activate")),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| popup::open(request, cx)).unwrap();
            Timer::after(Duration::from_millis(100)).await;
            native_key(native_owner, "\u{1b}", 53);
            Timer::after(Duration::from_millis(100)).await;
            assert!(!popup::is_modal_active(), "native Escape closes the menu");
            println!("Native Down/Right/Left/Return and Escape passed");
            cx.update(|cx| zoom::configure(12_345, std::sync::Arc::new(|_| Ok(())), cx)).unwrap();
            for (units, delta) in [(12_345, 0), (20_000, 100), (5_000, -100)] {
                cx.update(|cx| {
                    zoom::change(cx, delta);
                    let scale = zoom::factor(cx);
                    owner
                        .update(cx, |_, window, _| {
                            window.resize(size(px(220. * scale), px(160. * scale)));
                        })
                        .unwrap();
                })
                .unwrap();
                Timer::after(Duration::from_millis(100)).await;
                cx.update(|cx| capture(owner.into(), &format!("zoom-{units}-controls"), cx))
                    .unwrap();
                for anchor in ["context", "above", "below"] {
                    let selected = native_actions.clone();
                    selected.set(0);
                    let request = owner
                        .update(cx, |fixture, window, cx| {
                            let scale = zoom::factor(cx);
                            let mut first = action(1, "First", false);
                            if let MenuEntry::Action { checked, .. } = &mut first {
                                *checked = Some(true);
                            }
                            let items = vec![
                                first,
                                MenuEntry::Separator,
                                MenuEntry::Submenu {
                                    icon: None,
                                    label: "More".into(),
                                    disabled: false,
                                    items: vec![
                                        action(2, "Child", false),
                                        action(3, "Disabled", true),
                                    ],
                                },
                                action(4, "Last", false),
                            ];
                            let activate: controls::Activate =
                                Rc::new(move |id, _| selected.set(id));
                            if anchor == "context" {
                                popup::Request::context(
                                    window,
                                    point(px(70. * scale), px(60. * scale)),
                                    items,
                                    activate,
                                    cx,
                                )
                            } else {
                                let request = popup::Request::button(
                                    window,
                                    Bounds::new(
                                        point(px(20. * scale), px(20. * scale)),
                                        size(px(100. * scale), px(24. * scale)),
                                    ),
                                    "First".into(),
                                    items,
                                    activate,
                                    fixture.buttons[0].downgrade(),
                                    cx,
                                );
                                if anchor == "above" {
                                    request.above_button()
                                } else {
                                    request.below_button()
                                }
                            }
                        })
                        .unwrap();
                    cx.update(|cx| popup::open(request, cx)).unwrap();
                    Timer::after(Duration::from_millis(100)).await;
                    cx.update(|cx| capture(menu, &format!("zoom-{units}-{anchor}"), cx)).unwrap();
                    native_key(native_owner, "m", 46);
                    native_key(native_owner, "\u{f703}", 124);
                    Timer::after(Duration::from_millis(100)).await;
                    cx.update(|cx| capture(menu, &format!("zoom-{units}-{anchor}-child"), cx))
                        .unwrap();
                    native_key(native_owner, "\r", 36);
                    Timer::after(Duration::from_millis(50)).await;
                    assert_eq!(
                        native_actions.get(),
                        2,
                        "scaled {anchor} submenu action at {units}"
                    );
                }
            }
            let request = owner
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(40.), px(40.)),
                        vec![action(1, "First", false)],
                        Rc::new(|_, _| panic!("zoom must cancel without activating")),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| popup::open(request, cx)).unwrap();
            Timer::after(Duration::from_millis(50)).await;
            cx.update(|cx| key(menu, "cmd-=", cx)).unwrap();
            Timer::after(Duration::from_millis(50)).await;
            cx.update(|cx| assert_eq!(zoom::factor(cx), 0.75)).unwrap();
            assert!(!popup::is_modal_active(), "zoom cancels the menu's old hit regions");
            println!(
                "50.00%, 123.45%, 200.00% controls and all native popup anchors/submenus \
                 passed; live menu zoom cancels without activation"
            );
            // Exercise deferred cancellation, retargeting and owner closure on
            // the same retained native panel, with no app activation.
            let request = owner
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(70.), px(60.)),
                        vec![action(1, "First", false)],
                        Rc::new(|_, _| panic!("lifecycle checks must not activate")),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| {
                popup::open(request.clone(), cx);
                popup::dismiss(cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(30)).await;
            check_menu_detached(native_menu);
            let other = cx
                .update(|cx| {
                    cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                                point(px(400.), px(100.)),
                                size(px(220.), px(160.)),
                            ))),
                            focus: false,
                            ..Default::default()
                        },
                        |_, cx| cx.new(|_| OtherOwner),
                    )
                    .unwrap()
                })
                .unwrap();
            let other_native = cx.update(|cx| native_window(other.into(), cx)).unwrap();
            let other_key: bool = unsafe { msg_send![other_native, isKeyWindow] };
            let other_request = other
                .update(cx, |_, window, cx| {
                    popup::Request::context(
                        window,
                        point(px(210.), px(130.)),
                        vec![action(1, "Menu outside its owner", false)],
                        Rc::new(|_, _| panic!("owner closure must not activate")),
                        cx,
                    )
                })
                .unwrap();
            cx.update(|cx| popup::open(other_request, cx)).unwrap();
            Timer::after(Duration::from_millis(100)).await;
            check_menu_focus(other_native, native_menu, other_key);
            cx.update(|cx| capture(other.into(), "other-owner-overflow", cx)).unwrap();
            other.update(cx, |_, window, _| window.remove_window()).unwrap();
            Timer::after(Duration::from_millis(50)).await;
            assert!(!popup::is_modal_active(), "closing the owner must dismiss the menu");
            check_menu_detached(native_menu);
            let mut open_samples = Vec::with_capacity(1000);
            let mut close_samples = Vec::with_capacity(1000);
            for _ in 0..1000 {
                let start = std::time::Instant::now();
                cx.update(|cx| popup::open(request.clone(), cx)).unwrap();
                // Include the deferred native attachment and one event-loop
                // turn, then draw both the owner and menu. No GPU fence or
                // physical-presentation claim is made by this CPU boundary.
                Timer::after(Duration::from_millis(1)).await;
                owner_handle
                    .update(cx, |_, window, cx| {
                        let _ = window.draw(cx);
                    })
                    .unwrap();
                menu.update(cx, |_, window, cx| {
                    let _ = window.draw(cx);
                })
                .unwrap();
                let visible: bool = unsafe { msg_send![native_menu, isVisible] };
                assert!(visible, "menu must be attached before readiness is sampled");
                open_samples.push(start.elapsed().as_secs_f64() * 1000.);
                let start = std::time::Instant::now();
                cx.update(popup::dismiss).unwrap();
                check_menu_detached(native_menu);
                close_samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            for (name, samples) in [
                ("menu open + attach + owner/menu CPU draw", &mut open_samples),
                ("menu close + detach", &mut close_samples),
            ] {
                samples.sort_by(f64::total_cmp);
                println!(
                    "{name} ms n=1000 p50={} p95={} p99={} max={}",
                    samples[499], samples[949], samples[989], samples[999]
                );
            }
            println!(
                "Deferred cancellation, owner retargeting, overflow, \
                 owner closure and 1000 reuse cycles passed"
            );
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
