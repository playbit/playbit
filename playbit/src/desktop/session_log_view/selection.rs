//! One read-only selection across timestamps, wrapped messages and run markers.
use super::*;
use crate::ui::text::{Buffer, Movement, Selection};
use gpui::{FocusHandle, HighlightStyle, MouseButton, Point, Stateful, StyledText, TextLayout};

pub(super) struct TextCell {
    start: usize,
    layout: TextLayout,
}

struct Drag {
    position: Point<Pixels>,
    origin: Selection,
    clicks: usize,
}

pub(super) struct LogSelection {
    pub(super) focus: FocusHandle,
    // At most 64 KiB of messages plus the bounded timestamps and run markers.
    // There is no edit history; rebuild only when the retained rows change.
    buffer: Buffer,
    pub(super) row_starts: Vec<usize>,
    pub(super) cells: Vec<TextCell>,
    drag: Option<Drag>,
    removed_bytes: usize,
    pub(super) focus_observers: Option<[gpui::Subscription; 2]>,
}

impl LogSelection {
    pub(super) fn new(cx: &mut Context<LogView>) -> Self {
        Self {
            focus: cx.focus_handle().tab_stop(false),
            buffer: Buffer::new(),
            row_starts: Vec::new(),
            cells: Vec::new(),
            drag: None,
            removed_bytes: 0,
            focus_observers: None,
        }
    }

    pub(super) fn reset(&mut self) {
        self.buffer = Buffer::new();
        self.row_starts.clear();
        self.cells.clear();
        self.drag = None;
        self.removed_bytes = 0;
    }

    pub(super) fn blur(&mut self, window_active: bool) {
        self.drag = None;
        // Keep selected text when switching windows, but never keep an unfinished drag.
        if window_active {
            self.buffer.set_selection(Selection::caret(self.buffer.primary_selection().head));
        }
    }

    pub(super) fn active(&self) -> bool {
        self.drag.is_some() || !self.buffer.primary_selection().range().is_empty()
    }

    pub(super) fn remove_prefix(&mut self, rows: usize) {
        let bytes = self.row_starts.get(rows).copied().unwrap_or(self.buffer.as_str().len());
        // Apply after rebuilding: these offsets refer to the surviving suffix.
        self.removed_bytes = bytes;
    }

    fn range_at(&self, offset: usize, clicks: usize) -> std::ops::Range<usize> {
        if clicks == 2 {
            return self.buffer.word_range_at(offset);
        }
        let row = self.row_starts.partition_point(|start| *start <= offset).saturating_sub(1);
        self.row_starts[row]
            ..self.row_starts.get(row + 1).map_or(self.buffer.as_str().len(), |end| end - 1)
    }

    fn hit(&self, position: Point<Pixels>) -> Option<usize> {
        // Only painted cells are retained. Prefer the message on wrapped rows,
        // even when the pointer is to its left, below the one-line timestamp.
        let cell = self.cells.iter().min_by(|a, b| {
            let distance = |cell: &TextCell| {
                let bounds = cell.layout.bounds();
                let y = (bounds.top() - position.y).max(position.y - bounds.bottom()).max(px(0.));
                let x = (bounds.left() - position.x).max(position.x - bounds.right()).max(px(0.));
                (y, x)
            };
            distance(a).partial_cmp(&distance(b)).unwrap_or(std::cmp::Ordering::Equal)
        })?;
        let offset = cell.layout.index_for_position(position).unwrap_or_else(|offset| offset);
        Some(cell.start + offset)
    }
}

