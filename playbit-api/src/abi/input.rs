//! Window input payloads. Wire words use little-endian byte order.

use super::syscall::{Error, EventKind};

pub const FEATURE: &str = "ui.input.v1";
pub const POINTER: u32 = 1;
pub const KEYBOARD: u32 = 2;
pub const SCROLL: u32 = 4;
pub const GESTURE: u32 = 8;
pub const GAMEPAD: u32 = 16;
pub const ALL: u32 = POINTER | KEYBOARD | SCROLL | GESTURE | GAMEPAD;

/// Logical and physical key codes share this dense enumeration.
/// The printable characters produced by a key are carried separately.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u16)]
pub enum Key {
    #[default]
    None = 0,
    Space = 1,
    Quote = 2,
    Comma = 3,
    Minus = 4,
    Period = 5,
    Slash = 6,
    Digit0 = 7,
    Digit1 = 8,
    Digit2 = 9,
    Digit3 = 10,
    Digit4 = 11,
    Digit5 = 12,
    Digit6 = 13,
    Digit7 = 14,
    Digit8 = 15,
    Digit9 = 16,
    Semicolon = 17,
    Equal = 18,
    A = 19,
    B = 20,
    C = 21,
    D = 22,
    E = 23,
    F = 24,
    G = 25,
    H = 26,
    I = 27,
    J = 28,
    K = 29,
    L = 30,
    M = 31,
    N = 32,
    O = 33,
    P = 34,
    Q = 35,
    R = 36,
    S = 37,
    T = 38,
    U = 39,
    V = 40,
    W = 41,
    X = 42,
    Y = 43,
    Z = 44,
    LeftBracket = 45,
    Backslash = 46,
    RightBracket = 47,
    Grave = 48,
    Escape = 49,
    Enter = 50,
    Tab = 51,
    Backspace = 52,
    Insert = 53,
    Delete = 54,
    Left = 55,
    Right = 56,
    Down = 57,
    Up = 58,
    PageUp = 59,
    PageDown = 60,
    Home = 61,
    End = 62,
    CapsLock = 63,
    LeftShift = 64,
    LeftCtrl = 65,
    LeftAlt = 66,
    LeftSuper = 67,
    MediaNext = 68,
    MediaPrev = 69,
    MediaPlay = 70,
    MediaStop = 71,
    World1 = 72,
    World2 = 73,
    ScrollLock = 74,
    NumLock = 75,
    PrintScreen = 76,
    Pause = 77,
    F1 = 78,
    F2 = 79,
    F3 = 80,
    F4 = 81,
    F5 = 82,
    F6 = 83,
    F7 = 84,
    F8 = 85,
    F9 = 86,
    F10 = 87,
    F11 = 88,
    F12 = 89,
    F13 = 90,
    F14 = 91,
    F15 = 92,
    F16 = 93,
    F17 = 94,
    F18 = 95,
    F19 = 96,
    F20 = 97,
    F21 = 98,
    F22 = 99,
    F23 = 100,
    F24 = 101,
    Numpad0 = 102,
    Numpad1 = 103,
    Numpad2 = 104,
    Numpad3 = 105,
    Numpad4 = 106,
    Numpad5 = 107,
    Numpad6 = 108,
    Numpad7 = 109,
    Numpad8 = 110,
    Numpad9 = 111,
    NumpadDot = 112,
    NumpadDivide = 113,
    NumpadMultiply = 114,
    NumpadSubtract = 115,
    NumpadAdd = 116,
    NumpadEnter = 117,
    NumpadEquals = 118,
    NumpadClear = 119,
    RightShift = 120,
    RightCtrl = 121,
    RightAlt = 122,
    RightSuper = 123,
    Menu = 124,
    VolumeUp = 125,
    VolumeDown = 126,
    Mute = 127,
}

impl Key {
    pub const COUNT: usize = 128;

    pub fn from_code(code: u16) -> Option<Self> {
        KEYS.get(usize::from(code)).copied()
    }

    /// Names used by the C and OCaml SDKs and by local input tools.
    pub fn from_name(name: &str) -> Option<Self> {
        KEY_NAMES
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(name))
            .and_then(|index| KEYS.get(index).copied())
    }
}

