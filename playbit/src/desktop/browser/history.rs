//! Retained activity pages share the inspector's scroll viewport.
use super::*;
use crate::store::volume_index::{EntryAddress, EntryChange, EntryRevision, HistoryCursor};
use crate::ui::desktop::typography::tabular_numerals;
use crate::ui::desktop::zoom::{self, length as ui};
use std::ops::Range;

const PAGE_ROWS: u32 = 64;
// The canonical source scan already admits at most MAX_FACTS * 8 physical
// records, including schema revisions that are not namespace facts.
const MAX_HISTORY_ROWS: usize = crate::store::volume_index::namespace::MAX_FACTS * 8;
const ROW_HEIGHT: f32 = 24.;

#[derive(Default)]
pub(super) struct History {
    generation: u64,
    pending: Option<Pending>,
    rows: Vec<Row>,
    loaded: bool,
    next: Option<HistoryCursor>,
    error: String,
    list_top: Option<gpui::Pixels>,
    rendered: Range<usize>,
    near_end: bool,
}

struct Pending {
    admission: u64,
    generation: u64,
    address: EntryAddress,
    after: Option<HistoryCursor>,
}

struct Row {
    clock: u64,
    change: Activity,
}

enum Activity {
    Created,
    Name(String),
    Blob(u64),
    Database,
    Delete,
    Move(Option<String>),
    Schema,
}

impl Row {
    fn from_revision(record: EntryRevision) -> Self {
        let change = match record.change {
            EntryChange::Created { .. } => Activity::Created,
            EntryChange::Name { name } => Activity::Name(name),
            EntryChange::Content { kind: EntryKind::Blob { len_bytes, .. } } => {
                Activity::Blob(len_bytes)
            }
            EntryChange::Content { .. } => Activity::Database,
            EntryChange::Delete {} => Activity::Delete,
            EntryChange::Move { name, .. } => Activity::Move(name),
            EntryChange::Schema { .. } => Activity::Schema,
        };
        Self { clock: record.origin_clock, change }
    }
    fn label(&self) -> String {
        match &self.change {
            Activity::Created => "Created".into(),
            Activity::Name(name) => format!("Renamed to “{name}”"),
            Activity::Blob(bytes) => format!("File version · {bytes} bytes"),
            Activity::Database => "Database metadata".into(),
            Activity::Delete => "Deleted".into(),
            Activity::Move(name) => {
                name.as_ref().map_or_else(|| "Moved".into(), |name| format!("Moved to {name}"))
            }
            Activity::Schema => "Schema declaration".into(),
        }
    }
}

impl Browser {
    pub(super) fn reset_history(&mut self) {
        self.history.generation = self.history.generation.wrapping_add(1);
        self.history.rows.clear();
        self.history.loaded = false;
        self.history.next = None;
        self.history.error.clear();
        self.history.list_top = None;
        self.history.rendered = 0..0;
        self.history.near_end = false;
        // Preserve the old admission until its completion drains. Its generation
        // cannot populate this selection, and it cannot alias a newer request.
    }

    pub(in crate::desktop) fn retire_history(&mut self) {
        self.history.pending = None;
        self.reset_history();
    }

    pub(in crate::desktop) fn history_response_matches(&self, admission: Option<u64>) -> bool {
        self.history.pending.as_ref().is_some_and(|pending| Some(pending.admission) == admission)
    }

    pub(super) fn refresh_history(&mut self) -> bool {
        if self.history.loaded || !self.history.error.is_empty() {
            return false;
        }
        self.request_history(None)
    }

    fn request_history(&mut self, after: Option<HistoryCursor>) -> bool {
        if self.history.pending.is_some()
            || self.details.as_ref().is_none_or(|details| !details.retained_readable)
        {
            return false;
        }
        let Some(entry) = self.selected else { return false };
        let address = EntryAddress { volume: self.volume.id, entry };
        match self.send.enqueue(Request::EntryRevisions {
            session: self.request_session(),
            address,
            after,
            limit: PAGE_ROWS,
        }) {
            Ok(admission) => {
                self.history.pending = Some(Pending {
                    admission,
                    generation: self.history.generation,
                    address,
                    after,
                });
            }
            Err(error) => self.history.error = error.into(),
        }
        true
    }

