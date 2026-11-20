//! Actual callbacks replace Continuous demand after a held click response.
use super::*;
use playbit_api::{
    abi::operations as abi,
    context::Context,
    ui::{ButtonStyle, CanvasFrames},
};
use std::ffi::c_void;
struct State {
    replace: bool,
    changed: bool,
}
unsafe extern "C" fn init(
    _: *mut abi::Context,
    user: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    unsafe {
        output.write(
            Box::into_raw(Box::new(State { replace: user as usize == 1, changed: false })).cast(),
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
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut cx, mut view) = unsafe { Context::from_raw(raw, &mut context_state).unwrap() };
    for event in cx.raw_events() {
        if event.kind == EventKind::Click {
            state.changed = true;
        }
        if event.kind == EventKind::Frame {
            assert!(state.changed && state.replace && event.target == 18);
        }
    }
    view.column_start();
    view.button(2, "Change demand", ButtonStyle::Default, false);
    let frames = if state.changed && !state.replace {
        CanvasFrames::Static
    } else {
        CanvasFrames::Continuous
    };
    view.canvas(if state.changed && state.replace { 18 } else { 17 }, 100, 100, frames, &[]);
    view.column_end();
    view.submit(&mut cx).unwrap();
    0
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
                let app = abi::App {
                    size: std::mem::size_of::<abi::App>() as u32,
                    written: 0,
                    init: Some(init),
                    turn: Some(turn),
                    destroy: Some(destroy),
                    user_data: usize::from(replace) as *mut c_void,
                };
                Ok(vec![Launch { title: "Canvas fixture".into(), root, app }])
            })
        }
        .unwrap();
        let output = worker.output();
        let Output::Opened { handle, view, .. } = receive(&output) else { panic!() };
        assert_eq!(
            view.as_ref().and_then(crate::ui::desktop::canvas::canvas_frame_target),
            Some(17)
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
        let response = (0..32)
            .find_map(|_| {
                let response = receive(&output);
                match &response {
                    Output::Frame { ack: InputAck { target: 2, .. }, .. } => Some(response),
                    Output::Frame { result: Ok(_), .. } => None, // Earlier readiness presentation.
                    _ => panic!("unexpected native output"),
                }
            })
            .expect("click response");
        let Output::Frame { result: Ok(Some(view)), .. } = &response else { panic!() };
        let target = crate::ui::desktop::canvas::canvas_frame_target(view);
        assert_eq!(target, replace.then_some(18));
        assert!(!handle.send(frame(17), None), "response delivery still owns input");
        drop(response); // The GUI has now adopted the new demand.
        if let Some(target) = target {
            assert!(handle.send(frame(target), None));
            assert!(matches!(receive(&output), Output::Frame { result: Ok(_), .. }));
        } else {
            assert!(output.is_empty());
        }
        worker.shutdown();
    }
}
