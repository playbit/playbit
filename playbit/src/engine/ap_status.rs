//! One bounded, coalesced AP connection snapshot, independent of bulk work.
use super::{
    completion::Wake,
    reply::{ApConnectionState, ApConnectionStatus},
};
use std::sync::Mutex;

struct Snapshot {
    status: ApConnectionStatus,
    offline: bool,
    generation: u64,
    changed: bool,
}

pub(crate) struct Status(Mutex<Snapshot>);

impl Default for Status {
    fn default() -> Self {
        Self::new(crate::release_channel::ReleaseChannel::Public.ap_hostname())
    }
}

impl Status {
    #[cfg(feature = "file-provider")]
    pub(crate) fn live_generation(&self) -> Option<u64> {
        let current = self.0.lock().expect("AP status mutex is not poisoned");
        (current.status.state == ApConnectionState::Live).then_some(current.generation)
    }

    pub(crate) fn new(hostname: &str) -> Self {
        assert!(hostname.len() <= ApConnectionStatus::HOSTNAME_BYTES_MAX);
        Self(Mutex::new(Snapshot {
            status: ApConnectionStatus {
                state: ApConnectionState::Offline,
                hostname: hostname.into(),
            },
            offline: false,
            generation: 0,
            changed: false,
        }))
    }
}

impl Status {
    pub(crate) fn offline(&self) -> bool {
        self.0.lock().expect("AP status mutex is not poisoned").offline
    }

    pub(crate) fn set_offline(&self, enabled: bool, wake: &Wake) {
        let mut current = self.0.lock().expect("AP status mutex is not poisoned");
        current.offline = enabled;
        current.generation = current.generation.checked_add(1).expect("AP generation exhausted");
        let state =
            if enabled { ApConnectionState::OfflineMode } else { ApConnectionState::Offline };
        if current.status.state == state {
            return;
        }
        current.status.state = state;
        current.changed = true;
        drop(current);
        wake.notify();
    }

    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn snapshot(&self) -> ApConnectionStatus {
        self.0.lock().expect("AP status mutex is not poisoned").status.clone()
    }

    /// Every owner publication retires the preceding watch, including when its
    /// visible status is identical. A closed old transport cannot affect a new one.
    pub(crate) fn set(&self, state: ApConnectionState, hostname: &str, wake: &Wake) -> u64 {
        assert!(hostname.len() <= ApConnectionStatus::HOSTNAME_BYTES_MAX);
        let mut current = self.0.lock().expect("AP status mutex is not poisoned");
        let state = if current.offline { ApConnectionState::OfflineMode } else { state };
        current.generation = current.generation.checked_add(1).expect("AP generation exhausted");
        let generation = current.generation;
        if current.status.state == state && current.status.hostname == hostname {
            return generation;
        }
        current.status.state = state;
        current.status.hostname.clear();
        current.status.hostname.push_str(hostname);
        current.changed = true;
        drop(current);
        wake.notify();
        generation
    }

    /// The connection watcher sends its existing completion wake after this call.
    pub(crate) fn closed(&self, generation: u64, retryable: bool) {
        let mut current = self.0.lock().expect("AP status mutex is not poisoned");
        if current.generation != generation || current.offline {
            return;
        }
        current.status.state =
            if retryable { ApConnectionState::Connecting } else { ApConnectionState::Offline };
        current.changed = true;
    }

    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn take_changed(&self) -> bool {
        std::mem::take(&mut self.0.lock().expect("AP status mutex is not poisoned").changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaced_transport_cannot_overwrite_current_status() {
        let status = Status::default();
        let completion = super::super::completion::Completion::host(|| {});
        let wake = completion.wake.clone();
        let first = status.set(ApConnectionState::Live, "ap.test", &wake);
        let second = status.set(ApConnectionState::Live, "ap.test", &wake);
        #[cfg(feature = "file-provider")]
        assert_eq!(status.live_generation(), Some(second));
        status.closed(first, true);
        assert_eq!(status.snapshot().state, ApConnectionState::Live);
        status.closed(second, true);
        #[cfg(feature = "file-provider")]
        assert_eq!(status.live_generation(), None);
        assert_eq!(status.snapshot().state, ApConnectionState::Connecting);
        assert!(status.take_changed());
        assert!(!status.take_changed());
        status.set(ApConnectionState::Offline, "ap.test", &wake);
        status.closed(second, false);
        assert_eq!(status.snapshot().state, ApConnectionState::Offline);
    }

    #[test]
    fn offline_intent_retires_old_watches_and_overrides_late_health() {
        let status = Status::default();
        let completion = super::super::completion::Completion::host(|| {});
        let live = status.set(ApConnectionState::Live, "ap.test", &completion.wake);
        status.set_offline(true, &completion.wake);
        status.closed(live, true);
        status.set(ApConnectionState::Connecting, "ap.test", &completion.wake);
        assert_eq!(status.snapshot().state, ApConnectionState::OfflineMode);
        status.set_offline(false, &completion.wake);
        assert_eq!(status.snapshot().state, ApConnectionState::Offline);
    }
}
