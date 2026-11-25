//! AppKit input before GPUI maps it to semantic controls.
#![allow(unsafe_code, reason = "AppKit passes NSEvent and NSView objects on the main thread.")]

use super::super::worker::PendingInput;
use super::Handle;
use crate::ui::desktop::scene::{CanvasBounds, Input as SceneInput};
use block::ConcreteBlock;
use cocoa::foundation::{NSPoint, NSRect};
use gpui::{Bounds, Pixels, point, px};
use objc::{
    class, msg_send,
    runtime::{BOOL, Object, YES},
    sel, sel_impl,
};
use playbit_api::abi::input::{
    self as abi, Gesture, Input, Key, Keyboard, Pen, Pointer, Scroll, Source,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

mod gamepad;
mod keys;

use gamepad::{Controller, GamepadObservers};
use keys::{logical, physical};

type Motion = (Source, u32, Input);

#[derive(Default)]
pub(super) struct SurfaceInput {
    state: Rc<RefCell<State>>,
    observer: Option<Observer>,
    gamepads: Option<GamepadObservers>,
}

#[derive(Default)]
struct State {
    handle: Option<Handle>,
    mask: u32,
    legacy_root_focus: Option<bool>,
    viewport: Option<(Bounds<Pixels>, f32, Pixels)>,
    canvases: CanvasBounds,
    mouse: PointerStream,
    pen: PointerStream,
    pen_eraser: bool,
    held: BTreeMap<u16, (Keyboard, PendingInput)>,
    devices: BTreeMap<usize, Controller>,
    next_device_id: u32,
    motion: BTreeMap<(u32, u32, u32), Motion>,
}

#[derive(Default)]
struct PointerStream {
    last: Option<(Source, u32, Pointer)>,
    release: Option<PendingInput>,
}

impl SurfaceInput {
    pub fn attach(&mut self, window: &gpui::Window) {
        self.finish();
        self.observer = Observer::attach(window, self.state.clone());
    }

    pub fn detach(&mut self) {
        self.finish();
        self.observer = None;
        #[cfg(feature = "process")]
        if let Some(handle) = &self.state.borrow().handle {
            handle.update_tool_canvas_offsets([]);
        }
    }

    pub fn configure(
        &mut self,
        handle: Option<Handle>,
        mask: u32,
        canvases: CanvasBounds,
        active: bool,
        legacy_root_focus: Option<bool>,
    ) {
        let generation = self.state.borrow().handle.as_ref().map(|h| h.generation);
        if generation != handle.as_ref().map(|h| h.generation) {
            self.finish();
        }
        {
            let mut state = self.state.borrow_mut();
            let removed = state.mask & !mask;
            if removed != 0 {
                state.finish(removed);
            }
            state.handle = handle;
            state.mask = mask;
            state.legacy_root_focus = legacy_root_focus;
            state.canvases = canvases;
            state.flush_motion();
        }
        self.set_active(active);
    }

    pub fn set_active(&mut self, active: bool) {
        if active && self.state.borrow().mask & abi::GAMEPAD != 0 {
            if self.gamepads.is_none() {
                self.gamepads = Some(GamepadObservers::attach(self.state.clone()));
            }
        } else if self.gamepads.take().is_some() {
            self.state.borrow_mut().disconnect_devices();
        }
    }

    pub fn element(&self, chrome_top: Pixels) -> impl gpui::IntoElement {
        use gpui::prelude::*;
        let state = self.state.clone();
        gpui::canvas(
            move |bounds, window, _| {
                let mut state = state.borrow_mut();
                let zoom = f32::from(window.rem_size()) / 16.;
                state.viewport = Some((bounds, zoom, chrome_top));
                #[cfg(feature = "process")]
                if let Some(handle) = &state.handle {
                    handle.update_tool_canvas_offsets(state.canvases.borrow().iter().map(
                        |(&id, (canvas, _))| {
                            let offset = (canvas.origin - bounds.origin) / zoom;
                            (id, [f32::from(offset.x), f32::from(offset.y)])
                        },
                    ));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    pub fn begin_modal_input(&mut self) {
        // The menu's AppKit monitor takes mouse and key releases while the document stays key.
        self.state.borrow_mut().finish(abi::POINTER | abi::KEYBOARD | abi::SCROLL | abi::GESTURE);
    }

    pub fn finish(&mut self) {
        self.set_active(false);
        self.state.borrow_mut().finish(abi::ALL);
    }
}

struct Observer {
    token: *mut Object,
    view: *mut Object,
    window: *mut Object,
}
impl Drop for Observer {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![class!(NSEvent), removeMonitor: self.token];
            let _: () = msg_send![self.view, release];
            let _: () = msg_send![self.window, release];
        }
    }
}

impl Observer {
    fn attach(window: &gpui::Window, state: Rc<RefCell<State>>) -> Option<Self> {
        let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
        else {
            return None;
        };
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<Object>();
            let native: *mut Object = msg_send![view, window];
            if native.is_null() {
                return None;
            }
            let block = ConcreteBlock::new(move |event: *mut Object| {
                let Ok(mut state) = state.try_borrow_mut() else { return event };
                if state.mask != 0 && !super::popup::is_modal_active() {
                    let from: *mut Object = msg_send![event, window];
                    if from == native {
                        state.event(event, view);
                    }
                }
                event
            })
            .copy();
            let observer: *mut Object = msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: usize::MAX handler: block];
            if observer.is_null() {
                return None;
            }
            // The monitor can outlive the GPUI handle until close is adopted.
            // Keep both objects alive until AppKit has removed the callback.
            let view = msg_send![view, retain];
            let window = msg_send![native, retain];
            Some(Self { token: observer, view, window })
        }
    }
}

impl State {
    fn send(&mut self, source: Source, target: u32, mut input: Input, motion: bool) -> bool {
        if !motion {
            self.discard_motion_before_edge(source, input);
        }
        let Some(handle) = &self.handle else { return false };
        if motion && handle.input_pending() {
            const MAX_MOTION_STREAMS: usize = 16;
            let stream = match input {
                Input::Pointer(pointer) => pointer.id,
                Input::Keyboard(key) => u32::from(key.device_key as u16),
                Input::Scroll(scroll) => u32::from(scroll.kind),
                Input::Gesture(gesture) => u32::from(gesture.kind),
                Input::Gamepad(gamepad) => u32::from(gamepad.control),
            };
            let key = (input.kind() as u32, source.device_id, stream);
            if let Some((_, previous_target, previous)) = self.motion.get(&key).copied() {
                if previous_target == target {
                    merge_motion(previous, &mut input);
                }
            } else if self.motion.len() == MAX_MOTION_STREAMS {
                return false;
            }
            self.motion.insert(key, (source, target, input));
            return true;
        }
        handle.send(
            SceneInput { kind: input.kind(), target, data: input.encode(source), revision: None },
            None,
        )
    }

    fn discard_motion_before_edge(&mut self, source: Source, input: Input) {
        let disconnecting =
            matches!(input, Input::Gamepad(gamepad) if gamepad.phase == abi::GAMEPAD_DISCONNECTED);
        self.motion.retain(|_, (previous, _, item)| {
            matches!(item, Input::Gamepad(_))
                && !(disconnecting && source.device_id == previous.device_id)
        });
    }

    fn flush_motion(&mut self) {
        if self.handle.as_ref().is_some_and(|handle| !handle.input_pending())
            && let Some((_, (source, target, input))) = self.motion.pop_first()
        {
            self.send(source, target, input, true);
        }
    }

    fn finish(&mut self, families: u32) {
        self.motion.retain(|_, (_, _, input)| Input::family(input.kind()) & families == 0);
        let source =
            Source { timestamp_ns: playbit_trace::clock_ns(), device_id: 1, ..Source::default() };
        if families & abi::POINTER != 0 {
            let mouse = std::mem::take(&mut self.mouse);
            let pen = std::mem::take(&mut self.pen);
            self.finish_pointer(mouse, source.timestamp_ns);
            self.finish_pointer(pen, source.timestamp_ns);
            self.pen_eraser = false;
        }
        if families & abi::KEYBOARD == 0 {
            return;
        }
        for (_, (key, release)) in std::mem::take(&mut self.held) {
            self.release(
                Source { device_id: 2, ..source },
                0,
                Input::Keyboard(Keyboard {
                    pressed: false,
                    repeat: false,
                    text: ['\0'; 8],
                    text_len: 0,
                    ..key
                }),
                release,
            );
        }
    }

    fn finish_pointer(&mut self, stream: PointerStream, timestamp_ns: u64) {
        let Some((mut source, target, mut pointer)) = stream.last else { return };
        source.timestamp_ns = timestamp_ns;
        pointer.phase =
            if stream.release.is_some() { abi::POINTER_CANCEL } else { abi::POINTER_LEAVE };
        pointer.buttons = 0;
        pointer.button = abi::NO_BUTTON;
        pointer.flags &= !abi::IN_CONTACT;
        pointer.dx = 0.;
        pointer.dy = 0.;
        if let Some(release) = stream.release {
            self.release(source, target, Input::Pointer(pointer), release);
        } else {
            self.send(source, target, Input::Pointer(pointer), false);
        }
    }

    fn release(
        &mut self,
        source: Source,
        target: u32,
        input: Input,
        release: PendingInput,
    ) -> bool {
        self.discard_motion_before_edge(source, input);
        self.handle.as_ref().is_some_and(|handle| {
            handle.send_release(
                SceneInput {
                    kind: input.kind(),
                    target,
                    data: input.encode(source),
                    revision: None,
                },
                release,
            )
        })
    }

    fn event(&mut self, event: *mut Object, view: *mut Object) {
        unsafe {
            let kind: usize = msg_send![event, type];
            let flags: usize = msg_send![event, modifierFlags];
            let time: f64 = msg_send![event, timestamp];
            let modifiers = ((flags >> 17) & 1) as u16
                | (((flags >> 18) & 1) as u16 * abi::CTRL)
                | (((flags >> 19) & 1) as u16 * abi::ALT)
                | (((flags >> 20) & 1) as u16 * abi::META)
                | (((flags >> 16) & 1) as u16 * abi::CAPS_LOCK)
                | (((flags >> 23) & 1) as u16 * abi::FN);
            let source =
                Source { timestamp_ns: (time * 1e9) as u64, modifiers, device_id: 1, client_id: 0 };
            if matches!(kind, 10..=12) {
                let legacy_blocks_down = kind != 11
                    && self.legacy_root_focus.is_some_and(|focused| {
                        !focused || modifiers & (abi::CTRL | abi::ALT | abi::META | abi::FN) != 0
                    });
                if self.mask & abi::KEYBOARD != 0 && !legacy_blocks_down {
                    self.keyboard(event, kind, Source { device_id: 2, ..source });
                }
                return;
            }
            if !matches!(kind, 1..=9 | 18 | 22..=27 | 29..=31) {
                return;
            }
            let Some((bounds, zoom, chrome_top)) = self.viewport else { return };
            let p: NSPoint = msg_send![event, locationInWindow];
            let local: NSPoint =
                msg_send![view, convertPoint: p fromView: std::ptr::null_mut::<Object>()];
            let rect: NSRect = msg_send![view, bounds];
            let flipped: BOOL = msg_send![view, isFlipped];
            let p = point(
                px(local.x as f32),
                px(if flipped == YES { local.y } else { rect.size.height - local.y } as f32),
            );
            let x = f32::from(p.x - bounds.origin.x) / zoom;
            let y = f32::from(p.y - bounds.origin.y) / zoom;
            let inside = bounds.contains(&p) && p.y >= bounds.top() + chrome_top;
            if kind == 22 && inside && self.mask & abi::SCROLL != 0 {
                let precise: BOOL = msg_send![event, hasPreciseScrollingDeltas];
                let inverted: BOOL = msg_send![event, isDirectionInvertedFromDevice];
                let phase: usize = msg_send![event, phase];
                let momentum: usize = msg_send![event, momentumPhase];
                let dx: f64 = msg_send![event, scrollingDeltaX];
                let dy: f64 = msg_send![event, scrollingDeltaY];
                let dz: f64 = msg_send![event, deltaZ];
                let phase = if momentum != 0 { abi::MOMENTUM } else { gesture_phase(phase) };
                let scale = if precise == YES { zoom } else { 1. };
                self.send(
                    source,
                    0,
                    Input::Scroll(Scroll {
                        kind: if precise == YES { abi::TRACKPAD } else { abi::MOUSE },
                        phase,
                        flags: if precise == YES { abi::SCROLL_PRECISE } else { abi::SCROLL_LINES }
                            | if inverted == YES { abi::SCROLL_INVERTED } else { 0 },
                        x,
                        y,
                        dx: dx as f32 / scale,
                        dy: -dy as f32 / scale,
                        wheel_z: dz as f32,
                    }),
                    phase == abi::CHANGED,
                );
            } else if matches!(kind, 18 | 30 | 31) && inside && self.mask & abi::GESTURE != 0 {
                let phase: usize = msg_send![event, phase];
                let phase = gesture_phase(phase);
                let mut gesture = Gesture {
                    kind: abi::PAN,
                    phase,
                    x,
                    y,
                    dx: 0.,
                    dy: 0.,
                    scale: 1.,
                    rotation: 0.,
                };
                match kind {
                    18 => {
                        let rotation: f32 = msg_send![event, rotation];
                        gesture.kind = abi::ROTATE;
                        gesture.rotation = -rotation.to_radians();
                    }
                    30 => {
                        let magnification: f64 = msg_send![event, magnification];
                        gesture.kind = abi::PINCH;
                        gesture.scale = (1. + magnification as f32).max(f32::MIN_POSITIVE);
                    }
                    _ => {
                        let dx: f64 = msg_send![event, deltaX];
                        let dy: f64 = msg_send![event, deltaY];
                        gesture.dx = dx as f32 / zoom;
                        gesture.dy = -dy as f32 / zoom;
                    }
                }
                self.send(source, 0, Input::Gesture(gesture), phase == abi::CHANGED);
            } else if self.mask & abi::POINTER != 0 && matches!(kind, 1..=9 | 23..=27) {
                self.pointer(event, kind, source, p, (x, y), (inside, zoom));
            }
        }
    }

    fn keyboard(&mut self, event: *mut Object, kind: usize, source: Source) {
        unsafe {
            let code: u16 = msg_send![event, keyCode];
            let device_key = physical(code);
            if device_key == Key::None && kind == 12 {
                return;
            }
            let pressed = kind == 10 || (kind == 12 && !self.held.contains_key(&code));
            if !pressed && !self.held.contains_key(&code) {
                return;
            }
            let repeat: BOOL = if kind == 10 { msg_send![event, isARepeat] } else { false };
            if repeat == YES && !self.held.contains_key(&code) {
                return;
            }
            let mut key = device_key;
            let mut text = ['\0'; 8];
            let mut text_len = 0;
            if kind == 10 {
                let base: *mut Object = msg_send![event, charactersIgnoringModifiers];
                if let Some(c) = native_string(base).and_then(|s| s.chars().next()) {
                    key = logical(c, device_key);
                }
                let chars: *mut Object = msg_send![event, characters];
                if let Some(chars) = native_string(chars) {
                    for c in chars.chars().take(8) {
                        if c.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&c) {
                            continue;
                        }
                        text[text_len] = c;
                        text_len += 1;
                    }
                }
            }
            if let Some((old, _)) = self.held.get(&code) {
                key = old.key;
            }
            let keyboard = Keyboard {
                key,
                device_key,
                pressed,
                repeat: repeat == YES,
                text,
                text_len: text_len as u8,
            };
            let input = Input::Keyboard(keyboard);
            if !pressed {
                if let Some((_, release)) = self.held.remove(&code) {
                    self.release(source, 0, input, release);
                }
            } else if repeat == YES || self.held.contains_key(&code) {
                self.send(source, 0, input, true);
            } else if let Some(release) = self.handle.as_ref().and_then(Handle::reserve_release)
                && self.send(source, 0, input, false)
            {
                self.held.insert(code, (keyboard, release));
            }
        }
    }

    fn canvas_position(
        &self,
        point: gpui::Point<Pixels>,
        (x, y): (f32, f32),
        zoom: f32,
        capture: Option<(u32, Pointer)>,
    ) -> (u32, f32, f32) {
        let canvases = self.canvases.borrow();
        let hit = if let Some((target, last)) = capture {
            if target == 0 {
                return (0, 0., 0.);
            }
            let Some(bounds) = canvases.get(&target) else {
                // The terminal edge still belongs to the canvas that accepted the press.
                return (target, last.canvas_x + (x - last.x), last.canvas_y + (y - last.y));
            };
            Some((target, bounds))
        } else {
            canvases.iter().find(|(_, (_, clip))| clip.contains(&point)).map(|(&id, b)| (id, b))
        };
        hit.map_or((0, 0., 0.), |(id, (bounds, _))| {
            (
                id,
                f32::from(point.x - bounds.origin.x) / zoom,
                f32::from(point.y - bounds.origin.y) / zoom,
            )
        })
    }

    fn pointer(
        &mut self,
        event: *mut Object,
        kind: usize,
        source: Source,
        p: gpui::Point<Pixels>,
        (x, y): (f32, f32),
        (inside, zoom): (bool, f32),
    ) {
        unsafe {
            let subtype: i16 =
                if matches!(kind, 1..=7 | 25..=27) { msg_send![event, subtype] } else { 0 };
            if kind == 24 || subtype == 2 {
                let entering: BOOL = msg_send![event, isEnteringProximity];
                let device: usize = msg_send![event, pointingDeviceType];
                self.pen_eraser = entering == YES && device == 3;
                if entering != YES {
                    let pen = std::mem::take(&mut self.pen);
                    self.finish_pointer(pen, source.timestamp_ns);
                }
                return;
            }
            let is_pen = kind == 23 || subtype == 1;
            let old = if is_pen { self.pen.last } else { self.mouse.last };
            let held =
                if is_pen { self.pen.release.is_some() } else { self.mouse.release.is_some() };
            let button: isize = match kind {
                1 | 2 | 6 | 23 => 0,
                3 | 4 | 7 => 1,
                25..=27 => msg_send![event, buttonNumber],
                _ => -1,
            };
            let (mut source, buttons, pen) = if is_pen {
                let pressure: f32 = msg_send![event, pressure];
                let tangential_pressure: f32 = msg_send![event, tangentialPressure];
                let tilt: NSPoint = msg_send![event, tilt];
                let twist: f32 = msg_send![event, rotation];
                let device: usize = msg_send![event, deviceID];
                let buttons: usize = msg_send![event, buttonMask];
                let tilt_x = (tilt.x as f32).clamp(-1., 1.);
                let tilt_y = (-tilt.y as f32).clamp(-1., 1.);
                let pen = Pen {
                    pressure: pressure.clamp(0., 1.),
                    tangential_pressure: tangential_pressure.clamp(-1., 1.),
                    tilt_x,
                    tilt_y,
                    twist: twist.rem_euclid(360.),
                    altitude: (1. - tilt_x.hypot(tilt_y).min(1.)) * core::f32::consts::FRAC_PI_2,
                    azimuth: tilt_y.atan2(tilt_x).rem_euclid(core::f32::consts::TAU),
                    ..Pen::default()
                };
                (
                    Source { device_id: 0x8000_0000 | (device as u32 & 0x7fff_ffff), ..source },
                    buttons as u16 | u16::from(pen.pressure > 0.),
                    Some(pen),
                )
            } else {
                // AppKit's global button mask can already reflect a later queued event.
                let buttons = old.filter(|_| held).map_or(0, |(_, _, pointer)| pointer.buttons);
                (source, mouse_buttons(buttons, kind, button), None)
            };
            if let Some((previous, _, _)) = old
                && held
            {
                source.device_id = previous.device_id;
            }
            let phase = match kind {
                23 if inside && !held && buttons != 0 => abi::POINTER_DOWN,
                23 if held && buttons == 0 => abi::POINTER_UP,
                1 | 3 | 25 if inside && (!is_pen || !held) => abi::POINTER_DOWN,
                2 | 4 | 26 if held => abi::POINTER_UP,
                2 | 4 | 26 => return,
                9 if old.is_some() && !held => abi::POINTER_LEAVE,
                9 => return,
                _ if !inside && old.is_some() && !held => abi::POINTER_LEAVE,
                _ if !inside && old.is_none() => return,
                _ if old.is_none() => abi::POINTER_ENTER,
                _ => abi::POINTER_MOVE,
            };
            let capture = old.filter(|_| held).map(|(_, target, pointer)| (target, pointer));
            let (target, canvas_x, canvas_y) = self.canvas_position(p, (x, y), zoom, capture);
            let (button, click_count) = if matches!(phase, abi::POINTER_DOWN | abi::POINTER_UP) {
                let count: isize = if kind == 23 { 0 } else { msg_send![event, clickCount] };
                (
                    if (0..16).contains(&button) { button as u8 } else { abi::NO_BUTTON },
                    count.clamp(0, 255) as u8,
                )
            } else {
                (abi::NO_BUTTON, 0)
            };
            let pointer = Pointer {
                id: if is_pen { 2 } else { 1 },
                phase,
                kind: if pen.is_some() { abi::PEN } else { abi::MOUSE },
                button,
                click_count,
                flags: abi::PRIMARY
                    | if self.pen_eraser && is_pen { abi::ERASER } else { 0 }
                    | if pen.is_some_and(|pen| pen.pressure > 0.) { abi::IN_CONTACT } else { 0 },
                buttons: if phase == abi::POINTER_DOWN || held { buttons } else { 0 },
                x,
                y,
                dx: old.map_or(0., |(_, _, last)| x - last.x),
                dy: old.map_or(0., |(_, _, last)| y - last.y),
                canvas_x,
                canvas_y,
                pen,
            };
            let input = Input::Pointer(pointer);
            let mut stream = std::mem::take(if is_pen { &mut self.pen } else { &mut self.mouse });
            let accepted = if phase == abi::POINTER_DOWN && stream.release.is_none() {
                if let Some(release) = self.handle.as_ref().and_then(Handle::reserve_release)
                    && self.send(source, target, input, false)
                {
                    stream.release = Some(release);
                    true
                } else {
                    false
                }
            } else if phase == abi::POINTER_UP
                && pointer.buttons == 0
                && let Some(release) = stream.release.take()
            {
                self.release(source, target, input, release)
            } else {
                self.send(source, target, input, phase == abi::POINTER_MOVE)
            };
            if accepted {
                stream.last = (phase != abi::POINTER_LEAVE).then_some((source, target, pointer));
            }
            *(if is_pen { &mut self.pen } else { &mut self.mouse }) = stream;
        }
    }
}

