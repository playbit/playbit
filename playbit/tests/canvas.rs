#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::runtime::{
    ViewNode, decode_view,
    syscall::{Event, validate_input},
};
use playbit_api::abi::{
    input::{self, Input, Key, Keyboard, Pointer, Source},
    syscall::{EventKind, FrameInput},
};
use playbit_api::ui::{CanvasFrames, CanvasItem, CanvasRect, Color};

fn view() -> Vec<u8> {
    let mut encoder_state = playbit_api::context::ContextState::default();
    let mut encoder = encoder_state.view();
    encoder.column_start();
    encoder.canvas(
        17,
        320,
        400,
        CanvasFrames::Continuous,
        &[CanvasItem::Rect(CanvasRect {
            x: 2,
            y: 3,
            width: 20,
            height: 30,
            color: Color(0xaabbccff),
        })],
    );
    encoder.column_end();
    encoder.finish()
}

#[test]
fn canvas_round_trip_and_decoder_enforce_geometry() {
    let mut encoded = view();
    let ViewNode::Theme { child, .. } = decode_view(&encoded).unwrap() else { panic!() };
    let ViewNode::Column(children) = *child else { panic!() };
    let ViewNode::Canvas { id, width, height, items, frames, .. } = &children[0] else { panic!() };
    assert_eq!((*id, *width, *height), (17, 320, 400));
    assert_eq!(*frames, playbit_api::ui::CanvasFrames::Continuous);
    assert!(matches!(
        items[0],
        playbit::runtime::CanvasItem::Rect(CanvasRect { color: Color(0xaabbccff), .. })
    ));
    // Header 4 + column command 9 + canvas command header 9 + dimensions/frame mode/count 16 + item tag 4.
    encoded[42..46].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_view(&encoded).is_err());
}

#[test]
fn window_input_admission_uses_family_and_origin_and_canvas_frames_are_opt_in() {
    let mut bytes = view();
    let mut input_command = vec![playbit_api::ui::VIEW_WINDOW_INPUT];
    input_command.extend(0u32.to_le_bytes());
    input_command.extend(4u32.to_le_bytes());
    input_command.extend((input::POINTER | input::KEYBOARD).to_le_bytes());
    bytes.splice(4..4, input_command);
    let scene = decode_view(&bytes).unwrap();
    let pointer = Input::Pointer(Pointer {
        id: 2,
        kind: input::MOUSE,
        phase: input::POINTER_DOWN,
        button: 0,
        click_count: 1,
        flags: input::PRIMARY,
        buttons: 1,
        x: 325.,
        y: 410.,
        dx: 0.,
        dy: 0.,
        canvas_x: 320.,
        canvas_y: 400.,
        pen: None,
    });
    let mut event = Event {
        object_id: 1,
        kind: pointer.kind(),
        target: 17,
        data: pointer.encode(Source::default()),
    };
    assert!(validate_input(&scene, &event).is_ok());
    event.target = 0;
    assert!(validate_input(&scene, &event).is_ok());
    event.object_id = 0;
    assert!(validate_input(&scene, &event).is_err());
    let keyboard = Input::Keyboard(Keyboard {
        key: Key::Q,
        device_key: Key::A,
        pressed: true,
        repeat: false,
        text: ['\0'; 8],
        text_len: 0,
    });
    event = Event {
        object_id: 2,
        kind: keyboard.kind(),
        target: 0,
        data: keyboard.encode(Source::default()),
    };
    assert!(validate_input(&scene, &event).is_ok());
    event.target = 17;
    assert!(validate_input(&scene, &event).is_err());
    event = Event {
        object_id: 2,
        kind: EventKind::Frame,
        target: 17,
        data: FrameInput { elapsed_us: 16_667 }.encode().into(),
    };
    assert!(validate_input(&scene, &event).is_ok());
    bytes[43..47].copy_from_slice(&(CanvasFrames::Static as u32).to_le_bytes());
    assert!(validate_input(&decode_view(&bytes).unwrap(), &event).is_err());
}

#[test]
#[ignore = "optimized component qualification; run without competing build/proof work"]
fn optimized_canvas_encode_decode() {
    use std::{hint::black_box, time::Instant};
    for count in [0, 1024] {
        let rectangles = vec![
            CanvasItem::Rect(CanvasRect {
                x: 1,
                y: 1,
                width: 10,
                height: 10,
                color: Color(0xffaabbff)
            });
            count
        ];
        for frames in [CanvasFrames::Static, CanvasFrames::Continuous] {
            let mut samples = Vec::with_capacity(1000);
            for _ in 0..1000 {
                let started = Instant::now();
                let mut encoder_state = playbit_api::context::ContextState::default();
                let mut encoder = encoder_state.view();
                encoder.column_start();
                encoder.canvas(17, 320, 400, frames, black_box(&rectangles));
                encoder.column_end();
                let view = decode_view(black_box(&encoder.finish())).unwrap();
                black_box(view);
                samples.push(started.elapsed().as_nanos());
            }
            println!("canvas_samples rectangles={count} frames={frames:?} values_ns={samples:?}");
            samples.sort_unstable();
            println!(
                "canvas_component rectangles={count} frames={frames:?} samples=1000 p50_ns={} p99_ns={} max_ns={}",
                samples[499], samples[989], samples[999]
            );
        }
    }
}