    fn more_history(&mut self) -> bool {
        if !self.history.error.is_empty() {
            return false;
        }
        let Some(after) = self.history.next else { return false };
        self.request_history(Some(after))
    }

    pub(super) fn receive_history(&mut self, reply: Reply) -> Option<Reply> {
        let Reply::EntryRevisions { address, after, page } = reply else { return Some(reply) };
        let Some(pending) = &self.history.pending else { return None };
        if pending.address != address || pending.after != after {
            return None;
        }
        let current = pending.generation == self.history.generation
            && address.volume == self.volume.id
            && Some(address.entry) == self.selected;
        self.history.pending = None;
        if !current {
            self.refresh_history();
            return None;
        }
        if self.details.as_ref().is_none_or(|details| !details.retained_readable) {
            self.reset_history();
            return None;
        }
        if page.revisions.len() > MAX_HISTORY_ROWS.saturating_sub(self.history.rows.len()) {
            self.history.error = "History exceeds the volume’s retained-record limit.".into();
            return None;
        }
        self.history.rows.extend(page.revisions.into_iter().map(Row::from_revision));
        self.history.loaded = true;
        self.history.next = page.next;
        None
    }

    pub(super) fn failed_history(
        &mut self,
        address: EntryAddress,
        after: Option<HistoryCursor>,
        error: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = &self.history.pending else { return };
        if pending.address != address || pending.after != after {
            return;
        }
        let current = pending.generation == self.history.generation
            && address.volume == self.volume.id
            && Some(address.entry) == self.selected;
        self.history.pending = None;
        if current {
            self.history.error = error.into();
        } else {
            self.refresh_history();
        }
        cx.notify();
    }

    pub(super) fn history_panel(
        &mut self,
        details: &EntryDetails,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut section =
            div().flex().flex_col().flex_none().child(self.history_header(window_active, cx));
        if !details.retained_readable {
            return section.child(
                self.text("history-denied", "Read access is required to view history.", cx)
                    .pl(ui(12.))
                    .pr(ui(12. + self.scrolling.inspector_clearance(cx)))
                    .text_size(ui(11.))
                    .text_color(rgba(0x00000066)),
            );
        }
        section = section.child(self.history_list(cx));
        let status = if !self.history.error.is_empty() {
            Some(self.history.error.clone())
        } else if self.history.loaded && self.history.rows.is_empty() {
            Some("No recorded history".into())
        } else {
            None
        };
        if let Some(status) = status {
            section = section.child(
                self.text("history-status", status, cx)
                    .min_h(ui(ROW_HEIGHT))
                    .pl(ui(12.))
                    .pr(ui(12. + self.scrolling.inspector_clearance(cx)))
                    .text_size(ui(11.))
                    .text_color(rgba(0x00000066)),
            );
        }
        section
    }

