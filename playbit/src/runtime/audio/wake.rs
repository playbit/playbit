//! One coalesced Mach semaphore wake. Signaling from AudioQueue performs one
//! atomic exchange and at most one kernel call, without allocation or a mutex.
#![allow(unsafe_code, reason = "Owned Mach semaphore lifecycle and signal/wait ABI.")]
use playbit_api::abi::syscall::Error;
use std::sync::atomic::{AtomicBool, Ordering};

unsafe extern "C" {
    static mach_task_self_: u32;
    fn semaphore_create(task: u32, semaphore: *mut u32, policy: i32, value: i32) -> i32;
    fn semaphore_destroy(task: u32, semaphore: u32) -> i32;
    fn semaphore_signal(semaphore: u32) -> i32;
    fn semaphore_wait(semaphore: u32) -> i32;
}

pub(super) struct Wake {
    semaphore: u32,
    pending: AtomicBool,
}
impl Wake {
    pub(super) fn new() -> Result<Self, Error> {
        let mut semaphore = 0;
        // SAFETY: the current task owns the returned semaphore port. FIFO
        // policy and zero initial permits create an initially parked worker.
        let status = unsafe { semaphore_create(mach_task_self_, &mut semaphore, 0, 0) };
        if status != 0 {
            return Err(Error::Unavailable);
        }
        Ok(Self { semaphore, pending: AtomicBool::new(false) })
    }
    pub(super) fn notify(&self) -> bool {
        if self.pending.swap(true, Ordering::AcqRel) {
            return true;
        }
        // SAFETY: Shared ownership keeps the semaphore alive through every
        // callback and waiter; destruction follows worker/callback shutdown.
        let status = unsafe { semaphore_signal(self.semaphore) };
        if status != 0 {
            self.pending.store(false, Ordering::Release);
        }
        status == 0
    }
    pub(super) fn wait(&self) -> bool {
        loop {
            // SAFETY: only the owning device thread waits on this live port.
            let status = unsafe { semaphore_wait(self.semaphore) };
            if status == 14 {
                continue; // KERN_ABORTED: retry an interrupted worker wait.
            }
            if status != 0 {
                return false;
            }
            // Work is inspected after clearing pending. Notifications that
            // raced before the clear are covered by that inspection; later
            // notifications create the next single permit.
            self.pending.store(false, Ordering::Release);
            return true;
        }
    }
}
impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: final Shared destruction occurs off the audio callback,
        // after the worker and AudioQueue have relinquished their ownership.
        let status = unsafe { semaphore_destroy(mach_task_self_, self.semaphore) };
        if status != 0 {
            eprintln!("Native audio wake disposal failed (Mach status {status})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[repr(C)]
    struct Timespec {
        seconds: u32,
        nanoseconds: i32,
    }
    unsafe extern "C" {
        fn semaphore_timedwait(semaphore: u32, timeout: Timespec) -> i32;
    }

    #[test]
    fn wake_coalesces_and_can_rearm_without_accumulating_permits() {
        let wake = Wake::new().expect("test semaphore creation");
        for _ in 0..1024 {
            assert!(wake.notify());
        }
        assert!(wake.wait());
        // SAFETY: no other waiter exists; a zero timeout observes the permit
        // count directly without a timing-sensitive thread scheduling test.
        let status =
            unsafe { semaphore_timedwait(wake.semaphore, Timespec { seconds: 0, nanoseconds: 0 }) };
        assert_eq!(status, 49); // KERN_OPERATION_TIMED_OUT: exactly one permit.
        assert!(wake.notify());
        assert!(wake.wait());
    }
}
