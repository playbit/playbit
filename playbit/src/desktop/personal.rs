//! Explicit, session-scoped personal-volume consent. Editing never sends a grant.
use super::*;
use crate::store::{Id, PersonalVolumeGrant, Volume};
use playbit_api::ui::{CheckState, Theme, ThemeColor};

/// A received consent snapshot. Keep the engine's typed values while the user
/// edits a separate draft; only Save sends the selected grants back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub session: String,
    pub volumes: Vec<Volume>,
    pub grants: Option<Vec<PersonalVolumeGrant>>,
}

pub struct Save {
    session: String,
    opening: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
#[allow(
    clippy::items_after_test_module,
    reason = "Keep the consent regression fixtures beside the state they exercise."
)]
mod tests {
    use super::*;
    use crate::desktop::test_support::{focus_button, press};
    fn snapshot(count: usize) -> Snapshot {
        Snapshot {
            session: crate::identifier::encode(&[0xab; 32]),
            grants: None,
            volumes: (0..count)
                .map(|n| Volume {
                    id: Id::from_bytes((n as u128).to_le_bytes()),
                    name: format!("Volume {n:03} {}", "x".repeat(53)),
                    owner: Id::from_bytes([1; 16]),
                    personal: true,
                    share_key_with_service: false,
                })
                .collect(),
        }
    }

