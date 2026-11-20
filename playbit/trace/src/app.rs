//! Guest execution ownership and stateless batch ingestion. No active span
//! history is retained; ends and parent references may precede their starts.
use crate::{AppIdentity, AppNameKind};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppTraceError {
    Invalid,
    Unsupported,
    Disabled,
    Stale,
    Capacity,
    Exhausted,
}
pub struct AppScope {
    #[cfg(all(unix, target_has_atomic = "64"))]
    owner: Option<(usize, u32)>,
    #[cfg(all(unix, target_has_atomic = "64"))]
    loss_generation: u64,
    #[cfg(all(unix, target_has_atomic = "64"))]
    lost: u64,
}
impl AppScope {
    pub fn new(
        label: &str,
        identity: AppIdentity,
        names: &[(AppNameKind, &str)],
        instance: [u8; 16],
    ) -> Self {
        let scope = Self {
            #[cfg(all(unix, target_has_atomic = "64"))]
            owner: None,
            #[cfg(all(unix, target_has_atomic = "64"))]
            loss_generation: 0,
            #[cfg(all(unix, target_has_atomic = "64"))]
            lost: 0,
        };
        #[cfg(all(unix, target_has_atomic = "64"))]
        let mut scope = scope;
        #[cfg(all(unix, target_has_atomic = "64"))]
        if let Some(source) = crate::source::SOURCE.get() {
            if label.is_empty()
                || label.len() > 256
                || names.len() > 256
                || names.iter().any(|(_, n)| !crate::format::valid_app_name(n))
            {
                source.region.header().lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return scope;
            }
            scope.owner = register(source, label, identity, names, instance);
        }
        let _ = (label, identity, names, instance);
        scope
    }
    pub fn generation(&self) -> u64 {
        #[cfg(all(unix, target_has_atomic = "64"))]
        if let Some((slot, id)) = self.owner {
            let Some(source) = crate::source::SOURCE.get() else {
                return 0;
            };
            if source.region.header().disabled.load(std::sync::atomic::Ordering::Acquire) != 0 {
                return 0;
            }
            let app = &source.region.header().apps[slot];
            if app.scope.load(std::sync::atomic::Ordering::Acquire) == u64::from(id) {
                return app.effective_generation(
                    source.region.header().recording.load(std::sync::atomic::Ordering::Acquire),
                );
            }
        }
        0
    }
    pub fn record(
        &mut self,
        bytes: &[u8],
        generation: u64,
        lost: u64,
    ) -> Result<(), AppTraceError> {
        #[cfg(all(unix, target_has_atomic = "64"))]
        {
            use std::sync::atomic::Ordering;
            if generation == 0 {
                return Err(AppTraceError::Invalid);
            }
            let (slot, scope) = self.owner.ok_or(AppTraceError::Capacity)?;
            let source = crate::source::SOURCE.get().ok_or(AppTraceError::Disabled)?;
            let app = &source.region.header().apps[slot];
            let batch = crate::input::validate_batch(bytes, |name, kind| {
                usize::from(name) < app.count.load(Ordering::Relaxed) as usize
                    && app.kinds[usize::from(name)].load(Ordering::Relaxed)
                        == if kind == 1 { 1 } else { 2 }
            })
            .map_err(|e| match e {
                playbit_api::abi::syscall::Error::Unsupported => AppTraceError::Unsupported,
                _ => AppTraceError::Invalid,
            })?;
            if generation != self.generation() {
                add_counter(&app.lost, batch.events().count() as u64)?;
                source.wake();
                return Err(AppTraceError::Stale);
            }
            if self.loss_generation != generation {
                self.loss_generation = generation;
                self.lost = 0;
            }
            if lost < self.lost {
                return Err(AppTraceError::Invalid);
            }
            add_counter(&app.lost, lost - self.lost)?;
            self.lost = lost;
            for event in batch.events() {
                if !source.emit(scope, 1, generation, event.bytes) {
                    add_counter(&app.lost, 1)?;
                }
            }
            source.wake();
            Ok(())
        }
        #[cfg(not(all(unix, target_has_atomic = "64")))]
        {
            let _ = (bytes, generation, lost);
            Err(AppTraceError::Unsupported)
        }
    }
}
// AppScope is the sole counter writer until its catalog retires. The collector
// only samples these atomics; checked addition rejects untrusted u64 reports.
#[cfg(all(unix, target_has_atomic = "64"))]
fn add_counter(counter: &std::sync::atomic::AtomicU64, delta: u64) -> Result<(), AppTraceError> {
    use std::sync::atomic::Ordering;
    let next =
        counter.load(Ordering::Relaxed).checked_add(delta).ok_or(AppTraceError::Exhausted)?;
    counter.store(next, Ordering::Release);
    Ok(())
}
#[cfg(all(unix, target_has_atomic = "64"))]
fn register(
    source: &crate::source::Source,
    label: &str,
    identity: AppIdentity,
    names: &[(AppNameKind, &str)],
    instance: [u8; 16],
) -> Option<(usize, u32)> {
    use std::sync::atomic::Ordering;
    let header = source.region.header();
    let id = header
        .next_scope
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
            (id != 0 && id <= u64::from(u32::MAX)).then(|| id + 1)
        })
        .ok()? as u32;
    let slot = header.apps.iter().position(|app| {
        app.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok()
    })?;
    let app = &header.apps[slot];
    app.scope.store(u64::from(id), Ordering::Relaxed);
    for (word, bytes) in app.instance.iter().zip(instance.as_chunks::<8>().0.iter()) {
        word.store(u64::from_le_bytes(*bytes), Ordering::Relaxed);
    }
    app.identity_flags.store(
        u64::from(identity.session.is_some()) | (u64::from(identity.build.is_some()) << 1),
        Ordering::Relaxed,
    );
    for (offset, value) in [identity.session, identity.build].into_iter().enumerate() {
        for (word, bytes) in app.identity[offset * 4..offset * 4 + 4]
            .iter()
            .zip(value.unwrap_or([0; 32]).as_chunks::<8>().0.iter())
        {
            word.store(u64::from_le_bytes(*bytes), Ordering::Relaxed);
        }
    }
    let mut bytes = [0; 256];
    bytes[..label.len()].copy_from_slice(label.as_bytes());
    for (word, chunk) in app.label.iter().zip(bytes.as_chunks::<8>().0.iter()) {
        word.store(u64::from_le_bytes(*chunk), Ordering::Relaxed);
    }
    app.label_length.store(label.len() as u64, Ordering::Relaxed);
    app.count.store(names.len() as u64, Ordering::Relaxed);
    for ((name, kind), (tag, text)) in app.names.iter().zip(&app.kinds).zip(names) {
        name.set(text);
        kind.store(*tag as u64, Ordering::Relaxed);
    }
    app.selection.store(0, Ordering::Relaxed);
    app.generation.store(header.recording.load(Ordering::Acquire), Ordering::Relaxed);
    app.lost.store(0, Ordering::Relaxed);
    app.state.store(2, Ordering::Release);
    source.wake();
    Some((slot, id))
}
impl Drop for AppScope {
    fn drop(&mut self) {
        #[cfg(all(unix, target_has_atomic = "64"))]
        if let Some((slot, _)) = self.owner
            && let Some(source) = crate::source::SOURCE.get()
        {
            // This owner cannot publish again after Drop. Retain its immutable
            // catalog through already-published queue prefixes, without waiting
            // for unrelated future native or guest work to become idle.
            for (cut, queue) in source.region.header().apps[slot]
                .retire_heads
                .iter()
                .zip(&source.region.header().queues)
            {
                cut.store(
                    queue.head.load(std::sync::atomic::Ordering::Acquire),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            source.region.header().apps[slot].state.store(3, std::sync::atomic::Ordering::Release);
            source.wake();
        }
    }
}

#[cfg(test)]
#[cfg(all(unix, target_has_atomic = "64"))]
mod tests {
    #[test]
    fn loss_and_native_identifier_exhaustion_do_not_wrap() {
        // A guest can report any u64; a second capture cannot wrap its retained counter.
        let counter = std::sync::atomic::AtomicU64::new(0);
        super::add_counter(&counter, u64::MAX).expect("full-width report");
        assert_eq!(super::add_counter(&counter, 1), Err(super::AppTraceError::Exhausted));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), u64::MAX);
        use std::sync::{Arc, atomic::Ordering};
        let source = Arc::new(crate::source::Source::create(8, &[]).expect("bounded source"));
        source.region.header().next_scope.store(u64::from(u32::MAX), Ordering::Relaxed);
        let last = super::register(&source, "last", Default::default(), &[], [1; 16]);
        assert_eq!(last.map(|(_, id)| id), Some(u32::MAX));
        assert!(super::register(&source, "refused", Default::default(), &[], [2; 16]).is_none());
        assert_eq!(
            source.region.header().next_scope.load(Ordering::Relaxed),
            u64::from(u32::MAX) + 1
        );
        source.region.header().next_tid.store(u64::from(u32::MAX), Ordering::Relaxed);
        for expected in [Some(u32::MAX), None] {
            let source = source.clone();
            assert_eq!(
                std::thread::spawn(move || source.timestamp().map(|(tid, _)| tid))
                    .join()
                    .expect("fresh native thread"),
                expected
            );
        }
        assert_eq!(
            source.region.header().next_tid.load(Ordering::Relaxed),
            u64::from(u32::MAX) + 1
        );
    }
}
