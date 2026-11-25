//! GPUI presentation and platform-input adapter for `archipelago-text`.
//!
//! Editing semantics live in `archipelago-text`. This module owns only GPUI
//! focus, input-method, layout, painting, mouse, clipboard, and key dispatch.
//! GPUI's host text shaper is intentionally confined here: it is a replaceable
//! workbench bridge, not part of the portable text model or runtime renderer.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::desktop::{typography, zoom};
use crate::ui::text::{Buffer, DeleteUnit, Movement, Selection};
use gpui::{
    App, Bounds, BoxShadow, ClipboardItem, Context, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, Global,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, SharedString,
    Style, Subscription, Task, TextAlign, TextCheckingState, TextRun, Timer, UTF16Selection,
    UnderlineStyle, WeakEntity, Window, WrappedLine, actions, fill, font, point, prelude::*, px,
    relative, size,
};

#[cfg(target_os = "macos")]
mod context_menu;
pub(crate) mod hex;
mod text_checking;

const CARET_BLINK_DELAY: Duration = Duration::from_millis(500);
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(500);
const TYPING_UNDO_PAUSE: Duration = Duration::from_millis(350);

fn control_font() -> gpui::Font {
    gpui::Font { weight: gpui::FontWeight::MEDIUM, ..font(crate::ui::desktop::UI_FONT_FAMILY) }
}

fn tabular_font() -> gpui::Font {
    gpui::Font { features: typography::tabular_features(), ..control_font() }
}

actions!(
    archipelago_text_editor,
    [
        Backspace,
        DeleteForward,
        DeleteWordBackward,
        DeleteWordForward,
        DeleteToLineStart,
        DeleteToLineEnd,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        MoveWordLeft,
        MoveWordRight,
        MoveLineStart,
        MoveLineEnd,
        MoveDocumentStart,
        MoveDocumentEnd,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectLineStart,
        SelectLineEnd,
        SelectDocumentStart,
        SelectDocumentEnd,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        InsertNewline,
        Commit,
        ShowCharacterPalette,
    ]
);

pub type EditCallback =
    Arc<dyn Fn(&str, &[playbit_api::abi::text::TextEdit], u64, &mut App) -> bool + Send + Sync>;

pub type CommitCallback = Arc<dyn Fn(&str, u64, &mut App) -> bool + Send + Sync>;

#[derive(Default)]
struct EditRevision {
    current: u64,
    sent: u64,
    accepted: u64,
}
impl EditRevision {
    fn changed(&mut self) {
        self.current = self.current.checked_add(1).expect("text edit revision exhausted");
    }
    fn pending(&self) -> bool {
        self.current != self.accepted
    }
    fn accept(&mut self, revision: u64) -> bool {
        if revision < self.accepted || revision > self.current {
            return false;
        }
        self.accepted = revision;
        true
    }
}

#[derive(Clone)]
pub struct TextEditorStyle {
    pub background: Hsla,
    pub border: Hsla,
    pub border_focused: Hsla,
    pub text: Hsla,
    pub placeholder: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
    pub marked_text: Hsla,
    pub font_size: Pixels,
    pub letter_spacing: Pixels,
    pub line_height: Pixels,
    pub padding: Pixels,
    /// Optional trailing padding for a control embedded inside the field.
    pub trailing_padding: Option<Pixels>,
    pub corner_radius: Pixels,
    pub border_width: Pixels,
    pub border_width_focused: Pixels,
    /// Color of the enabled field's inset top shadow. `None` disables it.
    pub inner_shadow: Option<Hsla>,
}

impl Default for TextEditorStyle {
    fn default() -> Self {
        Self::for_theme(&playbit_api::ui::Theme::light())
    }
}

impl TextEditorStyle {
    fn scaled(&self, window: &Window) -> Self {
        let snap = |value: Pixels| zoom::pixels(window, value.into());
        let mut style = self.clone();
        style.font_size = snap(self.font_size);
        style.letter_spacing = zoom::scaled(window, self.letter_spacing.into());
        style.line_height = snap(self.line_height);
        style.padding = snap(self.padding);
        style.trailing_padding = self.trailing_padding.map(snap);
        style.corner_radius = snap(self.corner_radius);
        style.border_width = snap(self.border_width);
        style.border_width_focused = snap(self.border_width_focused);
        style
    }

