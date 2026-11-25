//! User-initiated environment switches retire this client's workers and any
//! engine it owns. Cached account/UI state never crosses that boundary.
use super::*;
use crate::release_channel::ReleaseChannel;

impl Shell {
    pub(super) fn switch_channel(
        &mut self,
        channel: ReleaseChannel,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.channel_switching {
            return Err("A channel switch is already in progress".into());
        }
        if self.crash_reports_pending {
            return Err("Finish handling the crash report before switching environments".into());
        }
        if self.signing_out || self.sign_out_prompt || self.recovery_busy.is_some() {
            return Err("Finish the current account action before switching channels".into());
        }
        let previous = self.channel;
        if previous == channel {
            return Ok(());
        }
        if previous.is_personal() || channel.is_personal() {
            return Err(
                "Launch Playbit with a separate profile to change personal Testing environments"
                    .into(),
            );
        }
        if self.volume_rename.is_some()
            || self.pending_volume.is_some()
            || self
                .browser
                .as_ref()
                .is_some_and(|browser| browser.read(cx).has_pending_entry_operation())
        {
            return Err("Finish naming entries or wait for the current file operation before \
                switching environments."
                .into());
        }
        if self.defer_draft_retirement(draft_lifecycle::Retirement::Channel(channel), cx) {
            return Ok(());
        }
        super::update::channel_switch_ready(cx)?;
        self.channel_switching = true;
        self.send.gate().pause();
        self.send.close();
        draft_publications::retire(cx);
        let (retired, _, _) = worker_queue::channel();
        retired.close();
        self.send = retired;
        // Retain the account/Sessions window between profile retirement and
        // the new worker's first status.
        let host = cx.entity();
        let profile = profile_path();
        cx.defer(move |cx| {
            Self::retire_channel_windows(&host, cx);
            host.update(cx, |host, cx| host.begin_channel_switch(profile, channel, previous, cx));
        });
        cx.notify();
        Ok(())
    }

    fn retire_channel_windows(host: &Entity<Self>, cx: &mut App) {
        Self::show_sessions(host, cx);
        crate::ui::desktop::popup::retire(cx);
        let sessions = host.read(cx).sessions_window;
        // Window-close observers update Shell synchronously. No Shell borrow
        // may remain live while windows are removed, including an idle popup.
        for window in cx.windows() {
            if Some(window) != sessions.map(Into::into)
                && window.downcast::<crate::ui::desktop::sign_in::SignIn>().is_none()
            {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            }
        }
    }

    fn begin_channel_switch(
        &mut self,
        profile: std::path::PathBuf,
        channel: ReleaseChannel,
        previous: ReleaseChannel,
        cx: &mut Context<Self>,
    ) {
        let job = cx.background_executor().spawn(async move {
            crate::diagnostic!(
                "Switching environment from {} to {}",
                previous.name(),
                channel.name()
            );
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let engine = crate::engine::launch::executable_for(&executable)
                .map_err(|error| error.to_string())?;
            crate::engine::launch::check_switch_destination(channel, &engine)
                .map_err(|error| error.to_string())?;
            if crate::engine::launch::retire_profile_engine(&profile, &engine, previous)
                .map_err(|error| error.to_string())?
            {
                crate::engine::launch_agent::control(
                    crate::engine::launch_agent::ServiceAction::RetireOwned,
                    previous,
                )
                .map_err(|error| error.to_string())?;
            }
            crate::engine::profile::switch_to(channel).map_err(|error| error.to_string())?;
            let mut reopen = std::process::Command::new(executable);
            crate::engine::profile::configure_relaunch(&mut reopen)
                .map_err(|error| error.to_string())?;
            if let Err(error) = reopen.spawn() {
                crate::engine::profile::switch_to(previous).map_err(|restore| {
                    format!(
                        "Could not reopen Playbit: {error}; \
                         could not restore the previous channel: {restore}"
                    )
                })?;
                return Err(format!("Could not reopen Playbit: {error}"));
            }
            crate::diagnostic!("Reopened Playbit in {}", channel.name());
            Ok(())
        });
        cx.spawn(async move |host, cx| {
            let result = job.await;
            let _ = host.update(cx, |host, cx| host.channel_switched(result, previous, cx));
        })
        .detach();
    }

    fn channel_switched(
        &mut self,
        result: Result<(), String>,
        previous: ReleaseChannel,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                cx.quit();
                return;
            }
            Err(error)
                if crate::engine::profile::initialize().is_err()
                    || crate::engine::profile::current().1 != previous =>
            {
                super::update::launch_error(&error);
                cx.quit();
                return;
            }
            Err(error) => self.error = error,
        }
        let (send, receive, errors) = worker::start(self.focus_window);
        Self::watch_errors(send.clone(), errors, cx);
        Self::watch_responses(send.clone(), receive, cx);
        let sign_in = self.sign_in.take();
        if let Some(sign_in) = &sign_in {
            sign_in.update(cx, |view, cx| view.channel_failed(self.error.clone(), cx));
        }
        let mut fresh = Self::with_editors(send, cx);
        fresh.sign_in = sign_in;
        fresh.channel = previous;
        fresh.focus_window = self.focus_window;
        fresh.sessions_window = self.sessions_window;
        fresh.initial_link = false;
        fresh.crash_report_profile = self.crash_report_profile.take();
        fresh.crash_reports_pending = self.crash_reports_pending;
        fresh.sending_crash_reports = self.sending_crash_reports.clone();
        fresh.error = std::mem::take(&mut self.error);
        *self = fresh;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn retiring_windows_allows_synchronous_host_observers(cx: &mut gpui::TestAppContext) {
        // A retained popup/guest closes before profile work. Its synchronous
        // close callback must be able to update the still-live Sessions host.
        let (host, sessions, _) = super::super::test_support::host(cx);
        let extra = cx.add_window(|_, _| gpui::EmptyView);
        let weak = host.downgrade();
        cx.update(|cx| {
            cx.on_window_closed(move |cx| {
                let _ = weak.update(cx, |host, _| host.error = "closed".into());
            })
            .detach();
            Shell::retire_channel_windows(&host, cx);
            assert!(cx.windows().contains(&sessions.into()));
            assert!(!cx.windows().contains(&extra.into()));
        });
        host.read_with(cx, |host, _| assert_eq!(host.error, "closed"));
    }

    #[gpui::test]
    fn retirement_preserves_the_only_sign_in_window(cx: &mut gpui::TestAppContext) {
        let (host, sessions, _) = super::super::test_support::host(cx);
        let (sign_in, view) = cx.update(|cx| {
            crate::ui::desktop::sign_in::open(
                ReleaseChannel::Public,
                false,
                std::rc::Rc::new(|_, _| Ok(())),
                std::rc::Rc::new(|_| {}),
                cx,
            )
            .expect("account window")
        });
        host.update(cx, |host, _| {
            host.sign_in = Some(view);
            host.enrolled = Some(false);
            host.sessions_window = None;
        });
        sessions.update(cx, |_, window, _| window.remove_window()).expect("close Sessions");
        let _popup = cx.add_window(|_, _| gpui::EmptyView);
        cx.update(|cx| {
            Shell::retire_channel_windows(&host, cx);
            assert!(cx.windows() == vec![sign_in.into()]);
        });
    }
}
