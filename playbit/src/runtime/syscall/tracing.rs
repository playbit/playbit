//! Shared bounded batch ingestion; adapters supply trusted execution ownership.
use super::{HostError, HostResult, State};
use crate::trace::AppTraceError;
pub(in crate::runtime) fn record(
    state: &mut State,
    bytes: &[u8],
    generation: u64,
    lost: u64,
) -> HostResult<()> {
    state.tracing.record(bytes, generation, lost).map_err(|error| match error {
        AppTraceError::Invalid => HostError::InvalidArgument,
        AppTraceError::Unsupported => HostError::Unsupported,
        AppTraceError::Disabled => HostError::Unavailable,
        AppTraceError::Stale => HostError::StaleHandle,
        AppTraceError::Capacity | AppTraceError::Exhausted => HostError::LimitExceeded,
    })
}
