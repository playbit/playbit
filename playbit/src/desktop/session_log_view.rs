//! Session log rows, timestamps, run markers, history, and scrolling.
use super::*;
use crate::store::Id;
use crate::ui::desktop::scrollbar::Scrollbars;
use crate::ui::desktop::typography::tabular_numerals;
use crate::ui::desktop::zoom::length as ui;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Started,
    Stopped,
}

#[derive(Clone)]
struct RunMarker {
    before: usize,
    kind: MarkerKind,
    timestamp: String,
}

enum RowContent {
    Message(usize),
    Marker(usize),
}

struct Row {
    top: f32,
    content: RowContent,
}

#[derive(Clone, Default)]
struct Snapshot {
    lines: Arc<Vec<crate::ui::app_control::LogEntry>>,
    cursor: Option<crate::ui::app_control::LogCursor>,
    markers: Arc<Vec<RunMarker>>,
    last_owner: Option<Id>,
}

pub(super) struct LogView {
    pub(super) session: Option<String>,
    pub(super) lines: Arc<Vec<crate::ui::app_control::LogEntry>>,
    pub(super) log_cursor: Option<crate::ui::app_control::LogCursor>,
    history: BTreeMap<(String, Option<Id>), Snapshot>,
    instance: Option<Id>,
    last_owner: Option<Id>,
    pub(super) running: Option<bool>,
    markers: Arc<Vec<RunMarker>>,
    pub(super) notice: Option<&'static str>,
    scroll: gpui::ScrollHandle,
    scrollbars: Entity<Scrollbars>,
    text_ids: Arc<Vec<Entity<()>>>,
    timestamps: Arc<Vec<String>>,
    rows: Arc<Vec<Row>>,
    height: f32,
}
impl LogView {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let scrollbars = cx.new(|_| Scrollbars::new(false));
        let scroll = scrollbars.read(cx).handle();
        Self {
            session: None,
            lines: Arc::new(Vec::new()),
            log_cursor: None,
            history: BTreeMap::new(),
            instance: None,
            last_owner: None,
            running: None,
            markers: Arc::new(Vec::new()),
            notice: None,
            scroll,
            scrollbars,
            text_ids: Arc::new(Vec::new()),
            timestamps: Arc::new(Vec::new()),
            rows: Arc::new(Vec::new()),
            height: 0.,
        }
    }
    pub(super) fn select(&mut self, session: &str, instance: Option<Id>, cx: &mut Context<Self>) {
        if self.session.as_deref() == Some(session) && self.instance == instance {
            return;
        }
        if let Some(previous) = self.session.take() {
            // At most the same 256 identities admitted by the sidebar, 64 KiB of messages each.
            if self.history.len() == 255 {
                self.history.pop_first();
            }
            self.history.insert(
                (previous, self.instance),
                Snapshot {
                    lines: self.lines.clone(),
                    cursor: self.log_cursor,
                    markers: self.markers.clone(),
                    last_owner: self.last_owner,
                },
            );
        }
        let saved = self.history.remove(&(session.into(), instance)).unwrap_or_default();
        self.session = Some(session.into());
        self.instance = instance;
        self.last_owner = saved.last_owner;
        self.running = None;
        self.notice = None;
        self.log_cursor = saved.cursor;
        self.markers = saved.markers;
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        self.set(session, &saved.lines, cx);
        self.follow_bottom(cx);
    }
    pub(super) fn set(
        &mut self,
        session: &str,
        lines: &[crate::ui::app_control::LogEntry],
        cx: &mut Context<Self>,
    ) {
        self.session = Some(session.into());
        self.lines = Arc::new(lines.to_vec());
        self.timestamps = Arc::new(
            lines
                .iter()
                .map(|entry| entry.timestamp_micros.map(message_time).unwrap_or_default())
                .collect(),
        );
        self.rebuild_rows(cx);
        cx.notify();
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let count = self.lines.len() + self.markers.len();
        let mut rows = Vec::with_capacity(count);
        let mut markers = self.markers.iter().enumerate().peekable();
        let mut top = 0.;
        for index in 0..=self.lines.len() {
            while let Some(&(marker_index, marker)) = markers.peek() {
                if marker.before != index {
                    break;
                }
                rows.push(Row { top, content: RowContent::Marker(marker_index) });
                top += if marker.kind == MarkerKind::Started { 32. } else { 20. };
                markers.next();
            }
            if index < self.lines.len() {
                rows.push(Row { top, content: RowContent::Message(index) });
                top += 20.;
            }
        }
        self.rows = Arc::new(rows);
        self.height = top;
        let ids = Arc::make_mut(&mut self.text_ids);
        ids.truncate(count);
        while ids.len() < count {
            ids.push(cx.new(|_| ()));
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.lines = Arc::new(Vec::new());
        self.text_ids = Arc::new(Vec::new());
        self.timestamps = Arc::new(Vec::new());
        self.markers = Arc::new(Vec::new());
        self.rows = Arc::new(Vec::new());
        self.height = 0.;
        self.notice = None;
        self.scroll.set_offset(gpui::Point::default());
        // Keep the source cursor so future batches cannot resurrect cleared lines.
        cx.notify();
    }
    pub(super) fn receive(&mut self, session: &str, update: log::Update, cx: &mut Context<Self>) {
        let handle = self.scroll.clone();
        let follow = handle.offset().y + handle.max_offset().height <= px(0.5);
        match update {
            log::Update::Running(owner) => {
                if self.last_owner != Some(owner) && self.lines.is_empty() {
                    self.markers = Arc::new(vec![RunMarker {
                        before: 0,
                        kind: MarkerKind::Started,
                        timestamp: start_time(owner),
                    }]);
                    self.rebuild_rows(cx);
                }
                self.last_owner = Some(owner);
                self.running = Some(true);
                self.notice = None;
            }
            log::Update::Stopped => {
                self.running = Some(false);
                self.notice = None;
                // Repeated ownership notifications and empty runs add no rows.
                // Each retained message admits at most one start and one stop.
                if !self.lines.is_empty()
                    && self.markers.last().is_none_or(|marker| {
                        marker.kind != MarkerKind::Stopped || marker.before != self.lines.len()
                    })
                {
                    Arc::make_mut(&mut self.markers).push(RunMarker {
                        before: self.lines.len(),
                        kind: MarkerKind::Stopped,
                        timestamp: message_time(wall_time_micros()),
                    });
                    self.rebuild_rows(cx);
                }
            }
            log::Update::Notice(notice) => {
                self.notice = Some(notice);
                self.running = None;
            }
            log::Update::Batch(batch) => {
                self.running = Some(true);
                self.notice = None;
                let new_run = self.log_cursor.map_or(
                    self.last_owner != Some(batch.cursor.instance),
                    |cursor| {
                        cursor.instance != batch.cursor.instance
                            || cursor.generation != batch.cursor.generation
                    },
                ) || self.markers.last().is_some_and(|marker| {
                    marker.kind == MarkerKind::Stopped && marker.before == self.lines.len()
                });
                if new_run && (!batch.lines.is_empty() || batch.dropped > 0) {
                    let already_marked = self.last_owner == Some(batch.cursor.instance)
                        && self.markers.last().is_some_and(|marker| {
                            marker.kind == MarkerKind::Started && marker.before == self.lines.len()
                        });
                    if !already_marked {
                        Arc::make_mut(&mut self.markers).push(RunMarker {
                            before: self.lines.len(),
                            kind: MarkerKind::Started,
                            timestamp: start_time(batch.cursor.instance),
                        });
                    }
                    self.last_owner = Some(batch.cursor.instance);
                }
                let mut lines = self.lines.as_ref().clone();
                let removed =
                    log::retain(&mut lines, &mut self.log_cursor, log::Update::Batch(batch));
                if removed > 0 {
                    let markers = Arc::make_mut(&mut self.markers);
                    let mut removed_height = 20. * removed as f32;
                    markers.retain_mut(|marker| {
                        if marker.before < removed
                            || (marker.before == removed && marker.kind == MarkerKind::Stopped)
                        {
                            removed_height +=
                                if marker.kind == MarkerKind::Started { 32. } else { 20. };
                            return false;
                        }
                        marker.before -= removed;
                        true
                    });
                    if !follow {
                        // Keep the same visible text when the bounded prefix is evicted.
                        let offset = handle.offset();
                        handle.set_offset(gpui::point(
                            offset.x,
                            (offset.y + px(removed_height * crate::ui::desktop::zoom::factor(cx)))
                                .min(px(0.)),
                        ));
                    }
                }
                self.set(session, &lines, cx);
            }
        }
        if follow {
            self.follow_bottom(cx);
        }
        cx.notify();
    }

    fn follow_bottom(&self, cx: &App) {
        // Layout clamps this to the new content height. Using the old maximum would
        // leave the new rows below the viewport, especially for several batches/frame.
        let handle = &self.scroll;
        handle.set_offset(gpui::point(
            handle.offset().x,
            -px((self.height + 8.) * crate::ui::desktop::zoom::factor(cx)),
        ));
    }
}

