#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;

#[gpui::test]
fn shell_zoom_scales_templates_modals_and_native_minimum(cx: &mut gpui::TestAppContext) {
    // Exercise the screenshot's mixed-control regression, live modal zoom and reopening.
    use crate::ui::desktop::zoom;
    cx.update(zoom::install);
    let (host, window, _) = test_support::host(cx);
    for units in zoom::SAMPLES {
        let scale = units as f32 / 10_000.;
        cx.update(|cx| {
            zoom::actual_size(cx);
            zoom::set_scale(units, cx).expect("supported test scale");
        });
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                assert_eq!(window.min_size(), Some(sessions_min_size(scale)));
                let template = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Make an app"))
                    .expect("welcome template")
                    .1
                    .bounds()
                    .expect("template bounds");
                assert!((template.width() - f64::from(200. * scale)).abs() < 1.);
                assert!((template.height() - f64::from(146. * scale)).abs() < 1.);
                let example = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Little List"))
                    .expect("featured example")
                    .1
                    .bounds()
                    .expect("example bounds");
                assert!((example.width() - f64::from(96. * scale)).abs() < 1.);
                assert!((example.height() - f64::from(80. * scale)).abs() < 1.);
                assert!((template.x0 - example.x0 - f64::from(8. * scale)).abs() < 1.);
                assert!((template.y0 - example.y0 - f64::from(140. * scale)).abs() < 2.);
            })
            .unwrap();
        host.update(cx, |host, cx| host.open_join_dialog(cx));
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                let submit = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Join Session…"))
                    .expect("join action")
                    .1;
                assert!(submit.is_disabled());
                let right = window.viewport_size().width.to_f64() / 2. + f64::from(192. * scale);
                assert!((submit.bounds().unwrap().x1 - right).abs() < 2.);
            })
            .unwrap();
        // Close the dialog by its actual keyboard route at the current scale.
        test_support::press(window, "escape", cx);
        host.read_with(cx, |host, _| assert!(host.join_dialog.is_none()));
    }
    window.update(cx, |_, window, _| window.remove_window()).unwrap();
    cx.update(|cx| Shell::show_sessions(&host, cx));
    let reopened = host.read_with(cx, |host, _| host.sessions_window.unwrap());
    reopened
        .update(cx, |_, window, _| {
            assert_eq!(window.min_size(), Some(size(px(1000.), px(600.))));
            assert!(window.viewport_size().width >= px(1000.));
            assert!(window.viewport_size().height >= px(600.));
        })
        .unwrap();
}

#[gpui::test]
fn sign_out_confirmation_cancels_without_changing_the_account(cx: &mut gpui::TestAppContext) {
    let (host, responses, _) = launch_link(cx);
    responses.try_send(saved_status(serde_json::json!({"enrolled": true}))).unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        Shell::show_sessions(&host, cx);
        host.read(cx)
            .sessions_window
            .unwrap()
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
    });
    cx.run_until_parked();
    cx.update(|cx| {
        Shell::confirm_sign_out(&host, cx);
        Shell::confirm_sign_out(&host, cx);
    });
    assert_eq!(
        cx.pending_prompt(),
        Some(("Sign out?".into(), "Signing out leaves all local data as is".into()))
    );
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt(), "duplicate activation admits only one prompt");
    host.read_with(cx, |host, _| {
        assert!(!host.signing_out && !host.sign_out_prompt);
        assert!(host.account_ready());
    });
}

