use playbit_api::abi::syscall::{KeyInput, PointerInput};
use playbit_api::ui::{CanvasRect, Color};

pub const HEIGHT: u32 = 250;
pub const NOTE_MS: u32 = 500;

#[derive(Debug, PartialEq)]
pub enum Action {
    Play(usize),
    Volume(i32),
}

pub struct Keyboard {
    pub width: u32,
    pub selected: usize,
    pub lit: [u32; 8],
    pointer_pad: Option<usize>,
    keys: [bool; 5],
}

impl Default for Keyboard {
    fn default() -> Self {
        Self { width: 720, selected: 0, lit: [0; 8], pointer_pad: None, keys: [false; 5] }
    }
}

impl Keyboard {
    pub fn pad(&self, index: usize) -> CanvasRect {
        let stride = (self.width - 32) / 8;
        CanvasRect {
            x: 16 + index as u32 * stride,
            y: 24,
            width: stride - 4,
            height: 202,
            color: Color(0),
        }
    }

    fn pad_at(&self, x: u32, y: u32) -> Option<usize> {
        (0..8).find(|&index| {
            let pad = self.pad(index);
            x >= pad.x && x < pad.x + pad.width && y >= pad.y && y < pad.y + pad.height
        })
    }

    pub fn pointer(&mut self, input: PointerInput) -> Option<Action> {
        let pressed = input.phase == 1 || (input.phase == 0 && input.buttons == 1);
        let pad = pressed.then(|| self.pad_at(input.x, input.y)).flatten();
        if pad == self.pointer_pad {
            return None;
        }
        self.pointer_pad = pad;
        pad.map(|index| {
            self.selected = index;
            Action::Play(index)
        })
    }

    pub fn key(&mut self, input: KeyInput) -> Option<Action> {
        let held = &mut self.keys[(input.key - 1) as usize];
        let trigger = input.pressed && !*held;
        *held = input.pressed;
        if !trigger {
            return None;
        }
        match input.key {
            1 => self.selected = self.selected.saturating_sub(1),
            2 => self.selected = (self.selected + 1).min(7),
            3 => return Some(Action::Volume(10)),
            4 => return Some(Action::Volume(-10)),
            5 => return Some(Action::Play(self.selected)),
            _ => {}
        }
        None
    }

    pub fn advance(&mut self, elapsed_ms: u32) {
        for remaining in &mut self.lit {
            *remaining = remaining.saturating_sub(elapsed_ms);
        }
    }

    pub fn cancel(&mut self) {
        self.pointer_pad = None;
        self.keys.fill(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(x: u32, phase: u32, buttons: u32) -> PointerInput {
        PointerInput { x, y: 80, phase, buttons }
    }

    #[test]
    fn drag_triggers_new_pads_and_release_or_cancel_allows_retrigger() {
        let mut keyboard = Keyboard::default();
        let x = keyboard.pad(0).x + 4;
        let next_x = keyboard.pad(1).x + 4;
        assert_eq!(keyboard.pointer(pointer(x, 1, 1)), Some(Action::Play(0)));
        assert_eq!(keyboard.pointer(pointer(x + 1, 0, 1)), None);
        assert_eq!(keyboard.pointer(pointer(next_x, 0, 1)), Some(Action::Play(1)));
        assert_eq!(keyboard.pointer(pointer(next_x, 2, 0)), None);
        assert_eq!(keyboard.pointer(pointer(next_x, 1, 1)), Some(Action::Play(1)));
        assert_eq!(keyboard.pointer(pointer(next_x, 3, 0)), None);
        assert_eq!(keyboard.pointer(pointer(next_x, 0, 0)), None);
        assert_eq!(keyboard.pointer(pointer(next_x, 1, 1)), Some(Action::Play(1)));
    }

    #[test]
    fn keyboard_selection_volume_and_highlights_follow_input() {
        let mut keyboard = Keyboard::default();
        for _ in 0..10 {
            keyboard.key(KeyInput { key: 2, pressed: true });
            keyboard.key(KeyInput { key: 2, pressed: false });
        }
        assert_eq!(keyboard.selected, 7);
        assert_eq!(keyboard.key(KeyInput { key: 5, pressed: true }), Some(Action::Play(7)));
        assert_eq!(keyboard.key(KeyInput { key: 5, pressed: true }), None);
        keyboard.cancel();
        assert_eq!(keyboard.key(KeyInput { key: 3, pressed: true }), Some(Action::Volume(10)));
        keyboard.lit[7] = NOTE_MS;
        keyboard.advance(200);
        assert_eq!(keyboard.lit[7], 300);
        keyboard.advance(400);
        assert_eq!(keyboard.lit[7], 0);
        for width in [240, 360, 720, 960] {
            keyboard.width = width;
            assert!((0..8).all(|note| keyboard.pad(note).fits(width, HEIGHT)));
        }
    }
}
