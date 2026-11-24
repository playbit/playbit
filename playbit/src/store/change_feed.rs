//! Native, single-consumer delivery of already committed changes.
//!
//! The caller must publish after successful durability, while retaining the
//! existing per-volume operation guard. Sequence numbers identify publications
//! within this feed epoch; they are NOT a recipient's gap-detection sequence.
//! Different volumes may enqueue in a different order than sequence allocation.
use crate::store::Id;
use crossbeam_queue::ArrayQueue;
use kvdb::sync::CommittedDelta;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};
#[cfg(feature = "local-store")]
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
};

pub(crate) const RECORD_LIMIT: usize = 1024;
const BYTE_LIMIT: usize = 4 * 1024 * 1024;
const DELTA_LIMIT: usize = crate::store::MAX_REPLICA_DELTA_BYTES;
const DRAIN_LIMIT: usize = 8;

/// Fixed compositions, not caller-selected unbounded queue sizes.
#[derive(Clone, Copy)]
struct Limits {
    records: usize,
    bytes: usize,
    delta: usize,
    drain: usize,
}
const DESKTOP: Limits =
    Limits { records: RECORD_LIMIT, bytes: BYTE_LIMIT, delta: DELTA_LIMIT, drain: DRAIN_LIMIT };

enum Readiness {
    Host,
    #[cfg(feature = "local-store")]
    Unix(UnixStream),
}

/// Terminal feed state. A new store/feed epoch is needed to resume delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FeedClosure {
    SourceDropped = 1,
    ReceiverDropped = 2,
    NotificationFailed = 3,
    SequenceExhausted = 4,
}

/// A fixed-size, persistent recovery boundary, even when every queue slot is full.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedStatus {
    pub epoch: Id,
    /// Monotonically increases on loss/failure. Never reset by draining. Consumers
    /// retain the generation they recovered and compare it with subsequent status.
    /// Exhaustion is terminal; neither this value nor publication sequence wraps.
    pub gap_generation: u64,
    pub closed: Option<FeedClosure>,
}

#[derive(Default)]
struct Budget {
    // Admitted records not yet handed to the consumer, including an enqueue
    // reservation. One atomic read avoids ArrayQueue::len's retry loop.
    undelivered: AtomicUsize,
    records: AtomicUsize,
    bytes: AtomicUsize,
}

/// One committed database delta addressed by its complete stable identity.
/// An object birth carries all initial fields in one source record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommittedChangeKind {
    Database(crate::store::DatabaseId),
    /// Local credential discovery only. Never serialized as an AP publication.
    VolumeAvailable,
}

impl CommittedChangeKind {
    fn affects_authority(self) -> bool {
        matches!(
            self,
            Self::Database(crate::store::DatabaseId::Index | crate::store::DatabaseId::Revocations)
        )
    }
}

/// Content snapshots still require uninterrupted current authorization.
#[cfg(any(
    test,
    feature = "native-runtime",
    feature = "wasm-runtime",
    feature = "file-provider",
    all(not(target_os = "ios"), feature = "process")
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryChanges {
    All,
    #[cfg(any(test, feature = "file-provider", all(not(target_os = "ios"), feature = "process")))]
    Authority,
}
#[cfg(any(
    test,
    feature = "native-runtime",
    feature = "wasm-runtime",
    feature = "file-provider",
    all(not(target_os = "ios"), feature = "process")
))]
impl DeliveryChanges {
    fn includes(self, authority: bool) -> bool {
        self == Self::All || authority
    }
}

/// Owns the payload and its record/allocated-capacity credits until dropped.
/// There is deliberately no operation that detaches the Vec from its credits.
pub struct CommittedChange {
    undelivered: bool,
    volume: Id,
    kind: CommittedChangeKind,
    sequence: u64,
    bytes: Vec<u8>,
    budget: Arc<Budget>,
}

impl CommittedChange {
    /// The caller reserved record and allocation credits before transferring
    /// ownership here. Failed enqueue and receiver closure drop this same owner.
    fn from_admitted(
        shared: &Shared,
        volume: Id,
        kind: CommittedChangeKind,
        sequence: u64,
        bytes: Vec<u8>,
    ) -> Self {
        shared.budget.undelivered.fetch_add(1, Ordering::AcqRel);
        Self { undelivered: true, volume, kind, sequence, bytes, budget: shared.budget.clone() }
    }
    pub fn volume(&self) -> Id {
        self.volume
    }
    pub fn kind(&self) -> CommittedChangeKind {
        self.kind
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn delivered(&mut self) {
        if std::mem::replace(&mut self.undelivered, false) {
            self.budget.undelivered.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for CommittedChange {
    fn drop(&mut self) {
        self.delivered();
        // Free the actual allocation before releasing its credit. Consumers can
        // retain this record across callbacks without defeating either limit.
        let capacity = self.bytes.capacity();
        drop(std::mem::take(&mut self.bytes));
        self.budget.bytes.fetch_sub(capacity, Ordering::AcqRel);
        self.budget.records.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The Unix host owns descriptors; an embedding host owns its event loop.
/// Neither adapter owns another queue or starts a worker.
enum Notification {
    #[cfg(feature = "local-store")]
    Unix(UnixStream),
    Host(Box<dyn Fn() + Send + Sync>),
}

struct Shared {
    epoch: Id,
    queue: ArrayQueue<CommittedChange>,
    budget: Arc<Budget>,
    // Native-width atomics keep counters portable. All counters
    // close on exhaustion; 32-bit feeds require a new epoch before 2^32 events.
    sequence: AtomicUsize,
    local_generation: AtomicUsize,
    gap_generation: AtomicUsize,
    account_generation: AtomicUsize,
    observed: Mutex<Observed>,
    // Provider upload badges are not source/authority changes. In particular,
    // acknowledging a lazy download must not invalidate that same data read.
    #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
    provider_observed: Mutex<Observed>,
    #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
    provider_epoch: Id,
    closed: AtomicU8,
    receiver_dropped: AtomicBool,
    notify: Notification,
    limits: Limits,
}

// One bounded history for delivery fencing, including incoming commits. It
// carries only volume IDs/revisions/classification, never payloads or outgoing work. None is a
// profile policy change that can affect every admitted context.
#[cfg(any(
    test,
    feature = "native-runtime",
    feature = "wasm-runtime",
    feature = "file-provider",
    all(not(target_os = "ios"), feature = "process")
))]
#[derive(Clone, Copy)]
struct ObservedChange {
    revision: u64,
    volume: Option<Id>,
    authority: bool,
}
#[cfg_attr(
    not(any(
        test,
        feature = "native-runtime",
        feature = "wasm-runtime",
        feature = "file-provider",
        all(not(target_os = "ios"), feature = "process")
    )),
    derive(Default)
)]
struct Observed {
    revision: usize,
    #[cfg(any(
        test,
        feature = "native-runtime",
        feature = "wasm-runtime",
        feature = "file-provider",
        all(not(target_os = "ios"), feature = "process")
    ))]
    history: [Option<ObservedChange>; RECORD_LIMIT],
}
const OBSERVED_HISTORY_BYTES_MAX: usize = 64 * 1024;
const _: () = assert!(std::mem::size_of::<Observed>() <= OBSERVED_HISTORY_BYTES_MAX);
#[cfg(any(
    test,
    feature = "native-runtime",
    feature = "wasm-runtime",
    feature = "file-provider",
    all(not(target_os = "ios"), feature = "process")
))]
impl Default for Observed {
    fn default() -> Self {
        Self { revision: 0, history: [None; RECORD_LIMIT] }
    }
}

