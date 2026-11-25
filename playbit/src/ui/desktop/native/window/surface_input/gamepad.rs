//! Foreground window ownership of Apple GameController input.
use super::*;
use playbit_api::abi::input::Gamepad;

#[link(name = "GameController", kind = "framework")]
unsafe extern "C" {
    static GCControllerDidConnectNotification: *mut Object;
    static GCControllerDidDisconnectNotification: *mut Object;
}

pub(super) struct Controller {
    profile: *mut Object,
    id: u32,
    buttons: [Option<PendingInput>; 13],
    axes: [f32; 8],
    disconnect: PendingInput,
}

pub(super) struct GamepadObservers {
    center: *mut Object,
    tokens: [*mut Object; 2],
}

impl GamepadObservers {
    pub(super) fn attach(state: Rc<RefCell<State>>) -> Self {
        unsafe {
            let center: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
            let queue: *mut Object = msg_send![class!(NSOperationQueue), mainQueue];
            let nil = std::ptr::null_mut::<Object>();
            let mut tokens = [nil; 2];
            for (index, name) in
                [GCControllerDidConnectNotification, GCControllerDidDisconnectNotification]
                    .into_iter()
                    .enumerate()
            {
                let weak = Rc::downgrade(&state);
                let callback = ConcreteBlock::new(move |note: *mut Object| {
                    if let Some(state) = weak.upgrade() {
                        let device: *mut Object = msg_send![note, object];
                        if index == 0 {
                            connect_controller(&state, device);
                        } else if let Ok(mut state) = state.try_borrow_mut() {
                            state.disconnect(device as usize);
                        }
                    }
                })
                .copy();
                tokens[index] = msg_send![center, addObserverForName: name object: nil queue: queue usingBlock: callback];
            }
            let devices: *mut Object = msg_send![class!(GCController), controllers];
            let count: usize = msg_send![devices, count];
            for index in 0..count.min(8) {
                let device: *mut Object = msg_send![devices, objectAtIndex: index];
                connect_controller(&state, device);
            }
            Self { center, tokens }
        }
    }
}
impl Drop for GamepadObservers {
    fn drop(&mut self) {
        for token in self.tokens {
            unsafe {
                let _: () = msg_send![self.center, removeObserver: token];
            }
        }
    }
}
impl Drop for SurfaceInput {
    fn drop(&mut self) {
        self.finish();
    }
}

unsafe fn connect_controller(state: &Rc<RefCell<State>>, device: *mut Object) {
    let weak = Rc::downgrade(state);
    let Ok(mut state) = state.try_borrow_mut() else { return };
    let key = device as usize;
    if device.is_null()
        || state.devices.len() >= 8
        || state.devices.contains_key(&key)
        || state.mask & abi::GAMEPAD == 0
    {
        return;
    }
    unsafe {
        let profile: *mut Object = msg_send![device, extendedGamepad];
        if profile.is_null() {
            return;
        }
        let Some(disconnect) = state.handle.as_ref().and_then(Handle::reserve_release) else {
            return;
        };
        let id = state.next_device_id.max(16);
        let Some(next) = id.checked_add(1) else { return };
        if !state.controller(id, abi::GAMEPAD_CONNECTED) {
            return;
        }
        state.next_device_id = next;
        let profile: *mut Object = msg_send![profile, retain];
        state.devices.insert(
            key,
            Controller { profile, id, buttons: [const { None }; 13], axes: [0.; 8], disconnect },
        );
        let handler = ConcreteBlock::new(move |profile: *mut Object, _: *mut Object| {
            if let Some(state) = weak.upgrade()
                && let Ok(mut state) = state.try_borrow_mut()
            {
                state.sample(profile, key);
            }
        })
        .copy();
        let _: () = msg_send![profile, setValueChangedHandler: handler];
        state.sample(profile, key);
    }
}

