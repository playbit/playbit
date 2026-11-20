#![cfg(target_arch = "wasm32")]

use playbit_api::abi::syscall::EventKind;
use playbit_api::context::Context;
use playbit_api::ui::{
    ButtonStyle, CanvasRect, CheckState, Color, MenuIcon, MenuItem, Segment, TextStyle, ViewEncoder,
};

const MAX_TEXT_BYTES: usize = 4096;
const REGIONS: [&str; 7] =
    ["Asia", "Africa", "North America", "South America", "Antarctica", "Europe", "Oceania"];

struct Gallery {
    last_action: &'static str,
    activations: u64,
    checks: [CheckState; 9],
    fields: [String; 6],
    selected_region: usize,
    selected_speed: u32,
    selected_animal: u32,
    error: &'static str,
}

impl Default for Gallery {
    fn default() -> Self {
        Self {
            last_action: "No control activated",
            activations: 0,
            checks: [
                CheckState::Off,
                CheckState::Off,
                CheckState::Off,
                CheckState::On,
                CheckState::On,
                CheckState::On,
                CheckState::Mixed,
                CheckState::Mixed,
                CheckState::Mixed,
            ],
            fields: [
                String::new(),
                String::new(),
                "Typing".into(),
                "Value entered".into(),
                "Disabled".into(),
                "Read only".into(),
            ],
            selected_region: 2,
            selected_speed: 122,
            selected_animal: 133,
            error: "",
        }
    }
}

#[allow(unsafe_code, reason = "The SDK resolves this app registration by its ABI name.")]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(Gallery::default(), turn, true)
}

fn turn(state: &mut Gallery, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    let mut changed = false;
    for event in context.raw_events() {
        let target = event.target;
        match event.kind {
            EventKind::Click => {
                state.last_action = match target {
                    1..=3 => "Regular button activated",
                    5..=7 => "Prominent button activated",
                    40..=42 => "Destructive button activated",
                    90..=96 => {
                        state.selected_region = (target - 90) as usize;
                        "Pop-up selection changed"
                    }
                    121..=123 => {
                        state.selected_speed = target;
                        "Speed changed"
                    }
                    131 | 133..=135 => {
                        state.selected_animal = target;
                        "Animal speed changed"
                    }
                    110 => "Context target activated",
                    111 => "Context menu: Inspect",
                    112 => "Context menu: Copy",
                    114 => "Context menu: Nested action",
                    10..=21 if target % 4 != 1 => "Checkbox changed",
                    _ => panic!("unexpected control {target}"),
                };
                state.activations += 1;
            }
            EventKind::TextChanged | EventKind::Submit => {
                let value = if event.kind == EventKind::TextChanged {
                    playbit_api::abi::text::TextInput::decode(event.data).unwrap().text
                } else {
                    std::str::from_utf8(event.data).unwrap()
                };
                if value.len() > MAX_TEXT_BYTES {
                    state.error = "Use at most 4,096 UTF-8 bytes in each field.";
                } else {
                    state.error = "";
                    if event.kind == EventKind::Submit {
                        state.last_action = "Field submitted";
                        state.activations += 1;
                    }
                }
            }
            _ => {}
        }
        changed = true;
    }
    if changed {
        encode(view, state);
        view.submit(context).unwrap();
    }
}

