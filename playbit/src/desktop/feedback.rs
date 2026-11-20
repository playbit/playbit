//! Non-error feedback uses one single-line Visual Bell; errors use native dialogs.
use super::*;
use crate::ui::desktop::zoom::length as ui;

const MAX_MESSAGE_CHARS: usize = 256;
const TRUNCATION_SUFFIX: &str = "...";

pub(super) struct Bell {
    message: String,
    icon: &'static str,
    _dismiss: gpui::Task<()>,
}

impl Shell {
    pub(super) fn show_visual_bell(
        &mut self,
        message: &str,
        icon: &'static str,
        cx: &mut Context<Self>,
    ) {
        let message = bell_message(message);
        let dismiss = cx.spawn(async move |host, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
            let _ = host.update(cx, |host, cx| {
                host.visual_bell = None;
                cx.notify();
            });
        });
        // Replacing a bell cancels its expiry task; it cannot dismiss a newer notice.
        self.visual_bell = Some(Bell { message, icon, _dismiss: dismiss });
        cx.notify();
    }

    pub(super) fn present_feedback(&mut self, window: &Window, cx: &mut Context<Self>) {
        let error = visible_error(&self.error, &self.last_transfer)
            .or(window.accessibility_error())
            .unwrap_or_default()
            .to_owned();
        if error.is_empty() {
            self.alerted_error.clear();
        } else if error != self.alerted_error {
            self.alerted_error = error.clone();
            crate::ui::desktop::alert::show(
                crate::ui::desktop::alert::Kind::Recoverable,
                &error,
                cx,
            );
        }
        let update = self.selected_details().and_then(|prepared| {
            prepared.update.clone().map(|update| (prepared.session.id.clone(), update))
        });
        if update != self.update_notice {
            if let Some((_, status)) = &update {
                match status.kind {
                    crate::engine::UpdateKind::Failed => crate::ui::desktop::alert::show(
                        crate::ui::desktop::alert::Kind::Recoverable,
                        &status.message,
                        cx,
                    ),
                    crate::engine::UpdateKind::Paused => {
                        self.show_visual_bell(&status.message, "session-share", cx);
                    }
                }
            }
            self.update_notice = update;
        }
    }

    pub(super) fn render_visual_bell(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let bell = self.visual_bell.as_ref()?;
        let message = bell.message.clone();
        let icon = bell.icon;
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(ui(16.))
                .px(ui(16.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("visual-bell")
                        .debug_selector(|| "visual-bell".into())
                        .max_w_full()
                        .min_w_0()
                        .h(ui(28.))
                        .px(ui(16.))
                        .flex()
                        .items_center()
                        .gap(ui(10.))
                        .rounded(ui(14.))
                        .bg(gpui::rgba(0x000000a6))
                        .text_color(gpui::white())
                        .text_size(ui(14.))
                        .child(
                            gpui::svg()
                                .path(icon)
                                .size(ui(14.))
                                .flex_none()
                                .text_color(gpui::white()),
                        )
                        .child(
                            self.host_text(HostText::VisualBell, message, cx)
                                .debug_selector(|| "visual-bell-label".into())
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_overflow(gpui::TextOverflow::Truncate(
                                    TRUNCATION_SUFFIX.into(),
                                )),
                        ),
                ),
        )
    }
}

fn bell_message(message: &str) -> String {
    let mut chars = message.chars();
    let mut text: String = chars
        .by_ref()
        .take(MAX_MESSAGE_CHARS)
        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
        .collect();
    if chars.next().is_some() {
        for _ in 0..TRUNCATION_SUFFIX.len() {
            text.pop();
        }
        text.push_str(TRUNCATION_SUFFIX);
    }
    text
}

