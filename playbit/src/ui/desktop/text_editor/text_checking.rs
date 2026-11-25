//! Versioned writing-assistance snapshots and bounded paragraph context.
//! Native corrections use exact UTF-16 boundaries and never inspect private fields.

use super::{Selection, TextCheckingState, TextEditor, utf8_range_to_utf16, utf16_range_to_utf8};
use std::ops::Range;

/// Keep native annotation and checking requests bounded even in long fields.
pub(super) const CONTEXT_UNITS_MAX: usize = 16_384;

pub(super) struct StateCache {
    selection: Selection,
    marked_range: Option<Range<usize>>,
    state: TextCheckingState,
}

impl TextEditor {
    pub(super) fn permits_text_checking(&self) -> bool {
        self.enabled && !self.read_only && !self.masked && !self.email_code && !self.literal
    }

    pub(super) fn checking_state(&mut self, editor_id: u64) -> Option<TextCheckingState> {
        if !self.permits_text_checking() {
            return None;
        }
        let selection = self.buffer.primary_selection();
        let marked_range = self.buffer.marked_range();
        if let Some(cache) = &self.checking_cache
            && cache.state.editor_id == editor_id
            && cache.selection == selection
            && cache.marked_range == marked_range
            && cache.state.revision == self.revision.current
            && cache.state.model_generation == self.model_generation
        {
            return Some(cache.state.clone());
        }
        let text = self.buffer.as_str();
        let document_length = self
            .checking_cache
            .as_ref()
            .filter(|cache| {
                cache.state.revision == self.revision.current
                    && cache.state.model_generation == self.model_generation
            })
            .map(|cache| cache.state.document_length)
            .unwrap_or_else(|| text.encode_utf16().count());
        let state = TextCheckingState {
            editor_id,
            revision: self.revision.current,
            model_generation: self.model_generation,
            document_length,
            selection: utf8_range_to_utf16(text, selection.range()),
            marked_range: marked_range.clone().map(|range| utf8_range_to_utf16(text, range)),
            inserted_range: self
                .checking_inserted_range
                .clone()
                .map(|range| utf8_range_to_utf16(text, range)),
        };
        self.checking_cache = Some(StateCache { selection, marked_range, state: state.clone() });
        Some(state)
    }
}

/// Reject split surrogate pairs, reversed ranges and offsets outside the document.
pub(super) fn exact_utf16_range(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    if range.start > range.end {
        return None;
    }
    let mut start = None;
    let mut units = 0;
    for (offset, character) in text.char_indices() {
        if units == range.start {
            start = Some(offset);
        }
        if units == range.end {
            return Some(start?..offset);
        }
        units += character.len_utf16();
        if units > range.end {
            return None;
        }
    }
    if units == range.start {
        start = Some(text.len());
    }
    (units == range.end).then(|| start.map(|start| start..text.len())).flatten()
}

pub(super) fn paragraph_context(
    text: &str,
    range: Option<Range<usize>>,
    selection_head: usize,
) -> Option<(String, Range<usize>)> {
    let requested = match range {
        Some(range) => {
            let length = text.encode_utf16().count();
            if range.start > range.end || range.start > length {
                return None;
            }
            utf16_range_to_utf8(text, range.start..range.end.min(length))
        }
        None => selection_head..selection_head,
    };
    if !text.is_char_boundary(requested.start) || !text.is_char_boundary(requested.end) {
        return None;
    }
    let requested_units =
        text[requested.clone()].encode_utf16().take(CONTEXT_UNITS_MAX + 1).count();
    // Preserve all requested paragraphs when they fit. Oversized proposals
    // receive bounded context at their start, reported by the actual range.
    let (requested, requested_units) = if requested_units > CONTEXT_UNITS_MAX {
        (requested.start..requested.start, 0)
    } else {
        (requested, requested_units)
    };
    // Scan at most one bounded window on either side. Paragraph terminators
    // belong to the preceding paragraph, as in AppKit's paragraph ranges.
    let remaining = CONTEXT_UNITS_MAX - requested_units;
    let (mut start, before) = extend_before(text, requested.start, remaining / 2);
    let (end, after) = if !requested.is_empty() && text[..requested.end].ends_with(['\r', '\n']) {
        (requested.end, 0)
    } else {
        extend_after(text, requested.end, remaining - before)
    };
    if before + after < remaining {
        start = extend_before(text, start, remaining - before - after).0;
    }
    let range = utf8_range_to_utf16(text, start..end);
    Some((text[start..end].to_owned(), range))
}

