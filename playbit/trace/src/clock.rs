//! Platform clock conversion, initialized on source bootstrap before emission.
#[cfg(target_vendor = "apple")]
use std::sync::OnceLock;

#[cfg(target_vendor = "apple")]
#[allow(
    unsafe_code,
    reason = "Native monotonic clock calls use initialized buffers with no retained pointers."
)]
pub(crate) fn timebase() -> (u64, u64) {
    use mach2::mach_time::{mach_timebase_info, mach_timebase_info_data_t};
    static SCALE: OnceLock<(u64, u64)> = OnceLock::new();
    let &(numer, denom) = SCALE.get_or_init(|| {
        let mut scale = mach_timebase_info_data_t { numer: 0, denom: 0 };
        // SAFETY: the API writes the initialized structure at this pointer.
        assert_eq!(unsafe { mach_timebase_info(&mut scale) }, 0);
        assert_ne!(scale.denom, 0);
        (u64::from(scale.numer), u64::from(scale.denom))
    });
    (numer, denom)
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code, reason = "The native monotonic clock retains no pointers.")]
pub(crate) fn now_ns() -> u64 {
    let (numer, denom) = timebase();
    // SAFETY: the platform clock takes no arguments and has no side effects.
    let ticks = unsafe { mach2::mach_time::mach_absolute_time() };
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}
#[cfg(all(unix, not(target_vendor = "apple")))]
#[allow(
    unsafe_code,
    reason = "Native monotonic clock calls use initialized buffers with no retained pointers."
)]
pub(crate) fn now_ns() -> u64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes one initialized timespec at this pointer.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    assert_eq!(result, 0, "monotonic clock unavailable");
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}
#[cfg(not(unix))]
pub(crate) fn now_ns() -> u64 {
    0
}
