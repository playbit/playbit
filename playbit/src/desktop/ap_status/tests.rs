#![allow(clippy::unwrap_used, reason = "Tests assert expected fixture operations.")]
use super::*;

#[gpui::test]
fn hover_preserves_geometry_and_updates_status_at_every_scale(cx: &mut gpui::TestAppContext) {
    let (host, window, requests, responses) = test_support::host_with_responses(cx);
    let indicator = host.read_with(cx, |host, _| host.ap_status.clone());
    cx.update(|cx| update::preview(host.downgrade(), cx));
    cx.update(zoom::install);
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    let mut identity = None;
    for display in [1., 2.] {
        cx.simulate_window_scale_factor(window.into(), display);
        for units in zoom::SAMPLES {
            cx.update(|cx| zoom::set_scale(units, cx).unwrap());
            for (state, expected) in [
                (ApConnectionState::Live, "Connected to configured.ap.test"),
                (ApConnectionState::Connecting, "Connecting to configured.ap.test"),
                (ApConnectionState::Offline, "Offline — can’t reach configured.ap.test"),
                (ApConnectionState::OfflineMode, "Offline mode. Turn off Work Offline to connect."),
            ] {
                let mut response = WorkerResponse::new(Ok(()));
                response.status_ap_connection =
                    Some(ApConnectionStatus { state, hostname: "configured.ap.test".into() });
                responses.try_send(response).unwrap();
                visual.run_until_parked();
                let icon = visual.debug_bounds("ap-status-icon").unwrap();
                let footer = visual.debug_bounds("sidebar-footer").unwrap();
                let tabs = visual.debug_bounds("sidebar-tabs").unwrap();
                let expected_size = (24. * units as f32 / 10_000. * display).round() / display;
                assert_eq!(icon.size, size(px(expected_size), px(expected_size)));
                assert!(
                    icon.right() <= footer.right() && icon.bottom() <= footer.bottom(),
                    "icon={icon:?} footer={footer:?} zoom={units} display={display}"
                );
                visual.simulate_mouse_move(icon.center(), None, Default::default());
                assert!(visual.debug_bounds("ap-status-label").is_some());
                assert_eq!(visual.debug_bounds("ap-status-icon"), Some(icon));
                assert_eq!(visual.debug_bounds("sidebar-tabs"), Some(tabs));
                visual.update(|window, _| {
                    let (id, node) = window
                        .accessibility_nodes_for_test()
                        .find(|(_, node)| node.value() == Some(expected))
                        .unwrap();
                    assert_eq!(node.live(), Some(gpui::accesskit::Live::Polite));
                    assert!(!node.supports_action(gpui::accesskit::Action::Click));
                    assert!(!node.supports_action(gpui::accesskit::Action::Focus));
                    if let Some(previous) = identity {
                        assert_eq!(id, previous);
                    }
                    identity = Some(id);
                    let account = window
                        .accessibility_nodes_for_test()
                        .find(|(_, node)| node.label() == Some("Account"))
                        .unwrap()
                        .1;
                    assert_eq!(account.description(), Some("Update ready to install"));
                    assert!(
                        account.bounds().unwrap().x1
                            <= f64::from(f32::from(icon.left()) - 72. * units as f32 / 10_000.)
                                + 1.
                    );
                });
                indicator.read_with(cx, |indicator, _| {
                    assert!(indicator.hovered);
                    assert_eq!(indicator.tooltip, expected);
                    assert_eq!(indicator.frame.is_some(), state == ApConnectionState::Connecting);
                });
                visual.simulate_click(icon.center(), Default::default());
            }
            visual.simulate_mouse_move(gpui::point(px(1.), px(100.)), None, Default::default());
            indicator.read_with(cx, |indicator, _| assert!(!indicator.hovered));
        }
    }
    assert!(requests.is_empty(), "passive status cannot dispatch network work");
}

#[gpui::test]
fn engine_loss_retires_stale_live_status_until_a_new_snapshot(cx: &mut gpui::TestAppContext) {
    let (host, window, requests, responses) = test_support::host_with_responses(cx);
    let indicator = host.read_with(cx, |host, _| host.ap_status.clone());
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    assert!(visual.debug_bounds("ap-status-icon").is_none());
    let mut response = WorkerResponse::new(Ok(()));
    response.status_ap_connection = Some(ApConnectionStatus {
        state: ApConnectionState::Live,
        hostname: "configured.ap.test".into(),
    });
    responses.try_send(response).unwrap();
    visual.run_until_parked();
    let icon = visual.debug_bounds("ap-status-icon").unwrap();
    visual.simulate_mouse_move(icon.center(), None, Default::default());
    let mut response = WorkerResponse::new(Err("engine lost".into()));
    response.engine_lost = true;
    responses.try_send(response).unwrap();
    visual.run_until_parked();
    visual.update(|window, _| {
        assert!(!window.accessibility_nodes_for_test().any(|(_, node)| {
            node.value().is_some_and(|value| value.contains("configured.ap.test"))
        }))
    });
    indicator.read_with(cx, |indicator, _| {
        assert!(!indicator.hovered);
        assert!(indicator.status.is_none());
        assert!(indicator.frame.is_none());
        assert!(indicator.tooltip.is_empty());
    });
    let mut response = WorkerResponse::new(Ok(()));
    response.engine_recovered = true;
    responses.try_send(response).unwrap();
    visual.run_until_parked();
    visual.update(|window, _| {
        assert!(!window.accessibility_nodes_for_test().any(|(_, node)| {
            node.value().is_some_and(|value| value.contains("configured.ap.test"))
        }))
    });
    assert!(requests.is_empty());
}
