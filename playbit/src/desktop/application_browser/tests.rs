use super::*;

fn scope(
    cx: &mut gpui::TestAppContext,
) -> (Entity<Shell>, gpui::WindowHandle<Shell>, async_channel::Receiver<Message>) {
    let (host, window, requests) = test_support::host(cx);
    host.update(cx, |host, cx| {
        let volume = crate::store::Volume {
            id: Id::from_bytes([9; 16]),
            name: "User".into(),
            owner: host.account.id,
            personal: false,
            share_key_with_service: false,
        };
        let session = Session {
            app: AppIdentity { publisher: host.account.id, app_id: "keyboard".into() },
            id: crate::identifier::encode(&[8; 32]),
            selector: crate::store::BuildSelector::Exact {
                build_id: crate::identifier::encode(&[7; 32]),
            },
            resolved_build_id: crate::identifier::encode(&[7; 32]),
            root_volume: Id::from_bytes([6; 16]),
            shared_volume: None,
            creator: host.account.id,
            update_authority: crate::store::UpdateAuthority::Creator,
        };
        // This renderer fixture never opens an IPC connection or supplies data authority.
        host.applications.refreshing = true;
        host.applications.scope_refreshing = true;
        host.sessions = vec![SessionRow {
            reference: crate::identifier::encode(&[5; 16]),
            id: session.id.clone(),
            resolved_build_id: session.resolved_build_id.clone(),
            app_name: Some("Keyboard".into()),
            build_state: crate::engine::builds::BuildState::Ready,
        }];
        host.scoped_volumes = vec![volume.clone()];
        host.apply_application_catalog(vec![session.clone()], cx);
        host.applications.slots_app = Some(session.app.clone());
        host.applications.app_slots = vec![AutomaticVolumeSlot {
            role: Role::User,
            volume: volume.id,
            overridden: false,
            state: AutomaticVolumeState::Ready,
        }];
        assert!(host.select_application_target(Target::Application(session.app), cx));
        assert!(host.select_application_target(Target::Session(session.id), cx));
        assert!(host.select_application_target(
            Target::Scope { role: Role::User, volume: volume.id, session: false },
            cx
        ));
    });
    (host, window, requests)
}

fn draw(visual: &mut gpui::VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.refresh();
        let _ = window.draw(cx);
    });
    visual.run_until_parked();
}

#[gpui::test]
fn unavailable_shared_row_cannot_open_or_send_an_action(cx: &mut gpui::TestAppContext) {
    // A recipient's unavailable scope stays visible in the normal columns,
    // but neither a pointer click nor a stale selection may open it.
    let (host, window, requests) = scope(cx);
    let volume = Id::from_bytes([4; 16]);
    host.update(cx, |host, cx| {
        let id = host.applications.catalog[0].id.clone();
        host.applications.catalog[0].creator = Id::from_bytes([3; 16]);
        assert!(host.select_application_target(Target::Session(id.clone()), cx));
        host.applications.slots_session = Some(id);
        host.applications.session_slots = vec![AutomaticVolumeSlot {
            role: Role::Session,
            volume,
            overridden: false,
            state: AutomaticVolumeState::Unavailable,
        }];
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(1200.), px(600.)));
    visual.update(|window, _| window.activate_window());
    draw(&mut visual);
    while requests.try_recv().is_ok() {}
    let position = visual.update(|window, _| {
        let row = window
            .accessibility_nodes_for_test()
            .find(|(_, node)| node.value() == Some("Shared"))
            .expect("unavailable Shared remains visible")
            .1;
        assert_ne!(row.role(), gpui::accesskit::Role::Button);
        let bounds = row.bounds().expect("visible scope bounds");
        gpui::point(
            px(((bounds.x0 + bounds.x1) / 2.) as f32),
            px(((bounds.y0 + bounds.y1) / 2.) as f32),
        )
    });
    visual.simulate_click(position, Default::default());
    draw(&mut visual);
    host.update(&mut visual, |host, cx| {
        assert!(!host.select_application_target(
            Target::Scope { role: Role::Session, volume, session: true },
            cx
        ));
        assert!(!host.applications.action_busy);
        assert!(host.applications.pending_scope.is_none());
        assert!(host.applications.selected_role.is_none());
        assert!(host.applications.scope_error.is_none());
    });
    assert!(requests.try_recv().is_err());
}

fn right_edges(visual: &mut gpui::VisualTestContext) -> [f64; 2] {
    visual.update(|window, _| {
        ["Keyboard", "User"].map(|label| {
            window
                .accessibility_nodes_for_test()
                .find(|(_, node)| {
                    node.role() == gpui::accesskit::Role::Button && node.label() == Some(label)
                })
                .expect("rendered navigation row")
                .1
                .bounds()
                .expect("navigation row has bounds")
                .x1
        })
    })
}

