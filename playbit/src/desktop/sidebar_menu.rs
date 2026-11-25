//! Sidebar menus bind actions to stable identities, even when rows reorder.
use super::*;
use crate::runtime::MenuEntry;
use crate::ui::desktop::popup;

fn item(id: u32, label: &str, destructive: bool) -> MenuEntry {
    MenuEntry::Action {
        icon: None,
        id,
        label: label.into(),
        checked: None,
        disabled: false,
        destructive,
    }
}
impl Shell {
    pub(super) fn session_row_interactions(
        &self,
        control: &Entity<controls::Control>,
        session: &str,
        cx: &mut Context<Self>,
    ) {
        let host = cx.weak_entity();
        let selected = session.to_owned();
        let select = Rc::new(move |_, cx: &mut App| {
            let _ = host.update(cx, |host, cx| {
                host.select_sidebar_session(selected.clone(), cx);
            });
        });
        let retry = self.sessions.iter().find(|row| row.id == session).is_some_and(|row| {
            super::sidebar_view::session_info_failed(row)
                || self.session_icons.failed(&row.resolved_build_id)
        });
        let host = cx.weak_entity();
        let session = session.to_owned();
        control.update(cx, |control, _| {
            control.set_single_click(select);
            control.set_context_menu(Rc::new(move |event, window, cx| {
                let host = host.clone();
                let session = session.clone();
                let activate = Rc::new(move |action, cx: &mut App| {
                    let _ = host.update(cx, |host, cx| {
                        if !host.sessions.iter().any(|row| row.id == session) {
                            return;
                        }
                        if action == 6 {
                            host.retry_session_info(&session, cx);
                            return;
                        }
                        if matches!(action, 4 | 5) {
                            host.select_sidebar_session(session.clone(), cx);
                            let form = cx.new(|cx| {
                                if action == 4 {
                                    new_session::Form::folders(
                                        session.clone(),
                                        host.send.clone(),
                                        cx,
                                    )
                                } else {
                                    new_session::Form::new(
                                        new_session::Target::Session(session.clone()),
                                        host.send.clone(),
                                    )
                                }
                            });
                            host.show_session_form(form, cx);
                            cx.notify();
                            return;
                        }
                        if action == 3 {
                            if let Some(prepared) = &mut host.selected
                                && prepared.session.id == session
                            {
                                prepared.update = None;
                                host.update_notice = None;
                            }
                            host.error = host
                                .send
                                .send(Request::CheckUpdates { session: session.clone() })
                                .err()
                                .unwrap_or_default()
                                .into();
                            cx.notify();
                            return;
                        }
                        if action == 2 {
                            host.request_personal(session.clone());
                            cx.notify();
                            return;
                        }
                        host.error = host
                            .send
                            .send(Request::ForgetSession { session: session.clone() })
                            .err()
                            .unwrap_or_default()
                            .into();
                        cx.notify();
                    });
                });
                let mut items = vec![
                    item(3, "Check for updates", false),
                    item(2, "Personal volume access…", false),
                    item(4, "Folder access…", false),
                    item(5, "Open another window…", false),
                    item(1, "Leave", false),
                ];
                if retry {
                    items.insert(0, item(6, "Retry app info", false));
                }
                let request = popup::Request::context(window, event.position, items, activate, cx);
                cx.defer(move |cx| popup::open(request, cx));
            }));
        });
    }
    pub(super) fn volume_row_interactions(
        &self,
        control: &Entity<controls::Control>,
        volume: crate::store::Id,
        cx: &mut Context<Self>,
    ) {
        let host = cx.weak_entity();
        let target = control.downgrade();
        control.update(cx, |control, _| {
            control.set_context_menu(Rc::new(move |event, window, cx| {
                if host
                    .upgrade()
                    .is_none_or(|host| !host.read(cx).volumes.iter().any(|row| row.id == volume))
                {
                    return;
                }
                let host = host.clone();
                let owner = window.window_handle();
                let activate = Rc::new(move |action, cx: &mut App| {
                    let Some(host) = host.upgrade() else { return };
                    if !host.read(cx).volumes.iter().any(|row| row.id == volume) {
                        return;
                    }
                    if action == 1 {
                        Self::confirm_delete_volume(&host, volume, owner, cx);
                    } else if action == 3 {
                        host.update(cx, |host, cx| host.rename_volume(volume, cx));
                    } else if action == 2 {
                        host.update(cx, |host, cx| {
                            host.start_volume_creation(Request::DuplicateVolume { volume }, cx);
                        });
                    } else if action == 4 {
                        host.update(cx, |host, cx| {
                            if let Ok(account) = crate::store::Id::parse(&host.account_id) {
                                super::file_provider::open_volume(account, volume, cx);
                            }
                        });
                    }
                });
                let request = popup::Request::context(
                    window,
                    event.position,
                    [
                        (3, "Rename", "menu-rename", false),
                        (1, "Delete…", "menu-delete", true),
                        (2, "Duplicate", "menu-duplicate", false),
                        (4, "Reveal in Finder", "menu-finder", false),
                    ]
                    .into_iter()
                    .map(|(id, label, glyph, destructive)| MenuEntry::Action {
                        id,
                        label: label.into(),
                        icon: Some(crate::runtime::MenuGlyph::Asset(glyph)),
                        checked: None,
                        disabled: false,
                        destructive,
                    })
                    .collect(),
                    activate,
                    cx,
                )
                .target(target.clone());
                cx.defer(move |cx| popup::open(request, cx));
            }))
        });
    }
    fn confirm_delete_volume(
        host: &Entity<Self>,
        volume: crate::store::Id,
        owner: gpui::AnyWindowHandle,
        cx: &mut App,
    ) {
        if host.read(cx).delete_volume_prompt {
            return;
        }
        let Some(row) = host.read(cx).volumes.iter().find(|row| row.id == volume) else { return };
        let title = format!("Delete “{}”?", row.name);
        let response = owner.update(cx, |_, window, cx| {
            window.prompt(
                gpui::PromptLevel::Warning,
                &title,
                Some(
                    "This deletes the volume from your account on all your devices. \
                     Apps using it will lose access. This can’t be undone.",
                ),
                &[
                    gpui::PromptButton::Cancel("Cancel".into()),
                    gpui::PromptButton::Ok("Delete".into()),
                ],
                cx,
            )
        });
        let Ok(response) = response else { return };
        host.update(cx, |host, _| host.delete_volume_prompt = true);
        let host = host.downgrade();
        cx.spawn(async move |cx| {
            let confirmed = response.await == Ok(1);
            let _ = host.update(cx, |host, cx| {
                host.delete_volume_prompt = false;
                if confirmed && host.volumes.iter().any(|row| row.id == volume) {
                    host.error = host
                        .send
                        .send(Request::DeleteVolume { volume })
                        .err()
                        .unwrap_or_default()
                        .into();
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated GUI actions.")]
mod tests {
    use super::*;
    #[gpui::test]
    fn retry_app_info_preserves_selection_and_waits_for_actual_loader_state(
        cx: &mut gpui::TestAppContext,
    ) {
        // A queued or rejected local retry must not launch an app or imply a download has begun.
        use crate::engine::builds::BuildState;
        let (host, _, requests) = test_support::host(cx);
        let id = crate::identifier::encode(&[0x37; 32]);
        let failed = BuildState::DownloadFailed { error: "Connection lost".into() };
        host.update(cx, |host, cx| {
            host.sessions = vec![SessionRow {
                reference: String::new(),
                id: id.clone(),
                resolved_build_id: crate::identifier::encode(&[0x42; 32]),
                app_name: None,
                build_state: failed.clone(),
            }];
            host.sidebar_session = Some(id.clone());
            host.retry_session_info(&id, cx);
            assert_eq!(host.sidebar_session.as_deref(), Some(id.as_str()));
            assert_eq!(host.sessions[0].build_state, failed);
            assert!(host.opening.is_none());
            assert!(host.selected.is_none());
        });
        let Message::Request(queued) = requests.try_recv().expect("metadata retry request") else {
            panic!("expected a request");
        };
        assert!(matches!(queued.request, Request::RetrySessionInfo { session } if session == id));
        assert!(requests.try_recv().is_err(), "retry does not enqueue a session launch");
        drop(requests);
        host.update(cx, |host, cx| {
            host.retry_session_info(&id, cx);
            assert_eq!(host.sessions[0].build_state, failed);
            assert!(!host.error.is_empty(), "failed admission is reported");
        });
    }

    #[gpui::test]
    fn delete_confirmation_cancels_and_targets_the_original_volume(cx: &mut gpui::TestAppContext) {
        let (send, requests, errors) = worker_queue::channel();
        let (_responses, receive) = async_channel::bounded(1);
        let volume = crate::store::Volume {
            id: crate::store::Id::generate().unwrap(),
            name: "Test volume".into(),
            owner: crate::store::Id::generate().unwrap(),
            personal: true,
            share_key_with_service: false,
        };
        let host = cx.update(|cx| {
            cx.text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                    "../../assets/InterVariable.ttf"
                ))])
                .unwrap();
            let host =
                Shell::launch((send, receive, errors), async_channel::bounded(4).1, true, cx);
            host.update(cx, |host, _| {
                host.volumes = vec![volume.clone()];
                host.enrolled = Some(true);
                host.account_profile = AccountProfile {
                    agreement_required: false,
                    replacement_required: false,
                    display_name: Some("Test".into()),
                    unlocked: true,
                    initialized: true,
                };
            });
            Shell::show_sessions(&host, cx);
            host
        });
        cx.run_until_parked();
        while requests.try_recv().is_ok() {}
        let owner = host.read_with(cx, |host, _| host.sessions_window.unwrap().into());
        cx.update(|cx| {
            Shell::confirm_delete_volume(&host, volume.id, owner, cx);
            Shell::confirm_delete_volume(&host, volume.id, owner, cx);
        });
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert!(requests.try_recv().is_err());
        cx.update(|cx| Shell::confirm_delete_volume(&host, volume.id, owner, cx));
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(
            matches!(requests.try_recv().unwrap(), worker_queue::Message::Request(worker_queue::QueuedRequest { request: Request::DeleteVolume { volume: id }, .. }) if id == volume.id)
        );
        assert!(requests.try_recv().is_err());
    }
}