const KEYS: [Key; Key::COUNT] = [
    Key::None,
    Key::Space,
    Key::Quote,
    Key::Comma,
    Key::Minus,
    Key::Period,
    Key::Slash,
    Key::Digit0,
    Key::Digit1,
    Key::Digit2,
    Key::Digit3,
    Key::Digit4,
    Key::Digit5,
    Key::Digit6,
    Key::Digit7,
    Key::Digit8,
    Key::Digit9,
    Key::Semicolon,
    Key::Equal,
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
    Key::LeftBracket,
    Key::Backslash,
    Key::RightBracket,
    Key::Grave,
    Key::Escape,
    Key::Enter,
    Key::Tab,
    Key::Backspace,
    Key::Insert,
    Key::Delete,
    Key::Left,
    Key::Right,
    Key::Down,
    Key::Up,
    Key::PageUp,
    Key::PageDown,
    Key::Home,
    Key::End,
    Key::CapsLock,
    Key::LeftShift,
    Key::LeftCtrl,
    Key::LeftAlt,
    Key::LeftSuper,
    Key::MediaNext,
    Key::MediaPrev,
    Key::MediaPlay,
    Key::MediaStop,
    Key::World1,
    Key::World2,
    Key::ScrollLock,
    Key::NumLock,
    Key::PrintScreen,
    Key::Pause,
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
    Key::F13,
    Key::F14,
    Key::F15,
    Key::F16,
    Key::F17,
    Key::F18,
    Key::F19,
    Key::F20,
    Key::F21,
    Key::F22,
    Key::F23,
    Key::F24,
    Key::Numpad0,
    Key::Numpad1,
    Key::Numpad2,
    Key::Numpad3,
    Key::Numpad4,
    Key::Numpad5,
    Key::Numpad6,
    Key::Numpad7,
    Key::Numpad8,
    Key::Numpad9,
    Key::NumpadDot,
    Key::NumpadDivide,
    Key::NumpadMultiply,
    Key::NumpadSubtract,
    Key::NumpadAdd,
    Key::NumpadEnter,
    Key::NumpadEquals,
    Key::NumpadClear,
    Key::RightShift,
    Key::RightCtrl,
    Key::RightAlt,
    Key::RightSuper,
    Key::Menu,
    Key::VolumeUp,
    Key::VolumeDown,
    Key::Mute,
];

pub const KEY_NAMES: [&str; Key::COUNT] = [
    "None",
    "Space",
    "Quote",
    "Comma",
    "Minus",
    "Period",
    "Slash",
    "0",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    "Semicolon",
    "Equal",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "LeftBracket",
    "Backslash",
    "RightBracket",
    "Grave",
    "Escape",
    "Enter",
    "Tab",
    "Backspace",
    "Insert",
    "Delete",
    "Left",
    "Right",
    "Down",
    "Up",
    "PageUp",
    "PageDown",
    "Home",
    "End",
    "CapsLock",
    "LeftShift",
    "LeftCtrl",
    "LeftAlt",
    "LeftSuper",
    "MediaNext",
    "MediaPrev",
    "MediaPlay",
    "MediaStop",
    "World1",
    "World2",
    "ScrollLock",
    "NumLock",
    "PrintScreen",
    "Pause",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "F11",
    "F12",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
    "Numpad0",
    "Numpad1",
    "Numpad2",
    "Numpad3",
    "Numpad4",
    "Numpad5",
    "Numpad6",
    "Numpad7",
    "Numpad8",
    "Numpad9",
    "NumpadDot",
    "NumpadDivide",
    "NumpadMultiply",
    "NumpadSubtract",
    "NumpadAdd",
    "NumpadEnter",
    "NumpadEquals",
    "NumpadClear",
    "RightShift",
    "RightCtrl",
    "RightAlt",
    "RightSuper",
    "Menu",
    "VolumeUp",
    "VolumeDown",
    "Mute",
];

pub const SHIFT: u16 = 1;
pub const CTRL: u16 = 2;
pub const ALT: u16 = 4;
pub const META: u16 = 8;
pub const CAPS_LOCK: u16 = 16;
pub const FN: u16 = 32;
pub const MODIFIERS: u16 = SHIFT | CTRL | ALT | META | CAPS_LOCK | FN;

pub const MOUSE: u8 = 1;
pub const TOUCH: u8 = 2;
pub const TRACKPAD: u8 = 3;
pub const PEN: u8 = 4;
pub const PRIMARY: u16 = 1;
pub const IN_CONTACT: u16 = 2;
pub const ERASER: u16 = 4;
pub const INVERTED: u16 = 8;
pub const COALESCED: u16 = 16;
pub const PREDICTED: u16 = 32;
pub const NO_BUTTON: u8 = 255;

