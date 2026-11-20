//! Samples from actual engine ownership. Guest execution belongs to the GUI.
use super::*;
use crate::trace::Gauge;

pub(super) struct Gauges {
    embedded_sessions: Gauge,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    instances: [Gauge; 4],
    remote_owner: Gauge,
}
impl Gauges {
    pub(super) fn new() -> Self {
        Self {
            embedded_sessions: Gauge::new("engine.sampled_embedded_sessions", 0),
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            instances: [
                "engine.sampled_owned_instances",
                "engine.sampled_reserved_instances",
                "engine.sampled_unknown_instances",
                "engine.sampled_draining_instances",
            ]
            .map(|name| Gauge::new(name, 0)),
            remote_owner: Gauge::new("engine.sampled_remote_owner", 1),
        }
    }
}
const _: () = assert!(std::mem::size_of::<Gauges>() <= 6 * 96);
impl Engine {
    pub(super) fn sample_gauges(&mut self) {
        if !crate::trace::is_recording() {
            return;
        }
        self.gauges.embedded_sessions.set(self.embedded_sessions.len() as u64);
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        for (gauge, count) in
            self.gauges.instances.iter_mut().zip(self.session_instances.trace_counts())
        {
            gauge.set(count);
        }
        // 0: unavailable or between owner slots; 1: Engine; 2: worker/result.
        let owner = if self.remote.is_some() {
            1
        } else if self.background.is_some() {
            2
        } else {
            0
        };
        self.gauges.remote_owner.set(owner);
        if let Some(remote) = &mut self.remote {
            remote.sample_gauges();
        }
    }
}
