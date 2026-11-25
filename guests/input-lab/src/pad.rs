use super::{Channel, PAD};
use playbit_api::abi::input::{self as raw, Input};
use playbit_api::ui::{CanvasFrames, CanvasItem, CanvasRect, CanvasText, Color, ViewEncoder};

const HEIGHT: u32 = 140;
const INK: u32 = 0x11232bff;
const GRID: u32 = 0x243b43ff;
const AXIS: u32 = 0x37545cff;
const MINT: u32 = 0x82ecd2ff;
const AMBER: u32 = 0xffc779ff;
const PAPER: u32 = 0xe7f7f4ff;

// Markers may cross an edge when pointer capture outlives the pad bounds.
struct Plot<'a> {
    width: u32,
    items: Vec<CanvasItem<'a>>,
}

impl<'a> Plot<'a> {
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let right = x.saturating_add(w).min(self.width as i32);
        let bottom = y.saturating_add(h).min(HEIGHT as i32);
        let x = x.max(0);
        let y = y.max(0);
        if right > x && bottom > y {
            self.items.push(CanvasItem::Rect(CanvasRect {
                x: x as u32,
                y: y as u32,
                width: (right - x) as u32,
                height: (bottom - y) as u32,
                color: Color(color),
            }));
        }
    }

    fn label(&mut self, x: u32, y: u32, size: u32, text: &'a str, color: u32) {
        self.items.push(CanvasItem::Text(CanvasText {
            bounds: CanvasRect {
                x,
                y,
                width: self.width - x,
                height: size + 6,
                color: Color(color),
            },
            font_size: size,
            bold: true,
            text,
        }));
    }

    fn marker(&mut self, x: i32, y: i32, color: u32) {
        self.rect(x - 8, y, 17, 1, color);
        self.rect(x, y - 8, 1, 17, color);
        self.rect(x - 2, y - 2, 5, 5, color);
    }

    fn vector(&mut self, dx: f32, dy: f32) {
        let cx = self.width as i32 / 2;
        let cy = HEIGHT as i32 / 2;
        let dx = dx.clamp(-(cx as f32 - 8.), cx as f32 - 8.);
        let dy = dy.clamp(-(cy as f32 - 8.), cy as f32 - 8.);
        for step in 0..24 {
            let t = step as f32 / 24.;
            self.rect(cx + (dx * t) as i32, cy + (dy * t) as i32, 2, 2, AXIS);
        }
        self.marker(cx + dx.round() as i32, cy + dy.round() as i32, MINT);
    }
}

pub fn draw(view: &mut ViewEncoder<'_>, family: u32, width: u32, channel: &Channel, offset: usize) {
    let mut plot = Plot { width, items: Vec::with_capacity(140) };
    let w = width as i32;
    let h = HEIGHT as i32;
    plot.rect(0, 0, w, h, INK);
    for x in (0..w).step_by(24) {
        plot.rect(x, 0, 1, h, GRID);
    }
    for y in (0..h).step_by(24) {
        plot.rect(0, y, w, 1, GRID);
    }
    plot.rect(w / 2, 0, 1, h, AXIS);
    plot.rect(0, h / 2, w, 1, AXIS);
    let title = match family {
        raw::POINTER => "CANVAS POSITION",
        raw::KEYBOARD => "KEY TRANSITIONS",
        raw::SCROLL => "SCROLL DELTA",
        raw::GESTURE => "GESTURE TRANSFORM",
        _ => "CONTROLLER",
    };
    let caption;
    if let Some(sample) = channel.events.get(offset) {
        match sample.input {
            Input::Pointer(_) => {
                for (age, past) in channel.events.iter().skip(offset).enumerate().rev() {
                    let Input::Pointer(p) = past.input else { continue };
                    if past.target != PAD
                        || p.canvas_x < 0.
                        || p.canvas_y < 0.
                        || p.canvas_x >= width as f32
                        || p.canvas_y >= HEIGHT as f32
                    {
                        continue;
                    }
                    let (x, y) = (p.canvas_x as i32, p.canvas_y as i32);
                    if age == 0 {
                        plot.marker(
                            x,
                            y,
                            if p.buttons != 0 || p.flags & raw::IN_CONTACT != 0 {
                                AMBER
                            } else {
                                MINT
                            },
                        );
                    } else {
                        plot.rect(x - 1, y - 1, 3, 3, AXIS);
                    }
                }
            }
            Input::Keyboard(k) => {
                plot.label(12, 44, 26, raw::KEY_NAMES[k.key as usize], PAPER);
                for (i, past) in channel.events.iter().skip(offset).take(16).rev().enumerate() {
                    let Input::Keyboard(key) = past.input else { continue };
                    let step = (w - 24) / 16;
                    let height = if key.pressed { 25 } else { 6 };
                    let color = if key.repeat {
                        AMBER
                    } else if key.pressed {
                        MINT
                    } else {
                        AXIS
                    };
                    plot.rect(12 + i as i32 * step, 125 - height, step - 2, height, color);
                }
            }
            Input::Scroll(s) => plot.vector(s.dx, s.dy),
            Input::Gesture(g) => match g.kind {
                raw::PAN => plot.vector(g.dx, g.dy),
                raw::PINCH => {
                    let r = (28. * g.scale).clamp(3., 60.) as i32;
                    plot.rect(w / 2 - r, h / 2, 2 * r, 2, MINT);
                    plot.marker(w / 2 - r, h / 2, AMBER);
                    plot.marker(w / 2 + r, h / 2, AMBER);
                }
                _ => plot.vector(46. * g.rotation.cos(), 46. * g.rotation.sin()),
            },
            Input::Gamepad(g) => {
                if g.phase == raw::GAMEPAD_AXIS_MOVE {
                    plot.vector(g.x * (w / 2 - 14) as f32, -g.y * (h / 2 - 10) as f32);
                } else {
                    caption = sample.summary();
                    plot.label(12, 43, 20, &caption, PAPER);
                }
                if matches!(
                    g.phase,
                    raw::GAMEPAD_BUTTON_DOWN | raw::GAMEPAD_BUTTON_UP | raw::GAMEPAD_AXIS_MOVE
                ) {
                    let half = (w - 24) / 2;
                    let length = (g.value * half as f32).round() as i32;
                    plot.rect(12, h - 14, w - 24, 3, AXIS);
                    plot.rect(w / 2 + length.min(0), h - 15, length.abs(), 5, AMBER);
                }
            }
        }
    }
    plot.label(10, 7, 10, title, MINT);
    view.canvas(PAD, width, HEIGHT, CanvasFrames::Static, &plot.items);
}
