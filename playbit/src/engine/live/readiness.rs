//! A finite source-readiness wait belongs to its exact claimed recovery entry.
//! The existing network owner schedules deadlines; no stream or payload is retained.
use super::{Entry, GAP_CAPTURE_LIMIT, LiveReplicas, RecoveryKind, Result, SCOPE_LIMIT, ScopeKey};
use std::time::{Duration, Instant};

const RETRY_LIMIT: u8 = 6;
const FIRST_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Prepared(u64),
    Receiving { publication: Option<u64>, epoch: [u8; 16] },
}
#[derive(Clone, Copy)]
enum State {
    Waiting(Instant),
    Running,
    Exhausted,
}
#[derive(Clone, Copy)]
pub(super) struct Readiness {
    scope: ScopeKey,
    boundary: Boundary,
    state: State,
    scheduled: u8,
    gaps: u8,
}
const _: () = assert!(std::mem::size_of::<Option<Readiness>>() * SCOPE_LIMIT <= 128 * 1024);

impl Entry {
    fn recovery_boundary(&self) -> Option<Boundary> {
        if !self.recovery_attempted {
            return None;
        }
        if !self.ready {
            self.preparation.map(Boundary::Prepared)
        } else {
            self.receive.as_ref().map(|receive| Boundary::Receiving {
                publication: self.publication_generation,
                epoch: receive.epoch,
            })
        }
    }
}

impl LiveReplicas {
    pub(super) fn readiness_exhausted(&self, entry: &Entry) -> bool {
        entry.readiness.is_some_and(|retry| {
            matches!(retry.state, State::Exhausted) && self.readiness_current(entry, retry)
        })
    }

    fn readiness_current(&self, entry: &Entry, retry: Readiness) -> bool {
        entry.key == retry.scope
            && entry.recovery_boundary() == Some(retry.boundary)
            && match retry.boundary {
                Boundary::Prepared(_) => true,
                Boundary::Receiving { .. } => {
                    self.recoverable(entry, RecoveryKind::Gap)
                        || self.recoverable(entry, RecoveryKind::Transport)
                }
            }
    }

    pub(crate) fn readiness_wake_after(&self, now: Instant) -> Option<Duration> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let retry = entry.readiness?;
                let State::Waiting(after) = retry.state else { return None };
                self.readiness_current(entry, retry).then_some(after.saturating_duration_since(now))
            })
            .min()
    }

    pub(crate) fn append_readiness_recoveries(&self, scopes: &mut Vec<ScopeKey>, now: Instant) {
        // These entries were already claimed, so take_recoveries excludes them.
        // Reuse its one bounded plan and allocation.
        for entry in &self.entries {
            let Some(retry) = entry.readiness else { continue };
            let State::Waiting(after) = retry.state else { continue };
            if after <= now && self.readiness_current(entry, retry) {
                assert!(scopes.len() < SCOPE_LIMIT);
                scopes.push(entry.key);
            }
        }
    }

    /// A preparation may already have retired the old Gap observation. Only
    /// this exact retained generation can continue that claimed episode.
    pub(crate) fn readiness_running(&self, scope: &ScopeKey) -> bool {
        self.entries.iter().any(|entry| {
            entry.key == *scope
                && entry.readiness.is_some_and(|retry| {
                    matches!(retry.state, State::Running) && self.readiness_current(entry, retry)
                })
        })
    }

    pub(crate) fn start_readiness(&mut self, scope: &ScopeKey, now: Instant) -> Result<u8> {
        let Some(index) = self.entries.iter().position(|entry| entry.key == *scope) else {
            return Err("live recovery scope was replaced".into());
        };
        let Some(retry) = self.entries[index].readiness else { return Ok(0) };
        if !self.readiness_current(&self.entries[index], retry) {
            self.entries[index].readiness = None;
            return Err("live readiness recovery generation changed".into());
        }
        if !matches!(retry.state, State::Waiting(after) if after <= now) {
            return Err("live readiness recovery is not due".into());
        }
        self.entries[index].readiness = Some(Readiness { state: State::Running, ..retry });
        Ok(retry.gaps)
    }

    pub(crate) fn defer_readiness(
        &mut self,
        scope: &ScopeKey,
        gaps: u8,
        now: Instant,
    ) -> Result<bool> {
        assert!(gaps < GAP_CAPTURE_LIMIT);
        let index = self
            .entries
            .iter()
            .position(|entry| entry.key == *scope)
            .ok_or("live recovery scope was replaced")?;
        let boundary = self.entries[index]
            .recovery_boundary()
            .ok_or("live readiness recovery has no retained boundary")?;
        let scheduled = self.entries[index].readiness.map_or(0, |retry| retry.scheduled);
        let state = if scheduled < RETRY_LIMIT {
            State::Waiting(now + FIRST_DELAY * (1u32 << scheduled))
        } else {
            State::Exhausted
        };
        let retry = Readiness {
            scope: *scope,
            boundary,
            state,
            scheduled: scheduled.saturating_add(1).min(RETRY_LIMIT),
            gaps,
        };
        if !self.readiness_current(&self.entries[index], retry) {
            return Err("live readiness recovery is no longer authorized to continue".into());
        }
        self.entries[index].readiness = Some(retry);
        Ok(matches!(state, State::Waiting(_)))
    }

    pub(crate) fn clear_readiness(&mut self, scope: &ScopeKey) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == *scope) {
            entry.readiness = None;
        }
    }
}
