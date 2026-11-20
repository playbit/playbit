//! Account menu commands and account-name editing.
use super::*;
use crate::runtime::MenuEntry;
use crate::ui::desktop::zoom::length as ui;
use gpui::Focusable;
use playbit_api::ui::MenuIcon;

impl Shell {
    pub(super) fn edit_display_name(&mut self, cx: &mut Context<Self>) {
        let name = self.account_profile.display_name.clone().unwrap_or_default();
        let host = cx.weak_entity();
        let editor = cx.new(|cx| {
            text_editor::TextEditor::new(&name, cx).on_commit(Arc::new(move |text, _, cx| {
                if !account_form::display_name(text) {
                    return false;
                }
                let name = text.trim().to_owned();
                let host = host.clone();
                // Submission disables this editor. Release its key-handler lease first.
                cx.defer(move |cx| {
                    let _ = host.update(cx, |this, cx| {
                        this.send_action(Request::SetDisplayName { name }, cx);
                    });
                });
                true
            }))
        });
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        self.name_editor = Some(editor);
    }

    pub(super) fn account_button(&mut self, cx: &mut Context<Self>) -> Entity<controls::Control> {
        let theme = theme();
        let properties = controls::Properties {
            kind: controls::Kind::Popup,
            label: "Account",
            disabled: false,
            theme: &theme,
        };
        let weak = cx.weak_entity();
        let activate: controls::Activate = Rc::new(move |action, cx| {
            let Some(host) = weak.upgrade() else { return };
            match action {
                1 => host.update(cx, |host, cx| host.copy_account_info(cx)),
                2 => host.update(cx, |host, cx| host.open_name_dialog(cx)),
                3 => cx.defer(move |cx| Self::confirm_sign_out(&host, cx)),
                4 => cx.open_url("https://playbit.app/docs/"),
                5 => cx.open_url("https://discord.com/invite/tkxnBD9tJ7"),
                6 => cx.defer(|cx| {
                    if super::update::ready(cx) {
                        cx.dispatch_action(&super::update::Upgrade);
                    }
                }),
                7 | 8 => host.update(cx, |host, cx| {
                    let channel = if action == 8 {
                        crate::release_channel::ReleaseChannel::Testing
                    } else {
                        crate::release_channel::ReleaseChannel::Public
                    };
                    if let Err(error) = host.switch_channel(channel, cx) {
                        host.error = error;
                        cx.notify();
                    }
                }),
                9 => host.update(cx, |host, cx| {
                    let enabled = !host.ap_status.read(cx).work_offline();
                    host.send_action(Request::WorkOffline { enabled }, cx);
                }),
                _ => {}
            }
        });
        if !self.navigation_controls.contains_key("account") {
            let control = cx.new(|cx| controls::Control::new(0, properties, activate.clone(), cx));
            cx.observe(&control, |_, _, cx| cx.notify()).detach();
            self.navigation_controls.insert("account".into(), control);
        }
        let control = self.navigation_controls["account"].clone();
        let name = self.account_profile.display_name.as_deref().unwrap_or("Account");
        let initial = name.chars().next().unwrap_or('A').to_uppercase().to_string();
        let update_ready = super::update::ready(cx);
        let item = |id, label: &str, icon: MenuIcon, disabled| MenuEntry::Action {
            icon: Some(icon.into()),
            id,
            label: label.into(),
            checked: None,
            disabled,
            destructive: false,
        };
        let mut items = vec![
            item(0, name, MenuIcon::Avatar, true),
            item(2, "Change Name...", MenuIcon::Signature, false),
            item(1, "Copy Account Info", MenuIcon::Copy, self.account_id.is_empty()),
            item(3, "Sign Out...", MenuIcon::ExitRight, false),
            MenuEntry::Action {
                id: 9,
                icon: None,
                label: "Work Offline".into(),
                checked: Some(self.ap_status.read(cx).work_offline()),
                disabled: self.channel_switching || !self.ap_status.read(cx).known(),
                destructive: false,
            },
            MenuEntry::Separator,
            MenuEntry::Submenu {
                label: "Environment".into(),
                icon: None,
                disabled: self.channel_switching || self.channel.is_personal(),
                items: [
                    crate::release_channel::ReleaseChannel::Public,
                    crate::release_channel::ReleaseChannel::Testing,
                ]
                .into_iter()
                .enumerate()
                .map(|(index, channel)| MenuEntry::Action {
                    id: 7 + index as u32,
                    icon: None,
                    label: channel.label().into(),
                    checked: Some(self.channel == channel),
                    disabled: false,
                    destructive: false,
                })
                .collect(),
            },
            MenuEntry::Separator,
            item(4, "Documentation", MenuIcon::OpenBook, false),
            item(5, "Feedback in Discord", MenuIcon::Talk, false),
        ];
        if update_ready {
            items.push(MenuEntry::Action {
                icon: None,
                id: 6,
                label: "Update Playbit".into(),
                checked: None,
                disabled: false,
                destructive: false,
            });
        }
        control.update(cx, |control, cx| {
            control.set(properties, cx);
            control.set_activation(activate);
            control.set_sidebar_icon(
                if update_ready {
                    controls::SidebarIcon::Update
                } else {
                    controls::SidebarIcon::Account(initial.into())
                },
                false,
                false,
                cx,
            );
            control.set_popup_items(&items, cx);
            control.set_popup_update_action(update_ready.then_some(6), cx);
        });
        control
    }