    fn history_header(&mut self, window_active: bool, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .h(ui(44.))
            .flex_none()
            .border_t_1()
            .border_color(super::super::theme::column_divider_color())
            .flex()
            .items_center()
            .pl(ui(12.))
            .pr(ui(8.))
            .child(
                self.text("history-title", "History", cx)
                    .text_size(ui(14.))
                    .line_height(ui(24.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(super::super::theme::section_title_color(window_active)),
            )
    }

    fn history_list(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let handle = self.scrolling.permissions.read(cx).handle();
        let scale = zoom::factor(cx);
        let row_height = gpui::px(ROW_HEIGHT * scale);
        let count = self.history.rows.len();
        let range = self.history.list_top.map_or(0..0, |top| {
            visible_rows(top + handle.visual_offset().y, handle.bounds(), row_height, count)
        });
        self.history.rendered = range.clone();
        let mut list = div().relative().flex_none().w_full().h(ui(ROW_HEIGHT * count as f32));
        for (slot, index) in range.enumerate() {
            list = list.child(self.history_row(index, slot, cx));
        }
        list.child(self.history_viewport(count, row_height, handle, cx))
    }

    fn history_row(&mut self, index: usize, slot: usize, cx: &mut Context<Self>) -> gpui::Div {
        let record = &self.history.rows[index];
        let label = record.label();
        let time = history_time(record.clock);
        let mut row = div()
            .absolute()
            .top(ui(index as f32 * ROW_HEIGHT))
            .left(ui(4.))
            .right(ui(4. + self.scrolling.inspector_clearance(cx)))
            .h(ui(ROW_HEIGHT))
            .px(ui(8.))
            .flex()
            .items_center()
            .gap(ui(6.))
            .text_size(ui(11.))
            .line_height(ui(20.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(gpui::black().alpha(0.7))
            .child(
                self.text(&format!("history-row-{slot}"), label, cx)
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap(),
            );
        if !time.is_empty() {
            row = row.child(
                tabular_numerals(self.text(&format!("history-time-{slot}"), time, cx))
                    .flex_none()
                    .whitespace_nowrap(),
            );
        }
        row
    }

    fn history_viewport(
        &self,
        count: usize,
        row_height: gpui::Pixels,
        handle: gpui::ScrollHandle,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.weak_entity();
        let generation = self.history.generation;
        gpui::canvas(
            move |bounds, window, cx| {
                let Some(entity) = weak.upgrade() else { return };
                let viewport = handle.bounds();
                if viewport.size.height <= gpui::px(0.) {
                    return;
                }
                let range = visible_rows(bounds.top(), viewport, row_height, count);
                let top = bounds.top() - handle.visual_offset().y;
                let near_end = bounds.bottom() <= viewport.bottom() + row_height
                    && bounds.bottom() >= viewport.top() - row_height;
                let state = &entity.read(cx).history;
                if state.generation != generation || state.rows.len() != count {
                    return;
                }
                let load = near_end
                    && state.pending.is_none()
                    && state.next.is_some()
                    && state.error.is_empty();
                if state.list_top == Some(top)
                    && state.rendered == range
                    && state.near_end == near_end
                    && !load
                {
                    return;
                }
                let weak = entity.downgrade();
                window.defer(cx, move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.update_history_viewport(generation, count, top, range, near_end, cx);
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn update_history_viewport(
        &mut self,
        generation: u64,
        count: usize,
        top: gpui::Pixels,
        range: Range<usize>,
        near_end: bool,
        cx: &mut Context<Self>,
    ) {
        if self.history.generation != generation || self.history.rows.len() != count {
            return;
        }
        let changed = self.history.rendered != range || self.history.near_end != near_end;
        self.history.list_top = Some(top);
        self.history.rendered = range;
        self.history.near_end = near_end;
        let loading = near_end && self.more_history();
        if changed || loading {
            cx.notify();
        }
    }
}

fn visible_rows(
    top: gpui::Pixels,
    viewport: gpui::Bounds<gpui::Pixels>,
    row_height: gpui::Pixels,
    count: usize,
) -> Range<usize> {
    if row_height <= gpui::px(0.) || viewport.size.height <= gpui::px(0.) {
        return 0..0;
    }
    let start = (((viewport.top() - top) / row_height).floor().max(0.) as usize)
        .saturating_sub(1)
        .min(count);
    let end = (((viewport.bottom() - top) / row_height).ceil().max(0.) as usize)
        .saturating_add(1)
        .min(count);
    start..end.max(start)
}

/// The HLC's physical seconds are the recorded activity time, not file mtime.
#[allow(unsafe_code, reason = "Formats recorded HLC wall seconds with reentrant calendar APIs.")]
pub(super) fn history_time(clock: u64) -> String {
    let clock = kvdb::Clock::from_u64(clock);
    if clock.is_zero() {
        return String::new();
    }
    let seconds = kvdb::clock::EPOCH_UNIX_SECS + u64::from(clock.time);
    let Ok(seconds) = libc::time_t::try_from(seconds) else { return String::new() };
    let mut calendar = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut buffer = [0u8; 64];
    // SAFETY: Both calendar and buffer are valid output storage for these calls.
    let length = unsafe {
        if libc::localtime_r(&seconds, calendar.as_mut_ptr()).is_null() {
            return String::new();
        }
        libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%b %e, %Y at %H:%M".as_ptr(),
            calendar.as_ptr(),
        )
    };
    String::from_utf8_lossy(&buffer[..length]).replace("  ", " ")
}

#[cfg(test)]
mod tests;
