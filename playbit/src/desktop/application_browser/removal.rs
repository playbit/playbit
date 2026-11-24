//! The menu, confirmation and worker reply all refer to the original account and app row.
use super::*;
use crate::{
    runtime::{MenuEntry, MenuGlyph},
    store::AppRemovalImpact,
    ui::desktop::{popup, zoom::length as ui},
};
use playbit_api::ui::{ButtonStyle, CheckState};
use std::cell::Cell;

#[derive(Clone)]
struct TargetApp {
    account: Id,
    app: AppIdentity,
    owner: gpui::WindowHandle<Shell>,
    worker: RequestSender,
    epoch: u64,
}

impl TargetApp {
    fn current(&self, shell: &Shell) -> bool {
        !shell.signing_out
            && !shell.channel_switching
            && shell.account.id == self.account
            && shell.sessions_window == Some(self.owner)
            && shell.send.same_worker(&self.worker)
            && self.worker.gate().accepts(self.epoch)
            && shell.applications.apps.iter().any(|row| row.identity == self.app)
    }
}

pub(super) struct Pending {
    admission: u64,
    target: TargetApp,
    kind: PendingKind,
}

enum PendingKind {
    Inspect { activate: controls::Activate, impact: Rc<Cell<Option<AppRemovalImpact>>> },
    Remove,
}

fn removal_item(impact: Option<AppRemovalImpact>) -> MenuEntry {
    let label = match impact {
        None => "Checking app data…",
        Some(impact) if impact.user_volumes || impact.local_data => "Remove…",
        Some(_) => "Remove",
    };
    MenuEntry::Action {
        id: 1,
        icon: Some(MenuGlyph::Asset("menu-delete")),
        label: label.into(),
        checked: None,
        disabled: impact.is_none(),
        destructive: true,
    }
}

pub(super) struct Dialog {
    target: TargetApp,
    local_data: bool,
    checked: bool,
    focus: bool,
}

impl Shell {
    pub(super) fn app_removal_menu(
        &self,
        control: &Entity<controls::Control>,
        app: AppIdentity,
        cx: &mut Context<Self>,
    ) {
        let host = cx.weak_entity();
        let row = control.downgrade();
        control.update(cx, |control, _| {
            control.set_context_menu(Rc::new(move |event, window, cx| {
                let _ = host.update(cx, |host, cx| {
                    let Some(owner) = window.window_handle().downcast::<Shell>() else { return };
                    let target = TargetApp {
                        account: host.account.id,
                        app: app.clone(),
                        owner,
                        worker: host.send.clone(),
                        epoch: host.send.gate().epoch(),
                    };
                    if !target.current(host)
                        || host.sidebar_volumes
                        || host.modal_open(cx)
                        || host
                            .applications
                            .removal
                            .as_ref()
                            .is_some_and(|pending| matches!(pending.kind, PendingKind::Remove))
                    {
                        return;
                    }
                    let request =
                        Request::AppRemovalImpact { account: target.account, app: app.clone() };
                    match host.send.enqueue(request) {
                        Ok(admission) => {
                            let impact = Rc::new(Cell::new(None));
                            let activate: controls::Activate = {
                                let impact = impact.clone();
                                let target = target.clone();
                                let host = cx.weak_entity();
                                Rc::new(move |_, cx| {
                                    let _ = host.update(cx, |shell, cx| {
                                        if let Some(impact) = impact.get()
                                            && target.current(shell)
                                        {
                                            shell.choose_app_removal(target.clone(), impact, cx);
                                        }
                                    });
                                })
                            };
                            let menu = popup::Request::context(
                                window,
                                event.position,
                                vec![removal_item(None)],
                                activate.clone(),
                                cx,
                            )
                            .target(row.clone());
                            host.applications.removal = Some(Pending {
                                admission,
                                target,
                                kind: PendingKind::Inspect { activate, impact },
                            });
                            cx.defer(move |cx| popup::open(menu, cx));
                        }
                        Err(error) => {
                            host.error = error.into();
                            cx.notify();
                        }
                    }
                });
            }));
        });
    }

