//! Turn-local element responses borrow their encoder; queries never consume input.
use super::{MenuItem, Segment, ViewEncoder};
use crate::abi::text::TextEdits;
use crate::abi::text::TextInput;
use crate::{
    abi::syscall::{EventKind, FrameInput, KeyInput, MAX_QUEUED_EVENTS, PointerInput},
    context::EventBuffer,
};

pub struct Button<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) id: u32,
}

impl Button<'_, '_> {
    pub fn was_clicked(&self) -> bool {
        self.encoder.events.click_indices(self.id).next().is_some()
    }

    pub fn click_count(&self) -> usize {
        self.encoder.events.click_indices(self.id).count()
    }
}

pub struct Checkbox<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) id: u32,
}

impl Checkbox<'_, '_> {
    pub fn was_clicked(&self) -> bool {
        self.encoder.events.click_indices(self.id).next().is_some()
    }

    pub fn click_count(&self) -> usize {
        self.encoder.events.click_indices(self.id).count()
    }
}

pub struct TextField<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextEvent<'a> {
    /// Complete committed text and exact native mutations in application order.
    /// Coordinates are UTF-8 bytes. `None` means the platform supplied only a
    /// snapshot; `Some` with no edits is a retry with no new native mutation.
    Changed {
        text: &'a str,
        edits: Option<TextEdits<'a>>,
    },
    Submitted(&'a str),
}

impl TextField<'_, '_> {
    /// Every edit and submission in delivery order, including repeated and empty values.
    pub fn events(&self) -> impl DoubleEndedIterator<Item = TextEvent<'_>> + '_ {
        text_events(self.encoder.events, self.id)
    }

    /// The last committed edit in this turn, including an empty value.
    pub fn changed_text(&self) -> Option<&str> {
        text(self.encoder.events, self.id, EventKind::TextChanged)
    }

    /// The last submitted value in this turn. `events()` retains earlier submissions.
    pub fn submitted_text(&self) -> Option<&str> {
        text(self.encoder.events, self.id, EventKind::Submit)
    }

    pub fn was_changed(&self) -> bool {
        self.changed_text().is_some()
    }

    pub fn was_submitted(&self) -> bool {
        self.submitted_text().is_some()
    }
}

pub struct AutocompleteField<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) id: u32,
}

impl AutocompleteField<'_, '_> {
    /// Every edit and submission in delivery order, including repeated and empty values.
    pub fn events(&self) -> impl DoubleEndedIterator<Item = TextEvent<'_>> + '_ {
        text_events(self.encoder.events, self.id)
    }

    pub fn changed_text(&self) -> Option<&str> {
        text(self.encoder.events, self.id, EventKind::TextChanged)
    }

    pub fn submitted_text(&self) -> Option<&str> {
        text(self.encoder.events, self.id, EventKind::Submit)
    }

    pub fn was_changed(&self) -> bool {
        self.changed_text().is_some()
    }

    pub fn was_submitted(&self) -> bool {
        self.submitted_text().is_some()
    }
}

fn text(events: &EventBuffer, id: u32, kind: EventKind) -> Option<&str> {
    events.for_target(id).rev().find(|event| event.kind == kind).map(|event| match kind {
        EventKind::TextChanged => TextInput::decode(event.data).expect("validated edit").text,
        _ => core::str::from_utf8(event.data).expect("validated submission"),
    })
}

pub(super) fn text_events(
    events: &EventBuffer,
    id: u32,
) -> impl DoubleEndedIterator<Item = TextEvent<'_>> {
    events.for_target(id).filter_map(|event| match event.kind {
        EventKind::TextChanged => {
            let input = TextInput::decode(event.data).expect("the event prepass validated edits");
            Some(TextEvent::Changed { text: input.text, edits: input.edits })
        }
        EventKind::Submit => Some(TextEvent::Submitted(
            core::str::from_utf8(event.data).expect("the event prepass validated text"),
        )),
        _ => None,
    })
}