fn mouse_buttons(previous: u16, kind: usize, button: isize) -> u16 {
    let bit = if (0..16).contains(&button) { 1 << button } else { 0 };
    match kind {
        1 | 3 | 25 | 6 | 7 | 27 => previous | bit,
        2 | 4 | 26 => previous & !bit,
        _ => previous,
    }
}

fn gesture_phase(phase: usize) -> u8 {
    if phase & 1 != 0 {
        abi::BEGAN
    } else if phase & (8 | 16) != 0 {
        abi::ENDED
    } else {
        abi::CHANGED
    }
}

// AppKit retains both strings through the monitor callback.
unsafe fn native_string<'event>(value: *mut Object) -> Option<&'event str> {
    if value.is_null() {
        return None;
    }
    let text: *const std::ffi::c_char = unsafe { msg_send![value, UTF8String] };
    if text.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(text) }.to_str().ok()
}

fn merge_motion(previous: Input, current: &mut Input) {
    match (previous, current) {
        (Input::Pointer(a), Input::Pointer(b)) => {
            b.dx += a.dx;
            b.dy += a.dy;
            b.flags |= abi::COALESCED;
        }
        (Input::Scroll(a), Input::Scroll(b)) if a.flags == b.flags && a.phase == b.phase => {
            b.dx += a.dx;
            b.dy += a.dy;
            b.wheel_z += a.wheel_z;
        }
        (Input::Gesture(a), Input::Gesture(b)) if a.phase == b.phase => {
            b.dx += a.dx;
            b.dy += a.dy;
            b.scale = (a.scale * b.scale).clamp(f32::MIN_POSITIVE, f32::MAX);
            b.rotation += a.rotation;
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "The isolated input fixture asserts admission and decoding.")]
mod tests {
    use super::*;
    use playbit_api::abi::syscall::EventKind;
    use std::sync::atomic::Ordering;

    #[test]
    fn mouse_edges_keep_their_buttons_and_canvas_across_layout_and_menu_changes() {
        // Deliver queued NSEvents directly so the test never changes physical button state.
        let (handle, events) = Handle::fixture(8);
        handle.object_id.store(31, Ordering::Release);
        let canvases = CanvasBounds::default();
        let bounds = Bounds::new(point(px(10.), px(20.)), gpui::size(px(100.), px(100.)));
        canvases.borrow_mut().insert(7, (bounds, bounds));
        let mut surface = SurfaceInput::default();
        surface.configure(Some(handle), abi::POINTER | abi::SCROLL, canvases.clone(), false, None);
        let send = |surface: &mut SurfaceInput, kind, x, y| {
            let event = unsafe {
                msg_send![class!(NSEvent), mouseEventWithType: kind
                location: NSPoint::new(x as f64, y as f64) modifierFlags: 0usize timestamp: 1f64
                windowNumber: 0isize context: std::ptr::null_mut::<Object>() eventNumber: 1isize
                clickCount: 1isize pressure: 0f32]
            };
            surface.state.borrow_mut().pointer(
                event,
                kind,
                Source::default(),
                point(px(x), px(y)),
                (x, y),
                (true, 1.),
            );
        };
        let take = || {
            let item = events.try_recv().unwrap().input;
            let (_, Input::Pointer(p)) = Input::decode(item.kind, &item.data).unwrap() else {
                panic!("expected pointer")
            };
            (item.target, p.phase, p.buttons, p.canvas_x, p.canvas_y)
        };
        send(&mut surface, 1, 15., 27.);
        assert_eq!(take(), (7, abi::POINTER_DOWN, 1, 5., 7.));
        canvases.borrow_mut().clear();
        send(&mut surface, 6, 17., 30.);
        assert_eq!(take(), (7, abi::POINTER_MOVE, 1, 7., 10.));
        send(&mut surface, 2, 18., 31.);
        assert_eq!(take(), (7, abi::POINTER_UP, 0, 8., 11.));
        canvases.borrow_mut().insert(7, (bounds, bounds));
        send(&mut surface, 1, 15., 27.);
        assert_eq!(take(), (7, abi::POINTER_DOWN, 1, 5., 7.));
        send(&mut surface, 3, 15., 27.);
        assert_eq!(take(), (7, abi::POINTER_DOWN, 3, 5., 7.));
        send(&mut surface, 4, 15., 27.);
        assert_eq!(take(), (7, abi::POINTER_UP, 1, 5., 7.));
        surface.begin_modal_input();
        assert_eq!(take(), (7, abi::POINTER_CANCEL, 0, 5., 7.));
    }

    #[test]
    fn surface_input_finishes_held_key_under_queue_pressure() {
        let (handle, events) = Handle::fixture(7);
        handle.object_id.store(12, Ordering::Release);
        let mut state = State { handle: Some(handle.clone()), ..State::default() };
        let keyboard = Keyboard {
            key: Key::Q,
            device_key: Key::A,
            pressed: true,
            repeat: false,
            text: ['q', '\0', '\0', '\0', '\0', '\0', '\0', '\0'],
            text_len: 1,
        };
        let release = handle.reserve_release().unwrap();
        assert!(!handle.input_pending());
        assert!(state.send(Source::default(), 0, Input::Keyboard(keyboard), false));
        state.held.insert(0, (keyboard, release));

        let click =
            || SceneInput { kind: EventKind::Click, target: 7, data: Vec::new(), revision: None };
        let mut clicks = 0;
        while handle.send(click(), None) {
            clicks += 1;
            assert!(clicks < 8);
        }
        state.finish(abi::SCROLL);
        assert!(state.held.contains_key(&0), "an unrelated subscription preserves the held key");
        state.finish(abi::KEYBOARD);
        let items: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert_eq!(items.len(), clicks + 2);
        for (item, pressed) in [(items.first().unwrap(), true), (items.last().unwrap(), false)] {
            let (_, Input::Keyboard(key)) =
                Input::decode(item.input.kind, &item.input.data).unwrap()
            else {
                panic!("expected a key edge")
            };
            assert_eq!((key.key, key.device_key, key.pressed), (Key::Q, Key::A, pressed));
        }
    }
}