pub const POINTER_MOVE: u8 = 0;
pub const POINTER_DOWN: u8 = 1;
pub const POINTER_UP: u8 = 2;
pub const POINTER_CANCEL: u8 = 3;
pub const POINTER_ENTER: u8 = 4;
pub const POINTER_LEAVE: u8 = 5;

pub const CHANGED: u8 = 0;
pub const BEGAN: u8 = 1;
pub const ENDED: u8 = 2;
pub const MOMENTUM: u8 = 3;
pub const SCROLL_PRECISE: u16 = 1;
pub const SCROLL_INVERTED: u16 = 2;
pub const SCROLL_LINES: u16 = 4;
pub const SCROLL_PAGES: u16 = 8;

pub const PAN: u8 = 0;
pub const PINCH: u8 = 1;
pub const ROTATE: u8 = 2;

pub const GAMEPAD_CONNECTED: u8 = 0;
pub const GAMEPAD_DISCONNECTED: u8 = 1;
pub const GAMEPAD_BUTTON_DOWN: u8 = 2;
pub const GAMEPAD_BUTTON_UP: u8 = 3;
pub const GAMEPAD_AXIS_MOVE: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum GamepadButton {
    A,
    B,
    X,
    Y,
    LeftShoulder,
    RightShoulder,
    LeftTrigger,
    RightTrigger,
    LeftThumbstick,
    RightThumbstick,
    Menu,
    Options,
    Home,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum GamepadAxis {
    LeftX,
    LeftY,
    RightX,
    RightY,
    DpadX,
    DpadY,
    LeftTrigger,
    RightTrigger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Source {
    pub timestamp_ns: u64,
    pub client_id: u32,
    pub device_id: u32,
    pub modifiers: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Keyboard {
    pub key: Key,
    pub device_key: Key,
    pub pressed: bool,
    pub repeat: bool,
    pub text: [char; 8],
    pub text_len: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pen {
    pub pressure: f32,
    pub tangential_pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub twist: f32,
    pub width: f32,
    pub height: f32,
    pub altitude: f32,
    pub azimuth: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub id: u32,
    pub phase: u8,
    pub kind: u8,
    pub button: u8,
    pub click_count: u8,
    pub flags: u16,
    pub buttons: u16,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
    /// Defined when the event targets an interactive canvas.
    pub canvas_x: f32,
    pub canvas_y: f32,
    pub pen: Option<Pen>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scroll {
    pub kind: u8,
    pub phase: u8,
    pub flags: u16,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
    pub wheel_z: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gesture {
    pub kind: u8,
    pub phase: u8,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
    pub scale: f32,
    pub rotation: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gamepad {
    pub phase: u8,
    pub control: u16,
    pub value: f32,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    Pointer(Pointer),
    Keyboard(Keyboard),
    Scroll(Scroll),
    Gesture(Gesture),
    Gamepad(Gamepad),
}

impl Input {
    pub fn kind(self) -> EventKind {
        match self {
            Self::Pointer(_) => EventKind::WindowPointer,
            Self::Keyboard(_) => EventKind::WindowKeyboard,
            Self::Scroll(_) => EventKind::WindowScroll,
            Self::Gesture(_) => EventKind::WindowGesture,
            Self::Gamepad(_) => EventKind::WindowGamepad,
        }
    }

    pub fn family(kind: EventKind) -> u32 {
        match kind {
            EventKind::WindowPointer => POINTER,
            EventKind::WindowKeyboard => KEYBOARD,
            EventKind::WindowScroll => SCROLL,
            EventKind::WindowGesture => GESTURE,
            EventKind::WindowGamepad => GAMEPAD,
            _ => 0,
        }
    }

    pub fn decode(kind: EventKind, bytes: &[u8]) -> Result<(Source, Self), Error> {
        let mut data = super::syscall::Reader::new(bytes)?;
        let source = Source {
            timestamp_ns: data.u64()?,
            client_id: data.u32()?,
            device_id: data.u32()?,
            modifiers: data.u16()?,
        };
        if source.modifiers & !MODIFIERS != 0 || data.u16()? != 0 {
            return Err(Error::InvalidArgument);
        }
        let input = match kind {
            EventKind::WindowPointer => Self::Pointer(Pointer::decode(&mut data)?),
            EventKind::WindowKeyboard => Self::Keyboard(Keyboard::decode(&mut data)?),
            EventKind::WindowScroll => Self::Scroll(Scroll::decode(&mut data)?),
            EventKind::WindowGesture => Self::Gesture(Gesture::decode(&mut data)?),
            EventKind::WindowGamepad => Self::Gamepad(Gamepad::decode(&mut data)?),
            _ => return Err(Error::InvalidArgument),
        };
        data.finish()?;
        Ok((source, input))
    }

    /// Host retirement edges may finish a sequence after a view unsubscribes.
    pub fn ends_sequence(self) -> bool {
        match self {
            Self::Pointer(value) => {
                matches!(value.phase, POINTER_UP | POINTER_CANCEL | POINTER_LEAVE)
            }
            Self::Keyboard(value) => !value.pressed,
            Self::Scroll(value) => value.phase == ENDED,
            Self::Gesture(value) => value.phase == ENDED,
            Self::Gamepad(value) => matches!(value.phase, GAMEPAD_DISCONNECTED | GAMEPAD_BUTTON_UP),
        }
    }

    pub fn encode(self, source: Source) -> alloc::vec::Vec<u8> {
        let mut data = alloc::vec::Vec::with_capacity(92);
        data.extend(source.timestamp_ns.to_le_bytes());
        data.extend(source.client_id.to_le_bytes());
        data.extend(source.device_id.to_le_bytes());
        data.extend(source.modifiers.to_le_bytes());
        data.extend([0; 2]);
        match self {
            Self::Pointer(v) => {
                data.extend(v.id.to_le_bytes());
                data.extend([v.phase, v.kind, v.button, v.click_count]);
                data.extend(v.flags.to_le_bytes());
                data.extend(v.buttons.to_le_bytes());
                floats(&mut data, [v.x, v.y, v.dx, v.dy, v.canvas_x, v.canvas_y]);
                if let Some(pen) = v.pen {
                    floats(
                        &mut data,
                        [
                            pen.pressure,
                            pen.tangential_pressure,
                            pen.tilt_x,
                            pen.tilt_y,
                            pen.twist,
                            pen.width,
                            pen.height,
                            pen.altitude,
                            pen.azimuth,
                        ],
                    );
                }
            }
            Self::Keyboard(v) => {
                data.extend((v.key as u16).to_le_bytes());
                data.extend((v.device_key as u16).to_le_bytes());
                data.extend([u8::from(v.pressed), u8::from(v.repeat), v.text_len, 0]);
                for scalar in v.text {
                    data.extend((scalar as u32).to_le_bytes());
                }
            }
            Self::Scroll(v) => {
                data.extend([v.kind, v.phase]);
                data.extend(v.flags.to_le_bytes());
                floats(&mut data, [v.x, v.y, v.dx, v.dy, v.wheel_z]);
            }
            Self::Gesture(v) => {
                data.extend([v.kind, v.phase, 0, 0]);
                floats(&mut data, [v.x, v.y, v.dx, v.dy, v.scale, v.rotation]);
            }
            Self::Gamepad(v) => {
                data.extend([v.phase, 0]);
                data.extend(v.control.to_le_bytes());
                floats(&mut data, [v.value, v.x, v.y]);
            }
        }
        data
    }
}

extern crate alloc;
use super::syscall::Reader;

fn floats<const N: usize>(out: &mut alloc::vec::Vec<u8>, values: [f32; N]) {
    for value in values {
        out.extend(value.to_le_bytes());
    }
}

fn float(data: &mut Reader<'_>) -> Result<f32, Error> {
    let value = f32::from_bits(data.u32()?);
    if value.is_finite() { Ok(value) } else { Err(Error::InvalidArgument) }
}

impl Pointer {
    fn decode(data: &mut Reader<'_>) -> Result<Self, Error> {
        let id = data.u32()?;
        let phase = data.u8()?;
        let kind = data.u8()?;
        let button = data.u8()?;
        let click_count = data.u8()?;
        let flags = data.u16()?;
        let buttons = data.u16()?;
        if id == 0
            || phase > POINTER_LEAVE
            || !(MOUSE..=PEN).contains(&kind)
            || (button >= 16 && button != NO_BUTTON)
            || flags & !63 != 0
        {
            return Err(Error::InvalidArgument);
        }
        let mut value = Self {
            id,
            phase,
            kind,
            button,
            click_count,
            flags,
            buttons,
            x: float(data)?,
            y: float(data)?,
            dx: float(data)?,
            dy: float(data)?,
            canvas_x: float(data)?,
            canvas_y: float(data)?,
            pen: None,
        };
        if kind == PEN {
            let pen = Pen {
                pressure: float(data)?,
                tangential_pressure: float(data)?,
                tilt_x: float(data)?,
                tilt_y: float(data)?,
                twist: float(data)?,
                width: float(data)?,
                height: float(data)?,
                altitude: float(data)?,
                azimuth: float(data)?,
            };
            if !(0. ..=1.).contains(&pen.pressure)
                || !(-1. ..=1.).contains(&pen.tangential_pressure)
                || !(-1. ..=1.).contains(&pen.tilt_x)
                || !(-1. ..=1.).contains(&pen.tilt_y)
                || !(0. ..360.).contains(&pen.twist)
                || pen.width < 0.
                || pen.height < 0.
                || !(0. ..=core::f32::consts::FRAC_PI_2).contains(&pen.altitude)
                || !(0. ..core::f32::consts::TAU).contains(&pen.azimuth)
            {
                return Err(Error::InvalidArgument);
            }
            value.pen = Some(pen);
        }
        Ok(value)
    }
}

impl Keyboard {
    fn decode(data: &mut Reader<'_>) -> Result<Self, Error> {
        let key = Key::from_code(data.u16()?).ok_or(Error::InvalidArgument)?;
        let device_key = Key::from_code(data.u16()?).ok_or(Error::InvalidArgument)?;
        let pressed = data.u8()?;
        let repeat = data.u8()?;
        let text_len = data.u8()?;
        if pressed > 1 || repeat > pressed || text_len > 8 || data.u8()? != 0 {
            return Err(Error::InvalidArgument);
        }
        let mut text = ['\0'; 8];
        for (index, scalar) in text.iter_mut().enumerate() {
            let value = data.u32()?;
            if index >= usize::from(text_len) && value != 0 {
                return Err(Error::InvalidArgument);
            }
            *scalar = char::from_u32(value).ok_or(Error::InvalidArgument)?;
        }
        Ok(Self { key, device_key, pressed: pressed == 1, repeat: repeat == 1, text, text_len })
    }
}

impl Scroll {
    fn decode(data: &mut Reader<'_>) -> Result<Self, Error> {
        let kind = data.u8()?;
        let phase = data.u8()?;
        let flags = data.u16()?;
        if !(MOUSE..=PEN).contains(&kind)
            || phase > MOMENTUM
            || flags & !15 != 0
            || flags & (SCROLL_LINES | SCROLL_PAGES) == (SCROLL_LINES | SCROLL_PAGES)
        {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            kind,
            phase,
            flags,
            x: float(data)?,
            y: float(data)?,
            dx: float(data)?,
            dy: float(data)?,
            wheel_z: float(data)?,
        })
    }
}

impl Gesture {
    fn decode(data: &mut Reader<'_>) -> Result<Self, Error> {
        let kind = data.u8()?;
        let phase = data.u8()?;
        if kind > ROTATE || phase > ENDED || data.u16()? != 0 {
            return Err(Error::InvalidArgument);
        }
        let value = Self {
            kind,
            phase,
            x: float(data)?,
            y: float(data)?,
            dx: float(data)?,
            dy: float(data)?,
            scale: float(data)?,
            rotation: float(data)?,
        };
        if value.scale <= 0. {
            return Err(Error::InvalidArgument);
        }
        Ok(value)
    }
}

impl Gamepad {
    fn decode(data: &mut Reader<'_>) -> Result<Self, Error> {
        let phase = data.u8()?;
        if phase > GAMEPAD_AXIS_MOVE || data.u8()? != 0 {
            return Err(Error::InvalidArgument);
        }
        let control = data.u16()?;
        let value = float(data)?;
        let x = float(data)?;
        let y = float(data)?;
        let valid = match phase {
            GAMEPAD_CONNECTED | GAMEPAD_DISCONNECTED => {
                control == 0 && value == 0. && x == 0. && y == 0.
            }
            GAMEPAD_BUTTON_DOWN | GAMEPAD_BUTTON_UP => {
                control <= GamepadButton::Home as u16
                    && (0. ..=1.).contains(&value)
                    && x == 0.
                    && y == 0.
            }
            GAMEPAD_AXIS_MOVE => {
                control <= GamepadAxis::RightTrigger as u16
                    && [value, x, y].iter().all(|value| (-1. ..=1.).contains(value))
            }
            _ => false,
        };
        if !valid {
            return Err(Error::InvalidArgument);
        }
        Ok(Self { phase, control, value, x, y })
    }
}
