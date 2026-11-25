//! Bounded recovery while a volume or an invited World awaits initialization.
//! Build adoption can start a World capture. Source readiness and pre-dispatch
//! capacity rejection can retry a capture; exhaustion waits for explicit recovery.
use super::{Duration, Id, Instant, Result};
use crate::engine::publisher::VOLUME_LIMIT;

pub(super) const CHANGING_CUT: &str =
    "volume source kept changing during bootstrap; no received stage was adopted";
const RETRY_LIMIT: u8 = 6;
const FIRST_DELAY: Duration = Duration::from_millis(250);

/// A scheduled retry is pending. Each attempt checks current authorization.
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Volume(Id),
    // The Shared ID is derived from the session. Its World admission is
    // independent of other sessions and of this room's own database capture.
    WorldControls(Id),
}
impl From<Id> for Source {
    fn from(volume: Id) -> Self {
        Self::Volume(volume)
    }
}

#[derive(Clone, Copy)]
struct Retry {
    source: Source,
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
    /// Explicit sync renews the retry budget even for an installed stream whose
    /// last capture left a sibling pending. Disconnect discards the bindings.
    pub(super) fn restart(&mut self, now: Instant, installed: impl Fn(Id) -> bool) {
        for slot in &mut self.slots {
            if let Some(retry) = slot
                && let Source::Volume(volume) = retry.source
                && installed(volume)
            {
                retry.scheduled = 1;
                retry.after = Some(now + FIRST_DELAY);
            } else {
                *slot = None;
            }
        }
    }
    pub(super) fn clear(&mut self) {
        self.slots.fill(None);
    }
    #[cfg(any(test, feature = "file-provider"))]
    pub(super) fn retain(&mut self, selected: &[Id]) {
        for slot in &mut self.slots {
            if slot.is_some_and(
                |retry| !matches!(retry.source, Source::Volume(id) if selected.contains(&id)),
            ) {
                *slot = None;
            }
        }
    }
    pub(super) fn forget(&mut self, source: impl Into<Source>) {
        let source = source.into();
        if let Some(slot) = self.slots.iter_mut().find(|s| s.is_some_and(|r| r.source == source)) {
            *slot = None;
        }
    }
    fn slot(&mut self, source: Source) -> Option<&mut Option<Retry>> {
        let index = self
            .slots
            .iter()
            .position(|s| s.is_some_and(|r| r.source == source))
            .or_else(|| self.slots.iter().position(Option::is_none))?;
        Some(&mut self.slots[index])
    }
    #[cfg(feature = "app-host")]
    pub(super) fn schedule_world(&mut self, shared: Id, now: Instant) -> bool {
        let source = Source::WorldControls(shared);
        let Some(slot) = self.slot(source) else { return false };
        // New build, new admission. A previous build's backoff or exhausted
        // retries must not delay the first attempt for the adopted build.
        *slot = Some(Retry { source, scheduled: 0, after: Some(now) });
        true
    }
    pub(super) fn failed(
        &mut self,
        source: impl Into<Source>,
        error: &str,
        now: Instant,
    ) -> Result<()> {
        self.failed_capture(source, error, now, false)
    }
    pub(super) fn failed_capture(
        &mut self,
        source: impl Into<Source>,
        error: &str,
        now: Instant,
        receiving: bool,
    ) -> Result<()> {
        let source = source.into();
        let changing =
            error == CHANGING_CUT && (receiving || matches!(source, Source::WorldControls(_)));
        if !retryable_error(error) && !changing {
            if receiving {
                // Keep a pending stream discoverable by explicit Sync after a terminal error.
                self.start(source);
            } else {
                self.forget(source);
            }
            return Ok(());
        }
        let slot = self.slot(source).ok_or("initial source readiness retry capacity exhausted")?;
        let retry = slot.get_or_insert(Retry { source, scheduled: 0, after: None });
        if retry.after.is_some() {
            return Ok(());
        }
        retry.after =
            (retry.scheduled < RETRY_LIMIT).then(|| now + FIRST_DELAY * (1u32 << retry.scheduled));
        retry.scheduled = retry.scheduled.saturating_add(1).min(RETRY_LIMIT);
        Ok(())
    }
    pub(super) fn tracked(&self, source: impl Into<Source>) -> bool {
        let source = source.into();
        self.slots.iter().flatten().any(|retry| retry.source == source)
    }
    pub(super) fn scheduled(&self, source: impl Into<Source>) -> bool {
        let source = source.into();
        self.slots.iter().flatten().any(|retry| retry.source == source && retry.after.is_some())
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
                .find(|r| r.source == Source::Volume(*volume))
                .is_none_or(|r| r.after.is_some_and(|after| after <= now))
        });
        for retry in self.slots.iter().flatten() {
            if volumes.len() == VOLUME_LIMIT {
                break;
            }
            if let Source::Volume(volume) = retry.source
                && retry.after.is_some_and(|after| after <= now)
                && !volumes.contains(&volume)
            {
                volumes.push(volume);
            }
        }
    }
    pub(super) fn interrupted(&mut self, source: Source, now: Instant) {
        if let Some(retry) = self.slots.iter_mut().flatten().find(|retry| retry.source == source)
            && retry.after.is_none()
        {
            retry.after = Some(now);
        }
    }

    pub(super) fn due_world_controls(&self, now: Instant) -> Vec<Id> {
        self.slots
            .iter()
            .flatten()
            .filter_map(|retry| match retry.source {
                Source::WorldControls(shared) if retry.after.is_some_and(|after| after <= now) => {
                    Some(shared)
                }
                _ => None,
            })
            .collect()
    }
    pub(super) fn start(&mut self, source: impl Into<Source>) {
        let source = source.into();
        // Consume only the attempt actually starting. If an earlier volume
        // loses the transport, unattempted selected volumes retain their wake.
        if let Some(retry) = self.slots.iter_mut().flatten().find(|r| r.source == source) {
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
        // A manually restarted live volume still needs its missing backing.
        pending.restart(now, |id| id == volume);
        assert!(pending.tracked(volume));
        assert_eq!(pending.wake_after(now), Some(FIRST_DELAY));
        pending.start(volume);
        pending
            .failed_capture(volume, CHANGING_CUT, now, true)
            .expect("tracked retry already fits");
        assert!(pending.scheduled(volume));
        pending
            .failed_capture(volume, "storage error", now, true)
            .expect("terminal error stops scheduling");
        assert!(pending.tracked(volume));
        assert!(!pending.scheduled(volume));
        pending.restart(now, |_| false);
        assert!(!pending.tracked(volume));
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
    fn world_capture_retries_keep_the_room_deadline_through_cut_changes_and_transport_loss() {
        let mut retries = Initializations::default();
        let mut now = Instant::now();
        let shared = Id::from_bytes([7; 16]);
        let room = Source::WorldControls(shared);
        retries
            .failed(room, "waiting for initial database synchronization", now)
            .expect("retry fits the bounded registry");
        for (attempt, error) in
            ["AP request capacity exhausted", CHANGING_CUT].into_iter().enumerate()
        {
            now += FIRST_DELAY * (1 << attempt);
            assert_eq!(retries.due_world_controls(now), [shared]);
            retries.start(room);
            retries.failed(room, error, now).expect("retry fits the bounded registry");
            assert_eq!(retries.wake_after(now), Some(FIRST_DELAY * (2 << attempt)));
        }
        now += FIRST_DELAY * 4;
        retries.start(room);
        retries.interrupted(room, now);
        assert_eq!(retries.due_world_controls(now), [shared]);
        retries.start(room);
        retries.failed(room, CHANGING_CUT, now).expect("retry fits the bounded registry");
        assert_eq!(retries.wake_after(now), Some(FIRST_DELAY * 8));
        // A healthy requalified World releases only its own source.
        retries
            .failed(shared, "waiting for initial database synchronization", now)
            .expect("retry fits the bounded registry");
        retries.forget(room);
        assert!(retries.due_world_controls(now + FIRST_DELAY * 8).is_empty());
        assert!(retries.scheduled(shared));
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
        let mut now = Instant::now();
        let waiting = "waiting for initial database synchronization";
        let exhausted = Id::from_bytes([1; 16]);
        for attempt in 0..=RETRY_LIMIT {
            pending.start(exhausted);
            pending.failed(exhausted, waiting, now).expect("retry fits");
            if attempt < RETRY_LIMIT {
                let delay = FIRST_DELAY * (1u32 << attempt);
                // Another attempt for this World while its wake is reserved must
                // neither consume the next attempt nor move this deadline.
                pending.failed(exhausted, waiting, now).expect("same retry fits");
                assert_eq!(pending.wake_after(now), Some(delay));
                now += delay;
            }
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
