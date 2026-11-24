//! Main window layout, navigation, and shared UI controls.
use super::*;
use crate::ui::desktop::zoom::length as ui;
use playbit_api::ui::{Theme, ThemeColor as Role};

pub(super) struct Shell {
    pub(super) sign_out_prompt: bool,
    pub(super) sidebar_session: Option<String>,
    pub(super) sidebar_volumes: bool,
    pub(super) account_id: String,
    pub(super) account_name_open: bool,
    pub(super) account_name_focus: bool,
    pub(super) visual_bell: Option<feedback::Bell>,
    pub(super) update_notice: Option<(String, crate::engine::UpdateStatus)>,
    pub(super) delete_volume_prompt: bool,
    pub(super) volume_rename: Option<sidebar_view::VolumeRename>,
    pub(super) pending_volume: Option<volume_creation::Pending>,
    pub(super) signing_out: bool,
    pub(super) sign_out_error: Option<String>,
    pub(super) sign_in: Option<Entity<crate::ui::desktop::sign_in::SignIn>>,
    pub(super) sign_in_opening: bool,
    pub(super) channel_switching: bool,
    pub(super) channel: crate::release_channel::ReleaseChannel,
    pub(super) focus: gpui::FocusHandle,
    pub(super) focus_window: bool,
    pub(super) send: RequestSender,
    pub(super) enrolled: Option<bool>,
    pub(super) selected: Option<crate::engine::reply::PreparedSession>,
    pub(super) sessions_window: Option<gpui::WindowHandle<Shell>>,
    // Consumed once the first account or Sessions window is ready.
    pub(super) crash_report_profile: Option<std::path::PathBuf>,
    pub(super) crash_reports_pending: bool,
    pub(super) sending_crash_reports: Entity<bool>,
    pub(super) initial_link: bool,
    pub(super) pending_database: Option<crate::store::DatabaseReference>,
    pub(super) url_generation: u64,
    // An intent awaiting sign-in has no admission yet. Admitted work owns its marker.
    pub(super) pending_windows: std::collections::BTreeMap<String, Option<u64>>,
    pub(super) error: String,
    pub(super) launch_failures: BTreeMap<guest_app::LaunchReceipt, String>,
    pub(super) launch_forms: BTreeMap<gpui::EntityId, Entity<new_session::Form>>,
    pub(super) launches: BTreeMap<String, launch_progress::Progress>,
    pub(super) startup: Vec<crate::engine::session_instance::StartupProgress>,
    pub(super) engine_recovering: bool,
    pub(super) recovery_notice: Option<gpui::Task<()>>,
    pub(super) alerted_error: String,
    pub(super) startup_prompt: bool,
    pub(super) invitation: Option<crate::store::InvitationPreview>,
    pub(super) invitation_link: Option<crate::store::SessionReference>,
    pub(super) invitation_resume: Option<u64>,
    pub(super) saved_invitations: Vec<crate::store::InvitationPreview>,
    pub(super) volumes: Vec<crate::store::Volume>,
    pub(super) scoped_volumes: Vec<crate::store::Volume>,
    pub(super) first_account_update: bool,
    pub(super) account: crate::store::Account,
    pub(super) account_profile: AccountProfile,
    pub(super) account_key_backup: crate::store::AccountKeyBackup,
    pub(super) sessions: Vec<SessionRow>,
    pub(super) applications: application_browser::State,
    pub(super) join_status: InvitationState,
    pub(super) invitation_status: Option<InvitationStatus>,
    pub(super) restore_pending_invitation: bool,
    pub(super) joining: bool,
    pub(super) opening: Option<String>,
    pub(super) name_editor: Option<Entity<text_editor::TextEditor>>,
    pub(super) recovery: Option<RecoveryStatus>,
    pub(super) recovery_busy: Option<worker_recovery::Action>,
    pub(super) personal: Option<personal::Snapshot>,
    pub(super) new_session: Option<Entity<new_session::Form>>,
    pub(super) app_preparation: new_session::Preparation,
    pub(super) personal_grants: BTreeMap<crate::store::Id, bool>,
    pub(super) personal_controls: BTreeMap<(crate::store::Id, bool), Entity<controls::Control>>,
    pub(super) personal_save: Option<personal::Save>,
    pub(super) last_transfer: TransferStatus,
    pub(super) ap_status: Entity<ap_status::Indicator>,
    pub(super) navigation_controls: BTreeMap<String, Entity<controls::Control>>,
    pub(super) browser: Option<Entity<browser::Browser>>,
    pub(super) browser_notice: Option<gpui::Subscription>,
    pub(super) _draft_observer: gpui::Subscription,
    pub(super) draft_retirement: Option<draft_lifecycle::Retirement>,
    pub(super) session_icons: session_icons::Icons,
    pub(super) session_scroll: gpui::UniformListScrollHandle,
    pub(super) volume_scroll: gpui::UniformListScrollHandle,
    pub(super) session_scrollbars: Entity<crate::ui::desktop::scrollbar::Scrollbars>,
    pub(super) volume_scrollbars: Entity<crate::ui::desktop::scrollbar::Scrollbars>,
    pub(super) host_controls: BTreeMap<&'static str, Entity<controls::Control>>,
    pub(super) rendered_host_controls: Vec<&'static str>,
    pub(super) host_text_ids: [Option<Entity<()>>; host_text::COUNT],
    pub(super) rendered_host_text: u64,
    pub(super) logs: Entity<LogView>,
    pub(super) log_watch: Option<log::Watch>,
    pub(super) log_session: Option<(String, Option<crate::store::Id>)>,
    pub(super) log_generation: u64,
    pub(super) session_action_busy: bool,
    pub(super) recent_sessions: Vec<String>,
    pub(super) join_dialog: Option<Entity<text_editor::TextEditor>>,
    pub(super) join_preview: Option<u64>,
    pub(super) session_dialog_id: Option<gpui::EntityId>,
    pub(super) welcome_scrollbars: Entity<crate::ui::desktop::scrollbar::Scrollbars>,
    pub(super) making_project: bool,
}

