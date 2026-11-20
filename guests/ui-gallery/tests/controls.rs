use playbit::engine::Engine;
#[path = "../../../playbit/tests/support/wasm_owner.rs"]
mod wasm_owner;
use playbit::package::Package;
use playbit::runtime::{
    CanvasItem, ViewNode, decode_view,
    syscall::{Event, EventKind},
};
use playbit_test_fixtures::Guest;
use std::{collections::BTreeMap, time::Instant};
use wasm_owner::WasmOwner;

fn fixture() -> (tempfile::TempDir, WasmOwner) {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root = engine.services.store.create_volume("Gallery", false).unwrap();
    let package = Package::new(
        "ui-gallery".into(),
        "UI Gallery".into(),
        Guest::UiGallery.wasm().to_vec(),
        BTreeMap::new(),
    )
    .unwrap();
    let package =
        playbit::package::signature::testing::signed(&engine.services.store, package).unwrap();
    let session = engine.create_session(package, root.id).unwrap();
    let owner = WasmOwner::open_with_role(&engine, &session.id, 0);
    assert_eq!(owner.session, session);
    (directory, owner)
}

fn leaves(view: &ViewNode) -> Vec<&ViewNode> {
    match view {
        ViewNode::Theme { child, .. }
        | ViewNode::ContextMenu { child, .. }
        | ViewNode::Scroll { child, .. } => leaves(child),
        ViewNode::Column(children) | ViewNode::Row(children) => {
            children.iter().flat_map(leaves).collect()
        }
        _ => vec![view],
    }
}

fn field(owner: &mut WasmOwner, target: u32) -> String {
    let view = decode_view(owner.scene()).unwrap();
    leaves(&view)
        .into_iter()
        .find_map(|engine| match engine {
            ViewNode::TextField { id, value, .. } if *id == target => Some(value.clone()),
            _ => None,
        })
        .unwrap()
}

fn input(owner: &mut WasmOwner, kind: EventKind, target: u32, text: &str) {
    let data = if kind == EventKind::TextChanged {
        playbit_api::abi::text::TextInput::encode(text, &[]).expect("fixture edit")
    } else {
        text.as_bytes().to_vec()
    };
    owner.input(Event { kind, target, data }).unwrap();
}

