//! Portable text storage and editing primitives.
//!
//! Positions are UTF-8 byte offsets. [`Buffer`] keeps every position on a
//! character boundary and edits by extended grapheme cluster where requested.
//! This crate deliberately knows nothing about fonts, layout, rendering, or a
//! window system.

use std::cmp::{max, min};
use std::ops::Range;

use playbit_api::abi::text::{MAX_TEXT_BYTES, MAX_TEXT_EDITS, TextEdit};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// A directed selection. `anchor` stays fixed while an extended selection's
/// `head` moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub const fn caret(offset: usize) -> Self {
        Self::new(offset, offset)
    }

    pub fn start(self) -> usize {
        min(self.anchor, self.head)
    }

    pub fn end(self) -> usize {
        max(self.anchor, self.head)
    }

    pub fn range(self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    pub fn is_reversed(self) -> bool {
        self.head < self.anchor
    }
}

/// The unit used by deletion commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteUnit {
    Grapheme,
    Word,
}

/// A logical movement independent of visual layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Movement {
    PreviousGrapheme,
    NextGrapheme,
    PreviousWord,
    NextWord,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
}

impl Movement {
    fn is_backward(self) -> bool {
        matches!(
            self,
            Self::PreviousGrapheme | Self::PreviousWord | Self::LineStart | Self::DocumentStart
        )
    }

