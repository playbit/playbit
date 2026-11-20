//! Two notification generations. The bootstrap writer is externally serialized;
//! readers pin and recheck once, never waiting for replacement or another reader.
use crate::atomic::{AtomicI32, AtomicU64, Ordering, fence};

pub struct Slot {
    readers: AtomicU64,
    descriptor: AtomicI32,
}
pub struct Bank {
    generation: AtomicU64,
    slots: [Slot; 2],
}
pub struct Pin<'a> {
    slot: &'a Slot,
}
impl Pin<'_> {
    pub fn descriptor(&self) -> i32 {
        self.slot.descriptor.load(Ordering::SeqCst)
    }
}
impl Drop for Pin<'_> {
    fn drop(&mut self) {
        self.slot.readers.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Bank {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            slots: std::array::from_fn(|_| Slot {
                readers: AtomicU64::new(0),
                descriptor: AtomicI32::new(-1),
            }),
        }
    }

    /// The sole writer must own the attachment mutex through publish. Exhaustion
    /// and a paused old reader both refuse this replacement without a retry loop.
    pub fn prepare(&self) -> Option<u64> {
        let generation = self.generation.load(Ordering::SeqCst).checked_add(1)?;
        if generation == u64::MAX {
            return None;
        }
        fence(Ordering::SeqCst);
        (self.slots[generation as usize & 1].readers.load(Ordering::SeqCst) == 0)
            .then_some(generation)
    }

    /// Called after replacing the owned socket in this inactive slot. A late
    /// pin of its old generation fails its generation recheck before using fd.
    pub fn publish(&self, generation: u64, descriptor: i32) {
        self.slots[generation as usize & 1].descriptor.store(descriptor, Ordering::SeqCst);
        self.generation.store(generation, Ordering::SeqCst);
    }

    pub fn pin(&self) -> Option<Pin<'_>> {
        let generation = self.generation.load(Ordering::SeqCst);
        if generation == 0 {
            return None;
        }
        let slot = &self.slots[generation as usize & 1];
        slot.readers.fetch_add(1, Ordering::SeqCst);
        let pin = Pin { slot };
        fence(Ordering::SeqCst);
        (self.generation.load(Ordering::SeqCst) == generation).then_some(pin)
    }
}