#[test]
fn gallery_renders_review_one_matrix_and_enabled_checks_toggle_without_mixed() {
    let (_directory, mut owner) = fixture();
    let initial = decode_view(owner.scene()).unwrap();
    for control in leaves(&initial) {
        match control {
            ViewNode::Text { .. }
            | ViewNode::Button { .. }
            | ViewNode::PopupButton { .. }
            | ViewNode::SegmentedControl { .. } => {}
            ViewNode::Canvas { id, width, height, frames, items, .. } => {
                assert_eq!((*id, *width, *height), (0, 88, 16));
                assert_eq!(*frames, playbit_api::ui::CanvasFrames::Static);
                assert_eq!(
                    items
                        .iter()
                        .map(|item| match item {
                            CanvasItem::Rect(rect) => rect.color.0,
                            CanvasItem::Text { .. } | CanvasItem::Pixels { .. } => {
                                panic!("gallery swatches are rectangles")
                            }
                        })
                        .collect::<Vec<_>>(),
                    [0xff0000ff, 0x00ff00ff, 0xffff00ff, 0xff00ffff]
                );
            }
            ViewNode::TextField { multiline, .. } => assert!(!multiline),
            ViewNode::Checkbox { id, state, disabled, .. } => {
                let event = Event { kind: EventKind::Click, target: *id, data: vec![] };
                if *disabled {
                    assert!(owner.input(event).is_err());
                } else {
                    owner.input(event).unwrap();
                    let next = decode_view(owner.scene()).unwrap();
                    let after = leaves(&next)
                        .into_iter()
                        .find_map(|control| match control {
                            ViewNode::Checkbox { id: next_id, state, .. } if id == next_id => {
                                Some(*state)
                            }
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(
                        after,
                        match state {
                            playbit_api::ui::CheckState::Off
                            | playbit_api::ui::CheckState::Mixed => playbit_api::ui::CheckState::On,
                            playbit_api::ui::CheckState::On => playbit_api::ui::CheckState::Off,
                        },
                        "enabled checkbox {id} must toggle to a binary state"
                    );
                    input(&mut owner, EventKind::Click, *id, "");
                    let next = decode_view(owner.scene()).unwrap();
                    let again = leaves(&next)
                        .into_iter()
                        .find_map(|control| match control {
                            ViewNode::Checkbox { id: next_id, state, .. } if id == next_id => {
                                Some(*state)
                            }
                            _ => None,
                        })
                        .unwrap();
                    assert_ne!(again, after);
                    assert_ne!(again, playbit_api::ui::CheckState::Mixed);
                }
            }
            ViewNode::JpegImage { .. } => {}
            other => panic!("deferred gallery control: {other:?}"),
        }
    }
    owner.quit();
}

#[test]
fn edits_survive_other_controls_and_oversized_text_is_visible_and_recoverable() {
    let (_directory, mut owner) = fixture();
    input(&mut owner, EventKind::TextChanged, 30, "Unsaved 🦀 draft");
    input(&mut owner, EventKind::TextChanged, 31, "Another field");
    input(&mut owner, EventKind::Click, 1, "");
    input(&mut owner, EventKind::Click, 10, "");
    assert_eq!(field(&mut owner, 30), "Unsaved 🦀 draft");
    assert_eq!(field(&mut owner, 31), "Another field");
    let maximum = "🦀".repeat(1024);
    input(&mut owner, EventKind::TextChanged, 30, &maximum);
    assert_eq!(field(&mut owner, 30), maximum);
    let oversized = "🦀".repeat(1025);
    input(&mut owner, EventKind::TextChanged, 30, &oversized);
    assert_eq!(field(&mut owner, 30), oversized);
    let view = owner.scene().to_vec();
    assert!(view.windows(b"4,096".len()).any(|bytes| bytes == b"4,096"));
    input(&mut owner, EventKind::Submit, 30, "Recovered");
    assert_eq!(field(&mut owner, 30), "Recovered");
    for (kind, target, text) in [
        (EventKind::Click, 4, ""),
        (EventKind::TextChanged, 32, "disabled"),
        (EventKind::TextChanged, 33, "read only"),
        (EventKind::Submit, 33, "read only"),
    ] {
        assert!(owner.input(Event { kind, target, data: text.as_bytes().to_vec() }).is_err());
    }
    owner.quit();
}

#[test]
fn buttons_report_each_activation_and_disabled_variants_reject_input() {
    let (_directory, mut owner) = fixture();
    for (index, feedback) in [(1, "Regular"), (5, "Prominent"), (40, "Destructive")] {
        input(&mut owner, EventKind::Click, index, "");
        let view = decode_view(owner.scene()).unwrap();
        let expected = format!(
            "{feedback} button activated · {} actions",
            if index == 1 {
                1
            } else if index == 5 {
                2
            } else {
                3
            }
        );
        assert!(
            leaves(&view)
                .iter()
                .any(|node| matches!(node, ViewNode::Text { text, .. } if text == &expected))
        );
    }
    for id in [4, 8, 43] {
        assert!(owner.input(Event { kind: EventKind::Click, target: id, data: vec![] }).is_err());
    }
    owner.quit();
}

#[test]
fn popup_and_context_actions_use_menu_targets_and_reject_disabled_items() {
    let (_directory, mut owner) = fixture();
    let initial = decode_view(owner.scene()).unwrap();
    assert!(leaves(&initial).iter().any(|node| matches!(node,
        ViewNode::PopupButton { id: 89, label, .. } if label == "North America")));
    fn nested_menu(view: &ViewNode) -> bool {
        match view {
            ViewNode::ContextMenu { items, .. } => items.iter().any(|entry| {
                matches!(entry,
                playbit::runtime::MenuEntry::Submenu { label, items, disabled: false, .. }
                    if label == "More" && items.len() == 2)
            }),
            ViewNode::Theme { child, .. } | ViewNode::Scroll { child, .. } => nested_menu(child),
            ViewNode::Column(children) | ViewNode::Row(children) => {
                children.iter().any(nested_menu)
            }
            _ => false,
        }
    }
    assert!(nested_menu(&initial));
    input(&mut owner, EventKind::Click, 90, "");
    let selected = decode_view(owner.scene()).unwrap();
    assert!(leaves(&selected).iter().any(|node| matches!(node,
        ViewNode::PopupButton { id: 89, label, .. } if label == "Asia")));
    input(&mut owner, EventKind::Click, 111, "");
    input(&mut owner, EventKind::Click, 114, "");
    for id in [89, 98, 99, 113, 115] {
        assert!(
            owner.input(Event { kind: EventKind::Click, target: id, data: vec![] }).is_err(),
            "disabled menu or non-action target {id} should reject Click"
        );
    }
    owner.quit();
}

#[test]
#[ignore = "diagnostic: local guest through CPU scene observation; excludes packaged UI/display"]
fn benchmark_basic_controls() {
    let (_directory, mut owner) = fixture();
    owner.scene();
    for bytes in [32, 4096] {
        let text = "x".repeat(bytes);
        let mut samples = Vec::with_capacity(1000);
        let mut maximum_scene = 0;
        for index in 0..1000 {
            let event = if index % 2 == 0 {
                Event {
                    kind: EventKind::TextChanged,
                    target: 30,
                    data: playbit_api::abi::text::TextInput::encode(&text, &[])
                        .expect("fixture edit"),
                }
            } else {
                Event { kind: EventKind::Click, target: 10, data: vec![] }
            };
            let start = Instant::now();
            owner.input(event).unwrap();
            let view = owner.scene();
            samples.push(start.elapsed().as_nanos());
            maximum_scene = maximum_scene.max(view.len());
        }
        samples.sort_unstable();
        println!(
            "gallery field_bytes={bytes} samples=1000 p50_ns={} p90_ns={} p95_ns={} p99_ns={} max_ns={} max_scene_bytes={maximum_scene}",
            samples[499], samples[899], samples[949], samples[989], samples[999]
        );
        assert!(maximum_scene <= 128 * 1024);
    }
    owner.quit();
}

#[test]
fn segmented_selection_roundtrips_and_disabled_inputs_are_rejected() {
    let (_directory, mut owner) = fixture();
    for (target, group) in [(123, 120), (121, 120), (134, 130), (131, 130)] {
        input(&mut owner, EventKind::Click, target, "");
        let view = decode_view(owner.scene()).unwrap();
        let selected = leaves(&view).into_iter().find_map(|node| match node {
            ViewNode::SegmentedControl { id, selected, .. } if *id == group => Some(*selected),
            _ => None,
        });
        assert_eq!(selected, Some(target));
    }
    for target in [120, 130, 132, 140, 141, 142, 143] {
        assert!(owner.input(Event { kind: EventKind::Click, target, data: vec![] }).is_err());
    }
    owner.quit();
}

#[test]
fn gallery_contains_a_real_photo_inside_a_scroll_view() {
    let (_directory, mut owner) = fixture();
    let initial = decode_view(owner.scene()).unwrap();
    let ViewNode::Theme { child, .. } = initial else { panic!() };
    let ViewNode::Scroll { id: 160, complex_background: true, child } = *child else { panic!() };
    let ViewNode::Column(children) = *child else { panic!() };
    let photo = children
        .iter()
        .find(|node| matches!(node, ViewNode::JpegImage { id: 161, .. }))
        .expect("photo in the gallery viewport");
    let ViewNode::JpegImage { id: 161, width: 512, height: 341, jpeg, label } = photo else {
        panic!()
    };
    assert_eq!(jpeg, include_bytes!("../assets/scroll-photo.jpg"));
    assert!(!label.is_empty());
    owner.quit();
}
