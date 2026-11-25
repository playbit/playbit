use super::*;
use crate::desktop::volume_invitations::{Decision, Review};
use crate::ui::desktop::zoom::length as ui;

const SUPPORT: &str = "support@playbit.app";
const MAILTO: &str = "mailto:support@playbit.app";

impl Browser {
    pub(super) fn volume_invitation_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(review) = self.volume_invitation else { return self.no_preview(cx) };
        let owner_id = self.volume.owner.to_string();
        let owner = self
            .owner_name
            .as_ref()
            .map_or_else(|| owner_id.clone(), |name| format!("{name} ({owner_id:.6})"));
        let busy = review == Review::Deciding;
        let text = self.text(
            "volume-grant-sender",
            format!("{owner} has given you access to this volume"),
            cx,
        );
        let mut buttons = div().flex().items_center().gap(ui(8.));
        for (accept, key, label, width, style) in [
            (false, "decline-volume-invitation", "Decline & Leave", 108., ButtonStyle::Default),
            (true, "accept-volume-invitation", "Accept", 62., ButtonStyle::Primary),
        ] {
            let button = self.control(
                key.into(),
                label,
                controls::Kind::Button(style),
                busy,
                move |this, cx| {
                    if this.selected.is_none() && this.volume_invitation == Some(Review::Pending) {
                        cx.emit(Decision { accept });
                    }
                },
                cx,
            );
            button.update(cx, |button, cx| button.set_button_size(width, 24., cx));
            buttons = buttons.child(button);
        }
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(ui(32.))
            .px(ui(12.))
            .pb(ui(8.))
            .text_size(ui(11.))
            .line_height(ui(16.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(text.text_center().text_color(gpui::black().alpha(0.9)))
            .child(buttons)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui(3.))
                    .text_color(gpui::black().alpha(0.4))
                    .child(self.text("volume-invitation-report", "Report abuse to", cx))
                    .child(self.support_link(cx)),
            )
    }

    fn support_link(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        use gpui::accesskit::{Action, Node, Rect, Role};
        let focus = self.invitation_link_focus.clone();
        let id = text_editor::accessibility_text_ids(cx.entity_id()).0;
        div()
            .id("volume-invitation-email")
            .relative()
            .track_focus(&focus)
            .underline()
            .cursor_pointer()
            .focus(|link| link.text_color(gpui::black()))
            .on_click(|_, _, cx| cx.open_url(MAILTO))
            .on_key_down(|event, _, cx| {
                if event.keystroke.key == "enter" && !event.keystroke.modifiers.modified() {
                    cx.open_url(MAILTO);
                    cx.stop_propagation();
                }
            })
            .child(SUPPORT)
            .child(
                gpui::canvas(
                    move |bounds, window, _| {
                        let mut link = Node::new(Role::Link);
                        link.set_label(SUPPORT);
                        link.set_url(MAILTO);
                        link.set_bounds(Rect::new(
                            bounds.left().to_f64(),
                            bounds.top().to_f64(),
                            bounds.right().to_f64(),
                            bounds.bottom().to_f64(),
                        ));
                        link.add_action(Action::Click);
                        link.add_action(Action::Focus);
                        let focused = focus.is_focused(window);
                        let focus = focus.clone();
                        window.insert_accessibility_node(
                            id,
                            link,
                            focused,
                            Some(Rc::new(move |request, window, cx| match request.action {
                                Action::Click => cx.open_url(MAILTO),
                                Action::Focus => focus.focus(window),
                                _ => {}
                            })),
                        );
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
    }
}