fn sessions_min_size(scale: f32) -> gpui::Size<gpui::Pixels> {
    size(px(500. * scale.max(1.)), px(300. * scale.max(1.)))
}

impl Shell {
    pub(super) fn launch(
        worker: (
            RequestSender,
            async_channel::Receiver<WorkerResponse>,
            async_channel::Receiver<&'static str>,
        ),
        urls: async_channel::Receiver<Result<String, &'static str>>,
        open_initial_window: bool,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::launch_with_focus(worker, urls, open_initial_window, true, cx)
    }

    pub(super) fn launch_with_focus(
        worker: (
            RequestSender,
            async_channel::Receiver<WorkerResponse>,
            async_channel::Receiver<&'static str>,
        ),
        urls: async_channel::Receiver<Result<String, &'static str>>,
        open_initial_window: bool,
        focus_window: bool,
        cx: &mut App,
    ) -> Entity<Self> {
        // AppKit may deliver either its ordinary-open action or a session URL
        // after the finish-launch callback. Neither needs an unrelated window.
        let opening_link = !open_initial_window || !urls.is_empty();
        let host = cx.new(|cx| {
            let mut host = Self::new(worker, urls, cx);
            host.channel = crate::engine::profile::current().1;
            host.focus_window = focus_window;
            host.initial_link = opening_link;
            host
        });
        // The application action owns the host while an initial link is being
        // resolved without a window. Guest and Sessions windows also retain it.
        let sessions_host = host.clone();
        cx.on_action(move |_: &menus::ShowSessions, cx| {
            let host = sessions_host.clone();
            cx.defer(move |cx| Self::show_sessions(&host, cx));
        });
        let sign_out_host = host.clone();
        cx.on_action(move |_: &menus::SignOut, cx| {
            let host = sign_out_host.clone();
            cx.defer(move |cx| Self::confirm_sign_out(&host, cx));
        });

        if opening_link {
            crate::trace::mark("ui.launch_with_link");
        } else {
            Self::show_sessions(&host, cx);
        }
        host
    }

    pub(super) fn show_initial_feedback(&self, cx: &mut Context<Self>) {
        if self.sessions_window.is_some()
            || self.sign_in.is_some()
            || (!self.account_ready() && (self.enrolled.is_some() || self.error.is_empty()))
        {
            return;
        }
        let needs_window = !self.initial_link
            || !self.error.is_empty()
            || self.invitation.is_some()
            || matches!(
                self.last_transfer.kind,
                TransferKind::Running | TransferKind::Failed | TransferKind::Paused
            );
        if needs_window {
            // First render must run after the current entity update releases
            // its borrow, including startup/worker failures without a window.
            let host = cx.entity();
            cx.defer(move |cx| Self::show_sessions(&host, cx));
        }
    }

