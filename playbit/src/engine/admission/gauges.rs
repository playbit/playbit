//! Owner observations follow existing admission turns; no freshness timer.
use super::*;
use crate::trace::Gauge;

pub(super) struct Gauges {
    connections: Gauge,
    charged: Gauge,
    change_waiters: Gauge,
}
impl Gauges {
    pub(super) fn new() -> Self {
        Self {
            connections: Gauge::new("ipc.sampled_connections", 0),
            charged: Gauge::new("ipc.sampled_charged_bytes", 0),
            change_waiters: Gauge::new("ipc.sampled_change_waiters", 0),
        }
    }
}
const _: () = assert!(std::mem::size_of::<Gauges>() <= 4 * 96);

impl AdmissionQueue {
    pub(super) fn sample_gauges(&mut self) {
        let mut changes = 0;
        // The same 32-entry bound used by dispatch; no payload scan.
        for pending in &self.pending {
            if let State::WaitingChanges { .. } = pending.state {
                changes += 1;
            }
        }
        self.gauges.connections.set(self.requests() as u64);
        self.gauges.charged.set(self.allocated() as u64);
        self.gauges.change_waiters.set(changes);
    }
}
