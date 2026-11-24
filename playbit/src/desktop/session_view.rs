//! Selected session lifecycle, invitation sharing and local log observation.
use super::*;
use crate::ui::desktop::zoom::length as ui;

impl Shell {
    /// Register the open intent for a session this device holds and start it.
    pub(super) fn open_retained(
        &mut self,
        id: String,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        self.leave_invitation();
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

    pub(super) fn refresh_logs(&mut self, cx: &mut Context<Self>) {
        let Some(session) =
            self.selected_session_id().filter(|_| !self.sidebar_volumes).map(str::to_owned)
        else {
            self.log_watch = None;
            self.log_session = None;
            self.log_generation =
                self.log_generation.checked_add(1).expect("log observation generation cannot wrap");
            return;
        };
        let instance = self.applications.instance;
        let target = (session.clone(), instance);
        if self.log_session.as_ref() == Some(&target) && self.log_watch.is_some() {
            return;
        }
        self.log_watch = None;
        self.log_session = Some(target);
        self.log_generation =
            self.log_generation.checked_add(1).expect("log observation generation cannot wrap");
        let generation = self.log_generation;
        self.logs.update(cx, |logs, cx| {
            logs.select(&session, instance, cx);
        });
        let (watch, receive) = match log::Watch::start(
            profile_path(),
            session.clone(),
            instance,
            self.logs.read(cx).log_cursor,
        ) {
            Ok(started) => started,
            Err(error) => {
                self.error = error;
                self.logs.update(cx, |logs, cx| {
                    logs.receive(
                        &session,
                        log::Update::Notice(
                            "Log observation unavailable. Select the session again to attach.",
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
        self.leave_invitation();
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
    pub(super) fn share_session(&mut self, cx: &mut Context<Self>) {
        if self.session_action_busy {
            return;
        }
        let Some(session) = self.selected_session_id().map(str::to_owned) else {
            return;
        };
        self.session_action_busy = true;
        self.error.clear();
        self.alerted_error.clear();
        self.show_visual_bell("Copying invitation…", "session-share", cx);
        let profile = profile_path();
        let sender = self.send.clone();
        let work =
            cx.background_executor().spawn(async move { session::invitation(&profile, session) });
        cx.spawn(async move |host, cx| {
            let result = work.await;
            let _ = host.update(cx, |host, cx| {
                if host.signing_out || !host.send.same_worker(&sender) {
                    return;
                }
                host.session_action_busy = false;
                match result {
                    Ok(invitation) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(invitation));
                        host.show_visual_bell("Invitation link copied", "menu-check", cx);
                    }
                    Err(error) => {
                        host.visual_bell = None;
                        host.error = format!("Couldn’t copy invitation: {error}");
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
                logs.select("session", None, cx);
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
            log::Update::Running(crate::store::Id::from_bytes([1; 16])),
            log::Update::Notice(NOTICE),
            log::Update::Running(crate::store::Id::from_bytes([1; 16])),
            log::Update::Notice(NOTICE),
            log::Update::Stopped,
            log::Update::Running(crate::store::Id::from_bytes([1; 16])),
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
}