    pub(super) fn show_sessions(host: &Entity<Self>, cx: &mut App) {
        host.update(cx, |host, cx| {
            host.initial_link = false;
            host.restore_startup_opening(cx);
            cx.notify();
        });
        if host.read(cx).enrolled.is_none() && !host.read(cx).error.is_empty() {
            Self::show_startup_failure(host, cx);
            return;
        }
        if !host.read(cx).account_ready() || host.read(cx).sign_in.is_some() {
            host.update(cx, |host, cx| {
                host.ensure_sign_in(cx);
                if let Some(view) = &host.sign_in {
                    view.update(cx, |view, cx| view.activate(cx));
                }
            });
            return;
        }
        let focus_window = host.read(cx).focus_window;
        if let Some(handle) = host.read(cx).sessions_window
            && gpui::AnyWindowHandle::from(handle)
                .update(cx, |_, window, _| {
                    if focus_window {
                        window.activate_window();
                    }
                })
                .is_ok()
        {
            host.update(cx, |host, cx| {
                maybe_send_crash_reports(cx, host);
            });
            return;
        }
        host.update(cx, |host, _| {
            if host.sessions_window.is_some() {
                host.sessions_window = None;
            }
        });
        // open_window performs its first render synchronously. Do not hold a
        // mutable borrow of the root entity while asking GPUI to render it.
        let entity = host.clone();
        let minimum = sessions_min_size(crate::ui::desktop::zoom::factor(cx));
        let dimensions = size(px(1024.).max(minimum.width), px(560.).max(minimum.height));
        let bounds = Bounds::centered(None, dimensions, cx);
        let result = cx.open_window(
            WindowOptions {
                focus: focus_window,
                window_min_size: Some(minimum),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                frame_autosave_name: Some("Sessions".into()),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Sessions".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(gpui::point(px(15.), px(15.))),
                }),
                ..Default::default()
            },
            |window, cx| {
                window.focus(&entity.read(cx).focus);
                let host = entity.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    host.update(cx, |host, cx| {
                        if host.modal_open(cx) {
                            return false;
                        }
                        host.browser.as_ref().is_none_or(|browser| {
                            browser.update(cx, |browser, cx| browser.retain_local_drafts(cx))
                        })
                    })
                    .unwrap_or(true)
                });
                entity
            },
        );
        host.update(cx, |host, _| match result {
            Ok(handle) => {
                host.sessions_window = Some(handle);
                host.initial_link = false;
            }
            Err(error) => host.error = error.to_string(),
        });
        host.update(cx, |host, cx| {
            maybe_send_crash_reports(cx, host);
        });
    }
}

pub(super) fn maybe_send_crash_reports(cx: &mut Context<Shell>, shell: &mut Shell) {
    // The distributed collector credential is only configured for Public and Testing.
    if shell.channel.is_personal()
        || shell.crash_reports_pending
        || shell.signing_out
        || shell.sign_out_prompt
    {
        return;
    }
    let window = shell.sessions_window.map(Into::into).or_else(|| {
        cx.windows()
            .into_iter()
            .find(|window| window.downcast::<crate::ui::desktop::sign_in::SignIn>().is_some())
    });
    if let Some(window) = window
        && shell.first_account_update
        && let Some(profile) = shell.crash_report_profile.take()
    {
        shell.crash_reports_pending = true;
        let scope = crash_reports::Scope {
            profile,
            channel: shell.channel,
            account: (shell.enrolled == Some(true) && shell.account.id.0 != [0; 16])
                .then_some(shell.account.id),
        };
        crash_reports::offer(scope, cx.weak_entity(), window, cx);
    }
}