    fn for_theme(theme: &playbit_api::ui::Theme) -> Self {
        use playbit_api::ui::ThemeColor as Role;
        let color = |role| gpui::Hsla::from(gpui::rgba(theme.color(role).0));
        Self {
            background: color(Role::ControlBackground),
            border: color(Role::Border),
            border_focused: color(Role::FocusRing),
            text: color(Role::Text),
            placeholder: color(Role::Text).alpha(0.4),
            selection: color(Role::TextSelection),
            caret: color(Role::Text),
            marked_text: color(Role::Text),
            font_size: px(11.0),
            letter_spacing: px(crate::ui::desktop::UI_CONTROL_TRACKING),
            line_height: px(16.0),
            padding: px(4.0),
            trailing_padding: None,
            corner_radius: px(2.0),
            border_width: px(0.5),
            border_width_focused: px(2.0),
            inner_shadow: Some(color(Role::Shadow).opacity(0.87)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragGranularity {
    Grapheme,
    Word,
    Paragraph,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaretAffinity {
    Upstream,
    Downstream,
}

#[derive(Clone, Copy, Debug)]
struct HitTest {
    /// The insertion boundary nearest the pointer.
    offset: usize,
    affinity: CaretAffinity,
    /// The rendered character under the pointer, independent of the nearest
    /// insertion boundary. `None` means the pointer is in blank row space.
    unit_offset: Option<usize>,
    /// The pointer is past the last rendered character on this row.
    after_text_on_row: bool,
}

#[derive(Clone, Debug)]
struct SelectionDrag {
    granularity: DragGranularity,
    origin: Selection,
    other_selections: Vec<Selection>,
}

#[derive(Clone)]
struct LayoutLine {
    start: usize,
    shaped: WrappedLine,
    hex_offset: Option<usize>,
}

#[derive(Clone)]
struct LayoutRow {
    line_index: usize,
    range: Range<usize>,
    top: Pixels,
    start_x: Pixels,
    newline_after: bool,
}

#[derive(Clone)]
struct LayoutSnapshot {
    bounds: Bounds<Pixels>,
    lines: Vec<LayoutLine>,
    rows: Vec<LayoutRow>,
    line_height: Pixels,
    scale: f32,
    content_height: Pixels,
    content_width: Pixels,
    scroll_x: Pixels,
    scroll_y: Pixels,
    text_len: usize,
    email_code: bool,
}

// Positions are relative to the editor's four-point content inset. Six
// 24-point boxes, four eight-point gaps and a 32-point group separator.
fn code_x(index: usize) -> Pixels {
    px([8., 40., 72., 128., 160., 192., 200.][index.min(6)])
}
impl LayoutSnapshot {
    fn code_x(&self, index: usize) -> Pixels {
        code_x(index) * self.scale
    }

    fn row_index_for_content_y(&self, y: Pixels) -> Option<usize> {
        self.rows.iter().position(|row| y < row.top + self.line_height)
    }

    fn hit_test(&self, position: Point<Pixels>) -> HitTest {
        if self.rows.is_empty() {
            return HitTest {
                offset: 0,
                affinity: CaretAffinity::Downstream,
                unit_offset: None,
                after_text_on_row: false,
            };
        }

        let local = point(
            position.x - self.bounds.left() + self.scroll_x,
            position.y - self.bounds.top() + self.scroll_y,
        );
        if local.y < Pixels::ZERO {
            return HitTest {
                offset: 0,
                affinity: CaretAffinity::Downstream,
                unit_offset: None,
                after_text_on_row: false,
            };
        }
        let Some(row_index) = self.row_index_for_content_y(local.y) else {
            return HitTest {
                offset: self.text_len,
                affinity: CaretAffinity::Downstream,
                unit_offset: None,
                after_text_on_row: false,
            };
        };
        if self.email_code {
            let offset = (0..=self.text_len)
                .min_by(|&a, &b| {
                    (local.x - self.code_x(a))
                        .abs()
                        .partial_cmp(&(local.x - self.code_x(b)).abs())
                        .expect("finite code positions")
                })
                .unwrap_or(0);
            return HitTest {
                offset,
                affinity: CaretAffinity::Downstream,
                unit_offset: (offset < self.text_len).then_some(offset),
                after_text_on_row: offset == self.text_len,
            };
        }
        let row = &self.rows[row_index];
        let line = &self.lines[row.line_index];
        let local_start = row.range.start - line.start;
        let local_end = row.range.end - line.start;
        let unwrapped_x = local.x + row.start_x;
        let index = line
            .shaped
            .unwrapped_layout
            .closest_index_for_x(unwrapped_x)
            .clamp(local_start, local_end);
        let offset = (line.start + index).min(self.text_len);
        let unit_offset = line
            .shaped
            .unwrapped_layout
            .index_for_x(unwrapped_x)
            .filter(|index| local_start <= *index && *index < local_end)
            .map(|index| line.start + index);
        let affinity = if offset == row.range.end
            && self.rows.get(row_index + 1).is_some_and(|next| next.range.start == offset)
        {
            CaretAffinity::Upstream
        } else {
            CaretAffinity::Downstream
        };
        let after_text_on_row = offset == row.range.end
            && row.range.start < row.range.end
            && unwrapped_x > line.shaped.unwrapped_layout.x_for_index(local_end);
        HitTest { offset, affinity, unit_offset, after_text_on_row }
    }

    fn row_for_offset(&self, offset: usize, affinity: CaretAffinity) -> Option<&LayoutRow> {
        let offset = offset.min(self.text_len);
        let contains = |row: &&LayoutRow| row.range.start <= offset && offset <= row.range.end;
        match affinity {
            CaretAffinity::Upstream => self.rows.iter().find(contains),
            CaretAffinity::Downstream => self.rows.iter().rev().find(contains),
        }
        .or_else(|| self.rows.last())
    }

    fn visual_row_range_at(&self, offset: usize, affinity: CaretAffinity) -> Range<usize> {
        self.row_for_offset(offset, affinity).map(|row| row.range.clone()).unwrap_or(0..0)
    }

    fn x_for_offset_in_row(&self, row: &LayoutRow, offset: usize) -> Pixels {
        if self.email_code {
            return self.code_x(offset);
        }
        let line = &self.lines[row.line_index];
        let local = offset.clamp(row.range.start, row.range.end) - line.start;
        line.shaped.unwrapped_layout.x_for_index(local) - row.start_x
    }

    fn bounds_for_row_range(&self, row: &LayoutRow, range: Range<usize>) -> Option<Bounds<Pixels>> {
        let start = range.start.max(row.range.start);
        let end = range.end.min(row.range.end);
        let includes_newline =
            row.newline_after && range.start <= row.range.end && range.end > row.range.end;
        if start >= end && !includes_newline {
            return None;
        }
        let left = self.x_for_offset_in_row(row, start);
        let mut right = self.x_for_offset_in_row(row, end);
        if includes_newline {
            right = right.max(self.x_for_offset_in_row(row, row.range.end) + px(4.0));
        }
        Some(Bounds::from_corners(
            point(
                self.bounds.left() + left.min(right) - self.scroll_x,
                self.bounds.top() + row.top - self.scroll_y,
            ),
            point(
                self.bounds.left() + left.max(right) - self.scroll_x,
                self.bounds.top() + row.top + self.line_height - self.scroll_y,
            ),
        ))
    }

    fn position_for_offset(&self, offset: usize, affinity: CaretAffinity) -> Option<Point<Pixels>> {
        let offset = offset.min(self.text_len);
        if self.email_code {
            return Some(point(self.bounds.left() + self.code_x(offset), self.bounds.top()));
        }
        if self.rows.is_empty() && offset == 0 {
            return Some(point(
                self.bounds.left() - self.scroll_x,
                self.bounds.top() - self.scroll_y,
            ));
        }
        let row = self.row_for_offset(offset, affinity)?;
        Some(point(
            self.bounds.left() + self.x_for_offset_in_row(row, offset) - self.scroll_x,
            self.bounds.top() + row.top - self.scroll_y,
        ))
    }

    fn rectangles_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        if range.is_empty() {
            return Vec::new();
        }
        if self.email_code {
            return (range.start..range.end.min(self.text_len))
                .map(|i| {
                    Bounds::new(
                        point(
                            self.bounds.left() + self.code_x(i) - self.line_height / 2.,
                            self.bounds.top(),
                        ),
                        size(self.line_height * 1.5, self.line_height),
                    )
                })
                .collect();
        }
        let mut rectangles = Vec::new();
        for row in &self.rows {
            if let Some(rectangle) = self.bounds_for_row_range(row, range.clone()) {
                rectangles.push(rectangle);
            }
        }
        rectangles
    }

    fn first_bounds_for_range(
        &self,
        range: Range<usize>,
        affinity: CaretAffinity,
    ) -> Option<Bounds<Pixels>> {
        self.first_bounds_with_actual_range(range, affinity).map(|(bounds, _)| bounds)
    }

    fn first_bounds_with_actual_range(
        &self,
        range: Range<usize>,
        affinity: CaretAffinity,
    ) -> Option<(Bounds<Pixels>, Range<usize>)> {
        if range.is_empty() {
            let position = self.position_for_offset(range.start, affinity)?;
            return Some((Bounds::new(position, size(px(1.0), self.line_height)), range));
        }
        if self.email_code {
            let actual = range.start..range.end.min(range.start + 1).min(self.text_len);
            let bounds = self.rectangles_for_range(actual.clone()).into_iter().next()?;
            return Some((bounds, actual));
        }
        self.rows.iter().find_map(|row| {
            let bounds = self.bounds_for_row_range(row, range.clone())?;
            let actual = range.start.max(row.range.start)
                ..range.end.min(row.range.end + usize::from(row.newline_after));
            Some((bounds, actual))
        })
    }
}

fn selection_for_pointer(buffer: &Buffer, hit: HitTest, granularity: DragGranularity) -> Selection {
    let range = match granularity {
        DragGranularity::Grapheme => hit.unit_offset.map(|offset| buffer.grapheme_range_at(offset)),
        DragGranularity::Word => hit
            .unit_offset
            .or_else(|| {
                if !hit.after_text_on_row {
                    return None;
                }
                let line_start = buffer.line_range_at(hit.offset).start;
                buffer.as_str()[line_start..hit.offset]
                    .unicode_word_indices()
                    .next_back()
                    .map(|(offset, _)| line_start + offset)
            })
            .map(|offset| buffer.word_range_at(offset)),
        DragGranularity::Paragraph => {
            Some(buffer.line_range_at(hit.unit_offset.unwrap_or(hit.offset)))
        }
    };
    range
        .map(|range| Selection::new(range.start, range.end))
        .unwrap_or_else(|| Selection::caret(hit.offset))
}

fn selection_for_click(
    buffer: &Buffer,
    hit: HitTest,
    clicks: usize,
) -> (DragGranularity, Selection) {
    match clicks {
        2 => (DragGranularity::Word, selection_for_pointer(buffer, hit, DragGranularity::Word)),
        3.. => (DragGranularity::Paragraph, Selection::new(0, buffer.as_str().len())),
        _ => (DragGranularity::Grapheme, Selection::caret(hit.offset)),
    }
}

#[derive(Clone, Copy)]
struct LastTyping {
    at: Instant,
    ended_in_whitespace: bool,
}

fn split_typing_group(
    buffer: &mut Buffer,
    last_typing: &mut Option<LastTyping>,
    range: &Option<Range<usize>>,
    text: &str,
    now: Instant,
) {
    let typing = !text.is_empty()
        && buffer.marked_range().is_none()
        && range.as_ref().is_none_or(|range| *range == buffer.primary_selection().range());
    if !typing {
        *last_typing = None;
        return;
    }
    if last_typing.is_some_and(|previous| {
        now.saturating_duration_since(previous.at) >= TYPING_UNDO_PAUSE
            || (previous.ended_in_whitespace
                && text.chars().next().is_some_and(|character| !character.is_whitespace()))
    }) {
        buffer.commit_edit_group();
    }
    *last_typing = Some(LastTyping {
        at: now,
        ended_in_whitespace: text.chars().last().is_some_and(char::is_whitespace),
    });
}

fn clear_selections_for_focus_change(buffer: &mut Buffer) -> bool {
    let caret = Selection::caret(buffer.primary_selection().head);
    if buffer.selections() == [caret] {
        return false;
    }
    buffer.set_selection(caret);
    true
}

fn caret_blink_state(elapsed: Duration) -> (bool, Duration) {
    if elapsed < CARET_BLINK_DELAY {
        return (true, CARET_BLINK_DELAY - elapsed);
    }
    let phase_elapsed = elapsed - CARET_BLINK_DELAY;
    let interval_nanos = CARET_BLINK_INTERVAL.as_nanos();
    let phase = phase_elapsed.as_nanos() / interval_nanos;
    let remainder = phase_elapsed.as_nanos() % interval_nanos;
    let visible = phase % 2 == 1;
    let until_transition = Duration::from_nanos((interval_nanos - remainder) as u64);
    (visible, until_transition)
}

pub struct TextEditor {
    buffer: Buffer,
    enabled: bool,
    read_only: bool,
    selectable_id: bool,
    message_scroll: Option<gpui::ScrollHandle>,
    max_bytes: Option<usize>,
    focus_handle: FocusHandle,
    keyboard_focus: bool,
    base_style: TextEditorStyle,
    masked: bool,
    email_code: bool,
    tabular_numerals: bool,
    inspector_field: bool,
    text_area: bool,
    hex: Option<hex::Format>,
    canonical_hex: bool,
    hex_input: hex::InputMode,
    validation_underline: Option<Range<usize>>,
    literal: bool,
    checking_inserted_range: Option<Range<usize>>,
    checking_cache: Option<text_checking::StateCache>,
    style: TextEditorStyle,
    layout: Option<LayoutSnapshot>,
    selection_drag: Option<SelectionDrag>,
    scroll_x: Pixels,
    scroll_y: Pixels,
    scroll_to_caret: bool,
    accessibility_scroll: bool,
    primary_affinity: CaretAffinity,
    preferred_x: Option<Pixels>,
    caret_activity: Instant,
    caret_visible: bool,
    caret_blink_active: bool,
    caret_clock: Option<CaretClock>,
    last_typing: Option<LastTyping>,
    placeholder: Option<SharedString>,
    on_commit: Option<CommitCallback>,
    on_blur: Option<CommitCallback>,
    on_change: Option<CommitCallback>,
    on_edit: Option<EditCallback>,
    revision: EditRevision,
    model_generation: u64,
    focus_subscription: Option<Subscription>,
    blur_subscription: Option<Subscription>,
}

#[derive(Default)]
struct TextSelectionCoordinator {
    active: Option<WeakEntity<TextEditor>>,
}

impl Global for TextSelectionCoordinator {}

struct CaretClock {
    _task: Task<()>,
    activity: async_channel::Sender<()>,
}

impl TextEditor {
    pub fn new(text: impl Into<String>, cx: &mut Context<Self>) -> Self {
        let text = text.into().replace(['\r', '\n'], "");
        Self {
            buffer: Buffer::from(text),
            enabled: true,
            read_only: false,
            selectable_id: false,
            message_scroll: None,
            max_bytes: None,
            focus_handle: cx.focus_handle().tab_stop(true),
            keyboard_focus: false,
            base_style: TextEditorStyle::default(),
            masked: false,
            email_code: false,
            tabular_numerals: false,
            inspector_field: false,
            text_area: false,
            hex: None,
            canonical_hex: false,
            hex_input: hex::InputMode::Overwrite,
            validation_underline: None,
            literal: false,
            checking_inserted_range: None,
            checking_cache: None,
            style: TextEditorStyle::default(),
            layout: None,
            selection_drag: None,
            scroll_x: Pixels::ZERO,
            scroll_y: Pixels::ZERO,
            scroll_to_caret: true,
            accessibility_scroll: false,
            primary_affinity: CaretAffinity::Downstream,
            preferred_x: None,
            caret_activity: Instant::now(),
            caret_visible: true,
            caret_blink_active: false,
            caret_clock: None,
            last_typing: None,
            placeholder: None,
            on_commit: None,
            on_blur: None,
            on_change: None,
            on_edit: None,
            revision: EditRevision::default(),
            model_generation: 0,
            focus_subscription: None,
            blur_subscription: None,
        }
    }

    /// Account keys are ASCII; keep layout offsets identical while masking all
    /// platform/AX text and disabling clipboard export of the private draft.
    pub fn account_key(mut self) -> Self {
        self.masked = true;
        self
    }

    /// Use equal-width digits in otherwise proportional field text.
    #[cfg(all(target_os = "macos", feature = "connected-engine"))]
    pub(crate) fn tabular_numerals(mut self) -> Self {
        self.tabular_numerals = true;
        self
    }
    /// One native editor owns all six digits, including grouped clipboard input.
    pub fn email_code(mut self) -> Self {
        self.email_code = true;
        self.base_style.font_size = px(16.);
        self
    }
    /// Literal values such as email addresses and paths bypass native writing assistance.
    pub fn literal(mut self) -> Self {
        self.literal = true;
        self
    }
    fn display_text(&self) -> String {
        if self.masked {
            "*".repeat(self.buffer.as_str().len())
        } else {
            self.buffer.as_str().to_owned()
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    /// Bound browser drafts before mutation, including paste and composition.
    pub fn bound_edits(mut self, max_bytes: usize) -> Self {
        self.buffer = self.buffer.bound_edits(max_bytes);
        self.max_bytes = Some(max_bytes);
        self
    }

    fn fresh_buffer(&self, text: String) -> Buffer {
        let buffer = Buffer::from(text);
        let buffer = if self.on_edit.is_some() { buffer.track_edits() } else { buffer };
        match self.max_bytes {
            Some(limit) => buffer.bound_edits(limit),
            None => buffer,
        }
    }
    pub fn set_placeholder(&mut self, placeholder: &str) {
        if self.placeholder.as_ref().map(|text| text.as_ref()) != Some(placeholder) {
            self.placeholder = Some(placeholder.to_owned().into());
        }
    }

    /// Reserve logical points for an in-field trailing accessory before text layout.
    pub fn set_trailing_padding(&mut self, padding: Pixels) {
        assert!(padding >= px(0.), "text accessory padding cannot be negative");
        self.base_style.trailing_padding = Some(padding);
    }

    /// Keep ordinary focus, selection and Copy while rejecting user edits.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
    /// Opaque metadata stays plain text while retaining native selection and Copy.
    pub fn selectable_id(mut self) -> Self {
        self.read_only = true;
        self.literal = true;
        self.selectable_id = true;
        self.focus_handle = self.focus_handle.clone().tab_stop(false);
        self.base_style.background = gpui::transparent_black();
        self.base_style.border_width = px(0.);
        self.base_style.border_width_focused = px(0.);
        self.base_style.padding = px(0.);
        self.base_style.corner_radius = px(0.);
        self.base_style.inner_shadow = None;
        self.base_style.selection = gpui::rgba(0xb3d7ffff).into();
        self
    }
    /// Wrapped, selectable status text uses the owner's shared scroll viewport.
    pub fn read_only_message(mut self, scroll: gpui::ScrollHandle) -> Self {
        self.read_only = true;
        self.message_scroll = Some(scroll);
        self.base_style.background = gpui::transparent_black();
        self.base_style.border_width = px(0.);
        self.base_style.padding = px(0.);
        self.base_style.inner_shadow = None;
        self.base_style.text = gpui::rgba(0x000000e6).into();
        self.base_style.selection = gpui::rgba(0xb3d7ffff).into();
        self.base_style.letter_spacing = px(0.);
        self
    }
    pub fn set_read_only(
        &mut self,
        read_only: bool,
        theme: &playbit_api::ui::Theme,
        cx: &mut Context<Self>,
    ) {
        if self.read_only != read_only {
            self.read_only = read_only;
            self.set_theme(theme);
            if read_only {
                self.revision.accepted = self.revision.current;
                self.revision.sent = self.revision.current;
            }
            cx.notify();
        }
        if read_only && !self.text_area {
            use playbit_api::ui::ThemeColor as Role;
            self.base_style.background =
                gpui::Hsla::from(gpui::rgba(theme.color(Role::PanelBackground).0));
            self.base_style.border =
                gpui::Hsla::from(gpui::rgba(theme.color(Role::Text).0)).alpha(0.2);
            self.base_style.inner_shadow = None;
            self.base_style.border_width = px(1.);
        }
    }
    /// The account form uses the shared custom field, with native selection color.
    pub fn account_dialog(&mut self) {
        self.base_style.selection = gpui::rgba(0xb3d7ffff).into();
        self.base_style.text = gpui::rgba(0x000000b8).into();
        self.base_style.padding = px(3.);
        self.base_style.letter_spacing = px(0.);
        self.base_style.corner_radius = px(1.);
        self.base_style.inner_shadow = None;
    }
    /// A compact name field fits a navigation row without changing its height.
    pub fn inline_name(mut self) -> Self {
        self.account_dialog();
        self.base_style.font_size = px(14.);
        self.base_style.line_height = px(20.);
        self.base_style.text = gpui::black();
        self.base_style.padding = px(2.);
        self.base_style.corner_radius = px(2.);
        self.base_style.border_width = px(2.);
        self.base_style.border_width_focused = px(2.);
        self.base_style.border = gpui::rgba(0x000000e6).into();
        self.base_style.border_focused = gpui::rgba(0x000000e6).into();
        self
    }
    /// Compact fields used by the management inspector.
    pub fn inspector_field(mut self) -> Self {
        self.inspector_field = true;
        self.apply_inspector_style();
        self
    }
    /// Editable values retain newlines inside one fixed, scrolling viewport.
    pub fn inspector_text_area(text: impl Into<String>, cx: &mut Context<Self>) -> Self {
        let mut editor = Self::new("", cx).inspector_field().literal();
        editor.buffer = Buffer::from(text.into());
        editor.text_area = true;
        editor.apply_inspector_style();
        editor
    }
    pub fn set_binary(&mut self, binary: bool, cx: &mut Context<Self>) {
        let next = binary.then_some(if self.text_area {
            hex::Format::Grouped
        } else {
            hex::Format::Compact
        });
        if self.hex != next {
            self.hex = next;
            self.hex_input = hex::InputMode::Overwrite;
            self.refresh_hex_format();
            self.model_generation =
                self.model_generation.checked_add(1).expect("text model generation exhausted");
            self.layout = None;
            self.scroll_x = Pixels::ZERO;
            self.scroll_to_caret = true;
            cx.notify();
        }
    }
    fn text_font(&self) -> gpui::Font {
        if self.selectable_id {
            gpui::Font { features: typography::identifier_features(), ..control_font() }
        } else if self.hex.is_some() {
            font("Menlo")
        } else if self.email_code || self.tabular_numerals {
            tabular_font()
        } else {
            control_font()
        }
    }
    fn apply_inspector_style(&mut self) {
        self.account_dialog();
        self.base_style.font_size = px(11.);
        self.base_style.line_height = px(16.);
        self.base_style.padding = px(4.);
        self.base_style.corner_radius = px(2.);
        self.base_style.border_width = px(0.5);
        self.base_style.text = gpui::rgba(0x000000e6).into();
        if self.text_area {
            self.base_style.background = gpui::white();
        }
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
    pub fn set_theme(&mut self, theme: &playbit_api::ui::Theme) {
        self.base_style = TextEditorStyle::for_theme(theme);
        if self.email_code {
            self.base_style.font_size = px(16.);
        }
        if self.inspector_field {
            self.apply_inspector_style();
        }
    }
    pub fn text(&self) -> &str {
        self.buffer.as_str()
    }
    pub fn is_composing(&self) -> bool {
        self.buffer.marked_range().is_some()
    }
    /// Owner-provided input feedback; independent of native composition.
    pub fn set_validation_underline(
        &mut self,
        range: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .filter(|range| !range.is_empty() && self.buffer.as_str().get(range.clone()).is_some());
        if self.validation_underline != range {
            self.validation_underline = range;
            cx.notify();
        }
    }
    /// Start a fresh transient input, such as a newly requested sign-in code.
    /// Runtime snapshots use acknowledge instead, preserving pending edits/IME.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.buffer = self.fresh_buffer(String::new());
        self.hex_input.finish_byte();
        self.refresh_hex_format();
        self.validation_underline = None;
        self.layout = None;
        self.revision.changed();
        self.checking_inserted_range = None;
        self.checking_cache = None;
        // A fresh host-owned input has no edit awaiting a runtime acknowledgment.
        // Retain the revision floor so replies to its previous use stay stale.
        self.revision.accepted = self.revision.current;
        self.revision.sent = self.revision.current;
        self.scroll_to_caret = true;
        cx.notify();
    }
    pub fn acknowledge(&mut self, revision: Option<u64>, value: &str, cx: &mut Context<Self>) {
        if let Some(revision) = revision
            && !self.revision.accept(revision)
        {
            return;
        }
        if !self.revision.pending()
            && self.buffer.marked_range().is_none()
            && self.buffer.as_str() != value
            && self.max_bytes.is_none_or(|limit| value.len() <= limit)
        {
            self.buffer = self.fresh_buffer(value.to_owned());
            self.hex_input.finish_byte();
            self.refresh_hex_format();
            self.validation_underline = None;
            // Model replacements do not advance the local edit/acknowledgment
            // sequence, but must retire actions captured against the old value.
            self.model_generation =
                self.model_generation.checked_add(1).expect("text model generation exhausted");
            self.checking_inserted_range = None;
            self.checking_cache = None;
            self.layout = None;
            self.scroll_to_caret = true;
            cx.notify();
        }
    }

    pub fn on_commit(mut self, callback: CommitCallback) -> Self {
        self.on_commit = Some(callback);
        self
    }
    /// Inline names commit on focus loss as well as Return. Owners defer any
    /// editor replacement until this callback has released its entity lease.
    pub fn on_blur(mut self, callback: CommitCallback) -> Self {
        self.on_blur = Some(callback);
        self
    }
    pub fn on_change(mut self, callback: CommitCallback) -> Self {
        self.on_change = Some(callback);
        self
    }
    pub fn on_edit(mut self, callback: EditCallback) -> Self {
        self.buffer = self.buffer.track_edits();
        self.on_edit = Some(callback);
        self
    }

    /// Replace only the delivery owner. Preserve private text and mark it
    /// undelivered; binding a fresh owner never submits the draft.
    pub fn rebind_delivery(&mut self, change: EditCallback, commit: CommitCallback) {
        self.on_edit = Some(change);
        self.on_commit = Some(commit);
        self.invalidate_delivery();
    }
    /// A fresh VM has not received even an acknowledged field value. Preserve
    /// text, selection and composition, rejecting acknowledgments from its predecessor.
    pub fn invalidate_delivery(&mut self) {
        self.revision.changed();
        self.revision.sent = self.revision.accepted;
    }
    pub fn retry_draft(&mut self) {
        self.revision.sent = self.revision.accepted;
    }

    /// Retry only an undelivered local draft before a later button action.
    /// Already queued drafts preserve FIFO order; persistent actions never retry.
    pub fn send_draft(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.enabled || self.read_only {
            return false;
        }
        if self.on_edit.is_none() && self.on_change.is_none() {
            return true;
        }
        if self.buffer.marked_range().is_some() {
            return false;
        }
        if self.revision.sent == self.revision.current {
            return true;
        }
        let accepted = if let Some(callback) = &self.on_edit {
            callback(self.buffer.as_str(), self.buffer.pending_edits(), self.revision.current, cx)
        } else {
            self.on_change.as_ref().expect("delivery callback")(
                self.buffer.as_str(),
                self.revision.current,
                cx,
            )
        };
        if !accepted {
            return false;
        }
        self.buffer.clear_pending_edits();
        self.revision.sent = self.revision.current;
        true
    }

    /// Selects the complete buffer synchronously. Focus traversal uses this
    /// after moving focus so the selection is visible in the same frame.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.buffer.select_all();
        self.accessibility_scroll = true;
        self.scroll_to_caret = true;
        self.primary_affinity = CaretAffinity::Downstream;
        self.reset_caret_blink();
        cx.notify();
    }

    /// Read-only inspector fields retain visible keyboard focus and ordinary Copy.
    pub fn focus_from_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled || self.is_composing() {
            return;
        }
        self.keyboard_focus = true;
        self.focus_handle.focus(window);
        self.select_all(cx);
    }

    fn did_change(&mut self, cx: &mut Context<Self>) {
        self.hex_input.finish_byte();
        if self.hex.is_some() && !self.canonical_hex {
            self.refresh_hex_format();
        }
        self.validation_underline = None;
        self.revision.changed();
        self.checking_inserted_range = None;
        self.checking_cache = None;
        self.send_draft(cx);
        self.preferred_x = None;
        self.scroll_to_caret = true;
        self.primary_affinity = CaretAffinity::Downstream;
        self.reset_caret_blink();
        cx.notify();
    }

    fn accessibility_action(
        &mut self,
        request: &gpui::accesskit::ActionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui::accesskit::{Action, ActionData};
        #[cfg(target_os = "macos")]
        if super::popup::is_modal_active() {
            return;
        }
        if !self.enabled {
            return;
        }
        match (&request.action, &request.data) {
            (Action::Focus, _) => {
                self.focus_handle.focus(window);
                self.accessibility_scroll = true;
                cx.notify();
            }
            (Action::SetValue, Some(ActionData::Value(value))) => {
                if self.read_only {
                    return;
                }
                let len = self.buffer.as_str().len();
                if let Some(format) = self.hex_format() {
                    if value.is_empty() {
                        self.delete_hex_selection(format, 0..len, cx);
                    } else {
                        self.paste_hex(0..len, value, cx);
                    }
                    return;
                }
                let Some(text) = self.input_text(value) else { return };
                replace_platform_text(&mut self.buffer, Some(0..len), &text);
                self.did_change(cx);
            }
            (Action::SetTextSelection, Some(ActionData::SetTextSelection(selection))) => {
                let run = accessibility_text_ids(cx.entity_id()).1;
                if selection.anchor.node != run || selection.focus.node != run {
                    return;
                }
                let Some(anchor) =
                    accessibility_offset(self.buffer.as_str(), selection.anchor.character_index)
                else {
                    return;
                };
                let Some(head) =
                    accessibility_offset(self.buffer.as_str(), selection.focus.character_index)
                else {
                    return;
                };
                self.focus_handle.focus(window);
                self.buffer.set_selection(self.hex_selection(Selection::new(anchor, head)));
                self.scroll_to_caret = true;
                self.accessibility_scroll = true;
                self.primary_affinity = CaretAffinity::Downstream;
                self.reset_caret_blink();
                cx.notify();
            }
            _ => {}
        }
    }

    fn reset_caret_blink(&mut self) {
        self.caret_activity = Instant::now();
        self.caret_visible = true;
        if let Some(clock) = &self.caret_clock {
            // One coalesced activity notification replaces the pending blink
            // deadline, without allocating a task for every input event.
            let _ = clock.activity.try_send(());
        }
    }

    fn update_caret_blink(&mut self, now: Instant, cx: &mut Context<Self>) -> Option<Duration> {
        if !self.caret_blink_active {
            return None;
        }
        let elapsed = now.saturating_duration_since(self.caret_activity);
        let (visible, until_transition) = caret_blink_state(elapsed);
        if visible != self.caret_visible {
            self.caret_visible = visible;
            crate::trace::mark("ui.caret.transition");
            cx.notify();
        }
        Some(until_transition)
    }

    fn did_focus(&mut self, cx: &mut Context<Self>) {
        // Selection is changed by explicit Tab navigation, never by activation.
        self.scroll_to_caret = true;
        let current = cx.weak_entity();
        let previous =
            cx.default_global::<TextSelectionCoordinator>().active.replace(current.clone());
        if let Some(previous) = previous.filter(|previous| *previous != current) {
            _ = previous.update(cx, |editor, cx| {
                editor.stop_caret_clock();
                editor.clear_selection_for_focus_change(cx);
            });
        }
        self.stop_caret_clock();
        if self.read_only || !self.enabled {
            cx.notify();
            return;
        }
        self.caret_blink_active = true;
        self.reset_caret_blink();
        let (activity, changes) = async_channel::bounded(1);
        let task = cx.spawn(async move |editor, cx| {
            let mut delay = CARET_BLINK_DELAY;
            loop {
                let deadline = {
                    let mut timer = pin!(Timer::after(delay));
                    let mut change = pin!(changes.recv());
                    poll_fn(|cx| {
                        match change.as_mut().poll(cx) {
                            Poll::Ready(Ok(())) => return Poll::Ready(Some(false)),
                            Poll::Ready(Err(_)) => return Poll::Ready(None),
                            Poll::Pending => {}
                        }
                        timer.as_mut().poll(cx).map(|_| Some(true))
                    })
                    .await
                }; // Drop the old timer before scheduling its replacement.
                let Some(deadline) = deadline else {
                    break;
                };
                if deadline {
                    crate::trace::mark("ui.caret.deadline");
                }
                match editor.update(cx, |editor, cx| editor.update_caret_blink(Instant::now(), cx))
                {
                    Ok(Some(next)) => delay = next,
                    Ok(None) | Err(_) => break,
                }
            }
        });
        self.caret_clock = Some(CaretClock { _task: task, activity });
        crate::trace::mark("ui.caret.start");
        cx.notify();
    }

    fn stop_caret_clock(&mut self) {
        self.caret_blink_active = false;
        if self.caret_clock.take().is_some() {
            crate::trace::mark("ui.caret.stop");
        }
    }

    fn clear_selection_for_focus_change(&mut self, cx: &mut Context<Self>) {
        // The shared NSView will discard its old conversion session after
        // focus moves. Commit even when the composition selection is a caret.
        let inserted = self.buffer.marked_range();
        if self.buffer.commit_composition() {
            self.checking_inserted_range = inserted.filter(|range| !range.is_empty());
            self.checking_cache = None;
            self.send_draft(cx);
            cx.notify();
        }
        if clear_selections_for_focus_change(&mut self.buffer) {
            self.preferred_x = None;
            self.primary_affinity = CaretAffinity::Downstream;
            cx.notify();
        }
    }

    fn input_text(&self, text: &str) -> Option<String> {
        if self.masked && !text.is_ascii() {
            return None;
        }
        if self.email_code {
            if text.len() > 32 {
                return None;
            }
            let text: String = text.chars().filter(|c| !matches!(c, ' ' | '-' | '–')).collect();
            return (text.len() <= 6 && text.bytes().all(|c| c.is_ascii_digit())).then_some(text);
        }
        if self.max_bytes.is_some_and(|limit| text.len() > limit) {
            return None;
        }
        Some(if self.text_area { text.to_owned() } else { text.replace(['\r', '\n'], "") })
    }

    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        if self.hex.is_some() || !self.text_area {
            self.commit(cx);
        } else if self.enabled && !self.read_only && self.buffer.marked_range().is_none() {
            self.buffer.commit_edit_group();
            self.buffer.replace_selections("\n");
            self.did_change(cx);
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if self.buffer.marked_range().is_some() {
            return;
        }
        self.buffer.commit_edit_group();
        if let Some(callback) = &self.on_commit
            && callback(self.buffer.as_str(), self.revision.current, cx)
        {
            self.buffer.clear_pending_edits();
            self.revision.sent = self.revision.current;
        }
    }

    fn move_logically(&mut self, movement: Movement, extend: bool, cx: &mut Context<Self>) {
        if let Some(selection) = self.hex_format().and_then(|format| {
            format.movement(self.text(), self.buffer.primary_selection(), movement, extend)
        }) {
            self.buffer.set_selection(selection);
        } else {
            self.buffer.move_selections(movement, extend);
            if self.hex_format().is_some() {
                self.buffer.set_selection(self.hex_selection(self.buffer.primary_selection()));
            }
        }
        self.preferred_x = None;
        self.scroll_to_caret = true;
        self.primary_affinity = CaretAffinity::Downstream;
        self.reset_caret_blink();
        cx.notify();
    }

    fn move_vertically(&mut self, upward: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.as_ref() else {
            self.move_logically(
                if upward { Movement::LineStart } else { Movement::LineEnd },
                extend,
                cx,
            );
            return;
        };
        let preferred_x = self.preferred_x.or_else(|| {
            layout
                .position_for_offset(self.buffer.primary_selection().head, self.primary_affinity)
                .map(|position| position.x)
        });
        let Some(preferred_x) = preferred_x else {
            return;
        };
        let old_primary = self.buffer.primary_selection();
        let mut primary = None;
        let mut primary_affinity = self.primary_affinity;
        let mut additional = Vec::with_capacity(self.buffer.selections().len().saturating_sub(1));
        for selection in self.buffer.selections() {
            let affinity = if *selection == old_primary {
                self.primary_affinity
            } else {
                CaretAffinity::Downstream
            };
            let Some(position) = layout.position_for_offset(selection.head, affinity) else {
                if *selection == old_primary {
                    primary = Some(*selection);
                } else {
                    additional.push(*selection);
                }
                continue;
            };
            let target = layout.hit_test(point(
                preferred_x,
                if upward {
                    position.y - layout.line_height
                } else {
                    position.y + layout.line_height
                },
            ));
            let moved = if extend {
                self.hex_selection(Selection::new(selection.anchor, target.offset))
            } else {
                Selection::caret(target.offset)
            };
            if *selection == old_primary {
                primary = Some(moved);
                primary_affinity = target.affinity;
            } else {
                additional.push(moved);
            }
        }
        self.buffer.set_selections_with_primary(primary.unwrap_or(old_primary), additional);
        self.preferred_x = Some(preferred_x);
        self.scroll_to_caret = true;
        self.primary_affinity = primary_affinity;
        self.reset_caret_blink();
        cx.notify();
    }

    fn move_to_visual_line_edge(&mut self, start: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.clone() else {
            self.move_logically(
                if start { Movement::LineStart } else { Movement::LineEnd },
                extend,
                cx,
            );
            return;
        };
        let old_primary = self.buffer.primary_selection();
        let mut primary = None;
        let mut additional = Vec::with_capacity(self.buffer.selections().len().saturating_sub(1));
        for selection in self.buffer.selections() {
            let affinity = if *selection == old_primary {
                self.primary_affinity
            } else {
                CaretAffinity::Downstream
            };
            let row = layout.visual_row_range_at(selection.head, affinity);
            let target = if start { row.start } else { row.end };
            let moved = if extend {
                self.hex_selection(Selection::new(selection.anchor, target))
            } else {
                Selection::caret(target)
            };
            if *selection == old_primary {
                primary = Some(moved);
            } else {
                additional.push(moved);
            }
        }
        self.buffer.set_selections_with_primary(primary.unwrap_or(old_primary), additional);
        self.preferred_x = None;
        self.scroll_to_caret = true;
        self.primary_affinity =
            if start { CaretAffinity::Downstream } else { CaretAffinity::Upstream };
        self.reset_caret_blink();
        cx.notify();
    }

    fn delete(&mut self, backward: bool, unit: DeleteUnit, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if let Some(format) = self.hex_format() {
            self.delete_hex(format, backward, cx);
        } else {
            if backward {
                self.buffer.backspace(unit);
            } else {
                self.buffer.delete_forward(unit);
            }
            self.did_change(cx);
        }
    }

    fn delete_to_line_edge(&mut self, start: bool, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let before = self.buffer.selections().to_vec();
        self.move_to_visual_line_edge(start, true, cx);
        if self.buffer.selections() != before.as_slice() {
            if let Some(format) = self.hex_format() {
                self.delete_hex_selection(format, self.buffer.primary_selection().range(), cx);
            } else {
                self.buffer.replace_selections("");
                self.did_change(cx);
            }
        }
    }

    fn copy(&self, cx: &mut App) {
        if !self.enabled || self.masked {
            return;
        }
        let text = self
            .buffer
            .selections()
            .iter()
            .filter(|selection| !selection.is_empty())
            .map(|selection| &self.buffer.as_str()[selection.range()])
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if !self.enabled || self.read_only || self.masked {
            return;
        }
        self.buffer.commit_edit_group();
        self.copy(cx);
        if let Some(format) = self.hex_format() {
            self.delete_hex_selection(format, self.buffer.primary_selection().range(), cx);
            return;
        }
        if self.buffer.selections().iter().any(|selection| !selection.is_empty()) {
            self.buffer.replace_selections("");
            self.did_change(cx);
        }
    }

    fn clipboard_text(&self, cx: &App) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            // Bound text copying/validation on the local input path. Short fields
            // use their tighter limit; code entry allows grouping separators.
            const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024;
            let limit = if let Some(format) = self.hex_format() {
                self.hex_clipboard_limit(format)
            } else if self.email_code {
                32
            } else if self.text_area {
                self.max_bytes.unwrap_or(MAX_CLIPBOARD_TEXT_BYTES)
            } else {
                self.max_bytes.unwrap_or(MAX_CLIPBOARD_TEXT_BYTES).min(MAX_CLIPBOARD_TEXT_BYTES)
            };
            cx.read_text_from_clipboard(limit)
        }
        #[cfg(not(target_os = "macos"))]
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if !self.enabled || self.read_only {
            return;
        }
        let Some(text) = self.clipboard_text(cx) else {
            return;
        };
        if self.hex_format().is_some() {
            self.paste_hex(self.buffer.primary_selection().range(), &text, cx);
            return;
        }
        let Some(text) = self.input_text(&text) else { return };
        self.buffer.commit_edit_group();
        self.buffer.replace_selections(&text);
        self.did_change(cx);
    }

    fn click(&mut self, event: &MouseDownEvent, layout: &LayoutSnapshot) {
        let hit = layout.hit_test(event.position);
        let offset = hit.offset;
        let (granularity, origin) = if self.hex_format().is_some() && event.click_count == 2 {
            (
                DragGranularity::Grapheme,
                self.hex_selection(selection_for_pointer(
                    &self.buffer,
                    hit,
                    DragGranularity::Grapheme,
                )),
            )
        } else {
            selection_for_click(&self.buffer, hit, event.click_count)
        };
        let mut other_selections = if event.modifiers.platform && self.hex_format().is_none() {
            self.buffer.selections().to_vec()
        } else {
            Vec::new()
        };

        if granularity == DragGranularity::Grapheme && event.modifiers.shift {
            let anchor = self.buffer.primary_selection().anchor;
            self.buffer.set_selection(self.hex_selection(Selection::new(anchor, offset)));
            self.selection_drag = Some(SelectionDrag {
                granularity,
                origin: Selection::caret(anchor),
                other_selections: Vec::new(),
            });
            self.primary_affinity = hit.affinity;
            self.reset_caret_blink();
            return;
        }

        other_selections.retain(|selection| selection.range() != origin.range());
        self.buffer.set_selections_with_primary(origin, other_selections.iter().copied());
        self.selection_drag = Some(SelectionDrag { granularity, origin, other_selections });
        self.primary_affinity = hit.affinity;
        self.reset_caret_blink();
    }

    fn drag(&mut self, position: Point<Pixels>) {
        let Some(drag) = self.selection_drag.clone() else {
            return;
        };
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let hit = layout.hit_test(position);
        let target = selection_for_pointer(&self.buffer, hit, drag.granularity);
        let active = if target.end() >= drag.origin.end() {
            Selection::new(drag.origin.start(), target.end())
        } else {
            Selection::new(drag.origin.end(), target.start())
        };
        self.buffer.set_selections_with_primary(self.hex_selection(active), drag.other_selections);
        self.primary_affinity = hit.affinity;
        self.reset_caret_blink();
    }

    fn grouped_hex(&self) -> bool {
        self.hex == Some(hex::Format::Grouped)
    }

    fn horizontal_scroll_limit(&self, content: Pixels, viewport: Pixels) -> Pixels {
        if self.grouped_hex() || self.email_code {
            Pixels::ZERO
        } else {
            (content - viewport).max(Pixels::ZERO)
        }
    }

    fn scroll_by(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let maximum_x =
            self.horizontal_scroll_limit(layout.content_width, layout.bounds.size.width);
        let maximum_y = (layout.content_height - layout.bounds.size.height).max(Pixels::ZERO);
        let scroll_x = (self.scroll_x + delta.x).clamp(Pixels::ZERO, maximum_x);
        let scroll_y = (self.scroll_y + delta.y).clamp(Pixels::ZERO, maximum_y);
        if scroll_x != self.scroll_x || scroll_y != self.scroll_y {
            self.scroll_x = scroll_x;
            self.scroll_y = scroll_y;
            self.scroll_to_caret = false;
            cx.notify();
        }
    }
}

impl Focusable for TextEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn outline_width(style: &TextEditorStyle, focused: bool) -> Pixels {
    if focused { style.border_width_focused } else { style.border_width }
}

fn surface_paint_radius(radius: Pixels) -> Pixels {
    if radius == px(1.0) { px(0.5) } else { radius }
}

impl Render for TextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.style = self.base_style.scaled(window);
        if self.selectable_id {
            let text = window.text_style();
            self.style.font_size = text.font_size.to_pixels(window.rem_size());
            self.style.line_height = text.line_height_in_pixels(window.rem_size());
            self.style.letter_spacing = text.letter_spacing;
            self.style.text = text.color;
        }
        if self.focus_subscription.is_none() {
            self.focus_subscription =
                Some(cx.on_focus(&self.focus_handle, window, |this, _, cx| this.did_focus(cx)));
        }
        if self.blur_subscription.is_none() {
            self.blur_subscription =
                Some(cx.on_blur(&self.focus_handle, window, |this, window, cx| {
                    this.keyboard_focus = false;
                    this.buffer.commit_edit_group();
                    this.last_typing = None;
                    this.stop_caret_clock();
                    if this.enabled
                        && this.buffer.marked_range().is_none()
                        && let Some(callback) = &this.on_blur
                    {
                        callback(this.buffer.as_str(), this.revision.current, cx);
                    }
                    if window.is_window_active() {
                        this.clear_selection_for_focus_change(cx);
                    }
                    cx.notify();
                }));
        }
        let focused = window.is_window_active()
            && self.focus_handle.is_focused(window)
            && !self.read_only
            && self.enabled;
        if !focused {
            self.stop_caret_clock();
        } else if !self.caret_blink_active {
            self.did_focus(cx);
        }
        let keyboard_focus = self.keyboard_focus
            && self.enabled
            && window.is_window_active()
            && self.focus_handle.is_focused(window);
        let outlined = focused || keyboard_focus;
        let outline_width = outline_width(&self.style, outlined);
        let outline_color = if outlined { self.style.border_focused } else { self.style.border };
        let outline = gpui::div()
            .when(keyboard_focus, |outline| {
                outline.debug_selector(|| format!("text-editor-keyboard-focus-{}", cx.entity_id()))
            })
            .absolute()
            .top(-outline_width)
            .right(-outline_width)
            .bottom(-outline_width)
            .left(-outline_width)
            .rounded(self.style.corner_radius + outline_width)
            .when(self.read_only, |outline| {
                outline.border(outline_width).border_color(outline_color)
            })
            .when(!self.read_only, |outline| outline.bg(outline_color));
        let surface = gpui::div()
            .debug_selector(|| format!("text-editor-surface-{}", cx.entity_id()))
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .overflow_hidden()
            .bg(self.style.background)
            .rounded(surface_paint_radius(self.style.corner_radius))
            .children(self.style.inner_shadow.map(|color| {
                gpui::div()
                    .absolute()
                    .top(-zoom::pixels(window, 1.0))
                    .left_0()
                    .right_0()
                    .h(zoom::pixels(window, 1.0))
                    .shadow(vec![BoxShadow {
                        color,
                        offset: point(px(0.0), zoom::pixels(window, 1.0)),
                        blur_radius: zoom::pixels(window, 0.5),
                        spread_radius: Pixels::ZERO,
                    }])
            }));
        gpui::div()
            .id(("ArchipelagoTextEditor", cx.entity().entity_id()))
            .key_context("ArchipelagoTextEditor")
            .track_focus(&self.focus_handle)
            .map(|field| {
                #[cfg(target_os = "macos")]
                let field = field.capture_any_mouse_down(cx.listener(
                    |this, event: &MouseDownEvent, window, cx| {
                        if event.button == MouseButton::Right
                            || (event.button == MouseButton::Left && event.modifiers.control)
                        {
                            window.prevent_default();
                            this.open_context_menu(event, window, cx);
                            cx.stop_propagation();
                        }
                    },
                ));
                field
            })
            .cursor(CursorStyle::IBeam)
            .opacity(if self.enabled { 1. } else { 0.5 })
            .relative()
            .flex()
            .flex_col()
            .when(self.message_scroll.is_some(), |field| field.min_h_0())
            .when(self.text_area, |field| field.flex_1().h_full().min_h_0())
            .p(self.style.padding)
            .when_some(self.style.trailing_padding, |field, padding| field.pr(padding))
            .when(!self.email_code && self.message_scroll.is_none(), |field| {
                field.child(outline).child(surface)
            })
            .when(self.email_code, |field| {
                let selected = self.buffer.primary_selection();
                field
                    .children((0..6).filter(|_| !self.read_only).map(|i| {
                        let active = focused
                            && (selected.range().contains(&i)
                                || (selected.is_empty() && selected.head.min(5) == i));
                        gpui::div()
                            .absolute()
                            .left(zoom::pixels(window, f32::from(code_x(i)) - 8.))
                            .top_0()
                            .w(zoom::pixels(window, 24.))
                            .h(zoom::pixels(window, 24.))
                            .rounded(zoom::pixels(window, 2.))
                            .bg(self.style.background)
                            .border(if active {
                                self.style.border_width_focused
                            } else {
                                self.style.border_width
                            })
                            .border_color(if active {
                                self.style.border_focused
                            } else {
                                self.style.border
                            })
                    }))
                    .child(
                        gpui::div()
                            .absolute()
                            .left(zoom::pixels(window, 88.))
                            .top_0()
                            .w(zoom::pixels(window, 32.))
                            .h(zoom::pixels(window, 24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(zoom::pixels(window, 16.))
                            .child("–"),
                    )
            })
            .on_key_down(cx.listener(|this, _, _, cx| {
                this.reset_caret_blink();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                this.move_logically(Movement::PreviousGrapheme, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                this.move_logically(Movement::NextGrapheme, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveWordLeft, _, cx| {
                this.move_logically(Movement::PreviousWord, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveWordRight, _, cx| {
                this.move_logically(Movement::NextWord, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveLineStart, _, cx| {
                this.move_to_visual_line_edge(true, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveLineEnd, _, cx| {
                this.move_to_visual_line_edge(false, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveDocumentStart, _, cx| {
                this.move_logically(Movement::DocumentStart, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveDocumentEnd, _, cx| {
                this.move_logically(Movement::DocumentEnd, false, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_vertically(true, false, cx)))
            .on_action(
                cx.listener(|this, _: &MoveDown, _, cx| this.move_vertically(false, false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                this.move_logically(Movement::PreviousGrapheme, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                this.move_logically(Movement::NextGrapheme, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                this.move_logically(Movement::PreviousWord, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                this.move_logically(Movement::NextWord, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLineStart, _, cx| {
                this.move_to_visual_line_edge(true, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLineEnd, _, cx| {
                this.move_to_visual_line_edge(false, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectDocumentStart, _, cx| {
                this.move_logically(Movement::DocumentStart, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectDocumentEnd, _, cx| {
                this.move_logically(Movement::DocumentEnd, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectUp, _, cx| this.move_vertically(true, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectDown, _, cx| this.move_vertically(false, true, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.select_all(cx);
            }))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(
                cx.listener(|_, _: &ShowCharacterPalette, window, _| {
                    window.show_character_palette()
                }),
            )
            .when(!self.read_only && self.enabled, |field| {
                field
                    .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                        this.delete(true, DeleteUnit::Grapheme, cx);
                    }))
                    .on_action(cx.listener(|this, _: &DeleteForward, _, cx| {
                        this.delete(false, DeleteUnit::Grapheme, cx);
                    }))
                    .on_action(cx.listener(|this, _: &DeleteWordBackward, _, cx| {
                        this.delete(true, DeleteUnit::Word, cx);
                    }))
                    .on_action(cx.listener(|this, _: &DeleteWordForward, _, cx| {
                        this.delete(false, DeleteUnit::Word, cx);
                    }))
                    .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                        this.delete_to_line_edge(true, cx);
                    }))
                    .on_action(cx.listener(|this, _: &DeleteToLineEnd, _, cx| {
                        this.delete_to_line_edge(false, cx);
                    }))
                    .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
                    .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
                    .on_action(cx.listener(|this, _: &Undo, _, cx| {
                        if this.read_only {
                            return;
                        }
                        if this.buffer.undo() {
                            this.refresh_hex_format();
                            this.did_change(cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &Redo, _, cx| {
                        if this.read_only {
                            return;
                        }
                        if this.buffer.redo() {
                            this.refresh_hex_format();
                            this.did_change(cx);
                        }
                    }))
                    .on_action(
                        cx.listener(|this, _: &InsertNewline, _, cx| this.insert_newline(cx)),
                    )
                    .on_action(cx.listener(|this, _: &Commit, _, cx| this.commit(cx)))
            })
            .when_some(self.message_scroll.clone(), |field, scroll| {
                field.child(
                    gpui::div()
                        .id("message-viewport")
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .child(
                            gpui::div()
                                .p(zoom::pixels(window, 8.))
                                .child(TextEditorElement::new(cx.entity())),
                        ),
                )
            })
            .when(self.message_scroll.is_none(), |field| {
                field.child(TextEditorElement::new(cx.entity()))
            })
    }
}

pub fn install_keybindings(cx: &mut App) {
    if !cx.has_global::<TextSelectionCoordinator>() {
        cx.set_global(TextSelectionCoordinator::default());
    }
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("ArchipelagoTextEditor")),
        KeyBinding::new("delete", DeleteForward, Some("ArchipelagoTextEditor")),
        KeyBinding::new("left", MoveLeft, Some("ArchipelagoTextEditor")),
        KeyBinding::new("right", MoveRight, Some("ArchipelagoTextEditor")),
        KeyBinding::new("up", MoveUp, Some("ArchipelagoTextEditor")),
        KeyBinding::new("down", MoveDown, Some("ArchipelagoTextEditor")),
        KeyBinding::new("shift-left", SelectLeft, Some("ArchipelagoTextEditor")),
        KeyBinding::new("shift-right", SelectRight, Some("ArchipelagoTextEditor")),
        KeyBinding::new("shift-up", SelectUp, Some("ArchipelagoTextEditor")),
        KeyBinding::new("shift-down", SelectDown, Some("ArchipelagoTextEditor")),
        KeyBinding::new("alt-left", MoveWordLeft, Some("ArchipelagoTextEditor")),
        KeyBinding::new("alt-right", MoveWordRight, Some("ArchipelagoTextEditor")),
        KeyBinding::new("alt-shift-left", SelectWordLeft, Some("ArchipelagoTextEditor")),
        KeyBinding::new("alt-shift-right", SelectWordRight, Some("ArchipelagoTextEditor")),
        KeyBinding::new("alt-backspace", DeleteWordBackward, Some("ArchipelagoTextEditor")),
        KeyBinding::new("alt-delete", DeleteWordForward, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-left", MoveLineStart, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-right", MoveLineEnd, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-shift-left", SelectLineStart, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-up", MoveDocumentStart, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-down", MoveDocumentEnd, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-shift-up", SelectDocumentStart, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-shift-down", SelectDocumentEnd, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-a", MoveLineStart, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-e", MoveLineEnd, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-b", MoveLeft, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-f", MoveRight, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-p", MoveUp, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-n", MoveDown, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-h", Backspace, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-d", DeleteForward, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-k", DeleteToLineEnd, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-a", SelectAll, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-c", Copy, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-x", Cut, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-v", Paste, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-z", Undo, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-shift-z", Redo, Some("ArchipelagoTextEditor")),
        KeyBinding::new("enter", InsertNewline, Some("ArchipelagoTextEditor")),
        KeyBinding::new("cmd-enter", Commit, Some("ArchipelagoTextEditor")),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some("ArchipelagoTextEditor")),
    ]);
}

pub fn accessibility_text_ids(
    id: gpui::EntityId,
) -> (gpui::accesskit::NodeId, gpui::accesskit::NodeId) {
    let id = id.as_u64().checked_mul(2).expect("accessibility identity exhausted");
    (gpui::accesskit::NodeId(id), gpui::accesskit::NodeId(id + 1))
}

fn accessibility_offset(text: &str, index: usize) -> Option<usize> {
    text.char_indices().map(|(offset, _)| offset).chain(std::iter::once(text.len())).nth(index)
}

struct TextEditorElement {
    editor: Entity<TextEditor>,
}

impl TextEditorElement {
    fn new(editor: Entity<TextEditor>) -> Self {
        Self { editor }
    }
}

impl IntoElement for TextEditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct PrepaintState {
    hitbox: Hitbox,
    layout: LayoutSnapshot,
    placeholder: Option<Vec<WrappedLine>>,
}

fn paint_hex_offsets(
    layout: &LayoutSnapshot,
    viewport: Bounds<Pixels>,
    style: &TextEditorStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let gutter = Bounds::new(
        viewport.origin,
        size(layout.bounds.left() - viewport.left(), viewport.size.height),
    );
    if gutter.size.width <= Pixels::ZERO {
        return;
    }
    window.with_content_mask(Some(gpui::ContentMask { bounds: gutter }), |window| {
        let mut y = viewport.top() - layout.scroll_y;
        for line in &layout.lines {
            let height = line.shaped.size(style.line_height).height;
            if y >= viewport.bottom() {
                break;
            }
            if y + height > viewport.top()
                && let Some(offset) = line.hex_offset
            {
                let text: SharedString = format!("{offset:04x}").into();
                let label = window.text_system().shape_line(
                    text.clone(),
                    style.font_size,
                    &[TextRun {
                        len: text.len(),
                        font: font("Menlo"),
                        color: gpui::black().alpha(0.4),
                        letter_spacing: px(0.),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                let x = gutter.right() - zoom::pixels(window, 8.) - label.width;
                label.paint(point(x, y), style.line_height, window, cx).ok();
            }
            y += height;
        }
    });
}

impl Element for TextEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let editor = self.editor.read(cx);
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        if editor.text_area {
            style.size.height = relative(1.0).into();
            style.min_size.height = px(0.).into();
            style.flex_grow = 1.;
            return (window.request_layout(style, [], cx), ());
        }
        if editor.message_scroll.is_some() {
            let text: SharedString = editor.display_text().into();
            let font_size = editor.style.font_size;
            let line_height = editor.style.line_height;
            let run = TextRun {
                len: text.len(),
                font: control_font(),
                color: editor.style.text,
                letter_spacing: editor.style.letter_spacing,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            return (
                window.request_measured_layout(style, move |known, available, window, _| {
                    let width = known.width.or(match available.width {
                        gpui::AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    });
                    let lines = window
                        .text_system()
                        .shape_text(
                            text.clone(),
                            font_size,
                            std::slice::from_ref(&run),
                            width,
                            None,
                        )
                        .unwrap_or_default();
                    size(
                        width.unwrap_or_else(|| {
                            lines.iter().map(|line| line.width()).fold(px(0.), Pixels::max)
                        }),
                        lines
                            .iter()
                            .map(|line| line.size(line_height).height)
                            .fold(px(0.), |total, height| total + height),
                    )
                }),
                (),
            );
        }
        style.size.height = editor.style.line_height.into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let editor = self.editor.read(cx);
        let viewport = bounds;
        // Four offset digits plus an eight-point gap.
        let gutter = if editor.grouped_hex() { zoom::pixels(window, 40.) } else { Pixels::ZERO };
        let bounds = Bounds::new(
            point(bounds.left() + gutter, bounds.top()),
            size((bounds.size.width - gutter).max(Pixels::ZERO), bounds.size.height),
        );
        let text: SharedString = editor.display_text().into();
        let text_run = TextRun {
            len: text.len(),
            letter_spacing: editor.style.letter_spacing,
            font: editor.text_font(),
            color: editor.style.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // An older malformed draft may contain a line wider than a complete hex row.
        let wraps = editor.message_scroll.is_some() || (editor.text_area && !editor.canonical_hex);
        let wrap_width = Some(if wraps { bounds.size.width } else { Pixels::MAX });
        let placeholder = if text.is_empty() && !editor.email_code {
            editor.placeholder.clone().map(|placeholder| {
                let run = TextRun {
                    len: placeholder.len(),
                    letter_spacing: editor.style.letter_spacing,
                    font: editor.text_font(),
                    color: editor.style.placeholder,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_text(placeholder, editor.style.font_size, &[run], wrap_width, None)
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            })
        } else {
            None
        };
        let shaped = window
            .text_system()
            .shape_text(text, editor.style.font_size, &[text_run], wrap_width, None)
            .unwrap_or_default();
        let text_len = editor.buffer.as_str().len();
        let mut lines = Vec::with_capacity(shaped.len());
        let mut rows = Vec::new();
        let mut start = 0;
        let mut top = Pixels::ZERO;
        let mut content_width = Pixels::ZERO;
        let mut hex_digits = 0;
        for line in shaped {
            let line_index = lines.len();
            let line_size = line.size(editor.style.line_height);
            let height = line_size.height;
            content_width = content_width.max(line_size.width);
            let len = line.len();
            let mut row_starts = vec![0];
            for boundary in line.wrap_boundaries() {
                let run = &line.runs()[boundary.run_ix];
                row_starts.push(run.glyphs[boundary.glyph_ix].index);
            }
            let newline_after = start + len < text_len;
            for (row_index, row_start) in row_starts.iter().copied().enumerate() {
                let row_end = row_starts.get(row_index + 1).copied().unwrap_or(len);
                rows.push(LayoutRow {
                    line_index,
                    range: start + row_start..start + row_end,
                    top: top + editor.style.line_height * row_index,
                    start_x: line.unwrapped_layout.x_for_index(row_start),
                    newline_after: newline_after && row_end == len,
                });
            }
            let hex_offset = (gutter > Pixels::ZERO).then(|| {
                if editor.canonical_hex {
                    line_index * 16
                } else {
                    let offset = hex_digits / 2;
                    hex_digits += editor.text()[start..start + len]
                        .bytes()
                        .filter(u8::is_ascii_hexdigit)
                        .count();
                    offset
                }
            });
            lines.push(LayoutLine { start, shaped: line, hex_offset });
            start = (start + len + 1).min(editor.buffer.as_str().len());
            top += height;
        }
        let content_height = rows
            .last()
            .map(|row| row.top + editor.style.line_height)
            .unwrap_or(editor.style.line_height);
        let maximum_scroll_x = editor.horizontal_scroll_limit(content_width, bounds.size.width);
        let maximum_scroll = (content_height - bounds.size.height).max(Pixels::ZERO);
        let mut scroll_x = editor.scroll_x.clamp(Pixels::ZERO, maximum_scroll_x);
        let mut scroll_y = editor.scroll_y.clamp(Pixels::ZERO, maximum_scroll);
        let line_height = editor.style.line_height;
        let primary_head = editor.buffer.primary_selection().head;
        let primary_affinity = editor.primary_affinity;
        let scroll_to_caret = editor.scroll_to_caret;
        let focus_handle = editor.focus_handle.clone();

        let mut layout = LayoutSnapshot {
            bounds,
            lines,
            rows,
            line_height,
            scale: f32::from(window.rem_size()) / 16.,
            content_height,
            content_width,
            scroll_x,
            scroll_y,
            text_len,
            email_code: editor.email_code,
        };
        if scroll_to_caret
            && let Some(caret) = layout.position_for_offset(primary_head, primary_affinity)
        {
            if editor.message_scroll.is_some() {
                window.request_autoscroll(Bounds::new(caret, size(px(1.), line_height)));
            }
            if caret.x < bounds.left() {
                scroll_x = (scroll_x - (bounds.left() - caret.x)).max(Pixels::ZERO);
            } else if caret.x + px(1.0) > bounds.right() {
                scroll_x = (scroll_x + caret.x + px(1.0) - bounds.right()).min(maximum_scroll_x);
            }
            if caret.y < bounds.top() {
                scroll_y = (scroll_y - (bounds.top() - caret.y)).max(Pixels::ZERO);
            } else if caret.y + line_height > bounds.bottom() {
                scroll_y = (scroll_y + caret.y + line_height - bounds.bottom()).min(maximum_scroll);
            }
            layout.scroll_x = scroll_x;
            layout.scroll_y = scroll_y;
            if editor.grouped_hex()
                && focus_handle.is_focused(window)
                && window.is_window_active()
                && let Some(caret) = layout.position_for_offset(primary_head, primary_affinity)
            {
                window.request_autoscroll(Bounds::new(caret, size(px(1.), line_height)));
            }
        }
        let hitbox = window.insert_hitbox(viewport, HitboxBehavior::Normal);
        window.set_focus_handle(&focus_handle, cx);
        self.editor.update(cx, |editor, cx| {
            use gpui::accesskit::{Action, Node, Rect, Role, TextPosition, TextSelection};
            let _span = crate::trace::Span::new("ui.text.accessibility");
            let (id, run_id) = accessibility_text_ids(cx.entity_id());
            let text = editor.buffer.as_str();
            let mut node = Node::new(if editor.message_scroll.is_some() || editor.text_area {
                Role::MultilineTextInput
            } else if editor.masked {
                Role::PasswordInput
            } else {
                Role::TextInput
            });
            node.set_bounds(Rect::new(
                viewport.left().to_f64(),
                viewport.top().to_f64(),
                viewport.right().to_f64(),
                viewport.bottom().to_f64(),
            ));
            if let Some(placeholder) = &editor.placeholder {
                node.set_placeholder(placeholder.to_string());
            }
            if editor.selectable_id {
                node.set_label("ID");
            }
            if !editor.enabled {
                node.set_disabled();
            }
            node.add_action(Action::Focus);
            if editor.read_only {
                node.set_read_only();
            } else {
                node.add_action(Action::SetValue);
            }
            if editor.message_scroll.is_some() {
                node.set_live(gpui::accesskit::Live::Polite);
            }
            if editor.message_scroll.is_some() || editor.text_area || editor.selectable_id {
                node.set_value(text);
            }
            node.add_action(Action::SetTextSelection);
            node.set_children(vec![run_id]);
            let selection = editor.buffer.primary_selection();
            node.set_text_selection(TextSelection {
                anchor: TextPosition {
                    node: run_id,
                    character_index: text[..selection.anchor].chars().count(),
                },
                focus: TextPosition {
                    node: run_id,
                    character_index: text[..selection.head].chars().count(),
                },
            });
            let mut run = Node::new(Role::TextRun);
            run.set_bounds(node.bounds().expect("text layout assigned bounds before prepaint"));
            run.set_value(editor.display_text());
            // Buffer selections admit every Unicode scalar boundary (while
            // arrow/delete commands use graphemes). Preserve that same range
            // model for native UTF-16 selection, including combining marks.
            run.set_character_lengths(text.chars().map(|c| c.len_utf8() as u8).collect::<Vec<_>>());
            window.insert_accessibility_node(
                id,
                node,
                editor.focus_handle.is_focused(window),
                Some(std::rc::Rc::new(cx.listener(TextEditor::accessibility_action))),
            );
            window.insert_accessibility_node(run_id, run, false, None);
            if std::mem::take(&mut editor.accessibility_scroll) && editor.message_scroll.is_none() {
                window.request_autoscroll(bounds.dilate(zoom::pixels(window, 8.)));
            }
            editor.scroll_x = scroll_x;
            editor.scroll_y = scroll_y;
            editor.scroll_to_caret = false;
            editor.layout = Some(layout.clone());
        });
        PrepaintState { hitbox, layout, placeholder }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _timing = crate::trace::Span::new("ui.text.paint");
        let editor = self.editor.read(cx);
        let focus_handle = editor.focus_handle.clone();
        let selections = editor.buffer.selections().to_vec();
        let primary_selection = editor.buffer.primary_selection();
        let primary_affinity = editor.primary_affinity;
        let marked_range = editor.buffer.marked_range();
        let validation_underline = editor
            .validation_underline
            .clone()
            .filter(|_| editor.enabled && !editor.read_only && marked_range.is_none());
        let style = editor.style.clone();
        let focused = focus_handle.is_focused(window);
        let caret_visible = editor.caret_visible && !editor.read_only && editor.enabled;
        let masked = editor.masked;
        let email_code = editor.email_code;
        let code = email_code.then(|| editor.buffer.as_str().to_owned());
        paint_hex_offsets(&state.layout, bounds, &style, window, cx);
        let bounds = state.layout.bounds;

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        window.set_cursor_style(CursorStyle::IBeam, &state.hitbox);

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if focused {
                for selection in &selections {
                    for rectangle in state.layout.rectangles_for_range(selection.range()) {
                        window.paint_quad(fill(rectangle, style.selection));
                    }
                }
            }

            let origin_x = bounds.left() - state.layout.scroll_x;
            let mut origin_y = bounds.top() - state.layout.scroll_y;
            if let Some(code) = &code {
                for (i, digit) in code.chars().enumerate() {
                    let text: SharedString = digit.to_string().into();
                    let line = window.text_system().shape_line(
                        text.clone(),
                        style.font_size,
                        &[TextRun {
                            len: text.len(),
                            font: tabular_font(),
                            color: style.text,
                            letter_spacing: px(0.),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    line.paint(
                        point(origin_x + state.layout.code_x(i) - line.width / 2., origin_y),
                        style.line_height,
                        window,
                        cx,
                    )
                    .ok();
                }
            } else if masked && state.layout.text_len > 0 {
                let bullet: SharedString = "•".into();
                let line = window.text_system().shape_line(
                    bullet,
                    style.font_size,
                    &[TextRun {
                        len: "•".len(),
                        font: control_font(),
                        color: style.text,
                        letter_spacing: px(0.),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                for i in 0..state.layout.text_len {
                    if let Some(position) =
                        state.layout.position_for_offset(i, CaretAffinity::Downstream)
                    {
                        line.paint(position, style.line_height, window, cx).ok();
                    }
                }
            } else if let Some(placeholder) = &state.placeholder {
                for line in placeholder {
                    line.paint(
                        point(origin_x, origin_y),
                        style.line_height,
                        TextAlign::Left,
                        Some(Bounds::new(
                            point(origin_x, origin_y),
                            size(bounds.size.width, line.size(style.line_height).height),
                        )),
                        window,
                        cx,
                    )
                    .ok();
                    origin_y += line.size(style.line_height).height;
                }
            } else {
                for line in &state.layout.lines {
                    let height = line.shaped.size(style.line_height).height;
                    if origin_y >= bounds.bottom() {
                        break;
                    }
                    if origin_y + height > bounds.top() {
                        line.shaped
                            .paint(
                                point(origin_x, origin_y),
                                style.line_height,
                                TextAlign::Left,
                                Some(Bounds::new(
                                    point(origin_x, origin_y),
                                    size(bounds.size.width, height),
                                )),
                                window,
                                cx,
                            )
                            .ok();
                    }
                    origin_y += height;
                }
            }

            if let Some(marked_range) = marked_range {
                let underline = UnderlineStyle {
                    color: Some(style.marked_text),
                    thickness: px(1.0),
                    wavy: false,
                };
                for rectangle in state.layout.rectangles_for_range(marked_range) {
                    window.paint_underline(
                        point(rectangle.left(), rectangle.bottom() - px(1.0)),
                        rectangle.size.width,
                        &underline,
                    );
                }
            }

            if let Some(range) = validation_underline {
                let thickness = zoom::pixels(window, 1.);
                let underline = UnderlineStyle { color: Some(style.text), thickness, wavy: false };
                for rectangle in state.layout.rectangles_for_range(range) {
                    window.paint_underline(
                        point(rectangle.left(), rectangle.bottom() - thickness),
                        rectangle.size.width,
                        &underline,
                    );
                }
            }

            if focused && caret_visible {
                for selection in selections.iter().filter(|selection| selection.is_empty()) {
                    let affinity = if *selection == primary_selection {
                        primary_affinity
                    } else {
                        CaretAffinity::Downstream
                    };
                    if let Some(position) =
                        state.layout.position_for_offset(selection.head, affinity)
                    {
                        window.paint_quad(fill(
                            Bounds::new(position, size(px(1.0), style.line_height)),
                            style.caret,
                        ));
                    }
                }
            }
        });

        let hitbox = state.hitbox.clone();
        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            editor.update(cx, |editor, cx| {
                if !editor.enabled {
                    return;
                }
                editor.keyboard_focus = false;
                editor.focus_handle.focus(window);
                if let Some(layout) = editor.layout.clone() {
                    editor.click(event, &layout);
                    editor.preferred_x = None;
                    editor.scroll_to_caret = true;
                    cx.notify();
                }
            });
            cx.stop_propagation();
        });

        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase != DispatchPhase::Bubble || !event.dragging() {
                return;
            }
            editor.update(cx, |editor, cx| {
                if editor.selection_drag.is_some() {
                    editor.drag(event.position);
                    editor.scroll_to_caret = true;
                    cx.notify();
                }
            });
        });

        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                editor.update(cx, |editor, _| editor.selection_drag = None);
            }
        });

        let editor = self.editor.clone();
        let line_height = style.line_height;
        let hitbox = state.hitbox.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || !hitbox.is_hovered(window)
                || editor.read(cx).message_scroll.is_some()
                || editor.read(cx).selectable_id
            {
                return;
            }
            let delta = event.delta.pixel_delta(line_height);
            let grouped_hex = editor.read(cx).grouped_hex();
            if !grouped_hex || delta.y != Pixels::ZERO {
                editor.update(cx, |editor, cx| editor.scroll_by(point(-delta.x, -delta.y), cx));
            }
            if !grouped_hex || delta.x == Pixels::ZERO {
                cx.stop_propagation();
            }
        });
    }
}

impl EntityInputHandler for TextEditor {
    fn sensitive_input(&mut self, window: &mut Window, _: &mut Context<Self>) -> bool {
        self.focus_handle.is_focused(window)
            && self.enabled
            && !self.read_only
            && (self.masked || self.email_code)
    }

    fn text_checking_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TextCheckingState> {
        if !self.focus_handle.is_focused(window) {
            return None;
        }
        self.checking_state(cx.entity_id().as_u64())
    }

    fn text_checking_text(
        &mut self,
        range: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<(String, Range<usize>)> {
        if !self.permits_text_checking() {
            return None;
        }
        text_checking::paragraph_context(
            self.buffer.as_str(),
            range,
            self.buffer.primary_selection().head,
        )
    }

    fn replace_text_for_checking(
        &mut self,
        expected: &TextCheckingState,
        range: Range<usize>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.text_checking_state(window, cx).as_ref() != Some(expected)
            || self.buffer.marked_range().is_some()
            || range.end.saturating_sub(range.start) > text_checking::CONTEXT_UNITS_MAX
            || text.len() > text_checking::CONTEXT_UNITS_MAX * 3
        {
            return false;
        }
        let Some(range) = text_checking::exact_utf16_range(self.buffer.as_str(), range) else {
            return false;
        };
        let Some(validated) = self.input_text(text) else { return false };
        if validated != text || text.encode_utf16().count() > text_checking::CONTEXT_UNITS_MAX {
            return false;
        }
        let Some(len) = self
            .buffer
            .as_str()
            .len()
            .checked_sub(range.len())
            .and_then(|len| len.checked_add(text.len()))
        else {
            return false;
        };
        if self.max_bytes.is_some_and(|limit| len > limit) {
            return false;
        }
        if self.buffer.as_str()[range.clone()] == *text {
            return true;
        }
        self.last_typing = None;
        self.buffer.commit_edit_group();
        self.buffer.replace_range(range, text);
        self.did_change(cx);
        true
    }

    fn select_text_for_checking(
        &mut self,
        expected: &TextCheckingState,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.text_checking_state(window, cx).as_ref() != Some(expected)
            || self.buffer.marked_range().is_some()
        {
            return false;
        }
        let Some(range) = text_checking::exact_utf16_range(self.buffer.as_str(), range) else {
            return false;
        };
        self.buffer.set_selection(Selection::new(range.start, range.end));
        self.scroll_to_caret = true;
        self.preferred_x = None;
        self.primary_affinity = CaretAffinity::Downstream;
        self.reset_caret_blink();
        cx.notify();
        true
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let utf8 = utf16_range_to_utf8(self.buffer.as_str(), range);
        *adjusted_range = Some(utf8_range_to_utf16(self.buffer.as_str(), utf8.clone()));
        Some(if self.masked {
            "*".repeat(utf8.len())
        } else {
            self.buffer.as_str()[utf8].to_owned()
        })
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.buffer.primary_selection();
        Some(UTF16Selection {
            range: utf8_range_to_utf16(self.buffer.as_str(), selection.range()),
            reversed: selection.is_reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.buffer.marked_range().map(|range| utf8_range_to_utf16(self.buffer.as_str(), range))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let inserted = self.buffer.marked_range();
        if self.buffer.commit_composition() {
            self.checking_inserted_range = inserted.filter(|range| !range.is_empty());
            self.checking_cache = None;
            self.send_draft(cx);
            self.reset_caret_blink();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled || self.read_only {
            return;
        }
        let _timing = crate::trace::Span::new("ui.text.input");
        if let Some(format) = self.hex_format() {
            let len = self.text().len();
            let range = range
                .map(|range| {
                    range.start.min(range.end).min(len)..range.end.max(range.start).min(len)
                })
                .unwrap_or_else(|| self.buffer.primary_selection().range());
            self.type_hex(format, range, text, cx);
            return;
        }
        let Some(text) = self.input_text(text) else { return };
        let range = range.map(|range| utf16_range_to_utf8(self.buffer.as_str(), range));
        if !self.permits_platform_edit(range.as_ref(), &text) {
            return;
        }
        let marked = self.buffer.marked_range();
        let start = match range.as_ref() {
            Some(range)
                if marked.is_none() && *range == self.buffer.primary_selection().range() =>
            {
                None
            }
            Some(range) => Some(range.start),
            None => marked.map(|range| range.start),
        };
        split_typing_group(&mut self.buffer, &mut self.last_typing, &range, &text, Instant::now());
        replace_platform_text(&mut self.buffer, range, &text);
        self.did_change(cx);
        if !text.is_empty() {
            let start = start
                .unwrap_or_else(|| self.buffer.primary_selection().head.saturating_sub(text.len()));
            let inserted = start..start.saturating_add(text.len());
            if self.buffer.as_str().get(inserted.clone()) == Some(text.as_str()) {
                self.checking_inserted_range = Some(inserted);
                self.checking_cache = None;
            }
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled || self.read_only || self.masked || self.email_code || self.hex.is_some() {
            return;
        }
        let _timing = crate::trace::Span::new("ui.text.composition");
        self.last_typing = None;
        let Some(text) = self.input_text(text) else { return };
        if let Some(range) = range {
            let range = utf16_range_to_utf8(self.buffer.as_str(), range);
            let old_primary = self.buffer.primary_selection();
            let additional = self
                .buffer
                .selections()
                .iter()
                .copied()
                .filter(|selection| *selection != old_primary)
                .collect::<Vec<_>>();
            self.buffer
                .set_selections_with_primary(Selection::new(range.start, range.end), additional);
        }
        let relative_selection = selected_range
            .map(|range| utf16_range_to_utf8(&text, range))
            .map(|range| Selection::new(range.start, range.end))
            .unwrap_or_else(|| Selection::caret(text.len()));
        self.buffer.update_composition(&text, relative_selection);
        if text.is_empty() {
            self.buffer.commit_composition();
        }
        self.did_change(cx);
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let range = utf16_range_to_utf8(self.buffer.as_str(), range);
        layout.first_bounds_for_range(range, self.primary_affinity)
    }

    fn bounds_for_range_with_actual(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<(Bounds<Pixels>, Range<usize>)> {
        let text = self.buffer.as_str();
        let range = if range.is_empty() {
            let caret = utf16_offset_to_utf8(text, range.start);
            caret..caret
        } else {
            utf16_range_to_utf8(text, range)
        };
        let layout = self.layout.as_ref()?;
        let (bounds, actual) =
            layout.first_bounds_with_actual_range(range, self.primary_affinity)?;
        if !bounds.intersects(&layout.bounds) {
            return None;
        }
        Some((bounds, utf8_range_to_utf16(text, actual)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.layout.as_ref()?.hit_test(point).offset;
        Some(utf8_offset_to_utf16(self.buffer.as_str(), offset))
    }
}

fn utf8_offset_to_utf16(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    text[..offset].encode_utf16().count()
}

fn insert_typing(buffer: &mut Buffer, text: &str) {
    buffer.begin_edit_group();
    buffer.replace_selections(text);
}

fn replace_platform_text(buffer: &mut Buffer, range: Option<Range<usize>>, text: &str) {
    match range {
        Some(range)
            if buffer.marked_range().is_none() && range == buffer.primary_selection().range() =>
        {
            insert_typing(buffer, text);
        }
        Some(range) => {
            buffer.replace_range(range, text);
        }
        None if buffer.marked_range().is_some() => {
            buffer.update_composition(text, Selection::caret(text.len()));
            buffer.commit_composition();
        }
        None => insert_typing(buffer, text),
    }
}

fn utf16_offset_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    for (utf8, character) in text.char_indices() {
        if utf16 >= offset {
            return utf8;
        }
        let next = utf16 + character.len_utf16();
        if next > offset {
            return utf8;
        }
        utf16 = next;
    }
    text.len()
}

fn utf16_offset_to_utf8_ceil(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    for (utf8, character) in text.char_indices() {
        if utf16 >= offset {
            return utf8;
        }
        let next = utf16 + character.len_utf16();
        if next >= offset {
            return utf8 + character.len_utf8();
        }
        utf16 = next;
    }
    text.len()
}

fn utf8_range_to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    utf8_offset_to_utf16(text, range.start)..utf8_offset_to_utf16(text, range.end)
}

fn utf16_range_to_utf8(text: &str, range: Range<usize>) -> Range<usize> {
    let start = range.start.min(range.end);
    let end = range.start.max(range.end);
    utf16_offset_to_utf8(text, start)..utf16_offset_to_utf8_ceil(text, end)
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn inspector_text_area_retains_newlines_and_mode_changes_preserve_composition(
        cx: &mut gpui::TestAppContext,
    ) {
        // Return edits the value; changing its presentation never replaces an IME draft.
        let committed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = committed.clone();
        let editor = cx.new(|cx| {
            TextEditor::inspector_text_area("café\n日本語", cx).bound_edits(1024).on_commit(
                Arc::new(move |text, revision, _| {
                    received.lock().unwrap().push((text.to_owned(), revision));
                    true
                }),
            )
        });
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.text(), "café\n日本語");
            assert_eq!(editor.input_text("\n🚀\r\n").unwrap(), "\n🚀\r\n");
            editor.buffer.set_selection(Selection::caret(editor.text().len()));
            editor.insert_newline(cx);
            assert_eq!(editor.text(), "café\n日本語\n");
            assert!(committed.lock().unwrap().is_empty());
            assert_eq!(editor.revision.current, 1);
            editor.commit(cx);
            assert_eq!(committed.lock().unwrap().as_slice(), &[(editor.text().to_owned(), 1)]);
            editor.buffer.update_composition("é\n", Selection::caret("é\n".len()));
            let value = editor.text().to_owned();
            let selection = editor.buffer.primary_selection();
            let marked = editor.buffer.marked_range();
            editor.set_validation_underline(Some(0..1), cx);
            editor.set_binary(true, cx);
            editor.set_binary(false, cx);
            editor.acknowledge(None, "a different model value", cx);
            assert_eq!(editor.text(), value);
            assert_eq!(editor.buffer.primary_selection(), selection);
            assert_eq!(editor.buffer.marked_range(), marked);
            editor.set_validation_underline(None, cx);
            assert_eq!(editor.buffer.marked_range(), marked);
            assert_eq!(editor.revision.current, 1);
            editor.stop_caret_clock();
        });
        let ordinary = cx.new(|cx| TextEditor::new("single\nline", cx));
        ordinary.update(cx, |editor, _| {
            assert_eq!(editor.text(), "singleline");
            assert_eq!(editor.input_text("a\nb\r").unwrap(), "ab");
        });
    }

    #[gpui::test]
    fn bounded_binary_text_area_copies_and_pastes_a_complete_writable_value(
        cx: &mut gpui::TestAppContext,
    ) {
        // The formatted 60-KiB value exceeds an ordinary field's clipboard budget.
        let line = "00010203 04050607 08090a0b 0c0d0e0f";
        let text = vec![line; 3840].join("\n");
        assert_eq!(text.len(), 138239);
        let editor = cx.new(|cx| TextEditor::inspector_text_area(&text, cx).bound_edits(138239));
        editor.update(cx, |editor, cx| {
            editor.set_binary(true, cx);
            editor.select_all(cx);
            editor.copy(cx);
            editor.clear(cx);
            editor.paste(cx);
            assert_eq!(editor.text(), text);
            cx.write_to_clipboard(ClipboardItem::new_string("00".into()));
            let revision = editor.revision.current;
            editor.paste(cx);
            assert_eq!(editor.text(), text);
            assert_eq!(editor.revision.current, revision);
            editor.select_all(cx);
            let spaced = "00 ".repeat(60 * 1024);
            cx.write_to_clipboard(ClipboardItem::new_string(spaced));
            editor.paste(cx);
            assert_eq!(editor.text(), hex::Format::Grouped.display(&vec![0; 60 * 1024]));
            editor.toggle_hex_insert(cx);
            editor.buffer.set_selection(Selection::caret(0));
            let revision = editor.revision.current;
            editor.type_hex(hex::Format::Grouped, 0..0, "a", cx);
            assert_eq!(editor.text(), hex::Format::Grouped.display(&vec![0; 60 * 1024]));
            assert_eq!(editor.revision.current, revision);
            editor.select_all(cx);
            editor.copy(cx);
        });
        let ordinary = cx.new(|cx| TextEditor::new("", cx).bound_edits(138239));
        ordinary.update(cx, |editor, cx| {
            editor.paste(cx);
            assert!(editor.text().is_empty(), "ordinary fields retain their clipboard bound");
        });
    }

    #[gpui::test]
    fn binary_editor_uses_byte_selections_and_keeps_rows_aligned_through_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(install_keybindings);
        let bytes: Vec<_> = (0..18).collect();
        let window = cx.add_window(|window, cx| {
            let mut editor =
                TextEditor::inspector_text_area(hex::Format::Grouped.display(&bytes), cx)
                    .bound_edits(256);
            editor.set_binary(true, cx);
            window.focus(&editor.focus_handle);
            window.activate_window();
            editor
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        let caret = |visual: &mut gpui::VisualTestContext, offset| {
            window
                .update(visual, |editor, _, cx| {
                    editor.buffer.set_selection(Selection::caret(offset));
                    cx.notify();
                })
                .unwrap();
        };
        let check = |visual: &mut gpui::VisualTestContext, expected: &[u8]| {
            window
                .update(visual, |editor, _, _| {
                    assert_eq!(editor.text(), hex::Format::Grouped.display(expected));
                })
                .unwrap();
        };
        caret(&mut visual, 33);
        visual.simulate_input("A");
        visual.simulate_input("B");
        let mut expected = bytes.clone();
        expected[15] = 0xab;
        check(&mut visual, &expected);
        // The caret crossed the newline. Shift-Right and Cut select only byte 16.
        visual.simulate_keystrokes("shift-right cmd-x");
        expected.remove(16);
        check(&mut visual, &expected);
        window
            .update(&mut visual, |_, _, cx| {
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()).as_deref(),
                    Some("10")
                );
                cx.write_to_clipboard(ClipboardItem::new_string("FE \r\n 00".into()));
            })
            .unwrap();
        visual.simulate_keystrokes("cmd-v");
        expected.splice(16..16, [0xfe, 0x00]);
        check(&mut visual, &expected);
        visual.simulate_keystrokes("backspace");
        expected.remove(17);
        check(&mut visual, &expected);
        visual.simulate_keystrokes("cmd-z");
        expected.insert(17, 0x00);
        check(&mut visual, &expected);
        visual.simulate_keystrokes("cmd-shift-z");
        expected.remove(17);
        check(&mut visual, &expected);
        caret(&mut visual, 9);
        visual.simulate_keystrokes("delete");
        expected.remove(4);
        check(&mut visual, &expected);
    }

    #[gpui::test]
    fn inspector_text_area_uses_a_fixed_viewport_and_noneditable_hex_offsets(
        cx: &mut gpui::TestAppContext,
    ) {
        // Draw the fixed viewport to check wrapping, scrolling, gutter offsets and Copy.
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let line = "00010203 04050607 08090a0b 0c0d0e0f";
        let value = format!("{}\n1011", vec![line; 19].join("\n"));
        let window = cx.add_window(|_, cx| {
            let mut editor = TextEditor::inspector_text_area(value.clone(), cx).bound_edits(138239);
            editor.set_binary(true, cx);
            editor.set_validation_underline(Some(line.len() - 1..line.len()), cx);
            editor
        });
        let editor = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let viewport = size(px(160.), px(80.));
        let (_, state) = visual
            .draw(point(px(0.), px(0.)), viewport, |_, _| TextEditorElement::new(editor.clone()));
        assert_eq!(state.layout.bounds.size.height, viewport.height);
        assert_eq!(state.layout.bounds.left(), px(40.));
        assert_eq!(state.layout.rows.len(), 20);
        assert!(state.layout.content_width > state.layout.bounds.size.width);
        assert_eq!(
            state.layout.lines.iter().take(3).map(|line| line.hex_offset).collect::<Vec<_>>(),
            vec![Some(0), Some(16), Some(32)]
        );
        editor.update(&mut visual, |editor, cx| {
            editor.scroll_by(point(px(24.), px(16.)), cx);
            editor.select_all(cx);
            editor.copy(cx);
            assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some(value.clone()));
            // Selecting/copying does not turn a scroll into caret-following.
            editor.scroll_to_caret = false;
        });
        let (_, scrolled) = visual
            .draw(point(px(0.), px(0.)), viewport, |_, _| TextEditorElement::new(editor.clone()));
        assert_eq!(scrolled.layout.bounds, state.layout.bounds);
        assert_eq!(scrolled.layout.scroll_y, px(16.));
        assert_eq!(scrolled.layout.scroll_x, Pixels::ZERO);
        editor.update(&mut visual, |editor, cx| editor.set_binary(false, cx));
        let (_, wrapped) = visual
            .draw(point(px(0.), px(0.)), viewport, |_, _| TextEditorElement::new(editor.clone()));
        assert_eq!(wrapped.layout.bounds.size, viewport);
        assert!(wrapped.layout.rows.len() > wrapped.layout.lines.len());
        assert!(wrapped.layout.lines.iter().all(|line| line.hex_offset.is_none()));
        editor.update(&mut visual, |editor, cx| {
            editor.set_read_only(true, &playbit_api::ui::Theme::light(), cx);
            assert_eq!(editor.base_style.background, gpui::white());
            assert_eq!(editor.base_style.border_width, px(0.5));
            editor.set_trailing_padding(px(28.));
            editor.stop_caret_clock();
            cx.notify();
        });
        // The full rendered editor reserves the in-field toggle even on its final text line.
        visual.simulate_resize(viewport);
        visual.run_until_parked();
        editor.read_with(&visual, |editor, _| {
            assert!(editor.layout.as_ref().unwrap().bounds.right() <= viewport.width - px(28.));
            assert_eq!(editor.text(), value);
        });
    }

    #[gpui::test]
    fn reactivation_preserves_caret_and_tab_explicitly_selects_all(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(TextSelectionCoordinator::default()));
        let editor = cx.new(|cx| TextEditor::new("existing", cx));
        editor.update(cx, |editor, cx| {
            editor.buffer.set_selection(Selection::caret(3));
            editor.did_focus(cx);
            assert_eq!(editor.buffer.primary_selection(), Selection::caret(3));
            editor.stop_caret_clock();
            editor.did_focus(cx);
            assert_eq!(editor.buffer.primary_selection(), Selection::caret(3));
            editor.select_all(cx);
            assert_eq!(editor.buffer.primary_selection(), Selection::new(0, 8));
            editor.stop_caret_clock();
        });
    }

    #[gpui::test]
    fn selection_survives_window_deactivation(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(TextSelectionCoordinator::default()));
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let window = cx.add_window(|window, cx| {
            let editor = TextEditor::new("Two words", cx);
            window.focus(&editor.focus_handle);
            window.activate_window();
            editor
        });
        cx.run_until_parked();
        window
            .update(cx, |editor, _, _| editor.buffer.set_selection(Selection::new(4, 9)))
            .unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.deactivate_window();
        window
            .update(cx, |editor, window, _| {
                assert_eq!(editor.buffer.primary_selection(), Selection::new(4, 9));
                window.activate_window();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |editor, _, _| {
                assert_eq!(editor.buffer.primary_selection(), Selection::new(4, 9))
            })
            .unwrap();
    }

    #[gpui::test]
    fn inactive_editor_cannot_reclaim_focus_from_another_window(cx: &mut gpui::TestAppContext) {
        // Render the inactive editor after selecting text in another window.
        // It must stay parked, preserve that selection, and leave its clock off.
        cx.update(|cx| cx.set_global(TextSelectionCoordinator::default()));
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let first = cx.add_window(|window, cx| {
            let editor = TextEditor::new("First", cx);
            window.focus(&editor.focus_handle);
            window.activate_window();
            editor
        });
        cx.run_until_parked();
        gpui::VisualTestContext::from_window(first.into(), cx).deactivate_window();
        let second = cx.add_window(|window, cx| {
            let editor = TextEditor::new("Second", cx);
            window.focus(&editor.focus_handle);
            window.activate_window();
            editor
        });
        cx.run_until_parked();
        second.update(cx, |editor, _, cx| editor.select_all(cx)).unwrap();
        first.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.run_until_parked();
        first.update(cx, |editor, _, _| assert!(!editor.caret_blink_active)).unwrap();
        second
            .update(cx, |editor, _, _| {
                assert!(editor.caret_blink_active);
                assert_eq!(editor.buffer.primary_selection(), Selection::new(0, 6));
            })
            .unwrap();
    }

    #[gpui::test]
    fn placeholder_updates_preserve_pending_draft_and_selection(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| TextEditor::new("Café 👩🏽‍💻", cx).placeholder("Before"));
        editor.update(cx, |editor, cx| {
            editor.revision.changed();
            editor.select_all(cx);
            let selection = editor.buffer.primary_selection();
            editor.set_placeholder("After source update");
            editor.set_placeholder("After source update");
            assert_eq!(
                editor.placeholder.as_ref().map(|text| text.as_ref()),
                Some("After source update")
            );
            assert_eq!(editor.text(), "Café 👩🏽‍💻");
            assert_eq!(editor.buffer.primary_selection(), selection);
            assert!(editor.revision.pending());
        });
    }

    #[gpui::test]
    #[ignore = "optimized placeholder property benchmark; excludes shaping and presentation"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "The ignored timing diagnostic requires an optimized build."
    )]
    fn benchmark_placeholder_update(cx: &mut gpui::TestAppContext) {
        assert!(!cfg!(debug_assertions), "run placeholder benchmarks with --release");
        for bytes in [128, 4096] {
            let values = ["a".repeat(bytes), "b".repeat(bytes)];
            for changed in [false, true] {
                let editor = cx
                    .new(|cx| TextEditor::new("retained draft", cx).placeholder(values[0].clone()));
                editor.update(cx, |editor, _| {
                    let mut times = Vec::with_capacity(1000);
                    for n in 0..1000 {
                        let value = std::hint::black_box(&values[if changed { n % 2 } else { 0 }]);
                        let start = Instant::now();
                        editor.set_placeholder(value);
                        times.push(start.elapsed().as_nanos());
                        std::hint::black_box(&editor.placeholder);
                    }
                    times.sort_unstable();
                    crate::diagnostic!(
                        "placeholder: bytes={bytes} changed={changed} n=1000 p50={} p99={} \
                         max={} ns (property update only)",
                        times[499],
                        times[989],
                        times[999]
                    );
                    assert!(times[989] < 20_000);
                    assert_eq!(editor.text(), "retained draft");
                });
            }
        }
    }

    #[gpui::test]
    fn obsolete_acknowledgment_cannot_roll_back_an_accepted_unicode_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        let editor = cx.new(|cx| TextEditor::new("Café 👩🏽‍💻", cx));
        editor.update(cx, |editor, cx| {
            editor.revision.changed();
            editor.revision.changed();
            editor.acknowledge(Some(2), "Café 👩🏽‍💻", cx);
            editor.select_all(cx);
            let selection = editor.buffer.primary_selection();
            editor.acknowledge(Some(1), "Caf", cx);
            assert_eq!(editor.text(), "Café 👩🏽‍💻");
            assert_eq!(editor.buffer.primary_selection(), selection);
            // Submit may use the same revision as its preceding draft. Its
            // authoritative result must still be allowed to clear the field.
            editor.acknowledge(Some(2), "", cx);
            assert_eq!(editor.text(), "");
        });
    }

    #[gpui::test]
    fn unknown_future_acknowledgment_does_not_accept_current_draft(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| TextEditor::new("new draft", cx));
        editor.update(cx, |editor, cx| {
            editor.revision.changed();
            editor.acknowledge(Some(9), "another editor", cx);
            assert_eq!(editor.text(), "new draft");
            assert!(editor.revision.pending());
            editor.acknowledge(Some(1), "new draft", cx);
            assert!(!editor.revision.pending());
        });
    }

    #[gpui::test]
    #[ignore = "optimized draft acknowledgment benchmark; excludes shaping and presentation"]
    #[allow(
        clippy::assertions_on_constants,
        reason = "The ignored timing diagnostic requires an optimized build."
    )]
    fn benchmark_text_acknowledgment(cx: &mut gpui::TestAppContext) {
        assert!(!cfg!(debug_assertions), "run acknowledgment benchmarks with --release");
        for bytes in [128, 4096] {
            let value = "a".repeat(bytes);
            let editor = cx.new(|cx| TextEditor::new(value.clone(), cx));
            editor.update(cx, |editor, cx| {
                editor.revision.current = 2;
                editor.revision.accepted = 2;
                for (kind, revision, response) in [
                    ("accepted", 2, value.as_str()),
                    ("obsolete", 1, "old"),
                    ("future", 3, "unknown"),
                ] {
                    let mut samples = Vec::with_capacity(1000);
                    for _ in 0..1000 {
                        let start = std::time::Instant::now();
                        editor.acknowledge(
                            Some(std::hint::black_box(revision)),
                            std::hint::black_box(response),
                            cx,
                        );
                        samples.push(start.elapsed().as_nanos());
                        assert_eq!(editor.text(), value);
                    }
                    samples.sort_unstable();
                    crate::diagnostic!(
                        "text_ack kind={kind} bytes={bytes} n=1000 p50={} p95={} p99={} max={} ns",
                        samples[499],
                        samples[949],
                        samples[989],
                        samples[999]
                    );
                    assert!(samples[989] < 20_000, "draft acknowledgment exceeds20 µs p99");
                }
            });
        }
    }

    // Reset an input for a new host selection, load its value, and reject an
    // acknowledgment from its previous use without changing the fresh text.
    #[gpui::test]
    fn fresh_input_accepts_its_host_value_and_rejects_previous_acknowledgments(
        cx: &mut gpui::TestAppContext,
    ) {
        let editor = cx.new(|cx| TextEditor::new("previous", cx));
        editor.update(cx, |editor, cx| {
            editor.revision.changed();
            let previous = editor.revision.current;
            editor.clear(cx);
            assert!(!editor.revision.pending());
            editor.acknowledge(None, "selected entry", cx);
            assert_eq!(editor.text(), "selected entry");
            editor.acknowledge(Some(previous), "delayed old value", cx);
            assert_eq!(editor.text(), "selected entry");
        });
    }

    #[gpui::test]
    fn accepted_submission_finishes_rejected_change_delivery(cx: &mut gpui::TestAppContext) {
        // A rejected edit followed by an accepted Submit must not reappear in a later edit.
        let editor = cx.new(|cx| {
            TextEditor::new("a", cx)
                .on_edit(Arc::new(|_, _, _, _| false))
                .on_commit(Arc::new(|_, _, _| true))
        });
        editor.update(cx, |editor, cx| {
            editor.buffer.insert("a");
            editor.did_change(cx);
            assert_eq!(editor.buffer.pending_edits().len(), 1);
            editor.commit(cx);
            assert!(editor.buffer.pending_edits().is_empty());
            editor.buffer.insert("b");
            editor.did_change(cx);
            assert_eq!(
                editor.buffer.pending_edits(),
                &[playbit_api::abi::text::TextEdit { start: 1, end: 1, inserted_len: 1 }]
            );
        });
    }

    #[gpui::test]
    fn restarted_vm_receives_acknowledged_draft_without_losing_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let delivered = attempts.clone();
        let editor = cx.new(|cx| {
            TextEditor::new("café 日本語", cx).on_change(Arc::new(move |text, revision, _| {
                delivered.lock().unwrap().push((text.to_owned(), revision));
                true
            }))
        });
        editor.update(cx, |editor, cx| {
            editor.select_all(cx);
            let selection = editor.buffer.primary_selection();
            editor.invalidate_delivery();
            editor.acknowledge(Some(0), "", cx);
            assert_eq!(editor.text(), "café 日本語");
            assert_eq!(editor.buffer.primary_selection(), selection);
            assert!(editor.send_draft(cx));
            assert!(editor.send_draft(cx), "one resend per revision");
            editor.acknowledge(Some(1), "café 日本語", cx);
            editor.acknowledge(None, "submitted", cx);
            assert_eq!(
                editor.text(),
                "submitted",
                "normal authoritative updates resume after acknowledgment"
            );
        });
        assert_eq!(*attempts.lock().unwrap(), [("café 日本語".into(), 1)]);
    }

    #[gpui::test]
    fn recovery_draft_survives_failed_admission_and_late_acknowledgment(
        cx: &mut gpui::TestAppContext,
    ) {
        let accept = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let allowed = accept.clone();
        let editor = cx.new(|cx| {
            TextEditor::new("retained", cx).on_change(Arc::new(move |_, _, _| {
                allowed.load(std::sync::atomic::Ordering::Acquire)
            }))
        });
        editor.update(cx, |editor, cx| {
            editor.revision.changed();
            editor.revision.sent = 1;
            editor.invalidate_delivery();
            assert!(!editor.send_draft(cx));
            editor.acknowledge(Some(1), "old VM", cx);
            assert_eq!(editor.text(), "retained");
            assert!(editor.revision.pending());
            accept.store(true, std::sync::atomic::Ordering::Release);
            assert!(editor.send_draft(cx));
            editor.acknowledge(Some(2), "retained", cx);
            assert!(!editor.revision.pending());
        });
    }

    #[test]
    fn stale_ack_cannot_replace_a_newer_edit_even_when_text_matches_again() {
        let mut revision = EditRevision::default();
        revision.changed(); // a
        revision.sent = revision.current;
        revision.changed(); // ab
        revision.changed(); // a again
        revision.accept(1);
        assert!(revision.pending(), "string equality cannot acknowledge a later revision");
        revision.sent = revision.current;
        revision.accept(3);
        assert!(!revision.pending());
        revision.accept(2);
        assert!(!revision.pending(), "late acknowledgments cannot move backward");
        assert!(std::mem::size_of::<EditRevision>() <= 32);
    }
    #[test]
    fn utf8_and_utf16_offsets_round_trip_at_character_boundaries() {
        let text = "aé🧑‍🚀z";
        for offset in 0..=text.len() {
            if text.is_char_boundary(offset) {
                let utf16 = utf8_offset_to_utf16(text, offset);
                assert_eq!(utf16_offset_to_utf8(text, utf16), offset);
            }
        }
    }

    #[test]
    fn consecutive_platform_typing_is_one_undo_transaction() {
        let mut buffer = Buffer::new();
        for character in ["r", "e", "p", "l", "a", "c", "e"] {
            insert_typing(&mut buffer, character);
        }
        assert_eq!(buffer.as_str(), "replace");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
        assert!(!buffer.undo());
    }

    #[test]
    fn current_selection_replacement_ranges_are_grouped_as_typing() {
        let mut buffer = Buffer::new();
        for character in ["r", "e", "p", "l", "a", "c", "e"] {
            let selection = buffer.primary_selection().range();
            replace_platform_text(&mut buffer, Some(selection), character);
        }
        assert_eq!(buffer.as_str(), "replace");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
        assert!(!buffer.undo());
    }

    #[test]
    fn differing_replacement_ranges_are_standalone_edits() {
        let mut buffer = Buffer::new();
        insert_typing(&mut buffer, "a");
        insert_typing(&mut buffer, "b");
        replace_platform_text(&mut buffer, Some(0..1), "X");
        assert_eq!(buffer.as_str(), "Xb");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "ab");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
    }

    #[test]
    fn utf16_offsets_inside_surrogate_pairs_clamp_to_character_start() {
        let text = "a🚀b";
        assert_eq!(utf16_offset_to_utf8(text, 1), 1);
        assert_eq!(utf16_offset_to_utf8(text, 2), 1);
        assert_eq!(utf16_offset_to_utf8(text, 3), 5);
    }

    #[test]
    fn utf16_ranges_expand_around_partial_surrogate_pairs() {
        let text = "a🚀b";
        assert_eq!(utf16_range_to_utf8(text, 1..2), 1..5);
        assert_eq!(utf16_range_to_utf8(text, 2..3), 1..5);
    }

    #[test]
    fn reversed_utf16_ranges_are_normalized() {
        let text = "a🚀b";
        let start = 4;
        let end = 1;
        assert_eq!(utf16_range_to_utf8(text, start..end), 1..6);
    }

    #[test]
    fn pointer_word_selection_uses_the_rendered_unit_not_the_nearest_caret() {
        let buffer = Buffer::from("Roasted squash with sage");
        let start = buffer.as_str().find("with").unwrap();
        let end = start + "with".len();
        let hit = HitTest {
            offset: end,
            affinity: CaretAffinity::Downstream,
            unit_offset: Some(end - 1),
            after_text_on_row: false,
        };

        assert_eq!(
            selection_for_pointer(&buffer, hit, DragGranularity::Word),
            Selection::new(start, end)
        );
    }

    #[test]
    fn pointer_grapheme_selection_includes_the_whole_rendered_cluster() {
        let buffer = Buffer::from("a🚀b");
        let hit = HitTest {
            offset: "a🚀".len(),
            affinity: CaretAffinity::Downstream,
            unit_offset: Some("a".len()),
            after_text_on_row: false,
        };

        assert_eq!(
            selection_for_pointer(&buffer, hit, DragGranularity::Grapheme),
            Selection::new("a".len(), "a🚀".len())
        );
    }

    #[test]
    fn word_selection_in_blank_row_space_stays_at_the_insertion_caret() {
        let buffer = Buffer::from("one two");
        let hit = HitTest {
            offset: buffer.as_str().len(),
            affinity: CaretAffinity::Downstream,
            unit_offset: None,
            after_text_on_row: false,
        };

        assert_eq!(
            selection_for_pointer(&buffer, hit, DragGranularity::Word),
            Selection::caret(buffer.as_str().len())
        );
    }

    #[test]
    fn clicking_after_field_text_selects_last_word_or_all_without_moving_single_click() {
        let buffer = Buffer::from("one last word");
        let hit = HitTest {
            offset: buffer.as_str().len(),
            affinity: CaretAffinity::Downstream,
            unit_offset: None,
            after_text_on_row: true,
        };
        assert_eq!(selection_for_click(&buffer, hit, 1).1, Selection::caret(13));
        assert_eq!(selection_for_click(&buffer, hit, 2).1, Selection::new(9, 13));
        assert_eq!(selection_for_click(&buffer, hit, 3).1, Selection::new(0, 13));
    }

    #[test]
    fn clicking_after_unicode_text_skips_trailing_nonwords() {
        // Reverse word traversal must preserve UTF-8 offsets past spaces and emoji.
        for (text, word) in
            [("one café   ", "café"), ("first\nlast résumé\t", "résumé"), ("👩‍💻 café ☕", "café")]
        {
            let buffer = Buffer::from(text);
            let hit = HitTest {
                offset: text.len(),
                affinity: CaretAffinity::Downstream,
                unit_offset: None,
                after_text_on_row: true,
            };
            let start = text.rfind(word).unwrap();
            assert_eq!(
                selection_for_click(&buffer, hit, 2).1,
                Selection::new(start, start + word.len())
            );
        }
    }

    #[test]
    fn typing_undo_splits_on_word_boundary_and_short_pause() {
        let start = Instant::now();
        let mut buffer = Buffer::new();
        let mut last = None;
        for (delay, text) in [(0, "one"), (20, " "), (40, "two"), (450, "three")] {
            split_typing_group(
                &mut buffer,
                &mut last,
                &None,
                text,
                start + Duration::from_millis(delay),
            );
            insert_typing(&mut buffer, text);
        }
        assert_eq!(buffer.as_str(), "one twothree");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "one two");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "one ");
        assert!(buffer.undo());
        assert_eq!(buffer.as_str(), "");
    }

    #[test]
    fn focus_change_collapses_all_selections_at_the_primary_head() {
        for primary in [Selection::new(0, 3), Selection::new(3, 0)] {
            let mut buffer = Buffer::from("one two");
            buffer.set_selections_with_primary(primary, [Selection::caret(7)]);

            assert!(clear_selections_for_focus_change(&mut buffer));
            assert_eq!(buffer.selections(), &[Selection::caret(primary.head)]);
            assert_eq!(buffer.as_str(), "one two");
            assert!(!clear_selections_for_focus_change(&mut buffer));
        }
    }

    #[test]
    fn caret_blink_stays_visible_after_activity_then_alternates() {
        assert_eq!(caret_blink_state(Duration::ZERO), (true, CARET_BLINK_DELAY));
        assert_eq!(
            caret_blink_state(CARET_BLINK_DELAY - Duration::from_millis(1)),
            (true, Duration::from_millis(1))
        );
        assert_eq!(caret_blink_state(CARET_BLINK_DELAY), (false, CARET_BLINK_INTERVAL));
        assert_eq!(
            caret_blink_state(CARET_BLINK_DELAY + Duration::from_millis(250)),
            (false, Duration::from_millis(250))
        );
        assert_eq!(
            caret_blink_state(CARET_BLINK_DELAY + CARET_BLINK_INTERVAL),
            (true, CARET_BLINK_INTERVAL)
        );
    }

    #[test]
    fn visual_row_boundaries_use_the_next_row_for_y_and_affinity_selects_an_edge() {
        let layout = LayoutSnapshot {
            email_code: false,
            bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(40.0))),
            lines: Vec::new(),
            rows: vec![
                LayoutRow {
                    line_index: 0,
                    range: 0..4,
                    top: px(0.0),
                    start_x: px(0.0),
                    newline_after: false,
                },
                LayoutRow {
                    line_index: 0,
                    range: 4..8,
                    top: px(20.0),
                    start_x: px(40.0),
                    newline_after: false,
                },
            ],
            line_height: px(20.0),
            scale: 1.,
            content_height: px(40.0),
            content_width: px(100.0),
            scroll_x: px(0.0),
            scroll_y: px(0.0),
            text_len: 8,
        };

        assert_eq!(layout.row_index_for_content_y(px(19.0)), Some(0));
        assert_eq!(layout.row_index_for_content_y(px(20.0)), Some(1));
        assert_eq!(layout.row_for_offset(4, CaretAffinity::Upstream).unwrap().range.clone(), 0..4);
        assert_eq!(
            layout.row_for_offset(4, CaretAffinity::Downstream).unwrap().range.clone(),
            4..8
        );
    }

    #[test]
    fn focused_outline_uses_the_focused_width_outside_the_surface() {
        let style = TextEditorStyle {
            corner_radius: px(1.0),
            border_width: px(0.5),
            border_width_focused: px(2.0),
            ..TextEditorStyle::default()
        };

        assert_eq!(outline_width(&style, false), px(0.5));
        assert_eq!(style.corner_radius + outline_width(&style, false), px(1.5));
        assert_eq!(outline_width(&style, true), px(2.0));
        assert_eq!(style.corner_radius + outline_width(&style, true), px(3.0));
        assert_eq!(surface_paint_radius(style.corner_radius), px(0.5));
        assert_eq!(surface_paint_radius(px(6.0)), px(6.0));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod accessibility_tests {
    use super::*;

    #[gpui::test]
    fn empty_code_caret_is_centered_and_code_typography_survives_read_only(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_window(|window, cx| {
            let editor = TextEditor::new("", cx).email_code();
            window.focus(&editor.focus_handle);
            window.activate_window();
            editor
        });
        cx.run_until_parked();
        window
            .update(cx, |editor, _, cx| {
                let layout = editor.layout.as_ref().unwrap();
                let caret = layout.position_for_offset(0, CaretAffinity::Downstream).unwrap();
                assert_eq!(caret.x, layout.bounds.left() + px(8.));
                assert!(editor.caret_visible && editor.caret_blink_active);
                for read_only in [true, false] {
                    editor.set_read_only(read_only, &playbit_api::ui::Theme::light(), cx);
                    assert_eq!(editor.base_style.font_size, px(16.));
                }
                assert!(tabular_font().features.tag_value_list().contains(&("tnum".into(), 1)));
            })
            .unwrap();
    }

    #[gpui::test]
    fn code_hit_testing_and_selection_follow_zoom(cx: &mut gpui::TestAppContext) {
        // Paint, hit-test and selection use the same scaled code-cell geometry.
        let window = cx.add_window(|_, cx| TextEditor::new("123456", cx).email_code());
        for units in zoom::SAMPLES {
            let scale = units as f32 / 10_000.;
            window
                .update(cx, |_, window, _| {
                    window.set_rem_size(px(16. * scale));
                    window.refresh();
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |editor, _, _| {
                    let layout = editor.layout.as_ref().unwrap();
                    for i in 0..6 {
                        let position =
                            layout.position_for_offset(i, CaretAffinity::Downstream).unwrap();
                        assert!(
                            (position.x - layout.bounds.left() - code_x(i) * scale).abs()
                                <= px(0.5)
                        );
                        assert_eq!(layout.hit_test(position).offset, i);
                    }
                })
                .unwrap();
        }
    }

    // Bound both platform replacement and IME before allocating normalized
    // input; a rejected acknowledgment cannot broaden the editor's bound.
    #[gpui::test]
    fn bounded_editor_preserves_accepted_text_on_oversized_input(cx: &mut gpui::TestAppContext) {
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("café", cx).bound_edits(8));
            editor.update(cx, |editor, cx| {
                editor.replace_text_in_range(Some(0..5), "123456789", window, cx);
                editor.replace_and_mark_text_in_range(Some(0..5), "123456789", None, window, cx);
                editor.acknowledge(None, "123456789", cx);
                assert_eq!(editor.text(), "café");
                assert!(editor.buffer.marked_range().is_none());
                editor.replace_text_in_range(Some(0..5), "日本", window, cx);
                assert_eq!(editor.text(), "日本");
            });
        });
    }
    #[gpui::test]
    fn grouped_code_paste_preserves_zeroes_selection_and_rejects_overflow(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("", cx).email_code().bound_edits(6));
            editor.update(cx, |editor, cx| {
                editor.replace_text_in_range(None, "001–002", window, cx);
                assert_eq!(editor.text(), "001002");
                editor.replace_text_in_range(None, "3", window, cx);
                assert_eq!(editor.text(), "001002");
                editor.replace_text_in_range(Some(3..6), "003", window, cx);
                assert_eq!(editor.text(), "001003");
                editor.replace_text_in_range(Some(0..6), "１２３４５６", window, cx);
                assert_eq!(editor.text(), "001003");
                editor.select_all(cx);
                editor.copy(cx);
                assert_eq!(cx.read_from_clipboard().unwrap().text().as_deref(), Some("001003"));
                editor.replace_text_in_range(Some(0..6), "000-004", window, cx);
                assert_eq!(editor.text(), "000004");
            });
        });
    }

    #[gpui::test]
    fn read_only_text_preserves_selection_and_copy_but_rejects_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("Recovery key", cx).read_only());
            let request = gpui::accesskit::ActionRequest {
                action: gpui::accesskit::Action::SetValue,
                target_tree: gpui::accesskit::TreeId::ROOT,
                target_node: accessibility_text_ids(editor.entity_id()).0,
                data: Some(gpui::accesskit::ActionData::Value("Changed".into())),
            };
            editor.update(cx, |editor, cx| {
                editor.select_all(cx);
                editor.copy(cx);
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "Recovery key");
                editor.cut(cx);
                editor.paste(cx);
                editor.replace_text_in_range(None, "Changed", window, cx);
                editor.replace_and_mark_text_in_range(None, "日本語", None, window, cx);
                editor.accessibility_action(&request, window, cx);
                assert_eq!(editor.text(), "Recovery key");
                assert_eq!(editor.buffer.primary_selection().range(), 0..12);
                assert!(editor.buffer.marked_range().is_none());
                assert!(!editor.revision.pending());
            });
        });
    }

    struct IdentifierField {
        editor: Entity<TextEditor>,
    }

    impl Render for IdentifierField {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .w(px(240.))
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(gpui::black().alpha(0.7))
                .child(self.editor.clone())
        }
    }

    #[gpui::test]
    fn selectable_identifier_requests_font_features_and_copies_the_pointer_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        // Inspect the rendered font request, then drag a substring and Copy through real key bindings.
        cx.update(install_keybindings);
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(crate::ui::desktop::UI_FONT_DATA)])
            .unwrap();
        let value = crate::identifier::encode(&[17; 16]);
        let window = cx.add_window(|window, cx| {
            window.activate_window();
            IdentifierField { editor: cx.new(|cx| TextEditor::new(&value, cx).selectable_id()) }
        });
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| drop(window.draw(cx)));
        let (start, end) = window
            .update(&mut visual, |field, window, cx| {
                let editor = field.editor.read(cx);
                let layout = editor.layout.as_ref().unwrap();
                assert_eq!(layout.line_height, px(16.));
                assert_eq!(editor.style.font_size, px(11.));
                assert_eq!(editor.style.text, gpui::black().alpha(0.7));
                assert_eq!(editor.style.padding, px(0.));
                assert_eq!(editor.style.border_width, px(0.));
                assert_eq!(editor.style.border_width_focused, px(0.));
                assert_eq!(editor.style.background, gpui::transparent_black());
                let runs = &layout.lines[0].shaped.unwrapped_layout.runs;
                assert!(!runs.is_empty(), "the identifier must have a real shaped run");
                // NoopTextSystem resolves input fonts to 1 but fabricates output runs with 0.
                // Inspect the descriptor actually registered by the rendered shaping request.
                let font = window.text_system().get_font_for_id(gpui::FontId(1)).unwrap();
                assert_eq!(font.family.as_ref(), crate::ui::desktop::UI_FONT_FAMILY);
                assert_eq!(font.weight, gpui::FontWeight::MEDIUM);
                assert_eq!(
                    font.features.tag_value_list(),
                    &[("ss02".into(), 1), ("tnum".into(), 1)]
                );
                let position = |offset| {
                    let mut position =
                        layout.position_for_offset(offset, CaretAffinity::Downstream).unwrap();
                    position.y += layout.line_height / 2.;
                    position
                };
                // Dragging includes the grapheme under the pointer. End inside byte 8,
                // rather than on byte 9's leading edge, to select exactly 3..9.
                let end = layout.rectangles_for_range(8..9).first().unwrap().center();
                (position(3), end)
            })
            .unwrap();
        visual.simulate_mouse_down(start, MouseButton::Left, Default::default());
        visual.simulate_mouse_move(end, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(end, MouseButton::Left, Default::default());
        visual.simulate_keystrokes("cmd-c");
        visual.run_until_parked();
        window
            .update(&mut visual, |field, window, cx| {
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), value[3..9]);
                field.editor.update(cx, |editor, cx| {
                    assert_eq!(editor.buffer.primary_selection().range(), 3..9);
                    editor.replace_text_in_range(None, "changed", window, cx);
                    assert_eq!(editor.text(), value);
                });
                let node = window
                    .accessibility_nodes_for_test()
                    .find(|(id, _)| *id == accessibility_text_ids(field.editor.entity_id()).0)
                    .unwrap()
                    .1;
                assert!(node.is_read_only());
                assert_eq!(node.label(), Some("ID"));
                assert_eq!(node.value(), Some(value.as_str()));
                assert!(window.accessibility_error().is_none());
            })
            .unwrap();
    }

    struct IdentifierNavigation {
        editor: Entity<TextEditor>,
        before: FocusHandle,
        after: FocusHandle,
    }

    impl Render for IdentifierNavigation {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .w(px(240.))
                .flex()
                .flex_col()
                .on_key_down(|event, window, cx| {
                    if event.keystroke.key == "tab" {
                        if event.keystroke.modifiers.shift {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                        cx.stop_propagation();
                    }
                })
                .child(gpui::div().id("before-id").track_focus(&self.before).h(px(24.)))
                .child(self.editor.clone())
                .child(gpui::div().id("after-id").track_focus(&self.after).h(px(24.)))
        }
    }

    #[gpui::test]
    fn tab_skips_selectable_identifiers_in_both_directions(cx: &mut gpui::TestAppContext) {
        // Dispatch real Tab through the rendered focus ancestry, with the ID between two stops.
        cx.update(install_keybindings);
        let window = cx.add_window(|window, cx| {
            let before = cx.focus_handle().tab_stop(true);
            before.focus(window);
            window.activate_window();
            IdentifierNavigation {
                editor: cx.new(|cx| TextEditor::new("123456789abcdefghijk", cx).selectable_id()),
                before,
                after: cx.focus_handle().tab_stop(true),
            }
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| drop(window.draw(cx)));
        for key in ["tab", "shift-tab", "tab", "shift-tab"] {
            visual.simulate_keystrokes(key);
            window
                .update(&mut visual, |fields, window, cx| {
                    let expected = if key == "tab" { &fields.after } else { &fields.before };
                    assert!(expected.is_focused(window), "{key} must skip the metadata ID");
                    assert!(!fields.editor.read(cx).focus_handle.is_focused(window));
                    assert_eq!(fields.editor.read(cx).text(), "123456789abcdefghijk");
                })
                .unwrap();
        }
    }

    struct IdentifierScroll {
        editor: Entity<TextEditor>,
        scroll: gpui::ScrollHandle,
    }

    impl Render for IdentifierScroll {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
                .id("identifier-ancestor")
                .w(px(200.))
                .h(px(100.))
                .overflow_scroll()
                .track_scroll(&self.scroll)
                .child(
                    gpui::div().relative().w(px(600.)).h(px(400.)).child(
                        gpui::div()
                            .absolute()
                            .left(px(80.))
                            .top(px(80.))
                            .w(px(60.))
                            .child(self.editor.clone()),
                    ),
                )
        }
    }

    #[gpui::test]
    fn wheel_over_a_clipped_identifier_scrolls_its_ancestor_on_both_axes(
        cx: &mut gpui::TestAppContext,
    ) {
        // Give the ID genuine internal overflow: both pixel and line wheels must reach the parent.
        let scroll = gpui::ScrollHandle::new();
        let window = cx.add_window(|_, cx| IdentifierScroll {
            editor: cx.new(|cx| TextEditor::new("123456789abcdefghijk", cx).selectable_id()),
            scroll: scroll.clone(),
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        for horizontal in [true, false] {
            for direction in [-1., 1.] {
                for lines in [false, true] {
                    let before = point(px(-40.), px(-40.));
                    visual.update(|window, cx| {
                        scroll.set_offset(before);
                        window.refresh();
                        drop(window.draw(cx));
                    });
                    let position = window
                        .update(&mut visual, |field, _, cx| {
                            let editor = field.editor.read(cx);
                            let layout = editor.layout.as_ref().unwrap();
                            assert!(layout.content_width > layout.bounds.size.width);
                            layout.bounds.center()
                        })
                        .unwrap();
                    let delta =
                        if horizontal { point(direction, 0.) } else { point(0., direction) };
                    visual.simulate_event(ScrollWheelEvent {
                        position,
                        delta: if lines {
                            gpui::ScrollDelta::Lines(delta)
                        } else {
                            gpui::ScrollDelta::Pixels(point(px(delta.x * 20.), px(delta.y * 20.)))
                        },
                        ..Default::default()
                    });
                    let after = scroll.offset();
                    let (changed, unchanged) = if horizontal {
                        (after.x - before.x, after.y - before.y)
                    } else {
                        (after.y - before.y, after.x - before.x)
                    };
                    assert!(changed * direction > px(0.));
                    assert_eq!(unchanged, px(0.));
                    window
                        .update(&mut visual, |field, _, cx| {
                            let editor = field.editor.read(cx);
                            assert_eq!((editor.scroll_x, editor.scroll_y), (px(0.), px(0.)));
                            assert_eq!(editor.text(), "123456789abcdefghijk");
                        })
                        .unwrap();
                }
            }
        }
    }

    #[gpui::test]
    fn disabled_or_retired_editor_rejects_native_value_actions(cx: &mut gpui::TestAppContext) {
        cx.add_empty_window().update(|window, cx| {
            let editor = cx.new(|cx| TextEditor::new("Original", cx));
            let request = gpui::accesskit::ActionRequest {
                action: gpui::accesskit::Action::SetValue,
                target_tree: gpui::accesskit::TreeId::ROOT,
                target_node: accessibility_text_ids(editor.entity_id()).0,
                data: Some(gpui::accesskit::ActionData::Value("Changed".into())),
            };
            editor.update(cx, |editor, cx| {
                editor.set_enabled(false);
                editor.accessibility_action(&request, window, cx);
                assert_eq!(editor.text(), "Original");
                editor.set_enabled(true);
                editor.accessibility_action(&request, window, cx);
                assert_eq!(editor.text(), "Changed");
            });
        });
    }

    #[test]
    fn native_selection_preserves_scalar_and_utf16_boundaries() {
        let text = "Aé😀e\u{301}";
        let offsets = (0..=5).map(|i| accessibility_offset(text, i).unwrap()).collect::<Vec<_>>();
        assert_eq!(offsets, [0, 1, 3, 7, 8, 10]);
        assert_eq!(accessibility_offset(text, 6), None);
        for offset in offsets {
            let utf16 = text[..offset].encode_utf16().count();
            assert_eq!(utf16_range_to_utf8(text, utf16..utf16), offset..offset);
        }
        let mut buffer = Buffer::from(text.to_owned());
        buffer.set_selection(Selection::new(10, 8));
        assert_eq!(buffer.primary_selection(), Selection::new(10, 8));
    }
}