    fn fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<Shell>,
        gpui::WindowHandle<Shell>,
        async_channel::Receiver<Message>,
        async_channel::Sender<WorkerResponse>,
    ) {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../assets/InterVariable.ttf"
            ))])
            .unwrap();
        let (send, requests, errors) = worker_queue::channel();
        let (responses, receive) = async_channel::bounded(1);
        let host =
            cx.new(|cx| Shell::new((send, receive, errors), async_channel::bounded(4).1, cx));
        cx.run_until_parked();
        drop(requests.try_recv().unwrap());
        host.update(cx, |host, _| {
            host.enrolled = Some(true);
            host.account_profile = AccountProfile {
                agreement_required: false,
                replacement_required: false,
                display_name: Some("Robin".into()),
                unlocked: true,
                initialized: true,
            };
            host.recovery = Some(crate::engine::reply::RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            });
            host.opening = Some(crate::identifier::encode(&[0xab; 32]));
        });
        let Snapshot { session, volumes, grants } = snapshot(4);
        let allocation = volumes.as_ptr() as usize;
        let response =
            WorkerResponse::from_reply(Ok(Reply::PersonalVolumes { session, volumes, grants }));
        assert!(response.result.is_ok());
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert_eq!(host.personal.as_ref().unwrap().volumes.as_ptr() as usize, allocation);
        });
        cx.update(|cx| Shell::show_sessions(&host, cx));
        let window = host.read_with(cx, |host, _| host.sessions_window.unwrap());
        (host, window, requests, responses)
    }
    fn check(
        window: gpui::WindowHandle<Shell>,
        id: Id,
        write: bool,
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    host.personal_controls[&(id, write)].read(cx).focus(window)
                })
                .unwrap()
        });
        press(window, "space", cx);
    }
    #[gpui::test]
    fn consent_text_and_row_names_are_readable_without_adding_actions(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests, _responses) = fixture(cx);
        for count in [0, 4, 128] {
            host.update(cx, |host, cx| {
                host.open_personal(snapshot(count));
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|cx| {
                window
                    .update(cx, |host, window, _| {
                        use gpui::accesskit::{Action, Role};
                        let nodes: Vec<_> = window.accessibility_nodes_for_test().collect();
                        let ids: std::collections::BTreeSet<_> =
                            nodes.iter().map(|(id, _)| *id).collect();
                        assert_eq!(ids.len(), nodes.len());
                        assert!(window.accessibility_error().is_none());
                        assert!(nodes.iter().any(|(_, n)| n.role() == Role::Heading
                            && n.label() == Some("Personal volume access")));
                        assert!(nodes.iter().any(|(_, n)| {
                            n.value()
                                .is_some_and(|v| v.contains("Mounts persist across app updates"))
                        }));
                        if count == 0 {
                            assert!(nodes.iter().any(|(_, n)| n.value()
                                == Some("No personal volumes are available on this device.")));
                        } else {
                            // The first row is visible; offscreen rows are not required to project.
                            let read = &host.personal_controls[&(Id::from_bytes([0; 16]), false)];
                            let id = text_editor::accessibility_text_ids(read.entity_id()).1;
                            let (_, node) = nodes.iter().find(|(key, _)| *key == id).unwrap();
                            assert!(node.value().unwrap().starts_with("Volume 000"));
                            assert!(node.is_read_only());
                            assert!(
                                !node.supports_action(Action::Focus)
                                    && !node.supports_action(Action::SetValue)
                            );
                        }
                        assert!(host.personal_grants.is_empty());
                    })
                    .unwrap()
            });
        }
        assert!(requests.try_recv().is_err(), "reading consent never grants access");
    }
    fn respond(
        responses: &async_channel::Sender<WorkerResponse>,
        result: Result<bool, String>,
        cx: &mut gpui::TestAppContext,
    ) {
        let mut response = WorkerResponse::from_reply(result.map(|personal_volumes_selected| {
            Reply::PersonalVolumesSelected { personal_volumes_selected }
        }));
        response.personal_session = Some(crate::identifier::encode(&[0xab; 32]));
        responses.try_send(response).unwrap();
        cx.run_until_parked();
    }
    #[gpui::test]
    fn keyboard_drafts_require_read_and_cancel_without_a_request(cx: &mut gpui::TestAppContext) {
        let (host, window, requests, _responses) = fixture(cx);
        let id = Id::from_bytes([0; 16]);
        check(window, id, true, cx);
        host.read_with(cx, |host, _| assert!(host.personal_grants.is_empty()));
        check(window, id, false, cx);
        host.read_with(cx, |host, _| assert_eq!(host.personal_grants.get(&id), Some(&false)));
        check(window, id, true, cx);
        host.read_with(cx, |host, _| assert_eq!(host.personal_grants.get(&id), Some(&true)));
        check(window, id, false, cx);
        host.read_with(cx, |host, _| assert!(host.personal_grants.is_empty()));
        check(window, id, false, cx);
        focus_button(window, "cancel-volume-access", cx);
        press(window, "enter", cx);
        assert!(requests.try_recv().is_err());
        host.read_with(cx, |host, _| {
            assert!(host.personal.is_none());
            assert_eq!(
                host.opening.as_deref(),
                Some(crate::identifier::encode(&[0xab; 32]).as_str())
            );
        });
        press(window, "tab", cx);
        cx.update(|cx| {
            window
                .update(cx, |host, window, cx| {
                    assert!(host.navigation_controls["join"].read(cx).focused(window));
                })
                .unwrap()
        });
    }
    #[gpui::test]
    fn failed_save_retains_exact_draft_and_success_resumes_once(cx: &mut gpui::TestAppContext) {
        let (host, window, requests, responses) = fixture(cx);
        let id = Id::from_bytes([0; 16]);
        check(window, id, false, cx);
        check(window, id, true, cx);
        focus_button(window, "save-volume-access", cx);
        press(window, "enter", cx);
        press(window, "enter", cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::SelectPersonalVolumes { grants, .. }, .. }) if grants == vec![PersonalVolumeGrant {volume:id,write:true}]));
        assert!(requests.try_recv().is_err(), "no early preparation or duplicate save");
        check(window, id, false, cx);
        focus_button(window, "cancel-volume-access", cx);
        press(window, "enter", cx);
        host.read_with(cx, |host, _| {
            assert!(host.personal.is_some());
            assert!(host.personal_save.is_some());
            assert_eq!(host.personal_grants.get(&id), Some(&true));
        });
        respond(&responses, Err("Access could not be saved".into()), cx);
        host.read_with(cx, |host, _| {
            assert_eq!(host.error, "Access could not be saved");
            assert!(host.personal_save.is_none());
            assert_eq!(host.personal_grants.get(&id), Some(&true));
        });
        focus_button(window, "save-volume-access", cx);
        press(window, "enter", cx);
        drop(requests.try_recv().unwrap());
        // A confirmed typed reply must repaint the completed save.
        respond(&responses, Ok(true), cx);
        assert!(matches!(requests.try_recv().unwrap(), Message::Request(QueuedRequest {
            request: Request::PrepareSession { ref session }, .. }) if session == &crate::identifier::encode(&[0xab; 32])));
        host.read_with(cx, |host, _| assert!(host.personal.is_none() && host.joining));
        respond(&responses, Ok(true), cx);
        assert!(requests.try_recv().is_err());
    }
    #[gpui::test]
    fn retired_save_preserves_draft_and_reenables_explicit_retry(cx: &mut gpui::TestAppContext) {
        // Retire an admitted save through the worker response path, then use the controls again.
        let (host, window, requests, responses) = fixture(cx);
        let gate = host.read_with(cx, |host, _| host.send.gate());
        let id = Id::from_bytes([0; 16]);
        check(window, id, false, cx);
        check(window, id, true, cx);
        focus_button(window, "save-volume-access", cx);
        press(window, "enter", cx);
        let Message::Request(queued) = requests.try_recv().unwrap() else {
            panic!("expected the admitted consent save");
        };
        assert!(matches!(queued.request, Request::SelectPersonalVolumes { .. }));
        gate.pause();
        gate.resume();
        assert!(!gate.accepts(queued.epoch));
        responses.try_send(worker::retired_response(queued)).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.personal_save.is_none());
            assert!(host.personal.is_some());
            assert_eq!(host.personal_grants.get(&id), Some(&true));
            assert_eq!(host.error, worker_queue::RETIRED);
        });
        assert!(requests.try_recv().is_err(), "retirement must not replay or prepare a session");

        focus_button(window, "save-volume-access", cx);
        press(window, "enter", cx);
        let Message::Request(retry) = requests.try_recv().unwrap() else {
            panic!("expected an explicit retry");
        };
        assert!(gate.accepts(retry.epoch));
        assert!(matches!(&retry.request, Request::SelectPersonalVolumes { grants, .. }
            if grants == &vec![PersonalVolumeGrant { volume: id, write: true }]));
        assert!(requests.try_recv().is_err());
        gate.pause();
        responses.try_send(worker::retired_response(retry)).unwrap();
        cx.run_until_parked();
        focus_button(window, "cancel-volume-access", cx);
        press(window, "enter", cx);
        host.read_with(cx, |host, _| assert!(host.personal.is_none()));
        assert!(requests.try_recv().is_err(), "Cancel sends no work");
    }
    #[gpui::test]
    fn unconfirmed_save_keeps_draft_and_does_not_resume(cx: &mut gpui::TestAppContext) {
        // A successful RPC without confirmation must leave consent pending.
        let (host, window, requests, responses) = fixture(cx);
        let id = Id::from_bytes([0; 16]);
        check(window, id, false, cx);
        focus_button(window, "save-volume-access", cx);
        press(window, "enter", cx);
        drop(requests.try_recv().unwrap());
        respond(&responses, Ok(false), cx);
        assert!(requests.try_recv().is_err());
        host.read_with(cx, |host, _| {
            assert!(host.personal.is_some());
            assert!(host.personal_save.is_none());
            assert_eq!(host.personal_grants.get(&id), Some(&false));
            assert!(host.error.contains("Access was not confirmed"));
        });
    }
    #[gpui::test]
    fn save_completion_after_navigation_never_reopens_previous_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests, responses) = fixture(cx);
        focus_button(window, "save-volume-access", cx);
        press(window, "enter", cx);
        drop(requests.try_recv().unwrap());
        host.update(cx, |host, cx| {
            host.personal = None;
            host.opening = Some("another-session".into());
            cx.notify();
        });
        respond(&responses, Ok(true), cx);
        assert!(requests.try_recv().is_err());
        host.read_with(cx, |host, _| assert_eq!(host.opening.as_deref(), Some("another-session")));
    }
    #[gpui::test]
    fn invalid_or_oversized_snapshots_cannot_replace_an_existing_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, _window, _requests, _responses) = fixture(cx);
        host.update(cx, |host, _| {
            let initial = host.personal.clone();
            let mut wrong_version = snapshot(0);
            wrong_version.session = "wrong".into();
            let mut unavailable = snapshot(1);
            unavailable.grants =
                Some(vec![PersonalVolumeGrant { volume: Id::from_bytes([9; 16]), write: true }]);
            let mut names = snapshot(1);
            names.volumes[0].name = "x".repeat(65537);
            let mut duplicate = snapshot(2);
            duplicate.volumes[1].id = duplicate.volumes[0].id;
            let mut repeated_grants = snapshot(2);
            repeated_grants.grants = Some(vec![
                PersonalVolumeGrant {
                    volume: repeated_grants.volumes[0].id,
                    write: true
                };
                2
            ]);
            let mut excess_grants = snapshot(1);
            excess_grants.grants = repeated_grants.grants.clone();
            let mut aggregate_names = snapshot(2);
            aggregate_names.volumes[0].name = "x".repeat(32768);
            aggregate_names.volumes[1].name = "y".repeat(32769);
            for value in [
                snapshot(129),
                wrong_version,
                unavailable,
                names,
                duplicate,
                repeated_grants,
                excess_grants,
                aggregate_names,
            ] {
                host.open_personal(value);
                assert_eq!(host.personal, initial);
                assert!(!host.error.is_empty());
            }
            let mut exact_names = snapshot(2);
            exact_names.volumes[0].name = "x".repeat(32768);
            exact_names.volumes[1].name = "y".repeat(32768);
            host.open_personal(exact_names);
            assert!(host.error.is_empty());
            host.open_personal(snapshot(128));
            assert!(host.error.is_empty());
        });
    }
}