impl Shell {
    fn shell_main(
        &mut self,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let mut main = div()
            .id("main-content")
            .overflow_y_scroll()
            .flex_1()
            .min_h(ui(0.))
            .min_w(ui(0.))
            .p_6()
            .flex()
            .flex_col()
            .gap_4();
        main = if let Some(browser) = &self.browser
            && self.inline_session_form(cx).is_none()
        {
            div().id("volume-browser").flex_1().min_h_0().min_w_0().child(browser.clone())
        } else if let Some(form) = self.inline_session_form(cx).cloned() {
            let scrollbars = form.update(cx, |form, cx| form.scrollbars(cx));
            let handle = scrollbars.read(cx).handle();
            // Constrain the parent before measuring the form's percentage width,
            // so wrapped content contributes its full height before Back is placed.
            let content = main.track_scroll(&handle).child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w_full()
                    .max_w(ui(640.))
                    .min_w_0()
                    .gap_4()
                    .child(form)
                    .child(self.host_callback(
                        "close-new-session",
                        "Back",
                        false,
                        |this, _| this.new_session = None,
                        cx,
                    )),
            );
            div()
                .id("session-form-viewport")
                .relative()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .flex()
                .flex_col()
                .child(content)
                .child(crate::ui::desktop::scrollbar::Scrollbars::overlay(&scrollbars))
        } else if self.personal.is_some() {
            main.child(self.personal_panel(cx))
        } else if let Some(session) = self.opening.clone() {
            self.opening_content(main, session, cx)
        } else if let Some(invitation) =
            self.invitation.clone().filter(|_| !self.session_dialog_open(cx))
        {
            self.invitation_content(main, invitation, cx)
        } else if self.selected_session_id().is_some() {
            self.session_log_content(cx)
        } else {
            self.welcome_content(window, theme, cx)
        };
        main
    }
    fn panel_title(&mut self, cx: &mut Context<Self>) -> String {
        if self.enrolled.is_none() {
            "Playbit".into()
        } else if let Some(browser) = &self.browser
            && self.inline_session_form(cx).is_none()
        {
            browser.read(cx).volume.name.clone()
        } else if self.inline_session_form(cx).is_some() {
            new_session::FOLDER_ACCESS_TITLE.into()
        } else if self.personal.is_some() {
            "Volume access".into()
        } else if self.invitation.is_some() && !self.session_dialog_open(cx) {
            "Invitation".into()
        } else if self.selected_session_id().is_some() {
            "Session".into()
        } else {
            "New session".into()
        }
    }
    fn panel_header(
        &mut self,
        window: &Window,
        theme: &Theme,
        panel_title: String,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let new_session = self.selected_session_id().is_none()
            && self.browser.is_none()
            && self.personal.is_none()
            && (self.invitation.is_none() || self.session_dialog_open(cx))
            && self.inline_session_form(cx).is_none();
        div()
            .h(ui(44.))
            .min_h(px(44.))
            .window_control_area(gpui::WindowControlArea::Drag)
            .flex_none()
            .flex()
            .items_center()
            .px(ui(16.))
            .bg(rgba(theme.color(Role::WindowBackground).0))
            .when(!new_session, |header| {
                header.border_b_1().border_color(rgba(theme.color(Role::Separator).0))
            })
            .font_family(UI_FONT_FAMILY)
            .text_size(ui(14.))
            .line_height(ui(24.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(super::theme::section_title_color(window.is_window_active()))
            .when(new_session, |header| header.pr(ui(10.)))
            .justify_between()
            .child(self.host_text(HostText::PanelTitle, panel_title, cx))
            .when(new_session, |header| {
                header.child(div().flex().items_center().occlude().child(self.host_callback(
                    "open-join-dialog",
                    "Join Session…",
                    false,
                    |host, cx| host.open_join_dialog(cx),
                    cx,
                )))
            })
    }
    fn shell_content(
        &mut self,
        main: gpui::Stateful<gpui::Div>,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let inline_form = self.inline_session_form(cx).is_some();
        if self.browser.is_some() && !inline_form {
            return div().h_full().flex_1().min_w_0().flex().flex_col().child(main);
        }
        if self.sidebar_volumes && self.browser.is_none() && !inline_form {
            return div().h_full().flex_1().bg(rgba(theme.color(Role::PanelBackground).0));
        }
        let panel_title = self.panel_title(cx);
        let header = self.panel_header(window, theme, panel_title, cx);
        div()
            .h_full()
            .flex_1()
            .min_w(ui(0.))
            .flex()
            .flex_col()
            .bg(rgba(theme.color(Role::PanelBackground).0))
            .child(header)
            .child(main)
    }
    fn repair_control_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A completed action can replace the focused control (Resume → Join,
        // for example). Keep keyboard navigation reachable after that change.
        if self.modal_open(cx) {
            return;
        }
        if self.host_controls.iter().any(|(id, control)| {
            !self.rendered_host_controls.contains(id) && control.read(cx).focused(window)
        }) || self.personal.is_none()
            && self.personal_controls.values().any(|control| control.read(cx).focused(window))
        {
            window.focus(&self.focus);
        }
    }
    fn shell_frame(
        &mut self,
        sidebar: gpui::Div,
        content: gpui::Div,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> gpui::Div {
        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .flex()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function
                {
                    if modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                } else {
                    this.navigate_management_lists(event, window, cx);
                }
            }))
            .font_family(UI_FONT_FAMILY)
            .text_color(rgba(theme.color(Role::Text).0))
            .bg(rgba(theme.color(Role::WindowBackground).0))
            // Paint the native drag region first so controls can exclude it.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(ui(44.))
                    .min_h(px(44.))
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .child(sidebar)
            .child(content)
            // The final element observes CPU scene construction. This is not
            // GPU completion or display presentation; those need platform traces.
            .child(
                gpui::canvas(
                    |_, _, _| crate::trace::mark("ui.prepaint_tail"),
                    move |_, _, _, _| {
                        crate::trace::mark("ui.paint_tail");
                    },
                )
                .absolute()
                .size_0(),
            )
    }
}
impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_action_filter(
            self.modal_open(cx)
                .then_some(account_menu::dialog_action as fn(&dyn gpui::Action) -> bool),
        );
        self.present_feedback(window, cx);
        crate::ui::desktop::zoom::apply(window, cx);
        window.set_min_size(sessions_min_size(crate::ui::desktop::zoom::factor(cx)));
        self.rendered_host_controls.clear();
        self.rendered_host_text = 0;
        let _timing = crate::trace::Span::new("ui.build_elements");
        if self.channel_switching {
            return div()
                .size_full()
                .p_6()
                .font_family(UI_FONT_FAMILY)
                .child(self.host_text(HostText::ChannelSwitch, "Switching environment…", cx))
                .into_any_element();
        }
        crate::trace::mark(if !self.sessions.is_empty() {
            "ui.build_retained_sessions"
        } else {
            "ui.build_empty_sessions"
        });
        if self.signing_out {
            let mut content = div()
                .size_full()
                .p_6()
                .flex()
                .flex_col()
                .gap_4()
                .font_family(UI_FONT_FAMILY)
                .bg(gpui::rgb(0xf5f5f5));
            if let Some(error) = self.sign_out_error.clone() {
                content = content
                    .child("Couldn’t finish signing out")
                    .child(error)
                    .child(self.host_callback(
                        "retry-sign-out",
                        "Retry",
                        false,
                        |this, cx| this.begin_sign_out(cx),
                        cx,
                    ))
                    .child(self.host_button(
                        "quit-sign-out",
                        "Quit Playbit",
                        false,
                        Rc::new(|_, cx| cx.quit()),
                        cx,
                    ));
            } else {
                content = content.child("Signing out…");
            }
            return content.into_any_element();
        }
        if self.enrolled.is_none() {
            return div().into_any_element();
        }
        self.focus_volume_rename(window, cx);
        self.begin_application_render();
        let sidebar = self.sidebar_view(window.is_window_active(), cx);
        let theme = theme();
        let status = self.status_bar(window, &theme, cx);

        let content = if let Some(content) = self.application_browser_content(window, cx) {
            content
        } else {
            let main = self.shell_main(window, &theme, cx);
            self.shell_content(main, window, &theme, cx)
        };
        self.finish_application_render(cx);
        let toast = self.render_visual_bell(cx);
        self.repair_control_focus(window, cx);
        let background = self
            .shell_frame(sidebar, content, &theme, cx)
            .children(status)
            .children(toast)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.volume_rename.as_ref().is_some_and(|rename| rename.focused(window, cx))
                    {
                        window.focus(&this.focus);
                    }
                }),
            );
        self.with_modal(background, window, cx)
    }
}

#[cfg(test)]
mod tests;