pub struct SegmentedControl<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) segments: &'view [Segment<'view>],
}

impl SegmentedControl<'_, '_> {
    /// Every activated segment ID in delivery order, including repeated activations.
    pub fn clicked_segments(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        let mut selected = [false; MAX_QUEUED_EVENTS];
        for segment in self.segments {
            for index in self.encoder.events.click_indices(segment.id) {
                selected[index] = true;
            }
        }
        self.encoder.events.selected_clicks(selected)
    }

    pub fn was_clicked(&self) -> bool {
        self.clicked_segments().next().is_some()
    }
}

pub struct PopupButton<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) items: &'view [MenuItem<'view>],
}

impl PopupButton<'_, '_> {
    /// Every activated action ID in delivery order, including submenus and repeated clicks.
    pub fn clicked_items(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        menu_clicks(self.encoder.events, self.items)
    }

    pub fn was_clicked(&self) -> bool {
        self.clicked_items().next().is_some()
    }
}

pub struct ContextMenu<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) items: &'view [MenuItem<'view>],
}

impl ContextMenu<'_, '_> {
    /// Every activated action ID in delivery order, including submenus and repeated clicks.
    pub fn clicked_items(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        menu_clicks(self.encoder.events, self.items)
    }

    pub fn was_clicked(&self) -> bool {
        self.clicked_items().next().is_some()
    }
}

fn menu_clicks<'a>(
    events: &'a EventBuffer,
    items: &[MenuItem<'_>],
) -> impl DoubleEndedIterator<Item = u32> + 'a {
    // Select each action once so delivery-order iteration does not rescan the menu per click.
    let mut selected = [false; MAX_QUEUED_EVENTS];
    select_menu_clicks(events, items, &mut selected, 0);
    events.selected_clicks(selected)
}

fn select_menu_clicks(
    events: &EventBuffer,
    items: &[MenuItem<'_>],
    selected: &mut [bool; MAX_QUEUED_EVENTS],
    depth: usize,
) {
    assert!(depth < super::MAX_MENU_DEPTH, "encoded menus have bounded depth");
    for item in items {
        match item {
            MenuItem::Action { id, .. } => {
                for index in events.click_indices(*id) {
                    selected[index] = true;
                }
            }
            MenuItem::Submenu { items, .. } => {
                select_menu_clicks(events, items, selected, depth + 1)
            }
            MenuItem::Separator => {}
        }
    }
}

pub struct Canvas<'view, 'events> {
    pub(super) encoder: &'view ViewEncoder<'events>,
    pub(super) id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanvasEvent {
    Pointer(PointerInput),
    Key(KeyInput),
    Frame(FrameInput),
}

impl Canvas<'_, '_> {
    /// Pointer, key and frame input in delivery order, including repeated keys and frames.
    pub fn events(&self) -> impl DoubleEndedIterator<Item = CanvasEvent> + '_ {
        self.encoder.events.for_target(self.id).filter_map(|event| match event.kind {
            EventKind::Pointer => Some(CanvasEvent::Pointer(
                PointerInput::decode(event.data)
                    .expect("the event prepass validated pointer input"),
            )),
            EventKind::Key => Some(CanvasEvent::Key(
                KeyInput::decode(event.data).expect("the event prepass validated key input"),
            )),
            EventKind::Frame => Some(CanvasEvent::Frame(
                FrameInput::decode(event.data).expect("the event prepass validated frame input"),
            )),
            _ => None,
        })
    }

    /// Elapsed display time for each frame delivered this turn.
    pub fn frames(&self) -> impl DoubleEndedIterator<Item = FrameInput> + '_ {
        self.encoder
            .events
            .for_target(self.id)
            .filter(|event| event.kind == EventKind::Frame)
            .map(|event| FrameInput::decode(event.data).expect("validated frame input"))
    }
}