impl LogView {
    pub(super) fn rebuild_selection_text(&mut self) {
        let selected = self.selection.buffer.primary_selection();
        let removed = std::mem::take(&mut self.selection.removed_bytes);
        let bytes: usize = self
            .rows
            .iter()
            .map(|row| match *row {
                RowContent::Message(i) => {
                    self.lines[i].message.len()
                        + self.timestamps[i].len()
                        + usize::from(!self.timestamps[i].is_empty())
                }
                RowContent::Marker(i) => self.markers[i].timestamp.len() + 8,
            })
            .sum();
        let mut text = String::with_capacity(bytes + self.rows.len().saturating_sub(1));
        self.selection.row_starts.clear();
        self.selection.cells.clear();
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            self.selection.row_starts.push(text.len());
            match *row {
                RowContent::Message(index) => {
                    let timestamp = &self.timestamps[index];
                    if !timestamp.is_empty() {
                        text.push_str(timestamp);
                        text.push(' ');
                    }
                    text.push_str(&self.lines[index].message);
                }
                RowContent::Marker(index) => {
                    let marker = &self.markers[index];
                    if marker.kind == MarkerKind::Started {
                        text.push_str("Started ");
                        text.push_str(&marker.timestamp);
                    } else {
                        text.push_str(&marker.timestamp);
                        text.push_str(" Stopped");
                    }
                }
            }
        }
        if let Some(drag) = &mut self.selection.drag {
            drag.origin = Selection::new(
                drag.origin.anchor.saturating_sub(removed),
                drag.origin.head.saturating_sub(removed),
            );
        }
        self.selection.buffer = Buffer::from(text);
        self.selection.buffer.set_selection(Selection::new(
            selected.anchor.saturating_sub(removed),
            selected.head.saturating_sub(removed),
        ));
    }

    pub(super) fn selectable_text(
        &self,
        text: SharedString,
        id: gpui::accesskit::NodeId,
        start: usize,
        layouts: &mut Vec<TextCell>,
    ) -> gpui::Div {
        let selected = self.selection.buffer.primary_selection().range();
        let range = selected.start.saturating_sub(start).min(text.len())
            ..selected.end.saturating_sub(start).min(text.len());
        let styled =
            StyledText::new(text.clone()).with_highlights((!range.is_empty()).then_some((
                range,
                HighlightStyle {
                    background_color: Some(rgba(0xb3d7ffff).into()),
                    ..Default::default()
                },
            )));
        layouts.push(TextCell { start, layout: styled.layout().clone() });
        div().relative().child(styled).child(
            gpui::canvas(
                move |bounds, window, _| {
                    let node = crate::ui::desktop::text_accessibility_node(&text, false, bounds);
                    window.insert_accessibility_node(id, node, false, None);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        )
    }

    fn copy_selection(&self, cx: &mut App) {
        let range = self.selection.buffer.primary_selection().range();
        if !range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.selection.buffer.as_str()[range].to_owned(),
            ));
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection.buffer.select_all();
        self.follow_pending = false;
        cx.notify();
    }

    fn mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control {
            self.selection_menu(event, window, cx);
            return;
        }
        let Some(offset) = self.selection.hit(event.position) else { return };
        self.selection.focus.focus(window);
        let selected = if event.modifiers.shift {
            Selection::new(self.selection.buffer.primary_selection().anchor, offset)
        } else if event.click_count >= 2 {
            let range = self.selection.range_at(offset, event.click_count);
            Selection::new(range.start, range.end)
        } else {
            Selection::caret(offset)
        };
        self.selection.buffer.set_selection(selected);
        self.selection.drag = Some(Drag {
            position: event.position,
            origin: selected,
            clicks: if event.modifiers.shift { 1 } else { event.click_count },
        });
        self.follow_pending = false;
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn selection_handlers(
        &self,
        pane: Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        pane.on_action(cx.listener(|view, _: &text_editor::Copy, _, cx| view.copy_selection(cx)))
            .on_action(cx.listener(|view, _: &text_editor::SelectAll, _, cx| view.select_all(cx)))
            .on_action(cx.listener(|view, _: &text_editor::SelectLeft, _, cx| {
                view.selection.buffer.move_selections(Movement::PreviousGrapheme, true);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &text_editor::SelectRight, _, cx| {
                view.selection.buffer.move_selections(Movement::NextGrapheme, true);
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::selection_menu))
    }

    pub(super) fn selection_drag_handlers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let moved = cx.listener(|view, event: &gpui::MouseMoveEvent, _, cx| {
            if let Some(drag) = &mut view.selection.drag {
                if event.pressed_button == Some(MouseButton::Left) {
                    drag.position = event.position;
                    view.extend_drag(cx);
                } else {
                    view.selection.drag = None;
                }
                cx.notify();
            }
        });
        window.on_mouse_event(move |event, phase, window, cx| {
            if phase == gpui::DispatchPhase::Bubble {
                moved(event, window, cx);
            }
        });
        let released = cx.listener(|view, event: &gpui::MouseUpEvent, _, cx| {
            if event.button == MouseButton::Left
                && let Some(drag) = &mut view.selection.drag
            {
                // The platform may coalesce moves before delivering the release.
                drag.position = event.position;
                view.extend_drag(cx);
                view.selection.drag = None;
                cx.notify();
            }
        });
        window.on_mouse_event(move |event, phase, window, cx| {
            if phase == gpui::DispatchPhase::Bubble {
                released(event, window, cx);
            }
        });
    }

    fn extend_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = &self.selection.drag
            && let Some(offset) = self.selection.hit(drag.position)
        {
            let previous = self.selection.buffer.primary_selection();
            let selected = if drag.clicks < 2 {
                Selection::new(drag.origin.anchor, offset)
            } else {
                let range = self.selection.range_at(offset, drag.clicks);
                if offset < drag.origin.start() {
                    Selection::new(drag.origin.end(), range.start)
                } else if offset >= drag.origin.end() {
                    Selection::new(drag.origin.start(), range.end)
                } else {
                    drag.origin
                }
            };
            self.selection.buffer.set_selection(selected);
            if previous != self.selection.buffer.primary_selection() {
                cx.notify();
            }
        }
    }

    pub(super) fn drag_scroll(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        let Some(drag) = &self.selection.drag else { return };
        let position = drag.position;
        let distance = if position.y < bounds.top() {
            bounds.top() - position.y
        } else if position.y > bounds.bottom() {
            bounds.bottom() - position.y
        } else {
            return;
        };
        let previous = self.scroll.offset();
        let y = (previous.y + distance.clamp(-scaled(window, 20.), scaled(window, 20.)))
            .clamp(-self.scroll.max_offset().height, px(0.));
        if y != previous.y {
            self.scroll.set_offset(gpui::point(previous.x, y));
            window.request_animation_frame();
        }
    }

    pub(super) fn drag_after_layout(&mut self, cx: &mut Context<Self>) {
        // The new visible cells now have painted geometry after scrolling.
        self.extend_drag(cx);
    }

    fn selection_menu(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::runtime::MenuEntry;
        use crate::ui::desktop::popup;
        self.selection.focus.focus(window);
        let items = [(0, "Copy"), (1, "Select All")]
            .into_iter()
            .map(|(id, label)| MenuEntry::Action {
                icon: None,
                id,
                label: label.into(),
                checked: None,
                disabled: if id == 0 {
                    self.selection.buffer.primary_selection().range().is_empty()
                } else {
                    self.rows.is_empty()
                },
                destructive: false,
            })
            .collect();
        let view = cx.weak_entity();
        let activate = Rc::new(move |action, cx: &mut App| {
            let _ = view.update(cx, |view, cx| {
                if action == 0 {
                    view.copy_selection(cx);
                } else {
                    view.select_all(cx);
                }
            });
        });
        let request = popup::Request::context(window, event.position, items, activate, cx);
        cx.defer(move |cx| popup::open(request, cx));
        cx.stop_propagation();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests own the rendered log and its synthetic messages.")]
mod tests {
    use super::*;
    use crate::ui::app_control::{LogBatch, LogCursor, LogEntry, LogSeverity};

    fn append(view: &mut LogView, next: u64, lines: Vec<LogEntry>, cx: &mut Context<LogView>) {
        view.receive(
            "one",
            log::Update::Batch(LogBatch {
                cursor: LogCursor { instance: Id::from_bytes([1; 16]), generation: 1, next },
                lines,
                dropped: 0,
                reset: false,
            }),
            cx,
        );
    }

    fn position(view: &LogView, offset: usize) -> Point<Pixels> {
        let cell = view.selection.cells.iter().rev().find(|cell| cell.start <= offset).unwrap();
        let mut point = cell.layout.position_for_index(offset - cell.start).unwrap();
        point.y += cell.layout.line_height() / 2.;
        point.x += px(0.1);
        point
    }

    #[gpui::test]
    fn pointer_copy_spans_timestamps_wrapped_messages_and_unicode(cx: &mut gpui::TestAppContext) {
        // Exercise actual hit testing and keyboard dispatch at wide and wrapped widths.
        cx.update(text_editor::install_keybindings);
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../../assets/JetBrainsMonoNL-Medium.ttf"
            ))])
            .unwrap();
        let window = cx.add_window(|_, cx| LogView::new(cx));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let message = "World state snapshot recovered (1 read retries)";
        window
            .update(&mut visual, |view, _, cx| {
                view.set(
                    "one",
                    &[
                        LogEntry {
                            message: message.into(),
                            severity: LogSeverity::Info,
                            timestamp_micros: Some(197501),
                        },
                        LogEntry {
                            message: "café 日本語 🌱\nsecond line".into(),
                            severity: LogSeverity::Error,
                            timestamp_micros: Some(200000),
                        },
                    ],
                    cx,
                );
            })
            .unwrap();
        for width in [1100., 420.] {
            visual.simulate_resize(size(px(width), px(600.)));
            visual.run_until_parked();
            let (start, end, expected) = window
                .update(&mut visual, |view, _, _| {
                    let text = view.selection.buffer.as_str();
                    let start = text.find(',').unwrap() + 2;
                    let end = text.find("second line").unwrap() + "second line".len();
                    (position(view, start), position(view, end), text[start..end].to_owned())
                })
                .unwrap();
            // Reverse dragging must copy in reading order, with no soft-wrap newlines.
            visual.simulate_mouse_down(end, MouseButton::Left, Default::default());
            visual.simulate_mouse_move(
                start + gpui::point(px(8.), px(0.)),
                MouseButton::Left,
                Default::default(),
            );
            visual.simulate_mouse_up(start, MouseButton::Left, Default::default());
            visual.simulate_keystrokes("cmd-c");
            visual.run_until_parked();
            visual.update(|_, cx| {
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), expected)
            });
            visual.simulate_keystrokes("cmd-a cmd-c");
            visual.run_until_parked();
            window
                .update(&mut visual, |view, _, cx| {
                    assert_eq!(
                        cx.read_from_clipboard().unwrap().text().unwrap(),
                        view.selection.buffer.as_str()
                    );
                    assert_eq!(view.lines[0].message, message);
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn live_updates_preserve_selection_and_rotation_clips_only_expired_text(
        cx: &mut gpui::TestAppContext,
    ) {
        // Selection remains tied to retained source text through append and prefix eviction.
        let view = cx.new(LogView::new);
        view.update(cx, |view, cx| {
            view.select("one", None, cx);
            let lines = (0..256).map(|i| LogEntry::notice(format!("message {i} café"))).collect();
            append(view, 256, lines, cx);
            let text = view.selection.buffer.as_str();
            let start = text.find("message 1 café").unwrap();
            let end = start + "message 1 café".len();
            view.selection.buffer.set_selection(Selection::new(end, start));
            append(view, 257, vec![LogEntry::notice("new".into())], cx);
            view.copy_selection(cx);
            assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), "message 1 café");
            append(view, 258, vec![LogEntry::notice("newer".into())], cx);
            assert!(view.selection.buffer.primary_selection().is_empty());
            view.select_all(cx);
            let original = view.selection.buffer.as_str().to_owned();
            view.receive("one", log::Update::Stopped, cx);
            view.copy_selection(cx);
            assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), original);
            view.select("two", None, cx);
            assert!(!view.selection.active());
            view.select("one", None, cx);
            assert!(!view.selection.active());
            view.select_all(cx);
            view.clear(cx);
            assert!(!view.selection.active());
            assert!(view.selection.buffer.as_str().is_empty());
        });
    }

    #[gpui::test]
    fn word_and_row_selection_and_drag_scrolling_use_the_live_viewport(
        cx: &mut gpui::TestAppContext,
    ) {
        // Multi-click selection survives repaint; dragging outside the pane scrolls until release.
        cx.update(text_editor::install_keybindings);
        let window = cx.add_window(|_, cx| LogView::new(cx));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(600.), px(180.)));
        let handle = window
            .update(&mut visual, |view, _, cx| {
                view.set(
                    "one",
                    &(0..40).map(|i| LogEntry::notice(format!("message {i}"))).collect::<Vec<_>>(),
                    cx,
                );
                view.scroll.clone()
            })
            .unwrap();
        visual.run_until_parked();
        window
            .update(&mut visual, |_, _, cx| {
                handle.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            })
            .unwrap();
        visual.run_until_parked();
        for (click_count, expected) in [(2, "message"), (3, "message 0")] {
            let point = window.update(&mut visual, |view, _, _| position(view, 2)).unwrap();
            visual.simulate_event(gpui::MouseDownEvent {
                position: point,
                button: MouseButton::Left,
                click_count,
                ..Default::default()
            });
            visual.run_until_parked();
            visual.simulate_mouse_up(point, MouseButton::Left, Default::default());
            visual.simulate_keystrokes("cmd-c");
            visual.update(|_, cx| {
                assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), expected)
            });
        }
        let start = window.update(&mut visual, |view, _, _| position(view, 0)).unwrap();
        visual.simulate_mouse_down(start, MouseButton::Left, Default::default());
        let outside = gpui::point(px(200.), px(220.));
        visual.simulate_mouse_move(outside, MouseButton::Left, Default::default());
        visual.run_until_parked();
        // Test windows do not schedule display-link frames themselves.
        for _ in 0..40 {
            visual.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
        }
        assert_eq!(handle.offset().y, -handle.max_offset().height);
        visual.simulate_mouse_up(outside, MouseButton::Left, Default::default());
        visual.simulate_keystrokes("cmd-c");
        window
            .update(&mut visual, |view, _, cx| {
                assert_eq!(
                    cx.read_from_clipboard().unwrap().text().unwrap(),
                    view.selection.buffer.as_str()
                );
                assert!(view.selection.drag.is_none());
                let offset = handle.offset();
                append(view, 41, vec![LogEntry::notice("new output".into())], cx);
                assert_eq!(handle.offset(), offset);
            })
            .unwrap();
        visual.run_until_parked();
        assert!(
            handle.offset().y > -handle.max_offset().height,
            "selected text suspends following"
        );
    }

    #[gpui::test]
    fn deactivation_cancels_drag_scrolling_and_preserves_text_for_copy(
        cx: &mut gpui::TestAppContext,
    ) {
        // Lose activation before mouse-up, then redraw and reactivate without a release event.
        cx.update(text_editor::install_keybindings);
        let window = cx.add_window(|window, cx| {
            window.activate_window();
            LogView::new(cx)
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(600.), px(180.)));
        let handle = window
            .update(&mut visual, |view, _, cx| {
                view.set(
                    "one",
                    &(0..40).map(|i| LogEntry::notice(format!("message {i}"))).collect::<Vec<_>>(),
                    cx,
                );
                view.scroll.clone()
            })
            .unwrap();
        visual.run_until_parked();
        window
            .update(&mut visual, |_, _, cx| {
                handle.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            })
            .unwrap();
        visual.run_until_parked();
        let start = window.update(&mut visual, |view, _, _| position(view, 0)).unwrap();
        visual.simulate_mouse_down(start, MouseButton::Left, Default::default());
        visual.simulate_mouse_move(
            gpui::point(px(200.), px(220.)),
            MouseButton::Left,
            Default::default(),
        );
        visual.run_until_parked();
        let selected = window
            .update(&mut visual, |view, _, _| {
                assert!(view.selection.drag.is_some());
                let selected = view.selection.buffer.primary_selection();
                assert!(!selected.is_empty());
                selected
            })
            .unwrap();
        let offset = handle.offset();
        assert!(offset.y < px(0.) && offset.y > -handle.max_offset().height);
        visual.deactivate_window();
        for _ in 0..4 {
            visual.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
        }
        window
            .update(&mut visual, |view, window, _| {
                assert!(!window.is_window_active());
                assert!(view.selection.drag.is_none());
                assert_eq!(view.selection.buffer.primary_selection(), selected);
                assert_eq!(handle.offset(), offset);
                window.activate_window();
            })
            .unwrap();
        visual.run_until_parked();
        visual.simulate_keystrokes("cmd-c");
        let other_focus = visual.update(|_, cx| cx.focus_handle());
        window
            .update(&mut visual, |view, window, cx| {
                assert!(view.selection.drag.is_none());
                assert_eq!(handle.offset(), offset);
                assert_eq!(
                    cx.read_from_clipboard().unwrap().text().unwrap(),
                    view.selection.buffer.as_str()[selected.range()]
                );
                // Moving focus within the active window still clears the selection.
                other_focus.focus(window);
            })
            .unwrap();
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        window
            .update(&mut visual, |view, _, _| {
                assert!(view.selection.buffer.primary_selection().is_empty());
            })
            .unwrap();
    }
}
