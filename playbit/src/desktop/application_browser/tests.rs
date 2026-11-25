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
fn loaded_invitation_row_opens_consent_and_becomes_a_retained_session(
    cx: &mut gpui::TestAppContext,
) {
    let (host, window, requests) = scope(cx);
    let (responses, receive) = async_channel::bounded(1);
    host.update(cx, |host, cx| Shell::watch_responses(host.send.clone(), receive, cx));
    let session = host.update(cx, |host, cx| {
        let mut session = host.applications.catalog[0].clone();
        session.id = crate::identifier::encode(&[12; 32]);
        session.app.app_id = "puzzle".into();
        host.invitation = Some(crate::store::InvitationPreview {
            session: session.id.clone(),
            paused: false,
            retained: false,
            received: Some(crate::store::ReceivedInvitation {
                session: session.clone(),
                root_name: "Daily puzzle".into(),
            }),
        });
        let next = crate::store::SessionReference::new([13; crate::store::SEED_BYTES])
            .expect("valid link");
        host.preview_invitation_link(next.clone()).expect("admit newer link");
        let admission = host.join_preview.expect("newer link has an outstanding preview");
        host.dismiss_invitation(cx);
        assert_eq!(
            host.invitation_link.as_ref().map(|link| link.internal_id()),
            Some(next.internal_id())
        );
        assert_eq!(host.join_preview, Some(admission));
        assert!(host.invitation.is_none());
        assert_eq!(host.applications.selected.as_ref(), Some(&session.app));
        session
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, _| window.activate_window());
    draw(&mut visual);
    let position = visual.update(|window, _| {
        let node = window
            .accessibility_nodes_for_test()
            .find(|(_, node)| node.label() == Some("Daily puzzle · Invitation"))
            .expect("loaded session listed under its app")
            .1;
        let bounds = node.bounds().expect("visible invitation");
        gpui::point(
            px(((bounds.x0 + bounds.x1) / 2.) as f32),
            px(((bounds.y0 + bounds.y1) / 2.) as f32),
        )
    });
    while requests.try_recv().is_ok() {}
    visual.simulate_click(position, Default::default());
    draw(&mut visual);
    let Message::Request(first) = requests.try_recv().expect("resume request") else {
        panic!("invitation action")
    };
    assert!(
        matches!(&first.request, Request::ResumeInvitation { session: id } if id == &session.id)
    );
    assert!(requests.is_empty(), "selecting the row only resumes invitation review");
    host.update(&mut visual, |host, cx| {
        assert_eq!(host.invitation.as_ref().map(|preview| &preview.session), Some(&session.id));
        host.dismiss_invitation(cx);
        assert!(host.select_application_target(Target::Invitation(session.id.clone()), cx));
    });
    let Message::Request(second) = requests.try_recv().expect("second resume request") else {
        panic!("invitation action")
    };
    let mut stale = WorkerResponse::from_reply(Ok(Reply::InvitationStatus {
        invitation_status: Some(InvitationStatus {
            session: session.id.clone(),
            state: InvitationState::Ready { build_id: session.resolved_build_id.clone() },
        }),
    }));
    stale.credit = first.credit;
    responses.try_send(stale).expect("old result");
    draw(&mut visual);
    host.read_with(&visual, |host, _| {
        assert!(host.invitation.as_ref().expect("new review").paused);
        assert_eq!(host.join_status, InvitationState::Checking);
    });
    responses.try_send(worker::retired_response(second)).expect("engine restart");
    draw(&mut visual);
    host.update(&mut visual, |host, cx| {
        assert!(host.invitation.is_none());
        assert!(host.invitation_resume.is_none());
        assert_eq!(host.applications.selected.as_ref(), Some(&session.app));
        let mut catalog = host.applications.catalog.clone();
        catalog.push(session.clone());
        host.apply_application_catalog(catalog, cx);
        let app = host
            .applications
            .apps
            .iter()
            .find(|app| app.identity == session.app)
            .expect("retained app");
        assert_eq!(app.sessions, [session.id]);
        assert!(app.invitations.is_empty());
    });
}

#[gpui::test]
fn pending_only_app_removal_targets_its_row_and_retires_an_open_review(
    cx: &mut gpui::TestAppContext,
) {
    let (host, window, requests) = scope(cx);
    let (responses, receive) = async_channel::bounded(1);
    let pending = host.update(cx, |host, cx| {
        Shell::watch_responses(host.send.clone(), receive, cx);
        let mut session = host.applications.catalog[0].clone();
        session.id = crate::identifier::encode(&[12; 32]);
        session.app.app_id = "puzzle".into();
        host.saved_invitations = vec![crate::store::InvitationPreview {
            session: session.id.clone(),
            paused: true,
            retained: false,
            received: Some(crate::store::ReceivedInvitation {
                session: session.clone(),
                root_name: "Daily puzzle".into(),
            }),
        }];
        host.rebuild_application_list();
        cx.notify();
        session
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, _| window.activate_window());
    draw(&mut visual);
    let point = visual.update(|window, _| {
        let bounds = window
            .accessibility_nodes_for_test()
            .find(|(_, node)| node.label() == Some("puzzle"))
            .expect("app row for the saved invitation")
            .1
            .bounds()
            .expect("visible row");
        gpui::point(
            px(((bounds.x0 + bounds.x1) / 2.) as f32),
            px(((bounds.y0 + bounds.y1) / 2.) as f32),
        )
    });
    while requests.try_recv().is_ok() {}
    visual.simulate_mouse_down(point, gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(point, gpui::MouseButton::Right, Default::default());
    let Message::Request(queued) = requests.try_recv().expect("inspect the clicked app") else {
        panic!("expected a removal impact request");
    };
    assert!(matches!(&queued.request, Request::AppRemovalImpact { account, app }
        if *account == pending.creator && app == &pending.app));
    host.update(&mut visual, |host, cx| {
        assert!(host.select_application_target(Target::Invitation(pending.id.clone()), cx));
        assert!(host.session_dialog_open(cx));
    });
    // A removal on another device refreshes the saved previews before the
    // clicked row's inspection reply arrives.
    let mut refreshed = WorkerResponse::from_reply(Ok(Reply::AppRemoved(
        crate::store::AppRemovalResult::Removed { erased_volumes: vec![], cleanup_error: None },
    )));
    refreshed.saved_invitations = Some(Vec::new());
    responses.try_send(refreshed).expect("account refresh");
    draw(&mut visual);
    let mut late =
        WorkerResponse::from_reply(Ok(Reply::AppRemovalImpact { impact: Default::default() }));
    late.credit = queued.credit;
    responses.try_send(late).expect("late inspection");
    draw(&mut visual);
    host.read_with(&visual, |host, cx| {
        assert!(!host.session_dialog_open(cx));
        assert!(host.invitation.is_none());
        assert!(host.saved_invitations.is_empty());
        assert!(!host.applications.apps.iter().any(|row| row.identity == pending.app));
        assert!(host.applications.removal.is_none());
    });
}

#[gpui::test]
fn ready_saved_invitation_reviews_folder_access_before_joining(cx: &mut gpui::TestAppContext) {
    let (host, _, requests) = scope(cx);
    let (responses, receive) = async_channel::bounded(1);
    let session = host.update(cx, |host, cx| {
        Shell::watch_responses(host.send.clone(), receive, cx);
        let mut session = host.applications.catalog[0].clone();
        session.id = crate::identifier::encode(&[12; 32]);
        host.saved_invitations = vec![crate::store::InvitationPreview {
            session: session.id.clone(),
            retained: false,
            paused: true,
            received: Some(crate::store::ReceivedInvitation {
                session: session.clone(),
                root_name: "Daily puzzle".into(),
            }),
        }];
        host.rebuild_application_list();
        while requests.try_recv().is_ok() {}
        assert!(host.select_application_target(Target::Invitation(session.id.clone()), cx));
        session
    });
    let Message::Request(resume) = requests.try_recv().expect("resume invitation") else {
        panic!("expected an invitation request")
    };
    assert!(
        matches!(&resume.request, Request::ResumeInvitation { session: id } if id == &session.id)
    );
    let mut ready = WorkerResponse::from_reply(Ok(Reply::InvitationStatus {
        invitation_status: Some(InvitationStatus {
            session: session.id.clone(),
            state: InvitationState::Ready { build_id: session.resolved_build_id.clone() },
        }),
    }));
    ready.credit = resume.credit;
    responses.try_send(ready).expect("deliver ready invitation");
    cx.run_until_parked();
    host.update(cx, |host, cx| {
        let form = host.new_session.as_ref().expect("folder access review").read(cx);
        assert_eq!(form.reviewed_invitation(), Some(session.id.as_str()));
        while let Ok(message) = requests.try_recv() {
            assert!(
                !matches!(
                    message,
                    Message::Request(QueuedRequest { request: Request::StartJoin { .. }, .. })
                ),
                "reviewing the invitation does not start the transfer"
            );
        }
        host.dismiss_launch_dialog(cx);
        assert!(host.new_session.is_none());
        assert_eq!(host.applications.selected.as_ref(), Some(&session.app));
        assert!(host.saved_invitations.iter().any(|preview| preview.session == session.id));
    });
    assert!(matches!(requests.try_recv(), Ok(Message::Request(QueuedRequest {
        request: Request::PauseInvitation { session: id }, ..
    })) if id == session.id));
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

#[gpui::test]
fn ready_shared_browser_keeps_session_only_on_the_selected_route(cx: &mut gpui::TestAppContext) {
    let (host, _window, requests) = scope(cx);
    host.update(cx, |host, cx| {
        let session = &mut host.applications.catalog[0];
        let id = crate::identifier::decode(&session.id).expect("retained session ID");
        let shared_id =
            session.app.volume_id(Role::Session, Some(id), None, None).expect("Shared slot ID");
        session.shared_volume = Some(shared_id);
        let mut volume = host.scoped_volumes[0].clone();
        volume.id = shared_id;
        volume.share_key_with_service = true;
        host.scoped_volumes.push(volume.clone());
        let session = session.id.clone();
        assert!(host.select_application_target(Target::Session(session.clone()), cx));
        host.applications.slots_session = Some(session);
        host.applications.session_slots = vec![AutomaticVolumeSlot {
            role: Role::Session,
            volume: shared_id,
            overridden: false,
            state: AutomaticVolumeState::Ready,
        }];
        while requests.try_recv().is_ok() {}
        assert!(host.select_application_target(
            Target::Scope { role: Role::Session, volume: shared_id, session: true },
            cx
        ));
        let inspect = |expected| {
            let mut reads = (false, false);
            while let Ok(Message::Request(queued)) = requests.try_recv() {
                match queued.request {
                    Request::DirectoryEntries { volume, session, .. } if volume == shared_id => {
                        assert_eq!(session.as_deref().copied(), expected);
                        reads.0 = true;
                    }
                    Request::VolumeDetails { volume, session } if volume == shared_id => {
                        assert_eq!(session.as_deref().copied(), expected);
                        reads.1 = true;
                    }
                    _ => {}
                }
            }
            assert_eq!(reads, (true, true));
        };
        inspect(Some(id));
        let scoped = host.browser.clone().expect("Shared browser");
        assert!(host.select_volume(volume, cx));
        assert_ne!(host.browser.as_ref().expect("volume browser").entity_id(), scoped.entity_id());
        inspect(None);
    });
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
fn volumes_lists_apps_by_name_and_browses_session_data(cx: &mut gpui::TestAppContext) {
    // Enter through the Volumes tab, traverse the combined sidebar and open a
    // session volume using only the rendered controls. Enter must stay in the browser.
    let (host, window, _requests) = scope(cx);
    let shared = Id::from_bytes([3; 16]);
    host.update(cx, |host, cx| {
        let volume = host.scoped_volumes[0].clone();
        host.volumes.push(crate::store::Volume {
            name: "Archive".into(),
            id: Id::from_bytes([2; 16]),
            ..volume.clone()
        });
        host.scoped_volumes.push(crate::store::Volume {
            name: "Shared".into(),
            id: shared,
            ..volume
        });
        host.applications.catalog[0].shared_volume = Some(shared);
        let session = host.applications.catalog[0].clone();
        host.apply_imported_app_catalog(vec![crate::store::ImportedApp {
            app: AppIdentity { publisher: host.account.id, app_id: "alpha".into() },
            build_id: session.resolved_build_id,
            name: "Alpha".into(),
        }]);
        host.applications.slots_session = Some(session.id);
        host.applications.session_slots = vec![AutomaticVolumeSlot {
            role: Role::Session,
            volume: shared,
            overridden: false,
            state: AutomaticVolumeState::Ready,
        }];
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(1500.), px(600.)));
    visual.update(|window, _| window.activate_window());
    draw(&mut visual);
    let click = |label: &str, visual: &mut gpui::VisualTestContext| {
        let point = visual.update(|window, _| {
            let bounds = window
                .accessibility_nodes_for_test()
                .find(|(_, node)| {
                    node.role() == gpui::accesskit::Role::Button && node.label() == Some(label)
                })
                .expect("visible navigation button")
                .1
                .bounds()
                .expect("button bounds");
            gpui::point(
                px(((bounds.x0 + bounds.x1) / 2.) as f32),
                px(((bounds.y0 + bounds.y1) / 2.) as f32),
            )
        });
        visual.simulate_click(point, Default::default());
        draw(visual);
    };
    click("Volumes", &mut visual);
    click("Archive", &mut visual);
    for name in ["Alpha", "Keyboard"] {
        visual.simulate_keystrokes("down");
        draw(&mut visual);
        host.read_with(&visual, |host, _| {
            let app =
                host.applications.apps.iter().find(|app| app.name == name).expect("fixture app");
            assert_eq!(host.applications.selected.as_ref(), Some(&app.identity));
            assert!(host.sidebar_volumes);
        });
    }
    for key in ["right", "enter", "right"] {
        visual.simulate_keystrokes(key);
        draw(&mut visual);
    }
    host.read_with(&visual, |host, cx| {
        assert!(host.sidebar_volumes && host.applications.scope_in_session);
        let browser = host.browser.as_ref().expect("selected Shared volume").read(cx);
        assert_eq!(browser.volume.id, shared);
        assert_eq!(browser.shared_session(), Some([8; 32]));
        assert!(host.pending_windows.is_empty() && host.log_watch.is_none());
    });
    visual.update(|window, _| {
        assert!(window.accessibility_nodes_for_test().all(|(_, node)| !matches!(
            node.label(),
            Some("Open app" | "New instance" | "Share session" | "Log")
        )));
    });
    click("Apps", &mut visual);
    visual.update(|window, _| {
        assert!(
            window
                .accessibility_nodes_for_test()
                .any(|(_, node)| node.label() == Some("New instance"))
        );
    });
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
    let (host, window, requests) = scope(cx);
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
        let browser =
            host.read_with(&visual, |host, _| host.browser.clone().expect("scope browser"));
        let columns = browser
            .update(&mut visual, |browser, cx| browser.fixture_selected_kv_store(&requests, cx));
        for width in [1400., 1323., 1297., 1280., 1250., 1040., 900., 1400.] {
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
            let metadata = visual.debug_bounds("browser-metadata").expect("store inspector");
            let header = visual.debug_bounds("kv-header").expect("key-value table header");
            let directory = columns.bounds_for_item(0).expect("root directory column");
            assert_eq!(directory.bottom(), lower.top(), "directory at width {width}");
            assert_eq!(metadata.bottom(), lower.top(), "inspector at width {width}");
            assert_eq!(metadata.right(), upper.right(), "inspector at width {width}");
            assert_eq!(header.right(), metadata.left(), "table header at width {width}");
        }
    }
}

#[gpui::test]
fn session_and_instance_inspectors_share_the_log_and_align_metadata(cx: &mut gpui::TestAppContext) {
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
        let sessions = host.applications.catalog.clone();
        let id = sessions[0].id.clone();
        let activity = crate::store::SessionActivity {
            started: Some(1_793_294_220),
            participants: vec![crate::store::SeenParticipant {
                account: Id::from_bytes([13; 16]),
                name: "Robin Fernández".into(),
                last_seen: 1_793_294_300,
            }],
        };
        host.applications.set_activity(
            BTreeMap::from([(id, activity)]),
            &sessions,
            host.account.id,
            Some("Anne"),
        );
        host.logs.update(cx, |logs, cx| {
            logs.set(
                &sessions[0].id,
                &[crate::ui::app_control::LogEntry::notice("Guest output".into())],
                cx,
            )
        });
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(1041.), px(480.)));
    draw(&mut visual);
    let log = visual.debug_bounds("application-log-pane").expect("session log");
    assert_eq!(log.top(), px(286.));
    assert_eq!(log.right(), px(1041.));
    assert!((log.left() - px(240.)).abs() <= px(1.));
    visual.update(|window, _| {
        let nodes: Vec<_> = window.accessibility_nodes_for_test().map(|(_, n)| n).collect();
        for text in ["Participant", "Last seen", "Robin Fernández", "Anne (You)", "Guest output"] {
            assert!(nodes.iter().any(|n| n.value() == Some(text)), "session shows {text}");
        }
        assert!(nodes.iter().any(|n| n.label() == Some("Share")));
    });
    host.update(&mut visual, |host, cx| {
        assert!(host.select_application_target(Target::Instance(Id::from_bytes([10; 16])), cx));
    });
    draw(&mut visual);
    assert_eq!(visual.debug_bounds("application-log-pane"), Some(log));
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
            for key in ["application-id", "application-started"] {
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
                    .any(|(_, node)| { node.label() == Some("Make an app") }),
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
                    .any(|(_, node)| { node.label() == Some("Make an app") }),
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
    visual.simulate_resize(gpui::size(px(1200.), px(130.)));
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

#[gpui::test]
fn offscreen_app_browser_rows_keep_keyboard_order_and_accessibility(cx: &mut gpui::TestAppContext) {
    use gpui::accesskit::{Action, ActionRequest, NodeId, TreeId};

    let (host, window, _requests) = scope(cx);
    host.update(cx, |host, cx| {
        let template = host.applications.catalog[0].clone();
        let session_row = host.sessions[0].clone();
        let sessions: Vec<_> = (1..=48)
            .map(|index| Session {
                id: crate::identifier::encode(&[index; 32]),
                ..template.clone()
            })
            .collect();
        host.sessions = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| SessionRow {
                id: session.id.clone(),
                reference: crate::identifier::encode(&[index as u8 + 1; 16]),
                ..session_row.clone()
            })
            .collect();
        let session = sessions[0].clone();
        host.apply_application_catalog(sessions, cx);
        assert!(host.select_application_target(Target::Session(session.id.clone()), cx));
        host.applications.owners_session = Some(session.id.clone());
        host.applications.owners = (1..=48)
            .map(|index| {
                let id = Id::from_bytes([index; 16]);
                LaunchOwner {
                    instance: crate::engine::session_instance::SessionInstance {
                        id,
                        launch_request: id,
                        account: session.creator,
                        session: session.id.clone(),
                        build: session.resolved_build_id.clone(),
                        root: session.root_volume,
                        extra_volumes: Vec::new(),
                        host_mounts: Box::new([]),
                        restore_host_mounts: false,
                        owner_generation: 1,
                        state: InstanceState::Owned,
                    },
                    process: None,
                    presentation: 1,
                }
            })
            .collect();
        cx.notify();
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_resize(gpui::size(px(1200.), px(520.)));
    visual.update(|window, _| window.activate_window());
    draw(&mut visual);
    let ax = |action, target_node| ActionRequest {
        action,
        target_node,
        target_tree: TreeId::ROOT,
        data: None,
    };

    // Exercise instances first. Selecting another session subsequently replaces
    // the instance column and must retire its accessibility actions.
    let mut retired_instance = None;
    for column in [2, 1] {
        let (handle, controls) = host.read_with(&visual, |host, cx| {
            let bars = if column == 2 {
                &host.applications.instances_scrolling
            } else {
                &host.applications.sessions_scrolling
            };
            let controls: Vec<_> = host.applications.columns[column]
                .iter()
                .filter(|row| matches!(row.target, Target::Session(_) | Target::Instance(_)))
                .map(|row| host.navigation_controls[&row.key].clone())
                .collect();
            assert_eq!(controls.len(), 48);
            (bars.as_ref().expect("list viewport").read(cx).handle(), controls)
        });
        let node_id = |index: usize| NodeId(controls[index].entity_id().as_u64() * 2);
        let frame_height = |visual: &mut gpui::VisualTestContext, index| {
            visual.update(|window, _| {
                window
                    .accessibility_nodes_for_test()
                    .find(|(id, _)| *id == node_id(index))
                    .expect("row stays in accessibility tree")
                    .1
                    .bounds()
                    .expect("row bounds")
                    .height()
            })
        };
        assert!(frame_height(&mut visual, 0) > 0.);
        assert_eq!(frame_height(&mut visual, 30), 0.);
        visual.simulate_accessibility_action(ax(Action::ScrollIntoView, node_id(30)));
        draw(&mut visual);
        assert!(handle.offset().y < px(-700.));
        assert!(frame_height(&mut visual, 30) > 0.);
        visual.update(|window, cx| assert!(!controls[30].read(cx).focused(window)));

        handle.set_offset(gpui::Point::default());
        draw(&mut visual);
        assert_eq!(frame_height(&mut visual, 30), 0.);
        visual.simulate_accessibility_action(ax(Action::Focus, node_id(30)));
        draw(&mut visual);
        visual.update(|window, cx| assert!(controls[30].read(cx).focused(window)));
        assert!(frame_height(&mut visual, 30) > 0.);
        for (key, index) in [("tab", 31), ("shift-tab", 30), ("down", 31)] {
            handle.set_offset(gpui::Point::default());
            draw(&mut visual);
            assert_eq!(frame_height(&mut visual, index), 0.);
            visual.simulate_keystrokes(key);
            draw(&mut visual);
            visual.update(|window, cx| assert!(controls[index].read(cx).focused(window)));
            assert!(handle.offset().y < px(-700.));
            assert!(frame_height(&mut visual, index) > 0.);
        }
        if column == 2 {
            retired_instance = Some(node_id(31));
        }
    }
    let retired_instance = retired_instance.expect("instance from the first session");
    visual.update(|window, _| {
        assert!(window.accessibility_nodes_for_test().all(|(id, _)| id != retired_instance))
    });
    visual.simulate_accessibility_action(ax(Action::Focus, retired_instance));
    host.read_with(&visual, |host, _| {
        assert_eq!(host.selected_session_id(), Some(crate::identifier::encode(&[32; 32]).as_str()));
        assert!(host.applications.instance.is_none());
    });
}
