//! One application-local zoom level, with bounded asynchronous profile saves.
//! Rendering never waits for storage. Normal termination drains the latest value.
use gpui::{App, Global, Pixels, Rems, Window, px};
use std::sync::Arc;

use crate::store::profile::{UI_ZOOM_MAX, UI_ZOOM_MIN, UI_ZOOM_ONE, validate_ui_zoom};
const STEP: u32 = 2_500;
#[cfg(test)]
pub(crate) const SAMPLES: [u32; 9] =
    [5_000, 7_500, 10_000, 12_345, 12_500, 15_000, 17_500, 18_765, 20_000];
pub type Save = Arc<dyn Fn(u32) -> Result<(), String> + Send + Sync>;
gpui::actions!(playbit_zoom, [ZoomIn, ZoomOut, ActualSize]);

pub struct UiZoom {
    scale: u32,
    save: Option<Save>,
    pending: Option<u32>,
    saving: bool,
    configured: bool,
    quit_after_save: bool,
}
impl Default for UiZoom {
    fn default() -> Self {
        Self {
            scale: UI_ZOOM_ONE,
            save: None,
            pending: None,
            saving: false,
            configured: false,
            quit_after_save: false,
        }
    }
}
impl Global for UiZoom {}

pub fn install(cx: &mut App) {
    if cx.has_global::<UiZoom>() {
        return;
    }
    cx.set_global(UiZoom::default());
    cx.on_action(|_: &ZoomIn, cx| change(cx, 1));
    cx.on_action(|_: &ZoomOut, cx| change(cx, -1));
    cx.on_action(|_: &ActualSize, cx| actual_size(cx));
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-+", ZoomIn, None),
        gpui::KeyBinding::new("cmd-=", ZoomIn, None),
        gpui::KeyBinding::new("cmd--", ZoomOut, None),
        gpui::KeyBinding::new("cmd-0", ActualSize, None),
    ]);
}

pub fn menu() -> gpui::Menu {
    gpui::Menu {
        name: "View".into(),
        items: vec![
            gpui::MenuItem::action("Zoom In", ZoomIn),
            gpui::MenuItem::action("Zoom Out", ZoomOut),
            gpui::MenuItem::action("Actual Size", ActualSize),
        ],
    }
}

pub fn configure(scale: u32, save: Save, cx: &mut App) {
    if !cx.has_global::<UiZoom>() {
        install(cx);
    }
    if cx.global::<UiZoom>().configured {
        return;
    }
    if let Err(error) = validate_ui_zoom(scale) {
        report_save_failure(&error.to_string(), cx);
        return;
    }
    let zoom = cx.global_mut::<UiZoom>();
    zoom.scale = scale;
    zoom.save = Some(save);
    zoom.configured = true;
    refresh(cx);
}