impl Shell {
    pub fn request_personal(&mut self, session: String) {
        self.new_session = None;
        if let Err(error) = self.send.send(Request::PersonalVolumes { session }) {
            self.error = error.into();
        }
    }

    pub fn open_personal(&mut self, value: Snapshot) {
        // An unrelated refresh must not replace an in-flight decision.
        if self.personal_save.is_some() {
            return;
        }
        let validated = (|| -> Result<BTreeMap<Id, bool>, String> {
            let session = &value.session;
            if !crate::identifier::valid(session, 32) {
                return Err("Invalid session".into());
            }
            let volumes = &value.volumes;
            if volumes.len() > 128 {
                return Err("Personal volume access supports at most 128 volumes.".into());
            }
            let mut ids = std::collections::BTreeSet::new();
            let mut names = 0;
            for volume in volumes {
                let id = volume.id;
                if !ids.insert(id) {
                    return Err("Duplicate personal volume".into());
                }
                if volume.name.len() > 64 * 1024 - names {
                    return Err("Personal volume names exceed 64 KiB.".into());
                }
                names += volume.name.len();
            }
            let grants = value.grants.as_deref().unwrap_or_default();
            if grants.len() > volumes.len() {
                return Err("Personal access contains too many grants.".into());
            }
            let mut selected = BTreeMap::new();
            for grant in grants {
                if !ids.contains(&grant.volume)
                    || selected.insert(grant.volume, grant.write).is_some()
                {
                    return Err(
                        "Personal access contains an unavailable or duplicate volume.".into()
                    );
                }
            }
            Ok(selected)
        })();
        match validated {
            Ok(grants) => {
                self.personal_grants = grants;
                self.personal_controls.clear();
                self.personal = Some(value);
                self.error.clear();
            }
            Err(error) => self.error = error,
        }
    }

