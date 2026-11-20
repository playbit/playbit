//! Colors shared by the desktop views.
use playbit_api::ui::{Color, Theme, ThemeColor as Role};

pub(super) fn section_title_color(window_active: bool) -> gpui::Hsla {
    gpui::black().alpha(if window_active { 0.7 } else { 0.5 })
}

pub(super) fn column_divider_color() -> gpui::Hsla {
    gpui::black().alpha(0.1)
}

pub(super) fn theme() -> Theme {
    let mut theme = Theme::light();
    theme.set_color(Role::PanelBackground, Color(0xf6f6f6ff));
    theme.set_color(Role::MenuSelection, Color(0x000000b3));
    theme
}
