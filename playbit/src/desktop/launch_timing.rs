//! Preserve three early timestamps until the guest's trace source is attached.
//! This has fixed storage and performs no I/O on process or AppKit startup.
use std::sync::atomic::{AtomicU64, Ordering};
static ENTRY: AtomicU64 = AtomicU64::new(0);
static APPKIT_START: AtomicU64 = AtomicU64::new(0);
static APPKIT_READY: AtomicU64 = AtomicU64::new(0);

pub fn process_entry() {
    ENTRY.store(crate::trace::clock_ns(), Ordering::Relaxed);
}
pub(crate) fn appkit_start() {
    APPKIT_START.store(crate::trace::clock_ns(), Ordering::Relaxed);
}
pub(crate) fn appkit_ready() {
    APPKIT_READY.store(crate::trace::clock_ns(), Ordering::Relaxed);
    flush();
}
pub(crate) fn flush() {
    if !crate::trace::is_recording() {
        return;
    }
    for (timestamp, label) in [
        (&ENTRY, "guest.process.entry"),
        (&APPKIT_START, "guest.appkit.start"),
        (&APPKIT_READY, "guest.appkit.ready"),
    ] {
        let time = timestamp.swap(0, Ordering::Relaxed);
        if time != 0 {
            crate::trace::mark_related_at(label, crate::trace::SpanId::default(), time);
        }
    }
}
