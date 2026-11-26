//! Capacity waits live exactly as long as the authenticated subscription.
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

pub(in crate::engine) const OWNER_BUSY: &str =
    "Private file preparation capacity is busy on the owner";

#[derive(Clone, Copy, Debug)]
pub(in crate::engine) enum PreparationBusy {
    Local,
    Owner,
}

impl std::fmt::Display for PreparationBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Local => "Private file preparation capacity is busy on this device",
            Self::Owner => OWNER_BUSY,
        })
    }
}
impl std::error::Error for PreparationBusy {}

#[derive(Default)]
pub(super) struct PreparationWaits {
    local: AtomicBool,
    local_armed: Notify,
    owner: AtomicBool,
    probe_needed: Notify,
}

impl PreparationWaits {
    pub(super) fn arm(&self, site: PreparationBusy) {
        let (armed, signal) = match site {
            PreparationBusy::Local => (&self.local, &self.local_armed),
            PreparationBusy::Owner => (&self.owner, &self.probe_needed),
        };
        if !armed.swap(true, Ordering::AcqRel) {
            signal.notify_one();
        }
    }

    /// A catalog frame may cancel this future between either await. The level
    /// remains armed until capacity actually returns, or the stream ends.
    pub(super) async fn local_ready(&self, available: impl Future<Output = ()>) {
        while !self.local.load(Ordering::Acquire) {
            self.local_armed.notified().await;
        }
        available.await;
        self.local.store(false, Ordering::Release);
    }

    pub(super) async fn owner_probe(&self) {
        self.probe_needed.notified().await;
    }

    pub(super) fn owner_ready(&self) -> bool {
        self.owner.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll, Waker};

    fn poll(future: impl Future<Output = ()>) -> Poll<()> {
        std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn capacity_waits_survive_intervening_frames_and_coalesce_until_release() {
        let waits = PreparationWaits::default();
        let free = || std::future::ready(());
        assert!(poll(waits.local_ready(free())).is_pending());
        waits.arm(PreparationBusy::Local);
        waits.arm(PreparationBusy::Local);
        assert!(poll(waits.local_ready(std::future::pending())).is_pending());
        assert!(poll(waits.local_ready(free())).is_ready());
        assert!(poll(waits.local_ready(free())).is_pending());

        waits.arm(PreparationBusy::Owner);
        assert!(poll(waits.owner_probe()).is_ready());
        waits.arm(PreparationBusy::Owner);
        assert!(poll(waits.owner_probe()).is_pending());
        assert!(waits.owner_ready());
        assert!(!waits.owner_ready());
        waits.arm(PreparationBusy::Owner);
        assert!(poll(waits.owner_probe()).is_ready());
    }
}
