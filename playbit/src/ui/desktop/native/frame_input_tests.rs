//! Actual callbacks replace Continuous demand after a held click response.
use super::*;
use playbit_api::{
    abi::operations as abi,
    context::Context,
    ui::{ButtonStyle, CanvasFrames, MenuItem},
};
use std::ffi::c_void;
struct State {
    replace: bool,
    changed: bool,
    yield_after_start: bool,
    started: bool,
}
unsafe extern "C" fn init(
    _: *mut abi::Context,
    user: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    unsafe {
        output.write(
            Box::into_raw(Box::new(State {
                replace: user as usize == 1,
                changed: false,
                yield_after_start: user as usize == 2,
                started: false,
            }))
            .cast(),
        );
    }
    0
}
unsafe extern "C" fn destroy(state: *mut c_void) {
    unsafe {
        drop(Box::from_raw(state.cast::<State>()));
    }
}
unsafe extern "C" fn turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let state = unsafe { &mut *state.cast::<State>() };
    if state.yield_after_start && state.started {
        // This guest processes only the already queued older event on this turn.
        // The action that changes the menu must run on a later turn.
        state.yield_after_start = false;
        let mut header = abi::Event::default();
        let mut scratch = [0u8; 16];
        let length = unsafe {
            abi::pb_event_poll(raw, &mut header, scratch.as_mut_ptr(), scratch.len() as u32)
        };
        if length == 0 && header.kind == EventKind::Click as u32 && header.target == 2 {
            return 0;
        }
        return -1;
    }
    state.started = true;
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut cx, mut view) = unsafe { Context::from_raw(raw, &mut context_state).unwrap() };
    let mut reconnected = false;
    let mut frame = None;
    for event in cx.raw_events() {
        if event.kind == EventKind::Click {
            match event.target {
                2 => state.changed = true,
                7 => reconnected = true,
                _ => return -1,
            }
        }
        if event.kind == EventKind::Frame {
            frame = Some(event.target);
        }
    }
    if let Some(target) = frame {
        cx.log(&format!("Display frame {target}"));
    }
    if reconnected {
        cx.log("Reconnect executed");
    }
    view.column_start();
    view.button(2, "Change demand", ButtonStyle::Default, false);
    let frames = if state.changed && !state.replace {
        CanvasFrames::Static
    } else {
        CanvasFrames::Continuous
    };
    if !state.changed {
        view.context_menu_start(&[MenuItem::Action {
            id: 7,
            label: "Reconnect",
            icon: None,
            checked: None,
            disabled: false,
            destructive: false,
        }]);
    }
    view.canvas(if state.changed && state.replace { 18 } else { 17 }, 100, 100, frames, &[]);
    if !state.changed {
        view.context_menu_end();
    }
    view.column_end();
    view.submit(&mut cx).unwrap();
    0
}
fn descriptor(mode: usize) -> abi::App {
    abi::App {
        size: std::mem::size_of::<abi::App>() as u32,
        written: 0,
        init: Some(init),
        turn: Some(turn),
        destroy: Some(destroy),
        user_data: mode as *mut c_void,
    }
}

#[test]
fn close_applies_queued_input_before_rechecking_the_next_menu_selection() {
    let directory = tempfile::tempdir().unwrap();
    let services = Services::open(directory.path(), || {}).unwrap();
    let root = services.store.create_volume("Closing match", false).unwrap().id;
    let mut apps = Apps::new(Arc::new(|| {}));
    let (wake, wake_events) = mpsc::sync_channel(1);
    let (output, events) = async_channel::bounded(OUTPUT_CAPACITY);
    let launches = vec![Launch { title: "Closing match".into(), root, app: descriptor(2) }];
    let slots = unsafe { attach(&mut apps, &services, launches, &wake, &output) }.unwrap();
    let Output::Opened { handle, view: Some(ViewNode::Theme { child, .. }), .. } = receive(&events)
    else {
        panic!("initialized match")
    };
    let ViewNode::Column(children) = child.as_ref() else { panic!("controls") };
    let ViewNode::ContextMenu { items, .. } = &children[1] else { panic!("context menu") };
    let menu = crate::ui::desktop::popup::menu_digest(items);
    apps.input(
        handle.generation,
        Event { object_id: 0, kind: EventKind::Click, target: 2, data: vec![] },
    )
    .unwrap();
    assert!(handle.send(
        Input {
            kind: EventKind::Frame,
            target: 17,
            data: 16667u32.to_le_bytes().to_vec(),
            revision: None
        },
        None
    ));
    let click = |target| Input { kind: EventKind::Click, target, data: vec![], revision: None };
    // The channel closes with a disposable frame, a menu-changing action and
    // its now-obsolete choice. An older runtime event gets its own first turn.
    assert!(handle.send(click(2), None));
    assert!(handle.send_menu(click(7), handle.epoch(), menu));
    handle.close();
    drive(&mut apps, slots, wake_events, &output, &AtomicBool::new(false), |_| Ok(false)).unwrap();
    let mut changed = false;
    loop {
        match receive(&events) {
            Output::Frame { result: Ok(scene), logs, .. } => {
                changed |= scene.as_ref().is_some_and(|view| {
                    crate::ui::desktop::canvas::canvas_frame_target(view).is_none()
                });
                assert!(!logs.iter().any(|line| line.contains("Reconnect executed")));
                assert!(!logs.iter().any(|line| line.contains("Display frame")));
            }
            Output::Closed(id) => {
                assert_eq!(id, handle.generation);
                break;
            }
            _ => panic!("unexpected close output"),
        }
    }
    assert!(changed, "the earlier action ran before confirmed shutdown");
    assert!(!handle.input_pending());
}