fn saved_status(mut status: serde_json::Value) -> super::WorkerResponse {
    let saved = status
        .as_object_mut()
        .unwrap()
        .remove("invitations")
        .map(|value| serde_json::from_value(value).unwrap())
        .unwrap_or_default();
    let sessions = status["sessions"]
        .as_array()
        .map(|sessions| {
            sessions
                .iter()
                .map(|session| super::SessionRow {
                    reference: String::new(),
                    id: session["id"].as_str().unwrap().into(),
                    resolved_build_id: session["resolved_build_id"].as_str().unwrap_or("").into(),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: session["app_name"].as_str().map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut response = super::WorkerResponse::new(Ok(()));
    response.status_enrolled = Some(status["enrolled"].as_bool().unwrap_or(false));
    response.status_account_profile = Some(super::AccountProfile {
        agreement_required: false,
        replacement_required: false,
        display_name: Some("Robin".into()),
        unlocked: true,
        initialized: true,
    });
    response.saved_invitations = Some(saved);
    response.session_rows = Some(sessions);
    response
}
fn launch_link(
    cx: &mut gpui::TestAppContext,
) -> (Entity<Shell>, async_channel::Sender<WorkerResponse>, async_channel::Receiver<Message>) {
    cx.update(text_editor::install_keybindings);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let (urls, receive_urls) = async_channel::bounded(4);
    urls.try_send(Ok(crate::store::session_link(
        &[7; crate::store::SEED_BYTES],
        crate::release_channel::ReleaseChannel::Public,
    )))
    .unwrap();
    let host = cx.update(|cx| Shell::launch((send, receive, errors), receive_urls, true, cx));
    (host, responses, requests)
}

#[gpui::test]
fn browser_replies_update_the_browser_without_notifying_the_host(cx: &mut gpui::TestAppContext) {
    // Exercise the real response task with typed success and engine-loss responses.
    let (send, _requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let (_urls, receive_urls) = async_channel::bounded(1);
    let host = cx.new(|cx| Shell::new((send, receive, errors), receive_urls, cx));
    let volume = crate::store::Volume {
        id: crate::store::Id::from_bytes([1; 16]),
        name: "Archive".into(),
        owner: crate::store::Id::from_bytes([1; 16]),
        personal: true,
        share_key_with_service: false,
    };
    let browser = host.update(cx, |host, cx| {
        let browser = cx.new(|cx| browser::Browser::new(volume.clone(), host.send.clone(), cx));
        host.browser = Some(browser.clone());
        browser
    });
    cx.run_until_parked();
    let host_count = Rc::new(std::cell::Cell::new(0));
    let browser_count = Rc::new(std::cell::Cell::new(0));
    let _host_observer = cx.update(|cx| {
        let count = host_count.clone();
        cx.observe(&host, move |_, _| count.set(count.get() + 1))
    });
    let _browser_observer = cx.update(|cx| {
        let count = browser_count.clone();
        cx.observe(&browser, move |_, _| count.set(count.get() + 1))
    });
    for _ in 0..2 {
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::DirectoryEntries {
                volume: volume.id,
                target: crate::store::volume_index::DirectoryTarget::Root,
                cut_requested: None,
                after: None,
                listing: crate::store::volume_index::DirectoryListing::Page {
                    cut: 1,
                    parent: None,
                    ancestors: vec![],
                    entries: vec![],
                    next: None,
                },
            })))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(host_count.get(), 0);
        assert_eq!(browser_count.get(), 1);
    }
    let mut lost = WorkerResponse::new(Err("Engine stopped".into()));
    lost.engine_lost = true;
    responses.try_send(lost).unwrap();
    cx.run_until_parked();
    assert_eq!(host_count.get(), 1, "engine recovery must not open another window");
    host.read_with(cx, |host, _| {
        assert!(host.engine_recovering);
        assert!(host.error.is_empty());
    });
}

#[gpui::test]
fn initial_link_retains_host_without_a_window_and_surfaces_worker_failure(
    cx: &mut gpui::TestAppContext,
) {
    let (host, responses, _requests) = launch_link(cx);
    let weak = host.downgrade();
    drop(host);
    cx.run_until_parked();
    let host = weak.upgrade().expect("application action must retain the windowless host");
    host.read_with(cx, |host, _| assert!(host.sessions_window.is_none()));
    responses.try_send(WorkerResponse::new(Err("Cannot open this session".into()))).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none(), "startup failure must not open Sessions");
        assert!(host.startup_prompt, "failed link must expose its native alert");
        assert!(!host.initial_link);
        assert_eq!(host.error, "Cannot open this session");
    });
}

