//! Bounded recovery of an owned source that AP cannot initialize yet.
//! Only explicit source readiness or pre-dispatch capacity rejection schedules
//! work. This is not a freshness poll; exhaustion waits for explicit recovery.
use super::{Duration, Id, Instant, Result};
use crate::engine::publisher::VOLUME_LIMIT;

const RETRY_LIMIT: u8 = 6;
const FIRST_DELAY: Duration = Duration::from_millis(250);

/// Only an authorized source with a scheduled retry may produce this error.
#[derive(Debug)]
pub(super) struct Pending(pub(super) Box<dyn std::error::Error + Send + Sync>);
impl std::fmt::Display for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Pending {}

fn retryable_error(error: &str) -> bool {
    matches!(
        error,
        "waiting for initial database synchronization"
            | "waiting for initial volume synchronization"
            // AP rejects admission before dispatching any operation. Reuse the
            // same finite bootstrap episode, never retry uncertain transport loss.
            | "AP request capacity exhausted"
    )
}

/// A retryable readiness wait must not conceal a later terminal failure.
pub(super) fn retain_error(
    retained: &mut Option<Box<dyn std::error::Error + Send + Sync>>,
    error: Box<dyn std::error::Error + Send + Sync>,
) {
    if retained.as_ref().is_none_or(|prior| prior.is::<Pending>()) {
        *retained = Some(error);
    }
}

#[derive(Clone, Copy)]
struct Retry {
    volume: Id,
    scheduled: u8,
    after: Option<Instant>,
}

pub(super) struct Initializations {
    slots: [Option<Retry>; VOLUME_LIMIT],
}
impl Default for Initializations {
    fn default() -> Self {
        Self { slots: [None; VOLUME_LIMIT] }
    }
}
// Retained IDs/counts/deadlines plus the existing claimed ID list. No snapshots,
// request payloads, authorization decisions, worker or additional queue.
const _: () = assert!(
    std::mem::size_of::<Initializations>() + VOLUME_LIMIT * std::mem::size_of::<Id>() <= 32 * 1024
);

