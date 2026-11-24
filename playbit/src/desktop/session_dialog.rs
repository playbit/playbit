//! The session dialogs share window-relative placement and one modal input scope.
use super::*;
use crate::ui::desktop::zoom::{length as ui, scaled};
use gpui::Focusable;

pub(super) fn panel(window: &Window) -> gpui::Div {
    div()
        .w(ui(416.))
        .max_w_full()
        .max_h(window.viewport_size().height - scaled(window, 40.))
        .flex()
        .flex_col()
        .rounded(ui(16.))
        .bg(rgb(0xececec))
        .shadow(vec![
            gpui::BoxShadow {
                color: rgba(0x000000cc).into(),
                offset: Default::default(),
                blur_radius: scaled(window, 0.417),
                spread_radius: px(0.),
            },
            gpui::BoxShadow {
                color: rgba(0x0000004d).into(),
                offset: gpui::point(px(0.), scaled(window, 18.)),
                blur_radius: scaled(window, 22.5),
                spread_radius: px(0.),
            },
        ])
}

pub(super) fn title(child: impl IntoElement) -> gpui::Div {
    div()
        .h(ui(32.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(ui(14.))
        .line_height(ui(24.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(0x413e39))
        .child(child)
}

impl Shell {
    pub(super) fn inline_session_form(&self, cx: &App) -> Option<&Entity<new_session::Form>> {
        self.new_session.as_ref().filter(|form| !form.read(cx).is_launch_dialog())
    }

    pub(super) fn session_dialog_open(&self, cx: &App) -> bool {
        self.join_dialog.is_some()
            || self.new_session.as_ref().is_some_and(|form| form.read(cx).is_launch_dialog())
    }

    pub(super) fn dismiss_launch_dialog(&mut self, cx: &mut Context<Self>) {
        if self.new_session.as_ref().is_some_and(|form| form.read(cx).can_cancel())
            && self.cancel_reviewed_invitation(cx)
        {
            self.new_session = None;
            cx.notify();
        }
    }

    pub(super) fn session_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let form = self.new_session.clone().filter(|form| form.read(cx).is_launch_dialog());
        let dialog_id = self
            .join_dialog
            .as_ref()
            .map(Entity::entity_id)
            .or_else(|| form.as_ref().map(Entity::entity_id));
        if dialog_id != self.session_dialog_id {
            self.session_dialog_id = dialog_id;
            #[cfg(all(target_os = "macos", not(test)))]
            crate::ui::desktop::appearance::set_content_modal(window, dialog_id.is_some());
            if let Some(editor) = &self.join_dialog {
                window.focus(&editor.focus_handle(cx));
            } else if let Some(form) = &form {
                form.update(cx, |form, cx| form.focus_launch(window, cx));
            } else {
                window.focus(&self.focus);
            }
        }
        let panel = if let Some(entry) = self.join_overlay(window, cx) {
            entry
        } else {
            panel(window).child(form?).into_any_element()
        };
        Some(
            div()
                .id("session-modal")
                .absolute()
                .inset_0()
                .occlude()
                .bg(rgba(0x48484833))
                .font_family(UI_FONT_FAMILY)
                .flex()
                .items_center()
                .justify_center()
                .px(ui(8.))
                .pb(ui(24.))
                .capture_key_down(cx.listener(|host, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" if host.join_dialog.is_some() && host.waiting_for_invitation() => {
                            host.dismiss_join_dialog(cx)
                        }
                        "escape" if host.join_dialog.is_some() => host.cancel_join_dialog(cx),
                        "escape" => host.dismiss_launch_dialog(cx),
                        "tab" if event.keystroke.modifiers.shift => window.focus_prev(),
                        "tab" => window.focus_next(),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .child(panel)
                .into_any_element(),
        )
    }
}