fn start_time(owner: Id) -> String {
    formatted_time(
        crate::engine::session_instance::reserved_unix_secs(owner).into(),
        c"%b %e at %H:%M",
    )
}

fn wall_time_micros() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_micros();
    u64::try_from(now).expect("wall clock fits u64 microseconds")
}

fn message_time(time: u64) -> String {
    format!("{}.{:06}", calendar_time((time / 1_000_000) as libc::time_t), time % 1_000_000)
}

#[allow(unsafe_code, reason = "Formats source wall time with POSIX reentrant calendar APIs.")]
pub(super) fn formatted_time(now: libc::time_t, format: &std::ffi::CStr) -> String {
    let mut calendar = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut buffer = [0u8; 64];
    // SAFETY: All pointers refer to initialized or output storage valid for each call.
    let length = unsafe {
        if libc::localtime_r(&now, calendar.as_mut_ptr()).is_null() {
            return String::new();
        }
        libc::strftime(buffer.as_mut_ptr().cast(), buffer.len(), format.as_ptr(), calendar.as_ptr())
    };
    String::from_utf8_lossy(&buffer[..length]).replace("  ", " ")
}

fn calendar_time(now: libc::time_t) -> String {
    formatted_time(now, c"%b %e, %H:%M:%S")
}

impl Render for LogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale = crate::ui::desktop::zoom::factor(cx);
        // The window is an upper bound on this viewport, including its first layout
        // and resizes. Render only intersecting rows plus one row above the viewport.
        let viewport = f32::from(window.viewport_size().height) / scale;
        // A queued follow-bottom offset is clamped during layout. Cover that final
        // viewport even before the scroll handle has received its new bounds.
        let visible_top = (-f32::from(self.scroll.offset().y) / scale)
            .clamp(0., (self.height + 8. - viewport).max(0.));
        let visible_bottom = visible_top + viewport;
        let start = self.rows.partition_point(|row| row.top + 32. + 4. < visible_top);
        let end = self.rows.partition_point(|row| row.top + 4. <= visible_bottom);
        let content = div().relative().w_full().h(ui(self.height + 8.)).flex_none().children(
            (start..end).map(|index| {
                let row = &self.rows[index];
                let (id, time_id) =
                    text_editor::accessibility_text_ids(self.text_ids[index].entity_id());
                let container =
                    div().absolute().top(ui(row.top + 4.)).left_0().right_0().px(ui(8.));
                let (text, timestamp, error, stopped) = match row.content {
                    RowContent::Marker(marker_index) => {
                        let marker = &self.markers[marker_index];
                        if marker.kind == MarkerKind::Started {
                            let rule = || div().flex_1().h(ui(1.)).bg(rgba(0x0000001a));
                            return container
                                .h(ui(32.))
                                .flex()
                                .items_center()
                                .gap(ui(8.))
                                .font_family(UI_FONT_FAMILY)
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(ui(11.))
                                .line_height(ui(16.))
                                .letter_spacing(crate::ui::desktop::zoom::scaled(window, 0.11))
                                .text_color(rgba(0x0000004d))
                                .child(rule())
                                .child(
                                    tabular_numerals(host_text::element(
                                        format!("Started {}", marker.timestamp).into(),
                                        id,
                                        false,
                                        false,
                                    ))
                                    .flex_none()
                                    .whitespace_nowrap(),
                                )
                                .child(rule());
                        }
                        ("Stopped".to_owned(), marker.timestamp.clone(), false, true)
                    }
                    RowContent::Message(line_index) => {
                        let entry = &self.lines[line_index];
                        (
                            entry.message.clone(),
                            self.timestamps[line_index].clone(),
                            entry.severity == crate::ui::app_control::LogSeverity::Error,
                            false,
                        )
                    }
                };
                container
                    .h(ui(20.))
                    .overflow_hidden()
                    .flex()
                    .gap(ui(8.))
                    .font_family("JetBrains Mono NL")
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(ui(13.))
                    .line_height(ui(20.))
                    .letter_spacing(crate::ui::desktop::zoom::scaled(window, 0.13))
                    .text_color(rgba(if stopped {
                        0x0042c7b3
                    } else if error {
                        0xcc3333b3
                    } else {
                        0x000000b3
                    }))
                    .when(!timestamp.is_empty(), |row| {
                        row.child(
                            host_text::element(timestamp.into(), time_id, false, false)
                                .flex_none()
                                .whitespace_nowrap()
                                .text_color(rgba(if error { 0xcc333380 } else { 0x00000059 })),
                        )
                    })
                    .child(host_text::element(text.into(), id, false, false).whitespace_nowrap())
            }),
        );
        div()
            .size_full()
            .relative()
            .bg(rgb(0xf6f6f6))
            .child(
                div()
                    .id("session-log")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(content),
            )
            .child(Scrollbars::overlay(&self.scrollbars))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    fn batch(next: u64, line: &str) -> log::Update {
        log::Update::Batch(crate::ui::app_control::LogBatch {
            cursor: crate::ui::app_control::LogCursor {
                instance: crate::store::Id::from_bytes([1; 16]),
                generation: 1,
                next,
            },
            lines: vec![crate::ui::app_control::LogEntry::recorded(
                line.into(),
                crate::ui::app_control::LogSeverity::Info,
            )],
            dropped: 0,
            reset: false,
        })
    }
    #[gpui::test]
    fn clear_and_navigation_preserve_the_cursor_and_last_run(cx: &mut gpui::TestAppContext) {
        // Reattach after a selection change without resurrecting a cleared prefix.
        let view = cx.new(LogView::new);
        view.update(cx, |view, cx| {
            view.select("one", None, cx);
            view.receive("one", batch(1, "first"), cx);
            assert!(view.markers[0].kind == MarkerKind::Started);
            assert_eq!(view.markers[0].before, 0);
            assert_eq!(view.markers[0].timestamp, start_time(Id::from_bytes([1; 16])));
            view.receive("one", log::Update::Notice("Connection lost"), cx);
            view.receive("one", log::Update::Running(Id::from_bytes([1; 16])), cx);
            assert_eq!(view.markers.len(), 1, "reattaching keeps the same start divider");
            view.clear(cx);
            assert_eq!(view.log_cursor.map(|c| c.next), Some(1));
            view.receive("one", batch(2, "second"), cx);
            view.receive("one", log::Update::Stopped, cx);
            let stopped = view.markers[0].timestamp.clone();
            let sibling = Some(Id::from_bytes([2; 16]));
            view.select("one", sibling, cx);
            assert!(view.lines.is_empty());
            view.receive("one", log::Update::Stopped, cx);
            assert!(view.markers.is_empty(), "a sibling instance has its own history");
            view.select("one", None, cx);
            assert_eq!(
                view.lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
                ["second"]
            );
            assert_eq!(view.markers[0].timestamp, stopped);
            assert_eq!(view.log_cursor.map(|c| c.next), Some(2));
            view.receive("one", log::Update::Notice("Connection lost"), cx);
            assert_eq!(
                view.lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
                ["second"],
                "observer failure preserves last logs"
            );
            let mut restarted = batch(1, "after restart");
            let log::Update::Batch(ref mut data) = restarted else { unreachable!() };
            data.cursor.instance = crate::store::Id::from_bytes([2; 16]);
            view.receive("one", log::Update::Running(Id::from_bytes([1; 16])), cx);
            view.receive("one", restarted, cx);
            assert_eq!(
                view.lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
                ["second", "after restart"],
                "restarting appends to the session history without restoring cleared lines"
            );
            assert_eq!(view.markers.len(), 2);
            assert_eq!(view.markers[0].before, 1);
            assert_eq!(view.markers[0].timestamp, stopped);
            assert!(view.markers[1].kind == MarkerKind::Started);
            assert_eq!(view.markers[1].before, 1);
        });
    }
    #[gpui::test]
    fn run_markers_are_bounded_and_clear_with_the_log(cx: &mut gpui::TestAppContext) {
        let view = cx.new(LogView::new);
        view.update(cx, |view, cx| {
            view.select("one", None, cx);
            for next in 1..=300 {
                view.receive("one", batch(next, "message"), cx);
                view.receive("one", log::Update::Stopped, cx);
                view.receive("one", log::Update::Stopped, cx);
                view.receive("one", log::Update::Running(Id::from_bytes([1; 16])), cx);
                view.receive("one", log::Update::Stopped, cx);
            }
            assert_eq!(view.lines.len(), 256);
            assert_eq!(view.markers.len(), 512);
            assert_eq!(view.rows.len(), 768);
            assert_eq!(
                view.markers
                    .iter()
                    .filter(|marker| marker.kind == MarkerKind::Stopped)
                    .map(|marker| marker.before)
                    .collect::<Vec<_>>(),
                (1..=256).collect::<Vec<_>>()
            );
            assert_eq!(
                view.markers
                    .iter()
                    .filter(|marker| marker.kind == MarkerKind::Started)
                    .map(|marker| marker.before)
                    .collect::<Vec<_>>(),
                (0..256).collect::<Vec<_>>()
            );
            view.clear(cx);
            view.receive("one", log::Update::Stopped, cx);
            assert!(view.lines.is_empty());
            assert!(view.markers.is_empty());
            assert!(view.rows.is_empty());
            assert_eq!(view.log_cursor.unwrap().next, 300);
        });
    }

    #[gpui::test]
    fn new_output_follows_only_at_bottom_and_preserves_the_reading_position(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(crate::ui::desktop::UI_FONT_DATA),
                std::borrow::Cow::Borrowed(include_bytes!(
                    "../../assets/JetBrainsMonoNL-Medium.ttf"
                )),
            ])
            .unwrap();
        let window = cx.add_window(|_, cx| LogView::new(cx));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(600.), px(200.)));
        let handle = window
            .update(&mut visual, |view, _, cx| {
                view.select("one", None, cx);
                // Multiple updates before layout must follow the latest batch.
                for next in 1..=40 {
                    view.receive("one", batch(next, &format!("message {next}")), cx);
                }
                view.scroll.clone()
            })
            .unwrap();
        visual.run_until_parked();
        assert!(handle.max_offset().height > px(0.));
        assert_eq!(handle.offset().y, -handle.max_offset().height);
        window
            .update(&mut visual, |view, _, cx| {
                view.receive("one", log::Update::Stopped, cx);
                view.receive("one", log::Update::Running(Id::from_bytes([1; 16])), cx);
                view.receive("one", batch(41, "after restart"), cx);
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(handle.offset().y, -handle.max_offset().height);
        window
            .update(&mut visual, |view, window, cx| {
                let labels = window
                    .accessibility_nodes_for_test()
                    .filter_map(|(_, node)| node.value())
                    .collect::<Vec<_>>();
                assert!(labels.contains(&"Stopped"));
                assert!(labels.iter().any(|label| label.starts_with("Started ")));
                assert!(labels.contains(&"after restart"));
                handle.set_offset(gpui::point(px(0.), px(-200.)));
                view.receive("one", batch(42, "while reading"), cx);
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(handle.offset().y, px(-200.));
        window
            .update(&mut visual, |view, _, cx| {
                handle.set_offset(gpui::point(px(0.), -handle.max_offset().height));
                view.receive("one", batch(43, "following again"), cx);
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(handle.offset().y, -handle.max_offset().height);
        window
            .update(&mut visual, |view, _, cx| {
                for next in 44..=256 {
                    view.receive("one", batch(next, "fill retention"), cx);
                }
            })
            .unwrap();
        visual.run_until_parked();
        window
            .update(&mut visual, |view, _, cx| {
                handle.set_offset(gpui::point(px(0.), px(-1000.)));
                // Evict forty messages, their start divider and stop row above the visible text.
                for next in 257..=296 {
                    view.receive("one", batch(next, "trim prefix"), cx);
                }
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(handle.offset().y, px(-148.));
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(px(100.), px(50.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(10_000.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        visual.run_until_parked();
        assert_eq!(handle.offset().y, px(0.));
        window
            .update(&mut visual, |view, window, _| {
                let labels = window
                    .accessibility_nodes_for_test()
                    .filter_map(|(_, node)| node.value())
                    .collect::<Vec<_>>();
                assert!(labels.contains(&"after restart"), "scrolling renders the retained prefix");
                assert!(labels.iter().any(|label| label.starts_with("Started ")));
                assert!(
                    labels.len() < view.rows.len(),
                    "rows outside the viewport stay virtualized"
                );
            })
            .unwrap();
    }
}