impl Initializations {
    pub(super) fn clear(&mut self) {
        self.slots.fill(None);
    }
    #[cfg(any(test, feature = "file-provider"))]
    pub(super) fn retain(&mut self, selected: &[Id]) {
        for slot in &mut self.slots {
            if slot.is_some_and(|retry| !selected.contains(&retry.volume)) {
                *slot = None;
            }
        }
    }
    pub(super) fn forget(&mut self, volume: Id) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.is_some_and(|r| r.volume == volume)) {
            *slot = None;
        }
    }
    pub(super) fn failed(&mut self, volume: Id, error: &str, now: Instant) -> Result<()> {
        if !retryable_error(error) {
            self.forget(volume);
            return Ok(());
        }
        let index = self
            .slots
            .iter()
            .position(|s| s.is_some_and(|r| r.volume == volume))
            .or_else(|| self.slots.iter().position(Option::is_none))
            .ok_or("initial source readiness retry capacity exhausted")?;
        let retry = self.slots[index].get_or_insert(Retry { volume, scheduled: 0, after: None });
        retry.after =
            (retry.scheduled < RETRY_LIMIT).then(|| now + FIRST_DELAY * (1u32 << retry.scheduled));
        retry.scheduled = retry.scheduled.saturating_add(1).min(RETRY_LIMIT);
        Ok(())
    }
    pub(super) fn scheduled(&self, volume: Id) -> bool {
        self.slots.iter().flatten().any(|retry| retry.volume == volume && retry.after.is_some())
    }
    #[cfg(any(test, feature = "app-host"))]
    pub(super) fn retrying(&self, error: &(dyn std::error::Error + 'static)) -> bool {
        // Completion owns no in-flight attempt. Any slot without a deadline
        // has exhausted its episode and must remain a visible failure.
        error.is::<Pending>()
            && self.slots.iter().any(Option::is_some)
            && self.slots.iter().flatten().all(|retry| retry.after.is_some())
    }
    pub(super) fn wake_after(&self, now: Instant) -> Option<Duration> {
        self.slots
            .iter()
            .flatten()
            .filter_map(|r| r.after)
            .min()
            .map(|after| after.saturating_duration_since(now))
    }
    pub(super) fn select(&self, volumes: &mut Vec<Id>, now: Instant) {
        assert!(volumes.len() <= VOLUME_LIMIT);
        // A recreated publisher notice cannot bypass a retained deadline or
        // reset an exhausted episode. Explicit Sync/disconnect clears it.
        volumes.retain(|volume| {
            self.slots
                .iter()
                .flatten()
                .find(|r| r.volume == *volume)
                .is_none_or(|r| r.after.is_some_and(|after| after <= now))
        });
        for retry in self.slots.iter().flatten() {
            if volumes.len() == VOLUME_LIMIT {
                break;
            }
            if retry.after.is_some_and(|after| after <= now) && !volumes.contains(&retry.volume) {
                volumes.push(retry.volume);
            }
        }
    }
    pub(super) fn start(&mut self, volume: Id) {
        // Consume only the attempt actually starting. If an earlier volume
        // loses the transport, unattempted selected volumes retain their wake.
        if let Some(retry) = self.slots.iter_mut().flatten().find(|r| r.volume == volume) {
            retry.after = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_source_retries_have_bounded_backoff_and_exclude_uncertain_failures() {
        // Readiness and authoritative pre-dispatch capacity rejection share
        // the same finite episode. Recreated notices cannot bypass its bounds.
        let mut pending = Initializations::default();
        let volume = Id::from_bytes([1; 16]);
        let mut now = Instant::now();
        for error in [
            "waiting for initial database synchronization",
            "waiting for initial volume synchronization",
            "AP request capacity exhausted",
        ] {
            pending.clear();
            for attempt in 0..=RETRY_LIMIT {
                pending.failed(volume, error, now).expect("one retry fits the fixed registry");
                let mut claimed = vec![volume];
                pending.select(&mut claimed, now);
                assert!(claimed.is_empty());
                if attempt == RETRY_LIMIT {
                    assert!(pending.wake_after(now).is_none());
                    assert!(!pending.scheduled(volume));
                    break;
                }
                assert!(pending.scheduled(volume));
                let delay = FIRST_DELAY * (1u32 << attempt);
                assert_eq!(pending.wake_after(now), Some(delay));
                now += delay;
                pending.select(&mut claimed, now);
                assert_eq!(claimed, [volume]);
                pending.start(volume);
                assert!(pending.wake_after(now).is_none());
            }
        }
        for error in ["denied", "waiting for an authorized peer", "disk full", "request timed out"]
        {
            pending.failed(volume, error, now).expect("nonretrying errors allocate no slot");
            assert!(pending.wake_after(now).is_none());
            let mut claimed = vec![volume];
            pending.select(&mut claimed, now);
            assert_eq!(claimed, [volume]);
        }
    }

    #[test]
    fn terminal_failures_take_precedence_over_initial_readiness() {
        // Full sync and multi-volume initialization retain terminal errors even
        // when a different volume still has a bounded readiness retry.
        let mut failure = None;
        retain_error(
            &mut failure,
            Box::new(Pending("waiting for initial database synchronization".into())),
        );
        retain_error(&mut failure, "storage failed".into());
        retain_error(&mut failure, Box::new(Pending("AP request capacity exhausted".into())));
        retain_error(&mut failure, "permission denied".into());
        assert_eq!(failure.expect("terminal failure retained").to_string(), "storage failed");

        let mut pending = Initializations::default();
        let now = Instant::now();
        let waiting = "waiting for initial database synchronization";
        for _ in 0..=RETRY_LIMIT {
            pending.failed(Id::from_bytes([1; 16]), waiting, now).expect("retry fits");
        }
        pending.failed(Id::from_bytes([2; 16]), waiting, now).expect("retry fits");
        assert!(pending.wake_after(now).is_some());
        assert!(
            !pending.retrying(&Pending(waiting.into())),
            "another retry cannot hide exhausted recovery"
        );
    }

    #[test]
    fn retry_capacity_retirement_and_explicit_recovery_are_bounded() {
        // Capacity includes exhausted episodes; unrelated volumes cannot reset
        // their budgets. Success/deletion and explicit recovery release slots.
        let mut pending = Initializations::default();
        let now = Instant::now();
        let waiting = "waiting for initial volume synchronization";
        for byte in 0..VOLUME_LIMIT {
            pending
                .failed(Id::from_bytes((byte as u128).to_be_bytes()), waiting, now)
                .expect("the admitted retry fits the fixed registry");
        }
        let extra = Id::from_bytes([255; 16]);
        assert!(pending.failed(extra, waiting, now).is_err());
        pending.forget(Id::from_bytes([0; 16]));
        pending.failed(extra, waiting, now).expect("retirement freed one retry slot");
        let mut claimed = Vec::new();
        pending.select(&mut claimed, now + FIRST_DELAY);
        assert_eq!(claimed.len(), VOLUME_LIMIT);
        assert!(claimed.contains(&extra));
        let attempted = claimed[0];
        pending.start(attempted);
        pending
            .failed(attempted, "connection lost", now + FIRST_DELAY)
            .expect("transport failure uses ordinary connection recovery");
        claimed.clear();
        pending.select(&mut claimed, now + FIRST_DELAY);
        assert_eq!(claimed.len(), VOLUME_LIMIT - 1, "unattempted retries survive interruption");
        assert!(!claimed.contains(&attempted));
        assert_eq!(pending.wake_after(now + FIRST_DELAY), Some(Duration::ZERO));
        pending.clear();
        assert!(pending.wake_after(now).is_none());
        claimed.clear();
        pending.select(&mut claimed, now);
        assert!(claimed.is_empty());
    }

    #[test]
    fn provider_selection_releases_excluded_deadlines() {
        let mut pending = Initializations::default();
        let now = Instant::now();
        for byte in 0..64 {
            let current = Id::from_bytes([byte; 16]);
            let next = Id::from_bytes([byte + 1; 16]);
            pending
                .failed(current, "waiting for initial volume synchronization", now)
                .expect("current retry fits capacity");
            pending
                .failed(next, "waiting for initial volume synchronization", now)
                .expect("next retry fits capacity");
            pending.retain(&[next]);
            let mut claimed = Vec::new();
            pending.select(&mut claimed, now + FIRST_DELAY);
            assert_eq!(claimed, [next]);
        }
        pending.retain(&[]);
        assert!(pending.wake_after(now).is_none());
    }
}
