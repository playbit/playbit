//! Invitation consent, joining, and app-opening progress.
use super::*;

fn invitation_description(joining: bool, state: &InvitationState) -> &'static str {
    if joining {
        "Downloading app and synchronizing shared data…"
    } else {
        match &state {
            InvitationState::WaitingRegistration => {
                "Waiting for the sender to connect and register this session. You can leave and resume later."
            }
            InvitationState::WaitingApp => {
                "Waiting for the app upload to finish. This invitation will become available automatically."
            }
            InvitationState::WaitingDatabase => "Waiting for shared data to finish synchronizing.",
            InvitationState::WaitingPeer => {
                "This data needs an authorized participant online. Try joining when a participant is available."
            }
            InvitationState::Ready { .. } => "Ready to join.",
            InvitationState::Failed { .. } => {
                "Couldn’t check this invitation. Your invitation is saved. Try again when connected."
            }
            InvitationState::Denied { .. } => {
                "Access is unavailable. Ask the sender to check the app’s provisioning and your access, then retry."
            }
            _ => "Checking invitation availability…",
        }
    }
}

impl Shell {
    pub(super) fn cancel_invitation_restoration(&mut self) {
        self.restore_pending_invitation = false;
    }

    pub(super) fn restore_startup_invitation(&mut self, cx: &mut Context<Self>) -> bool {
        // Either status or ordinary-open can arrive first. Explicit navigation
        // cancels restoration, and later status never changes the chosen page.
        if self.restore_pending_invitation
            && self.enrolled.is_some()
            && self.selected.is_none()
            && self.invitation.is_none()
            && self.opening.is_none()
            && !self.initial_link
        {
            let preview = self.saved_invitations.first().cloned();
            if preview.as_ref().is_some_and(|preview| preview.retained)
                && (!self.account_ready() || self.sign_in.is_some())
            {
                return false;
            }
            self.restore_pending_invitation = false;
            if let Some(preview) = preview.filter(|preview| preview.retained) {
                if preview.paused {
                    self.sidebar_session = Some(preview.session.clone());
                    self.opening = Some(preview.session);
                    self.joining = false;
                } else if let Err(error) = self.open_retained(preview.session, cx) {
                    self.joining = false;
                    self.error = error.into();
                }
            } else {
                self.invitation = self.saved_invitations.first().cloned();
            }
            return true;
        }
        false
    }

    fn paused_opening(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        session: String,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        main = main.child(self.host_callback(
            "resume-opening",
            "Resume opening",
            false,
            move |this, cx| {
                if this.joining {
                    return;
                }
                match this.prepare_session(session.clone(), cx) {
                    Ok(()) => {
                        this.joining = true;
                        this.error.clear();
                    }
                    Err(error) => this.error = error.into(),
                }
            },
            cx,
        ));
        main
    }

    pub(super) fn opening_content(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        session: String,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let status = if self.joining {
            "Downloading the app and restoring shared data…".into()
        } else if self.last_transfer.kind == TransferKind::Failed
            && self.last_transfer.session.as_deref() == Some(session.as_str())
        {
            self.last_transfer
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
                .map(|message| message.chars().take(512).collect::<String>())
                .unwrap_or_else(|| "Couldn’t open your app. Try again when connected.".into())
        } else {
            "Opening paused. Your app and saved data are retained.".into()
        };
        main = main
            .child(self.host_text(HostText::OpeningTitle, "Opening your app", cx).text_2xl())
            .child(self.host_text(HostText::OpeningStatus, status, cx))
            .when_some(self.session_build_status(cx), |view, status| view.child(status));
        if self.joining {
            main = main.child(self.host_callback(
                "cancel-opening",
                "Cancel for now",
                false,
                move |this, _| {
                    if let Err(error) = this.send.send(Request::CancelTransfer) {
                        this.error = error.into();
                        return;
                    }
                    if this
                        .launches
                        .get(&session)
                        .is_none_or(|progress| progress.instance.is_none())
                    {
                        this.launches.remove(&session);
                        this.pending_windows.remove(&session);
                    }
                    this.joining = false;
                },
                cx,
            ));
        } else {
            main = self.paused_opening(main, session, cx);
        }
        main
    }

    fn paused_invitation(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        main = main
            .child(self.host_text(
                HostText::InvitationStatus,
                "Invitation paused. Resume whenever you’re ready.",
                cx,
            ))
            .child(self.host_callback(
                "resume-preview",
                "Resume invitation",
                false,
                |this, _| {
                    if let Some(invitation) = &this.invitation {
                        this.send
                            .send(Request::ResumeInvitation { session: invitation.session.clone() })
                            .ok();
                    }
                },
                cx,
            ));
        main
    }

