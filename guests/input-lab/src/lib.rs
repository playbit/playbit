//! A live inspector for the five window input families.
#[cfg(not(target_arch = "wasm32"))]
extern crate playbit;

mod pad;
mod readout;

use playbit_api::abi::{
    input::{self, Input, Source},
    syscall::EventKind,
};
use playbit_api::context::{Context, Surface};
use playbit_api::ui::{ButtonStyle, MenuItem, Segment, TextStyle, ViewEncoder};
use std::collections::VecDeque;

const PAD: u32 = 1;
const TABS: u32 = 10;
const LISTEN: u32 = 20;
const FREEZE: u32 = 21;
const OLDER: u32 = 22;
const NEWER: u32 = 23;
const HISTORY: usize = 16;

struct Family {
    name: &'static str,
    mask: u32,
    hint: &'static str,
}

const FAMILIES: [Family; 5] = [
    Family {
        name: "Pointer",
        mask: input::POINTER,
        hint: "Move, drag, touch or draw on the pad. Try dragging past its edge.",
    },
    Family {
        name: "Keyboard",
        mask: input::KEYBOARD,
        hint: "Press a chord or hold a key to see repeats. Try typing a Unicode character.",
    },
    Family {
        name: "Scroll",
        mask: input::SCROLL,
        hint: "Use a wheel or scroll with two fingers. Try both axes and a momentum scroll.",
    },
    Family {
        name: "Gesture",
        mask: input::GESTURE,
        hint: "Pan, pinch or rotate with two fingers on a supported surface.",
    },
    Family {
        name: "Gamepad",
        mask: input::GAMEPAD,
        hint: "Connect a controller, then move a stick, press a button or squeeze a trigger.",
    },
];

#[derive(Clone, Copy)]
struct Sample {
    window: u32,
    target: u32,
    source: Source,
    input: Input,
}

#[derive(Clone, Default)]
struct Channel {
    events: VecDeque<Sample>,
    count: u64,
}

impl Channel {
    fn push(&mut self, sample: Sample) {
        if self.events.len() == HISTORY {
            self.events.pop_back();
        }
        self.events.push_front(sample);
        self.count = self.count.saturating_add(1);
    }
}

struct Lab {
    selected: usize,
    subscriptions: u32,
    channels: [Channel; 5],
    frozen: Option<[Channel; 5]>,
    offset: usize,
    surface_width: u32,
}

impl Default for Lab {
    fn default() -> Self {
        Self {
            selected: 0,
            subscriptions: input::ALL,
            channels: Default::default(),
            frozen: None,
            offset: 0,
            surface_width: 400,
        }
    }
}

impl Lab {
    fn channel(&self) -> &Channel {
        &self.frozen.as_ref().unwrap_or(&self.channels)[self.selected]
    }

    fn click(&mut self, target: u32) {
        match target {
            11..=15 => {
                self.selected = (target - 11) as usize;
                self.offset = 0;
            }
            LISTEN => self.subscriptions ^= FAMILIES[self.selected].mask,
            FREEZE => {
                self.frozen =
                    if self.frozen.is_some() { None } else { Some(self.channels.clone()) };
                self.offset = 0;
            }
            OLDER if self.frozen.is_some() && self.offset + 1 < self.channel().events.len() => {
                self.offset += 1
            }
            NEWER if self.frozen.is_some() && self.offset > 0 => self.offset -= 1,
            _ => {}
        }
    }

    fn render(&self, view: &mut ViewEncoder<'_>) {
        let family = &FAMILIES[self.selected];
        let channel = self.channel();
        let frozen = self.frozen.is_some();
        let listening = self.subscriptions & family.mask != 0;
        let compact = self.surface_width < 360;

        view.window_input(self.subscriptions);
        view.column_start();
        if compact {
            let items = std::array::from_fn::<_, 5, _>(|i| MenuItem::Action {
                id: 11 + i as u32,
                label: FAMILIES[i].name,
                checked: Some(i == self.selected),
                icon: None,
                disabled: false,
                destructive: false,
            });
            view.popup_button(TABS, family.name, &items, false);
        } else {
            let tabs = std::array::from_fn::<_, 5, _>(|i| Segment {
                id: 11 + i as u32,
                label: FAMILIES[i].name,
                disabled: false,
            });
            view.segmented_control(TABS, &tabs, 11 + self.selected as u32, false);
        }
        view.text(TextStyle::Title, family.name);
        view.text(TextStyle::Muted, family.hint);
        let pad_width = self.surface_width.saturating_sub(40).clamp(160, 600);
        pad::draw(view, family.mask, pad_width, channel, self.offset);
        if !compact {
            view.row_start();
        }
        view.button(
            LISTEN,
            if listening { "Unsubscribe" } else { "Subscribe" },
            ButtonStyle::Default,
            false,
        );
        view.button(
            FREEZE,
            if frozen { "Resume live" } else { "Freeze readout" },
            ButtonStyle::Default,
            false,
        );
        if frozen {
            view.button(
                OLDER,
                "Older",
                ButtonStyle::Default,
                self.offset + 1 >= channel.events.len(),
            );
            view.button(NEWER, "Newer", ButtonStyle::Default, self.offset == 0);
        }
        if !compact {
            view.row_end();
        }
        let mode = if listening { "Subscribed" } else { "Unsubscribed" };
        let display = if frozen { "Frozen" } else { "Live" };
        view.text(TextStyle::Muted, &format!("{mode} · {display} · {} events", channel.count));
        if let Some(sample) = channel.events.get(self.offset) {
            view.text(TextStyle::SectionTitle, &sample.summary());
            sample.details(view);
            view.text(TextStyle::SectionTitle, "Recent events");
            for (i, sample) in channel.events.iter().enumerate().skip(self.offset).take(4) {
                view.text(
                    TextStyle::Muted,
                    &format!("#{}   {}", channel.count - i as u64, sample.summary()),
                );
            }
        } else {
            view.text(
                TextStyle::Body,
                &format!("Waiting for {} input", family.name.to_lowercase()),
            );
        }
        view.column_end();
    }
}

#[allow(unsafe_code, reason = "The SDK resolves the app registration by this ABI name.")]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(Lab::default(), turn, true)
}

fn turn(lab: &mut Lab, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    for event in context.raw_events() {
        match event.kind {
            EventKind::Click => lab.click(event.target),
            EventKind::SurfaceChanged => {
                if let Ok(surface) = Surface::decode(event.data) {
                    lab.surface_width = surface.width;
                }
            }
            kind => {
                if let Some(index) =
                    FAMILIES.iter().position(|family| family.mask == Input::family(kind))
                {
                    let (source, input) = event.input().expect("validated window input");
                    // A release can arrive after its family has been unsubscribed.
                    lab.channels[index].push(Sample {
                        source,
                        input,
                        target: event.target,
                        window: event.object_id,
                    });
                }
            }
        }
    }
    lab.render(view);
    view.submit(context).expect("submit input lab");
}
