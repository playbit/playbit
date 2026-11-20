use playbit_api::context::{Context, ContextState};

// A separately compiled baseline consumer. The observer supplies future event
// kinds through the real scalar import and checks ordered known payloads.
#[unsafe(no_mangle)]
pub extern "C" fn probe_events() -> u64 {
    let mut state = ContextState::default();
    let (context, _) = match Context::guest(&mut state) {
        Ok(turn) => turn,
        Err(error) => return u64::from(error as u32),
    };
    let mut hash = 0u32;
    for event in context.raw_events() {
        hash = hash.wrapping_mul(31).wrapping_add(event.kind as u32);
        for byte in event.data {
            hash = hash.wrapping_mul(31).wrapping_add(u32::from(*byte));
        }
    }
    let status = context.event_status().err().map_or(0, |error| error as u32);
    (u64::from(hash) << 32) | u64::from(status)
}
