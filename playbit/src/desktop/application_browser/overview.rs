//! The app's inspector and publication history share the upper columns with session navigation.
use super::{inspector::initial, scrolling::vertical_viewport, *};
use crate::ui::desktop::zoom::{factor, length as ui};

const ARTWORK_POINTS: f32 = 64.;

impl Shell {
    pub(super) fn app_overview_inspector(
        &mut self,
        app: &Application,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let heading = self.application_heading(
            "application-overview-heading",
            "App",
            None,
            None,
            window.is_window_active(),
            cx,
        );
        let publisher = self.app_publisher_name(&app.identity, &app.build);
        let publisher_cell =
            div().flex().items_center().gap(ui(6.)).child(initial(&publisher, false)).child(
                self.application_text("application-publisher-name", publisher, cx).truncate(),
            );
        let ids = div()
            .flex()
            .flex_col()
            .child(self.selectable_metadata_value(
                "application-publisher-id",
                &app.identity.publisher.to_string(),
                cx,
            ))
            .child(self.selectable_metadata_value("application-app-id", &app.identity.app_id, cx));
        let size = self
            .applications
            .details
            .size
            .map(browser::format_byte_size)
            .unwrap_or_else(|| "—".into());
        let updated = self
            .applications
            .details
            .history
            .as_ref()
            .and_then(|history| history.updated)
            .map(release_date)
            .unwrap_or_else(|| "—".into());
        let mut body = div()
            .id("application-overview-fields")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(ui(12.))
            .pb(ui(12.))
            .flex()
            .flex_col()
            .gap(ui(8.))
            .text_size(ui(11.))
            .line_height(ui(16.));
        for (label, value) in [
            (
                "Name",
                self.application_text("application-overview-name", app.name.clone(), cx).truncate(),
            ),
            ("Publisher", publisher_cell),
            ("ID", ids),
            ("Size", self.application_text("application-size", size, cx)),
            ("Updated", self.application_text("application-updated", updated, cx)),
        ] {
            body = body.child(
                div()
                    .flex_none()
                    .flex()
                    .gap(ui(16.))
                    .child(
                        self.application_text(label, label, cx)
                            .w(ui(51.))
                            .flex_none()
                            .text_color(rgba(0x00000080)),
                    )
                    .child(value.flex_1().min_w_0()),
            );
        }
        div()
            .flex_1()
            .min_w(ui(260.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(0xececec))
            .child(heading)
            .child(vertical_viewport(body, &mut self.applications.inspector_scrolling, cx))
    }

    pub(super) fn app_release_notes(
        &mut self,
        app: &Application,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let side = (ARTWORK_POINTS * factor(cx) * window.scale_factor()).round() as u32;
        if self.session_icons.request_tiles(&BTreeSet::from([app.build.clone()]), side) {
            self.load_next_session_icon(cx);
        }
        let artwork = self.session_icons.tile(&app.build);
        let title = self
            .application_text("application-notes-heading", "Release notes", cx)
            .h(ui(44.))
            .flex_none()
            .flex()
            .items_center()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_size(ui(14.))
            .line_height(ui(24.))
            .text_color(theme::section_title_color(window.is_window_active()));
        let mut notes = div().flex_1().min_w_0().flex().flex_col().child(title);
        let details = &self.applications.details;
        let status = if let Some(error) = &details.history_error {
            if details.history.is_some() {
                Some(format!("Couldn’t load more release notes. {error}"))
            } else {
                Some(format!("Release notes unavailable. {error}"))
            }
        } else if let Some(error) = &details.watch_error {
            Some(format!("Live release note updates unavailable. {error}"))
        } else if details.needs_history_page() {
            Some("Loading release notes…".to_owned())
        } else if details.history.as_ref().is_some_and(|history| history.notes.is_empty()) {
            Some("No release notes".to_owned())
        } else {
            None
        };
        if let Some(history) = details.history.clone() {
            for note in history.notes {
                let mut row = div()
                    .flex_none()
                    .mb(ui(16.))
                    .child(self.application_text(
                        &format!("note-{}", note.build_id),
                        note.page_text(),
                        cx,
                    ))
                    .child(self.application_text(
                        &format!("note-date-{}", note.build_id),
                        release_date(note.published_at),
                        cx,
                    ));
                if note.page_count() > 1 {
                    row = row.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui(6.))
                            .mt(ui(4.))
                            .child(self.note_page_button(&note, false, cx))
                            .child(self.application_text(
                                &format!("note-page-{}", note.build_id),
                                format!("{} of {}", note.page + 1, note.page_count()),
                                cx,
                            ))
                            .child(self.note_page_button(&note, true, cx)),
                    );
                }
                notes = notes.child(row);
            }
        }
        if let Some(status) = status {
            notes = notes.child(
                self.application_text("application-history-status", status, cx)
                    .text_color(rgba(0x00000080)),
            );
        }
        if self.applications.details.history_error.is_some()
            || self.applications.details.watch_error.is_some()
        {
            let retry = self.host_callback(
                "application-retry-history",
                "Retry",
                false,
                |host, cx| host.retry_application_details(cx),
                cx,
            );
            notes = notes.child(div().flex().mt(ui(8.)).child(retry));
        }
        let icon: gpui::AnyElement = match artwork {
            Some(image) => gpui::img(image).size(ui(ARTWORK_POINTS)).into_any_element(),
            None => gpui::svg()
                .path("app-default")
                .size(ui(ARTWORK_POINTS))
                .text_color(rgba(0x00000080))
                .into_any_element(),
        };
        let body =
            div().id("application-release-notes").flex_1().min_h_0().overflow_y_scroll().child(
                div()
                    .flex()
                    .gap(ui(12.))
                    .pl(ui(12.))
                    .pr(ui(16.))
                    .pb(ui(12.))
                    .text_size(ui(11.))
                    .line_height(ui(16.))
                    .child(notes)
                    .child(div().flex_none().mt(ui(16.)).child(icon)),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(0xf6f6f6))
            .border_t_1()
            .border_color(theme::column_divider_color())
            .child(vertical_viewport(body, &mut self.applications.notes_scrolling, cx))
    }

    fn note_page_button(
        &mut self,
        note: &details::ReleaseNote,
        next: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let (label, target) = if next {
            ("Next page", (note.page + 1).min(note.page_count() - 1))
        } else {
            ("Previous page", note.page.saturating_sub(1))
        };
        let key = format!("application-note-{}-{label}", note.build_id);
        self.applications.controls.insert(key.clone());
        let build = note.build_id.clone();
        let shell = cx.weak_entity();
        let activate: controls::Activate = Rc::new(move |_, cx| {
            let _ = shell.update(cx, |shell, cx| {
                if let Some(history) = &mut shell.applications.details.history
                    && let Some(note) = history.notes.iter_mut().find(|note| note.build_id == build)
                    && target < note.page_count()
                {
                    note.page = target;
                    cx.notify();
                }
            });
        });
        let theme = theme();
        let properties = controls::Properties {
            kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Default),
            label,
            disabled: target == note.page,
            theme: &theme,
        };
        let control = self.navigation_controls.entry(key).or_insert_with(|| {
            cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx))
        });
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
        });
        control.clone()
    }
}

fn release_date(seconds: u64) -> String {
    libc::time_t::try_from(seconds)
        .ok()
        .map(|t| super::super::session_log_view::formatted_time(t, c"%b %e, %Y at %H:%M"))
        .filter(|date| !date.is_empty())
        .unwrap_or_else(|| "—".into())
}