    fn join_controls(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        received: &crate::store::ReceivedInvitation,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let session = received.session.id.clone();
        main = main.child(self.host_callback(
            "join-personal-volumes",
            "Allow access to my volumes…",
            false,
            move |this, _| this.request_personal(session.clone()),
            cx,
        ));
        main = main.child(self.host_callback(
            "join-session",
            "Join session",
            false,
            |this, _| {
                if this.joining {
                    return;
                }
                if let Some(invitation) = &this.invitation {
                    this.send.send(Request::StartJoin { session: invitation.session.clone() }).ok();
                    this.joining = true;
                    this.error.clear();
                }
            },
            cx,
        ));
        main
    }

    fn pause_invitation(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        main = main.child(self.host_callback(
            "pause-invitation",
            "Cancel for now",
            false,
            |this, _| {
                if let Some(invitation) = &this.invitation {
                    this.send
                        .send(Request::PauseInvitation { session: invitation.session.clone() })
                        .ok();
                }
            },
            cx,
        ));
        main
    }

    fn available_invitation(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        invitation: crate::store::InvitationPreview,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let state = self.join_status.clone();
        let description = invitation_description(self.joining, &state);
        main = main.child(self.host_text(HostText::InvitationStatus, description, cx));
        if let InvitationState::Denied { message } | InvitationState::Failed { message } = state {
            main = main.child(self.action(
                "retry-invitation",
                "Check again",
                Request::WatchInvitation { session: invitation.session.clone() },
                cx,
            ));
            main = main.child(self.host_text(HostText::InvitationError, message, cx));
        }
        if let Some(received) = invitation.received.as_ref().filter(|_| {
            !self.joining
                && matches!(
                    self.join_status,
                    InvitationState::Ready { .. } | InvitationState::WaitingPeer
                )
        }) {
            main = self.join_controls(main, received, cx);
        }
        main = self.pause_invitation(main, cx);
        main
    }

