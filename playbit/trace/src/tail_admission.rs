//! Fixed observer credits shared by queued and active in-process tail clients.
//! Pending and active clients retain the same fixed credit until retirement.
use crate::atomic::{AtomicBool, Ordering};
pub const CAPACITY: usize = 4;

pub fn claim(slots: &[AtomicBool; CAPACITY]) -> Option<usize> {
    slots.iter().position(|slot| {
        slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    })
}

/// The unique credit owner releases only the index returned by claim.
pub fn release(slots: &[AtomicBool; CAPACITY], index: usize) {
    slots[index].store(false, Ordering::Release);
}
