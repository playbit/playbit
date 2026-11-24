//! Session invitation entry and modal keyboard focus.
use super::*;
use crate::ui::desktop::zoom::length as ui;
impl Shell {
    pub(super) fn open_join_dialog(&mut self, cx: &mut Context<Self>) {
        if !self.session_dialog_open(cx) {
            self.leave_invitation();
            self.open_join_entry("", cx);
        }
    }

    pub(super) fn open_join_entry(&mut self, input: &str, cx: &mut Context<Self>) {
        if self.join_dialog.is_some() {
            return;
        }
        let host = cx.weak_entity();
        let channel = self.channel;
        let editor = cx.new(|cx| {
            let editor_id = cx.entity_id();
            let mut editor = text_editor::TextEditor::new(input, cx)
                .literal()
                .tabular_numerals()
                .placeholder("Session ID or link")
                .bound_edits(256)
                .on_commit(Arc::new(move |text, _, cx| {
                    let input = text.trim();
                    if !crate::identifier::valid(input, 32)
                        && crate::store::parse_session_link(input, channel).is_err()
                    {
                        return false;
                    }
                    let host = host.clone();
                    cx.defer(move |cx| {
                        let _ = host.update(cx, |host, cx| {
                            if host
                                .join_dialog
                                .as_ref()
                                .is_some_and(|editor| editor.entity_id() == editor_id)
                            {
                                host.submit_join_dialog(cx);
                            }
                        });
                    });
                    true
                }));
            editor.account_dialog();
            editor
        });
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        self.join_dialog = Some(editor);
        self.join_preview = None;
        self.cancel_invitation_restoration();
        self.join_status = InvitationState::Checking;
        cx.notify();
    }
    pub(super) fn waiting_for_invitation(&self) -> bool {
        self.invitation.is_some()
            && matches!(
                self.join_status,
                InvitationState::WaitingRegistration
                    | InvitationState::WaitingApp
                    | InvitationState::WaitingDatabase
                    | InvitationState::WaitingPeer
                    | InvitationState::SignIn
            )
    }

    pub(super) fn dismiss_join_dialog(&mut self, cx: &mut Context<Self>) {
        // Releasing the view allows the engine's saved invitation and readiness watch
        // to continue. Later status updates can no longer advance this modal.
        self.leave_invitation();
        self.join_preview = None;
        self.join_dialog = None;
        cx.notify();
    }

    pub(super) fn cancel_join_dialog(&mut self, cx: &mut Context<Self>) {
        let session = self
            .invitation
            .as_ref()
            .map(|preview| preview.session.as_str())
            .or_else(|| self.invitation_link.as_ref().map(|link| link.internal_id()));
        if let Some(session) = session
            && let Err(error) =
                self.send.send(Request::PauseInvitation { session: session.to_owned() })
        {
            self.error = error.into();
            cx.notify();
            return;
        }
        if self.invitation.is_some() {
            self.dismiss_invitation(cx);
            cx.notify();
        } else {
            self.dismiss_join_dialog(cx);
        }
    }

