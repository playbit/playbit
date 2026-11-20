//! App, session, logical scope and instance columns reuse native controls and Browser.
use super::scrolling::vertical_viewport;
use super::*;
use crate::ui::desktop::zoom::length as ui;

impl Shell {
    pub(super) fn application_text(
        &mut self,
        key: &str,
        value: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.applications.rendered_texts.insert(key.into());
        let entity = self.applications.texts.entry(key.into()).or_insert_with(|| cx.new(|_| ()));
        let id = text_editor::accessibility_text_ids(entity.entity_id()).0;
        host_text::element(value.into(), id, false, false)
    }

    fn application_row(
        &mut self,
        column: usize,
        row: Row,
        label: &str,
        glyph: Option<&'static str>,
        active: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        while self.applications.columns.len() <= column {
            self.applications.columns.push(Vec::new());
        }
        self.applications.columns[column].push(row.clone());
        let Row { key, target, selected } = row;
        self.applications.controls.insert(key.clone());
        let weak = cx.weak_entity();
        let select: controls::Activate = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |host, cx| {
                host.select_application_target(target.clone(), cx);
            });
        });
        let control = self.navigation_button(key, label, selected, select.clone(), None, cx);
        control.update(cx, |control, cx| {
            control.set_sidebar_row();
            control.set_single_click(select.clone());
            control.set_navigation_selection_active(active, cx);
            if let Some(glyph) = glyph {
                control.set_navigation_glyph(glyph, cx);
            }
        });
        control
    }

    pub(in crate::desktop) fn application_list(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let mut rows = Vec::new();
        for app in self.applications.apps.clone() {
            let selected = self.applications.selected.as_ref() == Some(&app.identity);
            let control = self.application_row(
                0,
                Row {
                    key: application_key(&app.identity),
                    target: Target::Application(app.identity),
                    selected,
                },
                &app.name,
                None,
                self.selected_session_id().is_none(),
                cx,
            );
            let icon = self.session_icons.get(&app.build);
            let failed = self.session_icons.failed(&app.build);
            control.update(cx, |control, cx| {
                control.set_compact_app_icon(cx);
                control.set_app_icon(icon, failed, cx);
            });
            rows.push(control);
        }
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                gpui::uniform_list("applications", rows.len(), move |range, _, _| {
                    range
                        .map(|index| div().h(ui(32.)).px(ui(4.)).child(rows[index].clone()))
                        .collect::<Vec<_>>()
                })
                .track_scroll(self.session_scroll.clone())
                .h_full(),
            )
            .child(crate::ui::desktop::scrollbar::Scrollbars::overlay(&self.session_scrollbars))
    }

    pub(super) fn application_heading(
        &mut self,
        key: &str,
        title: &str,
        glyph: Option<&'static str>,
        icon: Option<Arc<gpui::RenderImage>>,
        active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let foreground = theme::section_title_color(active);
        div()
            .h(ui(44.))
            .min_h(px(44.))
            .flex_none()
            .flex()
            .items_center()
            .gap(ui(6.))
            .pl(ui(12.))
            .pr(ui(8.))
            .window_control_area(gpui::WindowControlArea::Drag)
            .text_size(ui(14.))
            .line_height(ui(24.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(foreground)
            .when_some(icon, |heading, icon| {
                heading.child(gpui::img(icon).size(ui(16.)).flex_none())
            })
            .when_some(glyph, |heading, glyph| {
                heading
                    .child(gpui::svg().path(glyph).size(ui(16.)).flex_none().text_color(foreground))
            })
            .child(self.application_text(key, title.to_owned(), cx).truncate().flex_1().min_w_0())
    }

    fn application_section(&mut self, key: &str, title: &str, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .h(ui(32.))
            .flex_none()
            .flex()
            .items_end()
            .px(ui(8.))
            .pb(ui(4.))
            .text_size(ui(11.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgba(0x00000066))
            .child(self.application_text(key, title.to_owned(), cx))
    }

    fn automatic_scope_row(
        &mut self,
        role: Role,
        session: bool,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let slots = if session {
            (self.applications.slots_session.as_deref() == self.selected_session_id())
                .then_some(&self.applications.session_slots)
        } else {
            (self.applications.slots_app.as_ref() == self.applications.selected.as_ref())
                .then_some(&self.applications.app_slots)
        };
        let slot = slots.and_then(|slots| slots.iter().find(|slot| slot.role == role)).cloned();
        let key = scope_key(role, session);
        let label = match (role, session) {
            (Role::Root, true) => "Root override",
            (Role::User, true) => "User override",
            (Role::Root, false) => "World",
            (Role::Session, _) => "Shared",
            (Role::User | Role::UserSession, _) => "User",
            (Role::Local, _) => "Local",
            (Role::LocalSession, _) => "Local session",
        };
        let glyph = if role.is_local() { "app-browser-local" } else { "volume-cell" };
        if let Some(slot) = &slot
            && slot.state != AutomaticVolumeState::Unavailable
        {
            let selected = self.applications.root_open
                && self.applications.selected_role == Some(role)
                && self.applications.scope_in_session == session;
            let control = self.application_row(
                if session { 2 } else { 1 },
                Row { key, target: Target::Scope { role, volume: slot.volume, session }, selected },
                label,
                Some(glyph),
                selected
                    && self
                        .browser
                        .as_ref()
                        .is_none_or(|browser| browser.read(cx).is_showing_volume()),
                cx,
            );
            let private = !slot.overridden
                && matches!(
                    role,
                    Role::User | Role::UserSession | Role::Local | Role::LocalSession
                );
            control.update(cx, |control, cx| {
                control.set_navigation_child(!private, cx);
            });
            let foreground = if window_active
                && selected
                && self.browser.as_ref().is_some_and(|browser| browser.read(cx).is_showing_volume())
            {
                rgba(0xffffffb3)
            } else {
                rgba(0x00000080)
            };
            return div().h(ui(32.)).relative().child(control).when(private, |row| {
                row.child(
                    gpui::svg()
                        .path("lock")
                        .size(ui(16.))
                        .absolute()
                        .right(ui(10.))
                        .top(ui(8.))
                        .text_color(foreground),
                )
            });
        }
        div()
            .h(ui(32.))
            .flex_none()
            .flex()
            .items_center()
            .gap(ui(6.))
            .px(ui(8.))
            .text_size(ui(14.))
            .child(gpui::svg().path(glyph).size(ui(16.)).flex_none().text_color(rgba(0x00000080)))
            .child(
                self.application_text(&format!("{key}-label"), label, cx)
                    .flex_1()
                    .min_w_0()
                    .text_color(rgba(0x00000080)),
            )
    }

    pub(in crate::desktop) fn application_browser_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if self.sidebar_volumes
            || self.new_session.is_some()
            || self.personal.is_some()
            || self.invitation.is_some()
        {
            return None;
        }
        let app = self
            .applications
            .apps
            .iter()
            .find(|app| Some(&app.identity) == self.applications.selected.as_ref())?
            .clone();
        let selected = self.selected_session_id().map(str::to_owned);
        let busy = self.applications.action_busy
            || self.applications.uncertain_app.as_ref() == Some(&app.identity)
                && app.sessions.is_empty()
            || selected.as_ref().is_some_and(|id| self.launches.contains_key(id));
        let target = self.application_stop_target();
        let ambiguous =
            selected.is_some() && target.is_none() && self.application_running_owners().count() > 1;
        let toggle = self.host_callback(
            if target.is_some() { "application-stop" } else { "application-play" },
            if target.is_some() {
                "Stop instance"
            } else if ambiguous {
                "Select an instance"
            } else {
                "Open app"
            },
            busy || ambiguous,
            |host, cx| host.toggle_application(cx),
            cx,
        );
        toggle.update(cx, |control, cx| {
            if target.is_some() {
                control.set_sidebar_icon(
                    controls::SidebarIcon::Glyph("app-browser-stop"),
                    false,
                    false,
                    cx,
                );
            } else {
                control.set_sidebar_icon(
                    controls::SidebarIcon::Glyph("session-start"),
                    false,
                    false,
                    cx,
                );
            }
        });
        let header = self
            .application_heading(
                "application-title",
                &app.name,
                None,
                self.session_icons.get(&app.build),
                window.is_window_active(),
                cx,
            )
            .child(toggle);
        let mut sessions = list("application-sessions");
        for id in &app.sessions {
            let control = self.application_row(
                1,
                Row {
                    key: format!("application-session-{id}"),
                    target: Target::Session(id.clone()),
                    selected: selected.as_ref() == Some(id)
                        && !(self.applications.root_open && !self.applications.scope_in_session),
                },
                &short_id(id, &app.sessions, &self.sessions),
                Some("app-browser-session"),
                selected.as_ref() == Some(id)
                    && self.applications.instance.is_none()
                    && !self.applications.root_open,
                cx,
            );
            self.session_row_interactions(&control, id, cx);
            let weak = cx.weak_entity();
            let id = id.clone();
            control.update(cx, |control, cx| {
                control.set_activation(Rc::new(move |_, cx| {
                    let _ = weak.update(cx, |host, cx| host.open_retained_session(id.clone(), cx));
                }));
                control.set_navigation_glyph("app-browser-session", cx);
            });
            sessions = sessions.child(div().h(ui(32.)).child(control));
        }
        sessions = sessions.child(self.application_section("application-app-data", "App data", cx));
        for role in [Role::Root, Role::User, Role::Local] {
            sessions = sessions.child(self.automatic_scope_row(
                role,
                false,
                window.is_window_active(),
                cx,
            ));
        }
        if let Some(error) = self.applications.scope_error.clone() {
            sessions = sessions.child(
                self.application_text("application-scope-error", error, cx)
                    .px(ui(8.))
                    .text_size(ui(11.)),
            );
        }
        let sessions = vertical_viewport(sessions, &mut self.applications.sessions_scrolling, cx);
        let mut content =
            div().flex_1().min_w_0().h_full().flex().child(column().child(header).child(sessions));
        if self.applications.root_open
            && self.applications.selected_role.is_some()
            && !self.applications.scope_in_session
            && let Some(browser) = self.browser.clone()
        {
            let width = ui(browser::MIN_WIDTH);
            return Some(content.child(div().flex_1().min_w(width).h_full().flex().child(browser)));
        }
        let Some(session) = selected
            .and_then(|id| self.applications.catalog.iter().find(|session| session.id == id))
            .cloned()
        else {
            return Some(content.child(div().flex_1().min_w_0().bg(rgb(0xececec))));
        };
        let add = self.host_callback(
            "application-new-instance",
            "New instance",
            busy,
            |host, cx| host.create_application_instance(cx),
            cx,
        );
        add.update(cx, |control, cx| {
            control.set_sidebar_icon(controls::SidebarIcon::Glyph("add"), false, false, cx)
        });
        let header = self
            .application_heading(
                "application-session-title",
                &short_id(&session.id, &app.sessions, &self.sessions),
                Some("app-browser-session"),
                None,
                window.is_window_active(),
                cx,
            )
            .child(add);
        let mut instances = list("application-instances");
        let owners = if self.applications.owners_session.as_deref() == Some(&session.id) {
            self.applications.owners.clone()
        } else {
            Vec::new()
        };
        for (index, owner) in owners.iter().enumerate() {
            let id = owner.instance.id;
            let row = self.application_row(
                2,
                Row {
                    key: format!("application-instance-{id}"),
                    target: Target::Instance(id),
                    selected: self.applications.instance == Some(id),
                },
                &format!("Instance {}", index + 1),
                Some("app-browser-instance"),
                true,
                cx,
            );
            let weak = cx.weak_entity();
            let session_id = session.id.clone();
            row.update(cx, |control, cx| {
                control.set_navigation_detail(instance_state(owner, &self.startup), cx);
                control.set_activation(Rc::new(move |_, cx| {
                    let _ = weak.update(cx, |host, cx| {
                        host.show_application_instance(session_id.clone(), id, cx)
                    });
                }));
            });
            instances = instances.child(div().h(ui(32.)).child(row));
        }
        if owners.is_empty() {
            let text = if let Some(error) = &self.applications.instance_error {
                error.clone()
            } else if self.applications.refreshing {
                "Loading instances…".into()
            } else {
                "No running instances".into()
            };
            instances = instances.child(
                self.application_text("application-empty-instances", text, cx)
                    .px(ui(8.))
                    .py(ui(8.))
                    .text_size(ui(11.))
                    .text_color(rgba(0x00000080)),
            );
        }
        instances = instances.child(self.application_section(
            "application-session-data",
            "Session data",
            cx,
        ));
        for role in [Role::Session, Role::UserSession, Role::LocalSession] {
            instances = instances.child(self.automatic_scope_row(
                role,
                true,
                window.is_window_active(),
                cx,
            ));
        }
        if self.applications.slots_session.as_deref() == Some(&session.id) {
            for role in [Role::Root, Role::User] {
                if self
                    .applications
                    .session_slots
                    .iter()
                    .any(|slot| slot.role == role && slot.overridden)
                {
                    instances = instances.child(self.automatic_scope_row(
                        role,
                        true,
                        window.is_window_active(),
                        cx,
                    ));
                }
            }
        }
        instances =
            instances.child(self.application_section("application-session-tools", "Session", cx));
        let logs = self.application_row(
            2,
            Row {
                key: "application-log".into(),
                target: Target::Log,
                selected: self.applications.log_open,
            },
            "Log",
            Some("entry-append-log"),
            true,
            cx,
        );
        instances = instances.child(div().h(ui(32.)).child(logs));
        let share = self.host_callback(
            "application-share",
            "Share session",
            self.session_action_busy || self.applications.action_busy,
            |host, cx| host.share_session(cx),
            cx,
        );
        instances = instances.child(div().px(ui(8.)).py(ui(8.)).child(share));
        let instances =
            vertical_viewport(instances, &mut self.applications.instances_scrolling, cx);
        content = content.child(column().child(header).child(instances));
        if self.applications.root_open
            && let Some(browser) = self.browser.clone()
        {
            let width = ui(browser::MIN_WIDTH);
            return Some(content.child(div().flex_1().min_w(width).h_full().flex().child(browser)));
        }
        if self.applications.log_open {
            return Some(
                content.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(self.session_header(window, cx))
                        .child(self.session_log_content(cx)),
                ),
            );
        }
        let owner = self
            .applications
            .instance
            .and_then(|id| owners.iter().find(|owner| owner.instance.id == id))
            .cloned();
        Some(content.child(self.application_inspector(
            &session,
            owner.as_ref(),
            window.is_window_active(),
            cx,
        )))
    }

    fn application_inspector(
        &mut self,
        session: &Session,
        owner: Option<&LaunchOwner>,
        active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let title = if owner.is_some() { "Session Instance" } else { "Session" };
        let mut heading =
            self.application_heading("application-inspector-title", title, None, None, active, cx);
        if let Some(owner) = owner {
            let id = owner.instance.id;
            let session = session.id.clone();
            let stop = self.host_callback(
                "application-instance-stop",
                "Stop",
                self.applications.action_busy || !is_running(owner),
                move |host, cx| host.stop_application_instance(session.clone(), id, cx),
                cx,
            );
            heading = heading.pr(ui(10.)).child(div().flex_none().flex().occlude().child(stop));
        }
        let mut body = div()
            .id("application-inspector-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        let id = owner.map_or_else(
            || {
                self.sessions
                    .iter()
                    .find(|row| row.id == session.id)
                    .map(|row| row.reference.clone())
                    .unwrap_or_else(|| "Unavailable".into())
            },
            |owner| owner.instance.id.to_string(),
        );
        body = body.child(self.application_metadata("application-id", "ID", &id, cx));
        body = body.child(self.application_metadata(
            "application-root-id",
            "Root",
            &owner.map_or(session.root_volume, |owner| owner.instance.root).to_string(),
            cx,
        ));
        if let Some(owner) = owner {
            body = body.child(self.application_metadata(
                "application-state",
                "State",
                instance_state(owner, &self.startup),
                cx,
            ));
            if let Some(process) = &owner.process {
                body = body.child(self.application_metadata(
                    "application-process",
                    "Process",
                    &process.pid().to_string(),
                    cx,
                ));
            }
            let heading = self
                .application_heading(
                    "application-mounts-title",
                    "Local directories",
                    None,
                    None,
                    active,
                    cx,
                )
                .border_t_1()
                .border_color(theme::column_divider_color());
            body = body.child(heading);
            body = body.child(
                div()
                    .flex()
                    .gap(ui(12.))
                    .px(ui(12.))
                    .h(ui(24.))
                    .items_center()
                    .text_size(ui(11.))
                    .text_color(rgba(0x000000b3))
                    .child(
                        self.application_text(
                            "application-mount-path-heading",
                            "Local directory",
                            cx,
                        )
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                    )
                    .child(
                        self.application_text("application-mount-name-heading", "Name", cx)
                            .w(ui(100.)),
                    )
                    .child(
                        self.application_text("application-mount-mode-heading", "Permissions", cx)
                            .w(ui(88.)),
                    ),
            );
            if owner.instance.host_mounts.is_empty() {
                body = body.child(
                    self.application_text(
                        "application-no-mounts",
                        "No local directories granted",
                        cx,
                    )
                    .p(ui(12.))
                    .text_size(ui(11.))
                    .text_color(rgba(0x00000080)),
                );
            }
            for mount in &owner.instance.host_mounts {
                let path = mount.path().display().to_string();
                body = body.child(
                    div()
                        .flex()
                        .gap(ui(12.))
                        .px(ui(12.))
                        .py(ui(6.))
                        .text_size(ui(11.))
                        .child(
                            self.application_text(
                                &format!("application-mount-path-{}", mount.id),
                                path,
                                cx,
                            )
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                        )
                        .child(
                            self.application_text(
                                &format!("application-mount-name-{}", mount.id),
                                mount.label.clone(),
                                cx,
                            )
                            .w(ui(100.))
                            .truncate(),
                        )
                        .child(
                            self.application_text(
                                &format!("application-mount-mode-{}", mount.id),
                                if mount.write { "Read + Write" } else { "Read-only" },
                                cx,
                            )
                            .w(ui(88.)),
                        ),
                );
            }
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

    fn application_metadata(
        &mut self,
        key: &'static str,
        label: &str,
        value: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let editor = self
            .applications
            .identifiers
            .entry(key)
            .or_insert_with(|| cx.new(|cx| text_editor::TextEditor::new(value, cx).selectable_id()))
            .clone();
        editor.update(cx, |editor, cx| editor.acknowledge(None, value, cx));
        div()
            .h(ui(24.))
            .flex_none()
            .flex()
            .items_center()
            .gap(ui(6.))
            .px(ui(12.))
            .text_size(ui(11.))
            .child(
                self.application_text(&format!("{key}-label"), label.to_owned(), cx)
                    .w(ui(54.))
                    .flex_none()
                    .text_color(rgba(0x00000080)),
            )
            .child(div().flex_1().min_w_0().flex().flex_col().child(editor))
    }
}

fn column() -> gpui::Div {
    div()
        .w(ui(240.))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(rgb(0xececec))
        .border_r_1()
        .border_color(theme::column_divider_color())
        .horizontal_focus_reveal_scope()
}

fn list(key: &'static str) -> gpui::Stateful<gpui::Div> {
    div().id(key).flex_1().min_h_0().overflow_y_scroll().px(ui(4.))
}