#[gpui::test]
fn initial_link_shows_sign_in_for_an_unenrolled_profile(cx: &mut gpui::TestAppContext) {
    let (host, responses, _requests) = launch_link(cx);
    responses.try_send(saved_status(serde_json::json!({"sessions":[], "enrolled":false}))).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none(), "status alone must not preempt the launch link")
    });
    responses
        .try_send(WorkerResponse::from_reply(Ok(Reply::InvitationPreview(
            crate::store::InvitationPreview {
                session: crate::store::session_id(&[7; crate::store::SEED_BYTES]).unwrap(),
                paused: false,
                retained: false,
                received: None,
            },
        ))))
        .unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none());
        assert!(host.sign_in.is_some());
        assert_eq!(host.enrolled, Some(false));
        assert!(host.invitation.is_some());
    });
}

#[gpui::test]
fn resource_launch_waits_for_late_url_and_opens_retained_session(cx: &mut gpui::TestAppContext) {
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let (urls, receive_urls) = async_channel::bounded(4);
    let host = cx.update(|cx| Shell::launch((send, receive, errors), receive_urls, false, cx));
    let seed = [9; crate::store::SEED_BYTES];
    let id = crate::store::session_id(&seed).unwrap();
    responses.try_send(saved_status(serde_json::json!({
        "sessions":[{"id":id}], "enrolled":true,
        "invitations":[{"session":"older-invitation", "paused":false, "retained":false, "received":null}]
    }))).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none());
        assert!(
            host.invitation.is_none(),
            "an older invitation must not preempt the requested link"
        );
    });
    urls.try_send(Ok(crate::store::session_uri(
        &seed,
        crate::release_channel::ReleaseChannel::Public,
    )))
    .unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none());
        assert!(host.pending_windows.contains_key(&id));
    });
    assert!(matches!(
        requests.try_recv().unwrap(),
        Message::Request(QueuedRequest { request: Request::DesktopLaunch, .. })
    ));
    assert!(
        matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {request:Request::PrepareSession {session}, ..}) if session == id)
    );
    assert!(
        requests.try_recv().is_err(),
        "local session launch must not start sign-in preparation"
    );
}

#[gpui::test]
fn signed_out_launch_and_reopen_only_show_sign_in(cx: &mut gpui::TestAppContext) {
    let (host, responses, _requests) = launch_link(cx);
    responses.try_send(saved_status(serde_json::json!({"sessions":[], "enrolled":false}))).unwrap();
    cx.run_until_parked();
    let view = host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none());
        host.sign_in.clone().unwrap()
    });
    for _ in 0..3 {
        cx.update(|cx| {
            cx.dispatch_action(&menus::ShowSessions);
        });
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.sessions_window.is_none());
            assert_eq!(host.sign_in.as_ref(), Some(&view));
        });
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
    }
}

#[gpui::test]
fn retained_launch_waits_for_sign_in_and_resumes_once(cx: &mut gpui::TestAppContext) {
    use crate::ui::desktop::sign_in::flow::Step;
    let (host, responses, requests) = launch_link(cx);
    let session_id = crate::store::session_id(&[7; crate::store::SEED_BYTES]).unwrap();
    responses.try_send(saved_status(serde_json::json!({"sessions":[], "enrolled":false}))).unwrap();
    cx.run_until_parked();
    responses
        .try_send(WorkerResponse::from_reply(Ok(Reply::InvitationPreview(
            crate::store::InvitationPreview {
                session: session_id.clone(),
                retained: true,
                paused: false,
                received: None,
            },
        ))))
        .unwrap();
    cx.run_until_parked();
    while let Ok(message) = requests.try_recv() {
        assert!(!matches!(
            message,
            Message::Request(QueuedRequest { request: Request::PrepareSession { .. }, .. })
        ));
    }
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none());
        assert_eq!(host.opening.as_deref(), Some(session_id.as_str()));
        assert!(!host.joining);
    });
    host.update(cx, |host, cx| {
        host.enrolled = Some(true);
        host.account_profile = AccountProfile {
            agreement_required: false,
            replacement_required: false,
            display_name: Some("Robin".into()),
            unlocked: true,
            initialized: true,
        };
        let view = host.sign_in.clone().unwrap();
        view.update(cx, |view, cx| {
            view.flow.step = Step::Profile;
            view.profile(&host.account_profile, cx);
        });
    });
    cx.run_until_parked();
    host.update(cx, |host, cx| host.resume_sign_in_destination(cx));
    let mut starts = 0;
    while let Ok(message) = requests.try_recv() {
        if let Message::Request(QueuedRequest {
            request: Request::PrepareSession { session },
            ..
        }) = message
        {
            assert_eq!(session, session_id);
            starts += 1;
        }
    }
    assert_eq!(starts, 1);
    host.read_with(cx, |host, _| {
        assert!(host.sign_in.is_none());
        assert!(host.sessions_window.is_some());
    });
    assert_eq!(cx.update(|cx| cx.windows().len()), 1, "completion replaces the account window");
}

