//! The entry inspector has one bounded keyboard cycle, independent of paint order.
use super::*;
use gpui::{Focusable, KeyDownEvent};

#[derive(Clone, Copy)]
enum Stop {
    Key,
    Value,
    KeyEncoding,
    ValueEncoding,
    Discard,
    Save,
    Reload,
}

const STOPS: [Stop; 7] = [
    Stop::Key,
    Stop::Value,
    Stop::KeyEncoding,
    Stop::ValueEncoding,
    Stop::Discard,
    Stop::Save,
    Stop::Reload,
];

impl Browser {
    pub(in crate::desktop::browser) fn tab_kv_editor(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key != "tab"
            || modifiers.control
            || modifiers.alt
            || modifiers.platform
            || modifiers.function
            || self.overview
            || !self.kv_editor_open()
            || self.kv_is_loading()
        {
            return;
        }
        let Some(current) = STOPS.iter().position(|stop| self.kv_stop_focused(*stop, window, cx))
        else {
            return;
        };
        // Moving native focus would commit marked text before its owner accepts it.
        if !self.kv_is_composing(cx) {
            for distance in 1..=STOPS.len() {
                let index = if modifiers.shift {
                    (current + STOPS.len() - distance) % STOPS.len()
                } else {
                    (current + distance) % STOPS.len()
                };
                if self.kv_stop_enabled(STOPS[index], cx) {
                    self.focus_kv_stop(STOPS[index], window, cx);
                    break;
                }
            }
        }
        cx.stop_propagation();
    }

    fn kv_stop_focused(&self, stop: Stop, window: &Window, cx: &App) -> bool {
        match stop {
            Stop::Key => self.kv.key.read(cx).focus_handle(cx).is_focused(window),
            Stop::Value => self.kv.value.read(cx).focus_handle(cx).is_focused(window),
            Stop::KeyEncoding => {
                self.kv.key_representation.read(cx).focus_handle(cx).is_focused(window)
            }
            Stop::ValueEncoding => {
                self.kv.value_representation.read(cx).focus_handle(cx).is_focused(window)
            }
            Stop::Discard | Stop::Save | Stop::Reload => {
                let key = match stop {
                    Stop::Discard => "kv-discard",
                    Stop::Save => "kv-save",
                    _ => "kv-reload",
                };
                self.rendered_controls.contains(key)
                    && self
                        .controls
                        .get(key)
                        .is_some_and(|control| control.read(cx).focused(window))
            }
        }
    }

    fn kv_stop_enabled(&self, stop: Stop, cx: &Context<Self>) -> bool {
        match stop {
            Stop::Key | Stop::Value => true, // Read-only fields remain selectable and copyable.
            Stop::KeyEncoding => {
                self.kv.pending.is_none()
                    && self.kv.key_representation.read(cx).focus_handle(cx).tab_stop
            }
            Stop::ValueEncoding => {
                self.kv.pending.is_none()
                    && self.kv.value_representation.read(cx).focus_handle(cx).tab_stop
            }
            Stop::Discard => {
                self.rendered_controls.contains("kv-discard")
                    && !self
                        .kv
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.call.operation.is_mutation())
                    && !self
                        .kv
                        .draft_id
                        .is_some_and(|id| crate::desktop::draft_publications::pending(id, cx))
                    && (self.kv.creating || self.kv.dirty || self.kv.uncertain)
            }
            Stop::Save => self.rendered_controls.contains("kv-save") && self.kv_can_save(cx),
            Stop::Reload => {
                self.rendered_controls.contains("kv-reload")
                    && self.kv.read_unavailable
                    && self.kv.pending.is_none()
                    && !self.kv.uncertain
            }
        }
    }

    fn focus_kv_stop(&self, stop: Stop, window: &mut Window, cx: &mut Context<Self>) {
        match stop {
            Stop::Key | Stop::Value => {
                let editor = if matches!(stop, Stop::Key) { &self.kv.key } else { &self.kv.value };
                editor.update(cx, |editor, cx| editor.focus_from_tab(window, cx));
            }
            Stop::KeyEncoding | Stop::ValueEncoding => {
                let group = if matches!(stop, Stop::KeyEncoding) {
                    &self.kv.key_representation
                } else {
                    &self.kv.value_representation
                };
                group.update(cx, |group, cx| {
                    group.focus_handle(cx).focus(window);
                    group.show_keyboard_focus_if_focused(window, cx);
                });
            }
            Stop::Discard | Stop::Save | Stop::Reload => {
                let key = match stop {
                    Stop::Discard => "kv-discard",
                    Stop::Save => "kv-save",
                    _ => "kv-reload",
                };
                if let Some(control) = self.controls.get(key) {
                    control.update(cx, |control, cx| {
                        control.focus(window);
                        control.show_keyboard_focus_if_focused(window, cx);
                    });
                }
            }
        }
    }
}
