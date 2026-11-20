//! Upper metadata remains separate from the lower data editor.
use super::*;
use crate::ui::desktop::zoom::length as ui;

impl Browser {
    pub(super) fn inspector_heading(
        &mut self,
        key: &str,
        title: &str,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .h(ui(44.))
            .flex_none()
            .border_t_1()
            .border_color(super::super::theme::column_divider_color())
            .flex()
            .items_center()
            .pl(ui(12.))
            .pr(ui(8. + self.scrolling.inspector_clearance(cx)))
            .gap(ui(6.))
            .child(
                self.text(key, title.to_owned(), cx)
                    .flex_1()
                    .min_w_0()
                    .text_size(ui(14.))
                    .line_height(ui(24.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(super::super::theme::section_title_color(window_active)),
            )
    }

    pub(super) fn entry_inspector(
        &mut self,
        details: &EntryDetails,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let entry = !self.overview && self.kv_editor_open();
        let title = if entry {
            "Entry"
        } else {
            match details.entry.info.kind {
                EntryKind::KvDb {}
                    if details.entry.info.protocol == Some(DatabaseProtocol::BlobLog) =>
                {
                    "Append log"
                }
                EntryKind::KvDb {} => "Key-value store",
                EntryKind::ObjectDb { .. } => "Document database",
                EntryKind::Blob { .. } => "Blob",
                EntryKind::Directory {} => "Folder",
            }
        };
        let discard_name = self.deleted_name_discard(cx);
        let header =
            self.column_header("inspector-title", title, window_active, cx).children(discard_name);
        let body = if entry {
            self.kv_metadata(cx)
        } else {
            self.store_inspector(details, window_active, cx)
        };
        self.inspector_column(header, body)
    }

    pub(super) fn draft_recovery_inspector(
        &mut self,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let header = self.column_header("inspector-title", "Unsaved changes", window_active, cx);
        let retry = self.button(
            "retry-local-changes",
            "Retry",
            false,
            |this, cx| {
                this.retry_retired_kv_draft(cx);
                this.retry_retired_record_draft(cx);
                if !this.retired_kv_draft_pending() && !this.retired_record_draft_pending() {
                    this.error.clear();
                    this.alerted_error.clear();
                }
            },
            cx,
        );
        let busy = self.retired_kv_draft_busy(cx) || self.retired_record_draft_busy(cx);
        let discard = self.button(
            "discard-local-changes",
            "Discard Changes",
            busy,
            |this, cx| {
                this.discard_retired_kv_draft(cx);
                this.discard_retired_record_draft(cx);
                this.error.clear();
                if this.details.is_none() {
                    this.clear_selected_view(cx);
                }
            },
            cx,
        );
        let body = div()
            .flex()
            .flex_col()
            .gap(ui(12.))
            .p(ui(12.))
            .child(div().flex().gap(ui(8.)).child(discard).child(retry));
        self.inspector_column(header, body)
    }

    pub(super) fn volume_inspector(
        &mut self,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let header = self.column_header("inspector-title", "Volume", window_active, cx);
        let id = self.identifier("volume-id", self.volume.id.to_string(), cx);
        let mut metadata = div()
            .flex()
            .flex_col()
            .flex_none()
            .pl(ui(12.))
            .pr(ui(12. + self.scrolling.inspector_clearance(cx)))
            .pb(ui(8.))
            .child(self.metadata_row("volume-id", "ID", id, cx));
        let size = self.text("volume-size", self.payload_size_label(), cx);
        metadata = metadata.child(self.metadata_row("volume-size", "Size", size, cx));
        if let Some(details) = &self.volume_details {
            let created = history::history_time(details.created_clock);
            if !created.is_empty() {
                let value = self.text("volume-created", created, cx).truncate();
                metadata =
                    metadata.child(self.metadata_row("volume-created", "Created", value, cx));
            }
        }
        let permissions = self.volume_permissions_panel(window_active, cx);
        let mut history = div().flex().flex_col().flex_none().child(self.inspector_heading(
            "volume-history-title",
            "History",
            window_active,
            cx,
        ));
        if let Some(details) = self.volume_details.clone() {
            // Volume records retain their current label and creation marker, not an
            // append-only rename log. Show only the activity the stored clocks establish.
            if details.name_clock > details.created_clock {
                history = history.child(self.volume_history_row(
                    "volume-renamed",
                    format!("Renamed to “{}”", details.volume.name),
                    details.name_clock,
                    cx,
                ));
            }
            history = history.child(self.volume_history_row(
                "volume-born",
                "Created".into(),
                details.created_clock,
                cx,
            ));
        }
        let body = scrolling::inspector_viewport(
            div()
                .id("volume-inspector")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&self.scrolling.permissions.read(cx).handle())
                .child(metadata)
                .child(permissions)
                .child(history),
            &self.scrolling.permissions,
            cx,
        );
        self.inspector_column(header, body)
    }

    fn volume_history_row(
        &mut self,
        key: &str,
        title: String,
        clock: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let time = history::history_time(clock);
        div()
            .h(ui(24.))
            .flex_none()
            .flex()
            .items_center()
            .gap(ui(6.))
            .pl(ui(12.))
            .pr(ui(12. + self.scrolling.inspector_clearance(cx)))
            .text_size(ui(11.))
            .line_height(ui(20.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(gpui::black().alpha(0.7))
            .child(self.text(key, title, cx).flex_1().min_w_0().truncate())
            .child(self.text(&format!("{key}-time"), time, cx).flex_none().whitespace_nowrap())
    }

    fn inspector_column(&self, header: gpui::Div, body: gpui::Div) -> gpui::Div {
        div()
            .debug_selector(|| "browser-metadata".into())
            .w(ui(INSPECTOR_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(super::super::theme::column_divider_color())
            .child(header)
            .child(body)
    }

    fn store_inspector(
        &mut self,
        details: &EntryDetails,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let id = self.identifier("inspector-id", details.entry.id.to_string(), cx);
        let mut metadata = div()
            .flex()
            .flex_col()
            .flex_none()
            .pl(ui(12.))
            .pr(ui(12. + self.scrolling.inspector_clearance(cx)))
            .pb(ui(8.))
            .child(self.metadata_row("inspector-id", "ID", id, cx));
        let size = self.text("inspector-size", self.payload_size_label(), cx).truncate();
        metadata = metadata.child(self.metadata_row("inspector-size", "Size", size, cx));
        let created = history::history_time(details.created_clock);
        if !created.is_empty() {
            let created = self.text("inspector-created", created, cx).truncate();
            metadata =
                metadata.child(self.metadata_row("inspector-created", "Created", created, cx));
        }
        let permissions = self.permissions_panel(details, window_active, cx);
        let history = self.history_panel(details, window_active, cx);
        scrolling::inspector_viewport(
            div()
                .id("entry-inspector")
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&self.scrolling.permissions.read(cx).handle())
                .child(metadata)
                .child(permissions)
                .child(history),
            &self.scrolling.permissions,
            cx,
        )
    }

    pub(super) fn metadata_row(
        &mut self,
        key: &str,
        label: &str,
        value: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .h(ui(24.))
            .flex_none()
            .flex()
            .items_start()
            .gap(ui(16.))
            .text_size(ui(11.))
            .line_height(ui(16.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(gpui::black().alpha(0.9))
            .child(
                self.text(&format!("{key}-label"), label.to_owned(), cx)
                    .w(ui(43.))
                    .flex_none()
                    .text_color(gpui::black().alpha(0.5)),
            )
            .child(value.min_w_0().flex_1())
    }

    pub(super) fn identifier(
        &mut self,
        key: &'static str,
        value: String,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        // Metadata locations retain one editor each, never one per entry.
        self.rendered_text.insert(key.into());
        let editor = self.identifier_fields.entry(key).or_insert_with(|| {
            cx.new(|cx| text_editor::TextEditor::new(&value, cx).selectable_id())
        });
        editor.update(cx, |editor, cx| editor.acknowledge(None, &value, cx));
        div().debug_selector(move || key.into()).w_full().min_w_0().child(editor.clone())
    }
}

#[cfg(test)]
#[gpui::test]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert rendered identifier selection and canonical values."
)]
fn metadata_identifiers_copy_canonical_values_and_keep_selection_across_renders(
    cx: &mut gpui::TestAppContext,
) {
    // Render all three metadata locations, Copy their real fields, then replace the selected ID.
    use super::tests::{details, volume};
    use gpui::{EntityInputHandler, Focusable};
    cx.update(text_editor::install_keybindings);
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(crate::ui::desktop::UI_FONT_DATA)])
        .unwrap();
    let (send, _requests, _) = worker_queue::channel();
    let window = cx.add_window(|window, cx| {
        window.activate_window();
        Browser::new(volume(1), send, cx)
    });
    cx.simulate_window_resize(window.into(), gpui::size(px(1400.), px(600.)));
    cx.run_until_parked();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    visual.update(|window, cx| drop(window.draw(cx)));
    let volume_id = volume(1).id.to_string();
    let volume_editor = window
        .update(&mut visual, |browser, window, cx| {
            let editor = browser.identifier_fields["volume-id"].clone();
            editor.update(cx, |editor, cx| editor.focus_from_tab(window, cx));
            editor
        })
        .unwrap();
    visual.simulate_keystrokes("cmd-c");
    visual.run_until_parked();
    visual.update(|_, cx| {
        assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), volume_id);
    });
    window
        .update(&mut visual, |browser, window, cx| {
            assert!(volume_editor.read(cx).focus_handle(cx).is_focused(window));
            browser.selected = Some(details(1, EntryAcl::Inherit).entry.id);
            browser.details = Some(Arc::new(details(1, EntryAcl::Inherit)));
            browser.show_entry_details = true;
            cx.notify();
        })
        .unwrap();
    visual.run_until_parked();
    visual.update(|window, cx| drop(window.draw(cx)));
    let expected = details(1, EntryAcl::Inherit).entry.id.to_string();
    for key in ["entry-id", "inspector-id"] {
        let editor = window
            .update(&mut visual, |browser, window, cx| {
                assert_eq!(browser.identifier_fields.len(), 2);
                assert!(!volume_editor.read(cx).focus_handle(cx).is_focused(window));
                let editor = browser.identifier_fields[key].clone();
                assert_eq!(editor.read(cx).text(), expected);
                let node = window
                    .accessibility_nodes_for_test()
                    .find(|(id, _)| {
                        *id == text_editor::accessibility_text_ids(editor.entity_id()).0
                    })
                    .unwrap()
                    .1;
                assert!(node.is_read_only());
                assert_eq!(node.value(), Some(expected.as_str()));
                editor.update(cx, |editor, cx| editor.focus_from_tab(window, cx));
                cx.notify();
                editor
            })
            .unwrap();
        visual.run_until_parked();
        visual.update(|window, cx| drop(window.draw(cx)));
        window
            .update(&mut visual, |browser, window, cx| {
                assert_eq!(browser.identifier_fields[key].entity_id(), editor.entity_id());
                let selection = editor.update(cx, |editor, cx| {
                    editor.selected_text_range(false, window, cx).unwrap()
                });
                assert_eq!(selection.range, 0..expected.len());
            })
            .unwrap();
        visual.simulate_keystrokes("cmd-c");
        visual.run_until_parked();
        visual.update(|_, cx| {
            assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), expected);
        });
    }
    let inspector = window
        .update(&mut visual, |browser, _, cx| {
            let id = browser.identifier_fields["inspector-id"].entity_id();
            browser.selected = Some(details(2, EntryAcl::Inherit).entry.id);
            browser.details = Some(Arc::new(details(2, EntryAcl::Inherit)));
            cx.notify();
            id
        })
        .unwrap();
    visual.run_until_parked();
    visual.update(|window, cx| drop(window.draw(cx)));
    window
        .update(&mut visual, |browser, window, cx| {
            assert_eq!(browser.identifier_fields["inspector-id"].entity_id(), inspector);
            for key in ["entry-id", "inspector-id"] {
                assert_eq!(
                    browser.identifier_fields[key].read(cx).text(),
                    details(2, EntryAcl::Inherit).entry.id.to_string(),
                );
            }
            assert!(window.accessibility_error().is_none());
        })
        .unwrap();
}
