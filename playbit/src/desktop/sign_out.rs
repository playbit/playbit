use super::*;

impl Shell {
    pub(super) fn confirm_sign_out(host: &Entity<Self>, cx: &mut App) {
        if host.read(cx).crash_reports_pending {
            host.update(cx, |host, cx| {
                host.error = "Finish handling the crash report before signing out".into();
                cx.notify();
            });
            return;
        }
        if !host.read(cx).account_ready()
            || host.read(cx).sign_out_prompt
            || host.read(cx).signing_out
        {
            return;
        }
        let Some(window) = cx.active_window() else { return };
        host.update(cx, |host, _| host.sign_out_prompt = true);
        let response = window.update(cx, |_, window, cx| {
            window.prompt(
                gpui::PromptLevel::Info,
                "Sign out?",
                Some("Signing out leaves all local data as is"),
                &[
                    gpui::PromptButton::Ok("Sign Out".into()),
                    gpui::PromptButton::Cancel("Cancel".into()),
                ],
                cx,
            )
        });
        let Ok(response) = response else {
            host.update(cx, |host, _| host.sign_out_prompt = false);
            return;
        };
        let host = host.clone();
        cx.spawn(async move |cx| {
            let confirmed = response.await == Ok(0);
            let _ = cx.update(|cx| {
                host.update(cx, |host, _| host.sign_out_prompt = false);
                if !confirmed {
                    return;
                }
                // Keep one window while guest views release their engine leases.
                Self::show_sessions(&host, cx);
                if host.read(cx).crash_reports_pending {
                    host.update(cx, |host, cx| {
                        host.error = "Finish handling the crash report before signing out".into();
                        cx.notify();
                    });
                    return;
                }
                let sessions_window = host.read(cx).sessions_window;
                host.update(cx, |host, cx| host.begin_sign_out(cx));
                for window in cx.windows() {
                    if Some(window) != sessions_window.map(Into::into) {
                        let _ = window.update(cx, |_, window, _| window.remove_window());
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn begin_sign_out(&mut self, cx: &mut Context<Self>) {
        if self.defer_draft_retirement(draft_lifecycle::Retirement::SignOut, cx) {
            return;
        }
        if self.signing_out && self.sign_out_error.is_none() {
            return;
        }
        draft_publications::retire(cx);
        self.signing_out = true;
        self.ap_status.update(cx, |indicator, cx| indicator.set(None, cx));
        self.sign_out_error = None;
        cx.notify();
        let profile = profile_path();
        let job = cx.background_executor().spawn(async move {
            crate::engine::sign_out::keep_profile(&profile).map_err(|e| e.to_string())?;
            let mut reopen =
                std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            crate::engine::profile::configure_relaunch(&mut reopen).map_err(|e| e.to_string())?;
            reopen.spawn().map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        });
        cx.spawn(async move |host, cx| {
            let result = job.await;
            let _ = host.update(cx, |host, cx| match result {
                Ok(()) => cx.quit(),
                Err(error) => {
                    crate::diagnostic!("Sign out failed: {error}");
                    host.sign_out_error = Some(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
