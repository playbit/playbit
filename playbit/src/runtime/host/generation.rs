//! Process-local instance generations. Zero is reserved; exhaustion never wraps.
use super::{HostError, HostResult};
#[cfg(any(test, not(target_has_atomic = "32")))]
use std::sync::Mutex;
#[cfg(target_has_atomic = "32")]
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(target_has_atomic = "32")]
static NEXT: AtomicU32 = AtomicU32::new(1);
#[cfg(not(target_has_atomic = "32"))]
static NEXT: Mutex<u32> = Mutex::new(1);

#[inline]
pub(super) fn allocate() -> HostResult<u32> {
    #[cfg(target_has_atomic = "32")]
    {
        NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
            .map_err(|_| HostError::LimitExceeded)
    }
    #[cfg(not(target_has_atomic = "32"))]
    {
        allocate_locked(&NEXT)
    }
}
#[cfg(any(test, not(target_has_atomic = "32")))]
fn allocate_locked(next: &Mutex<u32>) -> HostResult<u32> {
    // No callback, I/O, or allocation while holding the generation lock.
    take(&mut next.lock().expect("runtime generation lock poisoned"))
}
#[cfg(any(test, not(target_has_atomic = "32")))]
fn take(next: &mut u32) -> HostResult<u32> {
    assert_ne!(*next, 0, "generation allocator starts at one and never wraps");
    let generation = *next;
    *next = generation.checked_add(1).ok_or(HostError::LimitExceeded)?;
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locked_generations_are_unique_across_threads() {
        // Collect concurrent allocations and compare the full nonrepeating sequence.
        let start = 100;
        let next = Mutex::new(start);
        let mut generations = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let next = &next;
                    scope.spawn(move || {
                        (0..1000)
                            .map(|_| allocate_locked(next).expect("generation"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().expect("worker"))
                .collect::<Vec<_>>()
        });
        generations.sort_unstable();
        assert_eq!(generations, (start..start + 4000).collect::<Vec<_>>());
    }
    #[test]
    fn exhaustion_never_mutates_or_reuses_a_generation() {
        // Exhaust a local allocator and retry to check that failed attempts cannot reuse an ID.
        let next = Mutex::new(u32::MAX - 1);
        assert_eq!(allocate_locked(&next), Ok(u32::MAX - 1));
        for _ in 0..4 {
            assert_eq!(allocate_locked(&next), Err(HostError::LimitExceeded));
            assert_eq!(*next.lock().expect("generation lock"), u32::MAX);
        }
    }
    #[cfg(target_has_atomic = "32")]
    #[test]
    fn locked_and_atomic_allocation_agree_at_boundaries() {
        // Replay boundary transitions through both implementations and compare state and results.
        for start in [1, 1 << 31, u32::MAX - 1, u32::MAX] {
            let locked = Mutex::new(start);
            let atomic = AtomicU32::new(start);
            for _ in 0..3 {
                let expected = atomic
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
                    .map_err(|_| HostError::LimitExceeded);
                assert_eq!(allocate_locked(&locked), expected);
                assert_eq!(
                    *locked.lock().expect("generation lock"),
                    atomic.load(Ordering::Relaxed)
                );
            }
        }
    }
}