    pub(super) fn invitation_content(
        &mut self,
        mut main: gpui::Stateful<gpui::Div>,
        invitation: crate::store::InvitationPreview,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        main = main.child(self.host_text(HostText::InvitationTitle, "Join this session", cx).text_2xl())
                        .child(self.host_text(HostText::InvitationRoot, match &invitation.received {
                            Some(received) => format!("Shared data: {}", received.root_name),
                            None => "Session details will appear when the sender makes them available.".to_owned(),
                        }, cx))
                        .child(self.host_text(HostText::InvitationAuthority, "This invitation gives the app access to its shared root and session data. Your personal volumes stay private.", cx));
        main = if invitation.paused {
            self.paused_invitation(main, cx)
        } else {
            self.available_invitation(main, invitation, cx)
        };
        main
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn startup_restores_retained_openings_and_preserves_durable_pause(
        cx: &mut gpui::TestAppContext,
    ) {
        // The first enrolled status must not turn an authorized opening into
        // consent or let its startup continuation resume a durable pause.
        for paused in [false, true] {
            let (host, window, requests, responses) = test_support::host_with_responses(cx);
            let session = crate::identifier::encode(&[7; 32]);
            let preview = crate::store::InvitationPreview {
                session: session.clone(),
                paused,
                retained: true,
                received: None,
            };
            host.update(cx, |host, _| {
                host.enrolled = None;
                host.restore_pending_invitation = true;
            });
            for _ in 0..2 {
                let mut response = WorkerResponse::new(Ok(()));
                response.status_enrolled = Some(true);
                response.saved_invitations = Some(vec![preview.clone()]);
                responses.try_send(response).unwrap();
                cx.run_until_parked();
            }
            host.read_with(cx, |host, _| {
                assert_eq!(host.opening.as_deref(), Some(session.as_str()));
                assert_eq!(host.joining, !paused);
                assert!(host.invitation.is_none());
                assert_eq!(host.host_controls.contains_key("resume-opening"), paused);
                assert_eq!(host.host_controls.contains_key("cancel-opening"), !paused);
            });
            if paused {
                assert!(requests.is_empty());
                focus_button(window, "resume-opening", cx);
                press(window, "enter", cx);
            }
            assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
                request: Request::PrepareSession { session: id }, ..
            }) if id == session));
            assert!(requests.is_empty());
        }
    }

    #[gpui::test]
    fn canceled_preparation_cannot_finish_a_newer_opening(cx: &mut gpui::TestAppContext) {
        // Return the retired request's real admission after Cancel and Resume.
        // Each kind must leave the newer admission and progress untouched.
        for kind in [TransferKind::Running, TransferKind::Failed, TransferKind::Complete] {
            let (host, window, requests, responses) = test_support::host_with_responses(cx);
            let session = crate::identifier::encode(&[8; 32]);
            host.update(cx, |host, cx| {
                host.open_retained(session.clone(), cx).unwrap();
                cx.notify();
            });
            let Message::Request(old) = requests.try_recv().unwrap() else { panic!("prepare") };
            cx.run_until_parked();
            focus_button(window, "cancel-opening", cx);
            press(window, "enter", cx);
            assert!(matches!(
                requests.try_recv().unwrap(),
                Message::Request(QueuedRequest { request: Request::CancelTransfer, .. })
            ));
            host.read_with(cx, |host, _| {
                assert!(!host.joining);
                assert!(!host.launches.contains_key(&session));
                assert!(!host.pending_windows.contains_key(&session));
            });
            responses
                .try_send(WorkerResponse::from_reply(Ok(Reply::Transfer {
                    transfer: TransferStatus {
                        kind: TransferKind::Complete,
                        session: Some(session.clone()),
                        ..Default::default()
                    },
                })))
                .unwrap();
            cx.run_until_parked();
            assert!(requests.is_empty(), "late completion cannot resume a canceled opening");
            focus_button(window, "resume-opening", cx);
            press(window, "enter", cx);
            let Message::Request(current) = requests.try_recv().unwrap() else { panic!("resume") };
            let admission = current.credit.as_ref().unwrap().admission();
            let mut response = WorkerResponse::from_reply(Ok(Reply::Transfer {
                transfer: TransferStatus {
                    kind,
                    session: Some(session.clone()),
                    ..Default::default()
                },
            }));
            response.session = Some(session.clone());
            response.credit = old.credit;
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(host.joining);
                assert_eq!(host.pending_windows.get(&session), Some(&Some(admission)));
                assert_eq!(host.launches[&session].admission, Some(admission));
                assert!(host.error.is_empty());
            });
            assert!(requests.is_empty());
        }
    }

    #[gpui::test]
    #[gpui::test]
    #[gpui::test]
    fn invitation_delivery_matches_session_and_retains_preview(cx: &mut gpui::TestAppContext) {
        let (send, requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        drop(requests.try_recv().unwrap());
        let session = String::from("selected-invitation");
        let allocation = session.as_ptr() as usize;
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::InvitationPreview(
                crate::store::InvitationPreview {
                    session,
                    paused: false,
                    retained: false,
                    received: None,
                },
            ))))
            .unwrap();
        cx.run_until_parked();
        assert!(
            matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::WatchInvitation { session }, .. }) if session == "selected-invitation")
        );
        host.read_with(cx, |host, _| {
            assert_eq!(host.invitation.as_ref().unwrap().session.as_ptr() as usize, allocation)
        });
        responses
            .try_send(WorkerResponse::from_reply(Ok(Reply::InvitationStatus {
                invitation_status: Some(InvitationStatus {
                    session: "another-invitation".into(),
                    state: InvitationState::Paused,
                }),
            })))
            .unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.join_status, InvitationState::Checking);
            assert!(!host.invitation.as_ref().unwrap().paused);
        });
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        for state in [InvitationState::Paused, InvitationState::Ready { build_id: "build".into() }]
        {
            let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
                panic!("status reply")
            };
            status.enrolled = true;
            status.invitation_status = Some(InvitationStatus {
                session: "selected-invitation".into(),
                state: state.clone(),
            });
            let response = WorkerResponse::from_reply(Ok(Reply::Status(status)));
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert_eq!(host.join_status, state);
                assert_eq!(
                    host.invitation.as_ref().unwrap().paused,
                    matches!(state, InvitationState::Paused)
                );
            });
        }
    }

    #[gpui::test]
    fn invitation_controls_join_pause_and_resume_the_same_session(cx: &mut gpui::TestAppContext) {
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let invitation = crate::store::InvitationPreview {
            session: "shared-session".into(),
            paused: false,
            retained: false,
            received: Some(crate::store::ReceivedInvitation {
                session: crate::store::Session {
                    id: "shared-session".into(),
                    selector: crate::store::BuildSelector::Exact { build_id: "build".into() },
                    resolved_build_id: "build".into(),
                    root_volume: crate::store::Id::from_bytes([1; 16]),
                    shared_volume: None,
                    app: crate::package::AppIdentity {
                        publisher: crate::store::Id::from_bytes([3; 16]),
                        app_id: "test".into(),
                    },

                    creator: crate::store::Id::from_bytes([3; 16]),
                    update_authority: crate::store::UpdateAuthority::Creator,
                },
                root_name: "Shared tasks".into(),
            }),
        };
        let status = |preview: crate::store::InvitationPreview| {
            let mut response = WorkerResponse::new(Ok(()));
            response.status_enrolled = Some(true);
            response.saved_invitations = Some(vec![preview]);
            response.invitation_status = Some(Some(InvitationStatus {
                session: "shared-session".into(),
                state: InvitationState::Ready { build_id: "fixture".into() },
            }));
            response
        };
        host.update(cx, |host, cx| {
            host.invitation = Some(invitation.clone());
            host.invitation.as_mut().unwrap().received = None;
            // A delayed initial preview can arrive after status cached its envelope.
            host.saved_invitations = vec![invitation.clone()];
            host.join_status = InvitationState::Ready { build_id: "fixture".into() };
            host.invitation_status = Some(InvitationStatus {
                session: invitation.session.clone(),
                state: host.join_status.clone(),
            });
            cx.notify();
        });
        cx.run_until_parked();
        host.read_with(cx, |host, _| assert!(!host.host_controls.contains_key("join-session")));
        responses.try_send(status(invitation.clone())).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.invitation.as_ref().unwrap().received, invitation.received);
            assert!(!host.joining, "receiving metadata does not consent to joining");
        });
        assert!(requests.is_empty());
        let mut stale = invitation.clone();
        stale.received = None;
        responses.try_send(status(stale)).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.invitation.as_ref().unwrap().received, invitation.received);
        });
        focus_button(window, "join-session", cx);
        press(window, "enter", cx);
        press(window, "enter", cx);
        let queued = requests.try_recv().unwrap();
        assert!(matches!(queued, Message::Request(QueuedRequest {
            request: Request::StartJoin { ref session }, .. }) if session == "shared-session"));
        drop(queued);
        assert!(requests.try_recv().is_err());
        // Joining retires the engine's readiness watch before preparation opens
        // the session. Status must not restart that completed invitation owner.
        let mut joining = status(invitation.clone());
        joining.invitation_status = Some(None);
        responses.try_send(joining).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| assert!(host.joining));
        assert!(requests.is_empty(), "joining owns progress until preparation completes");
        focus_button(window, "pause-invitation", cx);
        press(window, "space", cx);
        let queued = requests.try_recv().unwrap();
        assert!(matches!(queued, Message::Request(QueuedRequest {
            request: Request::PauseInvitation { ref session }, .. }) if session == "shared-session"));
        drop(queued);
        host.update(cx, |host, cx| {
            assert_eq!(host.invitation, Some(invitation.clone()));
            host.invitation.as_mut().unwrap().paused = true;
            host.joining = false;
            cx.notify();
        });
        cx.run_until_parked();
        press(window, "tab", cx);
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    assert!(
                        host.navigation_controls["join"].read(cx).focused(window),
                        "Tab must work after the focused Pause control disappears"
                    );
                })
                .unwrap()
        });
        focus_button(window, "resume-preview", cx);
        press(window, "enter", cx);
        let queued = requests.try_recv().unwrap();
        assert!(matches!(queued, Message::Request(QueuedRequest {
            request: Request::ResumeInvitation { ref session }, .. }) if session == "shared-session"));
        drop(queued);

        host.update(cx, |host, cx| {
            host.invitation = Some(crate::store::InvitationPreview {
                session: "another-invitation".into(),
                paused: false,
                retained: false,
                received: None,
            });
            cx.notify();
        });
        responses.try_send(status(invitation.clone())).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            let current = host.invitation.as_ref().unwrap();
            assert_eq!(current.session, "another-invitation");
            assert!(current.received.is_none(), "old status cannot replace a new preview");
        });
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    host.navigation_controls["join"].read(cx).focus(window)
                })
                .unwrap();
        });
        press(window, "enter", cx);
        responses.try_send(status(invitation)).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.invitation.is_none(), "late metadata cannot undo explicit navigation");
        });
    }

    #[gpui::test]
    fn opening_controls_preserve_session_on_retry_and_cancel(cx: &mut gpui::TestAppContext) {
        let (host, window, requests) = host(cx);
        host.update(cx, |host, cx| {
            host.opening = Some("saved-session".into());
            host.joining = false;
            cx.notify();
        });
        cx.run_until_parked();
        focus_button(window, "resume-opening", cx);
        press(window, "enter", cx);
        press(window, "enter", cx);
        let queued = requests.try_recv().unwrap();
        assert!(matches!(queued, Message::Request(QueuedRequest {
            request: Request::PrepareSession { ref session }, .. }) if session == "saved-session"));
        drop(queued);
        assert!(requests.try_recv().is_err(), "repeated activation must not start a second run");
        focus_button(window, "cancel-opening", cx);
        press(window, "space", cx);
        let queued = requests.try_recv().unwrap();
        assert!(matches!(
            queued,
            Message::Request(QueuedRequest { request: Request::CancelTransfer, .. })
        ));
        host.read_with(cx, |host, _| {
            assert!(!host.joining);
            assert_eq!(host.opening.as_deref(), Some("saved-session"));
        });
    }
}