#[test]
fn click_before_display_changes_demand_without_admitting_an_obsolete_frame() {
    // Keep the click's actual worker result unadopted while attempting a frame.
    // Only after releasing it do we use the new target, or stop for Static.
    for replace in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut worker = unsafe {
            Worker::start(directory.path().into(), move |services| {
                let root = services.store.create_volume("Canvas fixture", false).unwrap().id;
                let app = descriptor(usize::from(replace));
                Ok(vec![Launch { title: "Canvas fixture".into(), root, app }])
            })
        }
        .unwrap();
        let output = worker.output();
        let Output::Opened { handle, view, .. } = receive(&output) else { panic!() };
        let receipt = |target| {
            (0..32)
                .find_map(|_| {
                    let response = receive(&output);
                    match &response {
                        Output::Frame { ack, pending: Some(_), .. } if ack.target == target => {
                            Some(response)
                        }
                        Output::Frame { result: Ok(_), pending: None, .. } => None,
                        _ => panic!("unexpected native output"),
                    }
                })
                .expect("input receipt")
        };
        assert_eq!(
            view.as_ref().and_then(crate::ui::desktop::canvas::canvas_frame_target),
            Some(17)
        );
        let initial = view.as_ref().unwrap();
        let ViewNode::Theme { child, .. } = initial else { panic!("themed scene") };
        let ViewNode::Column(children) = child.as_ref() else { panic!("controls") };
        let ViewNode::ContextMenu { items, .. } = &children[1] else { panic!("context menu") };
        let menu = crate::ui::desktop::popup::menu_digest(items);
        let selection =
            || Input { kind: EventKind::Click, target: 7, data: vec![], revision: None };
        let epoch = handle.epoch();
        handle.failed();
        assert!(handle.send_menu(selection(), epoch, menu));
        assert!(
            matches!(receipt(7), Output::Frame { result: Err(error), .. } if error.contains("earlier input failed")),
            "a current menu still respects input failure ordering"
        );
        assert!(
            handle.send(
                Input { kind: EventKind::Click, target: 2, data: vec![], revision: None },
                None
            )
        );
        let frame = |target| Input {
            kind: EventKind::Frame,
            target,
            data: 16667u32.to_le_bytes().to_vec(),
            revision: None,
        };
        assert!(!handle.send(frame(17), None));
        let response = receipt(2);
        let Output::Frame { result: Ok(Some(view)), .. } = &response else { panic!() };
        let target = crate::ui::desktop::canvas::canvas_frame_target(view);
        assert_eq!(target, replace.then_some(18));
        assert!(!handle.send(frame(17), None), "response delivery still owns input");
        // The window still shows the old menu while this worker response is held.
        let epoch = handle.epoch();
        assert!(handle.send(
            Input { kind: EventKind::Click, target: 999, data: vec![], revision: None },
            Some(epoch)
        ));
        assert!(
            matches!(receipt(999), Output::Frame { result: Err(error), .. } if error.contains("InvalidArgument"))
        );
        assert!(handle.send_menu(selection(), epoch, menu));
        let obsolete = receive(&output);
        assert!(matches!(
            obsolete,
            Output::Frame { result: Ok(_), pending: None, ack: InputAck { target: 0, .. }, ref logs, .. }
                if !logs.iter().any(|line| line.contains("Reconnect executed"))
        ));
        drop(obsolete);
        assert_eq!(
            handle.epoch(),
            epoch + 1,
            "canceling a stale choice preserves the real failure"
        );
        drop(response); // The GUI has now adopted the new demand.
        if let Some(target) = target {
            assert!(handle.send(frame(target), None));
            assert!(matches!(receive(&output), Output::Frame { result: Ok(_), logs, .. }
                if logs.iter().any(|line| line.contains("Display frame 18"))));
        } else {
            assert!(output.is_empty());
        }
        worker.shutdown();
    }
}
