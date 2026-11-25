//! Drive the registered guest through its native input ABI and inspect the scenes it submits.
#![cfg(not(target_arch = "wasm32"))]

extern crate input_lab as _;

use playbit::runtime::{
    ViewNode,
    native::NativeInstance,
    syscall::{
        AvailableVolume, Event, EventKind, GuestContext, Host, HostError, Identity, Opcode,
        VolumeRights, validate_input,
    },
};
use playbit_api::abi::input::{self, Gamepad, Gesture, Input, Key, Keyboard, Pen, Pointer, Scroll};
use playbit_api::context::Surface;

const WINDOW: u32 = 73;
const CLIENT: u32 = 9;

struct InputHost;

impl Host for InputHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }

    fn supported_features(&self) -> &[&str] {
        &[input::FEATURE]
    }
}

struct Lab {
    guest: NativeInstance,
    view: ViewNode,
}

impl Lab {
    fn open() -> Self {
        let context = GuestContext {
            app_name: "Input Lab".into(),
            app_build: None,
            identity: Identity {
                display_name: String::new(),
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
                rights: VolumeRights::READ,
                is_root: true,
                role: 0,
            }],
            startup_arguments: Default::default(),
            trace_catalog: None,
            required_capabilities: [input::FEATURE.to_owned()].into(),
            participants: vec![],
        };
        // SAFETY: the app's linked callbacks live for the test, and the instance drops on this thread.
        #[allow(unsafe_code)]
        let mut guest = unsafe {
            NativeInstance::new(context, Box::new(InputHost), playbit_api::native_app()).unwrap()
        };
        let view = guest.pump().unwrap().into_native_parts().1.expect("initial scene");
        Self { guest, view }
    }

    fn deliver(&mut self, events: impl IntoIterator<Item = Event>) {
        for event in events {
            validate_input(&self.view, &event).expect("the published scene accepts this input");
            self.guest.queue_event(self.guest.generation(), event).unwrap();
        }
        if let Some(view) = self.guest.pump().unwrap().into_native_parts().1 {
            self.view = view;
        }
    }

    fn click(&mut self, target: u32) {
        self.deliver([Event { object_id: WINDOW, kind: EventKind::Click, target, data: vec![] }]);
    }

    fn mask(&self) -> u32 {
        let ViewNode::Theme { window_input: Some(mask), .. } = &self.view else {
            panic!("the lab must submit its input subscription");
        };
        *mask
    }

    fn readout(&self, selected_tab: u32) -> String {
        let mut nodes = vec![&self.view];
        let mut selected = None;
        let mut lines = Vec::new();
        while let Some(node) = nodes.pop() {
            match node {
                ViewNode::Theme { child, .. } | ViewNode::Scroll { child, .. } => nodes.push(child),
                ViewNode::Column(children) | ViewNode::Row(children) => {
                    nodes.extend(children.iter().rev())
                }
                ViewNode::SegmentedControl { id: 10, selected: id, .. } => selected = Some(*id),
                ViewNode::Text { text, .. } => lines.push(text.as_str()),
                _ => {}
            }
        }
        assert_eq!(selected, Some(selected_tab));
        lines.join("\n")
    }
}

fn raw(value: Input, device_id: u32) -> Event {
    let source = input::Source {
        timestamp_ns: 1_000_000 + u64::from(device_id),
        client_id: CLIENT,
        device_id,
        modifiers: input::SHIFT,
    };
    Event {
        object_id: WINDOW,
        kind: value.kind(),
        target: u32::from(matches!(value, Input::Pointer(_))),
        data: value.encode(source),
    }
}

fn keyboard(character: char) -> Input {
    let mut text = ['\0'; 8];
    text[0] = character;
    Input::Keyboard(Keyboard {
        key: Key::A,
        device_key: Key::Q,
        pressed: true,
        repeat: false,
        text,
        text_len: 1,
    })
}

#[test]
fn a_family_selected_from_the_narrow_menu_stays_selected_after_resize() {
    let mut lab = Lab::open();
    let resize = |width| Event {
        object_id: WINDOW,
        kind: EventKind::SurfaceChanged,
        target: 0,
        data: Surface { width, height: 800, pixel_scale: 1., inputs: Surface::POINTER }
            .encode()
            .unwrap()
            .to_vec(),
    };
    lab.deliver([resize(193)]);
    lab.click(15);
    let ViewNode::Theme { child, .. } = &lab.view else { panic!("expected a window") };
    let ViewNode::Column(nodes) = child.as_ref() else { panic!("expected a column") };
    assert!(matches!(nodes.first(), Some(ViewNode::PopupButton { label, items, .. })
        if label == "Gamepad" && items.len() == 5));
    lab.deliver([resize(800)]);
    assert!(lab.readout(15).contains("Waiting for gamepad input"));
}

