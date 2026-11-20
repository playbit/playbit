//! Context menus target the editor that opened them, never the current responder.
use super::*;
use crate::runtime::MenuEntry;
use crate::ui::desktop::{controls, popup};
use std::rc::Rc;

const CUT: u32 = 1;
const COPY: u32 = 2;
const PASTE: u32 = 3;

impl TextEditor {
    fn context_action_enabled(&self, id: u32, cx: &mut App) -> bool {
        if !self.enabled {
            return false;
        }
        let selected = self.buffer.selections().iter().any(|selection| !selection.is_empty());
        match id {
            CUT => !self.read_only && !self.masked && selected,
            COPY => !self.masked && selected,
            PASTE => {
                !self.read_only
                    && self.clipboard_text(cx).is_some_and(|text| {
                        !text.is_empty()
                            && self.input_text(&text).is_some_and(|text| !text.is_empty())
                    })
            }
            _ => false,
        }
    }

    fn context_selection(&mut self, hit: HitTest) {
        // AppKit preserves selection in trailing blank space and inside the
        // selection. Elsewhere it selects the word under the pointer.
        if let Some(offset) = hit.unit_offset
            && !self.buffer.selections().iter().any(|selection| selection.range().contains(&offset))
        {
            let range = self.buffer.word_range_at(offset);
            self.buffer.set_selection(Selection::new(range.start, range.end));
            self.primary_affinity = hit.affinity;
        }
        self.selection_drag = None;
        self.preferred_x = None;
        self.reset_caret_blink();
    }

    fn context_activation(&self, window: &Window, cx: &Context<Self>) -> controls::Activate {
        let target = cx.weak_entity();
        let owner = window.window_handle();
        let revision = self.revision.current;
        let model_generation = self.model_generation;
        let selections = self.buffer.selections().to_vec();
        Rc::new(move |id, cx| {
            let _ = owner.update(cx, |_, window, cx| {
                let _ = target.update(cx, |editor, cx| {
                    // Focus, content or selection may have changed while the menu
                    // was open (including a field being retired by its owner).
                    if !editor.focus_handle.is_focused(window)
                        || editor.revision.current != revision
                        || editor.model_generation != model_generation
                        || editor.buffer.selections() != selections
                        || !editor.context_action_enabled(id, cx)
                    {
                        return;
                    }
                    match id {
                        CUT => editor.cut(cx),
                        COPY => editor.copy(cx),
                        PASTE => editor.paste(cx),
                        _ => unreachable!("validated text menu action"),
                    }
                });
            });
        })
    }