#[cfg(any(
    test,
    feature = "native-runtime",
    feature = "wasm-runtime",
    feature = "file-provider",
    all(not(target_os = "ios"), feature = "process")
))]
fn retained_changes(before: u64, after: u64) -> Option<usize> {
    let count = after.checked_sub(before)?;
    (count <= RECORD_LIMIT as u64).then_some(count as usize)
}

impl Observed {
    fn record(&mut self, volume: Option<Id>, authority: bool) -> Option<()> {
        assert!(volume.is_some() || authority, "profile changes always affect authority");
        self.revision = self.revision.checked_add(1)?;
        #[cfg(any(
            test,
            feature = "native-runtime",
            feature = "wasm-runtime",
            feature = "file-provider",
            all(not(target_os = "ios"), feature = "process")
        ))]
        {
            let index = (self.revision - 1) % RECORD_LIMIT;
            self.history[index] =
                Some(ObservedChange { revision: self.revision as u64, volume, authority });
        }
        Some(())
    }
    #[cfg(any(
        test,
        feature = "native-runtime",
        feature = "wasm-runtime",
        feature = "file-provider",
        all(not(target_os = "ios"), feature = "process")
    ))]
    fn changed_since(
        &self,
        before: u64,
        volumes: Option<&[Id]>,
        changes: DeliveryChanges,
    ) -> Option<u64> {
        let after = self.revision as u64;
        let Some(count) = retained_changes(before, after) else {
            return Some(after); // Explicit history gap: refresh committed state.
        };
        let mut changed = None;
        for offset in 0..count {
            let revision = before + offset as u64 + 1;
            let index = ((revision - 1) % RECORD_LIMIT as u64) as usize;
            let Some(record) = self.history[index] else { return Some(after) };
            if record.revision != revision {
                return Some(after);
            }
            if changes.includes(record.authority)
                && volumes.is_none_or(|volumes| {
                    record.volume.is_none_or(|volume| volumes.contains(&volume))
                })
            {
                changed = Some(revision);
            }
        }
        changed
    }
}

impl Shared {
    fn close(&self, reason: FeedClosure) {
        let _ = self.closed.compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    fn gap(&self) {
        if self
            .gap_generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| old.checked_add(1))
            .is_err()
        {
            self.close(FeedClosure::SequenceExhausted);
        }
    }

    fn status(&self) -> FeedStatus {
        let closed = match self.closed.load(Ordering::Acquire) {
            0 => None,
            1 => Some(FeedClosure::SourceDropped),
            2 => Some(FeedClosure::ReceiverDropped),
            3 => Some(FeedClosure::NotificationFailed),
            4 => Some(FeedClosure::SequenceExhausted),
            _ => unreachable!("invalid feed closure"),
        };
        FeedStatus {
            epoch: self.epoch,
            gap_generation: self.gap_generation.load(Ordering::Acquire) as u64,
            closed,
        }
    }

    fn signal(&self) {
        match &self.notify {
            #[cfg(feature = "local-store")]
            Notification::Unix(notify) => self.signal_with(notify, || (&*notify).write(&[1])),
            Notification::Host(wake) => wake(),
        }
    }

    #[cfg(feature = "local-store")]
    fn signal_with(&self, notify: &UnixStream, mut write: impl FnMut() -> io::Result<usize>) {
        // One byte, nonblocking, with at most one interrupted-call retry. A full
        // socket is already readable; gap state does not live in its byte buffer.
        for _ in 0..2 {
            match write() {
                Ok(1) => return,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }
        self.gap();
        self.close(FeedClosure::NotificationFailed);
        // A failed write may not have armed readability. EOF provides the
        // terminal wake without another write, retry loop, thread or timer.
        let _ = notify.shutdown(std::net::Shutdown::Write);
    }

    fn release_queued(&self) {
        // Once the receiver is gone, in-flight producers also perform this
        // cleanup after enqueue. A record cannot be stranded by drop/enqueue.
        let mut discarded = false;
        for _ in 0..self.limits.records {
            if self.queue.pop().is_none() {
                break;
            }
            discarded = true;
        }
        if discarded {
            self.gap();
        }
    }
}

/// One source per store opening, with a fixed queue and one chosen wake adapter.
pub struct CommittedChangeFeed {
    shared: Arc<Shared>,
    // Used only to acquire the sole consumer, never by committed publication.
    // None means acquired; Host means no descriptor is allocated.
    receiver: Mutex<Option<Readiness>>,
}

impl CommittedChangeFeed {
    #[cfg(feature = "local-store")]
    pub fn new(epoch: Id) -> io::Result<Self> {
        let (notify, receiver) = UnixStream::pair()?;
        // Store teardown can notify after the consumer has closed its socket.
        // UIKit must receive an I/O failure rather than a fatal SIGPIPE.
        #[cfg(target_vendor = "apple")]
        rustix::net::sockopt::set_socket_nosigpipe(&notify, true)?;
        notify.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        Ok(Self::with_notification(
            epoch,
            DESKTOP,
            Notification::Unix(notify),
            Readiness::Unix(receiver),
        ))
    }

    /// Schedule the host's consumer without allocating notification descriptors.
    ///
    /// `wake` must be bounded, nonblocking and safe to call concurrently from
    /// producers and the consumer. It must arrange a later turn, never reenter
    /// storage or drain this feed synchronously: a producer may hold a volume
    /// guard. Coalescing is allowed; clear the host's pending flag BEFORE draining
    /// so a concurrent publication or a rearm is not lost. Wake failure must stop
    /// the host; returning without scheduling can strand committed changes.
    ///
    /// The callback can run on source drop and while a receiver drains remaining
    /// work. Its captured resources must outlive both endpoints; use owned state.
    pub fn with_wake(epoch: Id, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::with_notification(epoch, DESKTOP, Notification::Host(Box::new(wake)), Readiness::Host)
    }