#[test]
fn hidden_tabs_retain_their_input_and_report_its_window_and_device() {
    // Deliver all five families in one turn while Pointer is selected, then inspect each tab.
    let mut lab = Lab::open();
    assert_eq!(lab.mask(), input::ALL);
    let samples = [
        Input::Pointer(Pointer {
            id: 29,
            phase: input::POINTER_DOWN,
            kind: input::PEN,
            button: 0,
            click_count: 1,
            flags: input::PRIMARY | input::IN_CONTACT,
            buttons: 1,
            x: 227.,
            y: 159.,
            dx: 0.,
            dy: 0.,
            canvas_x: 43.,
            canvas_y: 55.,
            pen: Some(Pen { pressure: 0.75, ..Default::default() }),
        }),
        keyboard('λ'),
        Input::Scroll(Scroll {
            kind: input::TRACKPAD,
            phase: input::CHANGED,
            flags: input::SCROLL_PRECISE,
            x: 12.,
            y: 20.,
            dx: -37.,
            dy: 61.,
            wheel_z: 0.,
        }),
        Input::Gesture(Gesture {
            kind: input::PINCH,
            phase: input::CHANGED,
            x: 80.,
            y: 70.,
            dx: 0.,
            dy: 0.,
            scale: 1.75,
            rotation: 0.,
        }),
        Input::Gamepad(Gamepad {
            phase: input::GAMEPAD_AXIS_MOVE,
            control: input::GamepadAxis::RightY as u16,
            value: -0.5,
            x: 0.25,
            y: -0.5,
        }),
    ];
    lab.deliver(samples.into_iter().zip(51..).map(|(sample, device)| raw(sample, device)));
    for (tab, device, target, detail) in [
        (11, 51, 1, "Pressure 0.750"),
        (12, 52, 0, "Logical A · Physical Q"),
        (13, 53, 0, "(-37.00, 61.00, 0.00)"),
        (14, 54, 0, "1.750×"),
        (15, 55, 0, "Pair (0.250, -0.500)"),
    ] {
        if tab != 11 {
            lab.click(tab);
        }
        let readout = lab.readout(tab);
        let source =
            format!("Window {WINDOW} · Target {target} · Client {CLIENT} · Device {device}");
        assert!(readout.contains(&source), "{readout}");
        assert!(readout.contains(detail), "{readout}");
    }
}

#[test]
fn gesture_phases_and_gamepad_lifecycle_reach_the_inspector() {
    let mut lab = Lab::open();
    lab.click(14);
    let mut gesture = Gesture {
        kind: input::PAN,
        phase: input::BEGAN,
        x: 80.,
        y: 90.,
        dx: 5.,
        dy: -3.,
        scale: 1.,
        rotation: 0.,
    };
    for (kind, label) in [(input::PAN, "Pan"), (input::PINCH, "Pinch"), (input::ROTATE, "Rotate")] {
        gesture.kind = kind;
        gesture.scale = if kind == input::PINCH { 1.5 } else { 1. };
        gesture.rotation = if kind == input::ROTATE { -0.25 } else { 0. };
        for (phase, transition) in
            [(input::BEGAN, "began"), (input::CHANGED, "changed"), (input::ENDED, "ended")]
        {
            gesture.phase = phase;
            lab.deliver([raw(Input::Gesture(gesture), 7)]);
            assert!(lab.readout(14).contains(&format!("{label} {transition}")));
        }
    }
    lab.click(15);
    for (phase, control, value, label) in [
        (input::GAMEPAD_CONNECTED, 0, 0., "Gamepad connected"),
        (input::GAMEPAD_BUTTON_DOWN, input::GamepadButton::Options as u16, 1., "Options down"),
        (input::GAMEPAD_BUTTON_UP, input::GamepadButton::Options as u16, 0., "Options up"),
        (input::GAMEPAD_DISCONNECTED, 0, 0., "Gamepad disconnected"),
    ] {
        lab.deliver([raw(Input::Gamepad(Gamepad { phase, control, value, x: 0., y: 0. }), 8)]);
        assert!(lab.readout(15).contains(label));
    }
}

#[test]
fn freeze_retains_the_readout_while_capture_and_family_controls_remain_live() {
    // Compare distinct text payloads to distinguish a frozen presentation from stopped capture.
    let mut lab = Lab::open();
    lab.click(12);
    lab.deliver([raw(keyboard('ß'), 52), raw(keyboard('λ'), 52)]);
    assert!(lab.readout(12).contains('λ'));
    lab.click(21);
    lab.deliver([raw(keyboard('界'), 52)]);
    assert_eq!(lab.mask(), input::ALL);
    let frozen = lab.readout(12);
    assert!(frozen.contains('λ') && !frozen.contains('界'), "{frozen}");
    lab.click(22);
    assert!(lab.readout(12).contains('ß'));
    lab.click(23);
    assert!(lab.readout(12).contains('λ'));
    lab.click(21);
    assert!(lab.readout(12).contains('界'));
    lab.click(20);
    assert_eq!(lab.mask(), input::ALL & !input::KEYBOARD);
    assert_eq!(validate_input(&lab.view, &raw(keyboard('x'), 52)), Err(HostError::Unsupported));
    let Input::Keyboard(mut key) = keyboard('界') else { unreachable!() };
    key.pressed = false;
    key.text = ['\0'; 8];
    key.text_len = 0;
    lab.deliver([raw(Input::Keyboard(key), 52)]);
    assert!(lab.readout(12).contains("A up"));
    lab.click(20);
    assert_eq!(lab.mask(), input::ALL);
    lab.click(11);
    lab.click(12);
    assert!(lab.readout(12).contains("A up"));
}
