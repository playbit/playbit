//! Immediate presentation of one admitted local volume creation.
use super::*;

pub(super) struct Pending {
    admission: u64,
    name: String,
    pub(super) control: Entity<controls::Control>,
}

impl Pending {
    pub(super) fn position(&self, volumes: &[crate::store::Volume]) -> usize {
        volumes.partition_point(|volume| {
            crate::ui::collation::compare(&volume.name, &self.name).is_le()
        })
    }
}

impl Shell {
    pub(super) fn start_volume_creation(&mut self, request: Request, cx: &mut Context<Self>) {
        if self.pending_volume.is_some() {
            self.show_visual_bell(
                "Wait for the current volume to finish creating.",
                "menu-talk",
                cx,
            );
            return;
        }
        if self.volumes.len() >= 256 {
            self.show_visual_bell(
                "The 256-volume display limit has been reached.",
                "menu-talk",
                cx,
            );
            return;
        }
        let name = match &request {
            Request::CreateVolume { name, .. } => name.clone(),
            Request::DuplicateVolume { volume } => {
                let Some(source) = self.volumes.iter().find(|row| row.id == *volume) else {
                    return;
                };
                let mut name = source.name.clone();
                while name.len() + " Copy".len() > 256 {
                    name.pop();
                }
                name.push_str(" Copy");
                name
            }
            _ => {
                self.error = "This action cannot create a volume.".into();
                cx.notify();
                return;
            }
        };
        self.cancel_invitation_restoration();
        let admission = match self.send.enqueue(request) {
            Ok(admission) => admission,
            Err(error) => {
                self.error = error.into();
                cx.notify();
                return;
            }
        };
        let control = cx.new(|cx| {
            let mut control = controls::Control::new(
                0,
                controls::Properties {
                    kind: controls::Kind::Button(playbit_api::ui::ButtonStyle::Default),
                    label: &name,
                    disabled: true,
                    theme: &theme(),
                },
                Rc::new(|_, _| {}),
                cx,
            )
            .navigation()
            .accessibility_label(format!("{name}, creating volume"));
            control.set_sidebar_row();
            control.set_navigation_glyph("volume-cell", cx);
            control
        });
        let pending = Pending { admission, name, control };
        self.volume_scroll
            .scroll_to_item(pending.position(&self.volumes), gpui::ScrollStrategy::Bottom);
        self.pending_volume = Some(pending);
        self.error.clear();
        cx.notify();
    }

    pub(super) fn finish_pending_volume(
        &mut self,
        response: &WorkerResponse,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = &self.pending_volume else { return };
        if response.engine_lost
            || response.critical
            || response
                .credit
                .as_ref()
                .is_some_and(|credit| credit.admission() == pending.admission)
        {
            self.pending_volume = None;
            cx.notify();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    fn volume(byte: u8, name: &str) -> crate::store::Volume {
        crate::store::Volume {
            id: crate::store::Id::from_bytes([byte; 16]),
            owner: crate::store::Id::from_bytes([1; 16]),
            name: name.into(),
            share_key_with_service: false,
            personal: true,
        }
    }

    #[gpui::test]
    fn pending_copy_is_immediate_and_only_its_completion_replaces_it(
        cx: &mut gpui::TestAppContext,
    ) {
        // Hold the worker response while rendering the admitted row, then adopt the durable identity.
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let source = volume(4, "Archive");
        let copy = volume(5, "Archive Copy");
        let family = volume(6, "Family");
        host.update(cx, |host, cx| {
            host.sidebar_volumes = true;
            host.volumes = vec![source.clone(), family.clone()];
            host.start_volume_creation(Request::DuplicateVolume { volume: source.id }, cx);
            host.start_volume_creation(
                Request::CreateVolume { name: "Untitled".into(), share_key_with_service: true },
                cx,
            );
            assert!(host.pending_volume.is_some());
            assert_eq!(host.volumes, vec![source.clone(), family.clone()]);
        });
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("duplicate") };
        assert!(requests.try_recv().is_err(), "only one creation is admitted");
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |_, window, _| {
                    assert!(window.accessibility_nodes_for_test().any(|(_, node)| {
                        node.label() == Some("Archive Copy, creating volume") && node.is_disabled()
                    }));
                    let y = |label: &str| {
                        window
                            .accessibility_nodes_for_test()
                            .find_map(|(_, node)| {
                                (node.label() == Some(label)).then(|| node.bounds().unwrap().y0)
                            })
                            .expect("rendered volume")
                    };
                    assert!(y("Archive") < y("Archive Copy, creating volume"));
                    assert!(y("Archive Copy, creating volume") < y("Family"));
                })
                .unwrap()
        });
        responses.try_send(WorkerResponse::new(Ok(()))).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| assert!(host.pending_volume.is_some()));
        let mut stale = WorkerResponse::new(Err("Retired connection".into()));
        stale.engine_lost = true;
        stale.recovery_epoch = Some(u64::MAX);
        responses.try_send(stale).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| assert!(host.pending_volume.is_some()));
        let mut response = WorkerResponse::from_reply(Ok(Reply::Diagnostic(
            crate::engine::reply::DiagnosticReply::VolumeDuplicated {
                source: source.id,
                volume: copy.clone(),
            },
        )));
        response.credit = queued.credit;
        responses.try_send(response).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert!(host.pending_volume.is_none());
            assert!(host.volumes.contains(&copy));
            assert_eq!(host.volume_rename.as_ref().unwrap().volume(), copy.id);
            assert_eq!(host.browser.as_ref().unwrap().read(cx).volume, copy);
        });
    }

    #[gpui::test]
    fn failed_creation_clears_only_its_pending_row(cx: &mut gpui::TestAppContext) {
        // Failure and engine loss cannot leave an actionable volume with a fabricated identity.
        let (host, _, requests, responses) = test_support::host_with_responses(cx);
        for engine_lost in [false, true] {
            host.update(cx, |host, cx| {
                host.sidebar_volumes = true;
                host.start_volume_creation(
                    Request::CreateVolume { name: "Untitled".into(), share_key_with_service: true },
                    cx,
                );
            });
            let Message::Request(queued) = requests.try_recv().unwrap() else { panic!("create") };
            let mut response = WorkerResponse::new(Err("Creation failed".into()));
            response.engine_lost = engine_lost;
            response.credit = queued.credit;
            responses.try_send(response).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(host.pending_volume.is_none());
                assert!(host.volumes.is_empty());
                assert!(host.volume_rename.is_none());
            });
        }
    }
}