    fn save_personal(&mut self) {
        if self.personal_save.is_some() {
            return;
        }
        let Some(session) = self.personal.as_ref().map(|p| p.session.as_str()) else {
            return;
        };
        let save = Save {
            session: session.to_owned(),
            opening: self.opening.clone().filter(|id| id == session),
        };
        let request = Request::SelectPersonalVolumes {
            session: save.session.clone(),
            grants: self
                .personal_grants
                .iter()
                .map(|(volume, write)| PersonalVolumeGrant { volume: *volume, write: *write })
                .collect(),
        };
        match self.send.send(request) {
            Ok(()) => {
                self.personal_save = Some(save);
                self.error.clear();
            }
            Err(error) => self.error = error.into(),
        }
    }

    pub fn finish_personal_save(
        &mut self,
        session: &str,
        result: Result<bool, &str>,
        cx: &mut Context<Self>,
    ) {
        let _timing = crate::trace::Span::new("ui.personal.save_reply");
        if self.personal_save.as_ref().is_none_or(|save| save.session != session) {
            return;
        }
        let save =
            self.personal_save.take().expect("save completion was checked before consuming it");
        if result != Ok(true) {
            self.error = result
                .err()
                .unwrap_or("Access was not confirmed. Review your choices and retry.")
                .into();
            return;
        }
        // Navigation while saving must not reopen the previous app or dismiss
        // another chooser. A successful local commit alone permits continuation.
        if self.personal.as_ref().map(|p| p.session.as_str()) != Some(session) {
            return;
        }
        self.personal = None;
        self.error.clear();
        if let Some(session) = save.opening.filter(|id| self.opening.as_ref() == Some(id)) {
            match self.prepare_session(session, cx) {
                Ok(()) => {
                    self.joining = true;
                }
                Err(error) => {
                    self.error = error.into();
                    self.joining = false;
                }
            }
        }
    }