fn extend_before(text: &str, anchor: usize, limit: usize) -> (usize, usize) {
    let mut start = anchor;
    let mut units = 0;
    for (offset, character) in text[..anchor].char_indices().rev() {
        if matches!(character, '\r' | '\n') || units + character.len_utf16() > limit {
            break;
        }
        units += character.len_utf16();
        start = offset;
    }
    (start, units)
}

fn extend_after(text: &str, anchor: usize, limit: usize) -> (usize, usize) {
    let mut end = anchor;
    let mut units = 0;
    for (offset, character) in text[anchor..].char_indices() {
        if units + character.len_utf16() > limit {
            break;
        }
        units += character.len_utf16();
        end = anchor + offset + character.len_utf8();
        if matches!(character, '\r' | '\n') {
            break;
        }
    }
    (end, units)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use gpui::{AppContext, EntityInputHandler};
    use std::sync::{Arc, Mutex};

    #[test]
    fn checking_context_preserves_scalar_boundaries_and_paragraphs() {
        // Request the paragraph containing the caret and bound a long emoji field.
        let text = "first\n日本🙂 second\nlast";
        assert_eq!(
            paragraph_context(text, Some(8..10), 0),
            Some(("日本🙂 second\n".to_owned(), 6..18)),
        );
        assert_eq!(paragraph_context("", None, 0), Some((String::new(), 0..0)));
        assert_eq!(paragraph_context("abc", Some(1..99), 0), Some(("abc".to_owned(), 0..3)));
        assert_eq!(paragraph_context("abc", Some(4..99), 0), None);
        assert_eq!(
            paragraph_context("one\ntwo", Some(0..7), 0),
            Some(("one\ntwo".to_owned(), 0..7))
        );
        assert_eq!(paragraph_context("one\ntwo", Some(0..4), 0), Some(("one\n".to_owned(), 0..4)));
        assert_eq!(exact_utf16_range("a🙂b", 1..3), Some(1..5));
        assert_eq!(exact_utf16_range("a🙂b", 2..3), None);
        assert_eq!(exact_utf16_range("a🙂b", 1..2), None);
        assert_eq!(exact_utf16_range("a🙂b", 4..5), None);
        let long = "🙂".repeat(CONTEXT_UNITS_MAX);
        let (context, actual) = paragraph_context(&long, None, long.len() / 2).unwrap();
        assert_eq!(context.encode_utf16().count(), CONTEXT_UNITS_MAX);
        assert_eq!(actual.len(), CONTEXT_UNITS_MAX);
        assert_eq!(context, long[exact_utf16_range(&long, actual).unwrap()]);
    }

    #[gpui::test]
    fn checking_policy_and_state_follow_current_editor(cx: &mut gpui::TestAppContext) {
        // Ordinary text participates; literal, private and uneditable fields do not.
        for kind in 0..6 {
            let editor = cx.new(|cx| {
                let editor = TextEditor::new("a🙂", cx);
                match kind {
                    1 => editor.literal(),
                    2 => editor.account_key(),
                    3 => editor.email_code(),
                    4 => editor.read_only(),
                    _ => editor,
                }
            });
            editor.update(cx, |editor, cx| {
                if kind == 5 {
                    editor.set_enabled(false);
                }
                let id = cx.entity_id().as_u64();
                assert_eq!(editor.checking_state(id).is_some(), kind == 0);
                if kind != 0 {
                    return;
                }
                editor.buffer.set_selection(Selection::caret(5));
                let state = editor.checking_state(id).unwrap();
                assert_eq!(state.selection, 3..3);
                assert_eq!(state.document_length, 3);
                assert_eq!(editor.checking_state(id), Some(state.clone()));
                editor.buffer.set_selection(Selection::caret(1));
                assert_ne!(editor.checking_state(id), Some(state));
            });
        }
    }

    #[gpui::test]
    fn checking_correction_has_one_delivery_and_independent_undo(cx: &mut gpui::TestAppContext) {
        // Correct a Unicode range after typing, then undo correction separately.
        let delivered = Arc::new(Mutex::new(Vec::new()));
        cx.add_empty_window().update(|window, cx| {
            let delivered = delivered.clone();
            let editor = cx.new(|cx| {
                TextEditor::new("🙂 ", cx).on_change(Arc::new(move |text, _, _| {
                    delivered.lock().unwrap().push(text.to_owned());
                    true
                }))
            });
            editor.update(cx, |editor, cx| {
                editor.focus_handle.focus(window);
                editor.buffer.set_selection(Selection::caret("🙂 ".len()));
                editor.replace_text_in_range(None, "kbright", window, cx);
                let state = editor.text_checking_state(window, cx).unwrap();
                assert_eq!(state.inserted_range, Some(3..10));
                assert!(editor.replace_text_for_checking(&state, 3..10, "→", window, cx));
                assert_eq!(editor.text(), "🙂 →");
                assert!(!editor.replace_text_for_checking(&state, 3..10, "x", window, cx));
                assert!(editor.buffer.undo());
                editor.did_change(cx);
                assert_eq!(editor.text(), "🙂 kbright");
                assert!(editor.buffer.undo());
                editor.did_change(cx);
                assert_eq!(editor.text(), "🙂 ");
                assert!(editor.buffer.redo());
                assert_eq!(editor.text(), "🙂 kbright");
                assert!(editor.buffer.redo());
                assert_eq!(editor.text(), "🙂 →");
            });
        });
        assert_eq!(*delivered.lock().unwrap(), ["🙂 kbright", "🙂 →", "🙂 kbright", "🙂 "]);
    }

    #[gpui::test]
    fn checking_rejects_stale_unsafe_and_oversized_edits(cx: &mut gpui::TestAppContext) {
        // Reject stale selection/model snapshots and unsafe boundaries before mutation.
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("a🙂b", cx).bound_edits(8));
            editor.update(cx, |editor, cx| {
                editor.focus_handle.focus(window);
                let state = editor.text_checking_state(window, cx).unwrap();
                assert!(!editor.replace_text_for_checking(&state, 2..3, "x", window, cx));
                assert!(!editor.replace_text_for_checking(&state, 1..3, "1234567", window, cx));
                assert!(!editor.replace_text_for_checking(&state, 0..1, "x\ny", window, cx));
                assert_eq!(editor.text(), "a🙂b");
                assert!(editor.select_text_for_checking(&state, 1..3, window, cx));
                assert_eq!(editor.buffer.primary_selection(), Selection::new(1, 5));
                assert!(!editor.replace_text_for_checking(&state, 1..3, "x", window, cx));
                let state = editor.text_checking_state(window, cx).unwrap();
                editor.acknowledge(None, "changed", cx);
                assert!(!editor.replace_text_for_checking(&state, 1..3, "x", window, cx));
                let state = editor.text_checking_state(window, cx).unwrap();
                editor.set_enabled(false);
                assert!(!editor.replace_text_for_checking(&state, 0..1, "x", window, cx));
            });
        });
    }

    #[gpui::test]
    fn checking_suspends_during_composition_and_delivers_on_commit(cx: &mut gpui::TestAppContext) {
        // Composition updates remain private until committed and then identify inserted text.
        let delivered = Arc::new(Mutex::new(Vec::new()));
        cx.add_empty_window().update(|window, cx| {
            let delivered = delivered.clone();
            let editor = cx.new(|cx| {
                TextEditor::new("", cx).on_change(Arc::new(move |text, _, _| {
                    delivered.lock().unwrap().push(text.to_owned());
                    true
                }))
            });
            editor.update(cx, |editor, cx| {
                editor.focus_handle.focus(window);
                editor.replace_and_mark_text_in_range(None, "日本", None, window, cx);
                let state = editor.text_checking_state(window, cx).unwrap();
                assert_eq!(state.marked_range, Some(0..2));
                assert_eq!(state.inserted_range, None);
                assert!(!editor.replace_text_for_checking(&state, 0..2, "x", window, cx));
                editor.unmark_text(window, cx);
                let state = editor.text_checking_state(window, cx).unwrap();
                assert_eq!(state.marked_range, None);
                assert_eq!(state.inserted_range, Some(0..2));
                assert!(editor.replace_text_for_checking(&state, 0..2, "日本語", window, cx));
            });
        });
        assert_eq!(*delivered.lock().unwrap(), ["日本", "日本語"]);
    }

    #[gpui::test]
    fn checking_insertion_follows_primary_selection_after_multiple_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        // Earlier selections shift the primary insertion during one typing edit.
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("x ab", cx));
            editor.update(cx, |editor, cx| {
                editor.focus_handle.focus(window);
                editor
                    .buffer
                    .set_selections_with_primary(Selection::caret(4), [Selection::caret(0)]);
                editor.replace_text_in_range(Some(4..4), "c", window, cx);
                assert_eq!(editor.text(), "cx abc");
                assert_eq!(
                    editor.text_checking_state(window, cx).unwrap().inserted_range,
                    Some(5..6)
                );
            });
        });
    }

    #[gpui::test]
    fn checking_rejects_corrections_after_focus_moves(cx: &mut gpui::TestAppContext) {
        // Native work captured before focus moves must not mutate the old field.
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("kbright", cx));
            editor.update(cx, |editor, cx| {
                editor.focus_handle.focus(window);
                let expected = editor.text_checking_state(window, cx).unwrap();
                let other = cx.focus_handle();
                other.focus(window);
                assert_eq!(editor.text_checking_state(window, cx), None);
                assert!(!editor.replace_text_for_checking(&expected, 0..7, "→", window, cx));
                assert!(!editor.select_text_for_checking(&expected, 0..7, window, cx));
                assert_eq!(editor.text(), "kbright");
            });
        });
    }

    #[gpui::test]
    fn sensitive_input_policy_requires_focused_editable_private_field(
        cx: &mut gpui::TestAppContext,
    ) {
        // Literal formatting is independent of the focused private-input policy.
        cx.add_empty_window().update(|window, cx| {
            for kind in 0..7 {
                let editor = cx.new(|cx| {
                    let editor = TextEditor::new("", cx);
                    match kind {
                        1 => editor.account_key(),
                        2 => editor.email_code(),
                        3 => editor.literal(),
                        4 => editor.account_key().read_only(),
                        5 => editor.email_code().read_only(),
                        6 => editor.account_key(),
                        _ => editor,
                    }
                });
                editor.update(cx, |editor, cx| {
                    if kind == 6 {
                        editor.set_enabled(false);
                    }
                    assert!(!editor.sensitive_input(window, cx));
                    editor.focus_handle.focus(window);
                    assert_eq!(editor.sensitive_input(window, cx), matches!(kind, 1 | 2));
                });
            }
        });
    }

    #[gpui::test]
    fn sensitive_fields_reject_composition_without_changing_or_delivering_text(
        cx: &mut gpui::TestAppContext,
    ) {
        // Reject even ASCII marked text while preserving the ordinary ASCII insertion path.
        for code in [false, true] {
            let delivered = Arc::new(Mutex::new(Vec::new()));
            cx.add_empty_window().update(|window, cx| {
                let delivered = delivered.clone();
                let editor = cx.new(|cx| {
                    let editor = TextEditor::new("", cx);
                    let editor = if code { editor.email_code() } else { editor.account_key() };
                    editor.on_change(Arc::new(move |text, _, _| {
                        delivered.lock().unwrap().push(text.to_owned());
                        true
                    }))
                });
                editor.update(cx, |editor, cx| {
                    editor.focus_handle.focus(window);
                    let revision = editor.revision.current;
                    for text in [if code { "123" } else { "abc" }, "日本"] {
                        editor.replace_and_mark_text_in_range(None, text, None, window, cx);
                    }
                    assert_eq!(editor.text(), "");
                    assert!(editor.buffer.marked_range().is_none());
                    assert_eq!(editor.revision.current, revision);
                    let text = if code { "001002" } else { "abc123" };
                    editor.replace_text_in_range(None, text, window, cx);
                    assert_eq!(editor.text(), text);
                    assert_eq!(editor.revision.current, revision + 1);
                });
            });
            assert_eq!(*delivered.lock().unwrap(), [if code { "001002" } else { "abc123" }]);
        }
    }

    #[gpui::test]
    fn normal_composition_commits_before_focus_enters_sensitive_field(
        cx: &mut gpui::TestAppContext,
    ) {
        // A collapsed composition caret must not strand marked text in the outgoing editor.
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let delivery = delivered.clone();
        let normal = cx.add_window(|window, cx| {
            let editor = TextEditor::new("", cx).on_change(Arc::new(move |text, _, _| {
                delivery.lock().unwrap().push(text.to_owned());
                true
            }));
            editor.focus_handle.focus(window);
            window.activate_window();
            editor
        });
        cx.run_until_parked();
        let sensitive = cx.new(|cx| TextEditor::new("", cx).account_key());
        normal
            .update(cx, |editor, window, cx| {
                editor.replace_and_mark_text_in_range(None, "日本", None, window, cx);
                assert!(editor.buffer.primary_selection().is_empty());
                assert!(editor.buffer.marked_range().is_some());
                assert!(delivered.lock().unwrap().is_empty());
                sensitive.read(cx).focus_handle.focus(window);
            })
            .unwrap();
        cx.run_until_parked();
        normal
            .update(cx, |editor, window, cx| {
                assert!(editor.buffer.marked_range().is_none());
                assert_eq!(editor.text(), "日本");
                assert!(!editor.sensitive_input(window, cx));
                sensitive.update(cx, |editor, cx| assert!(editor.sensitive_input(window, cx)));
            })
            .unwrap();
        assert_eq!(*delivered.lock().unwrap(), ["日本"]);
    }

    #[gpui::test]
    fn checking_geometry_returns_only_the_first_visual_row(cx: &mut gpui::TestAppContext) {
        // Shape wrapped Unicode and multiline text, then compare the returned
        // UTF-16 range to the first rectangle's actual row and newline coverage.
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let text = format!("{}\nlast🙂", "🙂日本 words ".repeat(80));
        let window = cx.add_window(|_, cx| {
            let mut editor = TextEditor::new("", cx).read_only_message(gpui::ScrollHandle::new());
            editor.buffer = super::super::Buffer::from(text.clone());
            editor
        });
        cx.run_until_parked();
        window
            .update(cx, |editor, window, cx| {
                let layout = editor.layout.as_ref().unwrap();
                assert!(layout.rows.len() > 2);
                let first = &layout.rows[0];
                assert!(!first.newline_after, "first logical line wraps to multiple rows");
                let expected = utf8_range_to_utf16(&text, first.range.clone());
                let top = layout.bounds.top() - layout.scroll_y;
                let element_bounds = layout.bounds;
                let length = text.encode_utf16().count();
                let (bounds, actual) = editor
                    .bounds_for_range_with_actual(0..length, element_bounds, window, cx)
                    .unwrap();
                assert_eq!(actual, expected);
                assert_eq!(bounds.top(), top);
                let newline = text[..text.find('\n').unwrap()].encode_utf16().count();
                let (_, actual) = editor
                    .layout
                    .as_ref()
                    .unwrap()
                    .first_bounds_with_actual_range(
                        text.find('\n').unwrap()..text.len(),
                        editor.primary_affinity,
                    )
                    .unwrap();
                assert_eq!(utf8_range_to_utf16(&text, actual), newline..newline + 1);
                let previous_scroll = editor.layout.as_ref().unwrap().scroll_y;
                editor.layout.as_mut().unwrap().scroll_y += gpui::px(1_000_000.);
                assert!(
                    editor
                        .bounds_for_range_with_actual(0..length, element_bounds, window, cx)
                        .is_none(),
                    "offscreen text must not anchor a native suggestion"
                );
                editor.layout.as_mut().unwrap().scroll_y = previous_scroll;
                let (caret, actual) =
                    editor.bounds_for_range_with_actual(1..1, element_bounds, window, cx).unwrap();
                assert_eq!(actual, 0..0);
                assert_eq!(caret.size.width, gpui::px(1.));
                let (_, actual) =
                    editor.bounds_for_range_with_actual(1..2, element_bounds, window, cx).unwrap();
                assert_eq!(actual, 0..2);
            })
            .unwrap();
    }
}
