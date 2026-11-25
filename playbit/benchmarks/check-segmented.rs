//! Native 2× visual/interaction diagnostic. No activation or global input.
#![allow(unsafe_code, unexpected_cfgs, reason = "Read our own AppKit window ID for capture.")]
#![allow(clippy::unwrap_used, reason = "The visual fixture asserts expected window operations.")]
use gpui::{prelude::*, *};
use objc::{msg_send, runtime::Object, sel, sel_impl};
use playbit::runtime::SegmentEntry;
use playbit::ui::desktop::{controls, segmented::Segmented, zoom};
use playbit_api::ui::Theme;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    borrow::Cow,
    rc::Rc,
    time::{Duration, Instant},
};
struct Fixture {
    groups: Vec<Entity<Segmented>>,
}
impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgba(0xecececff))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .children(self.groups.iter().cloned())
    }
}
fn pointer(window: AnyWindowHandle, x: f32, y: f32, down: bool, cx: &mut App) {
    window
        .update(cx, |_, window, cx| {
            let position = point(px(x), px(y));
            window.dispatch_event(
                if down {
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
                },
                cx,
            );
        })
        .unwrap();
}
fn main() {
    let out = std::env::var("SEGMENT_CAPTURE_DIR")
        .unwrap_or_else(|_| "/private/tmp/archipelago-desktop-ui-review3".into());
    std::fs::create_dir_all(&out).unwrap();
    Application::new().with_assets(controls::Assets).run(move |cx| {
        zoom::install(cx);
        cx.text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!("../assets/InterVariable.ttf"))])
            .unwrap();
        let actions = Rc::new(std::cell::RefCell::new(Vec::new()));
        let received = actions.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(100.), px(100.)),
                        size(px(331.), px(176.)),
                    ))),
                    titlebar: None,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|cx| {
                        let mut groups = Vec::new();
                        for row in 0..4 {
                            let labels = if row == 2 {
                                vec!["Turtoise", "Slow", "Medium", "Fast", "Rabbit"]
                            } else if row == 3 {
                                vec!["Freddie", "Air", "Deep Purple"]
                            } else {
                                vec!["Slow", "Medium", "Fast"]
                            };
                            let segments = labels
                                .into_iter()
                                .enumerate()
                                .map(|(i, label)| SegmentEntry {
                                    id: (row * 10 + i + 1) as u32,
                                    label: label.into(),
                                    disabled: row == 2 && i == 1,
                                })
                                .collect::<Vec<_>>();
                            let received = received.clone();
                            groups.push(cx.new(|cx| {
                                Segmented::new(
                                    &segments,
                                    if row == 2 { 23 } else { (row * 10 + 2) as u32 },
                                    false,
                                    &Theme::light(),
                                    Rc::new(move |id, _| received.borrow_mut().push(id)),
                                    cx,
                                )
                            }));
                        }
                        Fixture { groups }
                    })
                },
            )
            .unwrap();
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(150)).await;
            window
                .update(cx, |root, window, cx| {
                    root.groups[1].read(cx).focus_handle(cx).focus(window);
                    root.groups[1].update(cx, |group, cx| {
                        group.show_keyboard_focus_if_focused(window, cx);
                    });
                })
                .unwrap();
            cx.update(|cx| pointer(window.into(), 200., 100., true, cx)).unwrap();
            Timer::after(Duration::from_millis(80)).await;
            let id = window
                .update(cx, |_, window, _| {
                    let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw()
                    else {
                        panic!()
                    };
                    unsafe {
                        let view = handle.ns_view.as_ptr() as *mut Object;
                        let native: *mut Object = msg_send![view, window];
                        let id: i64 = msg_send![native, windowNumber];
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
                        &format!("{out}/segmented-native-2x.png")
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
            cx.update(|cx| pointer(window.into(), 300., 125., false, cx)).unwrap();
            assert!(actions.borrow().is_empty(), "dragging outside cancels");
            let capture = |name: &str| {
                assert!(
                    std::process::Command::new("/usr/sbin/screencapture")
                        .args(["-x", "-o", "-l", &id.to_string(), &format!("{out}/{name}.png")])
                        .status()
                        .unwrap()
                        .success()
                );
            };
            cx.update(|cx| {
                pointer(window.into(), 160., 136., true, cx);
                pointer(window.into(), 160., 136., false, cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(110)).await;
            capture("segmented-stretch-2x");
            Timer::after(Duration::from_millis(220)).await;
            capture("segmented-settled-2x");
            // Reverse while moving, then verify that the latest selection wins.
            cx.update(|cx| {
                pointer(window.into(), 35., 136., true, cx);
                pointer(window.into(), 35., 136., false, cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(70)).await;
            cx.update(|cx| {
                pointer(window.into(), 95., 136., true, cx);
                pointer(window.into(), 95., 136., false, cx);
            })
            .unwrap();
            Timer::after(Duration::from_millis(300)).await;
            capture("segmented-interrupted-2x");
            assert_eq!(actions.borrow().last(), Some(&32));
            actions.borrow_mut().clear();
            let mut samples = Vec::new();
            for i in 0..1000 {
                let start = Instant::now();
                cx.update(|cx| {
                    let x = if i % 2 == 0 { 30. } else { 150. };
                    pointer(window.into(), x, 28., true, cx);
                    pointer(window.into(), x, 28., false, cx);
                    AnyWindowHandle::from(window)
                        .update(cx, |_, window, cx| {
                            let _ = window.draw(cx);
                        })
                        .unwrap();
                })
                .unwrap();
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            assert_eq!(actions.borrow().len(), 1000, "each distinct segment click dispatches once");
            window
                .update(cx, |root, window, cx| {
                    assert!(
                        root.groups[1].read(cx).focus_handle(cx).is_focused(window),
                        "pointer retains other group's keyboard focus"
                    )
                })
                .unwrap();
            samples.sort_by(f64::total_cmp);
            println!(
                "segment click dispatch + CPU draw ms n=1000 p50={} p95={} p99={} max={}",
                samples[499], samples[949], samples[989], samples[999]
            );
            // End on a value different from the burst's last choice. The final
            // scene must present even if drawable admission deferred earlier frames.
            cx.update(|cx| {
                pointer(window.into(), 95., 28., true, cx);
                pointer(window.into(), 95., 28., false, cx);
            })
            .unwrap();
            assert_eq!(actions.borrow().last(), Some(&2));
            Timer::after(Duration::from_millis(350)).await;
            capture("segmented-final-frame-2x");
            println!(
                "Segment pointer cancellation, selection, retained keyboard focus, \
                 and 2× capture passed"
            );
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
