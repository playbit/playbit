#![allow(clippy::unwrap_used, reason = "Tests assert native session invitation behavior.")]
use super::*;
use crate::engine::{Request, ipc, reply::Reply};
use std::{
    io::Read,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

fn invitation_server(
    profile: &std::path::Path,
    sessions: Vec<[u8; 32]>,
) -> (Arc<AtomicUsize>, thread::JoinHandle<()>) {
    crate::paths::private_directory(profile, "control").unwrap();
    let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let server = thread::spawn(move || {
        for (index, session) in sessions.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut stream).unwrap(),
            )
            .unwrap();
            let Request::Invitation { session: actual } = request else {
                panic!("expected session invitation request");
            };
            assert_eq!(actual, crate::identifier::encode(&session));
            // Set this before writing: when the synchronous background runnable
            // returns, the test can cancel its still-pending foreground result.
            count.store(index + 1, Ordering::Release);
            let reply = ipc::codec::encode(
                &Reply::Invitation {
                    invitation: format!("https://playbit.app/join/invitation-{index}"),
                    availability: "available".into(),
                },
                4096,
            )
            .unwrap();
            ipc::write(&mut stream, &reply).unwrap();
        }
    });
    (received, server)
}

fn retained_handle(
    profile: &std::path::Path,
    generation: u64,
    session: [u8; 32],
) -> (Handle, async_channel::Receiver<super::super::super::worker::Message>) {
    let (mut handle, inputs) = Handle::fixture(generation);
    handle.session = Some(session);
    handle.startup = Some(Arc::new((
        profile.to_owned(),
        crate::engine::session_instance::Context {
            instance: crate::store::Id::from_bytes([1; 16]),
            handle: crate::store::Id::from_bytes([2; 16]),
            incarnation: crate::store::Id::from_bytes([3; 16]),
            owner_generation: generation,
        },
    )));
    (handle, inputs)
}

fn add_font(cx: &gpui::TestAppContext) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
            "../../../../../../assets/InterVariable.ttf"
        ))])
        .unwrap();
}

fn copy_button_bounds(window: &Window) -> Option<gpui::Bounds<gpui::Pixels>> {
    let (_, node) = window
        .accessibility_nodes_for_test()
        .find(|(_, node)| node.label() == Some("Copy session link"))?;
    assert_eq!(node.role(), gpui::accesskit::Role::Button);
    let bounds = node.bounds().unwrap();
    Some(gpui::Bounds::from_corners(
        gpui::point(px(bounds.x0 as f32), px(bounds.y0 as f32)),
        gpui::point(px(bounds.x1 as f32), px(bounds.y1 as f32)),
    ))
}