    fn submit_join_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.join_dialog else { return };
        if self.join_preview.is_some() || self.invitation_resume.is_some() {
            return;
        }
        if let Some(preview) = &self.invitation {
            let request = if preview.paused {
                Request::ResumeInvitation { session: preview.session.clone() }
            } else if matches!(
                self.join_status,
                InvitationState::Failed { .. } | InvitationState::Denied { .. }
            ) {
                Request::WatchInvitation { session: preview.session.clone() }
            } else {
                return;
            };
            self.lookup_join_dialog(request);
        } else {
            let invitation = editor.read(cx).text().trim();
            if self.sessions.iter().any(|row| row.id == invitation)
                && crate::identifier::valid(invitation, 32)
            {
                let id = invitation.to_owned();
                self.join_dialog = None;
                if let Err(error) = self.open_retained(id, cx) {
                    self.error = error.into();
                }
            } else if let Ok(reference) =
                crate::store::SessionReference::from_link(invitation, self.channel)
            {
                if let Err(error) = self.preview_invitation_link(reference) {
                    self.join_status = InvitationState::Failed { message: error.into() };
                } else {
                    self.join_status = InvitationState::Checking;
                }
            }
        }
        cx.notify();
    }

    pub(super) fn lookup_join_dialog(&mut self, request: Request) {
        match self.send.join_dialog_request(request) {
            Ok(admission) => {
                self.join_preview = Some(admission);
                self.join_status = InvitationState::Checking;
            }
            Err(error) => self.join_status = InvitationState::Failed { message: error.into() },
        }
    }

    pub(super) fn receive_invitation_admission(
        &mut self,
        response: &WorkerResponse,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((origin, admission)) = response
            .credit
            .as_ref()
            .and_then(|credit| Some((credit.invitation?, credit.admission())))
        else {
            return true;
        };
        if origin == worker_queue::InvitationRequest::Saved {
            return self.invitation_resume == Some(admission);
        }
        if self.join_preview != Some(admission) {
            return false;
        }
        self.join_preview = None;
        if origin == worker_queue::InvitationRequest::Preview {
            return self.join_dialog.is_none() && self.new_session.is_none();
        }
        if let Err(error) = &response.result {
            if self.joining
                && let Some(form) = &self.new_session
            {
                self.joining = false;
                form.update(cx, |form, cx| form.join_failed(error.clone(), cx));
            } else {
                self.join_status = InvitationState::Failed { message: error.clone() };
            }
            cx.notify();
            return false;
        }
        true
    }

    pub(super) fn join_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui::{AnimationExt, Focusable};
        let editor = self.join_dialog.clone()?;
        let preview = self.invitation.as_ref();
        let fetching = self.join_preview.is_some() || preview.is_some();
        if fetching
            && self
                .host_controls
                .get("submit-join-dialog")
                .is_some_and(|control| control.read(cx).focused(window))
        {
            window.focus(&editor.focus_handle(cx));
        }
        let mut retry = None;
        let message = match &self.join_status {
            InvitationState::Failed { message } | InvitationState::Denied { message } => {
                retry = Some("Try Again");
                Some(message.clone())
            }
            InvitationState::Paused if preview.is_some() => {
                retry = Some("Resume");
                Some("Invitation paused.".into())
            }
            _ if self.waiting_for_invitation() => Some(
                super::invitation_view::invitation_description(false, &self.join_status).to_owned(),
            ),
            _ => None,
        };
        let input = editor.read(cx).text().trim();
        let validation = if preview.is_some() {
            None
        } else if crate::identifier::valid(input, 32) {
            (!self.sessions.iter().any(|row| row.id == input))
                .then(|| "Ask for an invitation link or code to join this session.".to_owned())
        } else {
            crate::store::parse_session_link(input, self.channel)
                .err()
                .map(|error| error.to_string())
        };
        let valid = !input.is_empty() && validation.is_none();
        let message = message.or_else(|| validation.filter(|_| !input.is_empty()));
        editor.update(cx, |editor, cx| editor.set_read_only(fetching, &theme(), cx));
        let panel = session_dialog::panel(window)
            .child(session_dialog::title(self.host_text(
                HostText::JoinDialogTitle,
                "Join Session",
                cx,
            )))
            .child(div().flex_none().h(ui(24.)).px(ui(16.)).child(editor));
        let mut footer = div().flex_none().flex().items_center().gap(ui(8.)).p(ui(16.));
        if fetching && retry.is_none() && message.is_none() {
            let spinner = gpui::svg()
                .path("progress-spinner")
                .size(ui(16.))
                .text_color(rgb(0x000000))
                .with_animation(
                    "fetch-session-progress",
                    gpui::Animation::new(std::time::Duration::from_secs(1)).repeat(),
                    |svg, progress| {
                        svg.with_transformation(gpui::Transformation::rotate(gpui::radians(
                            progress * std::f32::consts::TAU,
                        )))
                    },
                );
            footer = footer
                .h(ui(56.))
                .justify_center()
                .text_size(ui(11.))
                .child(spinner)
                .child(self.host_text(HostText::InvitationStatus, "Fetching session info", cx));
        } else {
            let cancel = self.host_callback(
                "cancel-join-dialog",
                "Cancel",
                false,
                Self::cancel_join_dialog,
                cx,
            );
            footer = footer.justify_end().child(cancel);
            if self.waiting_for_invitation() {
                footer = footer.child(self.host_callback(
                    "dismiss-join-dialog",
                    "Dismiss",
                    false,
                    Self::dismiss_join_dialog,
                    cx,
                ));
            } else if !fetching || retry.is_some() {
                let title = retry.unwrap_or("Join Session…");
                let join = self.host_callback(
                    "submit-join-dialog",
                    title,
                    !valid,
                    Self::submit_join_dialog,
                    cx,
                );
                join.update(cx, |control, cx| {
                    control.set(
                        controls::Properties {
                            kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Primary),
                            label: title,
                            disabled: !valid,
                            theme: &theme(),
                        },
                        cx,
                    )
                });
                footer = footer.child(join);
            }
        }
        Some(
            panel
                .when_some(message, |panel, text| {
                    panel.child(
                        div().px(ui(16.)).pt(ui(8.)).text_size(ui(11.)).child(self.host_text(
                            HostText::InvitationError,
                            text,
                            cx,
                        )),
                    )
                })
                .child(footer)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated modal state and admitted requests.")]
mod tests {
    use super::*;
    #[gpui::test]
    fn join_dialog_accepts_codes_and_partial_links(cx: &mut gpui::TestAppContext) {
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
        let code = "JVcihzXUGu6NzGEUjSgyNp";
        let seed = crate::store::parse_session_code(code).unwrap();
        for prefix in ["https://playbit.app/s#", "s#", "playbit:session:public:", ""] {
            let input = format!("{prefix}{code}");
            host.update(cx, |host, cx| {
                host.open_join_dialog(cx);
                host.join_dialog
                    .as_ref()
                    .unwrap()
                    .update(cx, |editor, cx| editor.acknowledge(None, &input, cx));
            });
            cx.run_until_parked();
            test_support::press(window, "enter", cx);
            cx.run_until_parked();
            assert!(host.read_with(cx, |host, _| host.join_preview.is_some()), "{input}");
            let Message::Request(QueuedRequest {
                request: Request::PreviewDesktopInvitation { invitation },
                ..
            }) = requests.try_recv().unwrap()
            else {
                panic!("expected invitation preview")
            };
            assert_eq!(
                crate::store::parse_session_link(
                    &invitation,
                    crate::release_channel::ReleaseChannel::Public
                )
                .unwrap(),
                seed
            );
            test_support::press(window, "escape", cx);
            assert!(matches!(
                requests.try_recv().unwrap(),
                Message::Request(QueuedRequest { request: Request::PauseInvitation { .. }, .. })
            ));
            assert!(requests.try_recv().is_err());
        }
    }
    #[gpui::test]
    fn join_dialog_validates_and_submits_once_and_escape_cancels(cx: &mut gpui::TestAppContext) {
        // Exercise the modal's actual controls and keep background controls inert.
        cx.update(text_editor::install_keybindings);
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        host.update(cx, |host, cx| host.open_join_dialog(cx));
        cx.run_until_parked();
        test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err());
        let link =
            crate::store::session_link(&[7; 16], crate::release_channel::ReleaseChannel::Public);
        host.update(cx, |host, cx| {
            host.join_dialog
                .as_ref()
                .unwrap()
                .update(cx, |editor, cx| editor.acknowledge(None, &link, cx));
            cx.notify();
        });
        cx.run_until_parked();
        test_support::press(window, "enter", cx);
        cx.run_until_parked();
        let Message::Request(QueuedRequest {
            request: Request::PreviewDesktopInvitation { invitation },
            credit,
            ..
        }) = requests.try_recv().unwrap()
        else {
            panic!("expected preview")
        };
        assert_eq!(invitation, link);
        test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err());
        test_support::press(window, "escape", cx);
        let session = crate::store::session_id(&[7; 16]).unwrap();
        assert!(
            matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::PauseInvitation { session: paused }, .. }) if paused == session)
        );
        host.update(cx, |host, cx| host.open_join_dialog(cx));
        let mut delayed = WorkerResponse::from_reply(Ok(Reply::InvitationPreview(
            crate::store::InvitationPreview {
                session,
                paused: false,
                retained: false,
                received: None,
            },
        )));
        delayed.credit = credit;
        responses.try_send(delayed).unwrap();
        cx.run_until_parked();
        assert!(host.read_with(cx, |host, _| host.join_dialog.is_some()
            && host.invitation.is_none()
            && host.join_preview.is_none()));
        test_support::press(window, "escape", cx);
        assert!(host.read_with(cx, |host, _| host.join_dialog.is_none()));
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn fetched_invitation_requires_start_and_owns_its_transfer(cx: &mut gpui::TestAppContext) {
        // Drive the same modal by Return twice. A superseded snapshot and a
        // canceled transfer must never dispatch a guest with another form's draft.
        cx.update(text_editor::install_keybindings);
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let code = "JVcihzXUGu6NzGEUjSgyNp";
        let id =
            crate::store::session_id(&crate::store::parse_session_code(code).unwrap()).unwrap();
        let app = crate::package::AppIdentity {
            app_id: "tasks".into(),
            publisher: crate::store::Id::from_bytes([3; 16]),
        };
        let build = crate::identifier::encode(&[4; 32]);
        let preview = crate::store::InvitationPreview {
            session: id.clone(),
            paused: false,
            retained: false,
            received: Some(crate::store::ReceivedInvitation {
                root_name: "Household".into(),
                session: crate::store::Session {
                    id: id.clone(),
                    app: app.clone(),
                    resolved_build_id: build.clone(),
                    selector: crate::store::BuildSelector::Exact { build_id: build.clone() },
                    root_volume: crate::store::Id::from_bytes([1; 16]),
                    shared_volume: None,
                    creator: app.publisher,
                    update_authority: crate::store::UpdateAuthority::Creator,
                },
            }),
        };
        for cancel in [true, false] {
            host.update(cx, |host, cx| {
                host.apply_imported_app_catalog(vec![crate::store::ImportedApp {
                    app: app.clone(),
                    build_id: build.clone(),
                    name: "Tasks".into(),
                }]);
                host.open_join_dialog(cx);
                host.join_dialog
                    .as_ref()
                    .unwrap()
                    .update(cx, |editor, cx| editor.acknowledge(None, code, cx));
            });
            cx.run_until_parked();
            test_support::press(window, "enter", cx);
            let Message::Request(QueuedRequest {
                request: Request::PreviewDesktopInvitation { .. },
                credit,
                ..
            }) = requests.try_recv().unwrap()
            else {
                panic!("expected preview")
            };
            let mut reply =
                WorkerResponse::from_reply(Ok(Reply::InvitationPreview(preview.clone())));
            reply.credit = credit;
            responses.try_send(reply).unwrap();
            cx.run_until_parked();
            let Message::Request(QueuedRequest {
                request: Request::WatchInvitation { .. },
                credit,
                ..
            }) = requests.try_recv().unwrap()
            else {
                panic!("expected watch")
            };
            let mut reply = WorkerResponse::from_reply(Ok(Reply::InvitationStatus {
                invitation_status: Some(InvitationStatus {
                    session: id.clone(),
                    state: InvitationState::Ready { build_id: build.clone() },
                }),
            }));
            reply.credit = credit;
            responses.try_send(reply).unwrap();
            cx.run_until_parked();
            assert!(host.read_with(cx, |host, _| host.join_dialog.is_none()
                && host.new_session.is_some()
                && !host.joining));
            assert!(requests.is_empty(), "preview does not grant access");
            test_support::press(window, "enter", cx);
            let Message::Request(QueuedRequest {
                request: Request::StartJoin { session },
                credit,
                ..
            }) = requests.try_recv().unwrap()
            else {
                panic!("expected StartJoin")
            };
            assert_eq!(session, id);
            let transfer = |kind| Reply::Transfer {
                transfer: TransferStatus { kind, session: Some(id.clone()), ..Default::default() },
            };
            responses
                .try_send(WorkerResponse::from_reply(Ok(transfer(TransferKind::Complete))))
                .unwrap();
            cx.run_until_parked();
            assert!(requests.is_empty(), "a snapshot before our StartJoin reply cannot finish it");
            let mut started = WorkerResponse::from_reply(Ok(transfer(TransferKind::Running)));
            started.credit = credit;
            responses.try_send(started).unwrap();
            cx.run_until_parked();
            if cancel {
                test_support::press(window, "escape", cx);
                assert!(
                    matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::PauseInvitation { session }, .. }) if session == id)
                );
            }
            responses
                .try_send(WorkerResponse::from_reply(Ok(transfer(TransferKind::Complete))))
                .unwrap();
            cx.run_until_parked();
            if !cancel {
                assert!(matches!(requests.try_recv().unwrap(), Message::Launch(_)));
            }
            assert!(requests.is_empty());
        }
    }

    #[gpui::test]
    fn waiting_dialog_can_dismiss_without_canceling_the_invitation(cx: &mut gpui::TestAppContext) {
        // Reopen the same saved invitation after dismissal. Its background state
        // may advance while the launcher is visible without consenting to launch.
        cx.update(text_editor::install_keybindings);
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let code = "JVcihzXUGu6NzGEUjSgyNp";
        let id =
            crate::store::session_id(&crate::store::parse_session_code(code).unwrap()).unwrap();
        let preview = crate::store::InvitationPreview {
            session: id.clone(),
            paused: false,
            retained: false,
            received: None,
        };
        let link = crate::store::SessionReference::from_link(
            code,
            host.read_with(cx, |host, _| host.channel),
        )
        .unwrap()
        .link(host.read_with(cx, |host, _| host.channel));
        for action in ["dismiss-join-dialog", "escape", "cancel-join-dialog"] {
            host.update(cx, |host, cx| {
                host.open_join_dialog(cx);
                host.join_dialog
                    .as_ref()
                    .unwrap()
                    .update(cx, |editor, cx| editor.acknowledge(None, code, cx));
            });
            cx.run_until_parked();
            test_support::press(window, "enter", cx);
            let Message::Request(queued) = requests.try_recv().unwrap() else {
                panic!("expected preview")
            };
            assert!(
                matches!(queued.request, Request::PreviewDesktopInvitation { invitation } if invitation == link)
            );
            let mut reply =
                WorkerResponse::from_reply(Ok(Reply::InvitationPreview(preview.clone())));
            reply.credit = queued.credit;
            responses.try_send(reply).unwrap();
            cx.run_until_parked();
            let Message::Request(queued) = requests.try_recv().unwrap() else {
                panic!("expected watch")
            };
            assert!(
                matches!(queued.request, Request::WatchInvitation { session } if session == id)
            );
            let status = |state| {
                WorkerResponse::from_reply(Ok(Reply::InvitationStatus {
                    invitation_status: Some(InvitationStatus { session: id.clone(), state }),
                }))
            };
            let mut waiting = status(InvitationState::WaitingRegistration);
            waiting.credit = queued.credit;
            responses.try_send(waiting).unwrap();
            cx.run_until_parked();
            window
                .update(cx, |_, window, _| {
                    let bounds = |label| {
                        window
                            .accessibility_nodes_for_test()
                            .find(|(_, node)| node.label() == Some(label))
                            .unwrap()
                            .1
                            .bounds()
                            .unwrap()
                    };
                    assert!(bounds("Cancel").x1 < bounds("Dismiss").x0);
                })
                .unwrap();
            if action == "escape" {
                test_support::press(window, "escape", cx);
            } else {
                test_support::focus_button(window, action, cx);
                test_support::press(window, "space", cx);
            }
            if action == "cancel-join-dialog" {
                assert!(
                    matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::PauseInvitation { session }, .. }) if session == id)
                );
            }
            assert!(requests.is_empty());
            responses
                .try_send(status(InvitationState::Ready {
                    build_id: crate::identifier::encode(&[1; 32]),
                }))
                .unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(
                    host.join_dialog.is_none()
                        && host.invitation.is_none()
                        && host.new_session.is_none()
                        && !host.joining
                );
            });
            window
                .update(cx, |_, window, _| {
                    assert!(
                        window
                            .accessibility_nodes_for_test()
                            .any(|(_, node)| node.label() == Some("Join Session…"))
                    )
                })
                .unwrap();
            assert!(requests.is_empty());
        }
    }

    #[gpui::test]
    fn public_session_id_reopens_only_a_retained_session(cx: &mut gpui::TestAppContext) {
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
        let id = crate::identifier::encode(&[5; 32]);
        host.update(cx, |host, cx| {
            host.open_join_dialog(cx);
            host.join_dialog
                .as_ref()
                .unwrap()
                .update(cx, |editor, cx| editor.acknowledge(None, &id, cx));
        });
        cx.run_until_parked();
        test_support::press(window, "enter", cx);
        assert!(requests.is_empty());
        host.update(cx, |host, cx| {
            host.sessions.push(SessionRow {
                id: id.clone(),
                reference: String::new(),
                app_name: Some("Tasks".into()),
                resolved_build_id: crate::identifier::encode(&[8; 32]),
                build_state: crate::engine::builds::BuildState::Ready,
            });
            cx.notify();
        });
        cx.run_until_parked();
        test_support::press(window, "enter", cx);
        assert!(
            matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::PrepareSession { session }, .. }) if session == id)
        );
        assert!(host.read_with(cx, |host, _| host.join_dialog.is_none()));
    }
}