    fn collapses_selection(self) -> bool {
        matches!(
            self,
            Self::PreviousGrapheme | Self::NextGrapheme | Self::PreviousWord | Self::NextWord
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    text: String,
    selections: Vec<Selection>,
    primary: usize,
}

#[derive(Clone, Debug)]
struct Composition {
    before: State,
    marked: Range<usize>,
}

#[derive(Clone, Debug)]
struct History {
    state: State,
    edit: Option<TextEdit>,
}

/// UTF-8 text, selections, and edit history.
#[derive(Clone, Debug)]
pub struct Buffer {
    text: String,
    selections: Vec<Selection>,
    primary: usize,
    undo: Vec<History>,
    redo: Vec<History>,
    history_edit: Option<TextEdit>,
    pending_edits: Option<Vec<TextEdit>>,
    edit_group: Option<State>,
    composition: Option<Composition>,
    max_bytes: Option<usize>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&str> for Buffer {
    fn from(text: &str) -> Self {
        Self::from_string(text.to_owned())
    }
}

impl From<String> for Buffer {
    fn from(text: String) -> Self {
        Self::from_string(text)
    }
}

impl Buffer {
    pub fn new() -> Self {
        Self::from_string(String::new())
    }

    pub fn from_string(text: String) -> Self {
        Self {
            text,
            selections: vec![Selection::caret(0)],
            primary: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            history_edit: None,
            pending_edits: None,
            edit_group: None,
            composition: None,
            max_bytes: None,
        }
    }

    /// Retain exact native operations until the delivery owner queues them.
    pub fn track_edits(mut self) -> Self {
        self.pending_edits = Some(Vec::new());
        self
    }

    pub fn pending_edits(&self) -> &[TextEdit] {
        self.pending_edits.as_deref().unwrap_or_default()
    }

    pub fn clear_pending_edits(&mut self) {
        if let Some(edits) = &mut self.pending_edits {
            edits.clear();
        }
    }

    fn permits_edits(&self, count: usize) -> bool {
        self.pending_edits.as_ref().is_none_or(|edits| count <= MAX_TEXT_EDITS - edits.len())
    }

    fn record_edit(&mut self, edit: TextEdit) {
        assert!(edit.start <= edit.end);
        if let Some(edits) = &mut self.pending_edits {
            assert!(edits.len() < MAX_TEXT_EDITS, "mutation reserved its edit record");
            edits.push(edit);
        }
        self.history_edit = Some(self.history_edit.map_or(edit, |prior| prior.then(edit)));
    }

    /// Browser fields reject oversized edits and retain at most sixteen undo
    /// states and thirty-two selections. Rejection preserves the whole value.
    pub fn bound_edits(mut self, max_bytes: usize) -> Self {
        assert!(self.text.len() <= max_bytes, "initial bounded text fits its editor");
        self.max_bytes = Some(max_bytes);
        self
    }

    pub(super) fn permits_replacement(&self, removed: usize, added: usize) -> bool {
        let limit = self.max_bytes.or_else(|| self.pending_edits.as_ref().map(|_| MAX_TEXT_BYTES));
        limit.is_none_or(|limit| {
            self.text
                .len()
                .checked_sub(removed)
                .and_then(|len| len.checked_add(added))
                .is_some_and(|len| len <= limit)
        })
    }

    pub(super) fn permits_selection_replacement(&self, replacement: &str) -> bool {
        let removed = self.selections.iter().map(|selection| selection.range().len()).sum();
        replacement
            .len()
            .checked_mul(self.selections.len())
            .is_some_and(|added| self.permits_replacement(removed, added))
    }

    fn bound_history(&mut self) {
        if self.max_bytes.is_some() && self.undo.len() > 16 {
            self.undo.drain(..self.undo.len() - 16);
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// All selections in sorted edit order. Use
    /// [`primary_selection`](Self::primary_selection) to obtain the active one.
    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    pub fn primary_selection(&self) -> Selection {
        self.selections[self.primary]
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.set_selections([selection]);
    }

    /// Sets selections after clamping offsets to UTF-8 character boundaries,
    /// sorting by range, and merging touching or overlapping ranges. The first
    /// supplied selection is primary, even when sorting moves it.
    pub fn set_selections(&mut self, selections: impl IntoIterator<Item = Selection>) {
        self.commit_edit_group();
        self.commit_composition();
        (self.selections, self.primary) = self.normalize_selections(selections);
    }

    /// Sets one explicit primary selection plus any additional selections.
    /// [`selections`](Self::selections) remains in sorted edit order.
    pub fn set_selections_with_primary(
        &mut self,
        primary: Selection,
        additional: impl IntoIterator<Item = Selection>,
    ) {
        self.commit_edit_group();
        self.commit_composition();
        (self.selections, self.primary) =
            self.normalize_selections(std::iter::once(primary).chain(additional));
    }

    fn normalize_selections(
        &self,
        selections: impl IntoIterator<Item = Selection>,
    ) -> (Vec<Selection>, usize) {
        let count =
            if self.max_bytes.is_some() || self.pending_edits.is_some() { 32 } else { usize::MAX };
        normalize_selections(&self.text, selections.into_iter().take(count), 0)
    }

    /// Admit the IME's explicit replacement before any selection or composition changes.
    #[cfg(feature = "desktop-ui")]
    pub(super) fn select_composition_range(&mut self, range: Range<usize>, added: usize) -> bool {
        let replacement = Selection::new(range.start, range.end);
        let additional = self
            .selections
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.primary)
            .map(|(_, selection)| *selection);
        let (selections, primary) =
            self.normalize_selections(std::iter::once(replacement).chain(additional));
        if !self.permits_replacement(selections[primary].range().len(), added) {
            return false;
        }
        self.commit_edit_group();
        self.commit_composition();
        self.selections = selections;
        self.primary = primary;
        true
    }

    /// Replaces every selection with the same string and leaves a caret after
    /// each inserted copy.
    pub fn replace_selections(&mut self, replacement: &str) {
        self.commit_composition();
        if !self.permits_edits(self.selections.len())
            || !self.permits_selection_replacement(replacement)
        {
            return;
        }
        if self.edit_group.is_none()
            && replacement.is_empty()
            && self.selections.iter().all(|selection| selection.is_empty())
        {
            // Every selection is a caret and there is nothing to insert, so
            // this can't change the text or selections. Skip snapshotting.
            return;
        }
        let before = self.edit_group.is_none().then(|| self.state());
        self.replace_selections_inner(replacement);
        if let Some(before) = before {
            self.finish_edit(before);
        }
    }

    fn replace_selections_inner(&mut self, replacement: &str) {
        let old_primary = self.primary;
        let mut next = Vec::with_capacity(self.selections.len());
        let mut shift = 0isize;

        for selection in &self.selections {
            let range = selection.range();
            let start = shifted(range.start, shift);
            let caret = start + replacement.len();
            next.push(Selection::caret(caret));
            shift += replacement.len() as isize - range.len() as isize;
        }

        for index in (0..self.selections.len()).rev() {
            let range = self.selections[index].range();
            if self.text.get(range.clone()) != Some(replacement) {
                self.record_edit(TextEdit {
                    start: range.start,
                    end: range.end,
                    inserted_len: replacement.len(),
                });
            }
            self.text.replace_range(range, replacement);
        }
        let (selections, primary) = normalize_selections(&self.text, next, old_primary);
        self.selections = selections;
        self.primary = primary;
    }

    pub fn insert(&mut self, text: &str) {
        self.replace_selections(text);
    }

    /// Starts a non-nesting edit group. Calls to [`insert`](Self::insert) and
    /// [`replace_selections`](Self::replace_selections) join the group until it
    /// is committed, producing at most one undo record. Returns false when a
    /// group was already open.
    pub fn begin_edit_group(&mut self) -> bool {
        self.commit_composition();
        if self.edit_group.is_some() {
            return false;
        }
        self.edit_group = Some(self.state());
        true
    }

    /// Commits the open edit group. Returns whether a group was open. A group
    /// containing no changes does not create an undo record.
    pub fn commit_edit_group(&mut self) -> bool {
        let Some(before) = self.edit_group.take() else {
            return false;
        };
        self.finish_edit(before);
        true
    }

    /// Replaces an explicit UTF-8 byte range and remaps all selections around
    /// it. Offsets are clamped down to character boundaries. Returns the range
    /// occupied by the replacement.
    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) -> Range<usize> {
        self.commit_edit_group();
        self.commit_composition();
        let replaced = normalized_range(&self.text, range);
        if !self.permits_edits(1) || !self.permits_replacement(replaced.len(), replacement.len()) {
            return replaced.start..replaced.start;
        }
        let inserted = replaced.start..replaced.start + replacement.len();
        let text_changed = self.text.get(replaced.clone()) != Some(replacement);
        // Remapping is bounded by the selection count, not the buffer size,
        // so it's cheap to compute even when the text turns out unchanged
        // (e.g. a caret at the start of the range still moves to its end).
        let remapped = remap_selections(&self.text, &self.selections, &replaced, replacement.len());

        if text_changed {
            let before = self.state();
            self.record_edit(TextEdit {
                start: replaced.start,
                end: replaced.end,
                inserted_len: replacement.len(),
            });
            self.text.replace_range(replaced, replacement);
            let (selections, primary) = normalize_selections(&self.text, remapped, self.primary);
            self.selections = selections;
            self.primary = primary;
            self.finish_edit(before);
            return inserted;
        }

        // The text is provably unchanged, so normalizing against the current
        // text (rather than a post-mutation copy) is correct. Only skip the
        // snapshot once the normalized selections and primary also come out
        // unchanged.
        let (selections, primary) = normalize_selections(&self.text, remapped, self.primary);
        if selections == self.selections && primary == self.primary {
            return inserted;
        }
        let before = self.state();
        self.selections = selections;
        self.primary = primary;
        self.finish_edit(before);
        inserted
    }

    /// Updates the active IME composition, starting one at the first (primary)
    /// selection when needed. `selection` contains byte offsets relative to
    /// `text`. Repeated updates replace the marked range without adding undo
    /// records. Call [`commit_composition`](Self::commit_composition) to end
    /// composition and create one undo step.
    ///
    /// Composition applies only to the primary selection. Additional
    /// selections are retained and shifted when the marked text changes.
    pub fn update_composition(&mut self, text: &str, selection: Selection) {
        self.commit_edit_group();
        let replaced = self
            .composition
            .as_ref()
            .map(|composition| composition.marked.clone())
            .unwrap_or_else(|| self.primary_selection().range());
        if !self.permits_edits(1) || !self.permits_replacement(replaced.len(), text.len()) {
            return;
        }
        let (before, replaced) = match self.composition.take() {
            Some(composition) => (composition.before, composition.marked),
            None => (self.state(), self.primary_selection().range()),
        };
        let old_selections = self.selections.clone();
        let additional: Vec<_> = old_selections
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.primary)
            .map(|(_, selection)| *selection)
            .collect();
        let additional = remap_selections(&self.text, &additional, &replaced, text.len());

        self.text.replace_range(replaced.clone(), text);
        let marked = replaced.start..replaced.start + text.len();
        let relative = clamp_selection(text, selection);
        let primary = Selection::new(marked.start + relative.anchor, marked.start + relative.head);
        let mut selections = Vec::with_capacity(1 + additional.len());
        selections.push(primary);
        selections.extend(additional);
        let (selections, primary) = normalize_selections(&self.text, selections, 0);
        self.selections = selections;
        self.primary = primary;
        self.composition = Some(Composition { before, marked });
    }

    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.composition.as_ref().map(|composition| composition.marked.clone())
    }

    /// Ends the active composition and records all of its updates as one undo
    /// transaction. Returns whether a composition was active.
    pub fn commit_composition(&mut self) -> bool {
        let Some(composition) = self.composition.take() else {
            return false;
        };
        let range = composition.before.selections[composition.before.primary].range();
        if composition.before.text != self.text {
            self.record_edit(TextEdit {
                start: range.start,
                end: range.end,
                inserted_len: composition.marked.len(),
            });
        }
        self.finish_edit(composition.before);
        true
    }

    /// Deletes a grapheme or word before every caret. Non-empty selections are
    /// deleted as-is.
    pub fn backspace(&mut self, unit: DeleteUnit) {
        self.delete(unit, true);
    }

    /// Deletes a grapheme or word after every caret. Non-empty selections are
    /// deleted as-is.
    pub fn delete_forward(&mut self, unit: DeleteUnit) {
        self.delete(unit, false);
    }

    /// Moves every selection. When `extend` is true the anchor stays fixed.
    /// Horizontal movement without extension first collapses a non-empty
    /// selection toward the movement direction.
    pub fn move_selections(&mut self, movement: Movement, extend: bool) {
        self.commit_edit_group();
        self.commit_composition();
        let old_primary = self.primary;
        let mut moved = Vec::with_capacity(self.selections.len());
        for selection in &self.selections {
            let head = if !extend && !selection.is_empty() && movement.collapses_selection() {
                if movement.is_backward() { selection.start() } else { selection.end() }
            } else {
                self.moved_offset(selection.head, movement)
            };
            let anchor = if extend { selection.anchor } else { head };
            moved.push(Selection::new(anchor, head));
        }
        let (selections, primary) = normalize_selections(&self.text, moved, old_primary);
        self.selections = selections;
        self.primary = primary;
    }

    /// Returns the extended Unicode grapheme containing `offset`.
    pub fn grapheme_range_at(&self, offset: usize) -> Range<usize> {
        grapheme_range_at(&self.text, offset)
    }

    /// Returns the Unicode word containing `offset`. On punctuation or other
    /// non-word text, returns one grapheme instead.
    pub fn word_range_at(&self, offset: usize) -> Range<usize> {
        word_range_at(&self.text, offset)
    }

    /// Selects the logical line containing `offset`, including its trailing
    /// newline when there is one.
    pub fn line_range_at(&self, offset: usize) -> Range<usize> {
        let offset = clamp_offset(&self.text, offset);
        let start = line_start(&self.text, offset);
        let end = match self.text[offset..].find('\n') {
            Some(relative) => offset + relative + 1,
            None => self.text.len(),
        };
        start..end
    }

    pub fn select_all(&mut self) {
        self.set_selection(Selection::new(0, self.text.len()));
    }

    pub fn undo(&mut self) -> bool {
        self.commit_edit_group();
        self.commit_composition();
        if self.undo.last().is_some_and(|previous| {
            !self.permits_replacement(self.text.len(), previous.state.text.len())
        }) {
            return false;
        }
        if !self.permits_edits(1) {
            return false;
        }
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        let edit = previous.edit.map(TextEdit::inverse);
        self.redo.push(History { state: self.state(), edit });
        if let Some(edit) = edit {
            self.record_edit(edit);
        }
        self.history_edit = None;
        self.restore(previous.state);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.commit_edit_group();
        self.commit_composition();
        if self
            .redo
            .last()
            .is_some_and(|next| !self.permits_replacement(self.text.len(), next.state.text.len()))
        {
            return false;
        }
        if !self.permits_edits(1) {
            return false;
        }
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let edit = next.edit.map(TextEdit::inverse);
        self.undo.push(History { state: self.state(), edit });
        self.bound_history();
        if let Some(edit) = edit {
            self.record_edit(edit);
        }
        self.history_edit = None;
        self.restore(next.state);
        true
    }

    fn delete(&mut self, unit: DeleteUnit, backward: bool) {
        self.commit_edit_group();
        self.commit_composition();
        let old_primary = self.primary;
        let expanded: Vec<_> = self
            .selections
            .iter()
            .map(|selection| {
                if !selection.is_empty() {
                    return *selection;
                }
                let position = selection.head;
                let other = match (unit, backward) {
                    (DeleteUnit::Grapheme, true) => {
                        previous_grapheme_boundary(&self.text, position)
                    }
                    (DeleteUnit::Grapheme, false) => next_grapheme_boundary(&self.text, position),
                    (DeleteUnit::Word, true) => previous_word_boundary(&self.text, position),
                    (DeleteUnit::Word, false) => next_word_boundary(&self.text, position),
                };
                Selection::new(position, other)
            })
            .collect();
        // Every caret already pinned against a boundary (e.g. backspace at
        // the start of the buffer) expands to an empty range, so nothing
        // would be deleted from the text. But normalizing can still merge or
        // reorder selections, so only skip the snapshot once the normalized
        // selections and primary also come out unchanged.
        let text_changed = expanded.iter().any(|selection| !selection.is_empty());
        let (selections, primary) = normalize_selections(&self.text, expanded, old_primary);
        let selections_changed = selections != self.selections || primary != self.primary;
        if !text_changed && !selections_changed {
            return;
        }
        if !self.permits_edits(selections.len()) {
            return;
        }
        let before = self.state();
        self.selections = selections;
        self.primary = primary;
        self.replace_selections_inner("");
        self.finish_edit(before);
    }

    fn moved_offset(&self, offset: usize, movement: Movement) -> usize {
        match movement {
            Movement::PreviousGrapheme => previous_grapheme_boundary(&self.text, offset),
            Movement::NextGrapheme => next_grapheme_boundary(&self.text, offset),
            Movement::PreviousWord => previous_word_boundary(&self.text, offset),
            Movement::NextWord => next_word_boundary(&self.text, offset),
            Movement::LineStart => line_start(&self.text, offset),
            Movement::LineEnd => line_end(&self.text, offset),
            Movement::DocumentStart => 0,
            Movement::DocumentEnd => self.text.len(),
        }
    }

    fn state(&self) -> State {
        State {
            text: self.text.clone(),
            selections: self.selections.clone(),
            primary: self.primary,
        }
    }

    fn restore(&mut self, state: State) {
        self.text = state.text;
        self.selections = state.selections;
        self.primary = state.primary;
    }

    fn finish_edit(&mut self, before: State) {
        // Compare fields directly against the live buffer instead of cloning
        // it into a second `State` just to throw the clone away.
        let changed = before.text != self.text
            || before.selections != self.selections
            || before.primary != self.primary;
        let edit = self.history_edit.take();
        if changed {
            self.undo.push(History { state: before, edit });
            self.bound_history();
            self.redo.clear();
        }
    }
}

fn shifted(offset: usize, shift: isize) -> usize {
    if shift < 0 { offset - shift.unsigned_abs() } else { offset + shift as usize }
}

fn clamp_offset(text: &str, offset: usize) -> usize {
    let mut offset = min(offset, text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn clamp_selection(text: &str, selection: Selection) -> Selection {
    Selection::new(clamp_offset(text, selection.anchor), clamp_offset(text, selection.head))
}

fn normalized_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = clamp_offset(text, range.start);
    let end = clamp_offset(text, range.end);
    min(start, end)..max(start, end)
}

fn remap_selections(
    text: &str,
    selections: &[Selection],
    replaced: &Range<usize>,
    replacement_len: usize,
) -> Vec<Selection> {
    selections
        .iter()
        .map(|selection| {
            let selection = clamp_selection(text, *selection);
            Selection::new(
                remap_offset(selection.anchor, replaced, replacement_len),
                remap_offset(selection.head, replaced, replacement_len),
            )
        })
        .collect()
}

fn remap_offset(offset: usize, replaced: &Range<usize>, replacement_len: usize) -> usize {
    if offset < replaced.start {
        offset
    } else if offset > replaced.end {
        shifted(offset, replacement_len as isize - replaced.len() as isize)
    } else {
        replaced.start + replacement_len
    }
}

fn normalize_selections(
    text: &str,
    selections: impl IntoIterator<Item = Selection>,
    primary: usize,
) -> (Vec<Selection>, usize) {
    struct TaggedSelection {
        selection: Selection,
        primary: bool,
    }

    struct Group {
        start: usize,
        end: usize,
        first: Selection,
        count: usize,
        primary: Option<Selection>,
    }

    let mut selections: Vec<_> = selections.into_iter().collect();
    if selections.is_empty() {
        return (vec![Selection::caret(0)], 0);
    }
    let primary = min(primary, selections.len() - 1);
    let mut selections: Vec<_> = selections
        .drain(..)
        .enumerate()
        .map(|(index, selection)| {
            let selection = clamp_selection(text, selection);
            TaggedSelection { selection, primary: index == primary }
        })
        .collect();
    selections.sort_by_key(|tagged| (tagged.selection.start(), tagged.selection.end()));

    let mut groups: Vec<Group> = Vec::with_capacity(selections.len());
    for tagged in selections {
        let selection = tagged.selection;
        let Some(previous) = groups.last_mut() else {
            groups.push(Group {
                start: selection.start(),
                end: selection.end(),
                first: selection,
                count: 1,
                primary: tagged.primary.then_some(selection),
            });
            continue;
        };

        let overlaps = selection.start() < previous.end
            || (selection.start() == previous.end
                && (!selection.is_empty()
                    || previous.start != previous.end
                    || selection.start() == previous.start));
        if overlaps {
            previous.start = min(previous.start, selection.start());
            previous.end = max(previous.end, selection.end());
            previous.count += 1;
            if tagged.primary {
                previous.primary = Some(selection);
            }
        } else {
            groups.push(Group {
                start: selection.start(),
                end: selection.end(),
                first: selection,
                count: 1,
                primary: tagged.primary.then_some(selection),
            });
        }
    }

    let primary = groups
        .iter()
        .position(|group| group.primary.is_some())
        .expect("primary selection was lost during normalization");
    let selections = groups
        .into_iter()
        .map(|group| {
            if group.count == 1 {
                return group.first;
            }
            match group.primary {
                Some(primary) if primary.head == group.start || primary.is_reversed() => {
                    Selection::new(group.end, group.start)
                }
                _ => Selection::new(group.start, group.end),
            }
        })
        .collect();
    (selections, primary)
}

// `GraphemeCursor` is always given the whole buffer as its chunk (with a
// chunk start of 0), so it never needs more context than it already has: it
// steps outward from `offset` a character at a time instead of rescanning
// the buffer from position 0, which is what made caret movement, backspace,
// and word jumps cost O(document length) on every keystroke.
fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = clamp_offset(text, offset);
    GraphemeCursor::new(offset, text.len(), true)
        .prev_boundary(text, 0)
        .expect("whole buffer is given as context")
        .unwrap_or(0)
}

fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = clamp_offset(text, offset);
    GraphemeCursor::new(offset, text.len(), true)
        .next_boundary(text, 0)
        .expect("whole buffer is given as context")
        .unwrap_or(text.len())
}

