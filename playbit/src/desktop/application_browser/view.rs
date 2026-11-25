//! App, session, logical scope and instance columns reuse native controls and Browser.
use super::scrolling::vertical_viewport;
use super::*;
use crate::ui::desktop::zoom::{length as ui, scaled};

impl Shell {
    pub(super) fn application_text(
        &mut self,
        key: &str,
        value: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let id = self.application_text_id(key, cx);
        host_text::element(value.into(), id, false, false)
    }

    fn application_text_id(
        &mut self,
        key: &str,
        cx: &mut Context<Self>,
    ) -> gpui::accesskit::NodeId {
        self.applications.rendered_texts.insert(key.into());
        let entity = self.applications.texts.entry(key.into()).or_insert_with(|| cx.new(|_| ()));
        text_editor::accessibility_text_ids(entity.entity_id()).0
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
        self.applications.push_row(column, row.clone());
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

    pub(in crate::desktop) fn application_rows(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<Entity<controls::Control>> {
        let mut rows = Vec::new();
        for app in self.applications.apps.clone() {
            if self.sidebar_volumes && !app.can_open {
                continue;
            }
            let selected = self.showing_application_path()
                && self.applications.selected.as_ref() == Some(&app.identity);
            let control = self.application_row(
                0,
                Row {
                    key: application_key(&app.identity),
                    target: Target::Application(app.identity.clone()),
                    selected,
                },
                &app.name,
                None,
                self.selected_session_id().is_none() && !self.applications.root_open,
                cx,
            );
            self.app_removal_menu(&control, app.identity, cx);
            let icon = self.session_icons.get(&app.build);
            let failed = self.session_icons.failed(&app.build);
            let loading = app.can_open && !self.session_icons.known(&app.build);
            control.update(cx, |control, cx| {
                control.set_compact_app_icon(cx);
                control.set_app_icon(icon, failed, cx);
                control.set_session_icon_loading(loading, cx);
            });
            rows.push(control);
        }
        rows
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

    pub(in crate::desktop) fn application_section(
        &mut self,
        key: &str,
        title: &'static str,
        cx: &mut Context<Self>,
    ) -> Section {
        Section { id: self.application_text_id(key, cx), title }
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
            (Role::LocalSession, true) if !self.sidebar_volumes => "Cache",
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
                control.set_navigation_child(self.sidebar_volumes && !private, cx);
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
        if !self.showing_application_path()
            || self.inline_session_form(cx).is_some()
            || self.personal.is_some()
            || self.invitation.is_some() && !self.session_dialog_open(cx)
        {
            return None;
        }
        let app = self
            .applications
            .apps
            .iter()
            .find(|app| Some(&app.identity) == self.applications.selected.as_ref())?
            .clone();
        if self.sidebar_volumes && !app.can_open {
            return None;
        }
        let selected = self.selected_session_id().map(str::to_owned);
        let busy = self.applications.action_busy
            || selected.is_none() && self.applications.creation_uncertain(&app)
            || selected.as_ref().is_some_and(|id| self.launches.contains_key(id));
        let header = self
            .application_heading(
                "application-title",
                &app.name,
                None,
                self.session_icons.get(&app.build),
                window.is_window_active(),
                cx,
            )
            .when(!self.sidebar_volumes && app.can_open, |header| {
                header.child(self.application_toggle(busy, cx))
            });
        let row_height = scaled(window, 32.);
        let mut session_rows = Vec::with_capacity(app.sessions.len() + app.invitations.len());
        for id in &app.sessions {
            let title = self.session_list_title(id, &app.sessions);
            let control = self.application_row(
                1,
                Row {
                    key: if self.sidebar_volumes {
                        format!("application-volume-session-{id}")
                    } else {
                        format!("application-session-{id}")
                    },
                    target: Target::Session(id.clone()),
                    selected: selected.as_ref() == Some(id)
                        && !(self.applications.root_open && !self.applications.scope_in_session),
                },
                &title,
                Some("app-browser-session"),
                selected.as_ref() == Some(id)
                    && (self.sidebar_volumes || self.applications.instance.is_none())
                    && !self.applications.root_open,
                cx,
            );
            if !self.sidebar_volumes {
                let peers = self
                    .applications
                    .activity
                    .get(id)
                    .map(|activity| activity.peers.as_str())
                    .unwrap_or("");
                control.update(cx, |control, cx| control.set_navigation_detail(peers, cx));
                self.session_row_interactions(&control, id, cx);
                let weak = cx.weak_entity();
                let id = id.clone();
                control.update(cx, |control, _| {
                    control.set_activation(Rc::new(move |_, cx| {
                        let _ =
                            weak.update(cx, |host, cx| host.open_retained_session(id.clone(), cx));
                    }));
                });
            }
            session_rows.push(control);
        }
        if !self.sidebar_volumes {
            for (session, root_name) in &app.invitations {
                let control = self.application_row(
                    1,
                    Row {
                        key: format!("application-invitation-{session}"),
                        target: Target::Invitation(session.clone()),
                        selected: false,
                    },
                    &format!("{root_name} · Invitation"),
                    Some("app-browser-session"),
                    false,
                    cx,
                );
                session_rows.push(control);
            }
        }
        let mut sessions = list("application-sessions");
        if self.sidebar_volumes {
            sessions =
                sessions.children(session_rows.into_iter().map(|row| div().h(ui(32.)).child(row)));
        } else if !session_rows.is_empty() {
            sessions = sessions.child(controls::virtual_rows(session_rows, row_height, px(0.)));
        }
        if app.can_open {
            sessions =
                sessions.child(self.application_section("application-app-data", "App data", cx));
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
        }
        let sessions = vertical_viewport(sessions, &mut self.applications.sessions_scrolling, cx);
        let mut content = div().size_full().flex().child(column().child(header).child(sessions));
        if self.applications.root_open
            && self.applications.selected_role.is_some()
            && !self.applications.scope_in_session
            && let Some(browser) = self.browser.clone()
        {
            let width = ui(browser::MIN_WIDTH);
            return Some(self.application_columns_viewport(
                content.child(div().flex_1().min_w(width).h_full().flex().child(browser)),
                cx,
            ));
        }
        let Some(session) = selected
            .and_then(|id| self.applications.catalog.iter().find(|session| session.id == id))
            .cloned()
        else {
            return Some(self.application_columns_viewport(
                content.child(div().flex_1().min_w_0().bg(rgb(0xececec))),
                cx,
            ));
        };
        let header = self
            .application_heading(
                "application-session-title",
                &self.session_list_title(&session.id, &app.sessions),
                Some("app-browser-session"),
                None,
                window.is_window_active(),
                cx,
            )
            .when(!self.sidebar_volumes, |header| {
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
                header.child(add)
            });
        let mut instances = list("application-instances");
        let owners = if !self.sidebar_volumes
            && self.applications.owners_session.as_deref() == Some(&session.id)
        {
            self.applications.owners.clone()
        } else {
            Vec::new()
        };
        if !self.sidebar_volumes {
            instances = self.instance_rows(instances, &session, &owners, row_height, cx);
            instances = instances.child(self.application_section(
                "application-session-data",
                "Session data",
                cx,
            ));
        }
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
        let instances =
            vertical_viewport(instances, &mut self.applications.instances_scrolling, cx);
        content = content.child(column().child(header).child(instances));
        if self.applications.root_open
            && let Some(browser) = self.browser.clone()
        {
            let width = ui(browser::MIN_WIDTH);
            return Some(self.application_columns_viewport(
                content.child(div().flex_1().min_w(width).h_full().flex().child(browser)),
                cx,
            ));
        }
        if self.sidebar_volumes {
            return Some(self.application_columns_viewport(content, cx));
        }
        let owner = self
            .applications
            .instance
            .and_then(|id| owners.iter().find(|owner| owner.instance.id == id))
            .cloned();
        let inspector = self.application_inspector(&session, owner.as_ref(), window, cx);
        let upper = self.application_columns_viewport(content.child(inspector), cx);
        Some(
            div()
                .h_full()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().h(ui(286.)).max_h(gpui::relative(0.8)).flex_none().flex().child(upper))
                .child(self.application_log(cx)),
        )
    }

    fn session_list_title(&self, id: &str, siblings: &[String]) -> String {
        self.applications
            .activity
            .get(id)
            .and_then(|a| a.date.clone())
            .unwrap_or_else(|| short_id(id, siblings, &self.sessions))
    }

    fn application_log(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let clear = self.host_callback(
            "application-clear-log",
            "Clear log",
            self.logs.read(cx).is_empty(),
            |shell, cx| shell.logs.update(cx, |logs, cx| logs.clear(cx)),
            cx,
        );
        clear.update(cx, |control, cx| {
            control.set_sidebar_icon(
                controls::SidebarIcon::Glyph("session-log-clear"),
                false,
                false,
                cx,
            )
        });
        div()
            .debug_selector(|| "application-log-pane".into())
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(0xf6f6f6))
            .border_t_1()
            .border_color(theme::column_divider_color())
            .child(self.session_log_content(cx))
            .child(
                div()
                    .absolute()
                    .top(ui(8.))
                    .right(ui(8.))
                    .bg(rgb(0xf6f6f6))
                    .rounded(ui(6.))
                    .child(clear),
            )
    }

    fn application_toggle(
        &mut self,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let target = self.application_stop_target();
        let label = if target.is_some() {
            "Stop instance"
        } else if self.selected_session_id().is_some() {
            "Resume session"
        } else {
            "Start new session"
        };
        let toggle = self.host_callback(
            if target.is_some() { "application-stop" } else { "application-play" },
            label,
            busy,
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
        toggle
    }

    fn instance_rows(
        &mut self,
        mut instances: gpui::Stateful<gpui::Div>,
        session: &Session,
        owners: &[LaunchOwner],
        row_height: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let mut rows = Vec::with_capacity(owners.len());
        for (index, owner) in owners.iter().enumerate() {
            let id = owner.instance.id;
            let row = self.application_row(
                2,
                Row {
                    key: format!("application-instance-{id}"),
                    target: Target::Instance(id),
                    selected: self.applications.instance == Some(id)
                        || (index == 0
                            && self.applications.instance.is_none()
                            && !self.applications.root_open),
                },
                &format!("Instance {}", index + 1),
                Some("app-browser-instance"),
                self.applications.instance == Some(id),
                cx,
            );
            let weak = cx.weak_entity();
            let session_id = session.id.clone();
            row.update(cx, |control, cx| {
                let state = instance_state(owner, &self.startup);
                control.set_navigation_detail(if state == "Running" { "" } else { state }, cx);
                control.set_activation(Rc::new(move |_, cx| {
                    let _ = weak.update(cx, |host, cx| {
                        host.show_application_instance(session_id.clone(), id, cx)
                    });
                }));
            });
            rows.push(row);
        }
        if !rows.is_empty() {
            instances = instances.child(controls::virtual_rows(rows, row_height, px(0.)));
        } else {
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
                    .py(ui(6.))
                    .line_height(ui(20.))
                    .text_size(ui(14.))
                    .text_color(rgba(0x00000066)),
            );
        }
        instances
    }

    pub(super) fn application_metadata(
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
                    .w(ui(50.))
                    .flex_none()
                    .text_color(rgba(0x00000080)),
            )
            .child(div().flex_1().min_w_0().flex().flex_col().child(editor))
    }
}

#[derive(Clone, gpui::IntoElement)]
pub(in crate::desktop) struct Section {
    id: gpui::accesskit::NodeId,
    title: &'static str,
}

impl gpui::RenderOnce for Section {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
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
            .child(host_text::element(self.title.into(), self.id, false, false))
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
