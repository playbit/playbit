//! Phase 1 multimedia experiment: one playable kart, no multiplayer yet.
#![cfg(any(target_arch = "wasm32", test))]
use playbit_api::abi::{
    input::{Input, Key, POINTER_DOWN, POINTER_MOVE, PRIMARY},
    syscall::{Error, EventKind, FrameInput},
};
#[cfg(target_arch = "wasm32")]
use playbit_api::context::Context;
use playbit_api::ui::{CanvasItem, CanvasRect, CanvasText, Color};
#[cfg(target_arch = "wasm32")]
use playbit_api::ui::{TextStyle, ViewEncoder};

#[cfg(target_arch = "wasm32")]
const CANVAS: u32 = 1;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 400;

#[derive(Default)]
struct Game {
    x: f64,
    speed: f64,
    distance: f64,
    left: bool,
    right: bool,
    throttle: bool,
    brake: bool,
    pointer: bool,
}

impl Game {
    fn new() -> Self {
        Self { x: 148.0, ..Self::default() }
    }

    fn input(&mut self, kind: EventKind, data: &[u8]) -> Result<(), Error> {
        match kind {
            EventKind::WindowPointer => {
                let (_, Input::Pointer(input)) = Input::decode(kind, data)? else {
                    return Err(Error::InvalidArgument);
                };
                if input.flags & PRIMARY != 0 {
                    self.pointer = input.buttons & 1 != 0
                        && matches!(input.phase, POINTER_DOWN | POINTER_MOVE);
                    if self.pointer {
                        self.x = (f64::from(input.canvas_x) - 12.0).clamp(42.0, 254.0);
                    }
                }
            }
            EventKind::WindowKeyboard => {
                let (_, Input::Keyboard(input)) = Input::decode(kind, data)? else {
                    return Err(Error::InvalidArgument);
                };
                match input.device_key {
                    Key::Left => self.left = input.pressed,
                    Key::Right => self.right = input.pressed,
                    Key::Up | Key::Space => self.throttle = input.pressed,
                    Key::Down => self.brake = input.pressed,
                    _ => {}
                }
            }
            EventKind::Frame => {
                let dt = f64::from(FrameInput::decode(data)?.elapsed_us) / 1_000_000.0;
                let acceleration = if self.brake {
                    -300.0
                } else if self.throttle || self.pointer {
                    160.0
                } else {
                    -100.0
                };
                self.speed = (self.speed + acceleration * dt).clamp(0.0, 280.0);
                self.x = (self.x
                    + (i32::from(self.right) - i32::from(self.left)) as f64 * 150.0 * dt)
                    .clamp(42.0, 254.0);
                self.distance += self.speed * dt;
            }
            _ => {}
        }
        Ok(())
    }

