//! One coalesced progress snapshot, published through the existing owner wake.
use super::completion::Wake;
use std::sync::Mutex;

use super::reply::TransferProgress;

#[derive(Default)]
pub(crate) struct Progress {
    state: Mutex<(Option<TransferProgress>, bool)>,
}
impl Progress {
    pub(crate) fn set(&self, step: Option<TransferProgress>, wake: &Wake) {
        let mut state = self.state.lock().expect("mutex is not poisoned");
        if state.0 == step {
            return;
        }
        *state = (step, true);
        drop(state);
        wake.notify();
    }
    pub(crate) fn snapshot(&self) -> Option<TransferProgress> {
        self.state.lock().expect("mutex is not poisoned").0.clone()
    }
    pub(crate) fn take_changed(&self) -> bool {
        std::mem::take(&mut self.state.lock().expect("mutex is not poisoned").1)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    #[test]
    fn progress_coalesces_and_never_claims_an_attempt_succeeded() {
        // Repeated stages coalesce; changing and clearing the slot each publish one change.
        let (read, write) = UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        write.set_nonblocking(true).unwrap();
        let write = Wake::unix(write);
        let progress = Progress::default();
        for current in 1..=20 {
            progress.set(Some(TransferProgress::Volumes { current, total: 20 }), &write);
        }
        assert!(progress.take_changed());
        assert!(!progress.take_changed());
        assert_eq!(
            progress.snapshot().map(|step| step.message()).as_deref(),
            Some("Syncing volume 20 of 20…")
        );
        progress.set(Some(TransferProgress::Volumes { current: 20, total: 20 }), &write);
        assert!(!progress.take_changed());
        progress.set(None, &write);
        assert!(progress.take_changed());
        assert_eq!(progress.snapshot().map(|step| step.message()), None);
    }

    #[test]
    fn clearing_a_connection_stage_retires_its_hostname() {
        // Success, failure, and a subsequent operation must not retain connection progress.
        let (_read, socket) = UnixStream::pair().unwrap();
        let wake = Wake::unix(socket);
        let progress = Progress::default();
        progress.set(
            Some(TransferProgress::Connecting { hostname: "configured.ap.test".into() }),
            &wake,
        );
        assert_eq!(progress.snapshot().unwrap().message(), "Connecting to configured.ap.test");
        assert!(progress.take_changed());
        progress.set(None, &wake);
        assert!(progress.take_changed());
        assert!(progress.snapshot().is_none());
        progress.set(Some(TransferProgress::Account), &wake);
        assert_eq!(progress.snapshot(), Some(TransferProgress::Account));
    }
}
