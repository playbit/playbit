use super::Sample;
use playbit_api::abi::input::{self as raw, Input};
use playbit_api::ui::{TextStyle, ViewEncoder};

fn flags(bits: u16, names: &[(u16, &str)]) -> String {
    let active: Vec<_> =
        names.iter().filter(|(bit, _)| bits & bit != 0).map(|(_, name)| *name).collect();
    if active.is_empty() { "none".into() } else { active.join(" + ") }
}

fn device(kind: u8) -> &'static str {
    match kind {
        raw::MOUSE => "Mouse",
        raw::TOUCH => "Touch",
        raw::TRACKPAD => "Trackpad",
        raw::PEN => "Pen",
        _ => "Unknown device",
    }
}

fn phase(value: u8) -> &'static str {
    match value {
        raw::BEGAN => "began",
        raw::CHANGED => "changed",
        raw::ENDED => "ended",
        raw::MOMENTUM => "momentum",
        _ => "unknown phase",
    }
}

const BUTTONS: [&str; 13] = [
    "A",
    "B",
    "X",
    "Y",
    "Left shoulder",
    "Right shoulder",
    "Left trigger",
    "Right trigger",
    "Left stick",
    "Right stick",
    "Menu",
    "Options",
    "Home",
];
const AXES: [&str; 8] = [
    "Left X",
    "Left Y",
    "Right X",
    "Right Y",
    "D-pad X",
    "D-pad Y",
    "Left trigger",
    "Right trigger",
];

impl Sample {
    pub fn summary(&self) -> String {
        match self.input {
            Input::Pointer(p) => {
                let action = match p.phase {
                    raw::POINTER_MOVE => "move",
                    raw::POINTER_DOWN => "down",
                    raw::POINTER_UP => "up",
                    raw::POINTER_CANCEL => "cancel",
                    raw::POINTER_ENTER => "enter",
                    raw::POINTER_LEAVE => "leave",
                    _ => "unknown phase",
                };
                format!("{} {action} · contact {}", device(p.kind), p.id)
            }
            Input::Keyboard(k) => {
                let action = if k.repeat {
                    "repeat"
                } else if k.pressed {
                    "down"
                } else {
                    "up"
                };
                format!("{} {action}", raw::KEY_NAMES[k.key as usize])
            }
            Input::Scroll(s) => format!("{} scroll · {}", device(s.kind), phase(s.phase)),
            Input::Gesture(g) => {
                let name = match g.kind {
                    raw::PAN => "Pan",
                    raw::PINCH => "Pinch",
                    raw::ROTATE => "Rotate",
                    _ => "Unknown gesture",
                };
                format!("{name} {}", phase(g.phase))
            }
            Input::Gamepad(g) => match g.phase {
                raw::GAMEPAD_CONNECTED => "Gamepad connected".into(),
                raw::GAMEPAD_DISCONNECTED => "Gamepad disconnected".into(),
                raw::GAMEPAD_BUTTON_DOWN | raw::GAMEPAD_BUTTON_UP => {
                    let action = if g.phase == raw::GAMEPAD_BUTTON_DOWN { "down" } else { "up" };
                    format!("{} {action}", BUTTONS[g.control as usize])
                }
                raw::GAMEPAD_AXIS_MOVE => format!("{} axis", AXES[g.control as usize]),
                _ => "Unknown gamepad event".into(),
            },
        }
    }

    pub fn details(&self, view: &mut ViewEncoder<'_>) {
        let mut line = |text: String| view.text(TextStyle::Body, &text);
        match self.input {
            Input::Pointer(p) => {
                line(format!(
                    "Window position ({:.2}, {:.2}) · Delta ({:.2}, {:.2})",
                    p.x, p.y, p.dx, p.dy
                ));
                if self.target != 0 {
                    line(format!("Canvas position ({:.2}, {:.2})", p.canvas_x, p.canvas_y));
                }
                let button =
                    if p.button == raw::NO_BUTTON { "none".into() } else { p.button.to_string() };
                line(format!(
                    "Changed button {button} · Held mask {:#06x} · Clicks {}",
                    p.buttons, p.click_count
                ));
                line(format!(
                    "Flags {}",
                    flags(
                        p.flags,
                        &[
                            (raw::PRIMARY, "primary"),
                            (raw::IN_CONTACT, "in contact"),
                            (raw::ERASER, "eraser"),
                            (raw::INVERTED, "inverted"),
                            (raw::COALESCED, "coalesced"),
                            (raw::PREDICTED, "predicted"),
                        ]
                    )
                ));
                if let Some(pen) = p.pen {
                    line(format!(
                        "Pressure {:.3} · Tangential {:.3} · Tilt ({:.3}, {:.3})",
                        pen.pressure, pen.tangential_pressure, pen.tilt_x, pen.tilt_y
                    ));
                    line(format!(
                        "Contact {:.2} × {:.2} · Twist {:.2}°",
                        pen.width, pen.height, pen.twist
                    ));
                    line(format!(
                        "Altitude {:.3} rad · Azimuth {:.3} rad",
                        pen.altitude, pen.azimuth
                    ));
                }
            }
            Input::Keyboard(k) => {
                let text: String = k.text[..usize::from(k.text_len)].iter().collect();
                line(format!(
                    "Logical {} · Physical {}",
                    raw::KEY_NAMES[k.key as usize],
                    raw::KEY_NAMES[k.device_key as usize]
                ));
                line(format!("Text {text:?} · {} Unicode scalars", k.text_len));
            }
            Input::Scroll(s) => {
                let units = if s.flags & raw::SCROLL_LINES != 0 {
                    "lines"
                } else if s.flags & raw::SCROLL_PAGES != 0 {
                    "pages"
                } else {
                    "logical units"
                };
                line(format!("Delta ({:.2}, {:.2}, {:.2}) · {units}", s.dx, s.dy, s.wheel_z));
                line(format!("Window position ({:.2}, {:.2})", s.x, s.y));
                line(format!(
                    "Flags {}",
                    flags(
                        s.flags,
                        &[(raw::SCROLL_PRECISE, "precise"), (raw::SCROLL_INVERTED, "inverted")]
                    )
                ));
            }
            Input::Gesture(g) => {
                line(format!("Center ({:.2}, {:.2}) · Delta ({:.2}, {:.2})", g.x, g.y, g.dx, g.dy));
                line(format!("Scale {:.3}× · Rotation {:.3} rad", g.scale, g.rotation));
            }
            Input::Gamepad(g) => {
                if matches!(
                    g.phase,
                    raw::GAMEPAD_BUTTON_DOWN | raw::GAMEPAD_BUTTON_UP | raw::GAMEPAD_AXIS_MOVE
                ) {
                    line(format!(
                        "Control {} · Value {:.3} · Pair ({:.3}, {:.3})",
                        g.control, g.value, g.x, g.y
                    ));
                }
            }
        }
        line(format!(
            "Window {} · Target {} · Client {} · Device {}",
            self.window, self.target, self.source.client_id, self.source.device_id
        ));
        line(format!(
            "{} ns · Modifiers {}",
            self.source.timestamp_ns,
            flags(
                self.source.modifiers,
                &[
                    (raw::SHIFT, "Shift"),
                    (raw::CTRL, "Ctrl"),
                    (raw::ALT, "Alt"),
                    (raw::META, "Meta"),
                    (raw::CAPS_LOCK, "Caps Lock"),
                    (raw::FN, "Fn"),
                ]
            )
        ));
    }
}
