#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
//! Shared desktop GUI fixtures and input helpers.
use super::*;

pub(super) fn host(
    cx: &mut gpui::TestAppContext,
) -> (Entity<Shell>, gpui::WindowHandle<Shell>, async_channel::Receiver<Message>) {
    let (host, window, requests, _) = host_with_responses(cx);
    (host, window, requests)
}

pub(super) fn host_with_responses(
    cx: &mut gpui::TestAppContext,
) -> (
    Entity<Shell>,
    gpui::WindowHandle<Shell>,
    async_channel::Receiver<Message>,
    async_channel::Sender<WorkerResponse>,
) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let host = cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
    drop(requests.try_recv().unwrap());
    host.update(cx, |host, cx| {
        host.account.id = crate::store::Id::from_bytes([1; 16]);
        drafts::configure_memory(host.account.id, cx);
        host.enrolled = Some(true);
        host.account_profile = AccountProfile {
            agreement_required: false,
            replacement_required: false,
            display_name: Some("Robin".into()),
            unlocked: true,
            initialized: true,
        };
        host.recovery = Some(crate::engine::reply::RecoveryStatus::Ready {
            revision: 1,
            pending_save: false,
            restored_sessions: None,
            restored_volumes: None,
        });
    });
    cx.update(|cx| Shell::show_sessions(&host, cx));
    let window = host.read_with(cx, |host, _| host.sessions_window.unwrap());
    (host, window, requests, responses)
}

pub(super) fn focus_button(
    window: gpui::WindowHandle<Shell>,
    id: &'static str,
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| {
        window.update(cx, |host, window, cx| host.host_controls[id].read(cx).focus(window)).unwrap()
    });
}

pub(super) fn press(window: gpui::WindowHandle<Shell>, key: &str, cx: &mut gpui::TestAppContext) {
    cx.simulate_keystrokes(window.into(), key);
    cx.dispatch_key_up(window.into(), gpui::Keystroke::parse(key).unwrap());
    cx.run_until_parked();
}

/// Render a settled view at every zoom and display scale, checking actual control
/// geometry rather than just the zoom setting. Call after preparing each state.
pub(crate) fn check_zoom_geometry(window: gpui::AnyWindowHandle, cx: &mut gpui::TestAppContext) {
    use crate::ui::desktop::zoom;
    use gpui::accesskit::Role;
    cx.update(|cx| {
        zoom::install(cx);
        zoom::actual_size(cx);
    });
    cx.run_until_parked();
    let baseline = cx.update(|cx| {
        window
            .update(cx, |_, window, _| {
                window
                    .accessibility_nodes_for_test()
                    .filter_map(|(id, node)| {
                        matches!(node.role(), Role::Button | Role::CheckBox | Role::RadioButton)
                            .then(|| {
                                node.bounds().map(|bounds| {
                                    (id, (bounds.y1 - bounds.y0, node.label().map(str::to_owned)))
                                })
                            })
                            .flatten()
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap()
    });
    for display in [1., 2.] {
        cx.simulate_window_scale_factor(window, display);
        for units in zoom::SAMPLES {
            let scale = units as f32 / 10_000.;
            cx.update(|cx| {
                zoom::actual_size(cx);
                zoom::set_scale(units, cx).expect("supported test scale");
            });
            cx.run_until_parked();
            cx.update(|cx| {
                window
                    .update(cx, |_, window, _| {
                        assert!(window.accessibility_error().is_none());
                        for (id, node) in window.accessibility_nodes_for_test() {
                            let Some((height, label)) = baseline.get(&id) else { continue };
                            let bounds = node.bounds().expect("visible control bounds");
                            let expected = height * f64::from(scale);
                            assert!(
                                (bounds.y1 - bounds.y0 - expected).abs() <= 1.,
                                "{label:?} height {:?} expected {expected} at \
                                 {scale}x on {display}x display",
                                bounds.y1 - bounds.y0
                            );
                        }
                    })
                    .unwrap()
            });
        }
    }
    cx.update(zoom::actual_size);
    cx.run_until_parked();
}