    fn copy_account_info(&mut self, cx: &mut Context<Self>) {
        if self.account_id.is_empty() {
            return;
        }
        let channel = self.channel;
        let json = serde_json::json!({"account_id": self.account_id,
            "channel": channel.name(), "build": crate::VERSION,
            "ap_hostname": self.ap_status.read(cx).hostname().unwrap_or(channel.ap_hostname())})
        .to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(json));
        self.show_visual_bell("Account info copied to clipboard", "menu-check", cx);
    }

    pub(super) fn open_name_dialog(&mut self, cx: &mut Context<Self>) {
        self.edit_display_name(cx);
        if let Some(editor) = &self.name_editor {
            editor.update(cx, |editor, _| editor.account_dialog());
        }
        self.account_name_open = true;
        self.account_name_focus = true;
        cx.notify();
    }

    fn dismiss_name_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.account_name_open = false;
        self.name_editor = None;
        window.focus(&self.focus);
        #[cfg(all(target_os = "macos", not(test)))]
        crate::ui::desktop::appearance::set_content_modal(window, false);
        window.refresh();
        cx.notify();
    }

    fn name_dialog_buttons(&mut self, valid: bool, cx: &mut Context<Self>) -> gpui::Div {
        let cancel = self.host_callback(
            "cancel-account-name",
            "Cancel",
            false,
            |this, cx| {
                this.account_name_open = false;
                this.name_editor = None;
                if let Some(window) = this.sessions_window {
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |host, window, _| {
                            window.focus(&host.focus);
                            #[cfg(all(target_os = "macos", not(test)))]
                            crate::ui::desktop::appearance::set_content_modal(window, false);
                            window.refresh();
                        });
                    });
                }
            },
            cx,
        );
        let disabled = !valid || self.recovery_busy.is_some();
        let save = self.host_callback(
            "save-account-name",
            "Save",
            disabled,
            |this, cx| {
                if let Some(editor) = &this.name_editor {
                    let name = editor.read(cx).text().trim().to_owned();
                    this.send_action(Request::SetDisplayName { name }, cx);
                }
            },
            cx,
        );
        let theme = theme();
        save.update(cx, |control, cx| {
            control.set(
                controls::Properties {
                    kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Primary),
                    label: "Save",
                    disabled,
                    theme: &theme,
                },
                cx,
            )
        });
        div().h(ui(48.)).flex_none().relative().child(
            div()
                .absolute()
                .bottom(ui(12.))
                .right(ui(12.))
                .flex()
                .gap(ui(8.))
                .child(cancel)
                .child(save),
        )
    }

    pub(super) fn name_dialog_keys(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.dismiss_name_dialog(window, cx);
        } else if event.keystroke.key == "tab" {
            if event.keystroke.modifiers.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
        } else {
            return;
        }
        cx.stop_propagation();
    }

    pub(super) fn account_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.account_name_open {
            return None;
        }
        let editor = self.name_editor.clone()?;
        if std::mem::take(&mut self.account_name_focus) {
            window.activate_window();
            window.focus(&editor.focus_handle(cx));
            #[cfg(all(target_os = "macos", not(test)))]
            crate::ui::desktop::appearance::set_content_modal(window, true);
            let host = cx.weak_entity();
            window.on_window_should_close(cx, move |_, cx| {
                host.upgrade().is_none_or(|host| !host.read(cx).account_name_open)
            });
            window.refresh();
            editor.update(cx, |editor, cx| editor.select_all(cx));
        }
        editor.update(cx, |editor, _| editor.set_enabled(self.recovery_busy.is_none()));
        let valid = account_form::display_name(editor.read(cx).text());
        let buttons = self.name_dialog_buttons(valid, cx);
        let panel = div()
            .w(ui(323.))
            .h(ui(104.))
            .mb(ui(48.))
            .rounded(ui(16.))
            .bg(gpui::rgb(0xececec))
            .flex()
            .flex_col()
            // GPUI uses Gaussian sigma. Match Figma's rasterized 54 pt / 1 pt
            // blurs, including the finite falloff of its blur filter.
            .shadow(vec![
                gpui::BoxShadow {
                    color: gpui::rgba(0x000000cc).into(),
                    offset: gpui::point(
                        crate::ui::desktop::zoom::scaled(window, 0.),
                        crate::ui::desktop::zoom::scaled(window, 0.),
                    ),
                    blur_radius: crate::ui::desktop::zoom::scaled(window, 0.417),
                    spread_radius: crate::ui::desktop::zoom::scaled(window, 0.),
                },
                gpui::BoxShadow {
                    color: gpui::rgba(0x0000004d).into(),
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
                    .pl(ui(2.))
                    .text_size(ui(14.))
                    .line_height(ui(24.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(0x555555))
                    .child(
                        div()
                            .relative()
                            .child(
                                div()
                                    .absolute()
                                    .top(ui(1.))
                                    .text_color(gpui::white())
                                    .child("Change Display Name"),
                            )
                            .child(self.host_text(
                                HostText::NameDialogTitle,
                                "Change Display Name",
                                cx,
                            )),
                    ),
            )
            .child(
                div()
                    .h(ui(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(ui(12.))
                    .gap(ui(8.))
                    .child(
                        div()
                            .text_size(ui(11.))
                            .line_height(ui(16.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .letter_spacing(crate::ui::desktop::zoom::scaled(window, 0.11))
                            .text_color(gpui::rgba(0x000000e6))
                            .child(self.host_text(HostText::NameDialogLabel, "Name:", cx)),
                    )
                    .child(div().flex_1().min_w_0().mx(ui(1.)).child(editor)),
            )
            .child(buttons);
        Some(
            div()
                .id("account-name-modal")
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::rgba(0xababab99))
                .font_family(UI_FONT_FAMILY)
                .flex()
                .items_center()
                .justify_center()
                .capture_key_down(cx.listener(Self::name_dialog_keys))
                .child(panel)
                .into_any_element(),
        )
    }

    pub(super) fn with_account_modal(
        &mut self,
        background: gpui::Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let overlay = self.join_overlay(window, cx).or_else(|| self.account_overlay(window, cx));
        self.retire_host_text();
        let Some(overlay) = overlay else {
            return background.into_any_element();
        };
        div()
            .size_full()
            .relative()
            .child(gpui::inert(background))
            .child(overlay)
            .into_any_element()
    }
}

// Editing commands stay within the focused modal field; shell commands are blocked.
pub(super) fn dialog_action(action: &dyn gpui::Action) -> bool {
    action.name().starts_with("archipelago_text_editor::")
        || action.name().starts_with("playbit_zoom::")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests inspect rendered modal input routes.")]
mod tests {
    use super::*;

    #[gpui::test]
    fn work_offline_menu_reflects_engine_intent_and_submits_the_opposite_mode(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, _, requests) = test_support::host(cx);
        for (state, checked) in [
            (crate::engine::reply::ApConnectionState::Live, false),
            (crate::engine::reply::ApConnectionState::OfflineMode, true),
        ] {
            let activate = host.update(cx, |host, cx| {
                host.ap_status.update(cx, |indicator, cx| {
                    indicator.set(
                        Some(crate::engine::reply::ApConnectionStatus {
                            state,
                            hostname: "configured.ap.test".into(),
                        }),
                        cx,
                    )
                });
                let control = host.account_button(cx);
                let (items, _, activate) = control.read(cx).popup_for_test();
                assert!(items.iter().any(|item| matches!(item, MenuEntry::Action {
                    id: 9, checked: Some(value), disabled: false, ..
                } if *value == checked)));
                activate
            });
            while requests.try_recv().is_ok() {}
            cx.update(|cx| activate(9, cx));
            assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
                request: Request::WorkOffline { enabled }, ..
            }) if enabled != checked));
        }
    }

    #[gpui::test]
    fn modal_isolates_pointer_shortcuts_and_focus_until_dismissed(cx: &mut gpui::TestAppContext) {
        let (host, window, requests) = test_support::host(cx);
        cx.update(menus::install);
        cx.update(text_editor::install_keybindings);
        host.update(cx, |host, cx| host.open_name_dialog(cx));
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        // The plus button and footer sit behind the scrim, including right-click/scroll.
        for point in [gpui::point(px(217.), px(22.)), gpui::point(px(52.), px(540.))] {
            visual.simulate_click(point, Default::default());
            visual.simulate_mouse_down(point, gpui::MouseButton::Right, Default::default());
            visual.simulate_mouse_up(point, gpui::MouseButton::Right, Default::default());
        }
        visual.simulate_keystrokes("cmd-w cmd-+ cmd-alt-1");
        cx.run_until_parked();
        assert!(requests.try_recv().is_err());
        host.read_with(cx, |host, _| {
            assert!(host.account_name_open);
            assert!(!host.sidebar_volumes);
            assert!(host.browser.is_none());
        });
        cx.update(|cx| assert_eq!(crate::ui::desktop::zoom::factor(cx), 1.25));
        test_support::check_zoom_geometry(window.into(), cx);
        for index in 0..6 {
            test_support::press(window, "tab", cx);
            window
                .update(cx, |host, window, cx| {
                    assert!(match index % 3 {
                        0 => host.host_controls["cancel-account-name"].read(cx).focused(window),
                        1 => host.host_controls["save-account-name"].read(cx).focused(window),
                        _ => host.name_editor.as_ref().unwrap().focus_handle(cx).is_focused(window),
                    });
                })
                .unwrap();
        }
        test_support::press(window, "escape", cx);
        host.read_with(cx, |host, _| assert!(!host.account_name_open));
        window
            .update(cx, |_, window, _| {
                assert!(
                    window
                        .accessibility_nodes_for_test()
                        .any(|(_, node)| node.label() == Some("Account"))
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn visual_bell_is_centered_in_the_window(cx: &mut gpui::TestAppContext) {
        let (host, window, _) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.account_id = "account-id".into();
            host.copy_account_info(cx);
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        for width in [800., 1200.] {
            visual.simulate_resize(size(px(width), px(560.)));
            visual.run_until_parked();
            let bell = visual.debug_bounds("visual-bell").expect("visible visual bell");
            assert_eq!(bell.center().x, px(width / 2.));
            assert_eq!(bell.bottom_right().y, px(544.));
        }
    }

    #[gpui::test]
    fn copy_account_info_and_empty_name_validation(cx: &mut gpui::TestAppContext) {
        cx.update(text_editor::install_keybindings);
        let (host, window, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.account_id = "account-id".into();
            host.copy_account_info(cx);
            host.open_name_dialog(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let info: serde_json::Value =
                serde_json::from_str(&cx.read_from_clipboard().unwrap().text().unwrap()).unwrap();
            assert_eq!(info["account_id"], "account-id");
            assert_eq!(info["channel"], "public");
            assert_eq!(info["build"], crate::VERSION);
            assert_eq!(
                info["ap_hostname"],
                crate::release_channel::ReleaseChannel::Public.ap_hostname()
            );
        });
        cx.simulate_keystrokes(window.into(), "backspace");
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| {
                let save = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Save"))
                    .unwrap()
                    .1;
                assert!(save.is_disabled());
            })
            .unwrap();
        test_support::press(window, "enter", cx);
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn display_name_draft_survives_status_and_failed_save_with_single_admission(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests) = host(cx);
        host.update(cx, |host, cx| {
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                initialized: true,
                unlocked: true,
            };
            cx.notify();
        });
        cx.run_until_parked();
        host.update(cx, |host, cx| host.open_name_dialog(cx));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    gpui::Focusable::focus_handle(host.name_editor.as_ref().unwrap().read(cx), cx)
                        .focus(window);
                })
                .unwrap()
        });
        cx.simulate_input(window.into(), " draft");
        let draft = host.read_with(cx, |host, cx| {
            host.name_editor.as_ref().unwrap().read(cx).text().to_owned()
        });
        assert_ne!(draft.trim(), "Robin");
        host.update(cx, |host, cx| {
            host.account_profile.display_name = Some("Changed on another device".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            host.read_with(cx, |host, cx| host
                .name_editor
                .as_ref()
                .unwrap()
                .read(cx)
                .text()
                .to_owned()),
            draft
        );
        focus_button(window, "save-account-name", cx);
        press(window, "enter", cx);
        press(window, "enter", cx);
        let queued = requests.try_recv().unwrap();
        assert!(
            matches!(queued, Message::Request(QueuedRequest { request: Request::SetDisplayName { ref name }, .. }) if name == draft.trim())
        );
        drop(queued);
        assert!(requests.try_recv().is_err(), "pending save admits only once");
        host.update(cx, |host, cx| {
            host.recovery_busy = None;
            host.error = "Local save failed".into();
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            host.read_with(cx, |host, cx| host
                .name_editor
                .as_ref()
                .unwrap()
                .read(cx)
                .text()
                .to_owned()),
            draft
        );
        focus_button(window, "cancel-account-name", cx);
        press(window, "enter", cx);
        assert!(host.read_with(cx, |host, _| host.name_editor.is_none()));
        assert!(requests.try_recv().is_err(), "Cancel does not write the draft");
    }
}