#[gpui::test]
fn unfinished_accounts_cannot_open_management(cx: &mut gpui::TestAppContext) {
    let (host, responses, _requests) = launch_link(cx);
    let mut response = saved_status(serde_json::json!({"sessions":[], "enrolled":true}));
    response.status_account_profile = Some(AccountProfile {
        agreement_required: false,
        replacement_required: false,
        display_name: None,
        unlocked: true,
        initialized: true,
    });
    responses.try_send(response).unwrap();
    cx.run_until_parked();
    cx.update(|cx| Shell::show_sessions(&host, cx));
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_none());
        assert!(host.sign_in.is_some());
    });
}

#[gpui::test]
fn ordinary_launch_waits_for_account_status_before_opening_a_window(cx: &mut gpui::TestAppContext) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, _requests, errors) = worker_queue::channel();
    let (_responses, receive) = async_channel::bounded(1);
    let (_urls, receive_urls) = async_channel::bounded(4);
    let host = cx.update(|cx| Shell::launch((send, receive, errors), receive_urls, true, cx));
    host.read_with(cx, |host, _| assert!(host.sessions_window.is_none()));
    assert!(cx.update(|cx| cx.windows().is_empty()));
}

#[gpui::test]
fn failed_recovery_waits_for_automatic_restart_and_success_clears_it(
    cx: &mut gpui::TestAppContext,
) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let host = cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
    drop(requests.try_recv().unwrap());
    let mut failed = WorkerResponse::new(Err(crate::platform::failure::MESSAGE.into()));
    failed.engine_lost = true;
    failed.recovery_action = Some(worker_recovery::Action::PrepareSignIn);
    responses.try_send(failed).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.engine_recovering, "sign-in preparation cannot swallow engine failure");
        assert!(host.error.is_empty());
    });
    let mut recovered = WorkerResponse::new(Ok(()));
    recovered.engine_recovered = true;
    responses.try_send(recovered).unwrap();
    cx.run_until_parked();
    responses.try_send(saved_status(serde_json::json!({"sessions":[],"enrolled":true}))).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(!host.engine_recovering);
        assert!(host.error.is_empty());
    });
}

#[gpui::test]
fn status_refresh_preserves_welcome_and_invitation_navigation(cx: &mut gpui::TestAppContext) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let (_urls, receive_urls) = async_channel::bounded(4);
    let host = cx.update(|cx| Shell::launch((send, receive, errors), receive_urls, true, cx));
    drop(requests.try_recv().unwrap());
    let mut status = serde_json::json!({"sessions":[], "enrolled":true,
        "invitations":[{"session":"older-invitation", "paused":true, "retained":false, "received":null}]});
    responses.try_send(saved_status(status.clone())).unwrap();
    cx.run_until_parked();
    let window = host.read_with(cx, |host, _| {
        assert!(host.invitation.is_none());
        host.sessions_window.unwrap()
    });
    status["availability"] = "Reconnecting…".into();
    responses.try_send(saved_status(status.clone())).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(
            host.invitation.is_none(),
            "background status must not replace explicitly selected welcome"
        )
    });
    cx.update(|cx| {
        window
            .update(cx, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.value() == Some("New session"))
                );
            })
            .unwrap()
    });
    let reference = crate::store::SessionReference::new([8; crate::store::SEED_BYTES]).unwrap();
    let selected = reference.internal_id().to_owned();
    host.update(cx, |host, _| host.preview_invitation_link(reference).unwrap());
    assert!(matches!(
        requests.try_recv().unwrap(),
        Message::Request(QueuedRequest { request: Request::PreviewDesktopInvitation { .. }, .. })
    ));
    assert!(requests.try_recv().is_err());
    responses
        .try_send(WorkerResponse::from_reply(Ok(Reply::InvitationPreview(
            crate::store::InvitationPreview {
                session: selected.clone(),
                paused: false,
                retained: false,
                received: None,
            },
        ))))
        .unwrap();
    cx.run_until_parked();
    assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
        request:Request::WatchInvitation {session}, ..
    }) if session == selected));
    status["availability"] = "Connected".into();
    responses.try_send(saved_status(status)).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert_eq!(
            host.invitation.as_ref().unwrap().session,
            selected,
            "status must preserve the explicitly selected invitation"
        )
    });
    assert!(requests.try_recv().is_err());
}

