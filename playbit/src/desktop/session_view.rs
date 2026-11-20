//! Session tabs, toolbar actions, and selected tab content.
use super::*;
use crate::runtime::MenuEntry;
use crate::ui::desktop::zoom::length as ui;
use controls::SessionStyle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionTab {
    Log,
}

#[derive(Clone, Copy)]
enum SessionAction {
    Share,
    Stop,
}

const SESSION_TABS: [(&str, &str, Option<SessionTab>); 3] = [
    ("session-tab-log", "Log", Some(SessionTab::Log)),
    ("session-tab-performance", "Performance", None),
    ("session-tab-network", "Network", None),
];

fn visible_session_tab_count(widths: [f32; 3], available: f32) -> usize {
    if widths.iter().sum::<f32>() + 24. <= available {
        return widths.len();
    }
    let mut used = 24.;
    for (index, width) in widths.iter().enumerate() {
        used += width + 8.;
        if used > available {
            return index;
        }
    }
    widths.len()
}

impl Shell {
    pub(super) fn session_header(&mut self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let tabs = self.session_tab_strip(window, cx);
        let toolbar = self.session_toolbar(cx);
        div()
            .h(ui(44.))
            .min_h(px(44.))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .pl(ui(8.))
            .pr(ui(10.))
            .bg(rgb(0xececec))
            .border_b_1()
            .border_color(rgba(0x0000001a))
            .window_control_area(gpui::WindowControlArea::Drag)
            .child(tabs)
            .child(toolbar)
    }

    fn session_tab_strip(&mut self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let text_width = |label: &str, font_size, tracking| {
            let style = gpui::TextStyle {
                font_family: crate::ui::desktop::UI_FONT_FAMILY.into(),
                font_weight: gpui::FontWeight::MEDIUM,
                letter_spacing: crate::ui::desktop::zoom::scaled(window, tracking),
                ..Default::default()
            };
            f32::from(
                window
                    .text_system()
                    .shape_line(
                        label.to_owned().into(),
                        crate::ui::desktop::zoom::pixels(window, font_size),
                        &[style.to_run(label.len())],
                        None,
                    )
                    .width
                    .ceil(),
            )
        };
        let widths = SESSION_TABS.map(|(_, label, _)| {
            text_width(label, 13., 0.13) + f32::from(crate::ui::desktop::zoom::pixels(window, 16.))
        });
        // Sidebar 241; header padding 18; action gap 10; share padding/icon 42;
        // two 24-point icon buttons and two 8-point gaps. Text uses cached shaping.
        let available = f32::from(window.viewport_size().width)
            - f32::from(crate::ui::desktop::zoom::pixels(window, 375.))
            - text_width("Share Session", 11., 0.11);
        let scale = crate::ui::desktop::zoom::factor(cx);
        let available = available - 87. * (1. - scale).max(0.);
        let visible =
            visible_session_tab_count(widths.map(|width| width / scale), available / scale);
        let mut tabs = div().flex().flex_none().h_full().items_center().gap(ui(8.));
        for &(id, label, tab) in &SESSION_TABS[..visible] {
            let control = self.session_tab_button(id, label, tab, cx);
            tabs = tabs.child(control);
        }
        if visible < SESSION_TABS.len() {
            let control = self.session_tab_overflow(visible, cx);
            tabs = tabs.child(control.clone());
        }
        tabs
    }

    fn session_tab_button(
        &mut self,
        id: &'static str,
        label: &'static str,
        tab: Option<SessionTab>,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let disabled = tab.is_none();
        let control = self.host_callback(
            id,
            label,
            disabled,
            move |shell, cx| {
                if let Some(tab) = tab {
                    shell.request_session_tab(tab, cx);
                }
            },
            cx,
        );
        control.update(cx, |control, cx| {
            control.set_session_style(SessionStyle::Tab(tab == Some(self.session_tab)), cx)
        });
        control
    }