    fn with_notification(
        epoch: Id,
        limits: Limits,
        notify: Notification,
        receiver: Readiness,
    ) -> Self {
        #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
        let provider_epoch = {
            let mut hash = blake3::Hasher::new_derive_key("app.playbit.provider.delivery-epoch");
            hash.update(epoch.as_bytes());
            let mut bytes = [0; 16];
            bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
            Id::from_bytes(bytes)
        };
        Self {
            shared: Arc::new(Shared {
                epoch,
                queue: ArrayQueue::new(limits.records),
                budget: Arc::new(Budget::default()),
                sequence: AtomicUsize::new(0),
                local_generation: AtomicUsize::new(0),
                gap_generation: AtomicUsize::new(0),
                account_generation: AtomicUsize::new(0),
                observed: Mutex::new(Observed::default()),
                #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
                provider_observed: Mutex::new(Observed::default()),
                #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
                provider_epoch,
                closed: AtomicU8::new(0),
                receiver_dropped: AtomicBool::new(false),
                notify,
                limits,
            }),
            receiver: Mutex::new(Some(receiver)),
        }
    }

    /// None means the consumer was already acquired, even if it was later closed.
    pub fn take_receiver(&self) -> Option<CommittedChangeReceiver> {
        self.receiver.lock().expect("feed receiver acquisition poisoned").take().map(|_readiness| {
            CommittedChangeReceiver {
                shared: self.shared.clone(),
                queue_gauge: crate::trace::Gauge::new(
                    "storage.feed.sampled_undelivered",
                    self.shared.budget.undelivered.load(Ordering::Acquire) as u64,
                ),
                records_gauge: crate::trace::Gauge::new(
                    "storage.feed.sampled_retained",
                    self.shared.budget.records.load(Ordering::Acquire) as u64,
                ),
                bytes_gauge: crate::trace::Gauge::new(
                    "storage.feed.sampled_allocated_bytes",
                    self.shared.budget.bytes.load(Ordering::Acquire) as u64,
                ),
                #[cfg(feature = "local-store")]
                readiness: _readiness,
            }
        })
    }

    /// Wake the existing account owner after a durable profile transaction.
    /// Payloads remain in its coalesced durable outbox, not a second RAM queue.
    #[cfg(feature = "local-store")]
    pub(crate) fn account_changed(&self) {
        self.observed_changed(None);
        if self
            .shared
            .account_generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |previous| previous.checked_add(1))
            .is_err()
        {
            self.shared.close(FeedClosure::SequenceExhausted);
        }
        self.shared.signal();
    }

    /// Local publication activity, including changes already drained or dropped
    /// after a gap. A received account name also schedules its central profile write.
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn publication_generation(&self) -> (u64, u64) {
        (
            self.shared.local_generation.load(Ordering::Acquire) as u64,
            self.shared.account_generation.load(Ordering::Acquire) as u64,
        )
    }