#[gpui::test]
fn initial_status_does_not_replace_a_late_launch_url(cx: &mut gpui::TestAppContext) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let (urls, receive_urls) = async_channel::bounded(4);
    let host = cx.update(|cx| Shell::launch((send, receive, errors), receive_urls, true, cx));
    drop(requests.try_recv().unwrap());
    urls.try_send(Ok(crate::examples::EXAMPLES[0].invitation.into())).unwrap();
    cx.run_until_parked();
    assert!(matches!(
        requests.try_recv().unwrap(),
        Message::Request(QueuedRequest { request: Request::PreviewDesktopInvitation { .. }, .. })
    ));
    responses.try_send(saved_status(serde_json::json!({
        "sessions":[], "enrolled":true,
        "invitations":[{"session":"older-invitation", "paused":true, "retained":false, "received":null}]
    }))).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(
            host.invitation.is_none(),
            "a late initial status must not preempt the requested example"
        )
    });
    assert!(requests.try_recv().is_err());
}

#[gpui::test]
fn ordinary_open_after_status_keeps_unresolved_invitation_out_of_the_window(
    cx: &mut gpui::TestAppContext,
) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, requests, errors) = worker_queue::channel();
    let (responses, receive) = async_channel::bounded(1);
    let (_urls, receive_urls) = async_channel::bounded(4);
    let host = cx.update(|cx| Shell::launch((send, receive, errors), receive_urls, false, cx));
    drop(requests.try_recv().unwrap());
    let directory = tempfile::tempdir().unwrap();
    let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
    let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
        panic!("status reply")
    };
    status.enrolled = true;
    status.account_profile = AccountProfile {
        agreement_required: false,
        replacement_required: false,
        display_name: Some("Robin".into()),
        unlocked: true,
        initialized: true,
    };
    status.invitations = vec![crate::store::InvitationPreview {
        session: "saved-invitation".into(),
        paused: true,
        retained: false,
        received: None,
    }];
    let allocation = status.invitations.as_ptr() as usize;
    responses.try_send(WorkerResponse::from_reply(Ok(Reply::Status(status)))).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.invitation.is_none());
        assert_eq!(host.saved_invitations.as_ptr() as usize, allocation);
    });
    cx.update(|cx| Shell::show_sessions(&host, cx));
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.invitation.is_none());
        assert!(host.sessions_window.is_some());
    });
    assert!(requests.try_recv().is_err());
}

#[gpui::test]
fn welcome_leaves_paused_and_pending_open_without_canceling_other_apps(
    cx: &mut gpui::TestAppContext,
) {
    let (host, window, requests) = test_support::host(cx);
    for joining in [false, true] {
        host.update(cx, |host, cx| {
            host.opening = Some("unavailable-example".into());
            host.joining = joining;
            host.pending_windows.insert("unavailable-example".into(), None);
            host.pending_windows.insert("other-app".into(), None);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    host.navigation_controls["join"].read(cx).focus(window)
                })
                .unwrap()
        });
        test_support::press(window, "enter", cx);
        host.read_with(cx, |host, _| {
            assert!(host.opening.is_none() && !host.joining);
            assert!(!host.pending_windows.contains_key("unavailable-example"));
            assert!(host.pending_windows.contains_key("other-app"));
        });
        cx.update(|cx| {
            window
                .update(cx, |_, window, _| {
                    assert!(
                        window
                            .accessibility_nodes_for_test()
                            .any(|(_, node)| node.value() == Some("New session"))
                    );
                })
                .unwrap()
        });
        assert!(requests.try_recv().is_err(), "navigation must not cancel another app's work");
    }
}

