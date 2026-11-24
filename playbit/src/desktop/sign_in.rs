//! IPC adapter for the shared account window. Tickets only correlate presentation;
//! the engine always owns admitted enrollment and durable recovery completion.
use super::*;
use crate::ui::desktop::sign_in::{
    self,
    flow::{Command, ProfileChange, Reply as SignInResult, Step, Ticket},
};

impl Shell {
    #[cfg(not(test))]
    pub(super) fn reset_sign_in(&mut self, ticket: Option<Ticket>, cx: &mut Context<Self>) {
        self.change_sign_in_profile(ticket, ProfileChange::Reset, cx);
    }
    pub(super) fn change_sign_in_profile(
        &mut self,
        ticket: Option<Ticket>,
        change: ProfileChange,
        cx: &mut Context<Self>,
    ) {
        if self.crash_reports_pending {
            let error = "Finish handling the crash report before changing accounts";
            if let Some(ticket) = ticket
                && let Some(view) = &self.sign_in
            {
                view.update(cx, |view, cx| view.receive(ticket, Err(error.into()), cx));
            } else {
                self.error = error.into();
                cx.notify();
            }
            return;
        }
        if self.defer_draft_retirement(draft_lifecycle::Retirement::Profile(ticket, change), cx) {
            return;
        }
        if change == ProfileChange::Replace(true) {
            // Retire views of A before admitting B. Keep the account decision visible.
            for window in cx.windows() {
                if Some(window) != self.sessions_window.map(Into::into)
                    && window.downcast::<sign_in::SignIn>().is_none()
                {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                }
            }
        }
        // Retire replies before changing enrollment or replacing the local profile.
        self.send.close();
        draft_publications::retire(cx);
        let (retired, _, _) = worker_queue::channel();
        retired.close();
        self.send = retired;
        let profile = profile_path();
        let retained_invitation = self.invitation.clone();
        // A link that has never resolved lives only for this GUI launch. Carry it
        // through a same-launch profile change without journaling its capability.
        let link = self
            .invitation_link
            .as_ref()
            .filter(|link| {
                retained_invitation.as_ref().is_some_and(|preview| {
                    preview.received.is_none() && preview.session == link.internal_id()
                })
            })
            .cloned();
        let invitation = retained_invitation
            .as_ref()
            .filter(|preview| preview.received.is_some())
            .map(|preview| preview.session.clone());
        let job = cx.background_executor().spawn(async move {
            let result = (|| -> crate::engine::Result<_> {
                std::fs::create_dir_all(&profile)?;
                match change {
                    ProfileChange::Replace(replace) => {
                        crate::engine::sign_out::resolve_replacement(&profile, replace)?;
                        Ok(None)
                    }
                    ProfileChange::Reset => {
                        crate::engine::sign_out::reset_profile(&profile, invitation.as_deref())
                    }
                    ProfileChange::SignOut => {
                        crate::engine::sign_out::keep_profile(&profile)?;
                        Ok(retained_invitation)
                    }
                }
            })();
            result.map_err(|error| {
                crate::platform::failure::log(
                    &profile,
                    "change sign-in profile",
                    &format!("change={change:?} error={error:?}"),
                );
                change.failure_message(false).to_owned()
            })
        });
        cx.spawn(async move |host, cx| {
            let result = job.await;
            let _ = host.update(cx, |host, cx| {
                if let Ok(invitation) = &result {
                    let (send, receive, errors) = worker::start(host.focus_window);
                    Self::watch_errors(send.clone(), errors, cx);
                    Self::watch_responses(send.clone(), receive, cx);
                    let mut fresh = Self::with_editors(send, cx);
                    fresh.channel = host.channel;
                    fresh.sign_in = host.sign_in.take();
                    fresh.focus_window = host.focus_window;
                    fresh.sessions_window = host.sessions_window;
                    fresh.crash_report_profile = host.crash_report_profile.take();
                    fresh.crash_reports_pending = host.crash_reports_pending;
                    fresh.sending_crash_reports = host.sending_crash_reports.clone();
                    fresh.initial_link = host.initial_link;
                    if change != ProfileChange::Replace(true) {
                        fresh.pending_database = host.pending_database.take();
                    }
                    fresh.invitation = invitation.clone();
                    if change != ProfileChange::Replace(true) {
                        fresh.opening = host.opening.take();
                    }
                    if matches!(change, ProfileChange::Reset | ProfileChange::SignOut)
                        && let Some(link) = link
                        && let Err(error) = fresh.preview_invitation_link(link)
                    {
                        fresh.error = error.into();
                    }
                    fresh.enrolled = Some(false);
                    fresh.restore_pending_invitation = false;
                    *host = fresh;
                    host.send.send(Request::Status).ok();
                }
                if let Some(ticket) = ticket {
                    if let Some(view) = &host.sign_in {
                        view.update(cx, |view, cx| {
                            view.receive(
                                ticket,
                                result.map(|_| match change {
                                    ProfileChange::Reset => SignInResult::Reset,
                                    ProfileChange::Replace(replace) => {
                                        SignInResult::ReplacementResolved(replace)
                                    }
                                    ProfileChange::SignOut => SignInResult::SignedOut,
                                }),
                                cx,
                            );
                        });
                    }
                } else {
                    host.startup_prompt = false;
                    if let Err(error) = result {
                        host.error = error;
                        let host = cx.entity();
                        cx.defer(move |cx| Self::show_startup_failure(&host, cx));
                    } else {
                        host.ensure_sign_in(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn account_ready(&self) -> bool {
        self.enrolled == Some(true)
            && self.account_profile.unlocked
            && !self.account_profile.agreement_required
            // A retained name and acknowledged recovery state establish local
            // setup. Live account replication is only needed for first setup;
            // requiring it here hides locally available sessions while offline.
            && self
                .account_profile
                .display_name
                .as_deref()
                .is_some_and(sign_in::validation::display_name)
            && !matches!(
                self.recovery,
                Some(RecoveryStatus::Setup { .. } | RecoveryStatus::Locked { .. })
            )
    }

    pub(super) fn ensure_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.sign_in {
            view.update(cx, |view, cx| {
                view.profile(&self.account_profile, cx);
                if view.flow.step == Step::Profile
                    && !view.flow.busy()
                    && self.last_transfer.kind == TransferKind::Failed
                {
                    view.flow.initialization_failed(
                        self.last_transfer
                            .message
                            .clone()
                            .unwrap_or_else(|| "Account information unavailable. Retry.".into()),
                    );
                    cx.notify();
                }
            });
            return;
        }
        if self.account_ready() || self.enrolled.is_none() || self.sign_in_opening {
            return;
        }
        self.sign_in_opening = true;
        let host = cx.weak_entity();
        let enrolled = self.enrolled == Some(true);
        let channel = self.channel;
        cx.defer(move |cx| {
            let complete = host.clone();
            let dispatch = host.clone();
            let result = sign_in::open(
                channel,
                enrolled,
                Rc::new(move |work, cx| {
                    let dispatch = dispatch.clone();
                    cx.defer(move |cx| {
                        let _ = dispatch.update(cx, |host, cx| {
                            if let Command::ChangeProfile(change) = work.command {
                                host.change_sign_in_profile(Some(work.ticket), change, cx);
                            } else if let Err(error) =
                                host.send.sign_in(request(work.command), work.ticket)
                                && let Some(view) = &host.sign_in
                            {
                                view.update(cx, |view, cx| {
                                    view.receive(work.ticket, Err(error.into()), cx)
                                });
                            }
                        });
                    });
                    Ok(())
                }),
                Rc::new(move |cx| {
                    let complete = complete.clone();
                    cx.defer(move |cx| {
                        let _ = complete.update(cx, |host, cx| {
                            host.sign_in = None;
                            host.error.clear();
                            host.resume_sign_in_destination(cx);
                            host.send.send(Request::Status).ok();
                            cx.notify();
                        });
                        if let Some(host) = complete.upgrade() {
                            Shell::show_sessions(&host, cx);
                        }
                    });
                }),
                cx,
            );
            let _ = host.update(cx, |host, cx| {
                host.sign_in_opening = false;
                match result {
                    Ok((_, view)) => {
                        let channel_host = cx.weak_entity();
                        view.update(cx, |view, cx| {
                            view.profile(&host.account_profile, cx);
                            view.channel_selection(
                                Rc::new(move |channel, cx| {
                                    channel_host
                                        .update(cx, |host, cx| host.switch_channel(channel, cx))
                                        .map_err(|_| {
                                            "Playbit closed before switching channels".to_owned()
                                        })?
                                }),
                                cx,
                            );
                        });
                        host.sign_in = Some(view);
                        maybe_send_crash_reports(cx, host);
                    }
                    Err(error) => host.error = error.to_string(),
                }
                cx.notify();
            });
        });
    }
    pub(super) fn resume_sign_in_destination(&mut self, cx: &mut Context<Self>) {
        if self.joining {
            return;
        }
        if let Some(session) = &self.opening {
            match self.prepare_session(session.clone(), cx) {
                Ok(()) => self.joining = true,
                Err(error) => self.error = error.into(),
            }
            return;
        }
        if self.join_preview.is_some()
            || self.invitation_resume.is_some()
            || self
                .new_session
                .as_ref()
                .is_some_and(|form| form.read(cx).reviewed_invitation().is_some())
        {
            return;
        }
        if let Some(preview) = self.invitation.as_ref().filter(|p| !p.paused) {
            let session = preview.session.clone();
            let input = self.invitation_link.as_ref().map(|link| link.link(self.channel));
            self.open_join_entry(input.as_deref().unwrap_or(&session), cx);
            self.lookup_join_dialog(Request::WatchInvitation { session });
        }
    }
    pub(super) fn apply_sign_in_response(
        &mut self,
        response: &mut WorkerResponse,
        cx: &mut Context<Self>,
    ) {
        let Some(ticket) = response.sign_in.take() else {
            return;
        };
        let result = match &response.result {
            Err(error) => Err(error.clone()),
            Ok(()) => match response.account_response.take() {
                Some(AccountResponse::CodeSent(true)) => Ok(SignInResult::CodeSent),
                Some(AccountResponse::SignedIn(state)) => {
                    if state.signed_in {
                        self.enrolled = Some(true);
                    }
                    Ok(SignInResult::SignedIn(state))
                }
                Some(AccountResponse::Recovery(state)) => {
                    self.apply_recovery(state.clone(), cx);
                    Ok(SignInResult::Recovery(state))
                }
                Some(AccountResponse::SavedAccount(value)) => Ok(SignInResult::SavedAccount(value)),
                Some(AccountResponse::Prepared(key)) => Ok(SignInResult::Prepared(key)),
                Some(AccountResponse::AgreementAccepted) => {
                    self.account_profile.agreement_required = false;
                    Ok(SignInResult::AgreementAccepted)
                }
                Some(AccountResponse::DisplayName(name)) => {
                    self.account_profile.display_name = Some(name);
                    Ok(SignInResult::NameSaved)
                }
                _ if response.transfer_status.is_some() => Ok(SignInResult::Initialized),
                _ => match (&response.status_account_profile, response.account_identity.take()) {
                    (Some(profile), Some((account, email))) => {
                        Ok(SignInResult::Profile { account, email, profile: profile.clone() })
                    }
                    _ => Err("Account operation returned an unexpected response".into()),
                },
            },
        };
        if let Some(view) = self.sign_in.clone() {
            view.update(cx, |view, cx| view.receive(ticket, result, cx));
        }
    }
}
fn request(command: Command) -> Request {
    match command {
        Command::ChangeProfile(_) => {
            unreachable!("profile changes are handled by the host before IPC dispatch")
        }
        Command::SavedAccount => Request::SavedAccount,
        Command::RequestCode(email) => Request::RequestCode { email },
        Command::SignIn(code) => Request::SignIn { code },
        Command::AcceptAgreement(account) => Request::AcceptAgreement {
            account,
            digest: crate::network::ap::registration::terms_digest(),
        },
        Command::SignInWithKey { email, key } => Request::SignInWithKey { email, account_key: key },
        Command::Recovery => Request::RecoveryStatus,
        Command::PrepareKey => Request::PrepareRecovery,
        Command::SaveKey => Request::SaveRecovery,
        Command::RestoreKey(secret) => Request::RecoverCredentials { secret },
        Command::Profile => Request::Status,
        Command::Initialize => Request::StartSync,
        Command::SaveName(name) => Request::SetDisplayName { name },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded adapter messages.")]
mod tests {
    use super::*;
    use crate::desktop::worker_queue::{Message, QueuedRequest};
    #[gpui::test]
    fn retired_worker_cannot_change_new_profile_state(cx: &mut gpui::TestAppContext) {
        let (host, _, _) = super::super::test_support::host(cx);
        let (responses, receive) = async_channel::bounded(1);
        let (errors, receive_errors) = async_channel::bounded(1);
        host.update(cx, |host, cx| {
            Shell::watch_responses(host.send.clone(), receive, cx);
            Shell::watch_errors(host.send.clone(), receive_errors, cx);
            host.send.close();
            host.send = worker_queue::channel().0;
            host.enrolled = Some(false);
            host.error.clear();
        });
        let mut stale = WorkerResponse::new(Err("retired response".into()));
        stale.engine_lost = true;
        responses.try_send(stale).unwrap();
        errors.try_send("retired error").unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.error.is_empty());
            assert!(!host.engine_recovering);
            assert_eq!(host.enrolled, Some(false));
        });
    }

    #[gpui::test]
    fn critical_profile_startup_opens_the_profile_failure_dialog(cx: &mut gpui::TestAppContext) {
        // Terminal profile errors open their own dialog without opening the
        // management window. Only corruption permits an explicit reset.
        for error in [ipc::PROFILE_OPEN_ERROR, ipc::CORRUPT_STORE_ERROR] {
            let (host, _, _) = super::super::test_support::host(cx);
            let (responses, receive) = async_channel::bounded(1);
            host.update(cx, |host, cx| {
                host.enrolled = None;
                host.sessions_window = None;
                Shell::watch_responses(host.send.clone(), receive, cx);
            });
            let mut response = WorkerResponse::new(Err(error.into()));
            response.critical = true;
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(host.startup_prompt);
                assert!(host.sessions_window.is_none());
                assert_eq!(host.error, error);
            });
        }
    }

    #[gpui::test]
    fn command_w_keeps_shared_window_open(cx: &mut gpui::TestAppContext) {
        cx.update(crate::ui::desktop::text_editor::install_keybindings);
        cx.update(super::super::menus::install);
        let (window, _) = cx.update(|cx| {
            sign_in::open(
                crate::release_channel::ReleaseChannel::Public,
                false,
                Rc::new(|_, _| Ok(())),
                Rc::new(|_| {}),
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| window.update(cx, |_, window, _| window.activate_window()).unwrap());
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |_, window, cx| {
                    window.dispatch_action(Box::new(super::super::menus::CloseWindow), cx)
                })
                .unwrap()
        });
        cx.run_until_parked();
        assert!(cx.update(|cx| cx.windows().contains(&window.into())));
    }
    #[gpui::test]
    fn completed_local_account_can_open_sessions_without_live_replication(
        cx: &mut gpui::TestAppContext,
    ) {
        // A completed local profile remains usable after offline restart, while
        // missing enrollment, name or acknowledged recovery still requires setup.
        let (host, _, requests) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.account_profile.initialized = false;
            assert!(host.account_ready());
            host.ensure_sign_in(cx);
            assert!(host.sign_in.is_none());
            host.open_retained_session("saved-session".into(), cx);
            host.enrolled = Some(false);
            assert!(!host.account_ready());
            host.enrolled = Some(true);
            host.account_profile.unlocked = false;
            assert!(!host.account_ready());
            host.account_profile.unlocked = true;
            host.account_profile.display_name = None;
            assert!(!host.account_ready());
        });
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::PrepareSession { ref session }, .. }) if session == "saved-session"));
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn unfinished_account_opens_before_metadata_and_adapter_waits_for_readiness(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, _, requests) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: None,
                unlocked: false,
                initialized: false,
            };
            host.ensure_sign_in(cx);
        });
        cx.run_until_parked();
        let Message::Request(QueuedRequest {
            request: Request::RecoveryStatus,
            sign_in: Some(ticket),
            ..
        }) = requests.try_recv().unwrap()
        else {
            panic!("shared flow looks up persisted recovery first")
        };
        host.update(cx, |host, cx| {
            let mut response = WorkerResponse::new(Ok(()));
            response.sign_in = Some(ticket);
            response.account_response = Some(AccountResponse::Recovery(RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            }));
            host.apply_sign_in_response(&mut response, cx);
        });
        let Message::Request(QueuedRequest {
            request: Request::Status, sign_in: Some(ticket), ..
        }) = requests.try_recv().unwrap()
        else {
            panic!("shared flow requests account metadata")
        };
        host.update(cx, |host, cx| {
            let mut response = WorkerResponse::new(Ok(()));
            response.sign_in = Some(ticket);
            response.account_identity = Some(("account".into(), "fixture@example.invalid".into()));
            response.status_account_profile = Some(AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: None,
                unlocked: true,
                initialized: false,
            });
            host.apply_sign_in_response(&mut response, cx);
            let view = host.sign_in.as_ref().unwrap();
            assert_eq!(view.read(cx).flow.step, Step::Profile);
            view.update(cx, |view, cx| {
                view.profile(
                    &AccountProfile {
                        agreement_required: false,
                        replacement_required: false,
                        display_name: None,
                        unlocked: true,
                        initialized: true,
                    },
                    cx,
                )
            });
            assert_eq!(view.read(cx).flow.step, Step::Name);
        });
    }
    #[gpui::test]
    fn completion_rechecks_one_unpaused_invitation_before_review(cx: &mut gpui::TestAppContext) {
        let (host, _, requests) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.invitation = Some(crate::store::InvitationPreview {
                session: "pending-journey".into(),
                paused: true,
                retained: false,
                received: None,
            });
            host.resume_sign_in_destination(cx);
            assert!(requests.is_empty());
            host.invitation.as_mut().unwrap().paused = false;
            host.resume_sign_in_destination(cx);
            host.resume_sign_in_destination(cx);
        });
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::WatchInvitation { session }, ..
        }) if session == "pending-journey"));
        assert!(requests.is_empty());
    }

    #[gpui::test]
    fn typed_account_controls_require_readiness_and_apply_saved_name(
        cx: &mut gpui::TestAppContext,
    ) {
        let (send, _requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        host.update(cx, |host, _| {
            host.enrolled = Some(false);
            host.account_profile.display_name = Some("Old".into());
        });
        host.update(cx, |host, cx| {
            host.name_editor = Some(cx.new(|cx| text_editor::TextEditor::new("Draft", cx)));
            host.recovery_busy = Some(worker_recovery::Action::SetDisplayName);
            host.error = "Previous save failed".into();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        for name in ["Remote name", "Renamed remotely"] {
            let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
                panic!("status reply")
            };
            status.enrolled = true;
            status.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some(name.into()),
                initialized: true,
                unlocked: true,
            };
            let allocation =
                status.account_profile.display_name.as_ref().unwrap().as_ptr() as usize;
            let response = WorkerResponse::from_reply(Ok(Reply::Status(status)));
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, cx| {
                assert_eq!(
                    host.account_profile.display_name.as_ref().unwrap().as_ptr() as usize,
                    allocation
                );
                assert_eq!(host.account_profile.display_name.as_deref(), Some(name));
                assert!(host.account_profile.initialized && host.account_profile.unlocked);
                assert_eq!(host.name_editor.as_ref().unwrap().read(cx).text(), "Draft");
            });
        }
        let mut response = WorkerResponse::from_reply(Ok(Reply::DisplayName {
            display_name: "Saved name".into(),
        }));
        assert!(matches!(response.account_response, Some(AccountResponse::DisplayName(_))));
        response.recovery_action = Some(worker_recovery::Action::SetDisplayName);
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.account_profile.display_name.as_deref(), Some("Saved name"));
            assert!(host.name_editor.is_none());
            assert!(host.recovery_busy.is_none());
            assert!(host.error.is_empty());
        });
    }
}