    /// Ephemeral delivery fence, including received commits and local policy
    /// adoption. The epoch belongs to this store owner, not its on-disk format.
    /// Exhaustion closes the feed and rejects further retained-data delivery.
    #[cfg(all(
        feature = "local-store",
        any(test, all(not(target_os = "ios"), feature = "process"))
    ))]
    pub(crate) fn delivery_revision(&self) -> Option<(Id, u64)> {
        let revision =
            self.shared.observed.lock().expect("delivery history poisoned").revision as u64;
        (self.shared.closed.load(Ordering::Acquire) == 0).then_some((self.shared.epoch, revision))
    }

    /// Read the ordered fence and scoped change history under one memory-only
    /// lock. Missing history/another epoch conservatively requests a refresh.
    #[cfg(all(
        feature = "local-store",
        any(
            test,
            feature = "native-runtime",
            feature = "wasm-runtime",
            all(not(target_os = "ios"), feature = "process")
        )
    ))]
    pub(crate) fn delivery_scope_revision(
        &self,
        previous: Option<(Id, u64)>,
        volumes: &[Id],
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.delivery_revision_for(
            &self.shared.observed,
            self.shared.epoch,
            previous,
            Some(volumes),
            DeliveryChanges::All,
        )
    }

    #[cfg(all(
        feature = "local-store",
        any(test, all(not(target_os = "ios"), feature = "process"))
    ))]
    pub(crate) fn delivery_snapshot_revision(
        &self,
        previous: Option<(Id, u64)>,
        volumes: &[Id],
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.delivery_revision_for(
            &self.shared.observed,
            self.shared.epoch,
            previous,
            Some(volumes),
            DeliveryChanges::Authority,
        )
    }

    /// Drive membership/labels can change in any index or profile transaction.
    /// Backing object and bloblog edits cannot change that catalog.
    #[cfg(all(
        feature = "local-store",
        any(test, all(feature = "file-provider", feature = "ios"))
    ))]
    pub(crate) fn delivery_catalog_revision(
        &self,
        previous: Option<(Id, u64)>,
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.delivery_revision_for(
            &self.shared.observed,
            self.shared.epoch,
            previous,
            None,
            DeliveryChanges::Authority,
        )
    }

    #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
    pub(crate) fn delivery_provider_revision(
        &self,
        previous: Option<(Id, u64)>,
        volumes: Option<&[Id]>,
    ) -> (Option<(Id, u64)>, Option<u64>) {
        self.delivery_revision_for(
            &self.shared.provider_observed,
            self.shared.provider_epoch,
            previous,
            volumes,
            DeliveryChanges::Authority,
        )
    }

    /// Receipt-only status wakes the existing owner without changing a guest's
    /// source cut, admission, authorization or retained file-read result.
    #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
    pub(crate) fn provider_status_changed(&self, volume: Id) {
        let mut observed = self.shared.provider_observed.lock().expect("provider history poisoned");
        if observed.record(Some(volume), true).is_none() {
            self.shared.close(FeedClosure::SequenceExhausted);
        }
        drop(observed);
        self.shared.signal();
    }

    /// Counted application bookkeeping changes Size, not the Finder catalog or
    /// guest data. Advance only the full receipt frontier, under the same bound.
    #[cfg(all(feature = "local-store", any(test, feature = "file-provider")))]
    pub(crate) fn payload_size_changed(&self, volume: Id) {
        let mut observed = self.shared.provider_observed.lock().expect("provider history poisoned");
        if observed.record(Some(volume), false).is_none() {
            self.shared.close(FeedClosure::SequenceExhausted);
        }
        drop(observed);
        self.shared.signal();
    }

    #[cfg(all(
        feature = "local-store",
        any(
            test,
            feature = "native-runtime",
            feature = "wasm-runtime",
            feature = "file-provider",
            all(not(target_os = "ios"), feature = "process")
        )
    ))]
    fn delivery_revision_for(
        &self,
        history: &Mutex<Observed>,
        history_epoch: Id,
        previous: Option<(Id, u64)>,
        volumes: Option<&[Id]>,
        changes: DeliveryChanges,
    ) -> (Option<(Id, u64)>, Option<u64>) {
        let observed = history.lock().expect("delivery history poisoned");
        if self.shared.closed.load(Ordering::Acquire) != 0 {
            return (None, None);
        }
        let revision = observed.revision as u64;
        let changed = match previous {
            Some((epoch, before)) if epoch == history_epoch => {
                observed.changed_since(before, volumes, changes)
            }
            _ => Some(revision),
        };
        (Some((history_epoch, revision)), changed)
    }

    /// Call after durable adoption while retaining its authority guard. This
    /// does not enqueue an outbound publication or echo received changes.
    #[cfg(feature = "local-store")]
    pub(crate) fn observed_changed(&self, volume: Option<Id>) {
        self.observe(volume, true);
    }

    /// A local policy commit has no replication record to wake its consumers.
    #[cfg(feature = "local-store")]
    pub(crate) fn local_policy_changed(&self) {
        self.observed_changed(None);
        self.shared.signal();
    }

    /// Incoming content has the same classification as local publication.
    /// Unknown policy/readiness changes use `observed_changed` instead.
    #[cfg(feature = "local-store")]
    pub(crate) fn observed_database_changed(&self, volume: Id, database: crate::store::DatabaseId) {
        self.observe(Some(volume), CommittedChangeKind::Database(database).affects_authority());
    }

    #[cfg(feature = "local-store")]
    fn observe(&self, volume: Option<Id>, authority: bool) {
        let mut observed = self.shared.observed.lock().expect("delivery history poisoned");
        if observed.record(volume, authority).is_none() {
            // Close while retaining the history lock: a reader cannot observe
            // an unrecorded commit through the exhaustion window.
            self.shared.close(FeedClosure::SequenceExhausted);
        }
        #[cfg(any(test, feature = "file-provider"))]
        {
            let mut provider =
                self.shared.provider_observed.lock().expect("provider history poisoned");
            if provider.record(volume, authority).is_none() {
                self.shared.close(FeedClosure::SequenceExhausted);
            }
        }
    }

    pub fn status(&self) -> FeedStatus {
        self.shared.status()
    }

    /// Includes records retained by projection and in-flight publication, not
    /// just records waiting in the queue. No storage access is required.
    pub fn pending_count(&self) -> usize {
        self.shared.budget.records.load(Ordering::Acquire)
    }

    pub fn publish(&self, volume: Id, database: crate::store::DatabaseId, delta: CommittedDelta) {
        self.publish_change(volume, CommittedChangeKind::Database(database), delta);
    }

    #[cfg(feature = "local-store")]
    pub(crate) fn volume_available(&self, volume: Id) {
        let _timing = crate::trace::Span::new("storage.change_feed.available");
        self.enqueue(volume, CommittedChangeKind::VolumeAvailable, Some(Vec::new()));
    }

    fn publish_change(&self, volume: Id, kind: CommittedChangeKind, delta: CommittedDelta) {
        self.enqueue(
            volume,
            kind,
            match delta {
                CommittedDelta::Bytes(bytes) => Some(bytes),
                CommittedDelta::PullRequired => None,
            },
        );
    }

    fn track_publication(&self, volume: Id, kind: CommittedChangeKind) {
        #[cfg(feature = "local-store")]
        if kind != CommittedChangeKind::VolumeAvailable {
            self.observe(Some(volume), kind.affects_authority());
        }
        let shared = &self.shared;
        if kind != CommittedChangeKind::VolumeAvailable
            && shared
                .local_generation
                .try_update(Ordering::AcqRel, Ordering::Acquire, |old| old.checked_add(1))
                .is_err()
        {
            shared.close(FeedClosure::SequenceExhausted);
        }
    }

    fn enqueue(&self, volume: Id, kind: CommittedChangeKind, bytes: Option<Vec<u8>>) {
        let _timing = crate::trace::Span::new("storage.change_feed.publish");
        let shared = &self.shared;
        self.track_publication(volume, kind);
        if shared.closed.load(Ordering::Acquire) != 0 {
            shared.gap();
            shared.signal();
            return;
        }
        let sequence = shared
            .sequence
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| old.checked_add(1));
        let Ok(previous) = sequence else {
            shared.gap();
            shared.close(FeedClosure::SequenceExhausted);
            shared.signal();
            return;
        };
        let Some(bytes) = bytes else {
            shared.gap();
            shared.signal();
            return;
        };
        let capacity = bytes.capacity();
        if bytes.len() > shared.limits.delta || capacity > shared.limits.bytes {
            shared.gap();
            shared.signal();
            return;
        }
        if shared
            .budget
            .records
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old < shared.limits.records).then_some(old + 1)
            })
            .is_err()
        {
            shared.gap();
            shared.signal();
            return;
        }
        if shared
            .budget
            .bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (capacity <= shared.limits.bytes - old).then_some(old + capacity)
            })
            .is_err()
        {
            shared.budget.records.fetch_sub(1, Ordering::AcqRel);
            shared.gap();
            shared.signal();
            return;
        }
        let record =
            CommittedChange::from_admitted(shared, volume, kind, previous as u64 + 1, bytes);
        if shared.queue.push(record).is_err() {
            shared.gap();
        }
        if shared.receiver_dropped.load(Ordering::Acquire) {
            shared.release_queued();
        }
        shared.signal();
    }
}

// Reject before allocating the grouped payload, including its four-byte header.

impl Drop for CommittedChangeFeed {
    fn drop(&mut self) {
        self.shared.close(FeedClosure::SourceDropped);
        self.shared.signal();
    }
}

pub struct CommittedChangeReceiver {
    shared: Arc<Shared>,
    queue_gauge: crate::trace::Gauge,
    records_gauge: crate::trace::Gauge,
    bytes_gauge: crate::trace::Gauge,
    #[cfg(feature = "local-store")]
    readiness: Readiness,
}

pub struct ChangeBatch {
    pub account_generation: u64,
    pub records: Vec<CommittedChange>,
    pub status: FeedStatus,
}

impl CommittedChangeReceiver {
    /// Register this socket for readability; only drain() should read it. Do not
    /// clone it: the Unix adapter owns exactly one nonblocking socket pair.
    /// A host-wake feed has no descriptor; its host schedules drain() directly.
    #[cfg(feature = "local-store")]
    pub fn readiness(&self) -> Option<&UnixStream> {
        match &self.readiness {
            Readiness::Host => None,
            Readiness::Unix(socket) => Some(socket),
        }
    }

