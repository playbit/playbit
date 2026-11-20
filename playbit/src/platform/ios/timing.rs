//! Numeric, payload-free probes for the native UIKit host. No owner lock needed.

/// Returns zero until the Engine worker initialized its source. No I/O,
/// allocation, worker creation or clock probe when tracing is disabled.
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_trace_start() -> u64 {
    if crate::trace::is_recording() { crate::trace::clock_ns() } else { 0 }
}

/// `source_ns` comes from pb_ios_trace_start in this process. Records the end
/// observation as a numeric duration in nanoseconds.
/// Stage numbers are the IosTraceStage contract in ArchipelagoBridge.h.
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_trace_end(stage: u32, source_ns: u64) {
    if source_ns == 0 || !crate::trace::is_recording() {
        return;
    }
    let name = match stage {
        1 => "ios.sessions.layout",
        2 => "ios.sessions.labels",
        3 => "ios.consent.construction",
        4 => "ios.consent.snapshot",
        5 => "ios.consent.cell",
        6 => "ios.consent.choice",
        7 => "ios.consent.response",
        8 => "ios.worker_delivery",
        9 => "ios.artwork.apply",
        10 => "ios.artwork.ready",
        11 => "ios.new_session.layout",
        12 => "ios.new_session.controls",
        _ => return,
    };
    if let Some(duration_ns) = crate::trace::clock_ns().checked_sub(source_ns) {
        crate::trace::mark_value(name, duration_ns);
    }
}
