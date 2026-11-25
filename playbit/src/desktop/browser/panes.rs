//! Navigation and metadata stay above the independently scrolling preview or editor.
use super::*;
use crate::ui::desktop::scrollbar::Scrollbars;
use crate::ui::desktop::zoom::length as ui;

// Fits a full 16-byte row, offset gutter, field padding and representation control.
const BINARY_INSPECTOR_WIDTH: f32 = 352.;

impl Browser {
    pub(super) fn panes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let active = window.is_window_active();
        if self
            .details
            .as_ref()
            .is_some_and(|details| !matches!(details.entry.info.kind, EntryKind::Directory {}))
        {
            self.scrolling.reveal_entry = None;
        }
        let directories = self.directory_columns(window, active, cx);
        let metadata = self.permissions_column(active, cx).horizontal_focus_reveal_scope();
        let columns = self.scrolling.columns.read(cx).handle();
        let height =
            f32::from(window.viewport_size().height) / crate::ui::desktop::zoom::factor(cx);
        // The reference keeps 288 points of root preview and a 286-point selected
        // navigator. At small window sizes both regions retain usable space.
        let upper_height = if self.selected.is_none() { height - 288. } else { 286. };
        let upper_height = upper_height.max(140.).min((height - 160.).max(0.));
        let upper = div()
            .debug_selector(|| "browser-upper".into())
            .relative()
            .w_full()
            .h(ui(upper_height))
            .flex_none()
            .child(
                div()
                    .id("browser-columns")
                    .size_full()
                    .flex()
                    .overflow_x_scroll()
                    .track_scroll(&columns)
                    .children(directories)
                    .child(div().flex_1().min_w_0().h_full())
                    .child(metadata)
                    .children(self.entry_reveal(cx)),
            )
            .child(Scrollbars::overlay(&self.scrolling.columns));
        let content = self.lower_content(window, cx);
        let lower = div()
            .debug_selector(|| "browser-lower".into())
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .border_t_1()
            .border_color(super::super::theme::column_divider_color())
            .bg(rgb(0xf6f6f6))
            .child(
                div()
                    .id("browser-content")
                    .size_full()
                    .flex()
                    .overflow_x_scroll()
                    .track_scroll(&self.scrolling.content_columns.read(cx).handle())
                    .child(content),
            )
            .child(Scrollbars::overlay(&self.scrolling.content_columns));
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .horizontal_focus_reveal_scope()
            .text_size(ui(14.))
            .child(upper)
            .child(lower)
    }

    fn lower_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let content = if !self.overview && (self.kv_address().is_some() || self.kv_editor_open()) {
            let editor_width =
                if self.kv_uses_binary_value() { BINARY_INSPECTOR_WIDTH } else { INSPECTOR_WIDTH };
            div()
                .size_full()
                .min_w(ui(MIN_WIDTH + editor_width - INSPECTOR_WIDTH))
                .flex()
                .child(div().flex_1().min_w(ui(240.)).h_full().child(self.kv_panel(window, cx)))
                .child(self.kv_editor_column(editor_width, cx))
        } else if self.selected.is_none()
            || (!self.show_entry_details
                && self.details.as_ref().is_some_and(|details| {
                    matches!(details.entry.info.kind, EntryKind::Directory {})
                }))
        {
            self.no_preview(cx)
        } else {
            let show_heading = self
                .details
                .as_ref()
                .is_some_and(|details| !matches!(details.entry.info.kind, EntryKind::Blob { .. }));
            let minimum = if self.records.type_id.is_some() { 480. } else { 300. };
            div()
                .size_full()
                .when(show_heading || self.show_entry_details, |panel| panel.min_w(ui(minimum)))
                .flex()
                .flex_col()
                .when(show_heading, |panel| {
                    panel.child(self.content_header(window.is_window_active(), cx))
                })
                .child(self.detail_panel(window, cx))
        };
        content.debug_selector(|| "browser-content".into())
    }

    fn kv_editor_column(&mut self, width: f32, cx: &mut Context<Self>) -> gpui::Div {
        let column = div()
            .w(ui(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .horizontal_focus_reveal_scope()
            .border_l_1()
            .border_color(super::super::theme::column_divider_color());
        if !self.kv_editor_open() {
            return column;
        }
        column
            .child(
                div()
                    .h(ui(40.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .items_center()
                    .px(ui(12.))
                    .child(self.kv_actions(cx)),
            )
            .child(div().flex_1().min_h_0().child(self.kv_editor(cx)))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.tab_kv_editor(event, window, cx);
            }))
    }

    pub(super) fn no_preview(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(self.text("no-preview", "No preview", cx).text_color(gpui::black().alpha(0.2)))
    }
}
