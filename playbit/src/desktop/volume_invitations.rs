//! A direct grant is already usable. The recipient's decision changes its place in the catalog.
use super::*;
use crate::store::Id;

pub(super) struct Decision {
    pub accept: bool,
}
impl gpui::EventEmitter<Decision> for browser::Browser {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Review {
    Pending,
    Deciding,
}

impl Shell {
    pub(super) fn sidebar_volume_count(&self) -> usize {
        self.volumes.len() + self.invited_volumes.len()
    }

    pub(super) fn set_browser_invitation(&self, browser: &mut browser::Browser) {
        browser.owner_name = self.volume_invitation_owners.get(&browser.volume.owner).cloned();
        browser.volume_invitation = self
            .invited_volumes
            .iter()
            .find(|row| row.id == browser.volume.id && browser.shared_session().is_none())
            .map(
                |_| if self.volume_decision.is_some() { Review::Deciding } else { Review::Pending },
            );
    }

    pub(super) fn decide_volume_invitation(
        &mut self,
        volume: Id,
        accept: bool,
        cx: &mut Context<Self>,
    ) {
        if self.volume_decision.is_some()
            || !self.invited_volumes.iter().any(|row| row.id == volume)
            || (!accept && !self.prepare_browser_navigation(cx))
        {
            return;
        }
        match self.send.enqueue(Request::DecideVolumeInvitation { volume, accept }) {
            Ok(admission) => self.volume_decision = Some((volume, admission)),
            Err(error) => self.error = error.into(),
        }
        if let Some(browser) = &self.browser {
            browser.update(cx, |browser, cx| {
                self.set_browser_invitation(browser);
                cx.notify();
            });
        }
        cx.notify();
    }