    pub(super) fn open_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        self.focus_handle.focus(window);
        if let Some(layout) = &self.layout {
            self.context_selection(layout.hit_test(event.position));
        }
        let items = [(CUT, "Cut"), (COPY, "Copy"), (PASTE, "Paste")]
            .into_iter()
            .map(|(id, label)| MenuEntry::Action {
                icon: None,
                id,
                label: label.into(),
                checked: None,
                disabled: !self.context_action_enabled(id, cx),
                destructive: false,
            })
            .collect();
        let activate = self.context_activation(window, cx);
        let request = popup::Request::context(window, event.position, items, activate, cx);
        cx.defer(move |cx| popup::open(request, cx));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fields {
        first: Entity<TextEditor>,
        second: Entity<TextEditor>,
    }
    impl Render for Fields {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div().flex().flex_col().child(self.first.clone()).child(self.second.clone())
        }
    }

    #[gpui::test]
    fn context_menu_activation_stays_with_its_rendered_field(cx: &mut gpui::TestAppContext) {
        // Two rendered editors share one window; changing focus must never route
        // the retained menu's Cut to the new responder.
        let window = cx.add_window(|_, cx| Fields {
            first: cx.new(|cx| TextEditor::new("first", cx)),
            second: cx.new(|cx| TextEditor::new("second", cx)),
        });
        let activate = window
            .update(cx, |fields, window, cx| {
                fields.first.update(cx, |editor, cx| {
                    editor.focus_handle.focus(window);
                    editor.select_all(cx);
                    editor.context_activation(window, cx)
                })
            })
            .expect("fixture window exists");
        window
            .update(cx, |fields, window, cx| {
                fields.second.update(cx, |editor, cx| {
                    editor.focus_handle.focus(window);
                    editor.select_all(cx);
                });
            })
            .expect("fixture window exists");
        cx.update(|cx| activate(CUT, cx));
        let activate = window
            .update(cx, |fields, window, cx| {
                assert_eq!(fields.first.read(cx).text(), "first");
                assert_eq!(fields.second.read(cx).text(), "second");
                fields.first.update(cx, |editor, cx| {
                    editor.focus_handle.focus(window);
                    editor.select_all(cx);
                    editor.context_activation(window, cx)
                })
            })
            .expect("fixture window exists");
        cx.update(|cx| activate(COPY, cx));
        cx.update(|cx| {
            assert_eq!(
                cx.read_from_clipboard().expect("copied item").text().expect("text clipboard"),
                "first"
            )
        });
        cx.update(|cx| activate(CUT, cx));
        window
            .update(cx, |fields, _, cx| {
                assert_eq!(fields.first.read(cx).text(), "");
                assert_eq!(fields.second.read(cx).text(), "second");
            })
            .expect("fixture window exists");
    }

    #[gpui::test]
    fn context_menu_availability_respects_clipboard_and_field_policy(
        cx: &mut gpui::TestAppContext,
    ) {
        // Exercise the policy used both to display and to activate each item.
        let editor = cx.new(|cx| TextEditor::new("private", cx).bound_edits(8));
        editor.update(cx, |editor, cx| {
            assert!(!editor.context_action_enabled(CUT, cx));
            assert!(!editor.context_action_enabled(COPY, cx));
            assert!(!editor.context_action_enabled(PASTE, cx));
            cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
            assert!(!editor.context_action_enabled(PASTE, cx));
            cx.write_to_clipboard(ClipboardItem::new_string("x".into()));
            assert!(editor.context_action_enabled(PASTE, cx));
            assert!(!editor.context_action_enabled(CUT, cx));
            assert!(!editor.context_action_enabled(COPY, cx));
            cx.write_to_clipboard(ClipboardItem::new_string("replacement".into()));
            assert!(!editor.context_action_enabled(PASTE, cx));
            cx.write_to_clipboard(ClipboardItem::new_string("new".into()));
            editor.select_all(cx);
            for id in [CUT, COPY, PASTE] {
                assert!(editor.context_action_enabled(id, cx));
            }
            editor.masked = true;
            assert!(!editor.context_action_enabled(CUT, cx));
            assert!(!editor.context_action_enabled(COPY, cx));
            editor.cut(cx);
            editor.copy(cx);
            assert_eq!(editor.text(), "private");
            assert_eq!(
                cx.read_from_clipboard()
                    .expect("test fixture operation succeeds")
                    .text()
                    .expect("test fixture operation succeeds"),
                "new"
            );
            assert!(editor.context_action_enabled(PASTE, cx));
            editor.masked = false; // Intentionally copyable saved-key field.
            editor.read_only = true;
            assert!(editor.context_action_enabled(COPY, cx));
            assert!(!editor.context_action_enabled(CUT, cx));
            assert!(!editor.context_action_enabled(PASTE, cx));
            editor.enabled = false;
            for id in [CUT, COPY, PASTE] {
                assert!(!editor.context_action_enabled(id, cx));
            }
            editor.copy(cx);
            editor.paste(cx);
            assert_eq!(editor.text(), "private");
            assert_eq!(
                cx.read_from_clipboard()
                    .expect("test fixture operation succeeds")
                    .text()
                    .expect("test fixture operation succeeds"),
                "new"
            );
        });
    }

    #[gpui::test]
    fn context_menu_preserves_selection_or_selects_clicked_word(cx: &mut gpui::TestAppContext) {
        // Match the native AppKit probe, including reversed and multiple selections.
        let editor = cx.new(|cx| TextEditor::new("alpha bravo charlie", cx));
        editor.update(cx, |editor, _| {
            editor
                .buffer
                .set_selections_with_primary(Selection::new(11, 6), [Selection::new(0, 5)]);
            let before = editor.buffer.selections().to_vec();
            let hit = |unit_offset| HitTest {
                offset: 19,
                affinity: CaretAffinity::Downstream,
                unit_offset,
                after_text_on_row: unit_offset.is_none(),
            };
            editor.context_selection(hit(Some(8)));
            assert_eq!(editor.buffer.selections(), before);
            editor.context_selection(hit(None));
            assert_eq!(editor.buffer.selections(), before);
            editor.context_selection(hit(Some(14)));
            assert_eq!(editor.buffer.primary_selection().range(), 12..19);
            assert!(editor.selection_drag.is_none());
        });
    }

    #[gpui::test]
    fn context_menu_rejects_changed_focus_selection_content_and_permissions(
        cx: &mut gpui::TestAppContext,
    ) {
        // Retain the actual callback, then change its owner before activation.
        let window = cx.add_window(|_, cx| TextEditor::new("target", cx));
        for change in 0..5 {
            let activate = window
                .update(cx, |editor, window, cx| {
                    editor.enabled = true;
                    editor.read_only = false;
                    editor.focus_handle.focus(window);
                    editor.select_all(cx);
                    let activate = editor.context_activation(window, cx);
                    match change {
                        0 => window.focus(&cx.focus_handle()),
                        1 => editor.buffer.set_selection(Selection::caret(0)),
                        2 => editor.revision.changed(),
                        3 => editor.enabled = false,
                        4 => editor.read_only = true,
                        _ => unreachable!(),
                    }
                    activate
                })
                .expect("test fixture operation succeeds");
            cx.update(|cx| activate(CUT, cx));
            assert_eq!(
                window
                    .read_with(cx, |editor, _| editor.text().to_owned())
                    .expect("test fixture operation succeeds"),
                "target"
            );
        }
    }

    #[gpui::test]
    fn model_replacement_retires_menu_without_creating_a_local_edit(cx: &mut gpui::TestAppContext) {
        for revision in [None, Some(0)] {
            let window = cx.add_window(|_, cx| TextEditor::new("", cx));
            let activate = window
                .update(cx, |editor, window, cx| {
                    editor.focus_handle.focus(window);
                    cx.write_to_clipboard(ClipboardItem::new_string("pasted-".into()));
                    let activate = editor.context_activation(window, cx);
                    editor.acknowledge(revision, "remote-value", cx);
                    assert!(!editor.revision.pending());
                    assert_eq!(editor.revision.current, 0);
                    activate
                })
                .expect("fixture window exists");
            cx.update(|cx| activate(PASTE, cx));
            let activate = window
                .update(cx, |editor, window, cx| {
                    assert_eq!(editor.text(), "remote-value");
                    let activate = editor.context_activation(window, cx);
                    // An acknowledgment of the same value must not retire the menu.
                    editor.acknowledge(revision, "remote-value", cx);
                    activate
                })
                .expect("fixture window exists");
            cx.update(|cx| activate(PASTE, cx));
            window
                .update(cx, |editor, _, _| {
                    assert_eq!(editor.text(), "pasted-remote-value");
                })
                .expect("fixture window exists");
        }
    }

    #[gpui::test]
    fn text_clipboard_admission_rejects_images_and_oversized_payloads(
        cx: &mut gpui::TestAppContext,
    ) {
        let editor = cx.new(|cx| TextEditor::new("", cx));
        editor.update(cx, |editor, cx| {
            cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::empty()));
            assert!(!editor.context_action_enabled(PASTE, cx));
            editor.paste(cx);
            assert_eq!(editor.text(), "");
            cx.write_to_clipboard(ClipboardItem::new_string("x".repeat(64 * 1024 + 1)));
            assert!(!editor.context_action_enabled(PASTE, cx));
            editor.paste(cx);
            assert_eq!(editor.text(), "");
            cx.write_to_clipboard(ClipboardItem::new_string("🚀🚀".into()));
            assert_eq!(cx.read_text_from_clipboard(7), None);
            assert_eq!(cx.read_text_from_clipboard(8).as_deref(), Some("🚀🚀"));
            assert!(editor.context_action_enabled(PASTE, cx));
            editor.paste(cx);
            assert_eq!(editor.text(), "🚀🚀");
        });
    }

    #[gpui::test]
    fn context_menu_paste_uses_code_validation_notifications_and_undo(
        cx: &mut gpui::TestAppContext,
    ) {
        // Menu and keyboard share paste(); normalization and edit delivery must agree.
        let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let delivered = changes.clone();
        let window = cx.add_window(|_, cx| {
            TextEditor::new("", cx).email_code().bound_edits(6).on_change(Arc::new(
                move |text, _, _| {
                    delivered
                        .lock()
                        .expect("test fixture operation succeeds")
                        .push(text.to_owned());
                    true
                },
            ))
        });
        let activate = window
            .update(cx, |editor, window, cx| {
                editor.focus_handle.focus(window);
                cx.write_to_clipboard(ClipboardItem::new_string("123-456".into()));
                assert!(editor.context_action_enabled(PASTE, cx));
                editor.context_activation(window, cx)
            })
            .expect("test fixture operation succeeds");
        cx.update(|cx| activate(PASTE, cx));
        window
            .update(cx, |editor, _, cx| {
                assert_eq!(editor.text(), "123456");
                assert_eq!(*changes.lock().expect("test fixture operation succeeds"), ["123456"]);
                assert!(editor.buffer.undo());
                editor.did_change(cx);
                assert_eq!(editor.text(), "");
                assert!(editor.buffer.redo());
                editor.did_change(cx);
                assert_eq!(editor.text(), "123456");
                editor.select_all(cx);
                cx.write_to_clipboard(ClipboardItem::new_string("invalid".into()));
                assert!(!editor.context_action_enabled(PASTE, cx));
                editor.paste(cx);
                assert_eq!(editor.text(), "123456");
                assert_eq!(changes.lock().expect("test fixture operation succeeds").len(), 3);
            })
            .expect("test fixture operation succeeds");
    }
}
