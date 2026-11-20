//! Detect a newly observed feed boundary without turning sticky loss into polling.
use crate::store::change_feed::FeedStatus;
pub(super) fn required(previous: Option<FeedStatus>, current: FeedStatus) -> bool {
    match previous {
        Some(previous) => previous != current,
        None => current.gap_generation != 0 || current.closed.is_some(),
    }
}