    pub(in crate::desktop) fn finish_app_removal(
        &mut self,
        response: &mut WorkerResponse,
        cx: &mut Context<Self>,
    ) {
        if response.engine_lost || response.critical {
            if let Some(Pending { kind: PendingKind::Inspect { activate, .. }, .. }) =
                self.applications.removal.take()
            {
                popup::resolve_context_action(&activate, None, cx);
            }
            self.dismiss_app_removal(false, cx);
            return;
        }
        let Some(pending) = self.applications.removal.as_ref() else { return };
        if response.credit.as_ref().map(|credit| credit.admission()) != Some(pending.admission) {
            return;
        }
        let pending = self.applications.removal.take().expect("matched app removal admission");
        match pending.kind {
            PendingKind::Inspect { activate, impact } => {
                let result = response
                    .app_removal_impact
                    .take()
                    .filter(|_| response.result.is_ok() && pending.target.current(self));
                impact.set(result);
                popup::resolve_context_action(
                    &activate,
                    result.map(|found| removal_item(Some(found))),
                    cx,
                );
            }
            PendingKind::Remove => {
                if response
                    .credit
                    .as_ref()
                    .and_then(|credit| credit.draft_erasure.as_ref())
                    .is_some_and(|hold| hold.same_profile(cx))
                    && self.browser.as_ref().is_some_and(|browser| {
                        response.erased_app_volumes.contains(&browser.read(cx).volume.id)
                    })
                {
                    self.browser = None;
                }
                if response.result.is_ok()
                    && pending.target.current(self)
                    && let Some(impact) = response.app_removal_impact.take()
                {
                    self.confirm_app_removal(pending.target, impact.local_data, cx);
                }
            }
        }
    }

    fn choose_app_removal(
        &mut self,
        target: TargetApp,
        impact: AppRemovalImpact,
        cx: &mut Context<Self>,
    ) {
        if self.applications.removal.is_some() || self.modal_open(cx) {
            return;
        }
        if impact.user_volumes || impact.local_data {
            self.confirm_app_removal(target, impact.local_data, cx);
        } else {
            self.submit_app_removal(target, false, false, cx);
        }
    }

    fn submit_app_removal(
        &mut self,
        target: TargetApp,
        confirmed: bool,
        delete_local_data: bool,
        cx: &mut Context<Self>,
    ) {
        if !target.current(self) || self.applications.removal.is_some() {
            return;
        }
        let request = Request::RemoveApp {
            account: target.account,
            app: target.app.clone(),
            confirmed,
            delete_local_data,
        };
        let hold = if delete_local_data {
            drafts::hold_for_erasure(target.account, target.app.clone(), cx)
        } else {
            None
        };
        match self.send.remove_app(request, hold) {
            Ok(admission) => {
                self.applications.removal =
                    Some(Pending { admission, target, kind: PendingKind::Remove });
            }
            Err(error) => self.error = error.into(),
        }
        cx.notify();
    }

    fn confirm_app_removal(&mut self, target: TargetApp, local_data: bool, cx: &mut Context<Self>) {
        self.applications.removal_dialog =
            Some(Dialog { target, local_data, checked: false, focus: true });
        cx.notify();
    }

    pub(in crate::desktop) fn app_removal_dialog_open(&self) -> bool {
        self.applications.removal_dialog.is_some()
    }

    fn dismiss_app_removal(&mut self, remove: bool, cx: &mut Context<Self>) {
        if remove
            && let Some(dialog) = &self.applications.removal_dialog
            && dialog.checked
        {
            let retained = self.browser.as_ref().is_none_or(|browser| {
                browser.update(cx, |browser, cx| browser.retain_local_drafts(cx))
            });
            if !retained || !drafts::can_erase(dialog.target.account, cx) {
                return;
            }
        }
        let Some(dialog) = self.applications.removal_dialog.take() else { return };
        let owner = dialog.target.owner;
        if remove {
            self.submit_app_removal(dialog.target, true, dialog.local_data && dialog.checked, cx);
        }
        cx.defer(move |cx| {
            let _ = owner.update(cx, |host, window, _| {
                window.focus(&host.focus);
                window.refresh();
            });
        });
        cx.notify();
    }