// Transfer errors belong to the transfer state, not the independently retained
// local operation error. Retrying a transfer must not clear another error.
fn visible_error<'a>(local: &'a str, transfer: &'a TransferStatus) -> Option<&'a str> {
    if !local.is_empty() {
        return Some(local);
    }
    (transfer.kind == TransferKind::Failed)
        .then(|| transfer.message.as_deref().unwrap_or("Transfer failed"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated native feedback state.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn update_failures_use_dialogs_and_paused_status_uses_an_expiring_bell(
        cx: &mut gpui::TestAppContext,
    ) {
        // Distinguish failed and canceled updates without changing session content layout.
        let (host, window, _) = test_support::host(cx);
        let id = crate::identifier::encode(&[0x01; 32]);
        host.update(cx, |host, cx| {
            host.sidebar_session = Some(id.clone());
            host.sessions = vec![SessionRow {
                reference: String::new(),
                id: id.clone(),
                build_state: crate::engine::builds::BuildState::Ready,
                app_name: Some("Example".into()),
                resolved_build_id: crate::identifier::encode(&[0x02; 32]),
            }];
            host.selected = Some(crate::engine::reply::PreparedSession {
                session: crate::store::Session {
                    id: id.clone(),
                    selector: crate::store::BuildSelector::Exact {
                        build_id: crate::identifier::encode(&[0x02; 32]),
                    },
                    resolved_build_id: crate::identifier::encode(&[0x02; 32]),
                    root_volume: crate::store::Id::from_bytes([1; 16]),
                    shared_volume: None,
                    app: crate::package::AppIdentity {
                        publisher: crate::store::Id::from_bytes([3; 16]),
                        app_id: "test".into(),
                    },

                    creator: crate::store::Id::from_bytes([3; 16]),
                    update_authority: crate::store::UpdateAuthority::Creator,
                },
                invitation: String::new(),
                availability: String::new(),
                update: None,
            });
            cx.notify();
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(800.), px(560.)));
        visual.run_until_parked();
        let before = visual.debug_bounds("session-content").unwrap();
        host.update(&mut visual, |host, cx| {
            host.selected.as_mut().unwrap().update = Some(crate::engine::UpdateStatus {
                kind: crate::engine::UpdateKind::Failed,
                build_id: None,
                message: "Updates unavailable. Check for updates to try again.".into(),
            });
            cx.notify();
        });
        visual.run_until_parked();
        assert_eq!(before, visual.debug_bounds("session-content").unwrap());
        assert_eq!(
            visual.update(|_, cx| crate::ui::desktop::alert::take_presented(cx)),
            vec![(
                crate::ui::desktop::alert::Kind::Recoverable,
                "Updates unavailable. Check for updates to try again.".into()
            )],
        );
        visual.executor().advance_clock(std::time::Duration::from_secs(6));
        host.update(&mut visual, |_, cx| cx.notify());
        visual.run_until_parked();
        assert!(visual.update(|_, cx| crate::ui::desktop::alert::take_presented(cx)).is_empty());
        host.update(&mut visual, |host, cx| {
            host.selected.as_mut().unwrap().update = Some(crate::engine::UpdateStatus {
                kind: crate::engine::UpdateKind::Paused,
                build_id: None,
                message: "Local update canceled; publication may already have completed".into(),
            });
            cx.notify();
        });
        visual.run_until_parked();
        assert_eq!(before, visual.debug_bounds("session-content").unwrap());
        host.read_with(&visual, |host, _| {
            assert_eq!(
                host.visual_bell.as_ref().unwrap().message,
                "Local update canceled; publication may already have completed"
            );
        });
        assert!(visual.update(|_, cx| crate::ui::desktop::alert::take_presented(cx)).is_empty());
        visual.executor().advance_clock(std::time::Duration::from_secs(6));
        visual.run_until_parked();
        host.update(&mut visual, |_, cx| cx.notify());
        visual.run_until_parked();
        assert!(host.read_with(&visual, |host, _| host.visual_bell.is_none()));
    }

    #[gpui::test]
    fn replacing_a_bell_keeps_the_new_notice_until_its_own_expiry(cx: &mut gpui::TestAppContext) {
        let (host, _, _) = test_support::host(cx);
        host.update(cx, |host, cx| host.show_visual_bell("First", "session-share", cx));
        cx.run_until_parked();
        cx.executor().advance_clock(std::time::Duration::from_secs(4));
        host.update(cx, |host, cx| host.show_visual_bell("Second", "session-share", cx));
        cx.run_until_parked();
        cx.executor().advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.visual_bell.as_ref().unwrap().message, "Second")
        });
        cx.executor().advance_clock(std::time::Duration::from_secs(4));
        cx.run_until_parked();
        assert!(host.read_with(cx, |host, _| host.visual_bell.is_none()));
    }

    #[gpui::test]
    fn recoverable_errors_use_dialogs_once_until_cleared(cx: &mut gpui::TestAppContext) {
        // Render changing, repeated and recurring failures through the native alert owner.
        let (host, _, requests) = test_support::host(cx);
        for (local, transfer, expected) in [
            (
                "Invalid invitation — café 🌱",
                "Transfer failed",
                Some("Invalid invitation — café 🌱"),
            ),
            ("Invalid invitation — café 🌱", "Transfer failed", None),
            ("Try another link", "Transfer failed", Some("Try another link")),
            ("", "Transfer failed", Some("Transfer failed")),
            ("", "Transfer failed", None),
            ("", "", None),
            ("", "Transfer failed", Some("Transfer failed")),
        ] {
            host.update(cx, |host, cx| {
                host.error = local.into();
                host.last_transfer = if transfer.is_empty() {
                    TransferStatus::default()
                } else {
                    TransferStatus {
                        kind: TransferKind::Failed,
                        message: Some(transfer.into()),
                        ..Default::default()
                    }
                };
                cx.notify();
            });
            cx.run_until_parked();
            let presented = cx.update(crate::ui::desktop::alert::take_presented);
            assert_eq!(
                presented,
                expected
                    .into_iter()
                    .map(|message| {
                        (crate::ui::desktop::alert::Kind::Recoverable, message.to_owned())
                    })
                    .collect::<Vec<_>>()
            );
        }
        assert!(requests.try_recv().is_err(), "displaying failures does not dispatch runtime work");
    }

    #[test]
    fn bell_messages_are_single_line_and_mark_admission_truncation() {
        // Preserve short Unicode text and exact boundaries while bounding multiline input.
        assert_eq!(bell_message("Copied café 🌱"), "Copied café 🌱");
        assert_eq!(
            bell_message("one\nline\r\nwith\ttabs\u{0085}and\u{2028}Unicode\u{2029}breaks"),
            "one line  with tabs and Unicode breaks"
        );
        assert_eq!(bell_message(""), "");
        let exact = "é".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(bell_message(&exact), exact);
        assert_eq!(bell_message(&format!("{exact}x")), format!("{}...", "é".repeat(253)));
        let long = bell_message(&"🌱".repeat(1000));
        assert_eq!(long.chars().count(), MAX_MESSAGE_CHARS);
        assert!(long.ends_with("..."));
        assert!(long.len() <= 1024);
    }

    struct BellFixture(Entity<Shell>);

    impl Render for BellFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let bell = self.0.update(cx, |host, cx| host.render_visual_bell(cx));
            div().size_full().relative().children(bell)
        }
    }

    #[gpui::test]
    fn bell_text_stays_on_one_line_inside_narrow_and_normal_windows(cx: &mut gpui::TestAppContext) {
        // Exercise the production overlay and its accessible text at constrained widths.
        let (host, _, _) = test_support::host(cx);
        let (_, visual) = cx.add_window_view(|_, _| BellFixture(host.clone()));
        for message in [
            "Copied café 🌱".to_owned(),
            "Copied account info ".repeat(30),
            "First line\nSecond line\tUnicode\u{2028}separator".into(),
        ] {
            host.update(visual, |host, cx| host.show_visual_bell(&message, "menu-check", cx));
            let expected = bell_message(&message);
            for width in [800., 180., 800.] {
                visual.simulate_resize(size(px(width), px(120.)));
                visual.run_until_parked();
                let bell = visual.debug_bounds("visual-bell").unwrap();
                let text = visual.debug_bounds("visual-bell-label").unwrap();
                assert_eq!(bell.size.height, px(28.));
                assert_eq!(bell.center().x, px(width / 2.));
                assert!(bell.left() >= px(16.) && bell.right() <= px(width - 16.));
                assert!(text.size.height <= bell.size.height);
                assert!(text.left() >= bell.left() && text.right() <= bell.right());
                visual.update(|window, _| {
                    let node = window
                        .accessibility_nodes_for_test()
                        .map(|(_, node)| node)
                        .find(|node| node.value() == Some(expected.as_str()))
                        .unwrap();
                    assert_eq!(node.live(), Some(gpui::accesskit::Live::Polite));
                });
            }
        }
    }

    #[test]
    fn transfer_retry_removes_only_its_own_error() {
        let failed = TransferStatus {
            kind: TransferKind::Failed,
            message: Some("AP timed out".into()),
            ..Default::default()
        };
        let complete = TransferStatus { kind: TransferKind::Complete, ..Default::default() };
        assert_eq!(super::visible_error("", &failed), Some("AP timed out"));
        assert_eq!(super::visible_error("", &complete), None);
        assert_eq!(super::visible_error("Invalid app view", &failed), Some("Invalid app view"));
        assert_eq!(super::visible_error("Invalid app view", &complete), Some("Invalid app view"));
    }
}
