//! Synthetic input for the selected guest window.
use super::{Operation, Result};
use playbit_api::abi::{
    input::{self, Input, Key, Keyboard, Source},
    syscall::{EventKind, KeyInput, PointerInput},
};

pub(crate) const CANVAS_KEYS: [Key; 5] = [Key::Left, Key::Right, Key::Up, Key::Down, Key::Space];

impl Operation {
    pub fn key(target: impl Into<Option<u32>>, key: &str, pressed: bool) -> Result<Self> {
        let key = Key::from_name(key)
            .filter(|key| *key != Key::None)
            .ok_or("Unknown Playbit key name")?;
        if let Some(target) = target.into() {
            if target == 0 {
                return Err("Canvas input requires a nonzero target".into());
            }
            let code = CANVAS_KEYS
                .iter()
                .position(|&candidate| candidate == key)
                .ok_or("Expected canvas key left, right, up, down or space")?;
            return Ok(Self::Input {
                kind: EventKind::Key as u32,
                target,
                data: KeyInput { key: code as u32 + 1, pressed }.encode().into(),
            });
        }
        let input = keyboard(key, pressed);
        Ok(Self::Input { kind: input.kind() as u32, target: 0, data: tool_data(input) })
    }

    pub fn pointer(target: u32, phase: &str, x: u32, y: u32, button: Option<&str>) -> Result<Self> {
        if target == 0 {
            return Err("Synthetic canvas input requires a nonzero target".into());
        }
        let phase = match phase {
            "move" => input::POINTER_MOVE,
            "down" => input::POINTER_DOWN,
            "up" => input::POINTER_UP,
            "cancel" => input::POINTER_CANCEL,
            _ => return Err("Expected pointer phase move, down, up or cancel".into()),
        };
        let buttons = match button {
            None => u16::from(phase == input::POINTER_DOWN),
            Some("primary") if phase <= input::POINTER_DOWN => 1,
            Some("none") if phase != input::POINTER_DOWN => 0,
            _ => {
                return Err("Expected primary for down, none for up/cancel, either for move".into());
            }
        };
        let pointer = PointerInput { x, y, phase: u32::from(phase), buttons: u32::from(buttons) };
        Ok(Self::Input { kind: EventKind::Pointer as u32, target, data: pointer.encode().into() })
    }
}

pub(crate) fn keyboard(key: Key, pressed: bool) -> Input {
    Input::Keyboard(Keyboard {
        key,
        device_key: key,
        pressed,
        repeat: false,
        text: ['\0'; 8],
        text_len: 0,
    })
}

fn tool_data(input: Input) -> Vec<u8> {
    input.encode(Source { timestamp_ns: playbit_trace::clock_ns(), ..Source::default() })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests decode the bytes received by the guest.")]
mod tests {
    use super::*;

    #[test]
    fn tool_input_encodes_independent_keys_and_canvas_coordinates() {
        for pressed in [false, true] {
            let Operation::Input { kind, target, data } =
                Operation::key(None, "f12", pressed).unwrap()
            else {
                panic!("expected input")
            };
            assert_eq!((kind, target), (EventKind::WindowKeyboard as u32, 0));
            let (_, Input::Keyboard(key)) =
                Input::decode(EventKind::WindowKeyboard, &data).unwrap()
            else {
                panic!("expected keyboard input")
            };
            assert_eq!((key.key, key.device_key, key.pressed), (Key::F12, Key::F12, pressed));
        }
        let Operation::Input { kind, target, data } = Operation::key(7, "up", true).unwrap() else {
            panic!("expected legacy canvas key")
        };
        assert_eq!((kind, target), (EventKind::Key as u32, 7));
        assert_eq!(KeyInput::decode(&data).unwrap(), KeyInput { key: 3, pressed: true });
        let Operation::Input { kind, target, data } =
            Operation::pointer(9, "move", 21, 33, Some("primary")).unwrap()
        else {
            panic!("expected input")
        };
        assert_eq!((kind, target), (EventKind::Pointer as u32, 9));
        assert_eq!(
            PointerInput::decode(&data).unwrap(),
            PointerInput { x: 21, y: 33, phase: 0, buttons: 1 }
        );
        assert!(Operation::key(None, "unknown", false).is_err());
        assert!(Operation::pointer(1, "up", 1, 1, Some("primary")).is_err());
    }
}
