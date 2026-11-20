//! Retry only engine initialization/read-only observation. Never replay app work.
use std::time::{Duration, Instant};

pub const NOTICE_AFTER: Duration = Duration::from_secs(10);
const INITIAL_DELAY: Duration = Duration::from_millis(100);
const MAX_DELAY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Backoff {
    delay: Duration,
    healthy_since: Option<Instant>,
}
impl Backoff {
    pub fn failed(&mut self, now: Instant) -> Duration {
        if self.healthy_since.take().is_some_and(|at| now.duration_since(at) >= NOTICE_AFTER) {
            self.delay = Duration::ZERO;
        }
        let delay = self.delay;
        self.delay = if delay.is_zero() { INITIAL_DELAY } else { (delay * 2).min(MAX_DELAY) };
        delay
    }
    pub fn recovered(&mut self, now: Instant) {
        self.healthy_since.get_or_insert(now);
    }
}

/// Reap only on a recovery attempt, not on a freshness timer. Retain at most
/// one live child per owner; the engine continues after its GUI closes.
pub fn reap(child: &mut Option<std::process::Child>, profile: &std::path::Path) {
    let Some(process) = child.as_mut() else { return };
    match process.try_wait() {
        Ok(None) => return,
        Ok(Some(status)) => crate::platform::failure::log(
            profile,
            "owned engine exited",
            &format!("child_pid={} status={status}", process.id()),
        ),
        Err(error) => crate::platform::failure::log(
            profile,
            "owned engine wait failed",
            &format!("child_pid={} error={error:?}", process.id()),
        ),
    }
    child.take();
}

pub fn terminal(error: &str) -> bool {
    #[cfg(target_os = "macos")]
    if error == super::launch_agent::START_ERROR {
        return true;
    }
    matches!(
        error,
        super::ipc::CORRUPT_STORE_ERROR
            | super::ipc::PROFILE_OPEN_ERROR
            | super::ipc::INCOMPATIBLE_ENGINE
            | "The Playbit installation is incomplete. Reinstall Playbit."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used, reason = "Tests assert successful child and diagnostic fixtures.")]
    fn exited_children_are_reaped_and_their_status_is_logged() {
        let profile = tempfile::tempdir().unwrap();
        let mut child =
            Some(std::process::Command::new("/bin/sh").args(["-c", "exit 23"]).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.is_some() {
            assert!(Instant::now() < deadline);
            reap(&mut child, profile.path());
            std::thread::sleep(Duration::from_millis(1));
        }
        let path = crate::paths::Paths::for_profile(profile.path()).unwrap().engine_log;
        let report = std::fs::read_to_string(&path).unwrap();
        assert!(report.contains("status=exit status: 23"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn repeated_crashes_back_off_until_the_engine_has_been_stable() {
        let now = Instant::now();
        let mut retry = Backoff::default();
        for ms in [0, 100, 200, 400, 800, 1600, 3200, 5000, 5000] {
            assert_eq!(retry.failed(now), Duration::from_millis(ms));
            retry.recovered(now);
        }
        retry.recovered(now);
        assert_eq!(retry.failed(now + NOTICE_AFTER), Duration::ZERO);
    }
}
