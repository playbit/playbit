//! Host notification tests exercise the same bounded queue without descriptors.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;

fn id(byte: u8) -> Id {
    Id::from_bytes([byte; 16])
}

fn feed() -> (CommittedChangeFeed, CommittedChangeReceiver, Arc<AtomicBool>) {
    let pending = Arc::new(AtomicBool::new(false));
    let wake = pending.clone();
    let feed = CommittedChangeFeed::with_wake(id(1), move || wake.store(true, Ordering::Release));
    let receiver = feed.take_receiver().unwrap();
    (feed, receiver, pending)
}

#[test]
fn coalesced_turns_rearm_until_the_queue_is_drained() {
    // Clear readiness before each bounded turn, as a host event loop must do.
    let (feed, mut receiver, pending) = feed();
    assert!(receiver.readiness().is_none());
    assert!(feed.take_receiver().is_none());
    for _ in 0..19 {
        feed.volume_available(id(2));
    }
    let mut seen = 0;
    for count in [8, 8, 3] {
        assert!(pending.swap(false, Ordering::AcqRel));
        let batch = receiver.drain();
        assert_eq!(batch.records.len(), count);
        assert_eq!(batch.status.gap_generation, 0);
        assert!(batch.status.closed.is_none());
        for record in &batch.records {
            seen += 1;
            assert_eq!(record.sequence(), seen);
        }
    }
    assert!(!pending.load(Ordering::Acquire));
    assert_eq!(feed.pending_count(), 0);
    feed.account_changed();
    assert!(pending.swap(false, Ordering::AcqRel));
    let batch = receiver.drain();
    assert!(batch.records.is_empty());
    assert_eq!(batch.account_generation, 1);
    assert!(!pending.load(Ordering::Acquire));
}

#[test]
fn overflow_and_closure_remain_observable_without_a_socket() {
    // Saturation cannot overwrite the sticky gap or prevent a terminal wake.
    let (feed, mut receiver, pending) = feed();
    for _ in 0..RECORD_LIMIT + 1 {
        feed.volume_available(id(2));
    }
    assert_eq!(feed.pending_count(), RECORD_LIMIT);
    assert!(pending.swap(false, Ordering::AcqRel));
    let retained = receiver.drain();
    assert_eq!(retained.records.len(), DRAIN_LIMIT);
    assert_eq!(retained.status.gap_generation, 1);
    assert_eq!(feed.pending_count(), RECORD_LIMIT, "leases keep their credits");
    drop(retained);
    assert_eq!(feed.pending_count(), RECORD_LIMIT - DRAIN_LIMIT);
    pending.store(false, Ordering::Release);
    drop(feed);
    assert!(pending.swap(false, Ordering::AcqRel));
    assert_eq!(receiver.drain().status.closed, Some(FeedClosure::SourceDropped));
    assert!(pending.load(Ordering::Acquire), "remaining records rearm after source closure");
}

#[test]
fn concurrent_producers_publish_before_signalling_the_host() {
    // A real blocked consumer is unparked without timers. Clearing the pending
    // flag first covers publication before, during and after a bounded drain.
    let pending = Arc::new(AtomicBool::new(false));
    let wake = pending.clone();
    let consumer = std::thread::current();
    let feed = CommittedChangeFeed::with_wake(id(1), move || {
        wake.store(true, Ordering::Release);
        consumer.unpark();
    });
    let mut receiver = feed.take_receiver().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let feed = &feed;
            scope.spawn(move || {
                for _ in 0..32 {
                    feed.volume_available(id(2));
                }
            });
        }
        // The timeout bounds a failed test, not production freshness polling.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.len() < 128 {
            if !pending.swap(false, Ordering::AcqRel) {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!remaining.is_zero(), "host wake was lost");
                std::thread::park_timeout(remaining);
                continue;
            }
            let batch = receiver.drain();
            assert_eq!(batch.status.gap_generation, 0);
            assert!(batch.status.closed.is_none());
            for record in &batch.records {
                assert!(seen.insert(record.sequence()));
            }
        }
    });
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), (1..=128).collect::<Vec<_>>());
    assert_eq!(feed.pending_count(), 0);
}

#[test]
fn wake_resources_outlive_both_endpoints_and_pending_records_release_on_close() {
    // Dropping the consumer cannot strand credits or release the host's wake
    // target while the source can still signal it during teardown.
    let (feed, mut receiver, pending) = feed();
    let weak = Arc::downgrade(&pending);
    for _ in 0..10 {
        feed.volume_available(id(2));
    }
    let retained = receiver.drain();
    drop(pending);
    drop(receiver);
    assert!(weak.upgrade().is_some());
    assert_eq!(feed.status().closed, Some(FeedClosure::ReceiverDropped));
    assert_eq!(feed.pending_count(), DRAIN_LIMIT);
    assert!(feed.take_receiver().is_none());
    drop(retained);
    assert_eq!(feed.pending_count(), 0);
    drop(feed);
    assert!(weak.upgrade().is_none());
}