fn grapheme_range_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp_offset(text, offset);
    if offset == text.len() {
        return previous_grapheme_boundary(text, offset)..offset;
    }
    let at_boundary = GraphemeCursor::new(offset, text.len(), true)
        .is_boundary(text, 0)
        .expect("whole buffer is given as context");
    let start = if at_boundary { offset } else { previous_grapheme_boundary(text, offset) };
    start..next_grapheme_boundary(text, start)
}

// Word boundaries never cross a newline (UAX #29 breaks before and after
// one), so each lookup only needs to scan the logical line(s) around
// `offset` instead of the whole buffer. The slice given to
// `unicode_word_indices` must start and end on a line boundary rather than
// at `offset` itself: cutting a line off mid-word can change how a leading
// combining mark or joiner is classified, which would move the reported
// word boundary.
fn previous_word_boundary(text: &str, offset: usize) -> usize {
    let offset = clamp_offset(text, offset);
    let mut end = offset;
    loop {
        let start = line_start(text, end);
        let line_end = line_end(text, start);
        if let Some(found) = text[start..line_end]
            .unicode_word_indices()
            .map(|(index, _)| start + index)
            .take_while(|found| *found < offset)
            .last()
        {
            return found;
        }
        if start == 0 {
            return 0;
        }
        end = start - 1;
    }
}

