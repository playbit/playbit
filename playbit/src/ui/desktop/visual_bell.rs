//! Bounded, single-line feedback shared by library and guest windows.
use super::zoom::length as ui;
use gpui::{div, prelude::*};

pub(crate) const DURATION: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const MAX_MESSAGE_CHARS: usize = 256;
const TRUNCATION_SUFFIX: &str = "...";

pub(crate) fn render(icon: &'static str, label: gpui::Div) -> gpui::Div {
    div().absolute().left_0().right_0().bottom(ui(16.)).px(ui(16.)).flex().justify_center().child(
        div()
            .id("visual-bell")
            .debug_selector(|| "visual-bell".into())
            .max_w_full()
            .min_w_0()
            .h(ui(28.))
            .px(ui(16.))
            .flex()
            .items_center()
            .gap(ui(10.))
            .rounded(ui(14.))
            .bg(gpui::rgba(0x000000a6))
            .text_color(gpui::white())
            .text_size(ui(14.))
            .child(gpui::svg().path(icon).size(ui(14.)).flex_none().text_color(gpui::white()))
            .child(
                label
                    .debug_selector(|| "visual-bell-label".into())
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate(TRUNCATION_SUFFIX.into())),
            ),
    )
}

pub(crate) fn message(message: &str) -> String {
    let mut chars = message.chars();
    let mut text: String = chars
        .by_ref()
        .take(MAX_MESSAGE_CHARS)
        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
        .collect();
    if chars.next().is_some() {
        for _ in 0..TRUNCATION_SUFFIX.len() {
            text.pop();
        }
        text.push_str(TRUNCATION_SUFFIX);
    }
    text
}