    pub(super) fn finish_volume_decision(
        &mut self,
        response: &WorkerResponse,
        cx: &mut Context<Self>,
    ) {
        let Some((volume, admission)) = self.volume_decision else { return };
        if response.credit.as_ref().map(|credit| credit.admission()) != Some(admission) {
            return;
        }
        self.volume_decision = None;
        if let Some((id, accept)) = response.volume_decided
            && id == volume
            && response.result.is_ok()
            && let Some(index) = self.invited_volumes.iter().position(|row| row.id == volume)
        {
            let invited = self.invited_volumes.remove(index);
            if accept {
                let position = self.insert_ordinary_volume(invited);
                if self.browser.as_ref().is_some_and(|browser| browser.read(cx).volume.id == volume)
                {
                    self.reveal_ordinary_volume_page(position);
                }
            } else if self
                .browser
                .as_ref()
                .is_some_and(|browser| browser.read(cx).volume.id == volume)
            {
                self.browser = None;
            }
        }
        if let Some(browser) = &self.browser {
            browser.update(cx, |browser, cx| {
                self.set_browser_volumes(browser);
                cx.notify();
            });
        }
        cx.notify();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert expected controls and completions.")]
mod tests {
    use super::*;
    use crate::store::Volume;
    use gpui::accesskit::{Action, ActionRequest, TreeId};

    fn click(label: &str, window: gpui::WindowHandle<Shell>, cx: &mut gpui::TestAppContext) {
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let target = visual.update(|window, _| {
            window
                .accessibility_nodes_for_test()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing {label}"))
                .0
        });
        visual.simulate_accessibility_action(ActionRequest {
            action: Action::Click,
            target_node: target,
            target_tree: TreeId::ROOT,
            data: None,
        });
        visual.run_until_parked();
    }

    #[gpui::test]
    fn direct_grants_open_in_order_and_respond_without_replacing_the_open_volume(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let owner = Id::from_bytes([7; 16]);
        let make = |byte, name: &str| Volume {
            id: Id::from_bytes([byte; 16]),
            name: name.into(),
            owner,
            share_key_with_service: true,
            personal: true,
        };
        let keep = make(3, "Sunday Game Club");
        let hide = Volume { owner: Id::from_bytes([8; 16]), ..make(4, "Unused") };
        let normal = make(5, "Weekly files");
        host.update(cx, |host, _| host.sidebar_volumes = true);
        let mut status = WorkerResponse::new(Ok(()));
        status.status_volumes = Some(vec![hide.clone(), normal.clone(), keep.clone()]);
        status.volume_invitations = vec![hide.id, keep.id];
        status.volume_invitation_owners.insert(owner, "Frank".into());
        responses.try_send(status).unwrap();
        cx.run_until_parked();
        let bounds = |window: &Window, label: &str| {
            window
                .accessibility_nodes_for_test()
                .find_map(|(_, node)| {
                    (node.label() == Some(label) || node.value() == Some(label))
                        .then(|| node.bounds().unwrap())
                })
                .unwrap()
        };
        window
            .update(cx, |_, window, _| {
                assert!(bounds(window, &normal.name).y0 < bounds(window, "Invitations").y0);
                assert!(bounds(window, "Invitations").y0 < bounds(window, &keep.name).y0);
                assert!(bounds(window, &keep.name).y0 < bounds(window, &hide.name).y0);
            })
            .unwrap();
        click(&normal.name, window, cx);
        window
            .update(cx, |host, window, cx| {
                host.navigation_controls[&format!("volume-{}", normal.id)]
                    .update(cx, |row, cx| row.focus_from_arrow(window, cx));
            })
            .unwrap();
        test_support::press(window, "down", cx);
        assert_eq!(
            host.read_with(cx, |host, cx| host.browser.as_ref().unwrap().read(cx).volume.id),
            keep.id
        );
        for (volume, accept) in [(&keep, true), (&hide, false)] {
            click(&volume.name, window, cx);
            let browser = host.read_with(cx, |host, _| host.browser.clone().unwrap());
            let sender = if accept {
                format!("Frank ({:.6})", owner.to_string())
            } else {
                hide.owner.to_string()
            };
            window
                .update(cx, |_, window, _| {
                    bounds(window, &format!("{sender} has given you access to this volume"));
                    assert_eq!(bounds(window, "Accept").height(), 24.);
                })
                .unwrap();
            click("support@playbit.app", window, cx);
            assert_eq!(cx.opened_url().as_deref(), Some("mailto:support@playbit.app"));
            click(if accept { "Accept" } else { "Decline & Leave" }, window, cx);
            let queued = loop {
                if let Message::Request(queued) = requests.try_recv().unwrap()
                    && matches!(queued.request, Request::DecideVolumeInvitation {volume: id, accept: flag} if id == volume.id && flag == accept)
                {
                    break queued;
                }
            };
            let mut reply = WorkerResponse::from_reply(Ok(Reply::VolumeInvitationDecided {
                volume: volume.id,
                accept,
            }));
            reply.credit = queued.credit;
            responses.try_send(reply).unwrap();
            cx.run_until_parked();
            host.read_with(cx, |host, cx| {
                assert!(!host.invited_volumes.iter().any(|row| row.id == volume.id));
                assert_eq!(host.volumes.iter().any(|row| row.id == volume.id), accept);
                if accept {
                    assert_eq!(host.browser.as_ref(), Some(&browser));
                    assert!(browser.read(cx).volume_invitation.is_none());
                } else {
                    assert!(host.browser.is_none());
                }
            });
        }
    }
    #[gpui::test]
    fn full_catalog_keeps_invitations_and_pages_through_every_ordinary_volume(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, window, requests, responses) = test_support::host_with_responses(cx);
        let volume = |n: u128, name| Volume {
            id: Id::from_bytes(n.to_be_bytes()),
            owner: Id::from_bytes([7; 16]),
            personal: true,
            share_key_with_service: true,
            name,
        };
        let invitation = volume(300, "Sunday Game Club".into());
        let mut rows: Vec<_> = (0..256).map(|i| volume(i, format!("Volume {i:03}"))).collect();
        rows.push(invitation.clone());
        host.update(cx, |host, _| host.sidebar_volumes = true);
        let mut status = WorkerResponse::new(Ok(()));
        status.status_volumes = Some(rows);
        status.volume_invitations = vec![invitation.id];
        responses.try_send(status).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            let page = host.visible_volume_page();
            assert_eq!(page.ordinary, 0..255);
            assert_eq!(page.invited, 0..1);
            assert_eq!(page.ordinary.len() + page.invited.len(), 256);
        });
        click("Next page", window, cx);
        click("Volume 255", window, cx);
        click("Previous page", window, cx);
        host.update(cx, |host, cx| {
            host.reveal_sidebar_row(255, true);
            cx.notify();
        });
        cx.run_until_parked();
        click(&invitation.name, window, cx);
        click("Decline & Leave", window, cx);
        let queued = loop {
            if let Message::Request(queued) = requests.try_recv().unwrap()
                && matches!(queued.request,Request::DecideVolumeInvitation{volume,accept:false} if volume==invitation.id)
            {
                break queued;
            }
        };
        let mut reply = WorkerResponse::from_reply(Ok(Reply::VolumeInvitationDecided {
            volume: invitation.id,
            accept: false,
        }));
        reply.credit = queued.credit;
        responses.try_send(reply).unwrap();
        cx.run_until_parked();
        host.read_with(cx, |host, _| {
            assert!(host.browser.is_none());
            assert_eq!(host.visible_volume_page().ordinary, 0..256);
            assert_eq!(host.visible_volume_page().pages, 1);
        });
    }
}
