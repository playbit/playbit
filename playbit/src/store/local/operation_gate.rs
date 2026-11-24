//! FIFO volume admission with bounded contention and permanent panic fencing.
use super::{Error, Result};
use parking_lot::{FairMutex, FairMutexGuard};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MAX_ADMISSIONS: usize = 64;

fn next_admission(active: usize) -> Option<usize> {
    if active < MAX_ADMISSIONS { Some(active + 1) } else { None }
}

#[derive(Default)]
pub(super) struct OperationGate {
    mutex: FairMutex<()>,
    active: AtomicUsize,
    poisoned: AtomicBool,
    publication_pending: AtomicBool,
    pub(super) write_epoch: std::sync::Arc<kvdb::redb::WriteEpoch>,
    #[cfg(feature = "connected-engine")]
    source_generation: std::sync::OnceLock<super::Id>,
}

struct Admission<'a>(&'a AtomicUsize);
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let active = self.0.fetch_sub(1, Ordering::AcqRel);
        assert!(active > 0 && active <= MAX_ADMISSIONS, "gate releases its own credit");
    }
}

pub(super) struct OperationGuard<'a> {
    gate: &'a OperationGate,
    // Unlock before releasing the credit. An admitted waiter owns its own credit.
    _locked: FairMutexGuard<'a, ()>,
    // Multiple gates can drop in array order; independent intervals do not
    // require the stack nesting of a synchronous trace span.
    _timing: Option<crate::trace::Interval>,
    _admission: Admission<'a>,
}
impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.gate.poisoned.store(true, Ordering::Release);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum TryLockError {
    WouldBlock,
    Poisoned,
}

impl OperationGate {
    #[cfg(feature = "connected-engine")]
    pub(super) fn snapshot_token(&self) -> Result<super::SnapshotToken> {
        let generation = match self.source_generation.get() {
            Some(generation) => *generation,
            None => {
                let generated = super::Id::generate()?;
                *self.source_generation.get_or_init(|| generated)
            }
        };
        Ok(super::SnapshotToken { generation, epoch: self.write_epoch.snapshot()? })
    }

    pub(super) fn idle_backend_fence(&self) -> bool {
        !self.publication_pending.load(Ordering::Acquire)
            && std::sync::Arc::strong_count(&self.write_epoch) == 1
    }

    fn admit(&self) -> Result<Admission<'_>> {
        if self.is_poisoned() {
            return Err(Error::Internal("volume operation lock is poisoned"));
        }
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, next_admission)
            .map_err(|_| Error::LocalContention("volume admission queue is full"))?;
        Ok(Admission(&self.active))
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub(super) fn lock(&self) -> Result<OperationGuard<'_>> {
        self.lock_checked(false)
    }

    pub(super) fn lock_for_publication(&self) -> Result<OperationGuard<'_>> {
        self.lock_checked(true)
    }

    pub(super) fn fence_publication(&self, pending: bool) {
        self.publication_pending.store(pending, Ordering::Release);
    }

    fn lock_checked(&self, publication: bool) -> Result<OperationGuard<'_>> {
        let admission = self.admit()?;
        let locked = {
            let _wait = crate::trace::Span::detail("storage.volume_gate_wait");
            self.mutex.lock()
        };
        if self.is_poisoned() {
            return Err(Error::Internal("volume operation lock is poisoned"));
        }
        if !publication && self.publication_pending.load(Ordering::Acquire) {
            return Err(Error::System("volume publication requires recovery".into()));
        }
        Ok(OperationGuard {
            gate: self,
            _locked: locked,
            _timing: crate::trace::Interval::detail("storage.volume_gate_hold"),
            _admission: admission,
        })
    }

    pub(super) fn try_lock(&self) -> std::result::Result<OperationGuard<'_>, TryLockError> {
        let admission = self.admit().map_err(|error| match error {
            Error::LocalContention(_) => TryLockError::WouldBlock,
            _ => TryLockError::Poisoned,
        })?;
        let locked = self.mutex.try_lock().ok_or(TryLockError::WouldBlock)?;
        if self.is_poisoned() {
            return Err(TryLockError::Poisoned);
        }
        if self.publication_pending.load(Ordering::Acquire) {
            return Err(TryLockError::WouldBlock);
        }
        Ok(OperationGuard {
            gate: self,
            _locked: locked,
            _timing: crate::trace::Interval::detail("storage.volume_gate_hold"),
            _admission: admission,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful bounded gate operations.")]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn credits_are_bounded_and_released_on_normal_return() {
        let gate = OperationGate::default();
        let mut credits = (0..MAX_ADMISSIONS).map(|_| gate.admit().unwrap()).collect::<Vec<_>>();
        assert!(matches!(gate.lock(), Err(Error::LocalContention(_))));
        credits.pop();
        assert!(gate.lock().is_ok());
        drop(credits);
        assert_eq!(gate.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn queued_reader_progresses_during_repeated_writer_admission() {
        let gate = OperationGate::default();
        std::thread::scope(|scope| {
            let held = gate.lock().unwrap();
            let (sent, received) = mpsc::channel();
            let gate = &gate;
            let reader = scope.spawn(move || {
                let _guard = gate.lock().unwrap();
                sent.send(()).unwrap();
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while gate.active.load(Ordering::Acquire) != 2 {
                assert!(std::time::Instant::now() < deadline, "reader joins bounded admission");
                std::thread::yield_now();
            }
            drop(held);
            for _ in 0..256 {
                drop(gate.lock().unwrap());
            }
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            reader.join().unwrap();
        });
        assert_eq!(gate.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn panic_fences_waiters_and_releases_all_credits() {
        let gate = OperationGate::default();
        std::thread::scope(|scope| {
            let (sent, received) = mpsc::channel();
            let gate = &gate;
            let holder = scope.spawn(move || {
                let _guard = gate.lock().unwrap();
                sent.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(10));
                panic!("interrupted durable operation");
            });
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            let waiter = scope.spawn(|| matches!(gate.lock(), Err(Error::Internal(_))));
            assert!(holder.join().is_err());
            assert!(waiter.join().unwrap());
        });
        assert!(gate.is_poisoned());
        assert!(matches!(gate.lock(), Err(Error::Internal(_))));
        assert_eq!(gate.active.load(Ordering::Acquire), 0);
    }
}