    fn drawing(&self) -> Vec<CanvasItem<'static>> {
        let mut rects = vec![
            rect(0, 0, WIDTH, HEIGHT, 0x346b43ff),
            rect(36, 0, 248, HEIGHT, 0xeee5c6ff),
            rect(42, 0, 236, HEIGHT, 0x343a46ff),
        ];
        let offset = self.distance as u32 % 64;
        for y in (0..HEIGHT).step_by(64) {
            let y = (y + offset) % HEIGHT;
            rects.push(rect(158, y, 4, 26.min(HEIGHT - y), 0xd7dddeff));
        }
        let x = self.x as u32;
        rects.extend([
            rect(x - 4, 311, 7, 15, 0x10151bff),
            rect(x + 21, 311, 7, 15, 0x10151bff),
            rect(x - 4, 341, 7, 15, 0x10151bff),
            rect(x + 21, 341, 7, 15, 0x10151bff),
            rect(x, 307, 24, 52, 0xf6b62eff),
            rect(x + 4, 321, 16, 17, 0x26374dff),
            rect(x + 3, 310, 18, 4, 0xffffd8ff),
            rect(x + 3, 352, 18, 4, 0xe3472cff),
        ]);
        rects.push(rect(50, 8, 220, 28, 0x202631ee));
        rects.push(CanvasItem::Text(CanvasText {
            bounds: CanvasRect { x: 60, y: 12, width: 200, height: 20, color: Color(0xffffffff) },
            font_size: 13,
            bold: false,
            text: if self.pointer || self.throttle {
                "Driving"
            } else {
                "Hold to drive · drag to steer"
            },
        }));
        rects
    }

    #[cfg(target_arch = "wasm32")]
    fn render(
        &mut self,
        context: &mut Context<'_>,
        view: &mut ViewEncoder<'_>,
    ) -> Result<(), Error> {
        let drawing = self.drawing();
        view.window_input(playbit_api::abi::input::POINTER | playbit_api::abi::input::KEYBOARD);
        view.column_start();
        view.text(TextStyle::Title, "Micro Kart");
        view.text(
            TextStyle::Body,
            &format!("Speed {:.0} · Distance {:.0}", self.speed, self.distance),
        );
        view.canvas(CANVAS, WIDTH, HEIGHT, playbit_api::ui::CanvasFrames::Continuous, &drawing);
        view.text(
            TextStyle::Muted,
            "Hold and drag to drive. Keyboard: ↑ accelerate, ← → steer, ↓ brake.",
        );
        view.column_end();
        view.submit(context)
    }
}

fn rect(x: u32, y: u32, width: u32, height: u32, color: u32) -> CanvasItem<'static> {
    CanvasItem::Rect(CanvasRect { x, y, width, height, color: Color(color) })
}

#[allow(unsafe_code, reason = "The SDK resolves this app registration by its ABI name.")]
#[unsafe(no_mangle)]
#[cfg(target_arch = "wasm32")]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(Game::new(), turn, true)
}

#[cfg(target_arch = "wasm32")]
fn turn(game: &mut Game, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    for event in context.raw_events() {
        if event.target == CANVAS || event.kind == EventKind::WindowKeyboard {
            game.input(event.kind, event.data).unwrap();
        }
    }
    game.render(context, view).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_and_pointer_change_actual_simulation_and_pixels() {
        let mut game = Game::new();
        let initial = game.drawing();
        use playbit_api::abi::input::{Keyboard, Pointer, Source};
        for key in [Key::Up, Key::Left] {
            let input = Input::Keyboard(Keyboard {
                key,
                device_key: key,
                pressed: true,
                repeat: false,
                text: ['\0'; 8],
                text_len: 0,
            });
            game.input(input.kind(), &input.encode(Source::default())).unwrap();
        }
        for _ in 0..60 {
            game.input(EventKind::Frame, &FrameInput { elapsed_us: 16_667 }.encode()).unwrap();
        }
        assert!(game.speed > 150.0 && game.distance > 75.0 && game.x < 100.0);
        assert_ne!(game.drawing(), initial);
        let mut pointer = Pointer {
            id: 1,
            kind: 1,
            phase: POINTER_DOWN,
            buttons: 1,
            button: 0,
            flags: PRIMARY,
            click_count: 1,
            x: 220.,
            y: 350.,
            canvas_x: 220.,
            canvas_y: 350.,
            dx: 0.,
            dy: 0.,
            pen: None,
        };
        let input = Input::Pointer(pointer);
        game.input(input.kind(), &input.encode(Source::default())).unwrap();
        assert_eq!(game.x, 208.0);
        assert!(game.pointer);
        pointer.phase = playbit_api::abi::input::POINTER_CANCEL;
        pointer.buttons = 0;
        let input = Input::Pointer(pointer);
        game.input(input.kind(), &input.encode(Source::default())).unwrap();
        assert!(!game.pointer);
        assert!(game.drawing().iter().all(|item| match item {
            CanvasItem::Rect(rect) => rect.fits(WIDTH, HEIGHT),
            CanvasItem::Text(text) => text.fits(WIDTH, HEIGHT),
            CanvasItem::Pixels(pixels) => pixels.fits(WIDTH, HEIGHT),
        }));
    }
}
