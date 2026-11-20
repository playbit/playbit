//! Session invitation entry and modal keyboard focus.
use super::*;
use crate::ui::desktop::zoom::length as ui;
use gpui::Focusable;

impl Shell {
    pub(super) fn open_join_dialog(&mut self, cx: &mut Context<Self>) {
        if self.join_dialog.is_some() {
            return;
        }
        let host = cx.weak_entity();
        let channel = self.channel;
        let editor = cx.new(|cx| {
            let mut editor = text_editor::TextEditor::new("", cx)
                .literal()
                .placeholder("Session ID or link")
                .bound_edits(256)
                .on_commit(Arc::new(move |text, _, cx| {
                    if crate::store::parse_session_link(text.trim(), channel).is_err() {
                        return false;
                    }
                    let host = host.clone();
                    cx.defer(move |cx| {
                        let _ = host.update(cx, |host, cx| host.submit_join_dialog(cx));
                    });
                    true
                }));
            editor.account_dialog();
            editor
        });
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        self.join_dialog = Some(editor);
        self.join_dialog_focus = true;
        cx.notify();
    }
    fn close_join_dialog(&mut self, cx: &mut Context<Self>) {
        self.join_dialog = None;
        if let Some(handle) = self.sessions_window {
            cx.defer(move |cx| {
                let _ = handle.update(cx, |host, window, _| {
                    window.focus(&host.focus);
                    #[cfg(all(target_os = "macos", not(test)))]
                    crate::ui::desktop::appearance::set_content_modal(window, false);
                    window.refresh();
                });
            });
        }
        cx.notify();
    }
    fn submit_join_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.join_dialog else {
            return;
        };
        let invitation = editor.read(cx).text().trim().to_owned();
        if crate::store::parse_session_link(&invitation, self.channel).is_err() {
            return;
        }
        if let Err(error) = self.send.send(Request::PreviewInvitation { invitation }) {
            self.error = error.into();
            cx.notify();
            return;
        }
        self.close_join_dialog(cx);
    }
    pub(super) fn join_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let editor = self.join_dialog.clone()?;
        if std::mem::take(&mut self.join_dialog_focus) {
            window.activate_window();
            window.focus(&editor.focus_handle(cx));
            #[cfg(all(target_os = "macos", not(test)))]
            crate::ui::desktop::appearance::set_content_modal(window, true);
            let host = cx.weak_entity();
            window.on_window_should_close(cx, move |_, cx| {
                host.upgrade().is_none_or(|host| host.read(cx).join_dialog.is_none())
            });
            window.refresh();
        }
        let parsed = crate::store::parse_session_link(editor.read(cx).text().trim(), self.channel);
        let valid = parsed.is_ok();
        let validation = parsed
            .err()
            .filter(|_| !editor.read(cx).text().trim().is_empty())
            .map(|error| error.to_string());
        let cancel = self.host_callback(
            "cancel-join-dialog",
            "Cancel",
            false,
            |host, cx| host.close_join_dialog(cx),
            cx,
        );
        let join = self.host_callback(
            "submit-join-dialog",
            "Join",
            !valid,
            |host, cx| host.submit_join_dialog(cx),
            cx,
        );
        join.update(cx, |control, cx| {
            control.set(
                controls::Properties {
                    kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Primary),
                    label: "Join",
                    disabled: !valid,
                    theme: &theme(),
                },
                cx,
            )
        });
        let panel = div()
            .w(ui(323.))
            .min_h(ui(104.))
            .mb(ui(84.))
            .rounded(ui(16.))
            .bg(rgb(0xececec))
            .flex()
            .flex_col()
            .shadow(vec![
                gpui::BoxShadow {
                    color: rgba(0x000000cc).into(),
                    offset: gpui::point(
                        crate::ui::desktop::zoom::scaled(window, 0.),
                        crate::ui::desktop::zoom::scaled(window, 0.),
                    ),
                    blur_radius: crate::ui::desktop::zoom::scaled(window, 0.417),
                    spread_radius: crate::ui::desktop::zoom::scaled(window, 0.),
                },
                gpui::BoxShadow {
                    color: rgba(0x0000004d).into(),
                    offset: gpui::point(
                        crate::ui::desktop::zoom::scaled(window, 0.),
                        crate::ui::desktop::zoom::scaled(window, 18.),
                    ),
                    blur_radius: crate::ui::desktop::zoom::scaled(window, 22.5),
                    spread_radius: crate::ui::desktop::zoom::scaled(window, 0.),
                },
            ])
            .child(
                div()
                    .h(ui(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(ui(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0x555555))
                    .child(self.host_text(HostText::JoinDialogTitle, "Join Session", cx)),
            )
            .child(div().h(ui(24.)).flex_none().px(ui(12.)).child(editor))
            .when_some(validation, |panel, message| {
                panel.child(
                    div()
                        .px(ui(12.))
                        .pt(ui(8.))
                        .text_size(ui(12.))
                        .text_color(rgb(0x9f351b))
                        .child(message),
                )
            })
            .child(
                div()
                    .h(ui(48.))
                    .flex_none()
                    .flex()
                    .items_end()
                    .justify_end()
                    .p(ui(12.))
                    .gap(ui(8.))
                    .child(cancel)
                    .child(join),
            );
        Some(
            div()
                .id("join-session-modal")
                .absolute()
                .inset_0()
                .occlude()
                .bg(rgba(0x00000033))
                .font_family(UI_FONT_FAMILY)
                .flex()
                .items_center()
                .justify_center()
                .capture_key_down(cx.listener(|host, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => host.close_join_dialog(cx),
                        "tab" => {
                            if event.keystroke.modifiers.shift {
                                window.focus_prev();
                            } else {
                                window.focus_next();
                            }
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .child(panel)
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
            assert!(host.read_with(cx, |host, _| host.join_dialog.is_none()), "{input}");
            let Message::Request(QueuedRequest {
                request: Request::PreviewInvitation { invitation },
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
            assert!(requests.try_recv().is_err());
        }
    }
    #[gpui::test]
    fn join_dialog_validates_and_submits_once_and_escape_cancels(cx: &mut gpui::TestAppContext) {
        // Exercise the modal's actual controls and keep background controls inert.
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
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
        assert!(host.read_with(cx, |host, _| host.join_dialog.is_none()));
        assert!(
            matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest { request: Request::PreviewInvitation { invitation }, .. }) if invitation == link)
        );
        assert!(requests.try_recv().is_err());
        host.update(cx, |host, cx| host.open_join_dialog(cx));
        cx.run_until_parked();
        test_support::press(window, "escape", cx);
        assert!(host.read_with(cx, |host, _| host.join_dialog.is_none()));
        assert!(requests.try_recv().is_err());
    }
}