#[gpui::test]
fn app_column_scrolls_with_data_and_sidebar_stays_fixed(cx: &mut gpui::TestAppContext) {
    let (host, window, _requests) = scope(cx);
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(900.), px(600.)));
    visual.update(|window, _| window.activate_window());
    draw(&mut visual);
    let handle = host.read_with(&visual, |host, cx| {
        host.applications.scrolling.as_ref().expect("application viewport").read(cx).handle()
    });
    assert!(handle.max_offset().width > px(80.));
    let before = right_edges(&mut visual);
    visual.simulate_event(gpui::ScrollWheelEvent {
        position: gpui::point(handle.bounds().right() - px(20.), handle.bounds().top() + px(80.)),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(-80.), px(0.))),
        ..Default::default()
    });
    draw(&mut visual);
    assert!(handle.offset().x < px(-40.));
    let after = right_edges(&mut visual);
    assert!((after[0] - before[0]).abs() < 1., "Apps remains fixed");
    assert!(after[1] < before[1] - 40., "App data moves with the main viewport");
    visual.simulate_resize(gpui::size(px(900.), px(340.)));
    draw(&mut visual);
    visual.update(|window, _| {
        let session = window
            .accessibility_nodes_for_test()
            .find(|(_, node)| {
                node.role() == gpui::accesskit::Role::Button
                    && node.label().is_some_and(|label| label.starts_with("Session "))
            })
            .expect("session row remains available")
            .1;
        assert_ne!(session.is_selected(), Some(true), "User is a separate selection path");
    });
}

#[gpui::test]
fn scoped_browser_fills_its_column_across_window_widths(cx: &mut gpui::TestAppContext) {
    // Exercise both embedding depths around their minimum total widths. The preview
    // and navigator must share the browser width even while the outer columns scroll.
    let (host, window, _requests) = scope(cx);
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for session_scope in [false, true] {
        if session_scope {
            host.update(&mut visual, |host, cx| {
                let session = host.applications.catalog[0].id.clone();
                let volume = host.scoped_volumes[0].id;
                host.applications.slots_session = Some(session.clone());
                host.applications.session_slots = vec![AutomaticVolumeSlot {
                    role: Role::UserSession,
                    volume,
                    overridden: false,
                    state: AutomaticVolumeState::Ready,
                }];
                assert!(host.select_application_target(Target::Session(session), cx));
                assert!(host.select_application_target(
                    Target::Scope { role: Role::UserSession, volume, session: true },
                    cx,
                ));
            });
        }
        for width in [1400., 1280., 1250., 1040., 900., 1400.] {
            visual.simulate_resize(gpui::size(px(width), px(600.)));
            draw(&mut visual);
            let upper = visual.debug_bounds("browser-upper").expect("browser navigator");
            let lower = visual.debug_bounds("browser-lower").expect("browser preview");
            let content = visual.debug_bounds("browser-content").expect("preview content");
            let expected =
                (width - if session_scope { 720. } else { 480. }).max(browser::MIN_WIDTH);
            assert!(
                (f32::from(lower.size.width) - expected).abs() <= 1.,
                "preview {lower:?} at window width {width}, session scope {session_scope}",
            );
            assert_eq!(upper.size.width, lower.size.width);
            assert_eq!(upper.left(), lower.left());
            assert_eq!(upper.bottom(), lower.top());
            assert_eq!(lower.bottom(), px(600.));
            assert_eq!(content.size.width, lower.size.width);
            assert_eq!(content.left(), lower.left());
            assert!((content.bottom() - lower.bottom()).abs() <= px(1.));
        }
    }
}