fn canvas_scene(items: &[CanvasItem<'_>]) -> Vec<u8> {
    let mut encoder_state = playbit_api::context::ContextState::default();
    let mut encoder = encoder_state.view();
    encoder.column_start();
    encoder.canvas(17, 320, 240, CanvasFrames::Static, items);
    encoder.column_end();
    encoder.finish()
}

fn label(text: &str) -> CanvasItem<'_> {
    CanvasItem::Text(playbit_api::ui::CanvasText {
        bounds: CanvasRect { x: 2, y: 3, width: 100, height: 24, color: Color(0xaabbccff) },
        font_size: 16,
        bold: true,
        text,
    })
}

#[test]
fn canvas_text_round_trips_and_rejects_malformed_external_fields() {
    use playbit::runtime::CanvasItem as Decoded;
    let original = canvas_scene(&[label("日本語 é →")]);
    let ViewNode::Theme { child, .. } = decode_view(&original).unwrap() else { panic!() };
    let ViewNode::Column(children) = *child else { panic!() };
    let ViewNode::Canvas { items, .. } = &children[0] else { panic!() };
    assert!(matches!(&items[0], Decoded::Text { bounds, font_size: 16, bold: true, text }
        if bounds.color == Color(0xaabbccff) && text == "日本語 é →"));
    // Column + Canvas headers place the item at 38. External inputs must be
    // checked even when they bypass the SDK's validation.
    for (offset, value) in [(38usize, 2u32), (62, 0), (62, 97), (66, 2), (70, u32::MAX)] {
        let mut bytes = original.clone();
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(decode_view(&bytes).is_err(), "offset={offset} value={value}");
    }
    for bad in [0u8, b'\n', b'\r', b'\t', 0x7f, 0xff] {
        let mut bytes = canvas_scene(&[label("a")]);
        bytes[74] = bad;
        assert!(decode_view(&bytes).is_err(), "text byte={bad}");
    }
    for bad in ["\u{0085}", "\u{2028}", "\u{2029}"] {
        let mut bytes = canvas_scene(&[label(&"a".repeat(bad.len()))]);
        bytes[74..74 + bad.len()].copy_from_slice(bad.as_bytes());
        assert!(decode_view(&bytes).is_err());
    }
    for length in 0..original.len() {
        assert!(decode_view(&original[..length]).is_err(), "truncation={length}");
    }
    assert!(decode_view(&canvas_scene(&[label("")])).is_ok());
    assert!(decode_view(&canvas_scene(&[label(&"é".repeat(2048))])).is_ok());
}

#[test]
fn canvas_limits_are_shared_across_controls_and_failed_encoding_preserves_the_scene() {
    use playbit_api::ui::{MAX_CANVAS_ITEMS, MAX_CANVAS_TEXT_BYTES};
    // An SDK refusal must not consume bytes, items, or text credit.
    let mut encoder_state = playbit_api::context::ContextState::default();
    let mut encoder = encoder_state.view();
    encoder.column_start();
    let long = "é".repeat(2049);
    for invalid in [label(&long), label("two\nlines"), label("\u{2028}")] {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                encoder.canvas(99, 320, 240, CanvasFrames::Static, &[invalid]);
            }))
            .is_err()
        );
    }
    let text = "é".repeat(2048);
    let half = vec![label(&text); MAX_CANVAS_TEXT_BYTES / 4096 / 2];
    encoder.canvas(1, 320, 240, CanvasFrames::Static, &half);
    encoder.canvas(2, 320, 240, CanvasFrames::Static, &half);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encoder.canvas(3, 320, 240, CanvasFrames::Static, &[label("a")]);
        }))
        .is_err()
    );
    encoder.canvas(4, 320, 240, CanvasFrames::Static, &[label("")]);
    encoder.column_end();
    let mut complete = encoder.finish();
    assert!(decode_view(&complete).is_ok());
    // Add one byte to the final empty label, bypassing the SDK. Scene-wide
    // text accounting must reject it even though each control fits its limit.
    let third = 13 + 2 * (9 + 16 + half.len() * (36 + 4096));
    complete[third + 5..third + 9].copy_from_slice(&53u32.to_le_bytes());
    complete[third + 9 + 48..third + 9 + 52].copy_from_slice(&1u32.to_le_bytes());
    complete.insert(third + 9 + 52, b'a');
    assert!(decode_view(&complete).is_err());

    let rect = CanvasItem::Rect(CanvasRect { x: 0, y: 0, width: 1, height: 1, color: Color(0) });
    let mut encoder_state = playbit_api::context::ContextState::default();
    let mut encoder = encoder_state.view();
    encoder.column_start();
    encoder.canvas(1, 320, 240, CanvasFrames::Static, &vec![rect; MAX_CANVAS_ITEMS / 2]);
    encoder.canvas(2, 320, 240, CanvasFrames::Static, &vec![rect; MAX_CANVAS_ITEMS / 2]);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encoder.canvas(3, 320, 240, CanvasFrames::Static, &[rect]);
        }))
        .is_err()
    );
    encoder.column_end();
    let complete = encoder.finish();
    assert!(decode_view(&complete).is_ok());
    let extra = canvas_scene(&[rect]);
    let mut over = complete[..complete.len() - 9].to_vec();
    over.extend_from_slice(&extra[13..]);
    assert!(decode_view(&over).is_err());
}
