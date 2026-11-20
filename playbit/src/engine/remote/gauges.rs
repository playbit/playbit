//! Samples read only exclusive Remote ownership; no cross-thread inspection.
use super::*;
use crate::trace::Gauge;

pub(super) struct Gauges {
    transport: Gauge,
    receivers: Gauge,
    retry_wait: Gauge,
}
impl Gauges {
    pub(super) fn new() -> Self {
        Self {
            transport: Gauge::new("remote.sampled_transport_owners", 0),
            receivers: Gauge::new("remote.sampled_subscription_owners", 0),
            retry_wait: Gauge::new("remote.sampled_retry_wait_ns", 0),
        }
    }
}
const _: () = assert!(std::mem::size_of::<Gauges>() <= 3 * 96);
impl Remote {
    pub(crate) fn sample_gauges(&mut self) {
        self.gauges.transport.set(u64::from(self.authenticated_transport().is_some()));
        self.gauges
            .receivers
            .set(self.live.as_ref().map_or(0, |live| live.receiver_owners() as u64));
        let remaining = self.reconnect.as_ref().map_or(0, |retry| {
            u64::try_from(retry.after.saturating_duration_since(Instant::now()).as_nanos())
                .expect("retry delay is bounded to 30 seconds")
        });
        self.gauges.retry_wait.set(remaining);
    }
}