fn next_word_boundary(text: &str, offset: usize) -> usize {
    let offset = clamp_offset(text, offset);
    let mut start = line_start(text, offset);
    loop {
        let end = line_end(text, start);
        if let Some(found) = text[start..end]
            .unicode_word_indices()
            .map(|(index, word)| start + index + word.len())
            .find(|found| *found > offset)
        {
            return found;
        }
        if end == text.len() {
            return text.len();
        }
        start = end + 1;
    }
}

fn word_range_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp_offset(text, offset);
    let start = line_start(text, offset);
    let end = line_end(text, offset);
    for (index, word) in text[start..end].unicode_word_indices() {
        let word_start = start + index;
        let word_end = word_start + word.len();
        if word_start <= offset && offset < word_end {
            return word_start..word_end;
        }
    }
    grapheme_range_at(text, offset)
}

fn line_start(text: &str, offset: usize) -> usize {
    let offset = clamp_offset(text, offset);
    text[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0)
}

fn line_end(text: &str, offset: usize) -> usize {
    let offset = clamp_offset(text, offset);
    text[offset..].find('\n').map(|relative| offset + relative).unwrap_or(text.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    // Keep browser drafts bounded by rejecting complete edits, then exercise
    // range replacement, multiple selections, composition and retained history.
    #[test]
    fn bounded_edits_preserve_values_and_limit_history() {
        let mut buffer = Buffer::from("café").bound_edits(8);
        buffer.set_selection(Selection::caret(5));
        buffer.insert("日本");
        assert_eq!(buffer.as_str(), "café");
        buffer.replace_range(0..5, "123456789");
        assert_eq!(buffer.as_str(), "café");
        buffer.update_composition("日本", Selection::caret(6));
        assert_eq!(buffer.as_str(), "café");
        assert!(buffer.marked_range().is_none());
        buffer.set_selections([Selection::caret(0), Selection::caret(5)]);
        buffer.insert("éé");
        assert_eq!(buffer.as_str(), "café");
        buffer.set_selection(Selection::new(0, 5));
        buffer.insert("日本");
        assert_eq!(buffer.as_str(), "日本");
        buffer.set_selection(Selection::new(0, 6));
        buffer.update_composition("é", Selection::caret(2));
        buffer.update_composition("123456789", Selection::caret(9));
        assert_eq!(buffer.as_str(), "é");
        assert_eq!(buffer.marked_range(), Some(0..2));
        buffer.commit_composition();
        for index in 0..100 {
            buffer.replace_range(0..buffer.as_str().len(), &format!("{index}"));
        }
        assert_eq!(buffer.undo.len(), 16);
        assert!((0..16).all(|_| buffer.undo()));
        assert!(!buffer.undo());
        assert!((0..16).all(|_| buffer.redo()));
        assert_eq!(buffer.as_str(), "99");
        buffer.set_selections((0..100).map(|_| Selection::caret(0)));
        assert!(buffer.selections.len() <= 32);
    }

    #[test]
    fn selection_helpers_preserve_direction() {
        let selection = Selection::new(8, 3);
        assert_eq!(selection.start(), 3);
        assert_eq!(selection.end(), 8);
        assert_eq!(selection.range(), 3..8);
        assert!(selection.is_reversed());
    }

    #[test]
    fn invalid_offsets_are_clamped_to_utf8_boundaries() {
        let mut buffer = Buffer::from("aéz");
        buffer.set_selection(Selection::new(2, 99));
        assert_eq!(buffer.selections(), &[Selection::new(1, 4)]);
    }

    #[test]
    fn overlapping_and_touching_selections_are_merged() {
        let mut buffer = Buffer::from("abcdefgh");
        buffer.set_selections([
            Selection::new(6, 4),
            Selection::new(1, 3),
            Selection::new(3, 5),
            Selection::caret(7),
            Selection::caret(7),
        ]);
        assert_eq!(buffer.selections(), &[Selection::new(6, 1), Selection::caret(7)]);
        assert_eq!(buffer.primary_selection(), Selection::new(6, 1));
    }

    #[test]
    fn distinct_carets_are_not_merged() {
        let mut buffer = Buffer::from("abc");
        buffer.set_selections([Selection::caret(0), Selection::caret(1)]);
        assert_eq!(buffer.selections(), &[Selection::caret(0), Selection::caret(1)]);
    }

    #[test]
    fn primary_selection_is_stable_when_sorted_to_the_right() {
        let mut buffer = Buffer::from("abc def");
        buffer.set_selections_with_primary(Selection::caret(7), [Selection::caret(1)]);
        assert_eq!(buffer.selections(), &[Selection::caret(1), Selection::caret(7)]);
        assert_eq!(buffer.primary_selection(), Selection::caret(7));

        buffer.insert("X");
        assert_eq!(buffer.as_str(), "aXbc defX");
        assert_eq!(buffer.selections(), &[Selection::caret(2), Selection::caret(9)]);
        assert_eq!(buffer.primary_selection(), Selection::caret(9));

        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "abc def");
        assert_eq!(buffer.primary_selection(), Selection::caret(7));
        assert!(buffer.redo());
        assert_eq!(buffer.primary_selection(), Selection::caret(9));
    }

    #[test]
    fn movement_and_deletion_retain_rightmost_primary() {
        let mut buffer = Buffer::from("abc def");
        buffer.set_selections_with_primary(Selection::caret(7), [Selection::caret(1)]);
        buffer.move_selections(Movement::PreviousGrapheme, false);
        assert_eq!(buffer.selections(), &[Selection::caret(0), Selection::caret(6)]);
        assert_eq!(buffer.primary_selection(), Selection::caret(6));

        buffer.backspace(DeleteUnit::Grapheme);
        assert_eq!(buffer.as_str(), "abc df");
        assert_eq!(buffer.selections(), &[Selection::caret(0), Selection::caret(5)]);
        assert_eq!(buffer.primary_selection(), Selection::caret(5));
    }

    #[test]
    fn merged_range_retains_primary_identity_and_active_end() {
        let mut buffer = Buffer::from("abcd");
        buffer.set_selections_with_primary(Selection::caret(1), [Selection::new(1, 3)]);
        assert_eq!(buffer.selections(), &[Selection::new(3, 1)]);
        assert_eq!(buffer.primary_selection(), Selection::new(3, 1));

        buffer.set_selections_with_primary(Selection::caret(3), [Selection::new(1, 3)]);
        assert_eq!(buffer.selections(), &[Selection::new(1, 3)]);
        assert_eq!(buffer.primary_selection(), Selection::new(1, 3));
    }

    #[test]
    fn inserts_at_multiple_carets_and_tracks_shifted_positions() {
        let mut buffer = Buffer::from("abcd");
        buffer.set_selections([Selection::caret(1), Selection::caret(3)]);
        buffer.insert("XY");
        assert_eq!(buffer.as_str(), "aXYbcXYd");
        assert_eq!(buffer.selections(), &[Selection::caret(3), Selection::caret(7)]);
    }

    #[test]
    fn replaces_reversed_and_forward_selections() {
        let mut buffer = Buffer::from("one two three");
        buffer.set_selections([Selection::new(3, 0), Selection::new(8, 13)]);
        buffer.replace_selections("x");
        assert_eq!(buffer.as_str(), "x two x");
        assert_eq!(buffer.selections(), &[Selection::caret(1), Selection::caret(7)]);
    }

    #[test]
    fn backspace_removes_one_combining_grapheme() {
        let mut buffer = Buffer::from("a\u{301}b");
        buffer.set_selection(Selection::caret("a\u{301}".len()));
        buffer.backspace(DeleteUnit::Grapheme);
        assert_eq!(buffer.as_str(), "b");
        assert_eq!(buffer.selections(), &[Selection::caret(0)]);
    }

    #[test]
    fn forward_delete_removes_one_zwj_emoji() {
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        let mut buffer = Buffer::from(format!("a{family}b"));
        buffer.set_selection(Selection::caret(1));
        buffer.delete_forward(DeleteUnit::Grapheme);
        assert_eq!(buffer.as_str(), "ab");
        assert_eq!(buffer.selections(), &[Selection::caret(1)]);
    }

    #[test]
    fn grapheme_movement_does_not_enter_a_combining_cluster() {
        let cluster = "a\u{301}";
        let mut buffer = Buffer::from(format!("x{cluster}y"));
        buffer.set_selection(Selection::caret(1));
        buffer.move_selections(Movement::NextGrapheme, false);
        assert_eq!(buffer.selections(), &[Selection::caret(1 + cluster.len())]);
        buffer.move_selections(Movement::PreviousGrapheme, false);
        assert_eq!(buffer.selections(), &[Selection::caret(1)]);
    }

    #[test]
    fn word_movement_skips_whitespace_and_punctuation() {
        let mut buffer = Buffer::from("one,   two three");
        buffer.set_selection(Selection::caret(0));
        buffer.move_selections(Movement::NextWord, false);
        assert_eq!(buffer.selections(), &[Selection::caret(3)]);
        buffer.move_selections(Movement::NextWord, false);
        assert_eq!(buffer.selections(), &[Selection::caret(10)]);
        buffer.move_selections(Movement::PreviousWord, false);
        assert_eq!(buffer.selections(), &[Selection::caret(7)]);
        buffer.move_selections(Movement::PreviousWord, false);
        assert_eq!(buffer.selections(), &[Selection::caret(0)]);
    }

    #[test]
    fn word_deletion_includes_intervening_separators() {
        let mut buffer = Buffer::from("one,   two");
        buffer.set_selection(Selection::caret(buffer.as_str().len()));
        buffer.backspace(DeleteUnit::Word);
        assert_eq!(buffer.as_str(), "one,   ");
        buffer.backspace(DeleteUnit::Word);
        assert_eq!(buffer.as_str(), "");
    }

    #[test]
    fn deletion_at_multiple_carets_is_one_edit() {
        let mut buffer = Buffer::from("abc def");
        buffer.set_selections([Selection::caret(1), Selection::caret(5)]);
        buffer.delete_forward(DeleteUnit::Grapheme);
        assert_eq!(buffer.as_str(), "ac df");
        assert_eq!(buffer.selections(), &[Selection::caret(1), Selection::caret(4)]);
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "abc def");
        assert_eq!(buffer.selections(), &[Selection::caret(1), Selection::caret(5)]);
    }

    #[test]
    fn line_and_document_movements_handle_multiline_text() {
        let mut buffer = Buffer::from("first\nsecond\nthird");
        buffer.set_selection(Selection::caret(9));
        buffer.move_selections(Movement::LineStart, false);
        assert_eq!(buffer.selections(), &[Selection::caret(6)]);
        buffer.move_selections(Movement::LineEnd, false);
        assert_eq!(buffer.selections(), &[Selection::caret(12)]);
        buffer.move_selections(Movement::DocumentStart, false);
        assert_eq!(buffer.selections(), &[Selection::caret(0)]);
        buffer.move_selections(Movement::DocumentEnd, false);
        assert_eq!(buffer.selections(), &[Selection::caret(18)]);
    }

    #[test]
    fn movement_can_extend_and_reverse_a_selection() {
        let mut buffer = Buffer::from("abc");
        buffer.set_selection(Selection::caret(2));
        buffer.move_selections(Movement::PreviousGrapheme, true);
        buffer.move_selections(Movement::PreviousGrapheme, true);
        assert_eq!(buffer.selections(), &[Selection::new(2, 0)]);
        buffer.move_selections(Movement::NextGrapheme, true);
        assert_eq!(buffer.selections(), &[Selection::new(2, 1)]);
    }

    #[test]
    fn horizontal_movement_collapses_existing_selection() {
        let mut buffer = Buffer::from("abcdef");
        buffer.set_selection(Selection::new(5, 2));
        buffer.move_selections(Movement::PreviousGrapheme, false);
        assert_eq!(buffer.selections(), &[Selection::caret(2)]);
        buffer.set_selection(Selection::new(1, 4));
        buffer.move_selections(Movement::NextWord, false);
        assert_eq!(buffer.selections(), &[Selection::caret(4)]);
    }

    #[test]
    fn select_all_selects_entire_document() {
        let mut buffer = Buffer::from("hello, world\nlast");
        buffer.select_all();
        assert_eq!(buffer.selections(), &[Selection::new(0, 17)]);
    }

    #[test]
    fn range_queries_do_not_change_selection() {
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        let text = format!("élan {family}\nnext");
        let mut buffer = Buffer::from(text);
        buffer.set_selection(Selection::caret(buffer.as_str().len()));
        let original = buffer.selections().to_vec();

        // Byte offset 1 is inside é; it is clamped to the word's start.
        assert_eq!(buffer.grapheme_range_at(1), 0.."é".len());
        assert_eq!(buffer.word_range_at(1), 0.."élan".len());
        let emoji_start = "élan ".len();
        assert_eq!(
            buffer.grapheme_range_at(emoji_start + 4),
            emoji_start..emoji_start + family.len()
        );
        assert_eq!(buffer.word_range_at(emoji_start + 4), emoji_start..emoji_start + family.len());
        assert_eq!(buffer.line_range_at(emoji_start), 0..emoji_start + family.len() + 1);
        assert_eq!(buffer.selections(), original);
    }

    #[test]
    fn explicit_range_replacement_clamps_and_remaps_selections() {
        let mut buffer = Buffer::from("aéz tail");
        buffer.set_selections([Selection::caret(0), Selection::caret(4)]);
        let inserted = buffer.replace_range(2..4, "XY");
        assert_eq!(inserted, 1..3);
        assert_eq!(buffer.as_str(), "aXY tail");
        assert_eq!(buffer.selections(), &[Selection::caret(0), Selection::caret(3)]);
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "aéz tail");
    }

    #[test]
    fn repeated_composition_updates_form_one_undo_transaction() {
        let mut buffer = Buffer::from("hello");
        buffer.select_all();
        buffer.update_composition("に", Selection::caret("に".len()));
        assert_eq!(buffer.marked_range(), Some(0.."に".len()));
        buffer.update_composition("日本", Selection::caret("日本".len()));
        assert_eq!(buffer.as_str(), "日本");
        assert_eq!(buffer.selections(), &[Selection::caret("日本".len())]);

        assert!(buffer.commit_composition());
        assert_eq!(buffer.marked_range(), None);
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "hello");
        assert_eq!(buffer.selections(), &[Selection::new(0, 5)]);
        assert!(!buffer.undo());
        assert!(buffer.redo());
        assert_eq!(buffer.as_str(), "日本");
    }

    #[test]
    fn composition_clamps_relative_unicode_selection() {
        let mut buffer = Buffer::from("x");
        buffer.select_all();
        buffer.update_composition("éx", Selection::new(1, 99));
        assert_eq!(buffer.as_str(), "éx");
        assert_eq!(buffer.selections(), &[Selection::new(0, 3)]);
        assert_eq!(buffer.marked_range(), Some(0..3));
    }

    #[test]
    fn composition_remaps_additional_selections() {
        let mut buffer = Buffer::from("abc xyz");
        buffer.set_selections([Selection::new(0, 3), Selection::caret(7)]);
        buffer.update_composition("hello", Selection::caret(5));
        assert_eq!(buffer.as_str(), "hello xyz");
        assert_eq!(buffer.selections(), &[Selection::caret(5), Selection::caret(9)]);
        buffer.update_composition("q", Selection::caret(1));
        assert_eq!(buffer.as_str(), "q xyz");
        assert_eq!(buffer.selections(), &[Selection::caret(1), Selection::caret(5)]);
        assert!(buffer.commit_composition());
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "abc xyz");
        assert_eq!(buffer.selections(), &[Selection::new(0, 3), Selection::caret(7)]);
    }

    #[test]
    fn composition_uses_primary_selection_instead_of_leftmost_selection() {
        let mut buffer = Buffer::from("left right");
        buffer.set_selections_with_primary(Selection::new(5, 10), [Selection::caret(0)]);
        assert_eq!(buffer.selections()[0], Selection::caret(0));
        assert_eq!(buffer.primary_selection(), Selection::new(5, 10));

        buffer.update_composition("日本", Selection::caret("日本".len()));
        assert_eq!(buffer.as_str(), "left 日本");
        assert_eq!(buffer.marked_range(), Some(5..11));
        assert_eq!(buffer.primary_selection(), Selection::caret(11));
        assert_eq!(buffer.selections()[0], Selection::caret(0));

        assert!(buffer.commit_composition());
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "left right");
        assert_eq!(buffer.primary_selection(), Selection::new(5, 10));
        assert_eq!(buffer.selections()[0], Selection::caret(0));
    }

    #[test]
    fn undo_and_redo_restore_text_and_selections() {
        let mut buffer = Buffer::from("abc");
        buffer.set_selection(Selection::new(1, 2));
        buffer.insert("XYZ");
        assert_eq!(buffer.as_str(), "aXYZc");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "abc");
        assert_eq!(buffer.selections(), &[Selection::new(1, 2)]);
        assert!(buffer.redo());
        assert_eq!(buffer.as_str(), "aXYZc");
        assert_eq!(buffer.selections(), &[Selection::caret(4)]);
    }

    #[test]
    fn grouped_typing_is_one_undo_transaction() {
        let mut buffer = Buffer::new();
        assert!(buffer.begin_edit_group());
        buffer.insert("a");
        buffer.insert("b");
        buffer.insert("c");
        assert_eq!(buffer.as_str(), "abc");
        assert!(buffer.commit_edit_group());

        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
        assert!(!buffer.undo());
        assert!(buffer.redo());
        assert_eq!(buffer.as_str(), "abc");
        assert_eq!(buffer.primary_selection(), Selection::caret(3));
    }

    #[test]
    fn edit_groups_do_not_nest_or_record_no_ops() {
        let mut buffer = Buffer::new();
        assert!(buffer.begin_edit_group());
        assert!(!buffer.begin_edit_group());
        assert!(buffer.commit_edit_group());
        assert!(!buffer.undo());
        assert!(!buffer.commit_edit_group());

        assert!(buffer.begin_edit_group());
        buffer.insert("");
        assert!(buffer.commit_edit_group());
        assert!(!buffer.undo());
    }

    #[test]
    fn movement_and_selection_changes_terminate_typing_group() {
        let mut buffer = Buffer::new();
        assert!(buffer.begin_edit_group());
        buffer.insert("a");
        buffer.insert("b");
        buffer.move_selections(Movement::PreviousGrapheme, false);
        assert_eq!(buffer.primary_selection(), Selection::caret(1));

        buffer.set_selection(Selection::caret(0));
        buffer.insert("X");
        assert_eq!(buffer.as_str(), "Xab");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "ab");
        assert_eq!(buffer.primary_selection(), Selection::caret(0));
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
        assert!(!buffer.undo());
    }

    #[test]
    fn deletion_terminates_typing_group() {
        let mut buffer = Buffer::new();
        assert!(buffer.begin_edit_group());
        buffer.insert("a");
        buffer.insert("b");
        buffer.backspace(DeleteUnit::Grapheme);
        assert_eq!(buffer.as_str(), "a");

        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "ab");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
    }

    #[test]
    fn composition_and_typing_groups_are_distinct_transactions() {
        let mut buffer = Buffer::new();
        assert!(buffer.begin_edit_group());
        buffer.insert("a");
        buffer.insert("b");
        buffer.update_composition("に", Selection::caret("に".len()));
        buffer.update_composition("日本", Selection::caret("日本".len()));
        assert!(buffer.commit_composition());
        assert_eq!(buffer.as_str(), "ab日本");

        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "ab");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
        assert!(buffer.redo());
        assert_eq!(buffer.as_str(), "ab");
        assert!(buffer.redo());
        assert_eq!(buffer.as_str(), "ab日本");
    }

    #[test]
    fn a_new_edit_discards_redo_history() {
        let mut buffer = Buffer::from("a");
        buffer.set_selection(Selection::caret(1));
        buffer.insert("b");
        assert!(buffer.undo());
        buffer.insert("c");
        assert_eq!(buffer.as_str(), "ac");
        assert!(!buffer.redo());
        assert_eq!(buffer.as_str(), "ac");
    }

    #[test]
    fn no_op_deletion_does_not_create_history() {
        let mut buffer = Buffer::from("abc");
        buffer.backspace(DeleteUnit::Grapheme);
        assert!(!buffer.undo());
        buffer.set_selection(Selection::caret(3));
        buffer.delete_forward(DeleteUnit::Grapheme);
        assert!(!buffer.undo());
    }

    // Brute-force reference implementations that scan the whole buffer from
    // position 0, mirroring what `previous_grapheme_boundary` and friends did
    // before they were bounded to a window around the offset. Used below to
    // check that the bounded versions never disagree with a full rescan.
    mod brute_force {
        use super::*;

        pub(super) fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
            let offset = clamp_offset(text, offset);
            text.grapheme_indices(true)
                .map(|(index, _)| index)
                .take_while(|index| *index < offset)
                .last()
                .unwrap_or(0)
        }

        pub(super) fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
            let offset = clamp_offset(text, offset);
            text.grapheme_indices(true)
                .map(|(index, _)| index)
                .find(|index| *index > offset)
                .unwrap_or(text.len())
        }

        pub(super) fn grapheme_range_at(text: &str, offset: usize) -> Range<usize> {
            let offset = clamp_offset(text, offset);
            for (start, grapheme) in text.grapheme_indices(true) {
                let end = start + grapheme.len();
                if start <= offset && offset < end {
                    return start..end;
                }
            }
            previous_grapheme_boundary(text, offset)..offset
        }

        pub(super) fn previous_word_boundary(text: &str, offset: usize) -> usize {
            let offset = clamp_offset(text, offset);
            text.unicode_word_indices()
                .map(|(start, _)| start)
                .take_while(|start| *start < offset)
                .last()
                .unwrap_or(0)
        }

        pub(super) fn next_word_boundary(text: &str, offset: usize) -> usize {
            let offset = clamp_offset(text, offset);
            text.unicode_word_indices()
                .map(|(start, word)| start + word.len())
                .find(|end| *end > offset)
                .unwrap_or(text.len())
        }

        pub(super) fn word_range_at(text: &str, offset: usize) -> Range<usize> {
            let offset = clamp_offset(text, offset);
            for (start, word) in text.unicode_word_indices() {
                let end = start + word.len();
                if start <= offset && offset < end {
                    return start..end;
                }
            }
            grapheme_range_at(text, offset)
        }
    }

    #[test]
    fn boundary_scans_match_a_brute_force_whole_buffer_scan_at_every_offset() {
        // ASCII, a combining mark, a ZWJ emoji sequence, CJK ideographs, and
        // newlines, so both the grapheme and the (line-bounded) word scans
        // are exercised across every kind of boundary they need to handle.
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        let text = format!("Hello, wor\u{301}ld!\n  {family} 你好 world\nlast\n");

        for offset in 0..=text.len() {
            assert_eq!(
                previous_grapheme_boundary(&text, offset),
                brute_force::previous_grapheme_boundary(&text, offset),
                "previous_grapheme_boundary mismatch at offset {offset}"
            );
            assert_eq!(
                next_grapheme_boundary(&text, offset),
                brute_force::next_grapheme_boundary(&text, offset),
                "next_grapheme_boundary mismatch at offset {offset}"
            );
            assert_eq!(
                grapheme_range_at(&text, offset),
                brute_force::grapheme_range_at(&text, offset),
                "grapheme_range_at mismatch at offset {offset}"
            );
            assert_eq!(
                previous_word_boundary(&text, offset),
                brute_force::previous_word_boundary(&text, offset),
                "previous_word_boundary mismatch at offset {offset}"
            );
            assert_eq!(
                next_word_boundary(&text, offset),
                brute_force::next_word_boundary(&text, offset),
                "next_word_boundary mismatch at offset {offset}"
            );
            assert_eq!(
                word_range_at(&text, offset),
                brute_force::word_range_at(&text, offset),
                "word_range_at mismatch at offset {offset}"
            );
        }
    }

    #[test]
    fn no_op_range_replacement_does_not_create_history() {
        let mut buffer = Buffer::from("abc");
        let inserted = buffer.replace_range(1..2, "b");
        assert_eq!(inserted, 1..2);
        assert_eq!(buffer.as_str(), "abc");
        assert!(!buffer.undo());
    }

    #[test]
    fn range_replacement_with_identical_bytes_still_remaps_a_caret() {
        let mut buffer = Buffer::from("abc");
        buffer.set_selection(Selection::caret(1));
        let inserted = buffer.replace_range(1..2, "b");
        assert_eq!(inserted, 1..2);
        assert_eq!(buffer.as_str(), "abc");
        assert_eq!(buffer.selections(), &[Selection::caret(2)]);
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "abc");
        assert_eq!(buffer.selections(), &[Selection::caret(1)]);
    }

    #[test]
    fn range_replacement_with_identical_bytes_still_remaps_a_selection() {
        let mut buffer = Buffer::from("abcabc");
        buffer.set_selection(Selection::new(0, 2));
        let inserted = buffer.replace_range(1..4, "bca");
        assert_eq!(inserted, 1..4);
        assert_eq!(buffer.as_str(), "abcabc");
        assert_eq!(buffer.selections(), &[Selection::new(0, 4)]);
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "abcabc");
        assert_eq!(buffer.selections(), &[Selection::new(0, 2)]);
    }
}