    fn session_tab_overflow(
        &mut self,
        visible: usize,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        self.rendered_host_controls.push("session-tab-overflow");
        let theme = theme();
        let properties = controls::Properties {
            kind: controls::Kind::Popup,
            label: "More tabs",
            disabled: false,
            theme: &theme,
        };
        let weak = cx.weak_entity();
        let activate: controls::Activate = Rc::new(move |id, cx| {
            if let Some((_, _, Some(tab))) = SESSION_TABS.get(id as usize) {
                let _ = weak.update(cx, |shell, cx| shell.request_session_tab(*tab, cx));
            }
        });
        let control = self.host_controls.entry("session-tab-overflow").or_insert_with(|| {
            cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx))
        });
        let items: Vec<_> = SESSION_TABS
            .iter()
            .enumerate()
            .skip(visible)
            .map(|(id, (_, label, tab))| MenuEntry::Action {
                icon: None,
                id: id as u32,
                label: (*label).into(),
                checked: tab.map(|tab| tab == self.session_tab),
                disabled: tab.is_none(),
                destructive: false,
            })
            .collect();
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
            control.set_sidebar_icon(
                controls::SidebarIcon::Glyph("session-overflow"),
                false,
                false,
                cx,
            );
            control.set_popup_below();
            control.set_popup_items(&items, cx);
        });
        control.clone()
    }

    fn session_toolbar(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let share = self.host_callback(
            "share-session",
            "Share Session",
            self.session_action_busy,
            |shell, cx| shell.share_session(cx),
            cx,
        );
        share.update(cx, |control, cx| control.set_session_style(SessionStyle::Share, cx));
        let clear = self.host_callback(
            "clear-session-log",
            "Clear log",
            self.logs.read(cx).lines.is_empty(),
            |shell, cx| shell.logs.update(cx, |logs, cx| logs.clear(cx)),
            cx,
        );
        clear.update(cx, |control, cx| {
            control.set_sidebar_icon(
                controls::SidebarIcon::Glyph("session-clear"),
                false,
                false,
                cx,
            )
        });
        let running = self.logs.read(cx).running;
        let action = if running == Some(true) {
            let control = self.host_callback(
                "stop-session",
                "Stop session",
                self.session_action_busy,
                |shell, cx| shell.stop_session(cx),
                cx,
            );
            control.update(cx, |control, cx| control.set_session_style(SessionStyle::Stop, cx));
            control
        } else {
            let control = self.host_callback(
                "start-session",
                "Start session",
                self.joining
                    || self.selected_session_id().is_some_and(|id| self.launches.contains_key(id)),
                |shell, cx| {
                    if let Some(session) = shell.selected_session_id().map(str::to_owned) {
                        shell.open_retained_session(session, cx);
                    }
                },
                cx,
            );
            control.update(cx, |control, cx| {
                control.set_sidebar_icon(
                    controls::SidebarIcon::Glyph("session-start"),
                    false,
                    false,
                    cx,
                )
            });
            control
        };
        let stage = self
            .selected_session_id()
            .and_then(|id| self.launches.get(id))
            .filter(|progress| progress.visible)
            .map(|progress| progress.phase.label());
        div()
            .flex()
            .min_w_0()
            .items_center()
            .gap(ui(8.))
            .when_some(stage, |view, stage| {
                view.child(div().min_w_0().truncate().text_size(ui(12.)).child(stage))
            })
            .child(share)
            .child(clear)
            .child(action)
    }

    /// Register the open intent for a session this device holds and start it.
    pub(super) fn open_retained(
        &mut self,
        id: String,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        self.sidebar_session = Some(id.clone());
        self.opening = Some(id.clone());
        self.joining = self.account_ready() && self.sign_in.is_none();
        if self.joining {
            self.prepare_session(id, cx)
        } else {
            self.pending_windows.insert(id, None);
            Ok(())
        }
    }

    pub(super) fn prepare_session(
        &mut self,
        session: String,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        if self
            .launches
            .get(&session)
            .is_some_and(|progress| progress.instance.is_some() || progress.form.is_some())
        {
            return Ok(());
        }
        if !self.launches.contains_key(&session) && !self.begin_launch(&session, cx) {
            return Err("The session could not begin opening.");
        }
        if self.pending_windows.get(&session).is_some_and(Option::is_some) {
            return Ok(());
        }
        let admission = match self.send.prepare_session(session.clone()) {
            Ok(admission) => admission,
            Err(error) => {
                self.launches.remove(&session);
                return Err(error);
            }
        };
        if let Some(progress) = self.launches.get_mut(&session) {
            progress.admission = Some(admission);
        }
        self.pending_windows.insert(session, Some(admission));
        Ok(())
    }

    pub(super) fn request_session_tab(&mut self, tab: SessionTab, cx: &mut Context<Self>) {
        if self.session_tab == tab
            && self.tab_change_pending.is_none()
            && (tab != SessionTab::Log || self.log_watch.is_some())
        {
            return;
        }
        if self.tab_change_pending == Some(tab) {
            return;
        }
        let Some(handle) = self.sessions_window else { return };
        self.tab_change_pending = Some(tab);
        // Control activation still borrows the Shell window. Apply ownership
        // and visible content together after that input callback releases it.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |shell, window, cx| {
                if shell.tab_change_pending != Some(tab) {
                    return;
                }
                shell.tab_change_pending = None;
                shell.apply_session_tab(tab, window, cx);
            });
        });
    }

    pub(super) fn apply_session_tab(
        &mut self,
        tab: SessionTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match tab {
            SessionTab::Log => {
                self.session_tab = SessionTab::Log;
                if self.log_watch.is_none() {
                    self.refresh_logs(cx);
                }
            }
        }
        cx.notify();
        true
    }

    pub(super) fn refresh_logs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session_id().map(str::to_owned) else {
            self.log_watch = None;
            self.log_session = None;
            self.log_generation =
                self.log_generation.checked_add(1).expect("log observation generation cannot wrap");
            return;
        };
        if self.log_session.as_ref() == Some(&session) && self.log_watch.is_some() {
            return;
        }
        self.log_watch = None;
        self.log_session = Some(session.clone());
        self.log_generation =
            self.log_generation.checked_add(1).expect("log observation generation cannot wrap");
        let generation = self.log_generation;
        self.logs.update(cx, |logs, cx| {
            logs.select(&session, cx);
        });
        let (watch, receive) =
            match log::Watch::start(profile_path(), session.clone(), self.logs.read(cx).log_cursor)
            {
                Ok(started) => started,
                Err(error) => {
                    self.error = error;
                    self.logs.update(cx, |logs, cx| {
                        logs.receive(
                            &session,
                            log::Update::Notice(
                                "Log observation unavailable. Select Log again to attach.",
                            ),
                            cx,
                        )
                    });
                    return;
                }
            };
        self.log_watch = Some(watch);
        self.watch_logs(session, generation, receive, cx);
    }

    fn watch_logs(
        &self,
        session: String,
        generation: u64,
        receive: async_channel::Receiver<log::Update>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |entity, cx| {
            while let Ok(update) = receive.recv().await {
                if entity
                    .update(cx, |this, cx| {
                        if this.log_generation != generation {
                            return;
                        }
                        // A log socket also closes on ordinary app exit. Keep
                        // passive observation notices in the Log pane while
                        // engine ownership notifications resolve the lifecycle.
                        this.logs.update(cx, |logs, cx| {
                            logs.receive(&session, update, cx);
                        });
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
            let _ = entity.update(cx, |this, cx| {
                if this.log_generation == generation {
                    this.log_watch = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn selected_session_id(&self) -> Option<&str> {
        self.sidebar_session
            .as_deref()
            .or_else(|| self.selected.as_ref().map(|p| p.session.id.as_str()))
    }

    pub(super) fn selected_details(&self) -> Option<&crate::engine::reply::PreparedSession> {
        self.selected
            .as_ref()
            .filter(|details| Some(details.session.id.as_str()) == self.selected_session_id())
    }

    pub fn open_retained_session(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.prepare_browser_navigation(cx) {
            return;
        }
        if !self.begin_launch(&id, cx) {
            return;
        }
        self.cancel_invitation_restoration();
        self.select_sidebar_session(id.clone(), cx);
        self.opening = Some(id.clone());
        self.invitation = None;
        self.joining = true;
        self.error.clear();
        if self.pending_windows.contains_key(&id) {
            // Keep the launch's existing admission credit.
        } else {
            if let Err(error) = self.prepare_session(id.clone(), cx) {
                self.launches.remove(&id);
                self.joining = false;
                self.error = error.into();
            }
        }
        cx.notify();
    }
}

impl Shell {
    fn perform_session_action(&mut self, action: SessionAction, cx: &mut Context<Self>) {
        if self.session_action_busy {
            return;
        }
        let Some(session) = self.selected_session_id().map(str::to_owned) else {
            return;
        };
        self.session_action_busy = true;
        if matches!(action, SessionAction::Share) {
            self.error.clear();
            self.alerted_error.clear();
            self.show_visual_bell("Copying invitation…", "session-share", cx);
        }
        let profile = profile_path();
        let sender = self.send.clone();
        let work = cx.background_executor().spawn(async move {
            match action {
                SessionAction::Stop => session::stop(&profile, session).map(|()| None),
                SessionAction::Share => session::invitation(&profile, session).map(Some),
            }
        });
        cx.spawn(async move |host, cx| {
            let result: crate::engine::Result<Option<String>> = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) {
                    return;
                }
                host.session_action_busy = false;
                match result {
                    Ok(Some(invitation)) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(invitation));
                        host.show_visual_bell("Invitation link copied", "menu-check", cx);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        host.error = if matches!(action, SessionAction::Share) {
                            host.visual_bell = None;
                            format!("Couldn’t copy invitation: {error}")
                        } else {
                            error.to_string()
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Shell {
    pub(super) fn session_build_status(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let message = self
            .sessions
            .iter()
            .find(|row| Some(row.id.as_str()) == self.selected_session_id())
            .and_then(|row| row.build_state.message())?;
        Some(
            div()
                .id("session-build-status")
                .flex_none()
                .max_h(px(160.))
                .overflow_y_scroll()
                .px(px(16.))
                .py(px(10.))
                .text_size(px(13.))
                .child(self.host_text(HostText::BuildStatus, message, cx)),
        )
    }

    pub(super) fn session_log_content(
        &mut self,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let notice = self.logs.read(cx).notice;
        div()
            .id("session-content")
            .debug_selector(|| "session-content".into())
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .when_some(self.session_build_status(cx), |view, status| view.child(status))
            .when_some(notice, |view, notice| {
                view.child(
                    div()
                        .flex_none()
                        .p(ui(8.))
                        .text_size(ui(12.))
                        .text_color(rgba(0x00000099))
                        .child(self.host_text(HostText::LogNotice, notice, cx)),
                )
            })
            .child(div().flex_1().min_h_0().child(self.logs.clone()))
    }
}

impl Shell {
    pub(super) fn share_session(&mut self, cx: &mut Context<Self>) {
        self.perform_session_action(SessionAction::Share, cx);
    }

    fn stop_session(&mut self, cx: &mut Context<Self>) {
        self.perform_session_action(SessionAction::Stop, cx);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[gpui::test]
    fn log_observation_notices_follow_the_source_lifecycle(cx: &mut gpui::TestAppContext) {
        // Feed the actual observer/UI channel through repeated disconnects,
        // retirement and a new run. Warnings stay readable beside retained logs.
        let (host, window, _requests) = test_support::host(cx);
        let (send, receive) = async_channel::bounded(1);
        host.update(cx, |host, cx| {
            host.sidebar_session = Some("session".into());
            host.logs.update(cx, |logs, cx| {
                logs.select("session", cx);
                logs.set(
                    "session",
                    &[crate::ui::app_control::LogEntry::notice("Retained output".into())],
                    cx,
                );
            });
            host.watch_logs("session".into(), host.log_generation, receive, cx);
        });
        const NOTICE: &str = "App logs are unavailable. Earlier log messages may be missing.";
        for update in [
            log::Update::Running,
            log::Update::Notice(NOTICE),
            log::Update::Running,
            log::Update::Notice(NOTICE),
            log::Update::Stopped,
            log::Update::Running,
        ] {
            let warning = matches!(update, log::Update::Notice(_));
            send.try_send(update).unwrap();
            cx.run_until_parked();
            window
                .update(cx, |host, window, cx| {
                    let logs = host.logs.read(cx);
                    assert_eq!(logs.notice, warning.then_some(NOTICE));
                    assert_eq!(logs.lines[0].message, "Retained output");
                    let values = window
                        .accessibility_nodes_for_test()
                        .filter_map(|(_, node)| node.value())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        values.iter().filter(|value| **value == NOTICE).count(),
                        usize::from(warning)
                    );
                    assert!(values.contains(&"Retained output"));
                    assert!(crate::ui::desktop::alert::take_presented(cx).is_empty());
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn session_toolbar_resizes_without_overlap_and_clear_tracks_log(cx: &mut gpui::TestAppContext) {
        let (host, window, _requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.sidebar_session = Some("toolbar-test".into());
            host.logs.update(cx, |logs, cx| logs.set("toolbar-test", &[], cx));
            cx.notify();
        });
        for (width, visible) in [(1024., 3), (730., 3), (650., 2), (540., 1), (500., 0), (1024., 3)]
        {
            cx.simulate_window_resize(window.into(), size(px(width), px(400.)));
            cx.update(|cx| {
                gpui::AnyWindowHandle::from(window)
                    .update(cx, |_, window, cx| window.draw(cx))
                    .unwrap()
            })
            .clear();
            host.read_with(cx, |host, _| {
                assert_eq!(
                    host.rendered_host_controls
                        .iter()
                        .filter(
                            |id| id.starts_with("session-tab-") && **id != "session-tab-overflow"
                        )
                        .count(),
                    visible,
                    "width {width}"
                );
                assert_eq!(
                    host.rendered_host_controls.contains(&"session-tab-overflow"),
                    visible < SESSION_TABS.len()
                );
            });
            cx.update(|cx| {
                window
                    .update(cx, |_, window, _| {
                        let nodes: Vec<_> =
                            window.accessibility_nodes_for_test().map(|(_, node)| node).collect();
                        let share = nodes
                            .iter()
                            .find(|n| n.label() == Some("Share Session"))
                            .unwrap()
                            .bounds()
                            .unwrap();
                        for label in ["Log", "Performance", "Network", "More tabs"] {
                            if let Some(node) = nodes.iter().find(|n| n.label() == Some(label)) {
                                assert!(
                                    node.bounds().unwrap().x1 <= share.x0,
                                    "{label} overlaps Share at {width}"
                                );
                            }
                        }
                        let clear = nodes.iter().find(|n| n.label() == Some("Clear log")).unwrap();
                        assert!(clear.is_disabled());
                    })
                    .unwrap()
            });
        }
        host.update(cx, |host, cx| {
            host.logs.update(cx, |logs, cx| {
                logs.set(
                    "toolbar-test",
                    &[crate::ui::app_control::LogEntry::recorded(
                        "A log line".into(),
                        crate::ui::app_control::LogSeverity::Info,
                    )],
                    cx,
                )
            });
            cx.notify();
        });
        cx.run_until_parked();
        test_support::check_zoom_geometry(window.into(), cx);
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    let clear = window
                        .accessibility_nodes_for_test()
                        .map(|(_, n)| n)
                        .find(|n| n.label() == Some("Clear log"))
                        .unwrap();
                    assert!(!clear.is_disabled());
                    host.host_controls["clear-session-log"].read(cx).focus(window);
                })
                .unwrap()
        });
        test_support::press(window, "enter", cx);
        host.read_with(cx, |host, cx| assert!(host.logs.read(cx).lines.is_empty()));
        cx.update(|cx| {
            window
                .update(cx, |_, window, _| {
                    assert!(
                        window
                            .accessibility_nodes_for_test()
                            .map(|(_, n)| n)
                            .find(|n| n.label() == Some("Clear log"))
                            .unwrap()
                            .is_disabled()
                    );
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn failed_log_observer_same_tab_retry_is_admitted_once(cx: &mut gpui::TestAppContext) {
        // A failed/completed observer leaves no Watch. Same-tab native input
        // must reach refresh_logs, while repeated pending requests coalesce.
        let (host, window, _requests) = host(cx);
        host.update(cx, |host, cx| {
            host.sidebar_session = Some("session".into());
            cx.notify();
        });
        host.update(cx, |host, cx| {
            host.session_tab = SessionTab::Log;
            host.log_watch = None;
            host.logs.update(cx, |logs, cx| {
                logs.set(
                    "session",
                    &[crate::ui::app_control::LogEntry::notice(
                        "Select Log again to attach".into(),
                    )],
                    cx,
                )
            });
        });
        cx.run_until_parked();
        let before = host.read_with(cx, |host, _| host.log_generation);
        focus_button(window, "session-tab-log", cx);
        press(window, "enter", cx);
        host.read_with(cx, |host, _| assert_eq!(host.log_generation, before + 1));
        host.update(cx, |host, cx| {
            host.log_watch = None;
            host.request_session_tab(SessionTab::Log, cx);
            host.request_session_tab(SessionTab::Log, cx);
        });
        cx.run_until_parked();
        host.read_with(cx, |host, _| assert_eq!(host.log_generation, before + 2));
    }
}
