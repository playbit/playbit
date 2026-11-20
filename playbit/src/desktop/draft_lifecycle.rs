//! Flush local recovery before retiring the engine that owns its profile writer.
use super::*;
use crate::ui::desktop::sign_in::flow::{ProfileChange, Ticket};

pub(super) enum Retirement {
    SignOut,
    Channel(crate::release_channel::ReleaseChannel),
    Profile(Option<Ticket>, ProfileChange),
}
impl Shell {
    pub(super) fn defer_draft_retirement(
        &mut self,
        action: Retirement,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.browser.as_ref().is_some_and(|browser| {
            !browser.update(cx, |browser, cx| browser.retain_local_drafts(cx))
        }) {
            return true;
        }
        if !drafts::pending(cx) {
            return false;
        }
        self.draft_retirement = Some(action);
        drafts::flush(cx);
        cx.notify();
        true
    }
    pub(super) fn resume_draft_retirement(&mut self, cx: &mut Context<Self>) {
        if drafts::pending(cx) {
            return;
        }
        let Some(action) = self.draft_retirement.take() else {
            return;
        };
        match action {
            Retirement::SignOut => self.begin_sign_out(cx),
            Retirement::Channel(channel) => {
                if let Err(error) = self.switch_channel(channel, cx) {
                    self.error = error;
                    cx.notify();
                }
            }
            Retirement::Profile(ticket, change) => self.change_sign_in_profile(ticket, change, cx),
        }
    }
}

pub(super) fn retain_window_drafts(cx: &mut App) -> bool {
    let mut retained = true;
    for window in cx.windows() {
        if let Some(window) = window.downcast::<Shell>() {
            let result = window.update(cx, |host, _, cx| {
                host.browser.as_ref().is_none_or(|browser| {
                    browser.update(cx, |browser, cx| browser.retain_local_drafts(cx))
                })
            });
            retained &= result.unwrap_or(false);
        }
    }
    retained
}