#[gpui::test]
fn instance_inspector_centers_stop_and_uses_available_metadata_width(
    cx: &mut gpui::TestAppContext,
) {
    // Check the actual header button and selectable metadata fields in the full shell.
    let (host, window, _requests) = scope(cx);
    host.update(cx, |host, cx| {
        let session = host.applications.catalog[0].clone();
        assert!(host.select_application_target(Target::Session(session.id.clone()), cx));
        let instance = Id::from_bytes([10; 16]);
        host.applications.owners_session = Some(session.id.clone());
        host.applications.owners = vec![LaunchOwner {
            instance: crate::engine::session_instance::SessionInstance {
                id: instance,
                launch_request: Id::from_bytes([11; 16]),
                account: session.creator,
                session: session.id,
                build: session.resolved_build_id,
                root: session.root_volume,
                extra_volumes: Vec::new(),
                host_mounts: Box::new([]),
                restore_host_mounts: false,
                owner_generation: 1,
                state: InstanceState::Owned,
            },
            process: None,
            presentation: 1,
        }];
        assert!(host.select_application_target(Target::Instance(instance), cx));
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for width in [1024., 1200.] {
        visual.simulate_resize(gpui::size(px(width), px(600.)));
        draw(&mut visual);
        visual.update(|window, cx| {
            let stop = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| {
                    node.role() == gpui::accesskit::Role::Button && node.label() == Some("Stop")
                })
                .expect("Stop button")
                .1
                .bounds()
                .expect("Stop bounds");
            assert!((stop.y0 - 10.).abs() <= 1., "Stop must be centered: {stop:?}");
            let right = host
                .read(cx)
                .applications
                .inspector_scrolling
                .as_ref()
                .expect("inspector viewport")
                .read(cx)
                .handle()
                .bounds()
                .right()
                .to_f64();
            assert!((stop.x1 - (right - 10.)).abs() <= 1., "Stop inset: {stop:?}");
            for key in ["application-id", "application-root-id", "application-state"] {
                let editor = &host.read(cx).applications.identifiers[key];
                let id = text_editor::accessibility_text_ids(editor.entity_id()).0;
                let field = window
                    .accessibility_nodes_for_test()
                    .find(|(node_id, _)| *node_id == id)
                    .expect("selectable metadata")
                    .1
                    .bounds()
                    .expect("metadata bounds");
                assert!(
                    (field.x1 - (right - 12.)).abs() <= 1.,
                    "{key} must use available width: {field:?}"
                );
            }
        });
    }
}

#[gpui::test]
fn new_session_retires_application_navigation_and_keeps_the_catalog(cx: &mut gpui::TestAppContext) {
    // Click the rendered + from app and data columns, then refresh and reselect the retained app.
    let (host, window, _requests) = scope(cx);
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, _| window.activate_window());
    let (app, volume) = host.read_with(&visual, |host, _| {
        (
            host.applications.selected.clone().expect("fixture app"),
            host.applications.app_slots[0].volume,
        )
    });
    for target in [
        Target::Application(app.clone()),
        Target::Scope { role: Role::User, volume, session: false },
    ] {
        host.update(&mut visual, |host, cx| {
            assert!(host.select_application_target(target, cx));
        });
        draw(&mut visual);
        let (positions, selected) = visual.update(|window, _| {
            let nodes = ["New session", "Keyboard"].map(|label| {
                window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| {
                        node.role() == gpui::accesskit::Role::Button && node.label() == Some(label)
                    })
                    .expect("rendered navigation button")
                    .1
            });
            let positions = nodes.map(|node| {
                let bounds = node.bounds().expect("rendered button bounds");
                gpui::point(
                    px(((bounds.x0 + bounds.x1) / 2.) as f32),
                    px(((bounds.y0 + bounds.y1) / 2.) as f32),
                )
            });
            (positions, nodes[0].is_selected())
        });
        visual.simulate_click(positions[0], Default::default());
        draw(&mut visual);
        visual.update(|window, _| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.label() == Some("Try a TODO app") }),
                "New session must show templates after selecting an app"
            );
        });
        assert_ne!(selected, Some(true), "+ must not appear selected while app columns are shown");
        host.update(&mut visual, |host, cx| {
            assert!(host.applications.selected.is_none());
            assert!(host.applications.selected_role.is_none());
            assert!(host.browser.is_none() && host.selected_session_id().is_none());
            assert!(host.applications.scope_refreshing, "navigation does not retire live work");
            host.apply_application_catalog(host.applications.catalog.clone(), cx);
            cx.notify();
        });
        draw(&mut visual);
        visual.update(|window, _| {
            assert!(
                window
                    .accessibility_nodes_for_test()
                    .any(|(_, node)| { node.label() == Some("Try a TODO app") }),
                "catalog refresh must preserve template navigation"
            );
        });
        visual.simulate_click(positions[1], Default::default());
        draw(&mut visual);
        host.read_with(&visual, |host, _| {
            assert_eq!(host.applications.selected.as_ref(), Some(&app))
        });
    }
}

#[gpui::test]
fn vertical_column_rails_scroll_their_own_viewports(cx: &mut gpui::TestAppContext) {
    cx.set_auto_hide_scrollbars(false);
    let (host, window, _requests) = scope(cx);
    host.update(cx, |host, cx| {
        let id = host.applications.catalog[0].id.clone();
        assert!(host.select_application_target(Target::Session(id), cx));
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    // Keep all three columns visible horizontally, with overflow even in the
    // two-row session inspector. No running process or live account is needed.
    visual.simulate_resize(gpui::size(px(1200.), px(80.)));
    draw(&mut visual);
    let handles = host.read_with(&visual, |host, cx| {
        [
            &host.applications.sessions_scrolling,
            &host.applications.instances_scrolling,
            &host.applications.inspector_scrolling,
        ]
        .map(|bars| bars.as_ref().expect("vertical viewport").read(cx).handle())
    });
    for (index, handle) in handles.iter().enumerate() {
        assert!(handle.max_offset().height > px(0.), "column {index} must overflow");
        let bounds = handle.bounds();
        let rail = gpui::point(bounds.right() - px(8.), bounds.bottom() - px(8.));
        visual.simulate_mouse_move(rail, None, Default::default());
        visual.simulate_click(rail, Default::default());
        draw(&mut visual);
        assert!(handle.offset().y < px(0.), "column {index} rail must scroll its content");
        for (other, handle) in handles.iter().enumerate() {
            if other != index {
                assert_eq!(handle.offset().y, px(0.), "other columns must remain still");
            }
        }
        handle.set_offset(gpui::Point::default());
        draw(&mut visual);
    }
}