    /// Consume bounded wake data and at most the composition's record limit.
    /// A producer signals after publishing; rearming covers work left by this turn.
    /// Status is sticky, so queue or notification saturation cannot erase a gap.
    pub fn drain(&mut self) -> ChangeBatch {
        self.sample_gauges();
        #[cfg(feature = "local-store")]
        if let Readiness::Unix(readiness) = &self.readiness {
            self.clear_readiness(readiness);
        }
        let mut records = Vec::with_capacity(self.shared.limits.drain);
        for _ in 0..self.shared.limits.drain {
            let Some(mut record) = self.shared.queue.pop() else {
                break;
            };
            record.delivered();
            records.push(record);
        }
        if !self.shared.queue.is_empty() {
            self.shared.signal();
        }
        self.sample_gauges();
        ChangeBatch {
            records,
            status: self.shared.status(),
            account_generation: self.shared.account_generation.load(Ordering::Acquire) as u64,
        }
    }

    /// Independent consumer observations, not a coherent cross-counter cut or
    /// an exact transient peak. Retained credits include work outside the queue.
    pub(crate) fn sample_gauges(&mut self) {
        self.queue_gauge.set(self.shared.budget.undelivered.load(Ordering::Acquire) as u64);
        self.records_gauge.set(self.shared.budget.records.load(Ordering::Acquire) as u64);
        self.bytes_gauge.set(self.shared.budget.bytes.load(Ordering::Acquire) as u64);
    }

    #[cfg(feature = "local-store")]
    fn clear_readiness(&self, mut readiness: &UnixStream) {
        let mut wake = [0; RECORD_LIMIT];
        for _ in 0..2 {
            match readiness.read(&mut wake) {
                Ok(0) => {
                    self.shared.gap();
                    self.shared.close(FeedClosure::NotificationFailed);
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.shared.gap();
                    self.shared.close(FeedClosure::NotificationFailed);
                }
            }
            break;
        }
    }
}

impl Drop for CommittedChangeReceiver {
    fn drop(&mut self) {
        self.shared.receiver_dropped.store(true, Ordering::Release);
        self.shared.close(FeedClosure::ReceiverDropped);
        self.shared.release_queued();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, sync::Barrier};

    fn id(byte: u8) -> Id {
        Id::from_bytes([byte; 16])
    }
    fn bytes(len: usize) -> CommittedDelta {
        CommittedDelta::Bytes(vec![7; len])
    }
    fn feed() -> (CommittedChangeFeed, CommittedChangeReceiver) {
        let feed = CommittedChangeFeed::new(id(1)).unwrap();
        let receiver = feed.take_receiver().unwrap();
        (feed, receiver)
    }

    fn ready(receiver: &CommittedChangeReceiver) -> bool {
        match receiver.readiness().unwrap().read(&mut [0]) {
            Ok(1) => true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
            other => panic!("unexpected readiness: {other:?}"),
        }
    }
    fn retain_all(receiver: &mut CommittedChangeReceiver) -> Vec<CommittedChange> {
        let mut result = Vec::new();
        loop {
            let batch = receiver.drain();
            assert!(batch.records.len() <= DRAIN_LIMIT);
            if batch.records.is_empty() {
                return result;
            }
            result.extend(batch.records);
        }
    }

    #[test]
    #[ignore = "optimized local capability notification benchmark; no storage or AP"]
    fn benchmark_capability_notification() {
        let (feed, mut receiver) = feed();
        let mut times = Vec::with_capacity(1000);
        for sample in 0..1100 {
            let start = std::time::Instant::now();
            feed.volume_available(id(2));
            let elapsed = start.elapsed().as_nanos();
            let batch = receiver.drain();
            assert_eq!(batch.records.len(), 1);
            assert_eq!(batch.status.gap_generation, 0);
            if sample >= 100 {
                times.push(elapsed);
            }
        }
        times.sort_unstable();
        crate::diagnostic!(
            "capability-notification samples=1000 p50_ns={} p95_ns={} p99_ns={} max_ns={}",
            times[499],
            times[949],
            times[989],
            times[999]
        );
        assert!(times[989] < 20_000);
    }

