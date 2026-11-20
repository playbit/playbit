//! Recent apps and templates for opening a session.
use super::*;
use crate::ui::desktop::zoom::length as ui;

impl Shell {
    pub(super) fn welcome_content(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        use crate::ui::desktop::scrollbar::Scrollbars;
        let mut seen = std::collections::BTreeSet::new();
        let recent: Vec<_> = self
            .recent_sessions
            .iter()
            .filter_map(|id| self.sessions.iter().find(|row| row.id == *id))
            .filter(|row| seen.insert(row.resolved_build_id.clone()))
            .take(16)
            .cloned()
            .collect();
        let mut apps = div().flex().flex_wrap().gap(ui(8.)).px(ui(16.)).pt(ui(4.)).pb(ui(30.));
        for (index, row) in recent.iter().enumerate() {
            let id = row.id.clone();
            let weak = cx.weak_entity();
            let control = self.navigation_button(
                format!("recent-{index}"),
                row.app_name.as_deref().unwrap_or("App unavailable"),
                false,
                Rc::new(move |_, cx| {
                    let _ = weak.update(cx, |host, cx| host.open_retained_session(id.clone(), cx));
                }),
                None,
                cx,
            );
            let icon = self.session_icons.get(&row.resolved_build_id);
            control.update(cx, |control, cx| {
                control.set_app_icon(icon, false, cx);
                control.set_session_style(controls::SessionStyle::Recent, cx);
            });
            apps = apps.child(control);
        }
        let todo = self.host_callback(
            "example-open-1",
            "Try a TODO app",
            false,
            |host, cx| {
                host.show_new_session(new_session::Target::Template(host.account.id), cx);
            },
            cx,
        );
        let game = self.host_callback("example-game", "Make a game", true, |_, _| {}, cx);
        for control in [&todo, &game] {
            control.update(cx, |control, cx| {
                control.set_session_style(controls::SessionStyle::Template, cx)
            });
        }
        let heading = |text| {
            div()
                .h(ui(44.))
                .flex_none()
                .flex()
                .items_center()
                .px(ui(16.))
                .text_size(ui(14.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(0x373737))
                .child(text)
        };
        let recent_heading =
            heading(self.host_text(HostText::RecentApps, "Recently used apps", cx))
                .when(recent.is_empty(), |v| v.text_color(rgba(0x00000033)));
        let templates_heading = heading(self.host_text(HostText::Templates, "Templates", cx));
        let handle = self.welcome_scrollbars.read(cx).handle();
        div()
            .id("new-session-content")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .child(
                div()
                    .id("new-session-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .child(recent_heading)
                    .when(!recent.is_empty(), |v| v.child(apps))
                    .child(
                        div().border_t_1().border_color(rgba(0x0000001a)).child(templates_heading),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(ui(16.))
                            .px(ui(16.))
                            .pb(ui(16.))
                            .child(todo)
                            .child(game),
                    ),
            )
            .child(Scrollbars::overlay(&self.welcome_scrollbars))
    }
}