    pub(in crate::desktop) fn app_removal_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dialog = self.applications.removal_dialog.as_ref()?;
        if !dialog.target.current(self) {
            self.dismiss_app_removal(false, cx);
            return None;
        }
        let app = self.applications.apps.iter().find(|app| app.identity == dialog.target.app)?;
        let title = format!("Remove “{}” from your account on all devices?", app.name);
        let local = dialog.local_data;
        let checked = dialog.checked;
        let focus = dialog.focus;
        let host = cx.weak_entity();
        let checkbox = self.host_control(
            "remove-local-data",
            controls::Properties {
                kind: controls::Kind::Checkbox(if checked {
                    CheckState::On
                } else {
                    CheckState::Off
                }),
                label: "Also delete local data",
                disabled: !local,
                theme: &theme(),
            },
            Rc::new(move |_, cx| {
                let _ = host.update(cx, |host, cx| {
                    if let Some(dialog) = &mut host.applications.removal_dialog
                        && dialog.local_data
                    {
                        dialog.checked = !dialog.checked;
                        if dialog.checked {
                            if let Some(browser) = &host.browser {
                                browser.update(cx, |browser, cx| browser.retain_local_drafts(cx));
                            }
                            drafts::flush(cx);
                        }
                        cx.notify();
                    }
                });
            }),
            cx,
        );
        let cancel = self.host_callback(
            "cancel-remove-app",
            "Cancel",
            false,
            |host, cx| host.dismiss_app_removal(false, cx),
            cx,
        );
        let weak = cx.weak_entity();
        let remove = self.host_control(
            "confirm-remove-app",
            controls::Properties {
                kind: controls::Kind::Button(ButtonStyle::Destructive),
                label: "Remove app",
                disabled: checked && !drafts::can_erase(self.account.id, cx),
                theme: &theme(),
            },
            Rc::new(move |_, cx| {
                let _ = weak.update(cx, |host, cx| host.dismiss_app_removal(true, cx));
            }),
            cx,
        );
        if focus {
            if let Some(dialog) = &mut self.applications.removal_dialog {
                dialog.focus = false;
            }
            cancel.read(cx).focus(window);
        }
        let detail = if local {
            "Your user volumes are kept. You can also erase this app’s local data on this Mac."
        } else {
            "Your user volumes are kept. This app has no local data to delete on this Mac."
        };
        let panel = div()
            .w(ui(328.))
            .max_w_full()
            .rounded(ui(16.))
            .bg(rgb(0xececec))
            .shadow_lg()
            .p(ui(16.))
            .flex()
            .flex_col()
            .gap(ui(10.))
            .child(
                div()
                    .text_size(ui(14.))
                    .line_height(ui(18.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.host_text(HostText::AppRemovalTitle, title, cx)),
            )
            .child(div().text_size(ui(12.)).line_height(ui(16.)).child(self.host_text(
                HostText::AppRemovalDetail,
                detail,
                cx,
            )))
            .child(checkbox)
            .child(div().flex().justify_end().gap(ui(8.)).child(cancel).child(remove));
        Some(
            div()
                .id("remove-app-modal")
                .absolute()
                .inset_0()
                .occlude()
                .bg(rgba(0x00000033))
                .flex()
                .items_center()
                .justify_center()
                .font_family(UI_FONT_FAMILY)
                .capture_key_down(cx.listener(|host, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => host.dismiss_app_removal(false, cx),
                        "tab" if event.keystroke.modifiers.shift => window.focus_prev(),
                        "tab" => window.focus_next(),
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
mod tests {
    use super::*;

    fn target(host: &Shell, owner: gpui::WindowHandle<Shell>) -> TargetApp {
        TargetApp {
            account: host.account.id,
            app: host.applications.apps[0].identity.clone(),
            owner,
            worker: host.send.clone(),
            epoch: host.send.gate().epoch(),
        }
    }

    #[gpui::test]
    fn removal_rechecks_data_and_submits_only_the_checked_device_choice(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, owner, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.apply_imported_app_catalog(vec![crate::store::ImportedApp {
                app: AppIdentity { publisher: host.account.id, app_id: "todo".into() },
                build_id: crate::identifier::encode(&[7; 32]),
                name: "TODO".into(),
            }]);
            host.choose_app_removal(
                target(host, owner),
                AppRemovalImpact { user_volumes: false, local_data: false },
                cx,
            );
        });
        let Message::Request(queued) = requests.try_recv().expect("direct removal") else {
            panic!("expected request")
        };
        assert!(matches!(
            queued.request,
            Request::RemoveApp { confirmed: false, delete_local_data: false, .. }
        ));
        host.update(cx, |host, cx| {
            // A Local volume appeared between the menu inspection and committing removal.
            let mut response = WorkerResponse::from_reply(Ok(Reply::AppRemoved(
                crate::store::AppRemovalResult::Confirmation(AppRemovalImpact {
                    user_volumes: false,
                    local_data: true,
                }),
            )));
            response.credit = queued.credit;
            host.finish_app_removal(&mut response, cx);
        });
        cx.run_until_parked();
        owner
            .update(cx, |_, window, _| {
                let (_, checkbox) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Also delete local data"))
                    .expect("removal checkbox");
                assert_eq!(checkbox.role(), gpui::accesskit::Role::CheckBox);
                assert_eq!(checkbox.toggled(), Some(gpui::accesskit::Toggled::False));
            })
            .expect("management window");
        test_support::focus_button(owner, "remove-local-data", cx);
        test_support::press(owner, "space", cx);
        test_support::focus_button(owner, "confirm-remove-app", cx);
        test_support::press(owner, "space", cx);
        let Message::Request(queued) = requests.try_recv().expect("confirmed removal") else {
            panic!("expected request")
        };
        assert!(
            matches!(queued.request, Request::RemoveApp { account, app, confirmed: true, delete_local_data: true }
            if account == Id::from_bytes([1; 16]) && app.app_id == "todo")
        );
        host.update(cx, |host, cx| {
            host.send.gate().pause();
            let mut lost = WorkerResponse::from_reply(Err(worker_queue::INTERRUPTED.into()));
            lost.engine_lost = true;
            host.finish_app_removal(&mut lost, cx);
            host.send.gate().resume();
            assert!(host.applications.removal.is_none());
            // A late response from the retired worker cannot reopen the prompt.
            let mut late = WorkerResponse::from_reply(Ok(Reply::AppRemoved(
                crate::store::AppRemovalResult::Confirmation(AppRemovalImpact {
                    user_volumes: true,
                    local_data: true,
                }),
            )));
            late.credit = queued.credit;
            host.finish_app_removal(&mut late, cx);
            assert!(!host.app_removal_dialog_open());
        });
        assert!(requests.try_recv().is_err());
    }

    #[gpui::test]
    fn user_volumes_prompt_without_enabling_local_deletion_and_cancel_is_inert(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, owner, requests) = test_support::host(cx);
        host.update(cx, |host, cx| {
            host.apply_imported_app_catalog(vec![crate::store::ImportedApp {
                app: AppIdentity { publisher: host.account.id, app_id: "todo".into() },
                build_id: crate::identifier::encode(&[7; 32]),
                name: "TODO".into(),
            }]);
            host.choose_app_removal(
                target(host, owner),
                AppRemovalImpact { user_volumes: true, local_data: false },
                cx,
            );
        });
        cx.run_until_parked();
        owner
            .update(cx, |_, window, _| {
                let (_, checkbox) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Also delete local data"))
                    .expect("disabled checkbox");
                assert!(checkbox.is_disabled());
            })
            .expect("management window");
        test_support::press(owner, "escape", cx);
        assert!(!host.read_with(cx, |host, _| host.app_removal_dialog_open()));
        assert!(requests.try_recv().is_err());
    }
}
