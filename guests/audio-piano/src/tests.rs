use super::*;
use playbit::runtime::native::NativeInstance;
use playbit::runtime::syscall::{
    AvailableVolume, Event, GuestContext, Host, Identity, Opcode, VolumeRights,
};
use playbit::runtime::{Frame, ViewNode};

struct NoAudio;
impl Host for NoAudio {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Piano headless test".into(),
        app_build: None,
        identity: Identity {
            account_id: [1; 16],
            device_id: [2; 16],
            instance_id: [3; 16],
            session_id: None,
            is_service: false,
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [4; 16],
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        }],
        startup_arguments: Default::default(),
        trace_catalog: None,
        required_capabilities: Default::default(),
        participants: vec![],
    }
}

fn check_error_scene(frame: &Frame, volume: &str) {
    let mut nodes = vec![frame.view().expect("piano submits a valid scene")];
    let (mut retry, mut status, mut controls, mut keyboard) = (false, false, false, false);
    while let Some(node) = nodes.pop() {
        match node {
            ViewNode::Theme { child, .. } => nodes.push(child),
            ViewNode::Column(children) | ViewNode::Row(children) => nodes.extend(children),
            ViewNode::Button { id: RETRY, label, disabled: false, .. } => {
                retry = label == "Retry audio"
            }
            ViewNode::Text { text, .. } => {
                status |= text.contains("Unsupported");
                controls |= text == volume;
            }
            ViewNode::Canvas { id: CANVAS, frames: CanvasFrames::Static, items, .. } => {
                keyboard = !items.is_empty()
            }
            _ => {}
        }
    }
    assert!(retry && status && controls && keyboard);
}

#[test]
#[allow(
    unsafe_code,
    reason = "The test owns the static native callbacks through same-thread teardown."
)]
fn unsupported_audio_renders_retry_and_keeps_controls_usable() {
    // This host never advertises audio, so the real guest registration and
    // error/retry path execute without creating an audio endpoint or device.
    // SAFETY: callbacks are linked static code; this test owns app state and
    // executes and destroys it on the same thread.
    let mut guest =
        unsafe { NativeInstance::new(context(), Box::new(NoAudio), playbit_api::native_app()) }
            .unwrap();
    check_error_scene(&guest.pump().unwrap(), "Volume 70%");
    guest
        .queue_event(
            guest.generation(),
            Event { kind: EventKind::Click, target: RETRY, data: vec![] },
        )
        .unwrap();
    check_error_scene(&guest.pump().unwrap(), "Volume 70%");
    guest
        .queue_event(
            guest.generation(),
            Event { kind: EventKind::Click, target: LOUDER, data: vec![] },
        )
        .unwrap();
    check_error_scene(&guest.pump().unwrap(), "Volume 80%");
}