#[gpui::test]
fn titlebar_pointer_and_keyboard_copy_current_session_without_guest_input(
    cx: &mut gpui::TestAppContext,
) {
    // Exercise titlebar and menu copies against the exact retained session.
    add_font(cx);
    let profile = tempfile::tempdir().unwrap();
    let session = [9; 32];
    let (_, server) = invitation_server(profile.path(), vec![session, session, session]);
    let (handle, inputs) = retained_handle(profile.path(), 7, session);
    let window = cx.update(|cx| open("Guest".into(), Some(handle), None, true, cx).unwrap());
    window.update(cx, |_, window, _| window.activate_window()).unwrap();
    cx.run_until_parked();
    while inputs.try_recv().is_ok() {}
    let bounds = window.update(cx, |_, window, _| copy_button_bounds(window).unwrap()).unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.simulate_click(bounds.center(), Default::default());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().unwrap(),
        "https://playbit.app/join/invitation-0"
    );
    assert!(inputs.is_empty(), "titlebar activation must not send guest events");
    visual.simulate_mouse_move(gpui::point(px(80.), px(80.)), None, Default::default());
    cx.run_until_parked();
    window
        .read_with(cx, |this, _| {
            assert!(this.session_link.as_ref().unwrap().copied.is_some());
        })
        .unwrap();
    cx.executor().advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    window
        .update(cx, |this, window, cx| {
            this.session_link.as_ref().unwrap().control.read(cx).focus(window);
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "space");
    cx.dispatch_key_up(window.into(), gpui::Keystroke::parse("space").unwrap());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().unwrap(),
        "https://playbit.app/join/invitation-1"
    );
    assert!(inputs.is_empty(), "keyboard activation must not reach the guest");

    cx.executor().advance_clock(std::time::Duration::from_millis(1999));
    cx.run_until_parked();
    window
        .read_with(cx, |this, _| {
            assert!(
                this.session_link.as_ref().unwrap().copied.is_some(),
                "a second successful copy renews feedback beyond the first deadline"
            );
        })
        .unwrap();
    cx.executor().advance_clock(std::time::Duration::from_millis(1));
    cx.run_until_parked();
    window
        .read_with(cx, |this, _| {
            assert!(
                this.session_link.as_ref().unwrap().copied.is_none(),
                "success feedback expires two seconds after the most recent copy"
            );
        })
        .unwrap();
    // Opting out while the copy button has keyboard focus returns ownership to
    // the guest; the Window menu still targets the same retained session.
    window
        .update(cx, |this, _, cx| {
            let view = ViewNode::Theme {
                window_background_specified: false,
                window_size_constraints: None,
                window_style: Some(WindowStyle::HIDE_SESSION_BUTTON),
                theme: Theme::light(),
                child: Box::new(ViewNode::Column(vec![])),
            };
            assert!(this.apply(view, InputAck::default(), cx));
            assert!(!this.close(cx), "borderless nonclosable window declines Close");
            assert!(!this.closing);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |this, window, cx| {
            assert!(copy_button_bounds(window).is_none());
            assert!(this.focus.is_focused(window));
            assert!(window.is_action_available(&CopySessionLink, cx));
            window.dispatch_action(Box::new(CopySessionLink), cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().unwrap(),
        "https://playbit.app/join/invitation-2"
    );
    server.join().unwrap();
}

#[gpui::test]
fn floating_session_button_preserves_guest_pointer_and_titlebar_drag_outside_its_bounds(
    cx: &mut gpui::TestAppContext,
) {
    add_font(cx);
    let profile = tempfile::tempdir().unwrap();
    let session = [9; 32];
    let (_, server) = invitation_server(profile.path(), vec![session]);
    let (handle, inputs) = retained_handle(profile.path(), 7, session);
    let scene = |window_style, child| ViewNode::Theme {
        window_style,
        window_size_constraints: None,
        theme: Theme::light(),
        window_background_specified: false,
        child: Box::new(child),
    };
    let titled = WindowStyle::STANDARD | WindowStyle::TRANSPARENT_TITLEBAR;
    let window = cx.update(|cx| {
        open(
            "Guest".into(),
            Some(handle),
            Some(scene(Some(titled), ViewNode::Column(vec![]))),
            true,
            cx,
        )
        .unwrap()
    });
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let bounds = window.update(cx, |_, window, _| copy_button_bounds(window).unwrap()).unwrap();
    let beside = gpui::point(bounds.left() - px(2.), bounds.center().y);
    assert!(cx.window_can_drag_at(window.into(), beside));
    assert!(!cx.window_can_drag_at(window.into(), bounds.center()));
    let set_canvas = |style, id, cx: &mut gpui::TestAppContext| {
        let canvas = ViewNode::Canvas {
            id,
            width: 640,
            height: 560,
            frames: playbit_api::ui::CanvasFrames::Static,
            extent: CanvasExtent::Window,
            clear_color: None,
            items: vec![],
        };
        window
            .update(cx, |this, _, cx| {
                assert!(this.apply(scene(style, canvas), InputAck::default(), cx));
            })
            .unwrap();
        cx.run_until_parked();
    };
    set_canvas(None, 0, cx);
    assert!(cx.window_can_drag_at(window.into(), beside));
    assert!(!cx.window_can_drag_at(window.into(), bounds.center()));
    set_canvas(Some(titled), 1, cx);
    assert!(cx.window_can_drag_at(window.into(), beside));
    assert!(!cx.window_can_drag_at(window.into(), bounds.center()));
    while inputs.try_recv().is_ok() {}
    visual.simulate_click(beside, Default::default());
    assert!(inputs.is_empty(), "a titlebar press belongs to the window");

    // A drag that starts on the pitch can cross the titlebar and finish there.
    let pitch = gpui::point(beside.x, px(100.));
    let left = gpui::MouseButton::Left;
    visual.simulate_mouse_down(pitch, left, Default::default());
    visual.simulate_mouse_move(beside, Some(left), Default::default());
    visual.simulate_mouse_up(beside, left, Default::default());
    for phase in [1, 0, 2] {
        let input = inputs.try_recv().unwrap().input;
        assert_eq!((input.kind, input.target), (EventKind::Pointer, 1));
        assert_eq!(
            playbit_api::abi::syscall::PointerInput::decode(&input.data).unwrap().phase,
            phase
        );
    }
    assert!(inputs.is_empty());
    for style in [None, Some(WindowStyle::NONE)] {
        set_canvas(style, 1, cx);
        let overlay =
            window.update(cx, |_, window, _| copy_button_bounds(window).unwrap()).unwrap();
        assert_eq!(overlay, bounds);
        while inputs.try_recv().is_ok() {}
        visual.simulate_click(beside, Default::default());
        assert_eq!(inputs.try_recv().unwrap().input.kind, EventKind::Pointer);
        assert_eq!(inputs.try_recv().unwrap().input.kind, EventKind::Pointer);
    }
    // Hide between down and up. A later drag from the canvas onto the
    // restored button must not complete the old press.
    visual.simulate_mouse_down(bounds.center(), gpui::MouseButton::Left, Default::default());
    set_canvas(Some(WindowStyle::HIDE_SESSION_BUTTON), 1, cx);
    visual.simulate_mouse_up(bounds.center(), gpui::MouseButton::Left, Default::default());
    set_canvas(Some(WindowStyle::NONE), 1, cx);
    visual.simulate_mouse_down(beside, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_up(bounds.center(), gpui::MouseButton::Left, Default::default());
    cx.run_until_parked();
    assert!(cx.read_from_clipboard().is_none());
    set_canvas(Some(titled), 1, cx);
    while inputs.try_recv().is_ok() {}
    visual.simulate_click(bounds.center(), Default::default());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().unwrap(),
        "https://playbit.app/join/invitation-0"
    );
    assert!(inputs.is_empty(), "the copy button owns its hitbox over the game");
    server.join().unwrap();

    // A menu-initiated copy can still be pending when the button reappears.
    set_canvas(Some(WindowStyle::HIDE_SESSION_BUTTON), 1, cx);
    window
        .update(cx, |this, _, _| {
            this.session_link.as_mut().unwrap().copying =
                Some(Copying { _task: gpui::Task::ready(()), state: Default::default() });
        })
        .unwrap();
    set_canvas(Some(WindowStyle::NONE), 1, cx);
    window
        .update(cx, |this, window, cx| {
            this.focus.focus(window);
            window.focus_next();
            assert!(!this.session_link.as_ref().unwrap().control.read(cx).focused(window));
        })
        .unwrap();
}

#[gpui::test]
fn copy_feedback_expires_only_for_its_current_window_owner(cx: &mut gpui::TestAppContext) {
    // Complete a copy before close/reopen or reconstruction, then make another
    // copy and advance virtual time across both the old and new deadlines.
    add_font(cx);
    for reconstruct in [false, true] {
        let profile = tempfile::tempdir().unwrap();
        let session = [9; 32];
        let next_session = if reconstruct { [10; 32] } else { session };
        let generation = if reconstruct { 8 } else { 7 };
        let (_, server) = invitation_server(profile.path(), vec![session, next_session]);
        let (handle, _inputs) = retained_handle(profile.path(), 7, session);
        let mut window = cx.update(|cx| {
            let window = open("Guest".into(), Some(handle), None, true, cx).unwrap();
            super::super::bind_window(window, false, Rc::new(Cell::new(1)), "Guest".into(), cx);
            window
        });
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                this.copy_session_link(
                    profile.path().to_owned(),
                    crate::identifier::encode(&session),
                    7,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |this, _| {
                assert!(this.session_link.as_ref().unwrap().copied.is_some());
            })
            .unwrap();
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        let entity = window.entity(cx).unwrap();
        let (replacement, _replacement_inputs) = retained_handle(profile.path(), 8, next_session);
        window
            .update(cx, |this, window, cx| {
                if reconstruct {
                    this.reconstruct(replacement, None, cx);
                } else {
                    assert!(this.close(cx));
                    window.remove_window();
                }
                assert!(this.session_link.as_ref().unwrap().copied.is_none());
            })
            .unwrap();
        if !reconstruct {
            window = cx.update(|cx| {
                let reopened = super::super::show("Guest".into(), entity, true, cx).unwrap();
                super::super::bind_window(
                    reopened,
                    false,
                    Rc::new(Cell::new(1)),
                    "Guest".into(),
                    cx,
                );
                reopened
            });
        }
        window
            .update(cx, |this, _, cx| {
                this.copy_session_link(
                    profile.path().to_owned(),
                    crate::identifier::encode(&next_session),
                    generation,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "https://playbit.app/join/invitation-1"
        );
        server.join().unwrap();
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        window
            .read_with(cx, |this, _| {
                assert!(
                    this.session_link.as_ref().unwrap().copied.is_some(),
                    "an old owner's deadline cannot clear current feedback"
                );
            })
            .unwrap();
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        window
            .read_with(cx, |this, _| {
                assert!(this.session_link.as_ref().unwrap().copied.is_none());
            })
            .unwrap();
    }
}

#[gpui::test]
fn completed_invitation_cannot_change_clipboard_after_reopen_or_reconstruction(
    cx: &mut gpui::TestAppContext,
) {
    // Finish IPC while holding its foreground result, then reopen the retained
    // window or reconstruct its owner before allowing clipboard completion.
    add_font(cx);
    for reconstruct in [false, true] {
        let profile = tempfile::tempdir().unwrap();
        let session = [9; 32];
        let (received, server) = invitation_server(profile.path(), vec![session]);
        let (handle, _inputs) = retained_handle(profile.path(), 7, session);
        let window = cx.update(|cx| {
            let window = open("Guest".into(), Some(handle), None, true, cx).unwrap();
            super::super::bind_window(window, false, Rc::new(Cell::new(1)), "Guest".into(), cx);
            window
        });
        cx.run_until_parked();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
        window
            .update(cx, |this, _, cx| {
                this.copy_session_link(
                    profile.path().to_owned(),
                    crate::identifier::encode(&session),
                    7,
                    cx,
                );
            })
            .unwrap();
        // One tick runs at most one runnable. Stop after IPC finishes, before
        // its foreground continuation can publish the clipboard result.
        while received.load(Ordering::Acquire) == 0 {
            assert!(cx.executor().tick());
        }
        server.join().unwrap();
        let entity = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        let (replacement, _replacement_inputs) = retained_handle(profile.path(), 8, [10; 32]);
        window
            .update(cx, |this, window, cx| {
                if reconstruct {
                    this.reconstruct(replacement, None, cx);
                } else {
                    assert!(this.close(cx));
                    window.remove_window();
                }
            })
            .unwrap();
        if !reconstruct {
            cx.update(|cx| {
                let reopened = super::super::show("Guest".into(), entity, true, cx).unwrap();
                super::super::bind_window(
                    reopened,
                    false,
                    Rc::new(Cell::new(1)),
                    "Guest".into(),
                    cx,
                );
            });
        }
        cx.run_until_parked();
        assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "sentinel");
    }
}

#[test]
fn cancelling_stalled_invitation_interrupts_socket_read_and_releases_worker() {
    // Hold an actual IPC reply on OS threads, then cancel its registered socket
    // and require both peer EOF and worker completion within a bounded wait.
    let profile = tempfile::tempdir().unwrap();
    crate::paths::private_directory(profile.path(), "control").unwrap();
    let listener = UnixListener::bind(profile.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
    let (ready, accepted) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
            &ipc::read(&mut stream).unwrap(),
        )
        .unwrap();
        let Request::Invitation { session } = request else {
            panic!("expected session invitation request");
        };
        assert_eq!(session, crate::identifier::encode(&[9; 32]));
        ready.send(()).unwrap();
        let mut byte = [0];
        // A timeout also drops the server, releasing a regressed blocking client
        // before the main thread joins it and reports the failed assertion.
        matches!(stream.read(&mut byte), Ok(0))
    });
    let state = Arc::new(Mutex::new(CopyState { cancelled: false, socket: None }));
    let worker_state = state.clone();
    let path = profile.path().to_owned();
    let (finished, completion) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let result = invitation(&path, crate::identifier::encode(&[9; 32]), &worker_state);
        finished.send(result.map_err(|error| error.to_string())).unwrap();
    });
    accepted.recv_timeout(Duration::from_secs(2)).unwrap();
    state.lock().unwrap().cancel();
    let result = completion.recv_timeout(Duration::from_secs(2));
    let peer_closed = server.join().unwrap();
    worker.join().unwrap();
    assert!(peer_closed, "cancellation must shut down the registered IPC socket");
    assert!(result.unwrap().is_err(), "cancelled work must terminate without a reply");
}

