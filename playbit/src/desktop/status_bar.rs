//! A passive status overlay showing current work.
use super::*;
use crate::engine::reply::TransferProgress;
use crate::ui::desktop::zoom;
use playbit_api::ui::{Theme, ThemeColor as Role};

const LABEL_BYTES_MAX: usize = 512;

#[derive(Debug, PartialEq, Eq)]
struct Status {
    icon: Option<&'static str>,
    label: String,
}

fn label(text: &str) -> String {
    let mut end = text.len().min(LABEL_BYTES_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn status(
    engine_recovering: bool,
    sending_crash_reports: bool,
    transfer: &TransferStatus,
) -> Option<Status> {
    let (icon, text) = if engine_recovering {
        (Some("status-plug"), "Restarting Engine…")
    } else if sending_crash_reports {
        (None, "Sending Crash Report…")
    } else {
        match transfer.kind {
            TransferKind::Failed => (Some("status-sync"), "Synchronization failed"),
            TransferKind::Canceling => (Some("status-sync"), "Canceling…"),
            TransferKind::Running => {
                return Some(match &transfer.progress {
                    Some(TransferProgress::Connecting { hostname }) => Status {
                        icon: Some("status-connection"),
                        label: label(&format!("Connecting to {}", label(hostname))),
                    },
                    Some(TransferProgress::Volumes { current, total }) => Status {
                        icon: Some("status-sync"),
                        label: format!("{current}/{total} volumes"),
                    },
                    _ => Status {
                        icon: Some("status-sync"),
                        label: label(transfer.message.as_deref().unwrap_or("Synchronizing…")),
                    },
                });
            }
            TransferKind::Idle | TransferKind::Complete | TransferKind::Paused => return None,
        }
    };
    Some(Status { icon, label: label(text) })
}

impl Shell {
    pub(super) fn status_bar(
        &mut self,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let status = status(
            self.engine_recovering,
            *self.sending_crash_reports.read(cx),
            &self.last_transfer,
        )?;
        let sp = |n| zoom::pixels(window, n);
        let foreground = gpui::Hsla::from(rgba(theme.color(Role::Text).0)).alpha(0.8);
        let text = self.host_text(HostText::StatusBar, status.label, cx);
        Some(
            div().absolute().bottom(sp(8.)).right(sp(8.)).left(sp(8.)).flex().justify_end().child(
                div()
                    .h(sp(24.))
                    .debug_selector(|| "status-pill".into())
                    .max_w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .px(sp(8.))
                    .gap(sp(7.))
                    .rounded(sp(8.))
                    .bg(rgb(0xdfdfdf))
                    .shadow(vec![gpui::BoxShadow {
                        color: gpui::Hsla::from(rgba(theme.color(Role::Shadow).0)).alpha(1.),
                        offset: gpui::point(px(0.), px(0.)),
                        // GPUI takes Gaussian sigma: Figma's one-point blur maps to 0.5.
                        // Scale continuously so zoom does not quantize the shadow width.
                        blur_radius: zoom::scaled(window, 0.5),
                        spread_radius: px(0.),
                    }])
                    .font_family(UI_FONT_FAMILY)
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(sp(11.))
                    .line_height(sp(20.))
                    .text_color(foreground)
                    .children(status.icon.map(|icon| {
                        div()
                            .size(sp(16.))
                            .flex_none()
                            .debug_selector(|| "status-icon-slot".into())
                            .child(gpui::svg().path(icon).size(sp(16.)).text_color(foreground))
                    }))
                    .child(text.min_w_0().truncate().debug_selector(|| "status-label".into())),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_tracks_work_without_claiming_completion() {
        // Exercise priority and terminal states with the same retained transfer.
        let mut transfer = TransferStatus::default();
        for kind in [TransferKind::Idle, TransferKind::Complete, TransferKind::Paused] {
            transfer.kind = kind;
            assert_eq!(status(false, false, &transfer), None);
        }
        transfer.kind = TransferKind::Running;
        transfer.progress = Some(TransferProgress::Volumes { current: 4, total: 4 });
        assert_eq!(status(false, false, &transfer).expect("running").label, "4/4 volumes");
        transfer.progress = Some(TransferProgress::Connecting { hostname: "ap.test".into() });
        let connecting = status(false, false, &transfer).expect("connecting");
        assert_eq!(connecting.icon, Some("status-connection"));
        assert_eq!(connecting.label, "Connecting to ap.test");
        transfer.kind = TransferKind::Canceling;
        assert_eq!(status(false, false, &transfer).expect("canceling").label, "Canceling…");
        transfer.kind = TransferKind::Failed;
        assert_eq!(
            status(false, false, &transfer).expect("failed").label,
            "Synchronization failed"
        );
        assert_eq!(status(true, false, &transfer).expect("restart").icon, Some("status-plug"));
        assert_eq!(
            status(false, true, &transfer),
            Some(Status { icon: None, label: "Sending Crash Report…".into() }),
        );
        assert_eq!(status(true, true, &transfer).expect("restart").icon, Some("status-plug"));
        transfer.kind = TransferKind::Running;
        transfer.progress = None;
        transfer.message = Some("Preparing app…".into());
        assert_eq!(status(false, false, &transfer).expect("preparing").label, "Preparing app…");
    }

    #[test]
    fn displayed_state_has_a_bounded_utf8_label() {
        // A long external error or hostname never becomes unbounded retained UI text.
        let text = label(&"🌊".repeat(1000));
        assert!(text.capacity() + std::mem::size_of::<Status>() < 1024);
        assert_eq!(text.len(), LABEL_BYTES_MAX);
    }

    #[test]
    fn warnings_belong_to_the_transfer_and_other_stages_keep_their_messages() {
        let mut transfer = TransferStatus { kind: TransferKind::Failed, ..Default::default() };
        let warning = status(false, false, &transfer);
        assert_eq!(status(false, false, &transfer), warning);
        transfer.kind = TransferKind::Paused;
        assert!(status(false, false, &transfer).is_none());
        transfer.kind = TransferKind::Running;
        for progress in [
            TransferProgress::Account,
            TransferProgress::Registrations { current: 1, total: 2 },
            TransferProgress::Sessions { current: 1, total: 2 },
            TransferProgress::Files { current: 1, total: 2 },
            TransferProgress::Finishing,
        ] {
            let message = progress.message();
            transfer.message = Some(message.clone());
            transfer.progress = Some(progress);
            assert_eq!(status(false, false, &transfer).expect("running").label, message);
        }
    }

    #[gpui::test]
    fn original_icons_decode_nonempty_in_gpui(cx: &mut gpui::TestAppContext) {
        // Validate the bundled exports through the actual SVG rasterizer.
        use gpui::AssetSource;
        let assets = controls::Assets;

        for name in ["status-sync", "status-plug", "status-connection"] {
            let bytes = assets.load(name).expect("asset source").expect("bundled status icon");
            let image = cx.update(|cx| {
                gpui::Image::from_bytes(gpui::ImageFormat::Svg, bytes.into_owned())
                    .to_image_data(cx.svg_renderer())
                    .expect("original SVG must decode")
            });
            assert_eq!(image.size(0), size(gpui::DevicePixels(16), gpui::DevicePixels(16)));
            assert!(
                image
                    .as_bytes(0)
                    .expect("frame")
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] > 0),
                "{name} decoded empty"
            );
        }
    }

    #[gpui::test]
    fn status_is_a_stable_live_region_and_hides_after_work(cx: &mut gpui::TestAppContext) {
        // Inspect the real shell's accessibility tree across activity and warning transitions.
        let (host, window, requests) = test_support::host(cx);
        let nodes = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |_, window, _| {
                    window
                        .accessibility_nodes_for_test()
                        .map(|(id, node)| (id, node.clone()))
                        .collect::<Vec<_>>()
                })
                .expect("shell window")
        };
        let mut identity = None;
        for (sending_crash_reports, kind, progress, expected) in [
            (
                false,
                TransferKind::Running,
                Some(TransferProgress::Volumes { current: 3, total: 4 }),
                "3/4 volumes",
            ),
            (
                false,
                TransferKind::Running,
                Some(TransferProgress::Connecting { hostname: "ap.test".into() }),
                "Connecting to ap.test",
            ),
            (true, TransferKind::Failed, None, "Sending Crash Report…"),
            (false, TransferKind::Failed, None, "Synchronization failed"),
            (true, TransferKind::Idle, None, "Sending Crash Report…"),
        ] {
            host.update(cx, |host, cx| {
                host.sending_crash_reports = cx.new(|_| sending_crash_reports);
                host.last_transfer = TransferStatus { kind, progress, ..Default::default() };
                cx.notify();
            });
            cx.run_until_parked();
            let tree = nodes(cx);
            let (id, node) = tree
                .iter()
                .find(|(_, node)| node.value() == Some(expected))
                .expect("visible status");
            assert_eq!(node.live(), Some(gpui::accesskit::Live::Polite));
            assert!(!node.supports_action(gpui::accesskit::Action::Focus));
            assert!(!node.supports_action(gpui::accesskit::Action::Click));
            if let Some(identity) = identity {
                assert_eq!(*id, identity);
            }
            identity = Some(*id);
        }
        host.update(cx, |host, cx| {
            host.sending_crash_reports = cx.new(|_| false);
            host.last_transfer =
                TransferStatus { kind: TransferKind::Complete, ..Default::default() };
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!nodes(cx).iter().any(|(id, _)| Some(*id) == identity));
        assert!(requests.try_recv().is_err(), "status rendering starts no worker request");
    }

    struct OverlayFixture {
        host: Entity<Shell>,
        clicked: usize,
        focus: gpui::FocusHandle,
    }

    impl Render for OverlayFixture {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let status =
                self.host.update(cx, |host, cx| host.status_bar(window, &Theme::light(), cx));
            div()
                .size_full()
                .relative()
                .track_focus(&self.focus)
                .child(
                    div()
                        .id("underlay")
                        .size_full()
                        .debug_selector(|| "status-underlay".into())
                        .on_click(cx.listener(|this, _, _, _| this.clicked += 1)),
                )
                .children(status)
        }
    }

    #[gpui::test]
    fn overlay_is_inset_bounded_and_passes_pointer_input(cx: &mut gpui::TestAppContext) {
        // Exercise the production pill over a clickable surface, including narrow viewports.
        let (host, _, _) = test_support::host(cx);
        host.update(cx, |host, _| {
            host.last_transfer = TransferStatus {
                kind: TransferKind::Running,
                message: Some("A long activity message ".repeat(10)),
                ..Default::default()
            };
        });
        let (fixture, visual) = cx.add_window_view(|_, cx| OverlayFixture {
            host,
            clicked: 0,
            focus: cx.focus_handle(),
        });
        visual.run_until_parked();
        for width in [670., 180.] {
            visual.simulate_resize(size(px(width), px(120.)));
            visual.run_until_parked();
            let pill = visual.debug_bounds("status-pill").expect("pill bounds");
            let underlay = visual.debug_bounds("status-underlay").expect("underlay bounds");
            let slot = visual.debug_bounds("status-icon-slot").expect("glyph slot");
            assert_eq!(underlay.size, size(px(width), px(120.)));
            assert_eq!(pill.size.height, px(24.));
            assert_eq!(pill.bottom_right(), gpui::point(px(width - 8.), px(112.)));
            assert!(pill.origin.x >= px(8.));
            assert_eq!(slot.size, size(px(16.), px(16.)));
            visual.simulate_click(pill.center(), Default::default());
        }
        assert_eq!(fixture.read_with(visual, |fixture, _| fixture.clicked), 2);
    }
}
