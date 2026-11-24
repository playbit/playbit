//! New session launcher for recent apps, bundled examples and project creation.
mod project;

use super::*;
use crate::ui::desktop::zoom::length as ui;
use playbit_api::ui::{Theme, ThemeColor as Role};

fn app_row() -> gpui::Div {
    // The card's top padding shares the last eight points of the section heading.
    div().flex().flex_wrap().gap(ui(8.)).mt(ui(-8.)).px(ui(8.)).pb(ui(16.))
}

fn heading(text: gpui::Div) -> gpui::Div {
    div()
        .h(ui(44.))
        .border_t_1()
        .border_color(super::theme::column_divider_color())
        .flex()
        .items_center()
        .px(ui(16.))
        .text_size(ui(14.))
        .line_height(ui(24.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(0x373737))
        .child(text)
}

impl Shell {
    fn make_project(&mut self, cx: &mut Context<Self>) {
        if self.making_project {
            return;
        }
        self.making_project = true;
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Project Folder".into()),
        });
        cx.spawn(async move |host, cx| {
            let result = match paths.await {
                Ok(Ok(Some(mut paths))) if paths.len() == 1 => {
                    let directory = paths.remove(0);
                    cx.background_executor()
                        .spawn(async move { project::initialize(&directory) })
                        .await
                        .map(Some)
                }
                Ok(Ok(None)) => Ok(None),
                _ => Err("Could not select a project folder.".into()),
            };
            let _ = host.update(cx, |host, cx| {
                host.making_project = false;
                match result {
                    Ok(Some(readme)) => cx.reveal_path(&readme),
                    Ok(None) => {}
                    Err(error) => {
                        host.alerted_error.clear();
                        host.error = error;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn welcome_content(
        &mut self,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use crate::ui::desktop::scrollbar::Scrollbars;
        let mut seen = std::collections::BTreeSet::new();
        let recent: Vec<_> = self
            .recent_sessions
            .iter()
            .filter_map(|id| self.sessions.iter().find(|row| row.id == *id))
            .filter(|row| seen.insert(row.resolved_build_id.clone()))
            .take(session_icons::MAX_RECENT_TILES)
            .cloned()
            .collect();
        let side = (controls::APP_TILE_ICON_POINTS
            * crate::ui::desktop::zoom::factor(cx)
            * window.scale_factor())
        .round() as u32;
        if self.session_icons.request_tiles(&seen, side) {
            self.load_next_session_icon(cx);
        }
        let mut apps = app_row();
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
            let icon = self.session_icons.tile(&row.resolved_build_id);
            control.update(cx, |control, cx| {
                control.set_app_icon(icon, false, cx);
                control
                    .set_session_style(controls::SessionStyle::AppTile { bundled_icon: None }, cx);
            });
            apps = apps.child(control);
        }
        let mut featured = app_row();
        for example in new_session::EXAMPLES {
            let tile = self.host_callback(
                example.app_id,
                example.name,
                false,
                move |host, cx| {
                    host.show_new_session(
                        new_session::Target::Template { account: host.account.id, example },
                        cx,
                    );
                },
                cx,
            );
            tile.update(cx, |control, cx| {
                control.set_session_style(
                    controls::SessionStyle::AppTile { bundled_icon: Some(example.app_id) },
                    cx,
                );
            });
            featured = featured.child(tile);
        }
        let make = self.host_callback(
            "make-project",
            "Make an app",
            self.making_project,
            Self::make_project,
            cx,
        );
        make.update(cx, |control, cx| {
            control.set_session_style(controls::SessionStyle::Template, cx)
        });
        let handle = self.welcome_scrollbars.read(cx).handle();
        div()
            .id("new-session-content")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .bg(rgba(theme.color(Role::WindowBackground).0))
            .child(
                div()
                    .id("new-session-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .when(!recent.is_empty(), |v| {
                        v.child(heading(self.host_text(HostText::RecentApps, "Recent apps", cx)))
                            .child(apps)
                    })
                    .child(heading(self.host_text(HostText::FeaturedApps, "Featured apps", cx)))
                    .child(featured)
                    .child(heading(self.host_text(HostText::Templates, "Templates", cx)))
                    .child(div().px(ui(16.)).pb(ui(16.)).child(make)),
            )
            .child(Scrollbars::overlay(&self.welcome_scrollbars))
    }
}