#[test]
fn cancelling_before_invitation_starts_prevents_engine_request() {
    // Cancel before invoking an OS worker, then require either no connection or
    // peer EOF without request bytes. Close the peer to bound a regressed read.
    let profile = tempfile::tempdir().unwrap();
    crate::paths::private_directory(profile.path(), "control").unwrap();
    let listener = UnixListener::bind(profile.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = Arc::new(Mutex::new(CopyState { cancelled: false, socket: None }));
    state.lock().unwrap().cancel();
    let path = profile.path().to_owned();
    let (finished, completion) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        finished
            .send(
                invitation(&path, crate::identifier::encode(&[9; 32]), &state)
                    .map_err(|error| error.to_string()),
            )
            .unwrap();
    });
    let result = completion.recv_timeout(Duration::from_secs(2));
    let no_request = match listener.accept() {
        Ok((mut stream, _)) => {
            // The canceled worker may already have closed its peer; macOS can
            // reject timeout options then. A nonblocking read observes EOF.
            stream.set_nonblocking(true).unwrap();
            let eof = matches!(stream.read(&mut [0]), Ok(0));
            let _ = stream.shutdown(std::net::Shutdown::Both);
            eof
        }
        Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
    };
    worker.join().unwrap();
    assert!(no_request, "pre-cancelled work must not send request bytes");
    assert!(result.unwrap().is_err());
}

