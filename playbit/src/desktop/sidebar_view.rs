//! Session and volume lists, sidebar controls, and keyboard navigation.
use super::*;
use crate::ui::desktop::zoom::length as ui;
use gpui::Focusable;
use playbit_api::ui::{Theme, ThemeColor as Role};

pub(super) fn sort_volumes(volumes: &mut [crate::store::Volume]) {
    if volumes.len() <= 256 {
        volumes.sort_unstable_by(|a, b| {
            crate::ui::collation::compare(&a.name, &b.name).then_with(|| a.id.cmp(&b.id))
        });
    }
}

pub(super) struct VolumeRename {
    volume: crate::store::Id,
    editor: Entity<text_editor::TextEditor>,
    focus: bool,
    pending: bool,
}
impl VolumeRename {
    pub(super) fn volume(&self) -> crate::store::Id {
        self.volume
    }
    pub(super) fn focused(&self, window: &Window, cx: &App) -> bool {
        self.editor.read(cx).focus_handle(cx).is_focused(window)
    }
}

impl Shell {
    pub(super) fn navigate_management_lists(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.modified()
            || !matches!(key, "up" | "down" | "left" | "right")
            || self.account_name_open
        {
            return;
        }
        if self.navigate_application_columns(key, window, cx) {
            return;
        }
        if self.new_session.is_none()
            && let Some(browser) = &self.browser
        {
            match browser.update(cx, |browser, cx| browser.navigate_list(key, window, cx)) {
                browser::ListNavigation::Handled => {
                    cx.stop_propagation();
                    return;
                }
                browser::ListNavigation::PreviousColumn => {
                    let id = browser.read(cx).volume.id;
                    if let Some(index) = self.volumes.iter().position(|volume| volume.id == id)
                        && let Some(control) = self.navigation_controls.get(&format!("volume-{id}"))
                    {
                        let pending_before = self.volumes.len() < 256
                            && self
                                .pending_volume
                                .as_ref()
                                .is_some_and(|pending| pending.position(&self.volumes) <= index);
                        self.volume_scroll.scroll_to_item(
                            index + usize::from(pending_before),
                            gpui::ScrollStrategy::Top,
                        );
                        control.update(cx, |control, cx| control.focus_from_arrow(window, cx));
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                browser::ListNavigation::Unhandled => {}
            }
        }
        if matches!(key, "up" | "down") {
            self.navigate_sidebar(event, window, cx);
        } else if self.sidebar_volumes
            && self.volumes.iter().any(|volume| {
                self.navigation_controls
                    .get(&format!("volume-{}", volume.id))
                    .is_some_and(|control| control.read(cx).focused(window))
            })
        {
            if key == "right"
                && self.new_session.is_none()
                && let Some(browser) = &self.browser
            {
                browser.update(cx, |browser, cx| browser.focus_entry_list(window, cx));
            }
            cx.stop_propagation();
        }
    }

    fn navigate_sidebar(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = match event.keystroke.key.as_str() {
            "up" => false,
            "down" => true,
            _ => return,
        };
        if self.sidebar_volumes {
            self.navigate_volume_list(next, window, cx);
        } else {
            self.navigate_session_list(next, window, cx);
        }
    }

    fn navigate_volume_list(&mut self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.volumes.len() > 256 {
            return;
        }
        let Some(index) = self.volumes.iter().position(|volume| {
            self.navigation_controls
                .get(&format!("volume-{}", volume.id))
                .is_some_and(|control| control.read(cx).focused(window))
        }) else {
            return;
        };
        let target =
            if next { (index + 1).min(self.volumes.len() - 1) } else { index.saturating_sub(1) };
        if target == index {
            cx.stop_propagation();
            return;
        }
        let volume = self.volumes[target].clone();
        if let Some(control) =
            self.navigation_controls.get(&format!("volume-{}", volume.id)).cloned()
        {
            if !self.select_volume(volume, cx) {
                cx.stop_propagation();
                return;
            }
            let pending_before = self.volumes.len() < 256
                && self
                    .pending_volume
                    .as_ref()
                    .is_some_and(|pending| pending.position(&self.volumes) <= target);
            self.volume_scroll.scroll_to_item(
                target + usize::from(pending_before),
                if next { gpui::ScrollStrategy::Bottom } else { gpui::ScrollStrategy::Top },
            );
            control.update(cx, |control, cx| control.focus_from_arrow(window, cx));
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn navigate_session_list(&mut self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        let sessions = &self.sessions;
        if sessions.len() > 256 {
            return;
        }
        let Some(index) = sessions.iter().position(|s| {
            self.navigation_controls.get(&s.id).is_some_and(|c| c.read(cx).focused(window))
        }) else {
            return;
        };
        let target =
            if next { (index + 1).min(sessions.len() - 1) } else { index.saturating_sub(1) };
        if target == index {
            cx.stop_propagation();
            return;
        }
        let id = sessions[target].id.clone();
        if let Some(control) = self.navigation_controls.get(&id).cloned() {
            if !self.select_sidebar_session(id, cx) {
                cx.stop_propagation();
                return;
            }
            self.session_scroll.scroll_to_item(
                target,
                if next { gpui::ScrollStrategy::Bottom } else { gpui::ScrollStrategy::Top },
            );
            control.update(cx, |control, cx| control.focus_from_arrow(window, cx));
            cx.notify();
            cx.stop_propagation();
        }
    }

    pub(super) fn select_sidebar_session(&mut self, id: String, cx: &mut Context<Self>) -> bool {
        if !self.prepare_browser_navigation(cx) {
            return false;
        }
        self.sidebar_session = Some(id);
        self.sidebar_volumes = false;
        self.browser = None;
        self.personal = None;
        self.new_session = None;
        self.invitation = None;
        self.opening = None;
        self.joining = false;
        self.error.clear();
        self.refresh_logs(cx);
        self.select_application_session(cx);
        cx.notify();
        true
    }

    pub fn sidebar_view(&mut self, window_active: bool, cx: &mut Context<Self>) -> gpui::Div {
        let _timing = crate::trace::Span::new("ui.management.sidebar");
        let theme = theme();
        let join = self.invitation_button(cx);
        let account = self.account_button(cx);
        let sessions = self.sessions.clone();
        let selected =
            if self.new_session.as_ref().is_some_and(|form| form.read(cx).creates_session()) {
                None
            } else {
                self.sidebar_session
                    .as_deref()
                    .or(self.opening.as_deref())
                    .or_else(|| self.selected.as_ref().map(|p| p.session.id.as_str()))
                    .map(str::to_owned)
            };
        self.retain_navigation_controls(&sessions, cx);
        let list = if self.applications.available() {
            self.application_list(cx)
        } else {
            let rows = self.session_rows(&sessions, selected.as_deref(), cx);
            self.session_list(rows, sessions.is_empty(), &theme, cx)
        };
        let list = if self.sidebar_volumes { self.volume_list(cx) } else { list };
        self.sidebar_frame(join, account, list, &theme, window_active, cx)
    }
    fn invitation_button(&mut self, cx: &mut Context<Self>) -> Entity<controls::Control> {
        let weak = cx.weak_entity();
        self.navigation_button(
            "join".into(),
            if self.sidebar_volumes { "New volume" } else { "New session" },
            self.browser.is_none()
                && self.personal.is_none()
                && self.opening.is_none()
                && self.invitation.is_none()
                && self.selected_session_id().is_none()
                && (self.sidebar_volumes
                    || self.new_session.is_some()
                    || !self.applications.has_selection()),
            Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if !this.prepare_browser_navigation(cx) {
                        return;
                    }
                    if this.sidebar_volumes {
                        this.start_volume_creation(
                            Request::CreateVolume {
                                share_key_with_service: true,
                                name: "Untitled".into(),
                            },
                            cx,
                        );
                        return;
                    }
                    this.sidebar_session = None;
                    this.cancel_invitation_restoration();
                    this.browser = None;
                    this.selected = None;
                    this.applications.clear_selection();
                    this.invitation = None;
                    this.personal = None;
                    this.new_session = None;
                    this.joining = false;
                    this.error.clear();
                    if let Some(session) = this.opening.take() {
                        this.pending_windows.remove(&session);
                    }
                    this.refresh_logs(cx);
                    cx.notify();
                });
            }),
            Some("add"),
            cx,
        )
    }
    fn retain_navigation_controls(&mut self, sessions: &[SessionRow], cx: &mut Context<Self>) {
        let draft_keys: std::collections::BTreeSet<_> = drafts::snapshots_for(self.account.id, cx)
            .iter()
            .map(|draft| format!("volume-draft-{}", draft.id))
            .collect();
        self.navigation_controls.retain(|key, control| {
            let keep = draft_keys.contains(key)
                || key.starts_with("recent-")
                || key == "join"
                || key == "account"
                || key == "sidebar-volumes"
                || key == "sidebar-sessions"
                || key.starts_with("application-")
                || sessions.iter().any(|s| s.id == *key)
                || self.volumes.len() <= 256
                    && self.volumes.iter().any(|volume| *key == format!("volume-{}", volume.id));
            if !keep {
                control.update(cx, |control, _| control.retire());
            }
            keep
        });
    }
    fn session_rows(
        &mut self,
        sessions: &[SessionRow],
        selected: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Vec<Entity<controls::Control>> {
        let mut rows = Vec::with_capacity(sessions.len().min(256));
        if sessions.len() > 256 {
            self.error = "This session list exceeds the 256-session display limit.".into();
        } else {
            for session in sessions {
                let id = session.id.as_str();
                let label = session_label(session, sessions);
                let weak = cx.weak_entity();
                let session_id = id.to_owned();
                let control = self.navigation_button(
                    id.into(),
                    &label,
                    selected == Some(id),
                    Rc::new(move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.open_retained_session(session_id.clone(), cx)
                        });
                    }),
                    None,
                    cx,
                );
                self.session_row_interactions(&control, id, cx);
                let version = session.resolved_build_id.as_str();
                let icon = self.session_icons.get(version);
                let failed = self.session_icons.failed(version);
                let icon_loading = !self.session_icons.known(version);
                let name = session.app_name.is_none().then(|| {
                    let id = session_id_prefix(session, sessions).to_owned().into();
                    if session_info_failed(session) {
                        controls::SessionName::Unavailable(id)
                    } else {
                        controls::SessionName::Loading(id)
                    }
                });
                control.update(cx, |control, cx| {
                    control.set_sidebar_row();
                    control.set_launch_progress(
                        self.launches.get(id).map(|progress| progress.phase.label()),
                        cx,
                    );
                    control.set_app_icon(icon, failed, cx);
                    control.set_session_placeholders(icon_loading, name, cx);
                    control.set_session_info_error(session.build_state.message(), cx);
                });
                rows.push(control);
            }
        }
        rows
    }
    fn session_list(
        &mut self,
        rows: Vec<Entity<controls::Control>>,
        sessions_empty: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let color = |role| rgba(theme.color(role).0);
        let mut list = div().relative().flex_1().min_h_0();
        if rows.is_empty() {
            list = list.child(
                self.host_text(
                    HostText::EmptySessions,
                    if self.enrolled.is_none() {
                        if self.error.is_empty() {
                            "Loading sessions…"
                        } else {
                            "Sessions unavailable"
                        }
                    } else if sessions_empty {
                        "No saved sessions"
                    } else {
                        "Session list unavailable"
                    },
                    cx,
                )
                .px(ui(12.))
                .py(ui(12.))
                .text_size(ui(13.))
                .text_color(color(Role::TextSecondary)),
            );
        } else {
            list = list
                .child(
                    gpui::uniform_list("sessions", rows.len(), move |range, _, _| {
                        range
                            .map(|i| div().h(ui(32.)).px(ui(4.)).child(rows[i].clone()))
                            .collect::<Vec<_>>()
                    })
                    .track_scroll(self.session_scroll.clone())
                    .h_full(),
                )
                .child(crate::ui::desktop::scrollbar::Scrollbars::overlay(
                    &self.session_scrollbars,
                ));
        }
        list
    }
    fn volume_list(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let mut volumes = div().relative().flex_1().min_h_0().on_key_down(cx.listener(
            |this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.cancel_volume_rename(window, cx) {
                    cx.stop_propagation();
                }
            },
        ));
        let entries = self.volumes.clone();
        if entries.len() > 256 {
            volumes = volumes.child(
                self.host_text(
                    HostText::VolumeCapacity,
                    "Volume list exceeds the 256-volume display limit.",
                    cx,
                )
                .text_size(ui(13.)),
            );
        } else {
            let mut rows = Vec::with_capacity(entries.len());
            for volume in entries {
                let control = self.volume_button(volume.clone(), cx);
                let editor = self
                    .volume_rename
                    .as_ref()
                    .filter(|rename| rename.volume == volume.id)
                    .map(|rename| rename.editor.clone());
                let selected = self
                    .browser
                    .as_ref()
                    .is_some_and(|browser| browser.read(cx).volume.id == volume.id);
                rows.push((control, editor, selected));
            }
            if let Some(pending) = &self.pending_volume
                && rows.len() < 256
            {
                rows.insert(
                    pending.position(&self.volumes),
                    (pending.control.clone(), None, false),
                );
            }
            if !rows.is_empty() {
                volumes = volumes
                    .child(
                        gpui::uniform_list("volumes", rows.len(), move |range, _, _| {
                            range
                                .map(|i| {
                                    let (control, editor, selected) = &rows[i];
                                    let row = if let Some(editor) = editor {
                                        div()
                                            .h_full()
                                            .flex()
                                            .items_center()
                                            .gap(ui(6.))
                                            .px(ui(8.))
                                            .rounded(ui(4.))
                                            .when(*selected, |row| {
                                                row.bg(rgba(theme().color(Role::MenuSelection).0))
                                            })
                                            .child(
                                                gpui::svg()
                                                    .path("volume-cell")
                                                    .size(ui(16.))
                                                    .flex_none()
                                                    .text_color(if *selected {
                                                        gpui::white()
                                                    } else {
                                                        gpui::black()
                                                    }),
                                            )
                                            .child(div().flex_1().min_w_0().child(editor.clone()))
                                    } else {
                                        div().child(control.clone())
                                    };
                                    div().h(ui(32.)).px(ui(4.)).child(row)
                                })
                                .collect::<Vec<_>>()
                        })
                        .track_scroll(self.volume_scroll.clone())
                        .w_full()
                        .h_full(),
                    )
                    .child(crate::ui::desktop::scrollbar::Scrollbars::overlay(
                        &self.volume_scrollbars,
                    ));
            }
        }
        volumes
    }
    fn volume_button(
        &mut self,
        volume: crate::store::Volume,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let id = volume.id;
        let selected =
            self.browser.as_ref().is_some_and(|browser| browser.read(cx).volume.id == id);
        let selection_active = !selected
            || self.browser.as_ref().is_none_or(|browser| browser.read(cx).is_showing_volume());
        let weak = cx.weak_entity();
        let label = display_label(&volume.name);
        let activate: controls::Activate = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.select_volume(volume.clone(), cx));
        });
        let control = self.navigation_button(
            format!("volume-{id}"),
            &label,
            selected,
            activate.clone(),
            None,
            cx,
        );
        self.volume_row_interactions(&control, id, cx);
        let rename = (selected
            && self.volume_rename.is_none()
            && self
                .browser
                .as_ref()
                .is_some_and(|browser| !browser.read(cx).has_pending_entry_operation()))
        .then(|| {
            let host = cx.weak_entity();
            Rc::new(move |_, cx: &mut App| {
                let _ = host.update(cx, |host, cx| {
                    if host.sidebar_volumes
                        && host
                            .browser
                            .as_ref()
                            .is_some_and(|browser| browser.read(cx).volume.id == id)
                    {
                        host.rename_volume(id, cx);
                    }
                });
            }) as controls::Activate
        });
        let accessory = self.volume_draft_button(id, cx);
        control.update(cx, |control, cx| {
            control.set_navigation_accessory(accessory, cx);
            control.set_single_click(activate);
            control.set_delayed_rename(rename);
            control.set_sidebar_row();
            control.set_navigation_glyph("volume-cell", cx);
            control.set_navigation_selection_active(selection_active, cx);
        });
        control
    }
    fn volume_draft_button(
        &mut self,
        volume: crate::store::Id,
        cx: &mut Context<Self>,
    ) -> Option<Entity<controls::Control>> {
        let draft = drafts::snapshots_for(self.account.id, cx)
            .into_iter()
            .find(|draft| draft.volume == volume)?;
        let id = draft.id;
        let weak = cx.weak_entity();
        let activate: controls::Activate = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |host, cx| host.open_local_draft(id, cx));
        });
        let theme = playbit_api::ui::Theme::light();
        let properties = controls::Properties {
            kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Default),
            label: "Contains draft changes",
            disabled: false,
            theme: &theme,
        };
        let control = self
            .navigation_controls
            .entry(format!("volume-draft-{id}"))
            .or_insert_with(|| {
                cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx))
            })
            .clone();
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
            control.set_navigation_accessory_icon("draft-changes", cx);
        });
        Some(control)
    }

    fn open_local_draft(&mut self, id: crate::store::Id, cx: &mut Context<Self>) {
        let Some(draft) = drafts::get_for(self.account.id, id, cx) else {
            return;
        };
        let Some(volume) = self.volumes.iter().find(|volume| volume.id == draft.volume).cloned()
        else {
            self.error = "The volume containing these local changes is unavailable.".into();
            cx.notify();
            return;
        };
        if self.select_volume(volume, cx)
            && let Some(browser) = &self.browser
        {
            browser.update(cx, |browser, cx| {
                browser.open_local_draft(id, cx);
            });
        }
    }

    pub(super) fn catalog_volume(&self, id: crate::store::Id) -> Option<&crate::store::Volume> {
        self.volumes.iter().chain(&self.scoped_volumes).find(|volume| volume.id == id)
    }

    pub(super) fn set_browser_volumes(&self, browser: &mut browser::Browser) {
        let mut volumes = Vec::new();
        if self.volumes.len() + self.scoped_volumes.len() <= 256 {
            volumes.extend(self.volumes.iter().chain(&self.scoped_volumes).cloned());
            if !self.scoped_volumes.is_empty() {
                sort_volumes(&mut volumes);
            }
        }
        browser.set_volumes(volumes);
    }

    pub(super) fn select_volume(
        &mut self,
        volume: crate::store::Volume,
        cx: &mut Context<Self>,
    ) -> bool {
        self.select_volume_in(volume, None, cx)
    }

    pub(super) fn select_volume_in(
        &mut self,
        volume: crate::store::Volume,
        session: Option<[u8; 32]>,
        cx: &mut Context<Self>,
    ) -> bool {
        let same_target = |browser: &browser::Browser| {
            browser.volume.id == volume.id && browser.shared_session() == session
        };
        if self.browser.as_ref().is_some_and(|browser| !same_target(browser.read(cx)))
            && !self.prepare_browser_navigation(cx)
        {
            return false;
        }
        self.sidebar_volumes = true;
        self.cancel_invitation_restoration();
        self.personal = None;
        self.new_session = None;
        self.invitation = None;
        self.error.clear();
        if let Some(browser) = self.browser.as_ref().filter(|browser| same_target(browser.read(cx)))
        {
            let selected = browser.update(cx, |browser, cx| browser.show_volume(cx));
            cx.notify();
            return selected;
        }
        let browser = cx.new(|cx| {
            let mut browser = browser::Browser::new(volume, self.send.for_browser(session), cx);
            browser.set_account(self.account.id, self.account_profile.display_name.as_deref());
            self.set_browser_volumes(&mut browser);
            browser.set_session_shared_volumes(self.applications.sessions());
            browser
        });
        self.browser_notice =
            Some(cx.subscribe(&browser, |host, _, notice: &browser::Notice, cx| {
                host.show_visual_bell(&notice.0, "draft-changes", cx)
            }));
        self.browser = Some(browser);
        cx.notify();
        true
    }

    pub(super) fn prepare_browser_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        self.browser
            .as_ref()
            .is_none_or(|browser| browser.update(cx, |browser, cx| browser.prepare_navigation(cx)))
    }

    pub(super) fn rename_volume(
        &mut self,
        volume: crate::store::Id,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.volume_rename.is_some() {
            return false;
        }
        let Some(index) = self.volumes.iter().position(|row| row.id == volume) else {
            return false;
        };
        if let Some(browser) = &self.browser
            && browser.read(cx).has_pending_entry_operation()
        {
            browser.update(cx, |browser, cx| {
                browser.show_hint(
                    "Finish naming the entry or wait for the file operation before renaming a volume.",
                    cx,
                );
            });
            return false;
        }
        let row = self.volumes[index].clone();
        let pending_before = self.volumes.len() < 256
            && self
                .pending_volume
                .as_ref()
                .is_some_and(|pending| pending.position(&self.volumes) <= index);
        self.volume_scroll
            .scroll_to_item(index + usize::from(pending_before), gpui::ScrollStrategy::Top);
        let host = cx.weak_entity();
        let editor = cx.new(|cx| {
            let editor_id = cx.entity_id();
            let commit: text_editor::CommitCallback = Arc::new(move |text, _, cx| {
                let name = text.to_owned();
                let host = host.clone();
                cx.defer(move |cx| {
                    let _ = host.update(cx, |host, cx| {
                        host.commit_volume_rename(editor_id, volume, name, cx)
                    });
                });
                true
            });
            text_editor::TextEditor::new(row.name, cx)
                .literal()
                .inline_name()
                .bound_edits(256)
                .on_commit(commit.clone())
                .on_blur(commit)
        });
        self.volume_rename = Some(VolumeRename { volume, editor, focus: true, pending: false });
        cx.notify();
        true
    }

    fn commit_volume_rename(
        &mut self,
        editor: gpui::EntityId,
        volume: crate::store::Id,
        name: String,
        cx: &mut Context<Self>,
    ) {
        if self
            .volume_rename
            .as_ref()
            .is_none_or(|rename| rename.editor.entity_id() != editor || rename.pending)
        {
            return;
        }
        if name.trim().is_empty() || name.len() > 256 {
            self.show_visual_bell(
                "A volume name must contain 1 to 256 UTF-8 bytes.",
                "menu-talk",
                cx,
            );
            cx.notify();
            return;
        }
        let Some(current) = self.volumes.iter().find(|row| row.id == volume) else {
            self.volume_rename = None;
            return;
        };
        if current.name != name {
            if let Err(error) = self.send.send(Request::LabelVolume { volume, label: name }) {
                self.error = error.into();
                cx.notify();
                return;
            }
            if let Some(rename) = &mut self.volume_rename {
                rename.pending = true;
                rename.editor.update(cx, |editor, _| editor.set_enabled(false));
            }
        } else {
            self.volume_rename = None;
        }
        cx.notify();
    }

    pub(super) fn finish_volume_rename(
        &mut self,
        volume: crate::store::Id,
        success: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .volume_rename
            .as_ref()
            .is_none_or(|rename| rename.volume != volume || !rename.pending)
        {
            return;
        }
        if success {
            self.volume_rename = None;
        } else if let Some(rename) = &mut self.volume_rename {
            rename.pending = false;
            rename.focus = true;
            rename.editor.update(cx, |editor, _| editor.set_enabled(true));
        }
        cx.notify();
    }

    pub(super) fn focus_volume_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = &mut self.volume_rename else { return };
        if rename.focus {
            rename.focus = false;
            rename.editor.update(cx, |editor, cx| {
                window.focus(&editor.focus_handle(cx));
                editor.select_all(cx);
            });
        }
    }

    pub(super) fn cancel_volume_rename(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(rename) = &self.volume_rename else { return false };
        if rename.pending || !rename.focused(window, cx) {
            return false;
        }
        self.volume_rename = None;
        window.focus(&self.focus);
        cx.notify();
        true
    }

    fn artwork_error(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let color = |role| rgba(theme.color(role).0);
        self.host_text(
            HostText::ArtworkError,
            if self.session_icons.failures() == 1 {
                "Artwork unavailable for one app".into()
            } else {
                format!("Artwork unavailable for {} apps", self.session_icons.failures())
            },
            cx,
        )
        .px(ui(12.))
        .py(ui(4.))
        .text_size(ui(11.))
        .text_color(color(Role::TextSecondary))
    }
    fn sidebar_frame(
        &mut self,
        join: Entity<controls::Control>,
        account: Entity<controls::Control>,
        list: gpui::Div,
        theme: &Theme,
        window_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let ap_known = self.ap_status.read(cx).known();
        let muted = account.read(cx).popup_is_open();
        let volumes = self.sidebar_tab(true, muted, cx);
        let sessions = self.sidebar_tab(false, muted, cx);
        let new_session = !self.sidebar_volumes
            && self.browser.is_none()
            && self.personal.is_none()
            && self.opening.is_none()
            && self.invitation.is_none()
            && self.selected_session_id().is_none()
            && (self.new_session.is_some() || !self.applications.has_selection());
        join.update(cx, |control, cx| {
            control.set_sidebar_icon(controls::SidebarIcon::Glyph("add"), new_session, false, cx)
        });
        div()
            .w(px(241. * crate::ui::desktop::zoom::factor(cx)
                + 87. * (1. - crate::ui::desktop::zoom::factor(cx)).max(0.)))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0xececec))
            .border_r_1()
            .border_color(super::theme::column_divider_color())
            .child(
                div()
                    .h(ui(44.))
                    .min_h(px(44.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(87. * crate::ui::desktop::zoom::factor(cx).max(1.)))
                    .pr(ui(8.))
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .text_size(ui(14.))
                    .line_height(ui(24.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(super::theme::section_title_color(window_active))
                    .child({
                        let title = if self.sidebar_volumes { "Volumes" } else { "Apps" };
                        self.host_text(HostText::SidebarTitle, title, cx)
                    })
                    .child(join),
            )
            .child(list)
            .when(!self.sidebar_volumes && self.session_icons.failures() > 0, |v| {
                v.child(self.artwork_error(theme, cx))
            })
            .child(
                div()
                    .h(ui(40.))
                    .debug_selector(|| "sidebar-footer".into())
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(ui(8.))
                    .pt(ui(7.))
                    .pb(ui(8.))
                    .border_t_1()
                    .border_color(gpui::rgba(0x0000001a))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .gap(ui(8.))
                            .debug_selector(|| "sidebar-tabs".into())
                            .child(account)
                            .child(volumes)
                            .child(sessions),
                    )
                    .child(div().flex_1().min_w_0())
                    .when(
                        self.channel != crate::release_channel::ReleaseChannel::Public,
                        |footer| {
                            footer.child(
                                self.host_text(HostText::ReleaseChannel, self.channel.label(), cx)
                                    .text_size(ui(10.))
                                    .text_color(gpui::rgba(0x00000099)),
                            )
                        },
                    )
                    .when(ap_known, |footer| footer.child(self.ap_status.clone())),
            )
    }

    fn sidebar_tab(
        &mut self,
        volumes: bool,
        muted: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let weak = cx.weak_entity();
        let control = self.navigation_button(
            if volumes { "sidebar-volumes" } else { "sidebar-sessions" }.into(),
            if volumes { "Volumes" } else { "Apps" },
            false,
            Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if !volumes && !this.prepare_browser_navigation(cx) {
                        return;
                    }
                    this.sidebar_volumes = volumes;
                    this.new_session = None;
                    if !volumes {
                        this.browser = None;
                    }
                    cx.notify();
                });
            }),
            None,
            cx,
        );
        control.update(cx, |control, cx| {
            control.set_sidebar_icon(
                controls::SidebarIcon::Glyph(if volumes {
                    "sidebar-volume"
                } else {
                    "app-browser-apps"
                }),
                self.sidebar_volumes == volumes,
                muted,
                cx,
            )
        });
        control
    }
}