impl State {
    fn controller(&mut self, id: u32, phase: u8) -> bool {
        self.send(controller_source(id), 0, controller_event(phase), false)
    }

    fn disconnect(&mut self, key: usize) {
        let Some(device) = self.devices.remove(&key) else { return };
        unsafe {
            let _: () =
                msg_send![device.profile, setValueChangedHandler: std::ptr::null_mut::<Object>()];
            let _: () = msg_send![device.profile, release];
        }
        self.release(
            controller_source(device.id),
            0,
            controller_event(abi::GAMEPAD_DISCONNECTED),
            device.disconnect,
        );
    }

    pub(super) fn disconnect_devices(&mut self) {
        while let Some((&key, _)) = self.devices.first_key_value() {
            self.disconnect(key);
        }
    }

    fn sample(&mut self, profile: *mut Object, key: usize) {
        let Some(mut device) = self.devices.remove(&key) else { return };
        let source = controller_source(device.id);
        unsafe {
            let buttons: [*mut Object; 13] = [
                msg_send![profile, buttonA],
                msg_send![profile, buttonB],
                msg_send![profile, buttonX],
                msg_send![profile, buttonY],
                msg_send![profile, leftShoulder],
                msg_send![profile, rightShoulder],
                msg_send![profile, leftTrigger],
                msg_send![profile, rightTrigger],
                msg_send![profile, leftThumbstickButton],
                msg_send![profile, rightThumbstickButton],
                msg_send![profile, buttonMenu],
                msg_send![profile, buttonOptions],
                msg_send![profile, buttonHome],
            ];
            for (index, button) in buttons.into_iter().enumerate() {
                if button.is_null() {
                    continue;
                }
                let pressed: BOOL = msg_send![button, isPressed];
                let terminal = &mut device.buttons[index];
                if pressed == terminal.is_some() {
                    continue;
                }
                let value: f32 = msg_send![button, value];
                let phase = if pressed { abi::GAMEPAD_BUTTON_DOWN } else { abi::GAMEPAD_BUTTON_UP };
                let input =
                    Input::Gamepad(Gamepad { phase, control: index as u16, value, x: 0., y: 0. });
                if let Some(release) = terminal.take() {
                    self.release(source, 0, input, release);
                } else if let Some(release) = self.handle.as_ref().and_then(Handle::reserve_release)
                    && self.send(source, 0, input, false)
                {
                    *terminal = Some(release);
                }
            }
            let left: *mut Object = msg_send![profile, leftThumbstick];
            let right: *mut Object = msg_send![profile, rightThumbstick];
            let dpad: *mut Object = msg_send![profile, dpad];
            let controls: [*mut Object; 8] = [
                msg_send![left, xAxis],
                msg_send![left, yAxis],
                msg_send![right, xAxis],
                msg_send![right, yAxis],
                msg_send![dpad, xAxis],
                msg_send![dpad, yAxis],
                buttons[6],
                buttons[7],
            ];
            let axes = controls.map(|control| {
                let value: f32 = msg_send![control, value];
                value
            });
            for (index, value) in axes.into_iter().enumerate() {
                if value == device.axes[index] {
                    continue;
                }
                let (x, y) = if index < 6 { (axes[index & !1], axes[index | 1]) } else { (0., 0.) };
                let input = Input::Gamepad(Gamepad {
                    phase: abi::GAMEPAD_AXIS_MOVE,
                    control: index as u16,
                    value,
                    x,
                    y,
                });
                if self.send(source, 0, input, true) {
                    device.axes[index] = value;
                }
            }
        }
        self.devices.insert(key, device);
    }
}

fn controller_source(device_id: u32) -> Source {
    Source { device_id, timestamp_ns: playbit_trace::clock_ns(), ..Source::default() }
}

fn controller_event(phase: u8) -> Input {
    Input::Gamepad(Gamepad { phase, control: 0, value: 0., x: 0., y: 0. })
}