fn encode(view: &mut ViewEncoder, state: &mut Gallery) {
    view.scroll_start(160, true);
    view.column_start();
    view.text(TextStyle::Muted, "Text field");
    view.row_start();
    view.text_field(30, &mut state.fields[0], "Idle", false, false, false);
    view.text_field(31, &mut state.fields[1], "Focused", false, false, false);
    view.text_field(34, &mut state.fields[2], "Typing", false, false, false);
    view.row_end();
    view.row_start();
    view.text_field(35, &mut state.fields[3], "", false, false, false);
    view.text_field(32, &mut state.fields[4], "", false, true, false);
    view.text_field(33, &mut state.fields[5], "", false, false, true);
    view.row_end();
    if !state.error.is_empty() {
        view.text(TextStyle::Muted, state.error);
    }
    view.text(TextStyle::Muted, "Label");
    view.row_start();
    view.text(TextStyle::Label, "Small");
    view.text(TextStyle::SectionTitle, "Medium");
    view.text(TextStyle::LabelLarge, "Large");
    view.row_end();
    view.text(TextStyle::Muted, "Regular button");
    for (start, style) in [(1, ButtonStyle::Default), (5, ButtonStyle::Primary)] {
        view.row_start();
        for (offset, label) in ["Idle", "Focused", "Pressed", "Disabled"].iter().enumerate() {
            view.button(start + offset as u32, label, style, offset == 3);
        }
        view.row_end();
    }
    view.row_start();
    for (offset, label) in ["Idle", "Focused", "Pressed", "Disabled"].iter().enumerate() {
        view.button(40 + offset as u32, label, ButtonStyle::Destructive, offset == 3);
    }
    view.row_end();
    view.text(TextStyle::Muted, "Pop-up button");
    let regions: [_; REGIONS.len()] = std::array::from_fn(|index| MenuItem::Action {
        icon: None,
        id: 90 + index as u32,
        label: REGIONS[index],
        checked: Some(state.selected_region == index),
        disabled: false,
        destructive: false,
    });
    view.row_start();
    view.popup_button(89, REGIONS[state.selected_region], &regions, false);
    view.popup_button(
        99,
        "Disabled",
        &[MenuItem::Action {
            icon: None,
            id: 98,
            label: "Unavailable",
            checked: None,
            disabled: true,
            destructive: false,
        }],
        true,
    );
    view.context_menu_start(&[
        MenuItem::Action {
            icon: None,
            id: 111,
            label: "Inspect",
            checked: None,
            disabled: false,
            destructive: false,
        },
        MenuItem::Action {
            icon: Some(MenuIcon::Copy),
            id: 112,
            label: "Copy",
            checked: None,
            disabled: false,
            destructive: false,
        },
        MenuItem::Submenu {
            icon: None,
            label: "More",
            items: &[
                MenuItem::Action {
                    icon: Some(MenuIcon::OpenBook),
                    id: 114,
                    label: "Nested action",
                    checked: Some(state.last_action == "Context menu: Nested action"),
                    disabled: false,
                    destructive: false,
                },
                MenuItem::Action {
                    icon: None,
                    id: 115,
                    label: "Unavailable",
                    checked: Some(false),
                    disabled: true,
                    destructive: false,
                },
            ],
            disabled: false,
        },
        MenuItem::Separator,
        MenuItem::Action {
            icon: None,
            id: 113,
            label: "Unavailable",
            checked: None,
            disabled: true,
            destructive: false,
        },
    ]);
    view.button(110, "Right-click", ButtonStyle::Default, false);
    view.context_menu_end();
    view.row_end();
    view.text(TextStyle::Muted, "Checkbox");
    for row in 0..3 {
        view.row_start();
        for column in 0..4 {
            let id = 10 + row * 4 + column;
            let mut disabled_state =
                [CheckState::Off, CheckState::On, CheckState::Mixed][row as usize];
            let check = if column == 3 {
                &mut disabled_state
            } else {
                &mut state.checks[(row * 3 + column) as usize]
            };
            view.checkbox(id, "Label", check, column == 3);
        }
        view.row_end();
    }
    view.text(TextStyle::Muted, "Segmented control");
    view.text(TextStyle::Label, "Tab to focus; use ← and → to change selection");
    view.segmented_control(
        120,
        &[
            Segment { id: 121, label: "Slow", disabled: false },
            Segment { id: 122, label: "Medium", disabled: false },
            Segment { id: 123, label: "Fast", disabled: false },
        ],
        state.selected_speed,
        false,
    );
    view.segmented_control(
        130,
        &[
            Segment { id: 131, label: "Turtoise", disabled: false },
            Segment { id: 132, label: "Slow", disabled: true },
            Segment { id: 133, label: "Medium", disabled: false },
            Segment { id: 134, label: "Fast", disabled: false },
            Segment { id: 135, label: "Rabbit", disabled: false },
        ],
        state.selected_animal,
        false,
    );
    view.segmented_control(
        140,
        &[
            Segment { id: 141, label: "Slow", disabled: false },
            Segment { id: 142, label: "Medium", disabled: false },
            Segment { id: 143, label: "Fast", disabled: false },
        ],
        142,
        true,
    );
    view.text(TextStyle::Muted, "Photo");
    view.jpeg_image(
        161,
        512,
        341,
        include_bytes!("../assets/scroll-photo.jpg"),
        "Passenger beside a car window; scroll horizontally and vertically",
    );
    view.text(TextStyle::Muted, "Display P3 gamut");
    view.canvas_static(
        88,
        16,
        &[
            CanvasRect { x: 0, y: 0, width: 16, height: 16, color: Color(0xff0000ff) },
            CanvasRect { x: 24, y: 0, width: 16, height: 16, color: Color(0x00ff00ff) },
            CanvasRect { x: 48, y: 0, width: 16, height: 16, color: Color(0xffff00ff) },
            CanvasRect { x: 72, y: 0, width: 16, height: 16, color: Color(0xff00ffff) },
        ],
    );
    view.text(TextStyle::Muted, &format!("{} · {} actions", state.last_action, state.activations));
    view.column_end();
    view.scroll_end()
}