    #[test]
    fn outgoing_activity_excludes_received_capability_discovery_and_survives_gaps() {
        let (feed, _receiver) = feed();
        feed.volume_available(id(2));
        assert_eq!(feed.publication_generation(), (0, 0));
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            CommittedDelta::PullRequired,
        );
        assert_eq!(feed.publication_generation(), (1, 0));
        assert!(feed.status().gap_generation > 0);
        feed.account_changed();
        assert_eq!(feed.publication_generation(), (1, 1));
    }

    #[test]
    fn received_delivery_fences_do_not_create_outgoing_publications() {
        let (feed, mut receiver) = feed();
        let before = feed.delivery_revision().unwrap();
        feed.observed_changed(None);
        let after = feed.delivery_revision().unwrap();
        assert_eq!(after.0, before.0);
        assert!(after.1 > before.1);
        assert_eq!(feed.publication_generation(), (0, 0));
        assert!(receiver.drain().records.is_empty());
        assert_eq!(feed.pending_count(), 0);
        feed.shared.observed.lock().unwrap().revision = usize::MAX;
        feed.observed_changed(None);
        assert!(feed.delivery_revision().is_none());
        assert_eq!(feed.status().closed, Some(FeedClosure::SequenceExhausted));
    }

    #[test]
    fn provider_status_has_an_independent_scoped_fence_without_invalidating_reads() {
        // Compare source, Finder and full receipt frontiers across real provider
        // status, Size-only bookkeeping, backing edits and bounded history loss.
        use crate::store::{DatabaseId, EntryId};
        let (feed, mut receiver) = feed();
        let source = feed.delivery_revision().unwrap();
        let provider = feed.delivery_provider_revision(None, None).0.unwrap();
        assert_ne!(provider.0, source.0);
        feed.provider_status_changed(id(2));
        assert_eq!(feed.delivery_revision(), Some(source));
        assert_eq!(feed.delivery_scope_revision(Some(source), &[id(2)]), (Some(source), None));
        assert_eq!(feed.delivery_snapshot_revision(Some(source), &[id(2)]), (Some(source), None));
        assert_eq!(feed.delivery_catalog_revision(Some(source)), (Some(source), None));
        let current = feed.delivery_provider_revision(Some(provider), None).0.unwrap();
        assert_eq!(current.1, provider.1 + 1);
        assert_eq!(
            feed.delivery_provider_revision(Some(provider), Some(&[id(2)])).1,
            Some(current.1)
        );
        assert_eq!(feed.delivery_provider_revision(Some(provider), Some(&[id(3)])).1, None);
        assert_eq!(feed.delivery_provider_revision(Some(current), None).1, None);
        // Interchanging cursors always refreshes, even if independent counters coincide.
        assert_eq!(feed.delivery_provider_revision(Some(source), None).1, Some(current.1));
        assert_eq!(feed.delivery_catalog_revision(Some(current)).1, Some(source.1));
        assert_eq!(feed.publication_generation(), (0, 0));
        assert!(receiver.drain().records.is_empty());
        assert_eq!(feed.pending_count(), 0);

        feed.payload_size_changed(id(2));
        let size = feed.delivery_provider_revision(Some(current), None).0.unwrap();
        assert_ne!(size, current);
        assert_eq!(feed.delivery_provider_revision(Some(current), None).1, None);
        assert_eq!(feed.delivery_provider_revision(Some(current), Some(&[id(2)])).1, None);
        assert_eq!(feed.delivery_revision(), Some(source));
        assert_eq!(feed.delivery_scope_revision(Some(source), &[id(2)]), (Some(source), None));
        assert_eq!(feed.publication_generation(), (0, 0));
        assert!(receiver.drain().records.is_empty());

        let entry = EntryId::from_bytes([3; 15]).unwrap();
        feed.observed_database_changed(id(2), DatabaseId::Entry(entry));
        assert_eq!(feed.delivery_provider_revision(Some(current), None).1, None);
        assert!(feed.delivery_scope_revision(Some(source), &[id(2)]).1.is_some());
        for database in [DatabaseId::Index, DatabaseId::Revocations] {
            let before = feed.delivery_provider_revision(None, None).0;
            feed.observed_database_changed(id(2), database);
            assert!(feed.delivery_provider_revision(before, Some(&[id(2)])).1.is_some());
            assert!(feed.delivery_snapshot_revision(Some(source), &[id(2)]).1.is_some());
        }
        let before_gap = feed.delivery_provider_revision(None, None).0;
        for _ in 0..=RECORD_LIMIT {
            feed.provider_status_changed(id(2));
        }
        assert!(feed.delivery_provider_revision(before_gap, Some(&[id(3)])).1.is_some());
        feed.shared.provider_observed.lock().unwrap().revision = usize::MAX;
        feed.provider_status_changed(id(2));
        assert_eq!(feed.delivery_provider_revision(None, None), (None, None));
        assert!(feed.delivery_revision().is_none());
        assert_eq!(feed.status().closed, Some(FeedClosure::SequenceExhausted));
    }

    #[test]
    fn mounted_changes_and_profile_policy_are_scoped_without_outgoing_echo() {
        let (feed, mut receiver) = feed();
        let before = feed.delivery_revision().unwrap();
        feed.observed_changed(Some(id(2)));
        let (ordered, changed) = feed.delivery_scope_revision(Some(before), &[id(3)]);
        assert_eq!(ordered.unwrap().1, before.1 + 1);
        assert_eq!(changed, None);
        assert_eq!(feed.delivery_scope_revision(Some(before), &[id(2)]).1, Some(before.1 + 1));
        feed.observed_changed(Some(id(4)));
        assert_eq!(feed.delivery_scope_revision(Some(before), &[id(2)]).1, Some(before.1 + 1));
        assert_eq!(feed.delivery_scope_revision(Some(before), &[id(3)]).1, None);
        feed.observed_changed(None);
        assert_eq!(feed.delivery_scope_revision(Some(before), &[id(3)]).1, Some(before.1 + 3));
        assert_eq!(feed.publication_generation(), (0, 0));
        assert!(receiver.drain().records.is_empty());
    }

    #[test]
    fn content_snapshots_ignore_only_backing_changes_and_notifications_keep_them() {
        // Classify actual local publications and incoming adoptions in the same
        // history; every control/profile change still fences a scoped snapshot.
        use crate::store::{DatabaseId, EntryId};
        let (feed, mut receiver) = feed();
        let before = feed.delivery_revision().unwrap();
        let entry = EntryId::from_bytes([3; 15]).unwrap();
        feed.publish(id(2), DatabaseId::Entry(entry), bytes(1));
        feed.publish(id(2), DatabaseId::Entry(entry), bytes(2));
        feed.observed_database_changed(id(2), DatabaseId::Entry(entry));
        assert_eq!(feed.delivery_snapshot_revision(Some(before), &[id(2)]).1, None);
        assert_eq!(feed.delivery_catalog_revision(Some(before)).1, None);
        assert_eq!(feed.delivery_scope_revision(Some(before), &[id(2)]).1, Some(before.1 + 3));
        assert_eq!(feed.publication_generation(), (2, 0));
        assert_eq!(receiver.drain().records.len(), 2, "incoming adoption has no outbound echo");
        for database in [DatabaseId::Index, DatabaseId::Revocations] {
            feed.observed_database_changed(id(2), database);
            assert_eq!(
                feed.delivery_snapshot_revision(Some(before), &[id(2)]).1,
                Some(feed.delivery_revision().unwrap().1)
            );
        }
        let before = feed.delivery_revision().unwrap();
        feed.observed_changed(Some(id(3)));
        assert_eq!(feed.delivery_snapshot_revision(Some(before), &[id(2)]).1, None);
        assert_eq!(feed.delivery_catalog_revision(Some(before)).1, Some(before.1 + 1));
        feed.observed_changed(None);
        assert_eq!(feed.delivery_snapshot_revision(Some(before), &[id(2)]).1, Some(before.1 + 2));
        crate::diagnostic!("delivery history resident bytes: {}", std::mem::size_of::<Observed>());
    }

    #[test]
    fn snapshot_authority_requires_exact_retained_history_and_a_live_epoch() {
        // A content-only horizon is admissible exactly through the retained record limit.
        // Overflow, missing/corrupt history, future cuts and closure fail closed.
        use crate::store::{DatabaseId, EntryId};
        let (feed, _receiver) = feed();
        let before = feed.delivery_revision().unwrap();
        for _ in 0..RECORD_LIMIT {
            feed.observed_database_changed(
                id(2),
                DatabaseId::Entry(EntryId::from_bytes([3; 15]).unwrap()),
            );
        }
        let latest = feed.delivery_revision().unwrap();
        assert_eq!(latest.1, RECORD_LIMIT as u64);
        assert_eq!(feed.delivery_catalog_revision(Some(before)).1, None);
        assert_eq!(feed.delivery_catalog_revision(Some((id(9), 0))).1, Some(latest.1));
        assert_eq!(feed.delivery_snapshot_revision(Some(before), &[id(2)]).1, None);
        assert_eq!(feed.delivery_snapshot_revision(Some((id(9), 0)), &[id(2)]).1, Some(latest.1));
        assert_eq!(
            feed.delivery_snapshot_revision(Some((latest.0, latest.1 + 1)), &[id(2)]).1,
            Some(latest.1)
        );
        {
            let mut observed = feed.shared.observed.lock().unwrap();
            let retained = observed.history[0];
            observed.history[0] = None;
            assert_eq!(
                observed.changed_since(before.1, Some(&[id(2)]), DeliveryChanges::Authority),
                Some(latest.1)
            );
            observed.history[0] = retained;
            observed.history[0].as_mut().unwrap().revision += 1;
            assert_eq!(
                observed.changed_since(before.1, Some(&[id(2)]), DeliveryChanges::Authority),
                Some(latest.1)
            );
            observed.history[0] = retained;
        }
        feed.observed_database_changed(
            id(2),
            DatabaseId::Entry(EntryId::from_bytes([3; 15]).unwrap()),
        );
        assert_eq!(feed.delivery_snapshot_revision(Some(before), &[id(2)]).1, Some(latest.1 + 1));
        assert_eq!(feed.delivery_catalog_revision(Some(before)).1, Some(latest.1 + 1));
        feed.shared.close(FeedClosure::ReceiverDropped);
        assert_eq!(feed.delivery_snapshot_revision(Some(latest), &[id(2)]), (None, None));
        assert_eq!(feed.delivery_catalog_revision(Some(latest)), (None, None));
        assert_eq!(feed.delivery_scope_revision(Some(latest), &[id(2)]), (None, None));
    }

    #[test]
    fn concurrent_scoped_history_retains_exact_horizon_and_reports_a_gap() {
        let (feed, _receiver) = feed();
        let before = feed.delivery_revision().unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let feed = &feed;
                scope.spawn(move || {
                    for _ in 0..RECORD_LIMIT / 8 {
                        feed.observed_changed(Some(id(2)));
                    }
                });
            }
        });
        let (current, changed) = feed.delivery_scope_revision(Some(before), &[id(3)]);
        assert_eq!(current.unwrap().1, RECORD_LIMIT as u64);
        assert_eq!(changed, None);
        feed.observed_changed(Some(id(2)));
        assert_eq!(
            feed.delivery_scope_revision(Some(before), &[id(3)]).1,
            Some(RECORD_LIMIT as u64 + 1)
        );
        assert_eq!(
            feed.delivery_scope_revision(Some((id(9), 0)), &[id(3)]).1,
            Some(RECORD_LIMIT as u64 + 1)
        );
    }

    #[test]
    fn local_discovery_shares_record_capacity_without_a_changeset_payload() {
        let (feed, mut receiver) = feed();
        for _ in 0..RECORD_LIMIT {
            feed.volume_available(id(2));
        }
        assert!(ready(&receiver));
        assert_eq!(feed.pending_count(), RECORD_LIMIT);
        assert_eq!(feed.shared.budget.bytes.load(Ordering::Acquire), 0);
        let retained = retain_all(&mut receiver);
        assert_eq!(retained.len(), RECORD_LIMIT);
        assert!(
            retained.iter().all(|record| record.kind() == CommittedChangeKind::VolumeAvailable
                && record.bytes().is_empty())
        );
        feed.publish(
            id(3),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(4),
        );
        assert_eq!(
            feed.status().gap_generation,
            1,
            "discovery credits also bound concurrent ordinary changes"
        );
        assert!(receiver.drain().records.is_empty());
        drop(retained);
        assert_eq!(feed.pending_count(), 0);
        feed.publish(
            id(3),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(4),
        );
        assert_eq!(receiver.drain().records.len(), 1);
    }

    #[test]
    fn ordered_delivery_readiness_and_bounded_rearm() {
        let (feed, mut receiver) = feed();
        assert!(feed.take_receiver().is_none());
        assert!(!ready(&receiver));
        for _ in 0..10 {
            feed.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                bytes(4),
            );
        }
        assert!(ready(&receiver));
        let first = receiver.drain();
        assert_eq!(first.records.len(), 8);
        assert_eq!(first.status.gap_generation, 0);
        assert_eq!(first.status.epoch, id(1));
        assert!(first.status.closed.is_none());
        assert!(ready(&receiver));
        let second = receiver.drain();
        assert_eq!(second.records.len(), 2);
        assert!(!ready(&receiver));
        for (index, record) in first.records.iter().chain(&second.records).enumerate() {
            assert_eq!(record.sequence(), index as u64 + 1);
            assert_eq!(
                (record.volume(), record.kind(), record.bytes()),
                (
                    id(2),
                    CommittedChangeKind::Database(crate::store::DatabaseId::Entry(
                        crate::store::EntryId::from_bytes([3; 15]).unwrap()
                    )),
                    &[7; 4][..]
                )
            );
        }
    }

    #[test]
    fn record_limit_includes_consumer_held_leases() {
        let (feed, mut receiver) = feed();
        assert_eq!(feed.pending_count(), 0);
        for _ in 0..RECORD_LIMIT {
            feed.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                bytes(1),
            );
        }
        let mut held = retain_all(&mut receiver);
        assert_eq!(held.len(), RECORD_LIMIT);
        assert_eq!(feed.pending_count(), RECORD_LIMIT);
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        let batch = receiver.drain();
        assert!(batch.records.is_empty());
        assert_eq!(batch.status.gap_generation, 1);
        drop(held.pop());
        assert_eq!(feed.pending_count(), RECORD_LIMIT - 1);
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        assert_eq!(receiver.drain().records.len(), 1);
        drop(held);
        assert_eq!(feed.pending_count(), 0);
        assert_eq!(feed.shared.budget.records.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.undelivered.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn byte_limit_counts_allocated_capacity_until_actual_release() {
        let (feed, mut receiver) = feed();
        for _ in 0..BYTE_LIMIT / DELTA_LIMIT {
            let mut payload = Vec::with_capacity(DELTA_LIMIT);
            payload.push(1);
            feed.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                CommittedDelta::Bytes(payload),
            );
        }
        let mut held = retain_all(&mut receiver);
        assert_eq!(held.len(), BYTE_LIMIT / DELTA_LIMIT);
        assert_eq!(feed.shared.budget.bytes.load(Ordering::Acquire), BYTE_LIMIT);
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        assert_eq!(receiver.drain().status.gap_generation, 1);
        drop(held.pop());
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(DELTA_LIMIT),
        );
        assert_eq!(receiver.drain().records.len(), 1);
        drop(held);
        assert_eq!(feed.shared.budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn oversized_and_pull_required_are_sticky_gaps_without_records() {
        let (feed, mut receiver) = feed();
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(DELTA_LIMIT + 1),
        );
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            CommittedDelta::Bytes(Vec::with_capacity(BYTE_LIMIT + 1)),
        );
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            CommittedDelta::PullRequired,
        );
        assert!(ready(&receiver));
        let batch = receiver.drain();
        assert!(batch.records.is_empty());
        assert_eq!(batch.status.gap_generation, 3);
        assert_eq!(receiver.drain().status.gap_generation, 3);
        assert_eq!(feed.shared.budget.records.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.undelivered.load(Ordering::Acquire), 0);
    }

    #[test]
    fn gap_survives_full_queue_and_concurrent_drain() {
        let (feed, mut receiver) = feed();
        for _ in 0..RECORD_LIMIT {
            feed.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                bytes(4),
            );
        }
        let start = Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let feed = &feed;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..128 {
                        feed.publish(
                            id(3),
                            crate::store::DatabaseId::Entry(
                                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                            ),
                            CommittedDelta::PullRequired,
                        );
                    }
                });
            }
            start.wait();
            drop(retain_all(&mut receiver));
        });
        assert_eq!(receiver.drain().status.gap_generation, 512);
        assert_eq!(feed.shared.budget.records.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.undelivered.load(Ordering::Acquire), 0);
    }

    #[test]
    fn concurrent_producers_preserve_all_records_and_wake_after_drain() {
        let (feed, mut receiver) = feed();
        let start = Barrier::new(5);
        std::thread::scope(|scope| {
            for volume in 2..6 {
                let feed = &feed;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..32 {
                        feed.publish(
                            id(volume),
                            crate::store::DatabaseId::Entry(
                                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                            ),
                            bytes(4),
                        );
                    }
                });
            }
            start.wait();
        });
        assert!(ready(&receiver));
        let held = retain_all(&mut receiver);
        assert_eq!(held.len(), 128);
        assert_eq!(feed.shared.budget.undelivered.load(Ordering::Acquire), 0);
        assert_eq!(held.iter().map(CommittedChange::sequence).collect::<BTreeSet<_>>().len(), 128);
        for volume in 2..6 {
            let seq: Vec<_> = held
                .iter()
                .filter(|r| r.volume() == id(volume))
                .map(CommittedChange::sequence)
                .collect();
            assert!(seq.windows(2).all(|w| w[0] < w[1]));
        }
        assert_eq!(feed.status().gap_generation, 0);
        assert!(!ready(&receiver));
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        assert!(ready(&receiver));
        assert_eq!(receiver.drain().records.len(), 1);
    }

    #[test]
    fn publication_racing_drain_is_returned_or_remains_ready() {
        let (feed, mut receiver) = feed();
        let start = Barrier::new(2);
        let (published, after_publish) = std::sync::mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..128 {
                    start.wait();
                    feed.publish(
                        id(2),
                        crate::store::DatabaseId::Entry(
                            crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                        ),
                        bytes(4),
                    );
                    published.send(()).unwrap();
                }
            });
            for sequence in 1..=128 {
                start.wait();
                let mut batch = receiver.drain();
                after_publish.recv().unwrap();
                if batch.records.is_empty() {
                    // The producer has finished enqueue and notification. If
                    // this callback missed it, readiness must still be armed.
                    assert!(ready(&receiver));
                    batch = receiver.drain();
                }
                assert_eq!(batch.records.len(), 1);
                assert_eq!(batch.records[0].sequence(), sequence);
                assert_eq!(batch.status.gap_generation, 0);
                drop(batch);
                assert!(receiver.drain().records.is_empty());
                assert!(!ready(&receiver));
            }
        });
    }

    #[test]
    fn notification_saturation_cannot_erase_global_gap() {
        let (feed, mut receiver) = feed();
        for _ in 0..4096 {
            feed.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                CommittedDelta::PullRequired,
            );
        }
        let batch = receiver.drain();
        assert!(batch.records.is_empty());
        assert_eq!(batch.status.gap_generation, 4096);
        assert!(batch.status.closed.is_none());
    }

    #[test]
    fn receiver_closure_releases_queue_but_not_outstanding_leases() {
        let (feed, mut receiver) = feed();
        for _ in 0..10 {
            feed.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                bytes(4),
            );
        }
        let held = receiver.drain().records;
        drop(receiver);
        assert!(feed.take_receiver().is_none());
        assert_eq!(feed.status().closed, Some(FeedClosure::ReceiverDropped));
        assert_eq!(feed.shared.budget.records.load(Ordering::Acquire), 8);
        assert!(feed.status().gap_generation > 0); // The two queued records were abandoned.
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(4),
        );
        assert!(feed.status().gap_generation > 0);
        drop(held);
        assert_eq!(feed.shared.budget.records.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.undelivered.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn receiver_drop_racing_producers_leaves_no_retained_queue() {
        let (feed, receiver) = feed();
        let start = Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let feed = &feed;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..128 {
                        feed.publish(
                            id(2),
                            crate::store::DatabaseId::Entry(
                                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                            ),
                            bytes(4),
                        );
                    }
                });
            }
            start.wait();
            drop(receiver);
        });
        assert_eq!(feed.status().closed, Some(FeedClosure::ReceiverDropped));
        assert!(feed.shared.queue.is_empty());
        assert_eq!(feed.shared.budget.records.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.undelivered.load(Ordering::Acquire), 0);
        assert_eq!(feed.shared.budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn source_closure_is_ready_and_remaining_records_are_drainable() {
        let (feed, mut receiver) = feed();
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(4),
        );
        drop(feed);
        assert!(ready(&receiver));
        let batch = receiver.drain();
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.status.closed, Some(FeedClosure::SourceDropped));
    }

    #[test]
    fn failed_notification_arms_eof_even_when_no_byte_was_written() {
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::Other] {
            let (feed, mut receiver) = feed();
            assert!(!ready(&receiver));
            let mut attempts = 0;
            let Notification::Unix(notify) = &feed.shared.notify else {
                panic!("test requires the Unix notification adapter");
            };
            feed.shared.signal_with(notify, || {
                attempts += 1;
                Err(io::Error::from(kind))
            });
            assert_eq!(attempts, if kind == io::ErrorKind::Interrupted { 2 } else { 1 });
            // The socket was healthy and empty before the injected write error;
            // without terminal shutdown this read would be WouldBlock.
            assert_eq!(receiver.readiness().unwrap().read(&mut [0]).unwrap(), 0);
            let status = receiver.drain().status;
            assert_eq!(status.closed, Some(FeedClosure::NotificationFailed));
            assert!(status.gap_generation > 0);
        }
    }

    #[test]
    fn broken_notification_endpoint_is_explicit_without_failing_publish() {
        let (feed, mut receiver) = feed();
        receiver.readiness().unwrap().shutdown(std::net::Shutdown::Read).unwrap();
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(4),
        );
        let batch = receiver.drain();
        assert_eq!(batch.status.closed, Some(FeedClosure::NotificationFailed));
        assert!(batch.status.gap_generation > 0);
        assert_eq!(batch.records.len(), 1);
    }

    #[test]
    fn sequence_exhaustion_never_wraps() {
        let (feed, mut receiver) = feed();
        feed.shared.sequence.store(usize::MAX - 1, Ordering::Release);
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        let batch = receiver.drain();
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].sequence(), usize::MAX as u64);
        assert_eq!(batch.status.closed, Some(FeedClosure::SequenceExhausted));
        assert_eq!(batch.status.gap_generation, 1);
        feed.shared.gap_generation.store(usize::MAX, Ordering::Release);
        feed.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            bytes(1),
        );
        assert_eq!(receiver.drain().status.gap_generation, usize::MAX as u64);
    }
}

#[cfg(test)]
mod host_wake_tests;