#[gpui::test]
fn initial_link_shows_transfer_progress_when_local_app_is_missing(cx: &mut gpui::TestAppContext) {
    let (host, responses, _requests) = launch_link(cx);
    responses.try_send(saved_status(serde_json::json!({"sessions":[], "enrolled":true}))).unwrap();
    cx.run_until_parked();

    responses
        .try_send(WorkerResponse::from_reply(Ok(Reply::Transfer {
            transfer: TransferStatus { kind: TransferKind::Running, ..Default::default() },
        })))
        .unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.sessions_window.is_some());
        assert_eq!(host.last_transfer.kind, TransferKind::Running);
    });
}

#[gpui::test]
fn cli_url_registers_open_intent_before_worker_response(cx: &mut gpui::TestAppContext) {
    let (send, requests, errors) = worker_queue::channel();
    let (_responses, receive) = async_channel::bounded(1);
    let (urls, receive_urls) = async_channel::bounded(4);
    let host = cx.new(|cx| Shell::new((send, receive, errors), receive_urls, cx));
    let seed = [7; crate::store::SEED_BYTES];
    let id = crate::store::session_id(&seed).unwrap();
    let uri = crate::store::session_uri(&seed, crate::release_channel::ReleaseChannel::Public);
    host.update(cx, |host, _| {
        host.enrolled = Some(true);
        host.account_profile = AccountProfile {
            agreement_required: false,
            replacement_required: false,
            display_name: Some("Robin".into()),
            unlocked: true,
            initialized: true,
        };
        host.sessions = vec![SessionRow {
            reference: String::new(),
            id: id.clone(),
            resolved_build_id: String::new(),
            build_state: crate::engine::builds::BuildState::Ready,
            app_name: Some("Session".into()),
        }];
    });
    urls.try_send(Ok(uri.clone())).unwrap();
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert!(host.pending_windows.contains_key(&id));
        assert_eq!(host.opening.as_deref(), Some(id.as_str()));
    });
    assert!(matches!(
        requests.try_recv().unwrap(),
        Message::Request(QueuedRequest { request: Request::DesktopLaunch, .. })
    ));
    assert!(
        matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {request:Request::PrepareSession {session}, ..}) if session == id)
    );
    // A new link opens a preview; an invalid link cannot start any engine work.
    host.update(cx, |host, _| {
        host.enrolled = Some(false);
        host.sessions.clear();
    });
    urls.try_send(Ok(uri)).unwrap();
    cx.run_until_parked();
    assert!(matches!(
        requests.try_recv().unwrap(),
        Message::Request(QueuedRequest { request: Request::PreviewDesktopInvitation { .. }, .. })
    ));
    urls.try_send(Ok("not a link".into())).unwrap();
    cx.run_until_parked();
    assert!(requests.is_empty());
    host.read_with(cx, |host, _| assert!(!host.error.is_empty()));
}
#[gpui::test]
fn reopen_sessions_window_reuses_host_and_does_not_borrow_it_during_first_render(
    cx: &mut gpui::TestAppContext,
) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
    let (send, _requests, errors) = worker_queue::channel();
    let (_responses, receive) = async_channel::bounded(1);
    let host = cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
    host.update(cx, |host, _| {
        host.enrolled = Some(true);
        host.account_profile = AccountProfile {
            agreement_required: false,
            replacement_required: false,
            display_name: Some("Robin".into()),
            unlocked: true,
            initialized: true,
        };
    });
    cx.update(|cx| Shell::show_sessions(&host, cx));
    let first = host.read_with(cx, |host, _| host.sessions_window.unwrap());
    cx.update(|cx| {
        first
            .update(cx, |host, window, _| {
                assert!(
                    host.focus.is_focused(window),
                    "window commands work before a control takes focus"
                );
            })
            .unwrap()
    });
    cx.update(|cx| Shell::show_sessions(&host, cx));
    assert_eq!(host.read_with(cx, |host, _| host.sessions_window.unwrap()), first);
    cx.update(|cx| {
        first.update(cx, |_, window, _| window.remove_window()).unwrap();
    });
    cx.update(|cx| Shell::show_sessions(&host, cx));
    let reopened = host.read_with(cx, |host, _| host.sessions_window.unwrap());
    assert_ne!(reopened, first);
}
