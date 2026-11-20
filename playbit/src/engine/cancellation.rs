//! Cooperative cancellation shared by connected engine compositions.
use std::sync::atomic::{AtomicBool, Ordering};

/// One background operation's cancellation. `raise` both records it, for the
/// synchronous checks between steps, and wakes a step that is already waiting
/// on the network, which cannot poll a flag.
///
/// The record is a plain flag rather than the watch's value: `raised` is read
/// between every step, including with the shared cancel handle held, and must
/// not queue behind anything.
pub(crate) struct Canceled {
    flag: AtomicBool,
    wake: tokio::sync::watch::Sender<bool>,
}
impl Default for Canceled {
    fn default() -> Self {
        Self { flag: AtomicBool::new(false), wake: tokio::sync::watch::channel(false).0 }
    }
}
impl Canceled {
    pub(crate) fn raise(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.wake.send_replace(true);
    }
    pub(crate) fn raised(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    /// Resolves as soon as this operation is canceled, including when it
    /// already is.
    pub(crate) async fn raising(&self) {
        let _ = self.wake.subscribe().wait_for(|canceled| *canceled).await;
    }
}
