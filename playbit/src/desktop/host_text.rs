//! Accessible read-only text for desktop views.
use super::*;
use crate::ui::desktop::text_accessibility_node;

#[derive(Clone, Copy)]
pub(super) enum Text {
    StatusBar,
    SidebarTitle,
    ReleaseChannel,
    ChannelSwitch,
    JoinDialogTitle,
    RecentApps,
    Templates,
    NameDialogTitle,
    NameDialogLabel,
    EmptySessions,
    VolumeCapacity,
    ArtworkError,
    PanelTitle,
    OpeningTitle,
    OpeningStatus,
    BuildStatus,
    LogNotice,
    InvitationTitle,
    InvitationRoot,
    InvitationAuthority,
    InvitationStatus,
    InvitationError,
    VisualBell,
    ConsentTitle,
    ConsentInstructions,
    ConsentHint,
    ConsentEmpty,
}
pub(super) const COUNT: usize = Text::ConsentEmpty as usize + 1;
const _: () = assert!(COUNT < 64);

pub(super) fn element(
    text: SharedString,
    id: gpui::accesskit::NodeId,
    heading: bool,
    live: bool,
) -> gpui::Div {
    div().relative().child(text.clone()).child(
        gpui::canvas(
            move |bounds, window, _| {
                if text.is_empty() {
                    return;
                }
                let mut node = text_accessibility_node(&text, heading, bounds);
                if live {
                    node.set_live(gpui::accesskit::Live::Polite);
                }
                window.insert_accessibility_node(id, node, false, None);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full(),
    )
}

impl Shell {
    pub(super) fn host_text(
        &mut self,
        key: Text,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let index = key as usize;
        self.rendered_host_text |= 1 << index;
        let identity = self.host_text_ids[index].get_or_insert_with(|| cx.new(|_| ()));
        let id = text_editor::accessibility_text_ids(identity.entity_id()).0;
        let heading = matches!(
            key,
            Text::NameDialogTitle
                | Text::OpeningTitle
                | Text::InvitationTitle
                | Text::ConsentTitle
                | Text::JoinDialogTitle
                | Text::RecentApps
                | Text::Templates
        );
        let live = matches!(
            key,
            Text::StatusBar
                | Text::VisualBell
                | Text::OpeningStatus
                | Text::BuildStatus
                | Text::LogNotice
                | Text::InvitationRoot
                | Text::InvitationStatus
                | Text::InvitationError
                | Text::ArtworkError
        );
        element(text.into(), id, heading, live)
    }

    pub(super) fn retire_host_text(&mut self) {
        for (index, identity) in self.host_text_ids.iter_mut().enumerate() {
            if self.rendered_host_text & (1 << index) == 0 {
                *identity = None;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use gpui::accesskit::{Action, Live, Role};

    fn snapshot(
        window: gpui::WindowHandle<Shell>,
        cx: &mut gpui::TestAppContext,
    ) -> Vec<(gpui::accesskit::NodeId, gpui::accesskit::Node)> {
        cx.update(|cx| {
            window
                .update(cx, |_, window, _| {
                    let nodes: Vec<_> = window
                        .accessibility_nodes_for_test()
                        .map(|(id, node)| (id, node.clone()))
                        .collect();
                    let ids: std::collections::BTreeSet<_> =
                        nodes.iter().map(|(id, _)| *id).collect();
                    assert_eq!(
                        ids.len(),
                        nodes.len(),
                        "each visible element needs a unique identity"
                    );
                    assert!(window.accessibility_error().is_none());
                    nodes
                })
                .unwrap()
        })
    }

    #[gpui::test]
    fn startup_sync_progress_is_live_and_does_not_claim_recovery_is_running(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.recovery = None;
            host.recovery_busy = None;
            host.last_transfer = TransferStatus {
                kind: TransferKind::Running,
                message: Some("Syncing volume 12 of 20…".into()),
                ..Default::default()
            };
            cx.notify();
        });
        cx.run_until_parked();
        let nodes = snapshot(window, cx);
        let (identity, progress) =
            nodes.iter().find(|(_, n)| n.value() == Some("Syncing volume 12 of 20…")).unwrap();
        assert_eq!(progress.live(), Some(Live::Polite));
        host.update(cx, |host, cx| {
            host.last_transfer =
                TransferStatus { kind: TransferKind::Complete, ..Default::default() };
            host.recovery = Some(crate::engine::reply::RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!snapshot(window, cx).iter().any(|(id, _)| id == identity));
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn build_failure_stays_readable_while_opening_and_paused(cx: &mut gpui::TestAppContext) {
        let (host, window, _) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.sessions = vec![SessionRow {
                reference: String::new(),
                id: "saved".into(),
                resolved_build_id: crate::identifier::encode(&[0xab; 32]),
                build_state: crate::engine::builds::BuildState::DownloadFailed {
                    error: "blob service temporarily unavailable".into(),
                },
                app_name: Some("Notes".into()),
            }];
            host.sidebar_session = Some("saved".into());
            cx.notify();
        });
        for opening in [None, Some(true), Some(false)] {
            host.update(cx, |host, cx| {
                host.opening = opening.map(|_| "saved".into());
                host.joining = opening.unwrap_or(false);
                cx.notify();
            });
            cx.run_until_parked();
            let nodes = snapshot(window, cx);
            let (_, notice) = nodes
                .iter()
                .find(|(_, node)| {
                    node.value()
                        == Some("Build download failed: blob service temporarily unavailable")
                })
                .unwrap();
            assert_eq!(notice.live(), Some(Live::Polite));
        }
    }

    #[gpui::test]
    fn recent_app_reopens_its_most_recent_session(cx: &mut gpui::TestAppContext) {
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.sessions = ["older", "newer"]
                .into_iter()
                .map(|id| SessionRow {
                    reference: String::new(),
                    id: id.into(),
                    resolved_build_id: crate::identifier::encode(&[0xab; 32]),
                    build_state: crate::engine::builds::BuildState::Ready,
                    app_name: Some("Example".into()),
                })
                .collect();
            host.recent_sessions = vec!["newer".into(), "older".into()];
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            snapshot(window, cx).iter().filter(|(_, n)| n.label() == Some("Example")).count(),
            1
        );
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    host.navigation_controls["recent-0"].read(cx).focus(window);
                })
                .unwrap()
        });
        test_support::press(window, "enter", cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::PrepareSession { session }, ..
        }) if session == "newer"));
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn templates_create_a_private_root_only_on_activation(cx: &mut gpui::TestAppContext) {
        let (host, window, requests) = test_support::host(cx);

        cx.run_until_parked();
        let nodes = snapshot(window, cx);

        assert!(nodes.iter().any(|(_, n)| n.label() == Some("Templates")));
        assert!(nodes.iter().any(|(_, n)| n.label() == Some("Try a TODO app")));

        assert!(requests.try_recv().is_err(), "showing examples must not fetch or join anything");
        test_support::focus_button(window, "example-open-1", cx);
        test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err(), "folder permissions are reviewed before launch");
        host.read_with(cx, |host, _| assert!(host.new_session.is_some()));
        assert!(snapshot(window, cx).iter().any(|(_, node)| node.label() == Some("Open session")));
    }

    #[gpui::test]
    fn initial_engine_failure_never_presents_signed_out_or_empty_account(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests) = test_support::host(cx);
        for error in ["", crate::engine::ipc::INCOMPATIBLE_ENGINE] {
            host.update(cx, |host, cx| {
                host.enrolled = None;
                host.error = error.into();
                // Even a pending recovery navigation must not imply signed out.
                cx.notify();
            });
            cx.run_until_parked();
            let nodes = snapshot(window, cx);
            for text in ["Sign in to Playbit", "No saved sessions", "Back"] {
                assert!(
                    !nodes.iter().any(|(_, n)| n.label() == Some(text) || n.value() == Some(text)),
                    "{text}"
                );
            }
            assert!(!nodes.iter().any(|(_, n)| n.label() == Some("Account & recovery")));
            assert!(!nodes.iter().any(|(_, n)| n.label() == Some("Open invitation")));
        }
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn invitation_status_is_stable_read_only_and_retires_with_its_screen(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::engine::reply::InvitationState;
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.invitation = Some(
                serde_json::from_value(serde_json::json!({
                    "session":"shared-session", "paused":false, "retained":true, "received":null
                }))
                .unwrap(),
            );
            cx.notify();
        });
        let mut identity = None;
        for (state, text) in [
            (InvitationState::WaitingRegistration, "Waiting for the sender"),
            (InvitationState::WaitingApp, "Waiting for the app upload"),
            (InvitationState::WaitingDatabase, "Waiting for shared data"),
            (InvitationState::WaitingPeer, "authorized participant online"),
            (
                InvitationState::Failed { message: "Details — café 🌱".into() },
                "Couldn’t check this invitation",
            ),
            (
                InvitationState::Denied { message: "Details — café 🌱".into() },
                "Access is unavailable",
            ),
            (InvitationState::Ready { build_id: "fixture".into() }, "Ready to join."),
            (InvitationState::Paused, "Invitation paused."),
        ] {
            host.update(cx, |host, cx| {
                host.invitation.as_mut().unwrap().paused = matches!(state, InvitationState::Paused);
                host.join_status = state;
                cx.notify();
            });
            cx.run_until_parked();
            let nodes = snapshot(window, cx);
            let (id, node) =
                nodes.iter().find(|(_, n)| n.value().is_some_and(|v| v.contains(text))).unwrap();
            if let Some(previous) = identity {
                assert_eq!(*id, previous);
            }
            identity = Some(*id);
            assert_eq!(node.live(), Some(Live::Polite));
            assert!(node.is_read_only());
            assert!(
                !node.supports_action(Action::Focus) && !node.supports_action(Action::SetValue)
            );
            assert!(
                nodes
                    .iter()
                    .any(|(_, n)| n.role() == Role::Heading
                        && n.label() == Some("Join this session"))
            );
            assert!(nodes.iter().any(|(_, n)| {
                n.value().is_some_and(|v| v.contains("personal volumes stay private"))
            }));
        }
        host.update(cx, |host, cx| {
            host.invitation = None;
            cx.notify();
        });
        cx.run_until_parked();
        let nodes = snapshot(window, cx);
        assert!(!nodes.iter().any(|(id, _)| Some(*id) == identity));
        assert!(host.read_with(cx, |host, _| {
            host.host_text_ids[Text::InvitationStatus as usize].is_none()
        }));
        assert!(nodes.iter().any(|(_, n)| n.value() == Some("New session")));
        assert!(requests.try_recv().is_err(), "projecting status does not trigger network work");
    }
}