#[cfg(test)]
mod edit_delivery_tests {
    use super::*;

    #[test]
    fn tracked_drafts_fit_delivery_and_reject_oversized_mutations_without_filling_the_log() {
        // Large pastes deliver; repeated over-limit edits leave room to delete and recover.
        let mut buffer = Buffer::from("").track_edits();
        buffer.insert(&"é".repeat(MAX_TEXT_BYTES / 2));
        let payload =
            playbit_api::abi::text::TextInput::encode(buffer.as_str(), buffer.pending_edits())
                .expect("maximum draft is deliverable");
        assert!(payload.len() > 64 * 1024);
        buffer.clear_pending_edits();
        for _ in 0..=MAX_TEXT_EDITS {
            buffer.insert("x");
        }
        assert_eq!(buffer.as_str().len(), MAX_TEXT_BYTES);
        assert!(buffer.pending_edits().is_empty());
        buffer.select_all();
        buffer.backspace(DeleteUnit::Grapheme);
        assert_eq!(buffer.as_str(), "");
        assert_eq!(buffer.pending_edits().len(), 1);
        // A guest can present an oversized initial model. Keep it intact and allow replacement.
        let mut buffer = Buffer::from("a".repeat(MAX_TEXT_BYTES + 1)).track_edits();
        buffer.select_all();
        buffer.insert("Recovered");
        assert_eq!(buffer.as_str(), "Recovered");
        assert!(!buffer.undo(), "undo cannot reintroduce an undeliverable value");
    }