#[cfg(feature = "app-host")]
pub fn configure_engine(
    profile: std::path::PathBuf,
    root: Option<crate::store::Id>,
    scale: u32,
    cx: &mut App,
) {
    configure(
        scale,
        Arc::new(move |scale| {
            crate::engine::ipc::call_archive(
                &profile,
                &crate::engine::Request::UiZoom { root, scale: Some(scale) },
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        }),
        cx,
    );
}

pub fn factor(cx: &App) -> f32 {
    if cx.has_global::<UiZoom>() {
        cx.global::<UiZoom>().scale as f32 / UI_ZOOM_ONE as f32
    } else {
        1.0
    }
}

pub fn change(cx: &mut App, steps: i32) {
    let current = cx.global::<UiZoom>().scale;
    let next = (i64::from(current) + i64::from(steps) * i64::from(STEP))
        .clamp(i64::from(UI_ZOOM_MIN), i64::from(UI_ZOOM_MAX)) as u32;
    if current != next {
        set_scale(next, cx).expect("clamped UI scale is within its range");
    }
}

pub fn actual_size(cx: &mut App) {
    set_scale(UI_ZOOM_ONE, cx).expect("actual size is within the supported range");
}

/// Set a scale in hundredths of a percentage point, applying it before storage.
pub(crate) fn set_scale(scale: u32, cx: &mut App) -> crate::store::Result<()> {
    validate_ui_zoom(scale)?;
    let zoom = cx.global_mut::<UiZoom>();
    zoom.scale = scale;
    if zoom.save.is_some() {
        zoom.pending = Some(scale);
        save_next(cx);
    }
    refresh(cx);
    Ok(())
}

fn save_next(cx: &mut App) {
    let zoom = cx.global_mut::<UiZoom>();
    if zoom.saving {
        return;
    }
    let Some(scale) = zoom.pending.take() else {
        return;
    };
    let save = zoom.save.clone().expect("pending zoom has a storage owner");
    zoom.saving = true;
    let task = cx.background_executor().spawn(async move { save(scale) });
    cx.spawn(async move |cx| {
        let result = task.await;
        let _ = cx.update(|cx| {
            let windowless_quit =
                result.is_err() && cx.global::<UiZoom>().quit_after_save && cx.windows().is_empty();
            let zoom = cx.global_mut::<UiZoom>();
            zoom.saving = false;
            if let Err(error) = result {
                zoom.quit_after_save = false;
                if windowless_quit {
                    // The last window closed during the save. Let the next Quit
                    // pass the zoom guard and run the normal data-drain guards.
                    zoom.pending = None;
                    crate::diagnostic!("Cannot save UI zoom before quitting: {error}");
                    cx.quit();
                } else {
                    zoom.pending.get_or_insert(scale);
                    report_save_failure(&error, cx);
                }
                return;
            }
            if zoom.pending.is_some() {
                save_next(cx);
            } else if std::mem::take(&mut zoom.quit_after_save) {
                cx.quit();
            }
        });
    })
    .detach();
}

fn report_save_failure(error: &str, _cx: &mut App) {
    crate::diagnostic!("Cannot save UI zoom: {error}");
    #[cfg(all(target_os = "macos", feature = "local-store"))]
    {
        let details =
            format!("Couldn’t save UI zoom. Try changing zoom again or retry quitting. {error}");
        let kind = super::alert::Kind::Recoverable;
        // During deferred Quit, AppKit can drop its main window designation
        // while Sign In is still on screen and available for a retry.
        let window = _cx.active_window().or_else(|| {
            let windows = _cx.window_stack().unwrap_or_else(|| _cx.windows());
            windows.first().copied()
        });
        if let Some(window) = window {
            super::alert::show_in_window(kind, &details, window, _cx);
        } else {
            super::alert::show(kind, &details, _cx);
        }
    }
}

/// Defer termination while saving the latest preference. A windowless failure can finish Quit.
#[cfg(target_os = "macos")]
pub fn defer_quit(cx: &mut App) -> bool {
    if !cx.has_global::<UiZoom>() {
        return false;
    }
    let zoom = cx.global_mut::<UiZoom>();
    if !zoom.saving && zoom.pending.is_none() {
        return false;
    }
    zoom.quit_after_save = true;
    save_next(cx);
    cx.reply_to_quit(false).expect("AppKit owns this pending termination request");
    true
}

fn refresh(cx: &mut App) {
    // A shortcut may run with its window borrowed. Update after dispatch so the
    // focused window cannot retain stale rems while its controls use the new zoom.
    cx.defer(|cx| {
        #[cfg(target_os = "macos")]
        super::popup::dismiss(cx);
        for handle in cx.windows() {
            let _ = handle.update(cx, |_, window, cx| apply(window, cx));
        }

        cx.refresh_windows();
    });
}

pub fn apply(window: &mut Window, cx: &App) {
    window.set_rem_size(px(16.0 * factor(cx)));
}

/// A layout dimension in logical UI units; rem resolution applies zoom once.
pub fn length(logical: f32) -> Rems {
    gpui::rems(logical / 16.)
}

/// Snap a logical control dimension to the physical pixel grid after app zoom.
pub fn pixels(window: &Window, logical: f32) -> Pixels {
    px(snap(logical, f32::from(window.rem_size()) / 16., window.scale_factor()))
}
fn snap(logical: f32, zoom: f32, display: f32) -> f32 {
    (logical * zoom * display).round() / display
}
pub fn scaled(window: &Window, logical: f32) -> Pixels {
    px(logical * f32::from(window.rem_size()) / 16.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn zoom_steps_are_bounded_and_reset(cx: &mut gpui::TestAppContext) {
        cx.update(install);
        cx.update(|cx| {
            assert_eq!(factor(cx), 1.0);
            change(cx, 1);
            assert_eq!(factor(cx), 1.25);
            change(cx, 100);
            assert_eq!(factor(cx), 2.0);
            change(cx, -100);
            assert_eq!(factor(cx), 0.5);
            actual_size(cx);
            assert_eq!(factor(cx), 1.0);
        });
    }

    #[gpui::test]
    fn saves_are_ordered_coalesced_and_restore_without_writing(cx: &mut gpui::TestAppContext) {
        // A rapid key burst retains one active write and only its final successor.
        let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writes = saved.clone();
        cx.update(|cx| {
            install(cx);
            configure(
                15_000,
                Arc::new(move |scale| {
                    writes.lock().expect("test lock").push(scale);
                    Ok(())
                }),
                cx,
            );
            assert_eq!(factor(cx), 1.5);
            change(cx, 1);
            change(cx, 1);
            change(cx, -1);
            actual_size(cx);
        });
        cx.run_until_parked();
        assert_eq!(*saved.lock().expect("test lock"), [17_500, 10_000]);
        cx.update(|cx| {
            let zoom = cx.global::<UiZoom>();
            assert!(!zoom.saving && zoom.pending.is_none());
            assert_eq!(factor(cx), 1.);
        });
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn failed_zoom_save_allows_windowless_quit_and_retains_a_visible_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[cfg(feature = "connected-engine")]
        let window = cx.update(|cx| {
            use super::super::sign_in;
            use std::rc::Rc;
            sign_in::open(
                crate::release_channel::ReleaseChannel::Public,
                false,
                Rc::new(|_, _| Ok(())),
                Rc::new(|_| {}),
                cx,
            )
            .expect("Sign In opens before the first guest or library window")
            .0
        });
        #[cfg(not(feature = "connected-engine"))]
        let window = cx.add_window(|_, _| gpui::EmptyView);
        window.update(cx, |_, window, _| window.activate_window()).expect("active zoom target");
        gpui::VisualTestContext::from_window(window.into(), cx).deactivate_window();
        assert!(cx.update(|cx| cx.active_window().is_none()));
        let attempts = Arc::new(AtomicUsize::new(0));
        let saves = attempts.clone();
        cx.update(|cx| {
            configure(
                UI_ZOOM_ONE,
                Arc::new(move |_| {
                    saves.fetch_add(1, Ordering::Relaxed);
                    Err("read-only test profile".into())
                }),
                cx,
            );
            change(cx, 1);
            change(cx, 1);
            // The test platform has no AppKit termination delegate.
            cx.global_mut::<UiZoom>().quit_after_save = true;
        });
        cx.run_until_parked();
        #[cfg(feature = "connected-engine")]
        window
            .update(cx, |sign_in, window, _| {
                assert!(
                    sign_in.flow.error.is_empty(),
                    "the preference save does not change sign-in flow"
                );
                assert!(window.accessibility_nodes_for_test().any(|(_, node)| {
                    node.value() == Some("Zoom wasn’t saved. Retry zoom or Quit.")
                        && node.live() == Some(gpui::accesskit::Live::Polite)
                }));
            })
            .expect("Sign In stays open for retry");
        #[cfg(feature = "connected-engine")]
        {
            window
                .update(cx, |sign_in, _, cx| {
                    sign_in.flow.step = super::super::sign_in::flow::Step::Agreement;
                    cx.notify();
                })
                .expect("Sign In opens the agreement");
            cx.run_until_parked();
            window
                .update(cx, |_, window, _| {
                    let bounds = |name: &str| {
                        window
                            .accessibility_nodes_for_test()
                            .find(|(_, node)| {
                                node.label() == Some(name) || node.value() == Some(name)
                            })
                            .and_then(|(_, node)| node.bounds())
                            .expect("rendered control bounds")
                    };
                    assert!(
                        bounds("Zoom wasn’t saved. Retry zoom or Quit.").y1
                            <= bounds("Decline & Quit").y0
                    );
                })
                .expect("agreement controls remain available");
        }
        cx.update(|cx| {
            let zoom = cx.global::<UiZoom>();
            assert_eq!(zoom.pending, Some(UI_ZOOM_ONE + 2 * STEP));
            assert!(!zoom.saving && !zoom.quit_after_save);
            // Retry with one more change queued while the write is in flight.
            save_next(cx);
            change(cx, 1);
            cx.global_mut::<UiZoom>().quit_after_save = true;
        });
        window.update(cx, |_, window, _| window.remove_window()).expect("window is still open");
        cx.run_until_parked();
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        cx.update(|cx| {
            assert!(cx.windows().is_empty());
            let zoom = cx.global::<UiZoom>();
            assert!(!zoom.saving && !zoom.quit_after_save && zoom.pending.is_none());
            assert!(
                !defer_quit(cx),
                "the next application Quit continues through the drain guards"
            );
        });
    }

    #[gpui::test]
    fn arbitrary_fixed_precision_scale_restores_steps_and_resets(cx: &mut gpui::TestAppContext) {
        // Fractional percentages survive restore and menu steps without quantizing to presets.
        cx.update(|cx| {
            install(cx);
            configure(12_345, Arc::new(|_| Ok(())), cx);
            assert_eq!(cx.global::<UiZoom>().scale, 12_345);
            change(cx, 1);
            assert_eq!(cx.global::<UiZoom>().scale, 14_845);
            change(cx, -1);
            assert_eq!(cx.global::<UiZoom>().scale, 12_345);
            assert!(set_scale(UI_ZOOM_MIN - 1, cx).is_err());
            assert!(set_scale(UI_ZOOM_MAX + 1, cx).is_err());
            assert_eq!(cx.global::<UiZoom>().scale, 12_345);
            actual_size(cx);
            assert_eq!(cx.global::<UiZoom>().scale, 10_000);
        });
        cx.run_until_parked();
    }

    #[test]
    fn all_zoom_levels_place_control_edges_on_physical_pixels() {
        for scale in SAMPLES {
            let zoom = scale as f32 / UI_ZOOM_ONE as f32;
            for display in [1.0, 2.0] {
                for logical in [0.5, 2.0, 24.0, 117.333] {
                    let device = snap(logical, zoom, display) * display;
                    assert!((device - device.round()).abs() < 0.001);
                }
            }
        }
    }
}