fn display_label(name: &str) -> String {
    if name.len() <= 256 {
        return name.into();
    }
    let mut end = 253;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &name[..end])
}

pub(super) fn session_info_failed(session: &SessionRow) -> bool {
    use crate::engine::builds::BuildState;
    !matches!(
        session.build_state,
        BuildState::Checking | BuildState::Downloading | BuildState::Ready
    )
}

fn session_id_prefix<'a>(session: &'a SessionRow, sessions: &[SessionRow]) -> &'a str {
    let code = session.reference.as_str();
    let mut end = 6.min(code.len());
    while end < code.len()
        && sessions
            .iter()
            .any(|other| other.id != session.id && other.reference.starts_with(&code[..end]))
    {
        end += 1;
    }
    &code[..end]
}

fn session_label(session: &SessionRow, sessions: &[SessionRow]) -> String {
    let Some(name) = &session.app_name else {
        let state = if session_info_failed(session) { "App unavailable" } else { "Loading app" };
        return if session.reference.is_empty() {
            state.to_owned()
        } else {
            format!("{state} · {}", session.reference)
        };
    };
    if sessions.iter().filter(|other| other.app_name.as_ref() == Some(name)).count() <= 1 {
        return name.clone();
    }
    format!("{name} · {}", session_id_prefix(session, sessions))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {

    #[gpui::test]
    fn volume_status_and_mutations_use_uca_order_and_keep_navigation_identity(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let ids: Vec<_> = (1..=8).map(|byte| crate::store::Id::from_bytes([byte; 16])).collect();
        let incoming = vec![
            volume(ids[0], "Family"),
            volume(ids[1], "Archive UI"),
            volume(ids[2], "Archive"),
            volume(ids[3], "école"),
            volume(ids[4], "apple"),
            volume(ids[7], "Cafe\u{301}"),
            volume(ids[6], "Café"),
        ];
        let mut status = WorkerResponse::new(Ok(()));
        status.status_volumes = Some(incoming);
        host.update(cx, |host, _| host.sidebar_volumes = true);
        responses.try_send(status).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(
                host.volumes.iter().map(|row| row.id).collect::<Vec<_>>(),
                [ids[4], ids[2], ids[1], ids[6], ids[7], ids[3], ids[0]]
            );
        });
        let point = window
            .update(cx, |_, window, _| {
                let bounds = window
                    .accessibility_nodes_for_test()
                    .find_map(|(_, node)| {
                        (node.label() == Some("Archive")).then(|| node.bounds().unwrap())
                    })
                    .unwrap();
                gpui::point(
                    px(((bounds.x0 + bounds.x1) / 2.) as f32),
                    px(((bounds.y0 + bounds.y1) / 2.) as f32),
                )
            })
            .unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_mouse_down(point, gpui::MouseButton::Left, Default::default());
        visual.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, ids[2]);
        });
        visual.simulate_mouse_up(point, gpui::MouseButton::Left, Default::default());
        let control = host.read_with(cx, |host, _| {
            host.navigation_controls[&format!("volume-{}", ids[2])].clone()
        });
        test_support::press(window, "down", cx);
        host.read_with(cx, |host, cx| {
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, ids[1]);
        });
        while requests.try_recv().is_ok() {}
        let mut renamed = WorkerResponse::from_reply(Ok(Reply::Volume(volume(ids[2], "Zebra"))));
        renamed.volume_label = Some(ids[2]);
        responses.try_send(renamed).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert_eq!(host.volumes.last().unwrap().id, ids[2]);
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, ids[1]);
            assert_eq!(
                host.navigation_controls[&format!("volume-{}", ids[2])].entity_id(),
                control.entity_id()
            );
        });
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::Volume(volume(ids[5], "Banana")))))
            .unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert_eq!(
                host.volumes.iter().map(|row| row.id).collect::<Vec<_>>(),
                [ids[4], ids[1], ids[5], ids[6], ids[7], ids[3], ids[0], ids[2]]
            );
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, ids[5]);
        });
    }

    #[gpui::test]
    fn created_volume_names_the_returned_identity_and_failed_save_retains_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(text_editor::install_keybindings);
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let id = crate::store::Id::from_bytes([4; 16]);
        host.update(cx, |host, _| host.sidebar_volumes = true);
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::Volume(volume(id, "Untitled")))))
            .unwrap();
        cx.run_until_parked();
        while requests.try_recv().is_ok() {}
        host.read_with(cx, |host, cx| {
            assert_eq!(host.volume_rename.as_ref().unwrap().volume, id);
            assert!(host.sidebar_volumes);
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, id);
        });
        cx.simulate_input(window.into(), "Archive");
        test_support::press(window, "enter", cx);
        let Message::Request(request) = requests.try_recv().unwrap() else {
            panic!("rename request")
        };
        assert!(
            matches!(request.request, Request::LabelVolume { volume, label } if volume == id && label == "Archive")
        );
        test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err(), "a pending rename is admitted once");
        let mut failure = WorkerResponse::new(Err("Name could not be saved".into()));
        failure.volume_label = Some(id);
        responses.try_send(failure).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            let rename = host.volume_rename.as_ref().unwrap();
            assert!(!rename.pending);
            assert_eq!(rename.editor.read(cx).text(), "Archive");
        });
        test_support::press(window, "enter", cx);
        let Message::Request(request) = requests.try_recv().unwrap() else {
            panic!("retry request")
        };
        assert!(matches!(request.request, Request::LabelVolume { volume, .. } if volume == id));
        let mut success = WorkerResponse::from_reply(Ok(Reply::Volume(volume(id, "Archive"))));
        success.volume_label = Some(id);
        responses.try_send(success).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.volume_rename.is_none());
            assert_eq!(host.volumes[0].name, "Archive");
        });
    }

    #[gpui::test]
    fn duplicated_volume_selects_and_names_the_committed_copy(cx: &mut gpui::TestAppContext) {
        let (host, _, _requests, responses) = test_support::host_with_responses(cx);
        let source = volume(crate::store::Id::from_bytes([4; 16]), "Archive");
        let copy = volume(crate::store::Id::from_bytes([5; 16]), "Archive Copy");
        host.update(cx, |host, cx| {
            host.volumes = vec![source.clone()];
            assert!(host.select_volume(source.clone(), cx));
        });
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::Diagnostic(
                crate::engine::reply::DiagnosticReply::VolumeDuplicated {
                    source: source.id,
                    volume: copy.clone(),
                },
            ))))
            .unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume, copy);
            let rename = host.volume_rename.as_ref().unwrap();
            assert_eq!(rename.volume, copy.id);
            assert_eq!(rename.editor.read(cx).text(), "Archive Copy");
        });
    }

    #[gpui::test]
    fn delayed_volume_copy_preserves_an_unsubmitted_name(cx: &mut gpui::TestAppContext) {
        // A copy finishing while another volume is being named cannot replace its editor.
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let source = crate::store::Id::from_bytes([3; 16]);
        let current = crate::store::Id::from_bytes([4; 16]);
        let copy = volume(crate::store::Id::from_bytes([5; 16]), "Archive Copy");
        host.update(cx, |host, cx| {
            host.volumes = vec![volume(source, "Archive"), volume(current, "Notes")];
            assert!(host.select_volume(volume(current, "Notes"), cx));
            assert!(host.rename_volume(current, cx));
        });
        cx.run_until_parked();
        while requests.try_recv().is_ok() {}
        cx.simulate_input(window.into(), "Personal notes");
        let editor = host.read_with(cx, |host, cx| {
            let rename = host.volume_rename.as_ref().unwrap();
            assert!(!rename.pending);
            assert_eq!(rename.editor.read(cx).text(), "Personal notes");
            rename.editor.entity_id()
        });
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::Diagnostic(
                crate::engine::reply::DiagnosticReply::VolumeDuplicated {
                    source,
                    volume: copy.clone(),
                },
            ))))
            .unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            let rename = host.volume_rename.as_ref().unwrap();
            assert_eq!(rename.editor.entity_id(), editor);
            assert_eq!(rename.editor.read(cx).text(), "Personal notes");
            assert!(!rename.pending);
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, current);
            assert!(host.volumes.contains(&copy));
            assert!(host.navigation_controls.contains_key(&format!("volume-{}", copy.id)));
        });
        assert!(requests.try_recv().is_err(), "the retained draft remains unsubmitted");
    }

    #[gpui::test]
    fn delayed_volume_copy_preserves_session_navigation(cx: &mut gpui::TestAppContext) {
        // A completed copy remains available without taking over the active Sessions tab.
        let (host, _, _requests, responses) = test_support::host_with_responses(cx);
        let session = crate::identifier::encode(&[6; 32]);
        let copy = volume(crate::store::Id::from_bytes([5; 16]), "Archive Copy");
        host.update(cx, |host, cx| {
            host.sessions = vec![SessionRow {
                reference: String::new(),
                id: session.clone(),
                resolved_build_id: crate::identifier::encode(&[7; 32]),
                app_name: Some("Notes".into()),
                build_state: Default::default(),
            }];
            host.select_sidebar_session(session.clone(), cx);
        });
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::Diagnostic(
                crate::engine::reply::DiagnosticReply::VolumeDuplicated {
                    source: crate::store::Id::from_bytes([3; 16]),
                    volume: copy.clone(),
                },
            ))))
            .unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(!host.sidebar_volumes);
            assert_eq!(host.sidebar_session.as_deref(), Some(session.as_str()));
            assert!(host.browser.is_none());
            assert!(host.volume_rename.is_none());
            assert!(host.volumes.contains(&copy));
        });
    }

    #[gpui::test]
    fn inline_volume_name_commits_on_blur_and_escape_discards(cx: &mut gpui::TestAppContext) {
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
        cx.update(|cx| {
            gpui::AnyWindowHandle::from(window)
                .update(cx, |_, window, _| window.activate_window())
                .unwrap()
        });
        cx.run_until_parked();
        let id = crate::store::Id::from_bytes([4; 16]);
        host.update(cx, |host, cx| {
            host.volumes = vec![volume(id, "Notes")];
            assert!(host.select_volume(volume(id, "Notes"), cx));
            host.rename_volume(id, cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            gpui::AnyWindowHandle::from(window)
                .update(cx, |_, window, cx| {
                    assert!(window.is_window_active());
                    window.draw(cx)
                })
                .unwrap()
        })
        .clear();
        while requests.try_recv().is_ok() {}
        cx.simulate_input(window.into(), "Archive");
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    host.navigation_controls["account"].read(cx).focus(window);
                })
                .unwrap()
        });
        cx.update(|cx| {
            gpui::AnyWindowHandle::from(window).update(cx, |_, window, cx| window.draw(cx)).unwrap()
        })
        .clear();
        cx.run_until_parked();
        let Message::Request(request) = requests.try_recv().unwrap() else { panic!("blur rename") };
        assert!(
            matches!(request.request, Request::LabelVolume { volume, label } if volume == id && label == "Archive")
        );
        host.update(cx, |host, cx| {
            host.finish_volume_rename(id, true, cx);
            host.rename_volume(id, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(window.into(), "Discarded");
        test_support::press(window, "escape", cx);
        host.read_with(cx, |host, _| assert!(host.volume_rename.is_none()));
        assert!(requests.try_recv().is_err(), "Escape does not submit a name");
    }

    #[gpui::test]
    fn naming_an_unselected_volume_preserves_the_current_browser(cx: &mut gpui::TestAppContext) {
        // Context Rename edits only its target, leaving the selected volume and content owner.
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
        let selected = volume(crate::store::Id::from_bytes([4; 16]), "Archive");
        let target = volume(crate::store::Id::from_bytes([5; 16]), "Family");
        let browser = host.update(cx, |host, cx| {
            host.volumes = vec![selected.clone(), target.clone()];
            assert!(host.select_volume(selected.clone(), cx));
            host.browser.as_ref().unwrap().entity_id()
        });
        cx.run_until_parked();
        while requests.try_recv().is_ok() {}
        host.update(cx, |host, cx| assert!(host.rename_volume(target.id, cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser);
                    assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, selected.id);
                    assert_eq!(host.volume_rename.as_ref().unwrap().volume, target.id);
                    assert!(window.accessibility_nodes_for_test().any(|(_, node)| {
                        node.label() == Some("Archive") && node.is_selected() == Some(true)
                    }));
                })
                .unwrap();
        });
        assert!(requests.try_recv().is_err(), "opening Rename never reloads browser content");
        cx.simulate_input(window.into(), "Home");
        test_support::press(window, "enter", cx);
        let Message::Request(request) = requests.try_recv().unwrap() else {
            panic!("target rename")
        };
        assert!(matches!(request.request, Request::LabelVolume { volume, label }
            if volume == target.id && label == "Home"));
        host.read_with(cx, |host, cx| {
            assert_eq!(host.browser.as_ref().unwrap().entity_id(), browser);
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, selected.id);
        });
    }

    #[gpui::test]
    fn selected_volume_click_starts_inline_naming_after_one_second(cx: &mut gpui::TestAppContext) {
        let (host, window, requests) = test_support::host(cx);
        let id = crate::store::Id::from_bytes([4; 16]);
        host.update(cx, |host, cx| {
            host.volumes = vec![volume(id, "Archive")];
            host.sidebar_volumes = true;
            cx.notify();
        });
        cx.update(|cx| {
            gpui::AnyWindowHandle::from(window)
                .update(cx, |_, window, _| window.activate_window())
                .unwrap();
        });
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let position = gpui::point(px(20.), px(60.));
        visual.simulate_click(position, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, id);
            assert!(host.volume_rename.is_none(), "the first click only selects the volume");
        });
        while requests.try_recv().is_ok() {}
        visual.simulate_click(position, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(999));
        cx.run_until_parked();
        host.read_with(cx, |host, _| assert!(host.volume_rename.is_none()));
        cx.executor().advance_clock(std::time::Duration::from_millis(1));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    let rename = host.volume_rename.as_ref().expect("inline volume name");
                    assert_eq!(rename.volume, id);
                    assert_eq!(rename.editor.read(cx).text(), "Archive");
                    assert!(rename.focused(window, cx));
                })
                .unwrap();
        });
        assert!(requests.try_recv().is_err(), "opening the name field does not save it");
    }

    #[gpui::test]
    fn new_volume_button_admits_one_credited_creation(cx: &mut gpui::TestAppContext) {
        // Activate the rendered Volumes + control through the normal UI admission path.
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.sidebar_volumes = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    host.navigation_controls["join"].read(cx).focus(window);
                })
                .unwrap();
        });
        test_support::press(window, "enter", cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else {
            panic!("expected volume creation");
        };
        assert!(
            matches!(&queued.request, Request::CreateVolume { name, share_key_with_service: true } if name == "Untitled")
        );
        assert!(queued.credit.is_some());
        assert!(requests.try_recv().is_err());
        host.read_with(cx, |host, _| assert!(host.error.is_empty()));
    }

    #[gpui::test]
    fn sidebar_modes_keep_the_selected_volume(cx: &mut gpui::TestAppContext) {
        let (host, window, _requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.volumes = vec![volume(crate::store::Id::from_bytes([1; 16]), "Notes")];
            cx.notify();
        });
        cx.run_until_parked();
        for (id, volumes) in [("sidebar-volumes", true), ("sidebar-sessions", false)] {
            cx.update(|cx| {
                window
                    .update(cx, |host, window, cx| {
                        host.new_session = Some(cx.new(|_| {
                            new_session::Form::new(
                                new_session::Target::Session(crate::identifier::encode(
                                    &[0xab; 32],
                                )),
                                host.send.clone(),
                            )
                        }));
                        host.navigation_controls[id].read(cx).focus(window);
                    })
                    .unwrap()
            });
            test_support::press(window, "enter", cx);
            host.read_with(cx, |host, _| {
                assert_eq!(host.sidebar_volumes, volumes);
                assert!(host.new_session.is_none());
            });
        }
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn titlebar_allows_native_dragging_except_over_controls_and_content(
        cx: &mut gpui::TestAppContext,
    ) {
        // Check the rendered shell's geometry published to AppKit,
        // including the bottom of the 44-point custom titlebar.
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, _requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        host.update(cx, |host, _| {
            host.enrolled = Some(true);
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                unlocked: true,
                initialized: true,
            };
        });
        cx.update(|cx| Shell::show_sessions(&host, cx));
        let handle = host.read_with(cx, |host, _| host.sessions_window.unwrap());
        for y in [8., 32., 43.5] {
            for x in [100., 180., 260., 600., 860.] {
                assert!(
                    cx.window_can_drag_at(handle.into(), gpui::point(px(x), px(y))),
                    "header at ({x}, {y}) must allow native dragging",
                );
            }
        }
        for (x, y) in [(220., 20.), (1000., 20.), (100., 60.), (600., 60.), (600., 300.)] {
            assert!(
                !cx.window_can_drag_at(handle.into(), gpui::point(px(x), px(y))),
                "control/content at ({x}, {y}) must not drag the window",
            );
        }
        // Control exclusions survive resizing; content remains non-draggable.
        cx.simulate_window_resize(handle.into(), size(px(640.), px(400.)));
        cx.update(|cx| {
            gpui::AnyWindowHandle::from(handle).update(cx, |_, window, cx| window.draw(cx)).unwrap()
        })
        .clear();
        assert!(cx.window_can_drag_at(handle.into(), gpui::point(px(500.), px(32.))));
        assert!(!cx.window_can_drag_at(handle.into(), gpui::point(px(620.), px(32.))));
        assert!(!cx.window_can_drag_at(handle.into(), gpui::point(px(220.), px(20.))));
        assert!(!cx.window_can_drag_at(handle.into(), gpui::point(px(620.), px(380.))));

        // Volumes without a selection have no panel header, but retain the
        // same native titlebar across the blank area and its divider.
        host.update(cx, |host, cx| {
            host.sidebar_volumes = true;
            cx.notify();
        });
        cx.run_until_parked();
        for dimensions in [size(px(640.), px(400.)), size(px(1024.), px(560.))] {
            cx.simulate_window_resize(handle.into(), dimensions);
            cx.update(|cx| {
                gpui::AnyWindowHandle::from(handle)
                    .update(cx, |_, window, cx| window.draw(cx))
                    .unwrap()
            })
            .clear();
            for y in [0.5, 22., 43.5] {
                for x in [100., 180., 240.5, 260., f32::from(dimensions.width) - 10.] {
                    assert!(
                        cx.window_can_drag_at(handle.into(), gpui::point(px(x), px(y))),
                        "empty Volumes titlebar at ({x}, {y}) must allow native dragging",
                    );
                }
            }
            for (x, y) in [(220., 20.), (260., 44.5), (500., 300.)] {
                assert!(
                    !cx.window_can_drag_at(handle.into(), gpui::point(px(x), px(y))),
                    "empty Volumes control/content at ({x}, {y}) must not drag",
                );
            }
        }

        // Check the complete band and actual button exclusions at UI zooms,
        // both before selection and with the browser's own column headers.
        cx.update(crate::ui::desktop::zoom::install);
        for selected in [false, true] {
            host.update(cx, |host, cx| {
                if selected {
                    let volume = volume(crate::store::Id::from_bytes([1; 16]), "Archive");
                    host.volumes = vec![volume.clone()];
                    host.browser =
                        Some(cx.new(|cx| browser::Browser::new(volume, host.send.clone(), cx)));
                }
                cx.notify();
            });
            for units in [5_000, 10_000, 20_000] {
                let scale = units as f32 / 10_000.;
                cx.update(|cx| crate::ui::desktop::zoom::set_scale(units, cx).unwrap());
                cx.run_until_parked();
                let dimensions = size(px(1024. * scale.max(1.)), px(560. * scale.max(1.)));
                cx.simulate_window_resize(handle.into(), dimensions);
                cx.update(|cx| {
                    gpui::AnyWindowHandle::from(handle)
                        .update(cx, |_, window, cx| window.draw(cx))
                        .unwrap()
                })
                .clear();
                let height = (44. * scale).max(44.);
                let buttons = handle
                    .update(cx, |_, window, _| {
                        window
                            .accessibility_nodes_for_test()
                            .filter(|(_, node)| {
                                matches!(
                                    node.role(),
                                    gpui::accesskit::Role::Button | gpui::accesskit::Role::ComboBox
                                )
                            })
                            .filter_map(|(_, node)| node.bounds())
                            .filter(|bounds| bounds.y0 < f64::from(height))
                            .collect::<Vec<_>>()
                    })
                    .unwrap();
                assert!(!buttons.is_empty(), "rendered header controls are exercised");
                for y in [0.5, height / 2., height - 0.5] {
                    for x in (88 * scale.max(1.) as usize..f32::from(dimensions.width) as usize)
                        .step_by(17)
                    {
                        let point = gpui::point(px(x as f32), px(y));
                        // GPUI's native hitboxes include their right/bottom edges.
                        if buttons.iter().any(|bounds| {
                            x as f64 >= bounds.x0
                                && x as f64 <= bounds.x1
                                && f64::from(y) >= bounds.y0
                                && f64::from(y) <= bounds.y1
                        }) {
                            continue;
                        }
                        assert!(
                            cx.window_can_drag_at(handle.into(), point),
                            "titlebar at {point:?}, zoom {scale}, selected {selected}, buttons {buttons:?}",
                        );
                    }
                }
                for bounds in buttons {
                    let point = gpui::point(
                        px(((bounds.x0 + bounds.x1) / 2.) as f32),
                        px(((bounds.y0 + bounds.y1) / 2.) as f32),
                    );
                    if point.x < dimensions.width {
                        assert!(!cx.window_can_drag_at(handle.into(), point));
                    }
                }
                assert!(!cx.window_can_drag_at(
                    handle.into(),
                    gpui::point(dimensions.width - px(10.), px(height + 0.5)),
                ));
            }
        }
    }

    fn volume(id: crate::store::Id, name: &str) -> crate::store::Volume {
        crate::store::Volume {
            id,
            name: name.into(),
            owner: crate::store::Id::from_bytes([9; 16]),
            personal: true,
            share_key_with_service: false,
        }
    }
    use super::*;
    #[test]
    fn volume_display_labels_are_bounded_without_splitting_unicode() {
        assert_eq!(display_label("Notes"), "Notes");
        assert_eq!(display_label(&"a".repeat(256)), "a".repeat(256));
        let label = display_label(&"🦀".repeat(100));
        assert!(label.len() <= 256);
        assert_eq!(label, format!("{}…", "🦀".repeat(63)));
    }
    #[gpui::test]
    fn loading_rows_keep_identity_focus_and_full_accessible_session_id(
        cx: &mut gpui::TestAppContext,
    ) {
        // Resolve metadata on the same row while checking loading/error labels and selection.
        use crate::engine::builds::BuildState;
        let (host, window, _) = test_support::host(cx);
        let reference = crate::store::SessionReference::new([0x37; 16]).unwrap();
        let id = reference.internal_id().to_owned();
        let code = reference.code();
        host.update(cx, |host, cx| {
            host.sidebar_volumes = false;
            host.sessions = vec![SessionRow {
                reference: code.clone(),
                id: id.clone(),
                resolved_build_id: crate::identifier::encode(&[0x42; 32]),
                app_name: None,
                build_state: BuildState::Downloading,
            }];
            host.sidebar_session = Some(id.clone());
            host.sidebar_view(true, cx);
            cx.notify();
        });
        let control = host.read_with(cx, |host, _| host.navigation_controls[&id].clone());
        cx.update(|cx| window.update(cx, |_, window, cx| control.read(cx).focus(window)).unwrap());
        for state in [
            BuildState::Downloading,
            BuildState::DownloadFailed { error: "Connection lost".into() },
            BuildState::Ready,
        ] {
            host.update(cx, |host, cx| {
                host.sessions[0].app_name =
                    matches!(state, BuildState::Ready).then(|| "Notes".into());
                host.sessions[0].build_state = state.clone();
                host.sidebar_view(true, cx);
                assert_eq!(host.navigation_controls[&id], control);
                assert_eq!(host.sidebar_session.as_deref(), Some(id.as_str()));
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|cx| {
                window
                    .update(cx, |_, window, cx| {
                        assert!(control.read(cx).focused(window));
                        let label = if matches!(state, BuildState::Ready) {
                            "Notes".into()
                        } else {
                            format!(
                                "{} · {code}",
                                if matches!(state, BuildState::Downloading) {
                                    "Loading app"
                                } else {
                                    "App unavailable"
                                }
                            )
                        };
                        let (_, node) = window
                            .accessibility_nodes_for_test()
                            .find(|(_, node)| node.label() == Some(label.as_str()))
                            .expect("row keeps its full accessible identity");
                        if matches!(state, BuildState::DownloadFailed { .. }) {
                            assert!(node.description().unwrap().contains("Connection lost"));
                        }
                    })
                    .unwrap()
            });
        }
    }

    #[gpui::test]
    fn session_status_moves_identity_and_updates_existing_row(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let (send, _requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        let reference = crate::store::SessionReference::new([0xab; 16]).unwrap();
        let session_id = reference.internal_id().to_owned();
        let mut control = None;
        for name in ["Before", "Updated app"] {
            let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
                panic!("status reply")
            };
            status.enrolled = true;
            let id = crate::store::Id::from_bytes([1; 16]);
            status.sessions = vec![crate::store::Session {
                id: session_id.clone(),
                resolved_build_id: crate::identifier::encode(&[0xcd; 32]),
                selector: crate::store::BuildSelector::Exact {
                    build_id: crate::identifier::encode(&[0xcd; 32]),
                },
                root_volume: id,
                shared_volume: None,
                app: crate::package::AppIdentity { publisher: id, app_id: "test".into() },

                creator: id,
                update_authority: crate::store::UpdateAuthority::Creator,
            }];
            status.session_references.insert(session_id.clone(), reference.code());
            let allocation = status.sessions[0].id.as_ptr() as usize;
            let mut response = WorkerResponse::from_reply(Ok(Reply::Status(status)));
            response.session_rows.as_mut().unwrap()[0].app_name = Some(name.into());
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.update(cx, |host, cx| {
                assert_eq!(host.sessions[0].id.as_ptr() as usize, allocation);
                assert_eq!(host.sessions[0].app_name.as_deref(), Some(name));
                let sessions = host.sessions.clone();
                host.retain_navigation_controls(&sessions, cx);
                host.session_rows(&sessions, None, cx);
                let current = host.navigation_controls[&session_id].entity_id();
                if let Some(control) = control {
                    assert_eq!(current, control);
                }
                control = Some(current);
            });
        }
    }

    #[gpui::test]
    fn browser_failure_stays_with_its_component_and_engine_loss_still_requires_restart(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Browser failure", false).unwrap();
        let entry = store
            .create_database(
                &crate::store::Access::owner(store.account().id),
                volume.id,
                "file",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let address =
            crate::store::volume_index::EntryAddress { volume: volume.id, entry: entry.id };
        let request = Request::EntryRevisions { session: None, address, after: None, limit: 64 };
        let (send, _requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        host.update(cx, |host, cx| {
            let browser = cx.new(|cx| browser::Browser::new(volume.clone(), host.send.clone(), cx));
            browser.update(cx, |browser, cx| browser.open_database(entry.id, cx));
            host.browser = Some(browser);
            host.joining = true;
            host.error = "unrelated shell error".into();
        });
        let mut response = WorkerResponse::from_reply(Err("read denied".into()));
        response.browser_failure = browser::Failure::for_request(&request);
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        host.update(cx, |host, _| {
            assert_eq!(host.error, "unrelated shell error");
            assert!(host.joining);
        });
        let mut response = WorkerResponse::from_reply(Err("engine lost".into()));
        response.browser_failure = browser::Failure::for_request(&request);
        response.engine_lost = true;
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        host.update(cx, |host, _| {
            assert!(host.engine_recovering);
            assert_eq!(host.error, "unrelated shell error");
        });
    }

    #[gpui::test]
    fn typed_volume_status_moves_storage_and_applies_rename(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let (send, _requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        let id = crate::store::Id::from_bytes([1; 16]);
        let mut identity = None;
        for name in ["Before", "Café 🌱"] {
            let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
                panic!("status reply")
            };
            status.enrolled = true;
            status.volumes = vec![volume(id, name)];
            let allocation = status.volumes.as_ptr() as usize;
            let response = WorkerResponse::from_reply(Ok(Reply::Status(status)));
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.update(cx, |host, cx| {
                assert_eq!(host.volumes.as_ptr() as usize, allocation);
                assert_eq!(host.volumes[0].name, name);
                host.sidebar_volumes = true;
                host.sidebar_view(true, cx);
                let current = host.navigation_controls[&format!("volume-{}", id)].entity_id();
                if let Some(identity) = identity {
                    assert_eq!(current, identity);
                }
                identity = Some(current);
            });
        }
    }

    #[gpui::test]
    fn volume_navigation_identity_survives_rename_and_reorder_but_not_removal(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        let first = crate::store::Id::from_bytes([1; 16]);
        let second = crate::store::Id::from_bytes([2; 16]);
        host.update(cx, |host, cx| {
            host.enrolled = Some(true);
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                unlocked: true,
                initialized: true,
            };
            host.volumes = vec![volume(first, "Before"), volume(second, "Other")];
            host.sidebar_volumes = true;
            host.sidebar_view(true, cx);
            let identity = host.navigation_controls[&format!("volume-{}", first)].entity_id();
            let other = host.navigation_controls[&format!("volume-{}", second)].entity_id();
            host.volumes = vec![volume(second, "Other"), volume(first, "Café 🌱")];
            host.sidebar_volumes = true;
            host.sidebar_view(true, cx);
            assert_eq!(
                host.navigation_controls[&format!("volume-{}", first)].entity_id(),
                identity
            );
            assert_eq!(host.navigation_controls[&format!("volume-{}", second)].entity_id(), other);
            host.volumes = vec![volume(first, "Café 🌱")];
            host.sidebar_volumes = true;
            host.sidebar_view(true, cx);
            assert!(!host.navigation_controls.contains_key(&format!("volume-{second}")));
            host.volumes = vec![volume(second, "Other")];
            host.sidebar_volumes = true;
            host.sidebar_view(true, cx);
            assert_ne!(host.navigation_controls[&format!("volume-{}", second)].entity_id(), other);
            assert!(!host.navigation_controls.contains_key(&format!("volume-{first}")));
            host.volumes = vec![volume(first, "Overflow"); 257];
            host.sidebar_volumes = true;
            host.sidebar_view(true, cx);
            assert!(host.navigation_controls.keys().all(|key| !key.starts_with("volume-")));
        });
        assert!(
            requests.try_recv().is_err(),
            "read-only text must not dispatch runtime operations"
        );
    }
    #[gpui::test]
    fn long_volume_rows_fit_sidebar_and_keep_full_accessible_name(cx: &mut gpui::TestAppContext) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        let name = format!("Volume {}abc", "é".repeat(123));
        let id = crate::store::Id::from_bytes([1; 16]);
        host.update(cx, |host, _| {
            host.enrolled = Some(true);
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                unlocked: true,
                initialized: true,
            };
            host.volumes = vec![volume(id, &name)];
            host.sidebar_volumes = true;
        });
        cx.update(|cx| Shell::show_sessions(&host, cx));
        cx.run_until_parked();
        let handle = host.read_with(cx, |host, _| host.sessions_window.unwrap());
        cx.update(|cx| {
            handle
                .update(cx, |_, window, _| {
                    let node = window
                        .accessibility_nodes_for_test()
                        .map(|(_, node)| node)
                        .find(|node| node.label() == Some(name.as_str()))
                        .expect("full accessible volume name");
                    let bounds = node.bounds().unwrap();
                    assert!(
                        bounds.x0 >= 4. && bounds.x1 <= 236.,
                        "row must fit sidebar padding: {bounds:?}"
                    );
                    assert!((bounds.height() - 32.).abs() < 1.);
                    assert!(node.supports_action(gpui::accesskit::Action::Click));
                })
                .unwrap()
        });
        assert!(requests.try_recv().is_err());
    }
    #[gpui::test]
    fn sidebar_uses_shared_scrollbars_for_both_lists(cx: &mut gpui::TestAppContext) {
        // Exercise the shared rails, virtual list offsets and macOS elasticity together.
        cx.set_auto_hide_scrollbars(false);
        let (host, window, _requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.sessions = (0..64)
                .map(|i| SessionRow {
                    reference: String::new(),
                    id: crate::identifier::encode(&{
                        let mut bytes = [0; 32];
                        bytes[16..].copy_from_slice(&(i as u128).to_be_bytes());
                        bytes
                    }),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some(format!("App {i}")),
                    resolved_build_id: String::new(),
                })
                .collect();
            host.volumes = (1..=64)
                .map(|i| volume(crate::store::Id::from_bytes([i; 16]), &format!("Volume {i}")))
                .collect();
            cx.notify();
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(800.), px(400.)));
        for volumes in [false, true] {
            let (handle, list) = host.update(&mut visual, |host, cx| {
                host.sidebar_volumes = volumes;
                cx.notify();
                if volumes {
                    (host.volume_scrollbars.read(cx).handle(), host.volume_scroll.clone())
                } else {
                    (host.session_scrollbars.read(cx).handle(), host.session_scroll.clone())
                }
            });
            visual.run_until_parked();
            assert!(handle.max_offset().height > px(0.));
            let bounds = handle.bounds();
            assert_eq!(bounds.right(), px(240.));
            let rail = gpui::point(bounds.right() - px(8.), bounds.center().y);
            visual.simulate_mouse_move(rail, None, Default::default());
            visual.simulate_click(rail, Default::default());
            assert!(handle.offset().y < px(0.), "the shared rail must scroll the list");
            assert_eq!(list.0.borrow().base_handle.offset(), handle.offset());
            window
                .update(&mut visual, |_, window, _| {
                    handle.set_offset(gpui::Point::default());
                    window.refresh();
                })
                .unwrap();
            visual.run_until_parked();
            #[cfg(target_os = "macos")]
            {
                let row_top = |visual: &mut gpui::VisualTestContext, label: &str| {
                    window
                        .update(visual, |_, window, _| {
                            window
                                .accessibility_nodes_for_test()
                                .map(|(_, node)| node)
                                .find(|node| node.label() == Some(label))
                                .expect("visible edge row")
                                .bounds()
                                .expect("row bounds")
                                .y0
                        })
                        .unwrap()
                };
                for bottom in [false, true] {
                    let edge = if bottom { -handle.max_offset().height } else { px(0.) };
                    window
                        .update(&mut visual, |_, window, _| {
                            handle.set_offset(gpui::point(px(0.), edge));
                            window.refresh();
                        })
                        .unwrap();
                    visual.run_until_parked();
                    let label = match (volumes, bottom) {
                        (false, false) => "App 0",
                        (false, true) => "App 63",
                        (true, false) => "Volume 1",
                        (true, true) => "Volume 64",
                    };
                    let before = row_top(&mut visual, label);
                    visual.simulate_event(gpui::ScrollWheelEvent {
                        position: bounds.center(),
                        delta: gpui::ScrollDelta::Pixels(gpui::point(
                            px(0.),
                            px(if bottom { -80. } else { 80. }),
                        )),
                        is_gesture: true,
                        touch_phase: gpui::TouchPhase::Started,
                        ..Default::default()
                    });
                    let stretch = f32::from(handle.elastic_offset().y) as f64;
                    assert!(if bottom { stretch < 0. } else { stretch > 0. });
                    assert_eq!(handle.offset().y, edge);
                    let movement = row_top(&mut visual, label) - before;
                    assert!(
                        (movement - stretch).abs() <= 1.,
                        "content must follow elastic displacement at bottom={bottom}: movement={movement}, stretch={stretch}"
                    );
                    handle.set_offset(gpui::point(px(0.), edge));
                }
            }
        }
    }
    #[gpui::test]
    fn arrow_navigation_reaches_sessions_outside_the_visible_rows(cx: &mut gpui::TestAppContext) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        host.update(cx, |host, _| {
            host.enrolled = Some(true);
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                unlocked: true,
                initialized: true,
            };
            host.sessions = (0..128)
                .map(|i| SessionRow {
                    reference: String::new(),
                    id: crate::identifier::encode(&{
                        let mut bytes = [0; 32];
                        bytes[16..].copy_from_slice(&(i as u128).to_be_bytes());
                        bytes
                    }),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some(format!("App {i}")),
                    resolved_build_id: String::new(),
                })
                .collect();
        });
        cx.update(|cx| Shell::show_sessions(&host, cx));
        let handle = host.read_with(cx, |host, _| host.sessions_window.unwrap());
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        visual.simulate_click(gpui::point(px(20.), px(60.)), Default::default());
        for _ in 0..127 {
            cx.simulate_keystrokes(handle.into(), "down");
        }
        cx.update(|cx| {
            handle
                .update(cx, |host, window, cx| {
                    assert!(
                        host.navigation_controls[&crate::identifier::encode(&{
                            let mut bytes = [0; 32];
                            bytes[31] = 127;
                            bytes
                        })]
                            .read(cx)
                            .focused(window)
                    );
                    assert_eq!(
                        host.sidebar_session,
                        Some(crate::identifier::encode(&{
                            let mut bytes = [0; 32];
                            bytes[31] = 127;
                            bytes
                        }))
                    );
                })
                .unwrap()
        });
        for _ in 0..127 {
            cx.simulate_keystrokes(handle.into(), "up");
        }
        cx.update(|cx| {
            handle
                .update(cx, |host, window, cx| {
                    assert!(
                        host.navigation_controls[&crate::identifier::encode(&{
                            let mut bytes = [0; 32];
                            bytes[31] = 0;
                            bytes
                        })]
                            .read(cx)
                            .focused(window)
                    );
                    assert_eq!(
                        host.sidebar_session,
                        Some(crate::identifier::encode(&{
                            let mut bytes = [0; 32];
                            bytes[31] = 0;
                            bytes
                        }))
                    );
                })
                .unwrap()
        });
        assert!(requests.try_recv().is_err(), "focus navigation does not launch a session");
    }
    #[gpui::test]
    fn pointer_selection_enables_volume_arrow_navigation(cx: &mut gpui::TestAppContext) {
        let (host, window, _requests, responses) = test_support::host_with_responses(cx);
        let ids = [1, 2, 3].map(|byte| crate::store::Id::from_bytes([byte; 16]));
        let scoped_id = crate::store::Id::from_bytes([4; 16]);
        let mut status = WorkerResponse::new(Ok(()));
        status.status_volumes = Some(ids.iter().map(|id| volume(*id, "Volume")).collect());
        status.status_scoped_volumes = vec![volume(scoped_id, "Shared")];
        host.update(cx, |host, _| host.sidebar_volumes = true);
        responses.try_send(status).unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(gpui::point(px(20.), px(60.)), Default::default());
        for (keys, expected) in [("down", 1), ("down down", 2), ("up up up", 0)] {
            visual.simulate_keystrokes(keys);
            window
                .update(cx, |host, window, cx| {
                    assert_eq!(host.browser.as_ref().unwrap().read(cx).volume.id, ids[expected]);
                    assert!(!host.navigation_controls.contains_key(&format!("volume-{scoped_id}")));
                    assert!(host.catalog_volume(scoped_id).is_some());
                    let row =
                        host.navigation_controls[&format!("volume-{}", ids[expected])].read(cx);
                    assert!(row.focused(window));
                    assert!(!row.keyboard_focus_visible(window));
                })
                .unwrap();
        }
    }
    #[test]
    fn repeated_apps_keep_distinct_labels_even_when_short_prefixes_collide() {
        let sessions = vec![
            SessionRow {
                reference: "12345678a11111111111111".into(),
                id: format!("12345678{}", "a".repeat(56)),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("TODO".into()),
                resolved_build_id: String::new(),
            },
            SessionRow {
                reference: "12345678b11111111111111".into(),
                id: format!("12345678{}", "b".repeat(56)),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("TODO".into()),
                resolved_build_id: String::new(),
            },
            SessionRow {
                reference: String::new(),
                id: crate::identifier::encode(&[0x33; 32]),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("Controls".into()),
                resolved_build_id: String::new(),
            },
        ];
        assert_eq!(session_label(&sessions[0], &sessions), "TODO · 12345678a");
        assert_eq!(session_label(&sessions[1], &sessions), "TODO · 12345678b");
        assert_eq!(session_label(&sessions[2], &sessions), "Controls");
    }
    #[gpui::test]
    fn retired_session_open_releases_only_its_marker_and_allows_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        // Deliver a retired launch after a newer session is pending, then explicitly retry it.
        let (send, requests, errors) = worker_queue::channel();
        let gate = send.gate();
        let (responses, receive) = async_channel::bounded(1);
        let shell =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        shell.update(cx, |shell, cx| {
            shell.enrolled = Some(true);
            shell.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                unlocked: true,
                initialized: true,
            };
            shell.open_retained_session("retired".into(), cx);
        });
        let Message::Request(queued) = requests.try_recv().unwrap() else {
            panic!("expected the admitted session open");
        };
        gate.pause();
        gate.resume();
        assert!(!gate.accepts(queued.epoch));
        shell.update(cx, |shell, cx| shell.open_retained_session("newer".into(), cx));
        let newer = requests.try_recv().unwrap();
        responses.try_send(worker::retired_response(queued)).unwrap();
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert!(!shell.pending_windows.contains_key("retired"));
            assert!(shell.pending_windows.contains_key("newer"));
            assert_eq!(shell.opening.as_deref(), Some("newer"));
            assert!(shell.joining, "an older failure must not finish the newer launch");
        });
        assert!(requests.try_recv().is_err(), "retirement must not replay the launch");
        shell.update(cx, |shell, cx| {
            shell.open_retained_session("retired".into(), cx);
            shell.open_retained_session("retired".into(), cx);
        });
        let Message::Request(retry) = requests.try_recv().unwrap() else {
            panic!("expected an explicit retry");
        };
        assert!(gate.accepts(retry.epoch));
        assert!(
            matches!(&retry.request, Request::PrepareSession { session } if session == "retired")
        );
        assert!(requests.try_recv().is_err(), "a pending retry keeps its one admission");
        drop(newer);
    }
    #[gpui::test]
    fn old_session_response_preserves_reopened_admission(cx: &mut gpui::TestAppContext) {
        // Navigate away and reopen the same session before its old response arrives,
        // within one epoch and across recovery, with errors and success. Use rendered controls.
        let (shell, window, requests) = test_support::host(cx);
        let (responses, receive) = async_channel::bounded(1);
        shell.update(cx, |shell, cx| Shell::watch_responses(shell.send.clone(), receive, cx));
        let gate = shell.read_with(cx, |shell, _| shell.send.gate());
        shell.update(cx, |shell, cx| {
            shell.sessions = vec![SessionRow {
                reference: String::new(),
                id: "same-session".into(),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("TODO".into()),
                resolved_build_id: String::new(),
            }];
            cx.notify();
        });
        cx.run_until_parked();
        for (recovering, successful) in [(false, false), (true, false), (false, true)] {
            let activate = |id: &str, cx: &mut gpui::TestAppContext| {
                cx.update(|cx| {
                    window
                        .update(cx, |shell, window, cx| {
                            shell.navigation_controls[id].read(cx).focus(window);
                        })
                        .unwrap();
                });
                test_support::press(window, "enter", cx);
            };
            activate("same-session", cx);
            let Message::Request(old) = requests.try_recv().unwrap() else {
                panic!("expected original session admission");
            };
            if recovering {
                gate.pause();
                gate.resume();
                assert!(!gate.accepts(old.epoch));
            }
            activate("join", cx);
            activate("same-session", cx);
            let Message::Request(newer) = requests.try_recv().unwrap() else {
                panic!("expected reopened session admission");
            };
            assert!(gate.accepts(newer.epoch));
            let admission = newer.credit.as_ref().unwrap().admission();
            assert_ne!(old.credit.as_ref().unwrap().admission(), admission);
            let response = if recovering {
                worker::retired_response(old)
            } else {
                let mut response = if successful {
                    WorkerResponse::from_reply(Ok(Reply::SessionPrepared(
                        crate::engine::reply::PreparedSession {
                            session: serde_json::from_value(serde_json::json!({
                                "id":"same-session",
                                "selector":{"kind":"exact","build_id":crate::identifier::encode(&[0xcd; 32])},
                                "resolved_build_id":crate::identifier::encode(&[0xcd; 32]),
                                "root_volume":crate::identifier::encode(&[0x01; 16]),

                                "app":{"publisher":crate::identifier::encode(&[0x03; 16]),"app_id":"test"}, "creator":crate::identifier::encode(&[0x03; 16]), "update_authority":"creator"
                            }))
                            .unwrap(),
                            invitation: "session invitation".into(),
                            availability: "Local".into(),
                            update: None,
                        },
                    )))
                } else {
                    WorkerResponse::new(Err("Preparation failed".into()))
                };
                response.session = Some("same-session".into());
                response.credit = old.credit;
                response
            };
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            shell.read_with(cx, |shell, _| {
                assert_eq!(shell.pending_windows.get("same-session"), Some(&Some(admission)));
                assert!(shell.joining, "the reopened admission still owns progress");
                assert!(shell.error.is_empty(), "an old failure must not replace newer progress");
                assert!(shell.selected.is_none(), "an old success must not finish the reopen");
            });
            activate("same-session", cx);
            assert!(requests.try_recv().is_err(), "sidebar activation must keep one admission");
            gate.pause();
            responses.try_send(worker::retired_response(newer)).unwrap();
            cx.run_until_parked();
            shell.read_with(cx, |shell, _| {
                assert!(!shell.pending_windows.contains_key("same-session"));
                assert!(!shell.joining);
            });
            gate.resume();
        }
    }
    #[gpui::test]
    fn selecting_a_pending_session_does_not_duplicate_its_launch(cx: &mut gpui::TestAppContext) {
        let (send, requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let shell =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        shell.update(cx, |shell, cx| {
            shell.open_retained_session("a".into(), cx);
            shell.open_retained_session("a".into(), cx);
            assert_eq!(shell.opening.as_deref(), Some("a"));
            assert_eq!(shell.pending_windows.len(), 1);
        });
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::PrepareSession { session }, ..
        }) if session == "a"));
        assert!(requests.try_recv().is_err());
    }
}