#[gpui::test]
fn window_lifecycle_shuts_down_registered_copy_socket(cx: &mut gpui::TestAppContext) {
    // Install a pending copy with an actual socket pair, then exercise each
    // owner-lifecycle path and observe peer EOF rather than only task removal.
    add_font(cx);
    for lifecycle in 0..3 {
        let profile = tempfile::tempdir().unwrap();
        let (handle, _inputs) = retained_handle(profile.path(), 7, [9; 32]);
        let window = cx.update(|cx| {
            let window = open("Guest".into(), Some(handle), None, true, cx).unwrap();
            super::super::bind_window(window, false, Rc::new(Cell::new(1)), "Guest".into(), cx);
            window
        });
        cx.run_until_parked();
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let state = Arc::new(Mutex::new(CopyState { cancelled: false, socket: Some(socket) }));
        let (replacement, _replacement_inputs) = retained_handle(profile.path(), 8, [10; 32]);
        window
            .update(cx, |this, _, cx| {
                this.session_link.as_mut().unwrap().copying = Some(Copying {
                    _task: cx.spawn(async move |_, _| std::future::pending::<()>().await),
                    state: state.clone(),
                });
                match lifecycle {
                    0 => assert!(this.close(cx)),
                    1 => this.reconstruct(replacement, None, cx),
                    2 => this.stop_inputs(cx),
                    _ => unreachable!(),
                }
            })
            .unwrap();
        assert!(state.lock().unwrap().cancelled);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "owner transition must cancel socket I/O");
    }
}
