//! Process-local scrollbar interaction, capture, and CPU readiness diagnostic.
#![allow(clippy::unwrap_used, reason = "Diagnostic fixtures assert successful operations.")]
#![allow(unsafe_code, unexpected_cfgs, reason = "Read our own AppKit window ID for capture.")]
use gpui::{prelude::*, *};
use objc::{msg_send, runtime::Object, sel, sel_impl};
use playbit::ui::desktop::{controls, scrollbar::Scrollbars, zoom};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};
struct Fixture {
    bars: [Entity<Scrollbars>; 2],
    focus: FocusHandle,
    extent: Size<Pixels>,
}
impl Render for Fixture {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().bg(rgba(0xecececff)).track_focus(&self.focus).children(
            self.bars.iter().enumerate().map(|(i, bars)| {
                let body = div()
                    .flex_none()
                    .w(self.extent.width)
                    .h(self.extent.height)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .children((0..40).map(|row| {
                        div()
                            .flex_none()
                            .h(px(25.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .w_full()
                            .when(i == 1, |d| {
                                d.bg(rgba(if row % 2 == 0 { 0x66aaccff } else { 0xe6bd88ff }))
                            })
                            .child(format!(
                                "Row {row:02} — drag either scrollbar; click its track to jump."
                            ))
                    }));
                div()
                    .relative()
                    .w(px(330.))
                    .h_full()
                    .child(
                        div()
                            .id(("viewport", i))
                            .size_full()
                            .overflow_scroll()
                            .rounded(zoom::pixels(window, 16.))
                            .track_scroll(&bars.read(cx).handle())
                            .child(body),
                    )
                    .child(Scrollbars::overlay(bars))
            }),
        )
    }
}
fn event(handle: WindowHandle<Fixture>, input: PlatformInput, cx: &mut App) {
    AnyWindowHandle::from(handle)
        .update(cx, |_, w, cx| {
            w.dispatch_event(input, cx);
            let _ = w.draw(cx);
        })
        .unwrap();
}
fn movement(handle: WindowHandle<Fixture>, x: f32, y: f32, pressed: bool, cx: &mut App) {
    event(
        handle,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: point(px(x), px(y)),
            pressed_button: pressed.then_some(MouseButton::Left),
            modifiers: Default::default(),
        }),
        cx,
    );
}
fn button(handle: WindowHandle<Fixture>, x: f32, y: f32, down: bool, cx: &mut App) {
    let position = point(px(x), px(y));
    event(
        handle,
        if down {
            PlatformInput::MouseDown(MouseDownEvent {
                position,
                button: MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            })
        } else {
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                button: MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
            })
        },
        cx,
    );
}
fn capture(handle: WindowHandle<Fixture>, name: &str, cx: &mut App) {
    let id = handle
        .update(cx, |_, w, _| {
            let RawWindowHandle::AppKit(h) = w.window_handle().unwrap().as_raw() else { panic!() };
            unsafe {
                let v = h.ns_view.as_ptr() as *mut Object;
                let n: *mut Object = msg_send![v, window];
                let id: i64 = msg_send![n, windowNumber];
                id
            }
        })
        .unwrap();
    assert!(
        std::process::Command::new("/usr/sbin/screencapture")
            .args([
                "-x",
                "-o",
                "-l",
                &id.to_string(),
                &format!("{}/{name}.png", capture_directory())
            ])
            .status()
            .unwrap()
            .success()
    );
}
fn capture_directory() -> String {
    std::env::var("SCROLLBAR_CAPTURE_DIR")
        .unwrap_or_else(|_| "/private/tmp/archipelago-desktop-ui-review4".into())
}
fn main() {
    std::fs::create_dir_all(capture_directory()).unwrap();
    Application::new().with_assets(controls::Assets).run(|cx| {
        println!("auto_hide_scrollbars={}", cx.should_auto_hide_scrollbars());
        zoom::install(cx);
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../assets/InterVariable.ttf"))])
            .unwrap();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(100.), px(100.)),
                        size(px(660.), px(336.)),
                    ))),
                    titlebar: None,
                    focus: false,
                    ..Default::default()
                },
                |w, cx| {
                    cx.new(|cx| {
                        let focus = cx.focus_handle();
                        focus.focus(w);
                        Fixture {
                            bars: [
                                cx.new(|_| Scrollbars::new(false)),
                                cx.new(|_| Scrollbars::new(true)),
                            ],
                            focus,
                            extent: size(px(620.), px(1000.)),
                        }
                    })
                },
            )
            .unwrap();
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(200)).await;
            cx.update(|cx| capture(window, "scrollbars-idle-2x", cx)).unwrap();
            cx.update(|cx| {
                event(
                    window,
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: point(px(647.), px(150.)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
                        ..Default::default()
                    }),
                    cx,
                )
            })
            .unwrap();
            window
                .update(cx, |root, _, cx| {
                    assert_eq!(
                        root.bars[1].read(cx).handle().offset().y,
                        px(-40.),
                        "wheel passes through rail to its viewport"
                    )
                })
                .unwrap();
            cx.update(|cx| {
                movement(window, 317., 200., false, cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(50)).await;
            cx.update(|cx| capture(window, "scrollbars-hover-2x", cx)).unwrap();
            cx.update(|cx| button(window, 317., 210., true, cx)).unwrap();
            Timer::after(Duration::from_millis(50)).await;
            cx.update(|cx| capture(window, "scrollbars-pressed-2x", cx)).unwrap();
            window
                .update(cx, |root, _, cx| {
                    let h = root.bars[0].read(cx).handle();
                    assert!(h.offset().y < px(-300.), "track jumps to click");
                })
                .unwrap();
            cx.update(|cx| {
                movement(window, 200., 450., true, cx);
                capture(window, "scrollbars-drag-outside-2x", cx);
                button(window, 200., 450., false, cx);
            })
            .unwrap();
            window
                .update(cx, |root, w, cx| {
                    let h = root.bars[0].read(cx).handle();
                    assert_eq!(
                        h.offset().y,
                        -h.max_offset().height,
                        "drag outside reaches endpoint"
                    );
                    assert!(root.focus.is_focused(w), "scrollbar preserves focus");
                })
                .unwrap();
            cx.update(|cx| {
                movement(window, 100., 317., false, cx);
                button(window, 100., 317., true, cx);
                button(window, 100., 317., false, cx);
            })
            .unwrap();
            window
                .update(cx, |root, _, cx| {
                    assert!(
                        root.bars[0].read(cx).handle().offset().x < px(0.),
                        "horizontal track scrolls"
                    )
                })
                .unwrap();
            window
                .update(cx, |root, w, cx| {
                    root.bars[1].read(cx).handle().set_offset(point(px(0.), px(0.)));
                    w.refresh();
                })
                .unwrap();
            for (delta, phase) in [(200., TouchPhase::Started), (200., TouchPhase::Moved)] {
                cx.update(|cx| {
                    event(
                        window,
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: point(px(500.), px(150.)),
                            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                            touch_phase: phase,
                            is_gesture: true,
                            ..Default::default()
                        }),
                        cx,
                    )
                })
                .unwrap();
                Timer::after(Duration::from_millis(16)).await;
            }
            window
                .update(cx, |root, _, cx| {
                    let h = root.bars[1].read(cx).handle();
                    assert_eq!(h.offset().y, px(0.));
                    assert!(h.visual_offset().y > px(0.));
                    println!("native elastic pull={}dp", f32::from(h.visual_offset().y));
                })
                .unwrap();
            cx.update(|cx| capture(window, "scrollbars-elastic-pull-2x", cx)).unwrap();
            cx.update(|cx| {
                event(
                    window,
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: point(px(500.), px(150.)),
                        touch_phase: TouchPhase::Ended,
                        is_gesture: true,
                        ..Default::default()
                    }),
                    cx,
                )
            })
            .unwrap();
            Timer::after(Duration::from_millis(1200)).await;
            window
                .update(cx, |root, _, cx| {
                    let h = root.bars[1].read(cx).handle();
                    assert_eq!(h.visual_offset(), h.offset(), "native rebound settles completely");
                })
                .unwrap();
            cx.update(|cx| capture(window, "scrollbars-elastic-settled-2x", cx)).unwrap();
            let mut wheel_samples = Vec::new();
            window
                .update(cx, |root, w, cx| {
                    root.bars[1].read(cx).handle().set_offset(point(px(0.), px(-300.)));
                    w.refresh();
                })
                .unwrap();
            for i in 0..1000 {
                let start = Instant::now();
                cx.update(|cx| {
                    event(
                        window,
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: point(px(500.), px(150.)),
                            delta: ScrollDelta::Pixels(point(
                                px(0.),
                                px(if i % 2 == 0 { 10. } else { -10. }),
                            )),
                            touch_phase: TouchPhase::Started,
                            is_gesture: true,
                            ..Default::default()
                        }),
                        cx,
                    )
                })
                .unwrap();
                wheel_samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            wheel_samples.sort_by(f64::total_cmp);
            println!(
                "1000 trackpad events + synchronous CPU draw ms: p50={} p95={} p99={} max={}",
                wheel_samples[500], wheel_samples[950], wheel_samples[990], wheel_samples[999]
            );
            let mut samples = Vec::new();
            for i in 0..1000 {
                let start = Instant::now();
                cx.update(|cx| {
                    let y = if i % 2 == 0 { 50. } else { 270. };
                    movement(window, 317., y, false, cx);
                    button(window, 317., y, true, cx);
                    button(window, 317., y, false, cx);
                })
                .unwrap();
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "1000 track click + synchronous CPU draw ms: p50={} p95={} p99={} max={}",
                samples[500], samples[950], samples[990], samples[999]
            );
            cx.update(|cx| movement(window, 150., 150., false, cx)).unwrap();
            Timer::after(Duration::from_millis(2050)).await;
            cx.update(|cx| capture(window, "scrollbars-fading-2x", cx)).unwrap();
            Timer::after(Duration::from_millis(350)).await;
            cx.update(|cx| capture(window, "scrollbars-hidden-2x", cx)).unwrap();
            window
                .update(cx, |root, _, cx| {
                    root.extent = size(px(200.), px(150.));
                    cx.notify();
                })
                .unwrap();
            Timer::after(Duration::from_millis(80)).await;
            window
                .update(cx, |root, _, cx| {
                    for bar in &root.bars {
                        assert_eq!(
                            bar.read(cx).handle().max_offset(),
                            size(px(0.), px(0.)),
                            "fitting content removes both rails"
                        );
                    }
                })
                .unwrap();
            cx.update(|cx| capture(window, "scrollbars-no-overflow-2x", cx)).unwrap();
            window
                .update(cx, |root, _, cx| {
                    root.extent = size(px(620.), px(1400.));
                    cx.notify();
                })
                .unwrap();
            cx.update(|cx| zoom::change(cx, 1)).unwrap();
            Timer::after(Duration::from_millis(80)).await;
            cx.update(|cx| capture(window, "scrollbars-resized-zoom125-2x", cx)).unwrap();
            println!(
                "PASS: track jump, pointer capture, both axes, focus preservation, expiry, \
                 content shrink/resize, zoom"
            );
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