    fn personal_check(
        &mut self,
        id: Id,
        name: &str,
        write: bool,
        cx: &mut Context<Self>,
    ) -> Entity<controls::Control> {
        let grant = self.personal_grants.get(&id).copied();
        let checked = if write { grant == Some(true) } else { grant.is_some() };
        let disabled = self.personal_save.is_some() || write && grant.is_none();
        let theme = Theme::light();
        let properties = controls::Properties {
            kind: controls::Kind::Checkbox(if checked { CheckState::On } else { CheckState::Off }),
            label: if write { "Write" } else { "Read" },
            disabled,
            theme: &theme,
        };
        let weak = cx.weak_entity();
        let control = self.personal_controls.entry((id, write)).or_insert_with(|| {
            cx.new(|cx| {
                controls::Control::new(
                    0,
                    properties,
                    Rc::new(move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            let _timing = crate::trace::Span::new("ui.personal.edit");
                            if this.personal.is_none() || this.personal_save.is_some() {
                                return;
                            }
                            if write {
                                if let Some(selected) = this.personal_grants.get_mut(&id) {
                                    *selected = !*selected;
                                }
                            } else if this.personal_grants.remove(&id).is_none() {
                                this.personal_grants.insert(id, false);
                            }
                            cx.notify();
                        });
                    }),
                    cx,
                )
                .accessibility_label(format!(
                    "{} access to {name}",
                    if write { "Write" } else { "Read" }
                ))
            })
        });
        control.update(cx, |control, cx| control.set(properties, cx));
        control.clone()
    }

    pub fn personal_panel(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let _timing = crate::trace::Span::new("ui.personal.build");
        let theme = Theme::light();
        let mut panel = div().flex().flex_col().gap_4()
            .child(self.host_text(HostText::ConsentTitle, "Personal volume access", cx).text_2xl())
            .child(self.host_text(HostText::ConsentInstructions, "Choose the volumes this session may use. Mounts persist across app updates.", cx))
            .child(self.host_text(HostText::ConsentHint,
                "Unchecked volumes stay private. Write access also requires Read. Changes apply when you save.", cx)
                .text_size(crate::ui::desktop::zoom::length(12.)).text_color(rgba(theme.color(ThemeColor::TextSecondary).0)));
        let volumes = self
            .personal
            .as_ref()
            .expect("the personal-volume response was checked as present")
            .volumes
            .iter()
            .map(|volume| (volume.id, volume.name.clone()))
            .collect::<Vec<_>>();
        if volumes.is_empty() {
            panel = panel.child(self.host_text(
                HostText::ConsentEmpty,
                "No personal volumes are available on this device.",
                cx,
            ));
        }
        for (id, name) in volumes {
            let read = self.personal_check(id, &name, false, cx);
            let name_id = text_editor::accessibility_text_ids(read.entity_id()).1;
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        host_text::element(name.to_owned().into(), name_id, false, false)
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(read)
                    .child(self.personal_check(id, &name, true, cx)),
            );
        }
        let saving = self.personal_save.is_some();
        panel.child(
            div()
                .flex()
                .gap_2()
                .child(self.host_callback(
                    "save-volume-access",
                    if saving { "Saving access…" } else { "Save access" },
                    saving,
                    |this, _| this.save_personal(),
                    cx,
                ))
                .child(self.host_callback(
                    "cancel-volume-access",
                    "Cancel",
                    saving,
                    |this, _| {
                        if this.personal_save.is_none() {
                            this.personal = None;
                            this.error.clear();
                        }
                    },
                    cx,
                )),
        )
    }
}
