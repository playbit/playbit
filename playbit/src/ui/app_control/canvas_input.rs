//! Names used by both developer tools map to the ordinary guest canvas ABI.
use super::{Operation, Result};
use playbit_api::abi::syscall::{EventKind, KeyInput, PointerInput};

impl Operation {
    pub fn key(target: u32, key: &str, pressed: bool) -> Result<Self> {
        if target == 0 {
            return Err("Canvas input requires a nonzero target".into());
        }
        let key = match key {
            "left" => 1,
            "right" => 2,
            "up" => 3,
            "down" => 4,
            "space" => 5,
            _ => return Err("Expected canvas key left, right, up, down or space".into()),
        };
        Ok(Self::Input {
            kind: EventKind::Key as u32,
            target,
            data: KeyInput { key, pressed }.encode().into(),
        })
    }

    pub fn pointer(target: u32, phase: &str, x: u32, y: u32, button: Option<&str>) -> Result<Self> {
        if target == 0 {
            return Err("Canvas input requires a nonzero target".into());
        }
        let phase = match phase {
            "move" => 0,
            "down" => 1,
            "up" => 2,
            "cancel" => 3,
            _ => return Err("Expected pointer phase move, down, up or cancel".into()),
        };
        let buttons = match button {
            None => u32::from(phase == 1),
            Some("primary") if phase <= 1 => 1,
            Some("none") if phase != 1 => 0,
            _ => {
                return Err("Expected primary for down, none for up/cancel, either for move".into());
            }
        };
        Ok(Self::Input {
            kind: EventKind::Pointer as u32,
            target,
            data: PointerInput { x, y, phase, buttons }.encode().into(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests decode the bytes received by the guest.")]
mod tests {
    use super::*;

    #[test]
    fn tool_gestures_encode_independent_edges_and_canvas_coordinates() {
        for pressed in [false, true] {
            let Operation::Input { kind, target, data } = Operation::key(7, "up", pressed).unwrap()
            else {
                panic!("expected input")
            };
            assert_eq!((kind, target), (EventKind::Key as u32, 7));
            assert_eq!(KeyInput::decode(&data).unwrap(), KeyInput { key: 3, pressed });
        }
        for (phase, button, expected_phase, buttons) in [
            ("down", None, 1, 1),
            ("move", Some("primary"), 0, 1),
            ("up", None, 2, 0),
            ("move", None, 0, 0),
            ("cancel", None, 3, 0),
        ] {
            let Operation::Input { kind, target, data } =
                Operation::pointer(9, phase, 21, 33, button).unwrap()
            else {
                panic!("expected input")
            };
            assert_eq!((kind, target), (EventKind::Pointer as u32, 9));
            assert_eq!(
                PointerInput::decode(&data).unwrap(),
                PointerInput { x: 21, y: 33, phase: expected_phase, buttons }
            );
        }
        assert!(Operation::key(0, "up", true).is_err());
        assert!(Operation::key(1, "unknown", false).is_err());
        assert!(Operation::pointer(1, "up", 1, 1, Some("primary")).is_err());
    }
}