    #[test]
    fn native_edits_keep_actual_byte_positions_and_undo_ranges() {
        // Repeated text makes a string diff ambiguous; the native selection is authoritative.
        let mut buffer = Buffer::from("éaaa").track_edits();
        buffer.set_selection(Selection::caret(2));
        buffer.insert("a");
        assert_eq!(buffer.pending_edits(), &[TextEdit { start: 2, end: 2, inserted_len: 1 }]);
        buffer.clear_pending_edits();
        assert!(buffer.undo());
        assert_eq!(buffer.pending_edits(), &[TextEdit { start: 2, end: 3, inserted_len: 0 }]);
        buffer.clear_pending_edits();
        assert!(buffer.redo());
        assert_eq!(buffer.pending_edits(), &[TextEdit { start: 2, end: 2, inserted_len: 1 }]);
        buffer.clear_pending_edits();
        buffer.set_selection(Selection::new(0, 2));
        buffer.backspace(DeleteUnit::Grapheme);
        assert_eq!(buffer.pending_edits(), &[TextEdit { start: 0, end: 2, inserted_len: 0 }]);
    }

    #[test]
    fn multi_caret_and_composition_report_committed_native_operations() {
        // Multiple carets edit from right to left. IME updates publish only their final commit.
        let mut buffer = Buffer::from("aa🦀aa").track_edits();
        buffer.set_selections([Selection::caret(0), Selection::caret(6)]);
        buffer.insert("é");
        assert_eq!(
            buffer.pending_edits(),
            &[
                TextEdit { start: 6, end: 6, inserted_len: 2 },
                TextEdit { start: 0, end: 0, inserted_len: 2 },
            ]
        );
        buffer.clear_pending_edits();
        buffer.set_selection(Selection::new(2, 4));
        buffer.update_composition("に", Selection::caret(3));
        buffer.update_composition("日本", Selection::caret(6));
        assert!(buffer.pending_edits().is_empty());
        buffer.commit_composition();
        assert_eq!(buffer.pending_edits(), &[TextEdit { start: 2, end: 4, inserted_len: 6 }]);
    }

    #[test]
    fn rejected_delivery_retains_edits_and_bounds_further_mutation() {
        // A full delivery log preserves the draft instead of accepting an unreportable edit.
        let mut buffer = Buffer::from("").track_edits();
        for _ in 0..MAX_TEXT_EDITS {
            buffer.insert("a");
        }
        assert_eq!(buffer.pending_edits().len(), MAX_TEXT_EDITS);
        buffer.insert("b");
        assert_eq!(buffer.as_str().len(), MAX_TEXT_EDITS);
        buffer.clear_pending_edits();
        buffer.insert("b");
        assert_eq!(buffer.as_str().len(), MAX_TEXT_EDITS + 1);
    }
}
