//! One installed AP subscription, then bounded durable receive on its owner's runtime.
//! The caller admits at most 512 scopes, finishes bootstrap before start(), and
//! supplies the account's shared two-permit apply semaphore. No freshness timers.
// Error ownership: RunNotice owns terminal errors; checkpoint waiters get
// explicit failure and admitted applies retain credit to durable completion.
// Tests cover malformed input, gaps, canceled waiters and apply ordering, not
// every store/transport failure at every call. A lost reply is never a durable
// acknowledgment.
use super::completion::Wake;
use crate::engine::Result;
use crate::network::ap::{
    ApConnection, ApReceiveStream, ApSendStream, ApStream,
    operation::ServiceRequest,
    replication::{
        Cursor, CursorError, ReplicaEvent, ReplicaEventKind, SubscriptionAck, TerminalCause,
    },
    service_client as wire,
};
use crate::store::{Access, Id, LocalStore, Session, volume_index::EntryAddress};
use std::{
    fmt::{self, Write as _},
    future::{Future, poll_fn},
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
#[cfg(test)]
use std::{io::Write, os::unix::net::UnixStream};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

pub(crate) const NOTICE_LIMIT: usize = super::live::SCOPE_LIMIT;
const ERROR_LIMIT: usize = 128;
const CHECKPOINT_TIMEOUT: &str = "replica checkpoint response deadline exceeded";

/// Account and volume subscriptions share capacity and lifecycle bookkeeping;
/// no account ID can alias a volume ID in the local registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReplicaSource {
    Account,
    Volume(Id),
    Entry(EntryAddress),
}

#[derive(Clone, Debug)]
pub enum ReplicaScope {
    Account,
    Volume { volume: Id, session: Option<Session> },
    Entry { address: EntryAddress, session: Option<Session> },
}
impl ReplicaScope {
    pub fn source(&self) -> ReplicaSource {
        match self {
            Self::Account => ReplicaSource::Account,
            Self::Volume { volume, .. } => ReplicaSource::Volume(*volume),
            Self::Entry { address, .. } => ReplicaSource::Entry(*address),
        }
    }
    fn access(&self, account: Id) -> Access {
        match self {
            Self::Volume { session: Some(session), .. }
            | Self::Entry { session: Some(session), .. } => {
                Access::session_version(account, &session.id, &session.resolved_build_id)
            }
            _ => Access::owner(account),
        }
    }
}

pub struct PreparedSubscription {
    connection: Arc<ApConnection>,
    scope: ReplicaScope,
    stream: ApStream,
    ack: SubscriptionAck,
}

/// Subscription admission/acknowledgment completes before the caller pulls.
pub async fn subscribe(
    connection: &Arc<ApConnection>,
    scope: ReplicaScope,
) -> Result<PreparedSubscription> {
    let operation_id = Id::generate()?.to_string();
    let request = match &scope {
        ReplicaScope::Account => ServiceRequest::SubscribeAccount { operation_id },
        ReplicaScope::Volume { volume, session } => ServiceRequest::SubscribeReplica {
            entry_scope: None,
            operation_id,
            session_id: session.as_ref().map(|s| s.id.clone()),
            build_id: session.as_ref().map(|s| s.resolved_build_id.clone()),
            volume_id: volume.to_string(),
        },
        ReplicaScope::Entry { address, session } => ServiceRequest::SubscribeReplica {
            entry_scope: Some(address.entry),
            operation_id,
            session_id: session.as_ref().map(|s| s.id.clone()),
            build_id: session.as_ref().map(|s| s.resolved_build_id.clone()),
            volume_id: address.volume.to_string(),
        },
    };
    let (stream, ack) = wire::subscribe_replica(connection, &request).await?;
    Ok(PreparedSubscription { connection: connection.clone(), scope, stream, ack })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaState {
    CatchingUp,
    Live,
    Gap,
    Revoked,
    Disconnected,
    Failed,
}

// Wire validation, service rejection and local errors must not request a
// bootstrap. Only typed transport failures may take the reconnect path.
fn stream_failure(error: &(dyn std::error::Error + 'static)) -> ReplicaState {
    use crate::network::ap::ClientError;
    if error.downcast_ref::<ClientError>().is_some_and(ClientError::is_transport_lost) {
        ReplicaState::Disconnected
    } else {
        ReplicaState::Failed
    }
}

#[derive(Debug)]
pub struct ReplicaNotice {
    pub source: ReplicaSource,
    pub epoch: [u8; 16],
    pub dirty: bool,
    pub state: ReplicaState,
    pub error: Option<String>,
}
struct NoticeSlot {
    notice: ReplicaNotice,
    pending: bool,
    receiving: bool,
    applies: usize,
}

/// Fixed account-level capacity; drain moves bounded errors instead of copying them.
/// Slots retain their epochs across drains to reject stale task state updates.
pub struct ReplicaNotices {
    slots: Mutex<Vec<NoticeSlot>>,
    inactive: Notify,
    control_slots: Arc<Semaphore>,
}
impl Default for ReplicaNotices {
    fn default() -> Self {
        Self::new()
    }
}
impl ReplicaNotices {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::with_capacity(NOTICE_LIMIT)),
            inactive: Notify::new(),
            control_slots: Arc::new(Semaphore::new(8)),
        }
    }

    /// Shared with the publisher: eight commands total, including requests
    /// whose original waiter was canceled but whose work has not settled.
    pub(crate) fn control_slots(&self) -> Arc<Semaphore> {
        self.control_slots.clone()
    }

    fn register(&self, source: ReplicaSource, epoch: [u8; 16]) -> Result<()> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        assert!(slots.len() <= NOTICE_LIMIT, "replica notice admission stays bounded");
        if let Some(slot) = slots.iter_mut().find(|s| s.notice.source == source) {
            if slot.notice.epoch == epoch {
                return Err("replica subscription already started".into());
            }
            if slot.receiving || slot.applies != 0 {
                return Err("previous replica subscription has not stopped".into());
            }
            // Retain dirty even when replacing a canceled stream's generation.
            slot.notice.epoch = epoch;
            slot.notice.state = ReplicaState::CatchingUp;
            slot.notice.error = None;

            slot.pending = true;
            slot.receiving = true;
        } else {
            if slots.len() == NOTICE_LIMIT {
                return Err("replica notice capacity exceeded".into());
            }
            slots.push(NoticeSlot {
                notice: ReplicaNotice {
                    source,
                    epoch,
                    dirty: false,
                    state: ReplicaState::CatchingUp,
                    error: None,
                },
                pending: true,
                receiving: true,
                applies: 0,
            });
        }
        Ok(())
    }

    /// Read current state without consuming the owner's coalesced dirty notice.
    pub(crate) fn state(&self, source: ReplicaSource) -> Option<([u8; 16], ReplicaState)> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.iter().find(|s| s.notice.source == source).map(|s| (s.notice.epoch, s.notice.state))
    }

    /// Exhausting recovery is terminal for this exact Gap, even before the
    /// publisher observes cancellation. Never overwrite a successor or revocation.
    pub(crate) fn fail_gap(&self, source: ReplicaSource, epoch: [u8; 16]) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.iter_mut().find(|slot| {
            slot.notice.source == source
                && slot.notice.epoch == epoch
                && slot.notice.state == ReplicaState::Gap
        }) else {
            return false;
        };
        slot.notice.state = ReplicaState::Failed;

        #[cfg(feature = "test-support")]
        crate::diagnostic!("replica {source:?} exhausted its gap recovery attempts");
        // Retain the existing diagnostic and dirty bit without allocating.
        // The recovery caller returns the exhaustion reason separately.
        slot.pending = true;
        true
    }

    #[cfg(test)]
    pub(crate) fn test_state(&self, source: ReplicaSource, epoch: [u8; 16], state: ReplicaState) {
        if self.state(source).is_none_or(|(current, _)| current != epoch) {
            self.register(source, epoch).expect("the test fixture has capacity for its replica");
        }
        // These controller fixtures install independent observations, including
        // stale epochs and terminal siblings; they do not simulate receiver transitions.
        // Production transitions are exercised through update/start instead.
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot =
            slots.iter_mut().find(|s| s.notice.source == source).expect("registered fixture");
        slot.notice.state = state;
        slot.pending = true;
        slot.receiving = false;
    }

    /// Model an already canceled receiver whose admitted blocking apply is
    /// still running. Completion uses the same dirty/apply retirement path.
    #[cfg(all(test, feature = "file-provider"))]
    pub(crate) fn test_held_apply(
        self: &Arc<Self>,
        source: ReplicaSource,
        epoch: [u8; 16],
        completion: Arc<Wake>,
    ) -> impl FnOnce() + Send + 'static {
        self.register(source, epoch).expect("fixture source fits notice capacity");
        self.begin_apply(source, epoch).expect("fixture receiver admits one apply");
        self.stop_receiving(source, epoch);
        let notices = self.clone();
        let activity = ApplyNotice { notices: notices.clone(), completion, source };
        move || {
            notices.update(source, epoch, true, None, None);
            drop(activity);
        }
    }

    /// Forget an inactive scope only after its receive task AND any admitted
    /// blocking apply have finished and durable dirty work has been consumed.
    #[cfg(any(test, feature = "file-provider"))]
    pub fn forget(&self, source: ReplicaSource, epoch: [u8; 16]) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = slots.iter().position(|s| {
            s.notice.source == source
                && s.notice.epoch == epoch
                && !s.receiving
                && s.applies == 0
                && !s.notice.dirty
        }) {
            slots.swap_remove(index);
            true
        } else {
            false
        }
    }

    fn stop_receiving(&self, source: ReplicaSource, epoch: [u8; 16]) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) =
            slots.iter_mut().find(|s| s.notice.source == source && s.notice.epoch == epoch)
        {
            slot.receiving = false;
        }
        drop(slots);
        self.inactive.notify_waiters();
    }

    /// The owner aborts and joins the receiving task, then awaits this before
    /// preparing another bootstrap. A canceled spawn_blocking await does not
    /// cancel its durable write. Keep the old slot/dirty bit for the replacement.
    /// Only LiveReplicas::retire calls this in production, through its exclusive
    /// &mut account owner. There is at most one retirement waiter per account.
    pub(crate) async fn wait_inactive(&self, source: ReplicaSource, epoch: [u8; 16]) -> Result<()> {
        loop {
            let changed = self.inactive.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
                let slot = slots
                    .iter()
                    .find(|s| s.notice.source == source)
                    .ok_or("replica subscription was forgotten before retirement")?;
                if slot.notice.epoch != epoch {
                    return Err("replica subscription changed before retirement".into());
                }
                if !slot.receiving && slot.applies == 0 {
                    return Ok(());
                }
            }
            changed.await;
        }
    }

    fn begin_apply(&self, source: ReplicaSource, epoch: [u8; 16]) -> Result<()> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots
            .iter_mut()
            .find(|s| s.notice.source == source && s.notice.epoch == epoch)
            .ok_or("replica subscription replaced")?;
        assert!(slot.receiving, "only an installed receiver can admit an apply");
        assert_eq!(slot.applies, 0, "each receiver waits for its previous durable apply");
        slot.applies += 1; // Account-wide semaphore bounds all slots together to two.
        Ok(())
    }

    fn end_apply(&self, source: ReplicaSource) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots
            .iter_mut()
            .find(|s| s.notice.source == source)
            .expect("an admitted apply retains its slot through completion");
        assert_eq!(slot.applies, 1, "each apply releases its retained credit once");
        slot.applies -= 1;
        drop(slots);
        self.inactive.notify_waiters();
    }

    pub fn drain(&self) -> Vec<ReplicaNotice> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        assert!(slots.len() <= NOTICE_LIMIT, "replica notice drain stays bounded");
        let mut output = Vec::with_capacity(slots.iter().filter(|s| s.pending).count());
        for slot in slots.iter_mut().filter(|s| s.pending) {
            output.push(ReplicaNotice {
                source: slot.notice.source,
                epoch: slot.notice.epoch,
                dirty: std::mem::take(&mut slot.notice.dirty),
                state: slot.notice.state,
                error: slot.notice.error.take(),
            });
            slot.pending = false;
        }
        output
    }

    fn update(
        &self,
        source: ReplicaSource,
        epoch: [u8; 16],
        dirty: bool,
        state: Option<ReplicaState>,
        error: Option<&dyn fmt::Display>,
    ) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.iter_mut().find(|s| s.notice.source == source) else {
            return false;
        };
        // A completed durable merge still changes this source after cancellation.
        // Only its state/error belongs to the generation that issued it.
        slot.notice.dirty |= dirty;
        let current = slot.notice.epoch == epoch;
        if current {
            if let Some(state) = state {
                assert!(
                    slot.notice.state == state
                        || (matches!(
                            slot.notice.state,
                            ReplicaState::CatchingUp | ReplicaState::Live
                        ) && state != ReplicaState::CatchingUp),
                    "a replica generation cannot regress or resume terminal delivery"
                );
                slot.notice.state = state;
            }
            if let Some(error) = error {
                #[cfg(feature = "test-support")]
                crate::diagnostic!("replica {source:?} {state:?}: {error}");
                slot.notice.error = Some(bounded_error(error));
            }
        }
        let accepted = dirty || (current && (state.is_some() || error.is_some()));

        slot.pending |= accepted;
        slot.pending
    }
}

fn bounded_error(error: &dyn fmt::Display) -> String {
    struct Bounded {
        text: String,
        truncated: bool,
    }
    impl fmt::Write for Bounded {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            let mut end = value.len().min(ERROR_LIMIT - self.text.len());
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            self.truncated |= end < value.len();
            self.text.push_str(&value[..end]);
            Ok(())
        }
    }
    let mut out = Bounded { text: String::with_capacity(ERROR_LIMIT), truncated: false };
    let _ = write!(&mut out, "{error}");
    if out.truncated {
        let mut end = out.text.len().min(ERROR_LIMIT - 3);
        while !out.text.is_char_boundary(end) {
            end -= 1;
        }
        out.text.truncate(end);
        out.text.push('…');
    }
    out.text
}

/// Concrete pending+drained storage, including concurrently replaced error
/// text and the single account owner's retirement future. Shared control
/// semaphore allocation is charged by the separate control budget.
#[cfg(test)]
pub(crate) fn notice_allocation_bytes() -> usize {
    let notices = ReplicaNotices::new();
    std::mem::size_of::<ReplicaNotices>()
        + 2 * std::mem::size_of::<usize>()
        + std::mem::size_of::<Vec<ReplicaNotice>>()
        + NOTICE_LIMIT
            * (std::mem::size_of::<NoticeSlot>()
                + std::mem::size_of::<ReplicaNotice>()
                + 2 * ERROR_LIMIT)
        + ERROR_LIMIT
        + std::mem::size_of_val(
            &notices.wait_inactive(ReplicaSource::Volume(Id::from_bytes([0; 16])), [0; 16]),
        )
}

struct RunNotice {
    notices: Arc<ReplicaNotices>,
    completion: Arc<Wake>,
    source: ReplicaSource,
    epoch: [u8; 16],
    terminal: bool,
}
impl RunNotice {
    fn state(&mut self, state: ReplicaState, error: Option<&dyn fmt::Display>) {
        assert!(!self.terminal, "a receiver publishes its terminal transition only once");
        self.terminal = matches!(
            state,
            ReplicaState::Gap
                | ReplicaState::Revoked
                | ReplicaState::Disconnected
                | ReplicaState::Failed
        );
        if self.notices.update(self.source, self.epoch, false, Some(state), error) {
            self.completion.notify();
        }
    }
}
impl Drop for RunNotice {
    fn drop(&mut self) {
        if !self.terminal {
            self.state(ReplicaState::Disconnected, Some(&"replica receive canceled"));
        }
        self.notices.stop_receiving(self.source, self.epoch);
        self.completion.notify();
    }
}

struct ApplyNotice {
    notices: Arc<ReplicaNotices>,
    completion: Arc<Wake>,
    source: ReplicaSource,
}
impl Drop for ApplyNotice {
    fn drop(&mut self) {
        self.notices.end_apply(self.source);
        self.completion.notify();
    }
}

struct CheckpointRequest {
    reply: Option<oneshot::Sender<std::result::Result<(), &'static str>>>,
    _credit: OwnedSemaphorePermit,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CheckpointPhase {
    #[default]
    Open,
    Draining,
    Closed,
}
#[derive(Default)]
struct CheckpointSlot {
    request: Option<CheckpointRequest>,
    deadline: Option<tokio::time::Instant>,
    phase: CheckpointPhase,
    issued: bool,
    applying: bool,
    waker: Option<std::task::Waker>,
}
struct CheckpointMailbox {
    timeout: Duration,
    slot: Mutex<CheckpointSlot>,
    credits: Arc<Semaphore>,
}
impl CheckpointMailbox {
    fn request(&self) -> Result<oneshot::Receiver<std::result::Result<(), &'static str>>> {
        let credit = self
            .credits
            .clone()
            .try_acquire_owned()
            .map_err(|_| "replica control capacity exceeded")?;
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.phase == CheckpointPhase::Closed {
            return Err("replica subscription disconnected".into());
        }
        if slot.request.is_some() {
            return Err("replica checkpoint already outstanding".into());
        }
        let (reply, receiver) = oneshot::channel();
        if slot.phase == CheckpointPhase::Open {
            slot.deadline = Some(
                tokio::time::Instant::now()
                    .checked_add(self.timeout)
                    .ok_or("replica checkpoint deadline overflow")?,
            );
        } else {
            // Join the ordered terminal without sending or extending its wait.
            assert!(slot.deadline.is_some(), "a terminal drain retains its response deadline");
        }
        slot.request = Some(CheckpointRequest { reply: Some(reply), _credit: credit });
        let waker = slot.waker.take();
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(receiver)
    }
    fn take(&self) -> bool {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.phase != CheckpointPhase::Open || slot.issued || slot.request.is_none() {
            return false;
        }
        slot.issued = true;
        true
    }
    // There is exactly one receiving task. A mailbox mutation wakes its
    // registered poll directly; no second Notify waiter/future is retained.
    fn register_waker(&self, waker: &std::task::Waker) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.waker.as_ref().is_none_or(|previous| !previous.will_wake(waker)) {
            slot.waker = Some(waker.clone());
        }
    }
    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).deadline
    }
    fn is_open(&self) -> bool {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).phase == CheckpointPhase::Open
    }
    fn has_request(&self) -> bool {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.phase == CheckpointPhase::Open && !slot.issued && slot.request.is_some()
    }
    fn settle(&self, result: std::result::Result<(), &'static str>) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!slot.applying, "a checkpoint cannot overtake its preceding durable apply");
        assert_eq!(
            slot.phase,
            CheckpointPhase::Open,
            "only an open stream acknowledges checkpoints"
        );
        slot.deadline = None;
        slot.issued = false;
        if let Some(mut request) = slot.request.take()
            && let Some(reply) = request.reply.take()
        {
            crate::trace::mark(if result.is_ok() {
                "replica.checkpoint.complete"
            } else {
                "replica.checkpoint.failed"
            });
            let _ = reply.send(result);
        }
        let waker = slot.waker.take();
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    fn close(&self, error: &'static str) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.phase = CheckpointPhase::Closed;
        if let Some(request) = slot.request.as_mut()
            && let Some(reply) = request.reply.take()
        {
            crate::trace::mark("replica.checkpoint.failed");
            let _ = reply.send(Err(error));
        }
        // An abort or timeout cannot free this command's credit while its
        // preceding admitted disk write is still executing.
        if !slot.applying {
            slot.request = None;
        }
        let waker = slot.waker.take();
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    fn begin_apply(self: &Arc<Self>) -> CheckpointApply {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!slot.applying, "a checkpoint cannot overtake its preceding durable apply");
        slot.applying = true;
        CheckpointApply(self.clone())
    }
}
struct CheckpointApply(Arc<CheckpointMailbox>);
impl Drop for CheckpointApply {
    fn drop(&mut self) {
        let mut slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
        assert!(slot.applying, "an apply releases its checkpoint ownership once");
        slot.applying = false;
        if slot.phase == CheckpointPhase::Closed {
            slot.request = None;
        }
    }
}

/// One outstanding checkpoint per source. Clones refer to the same exact AP
/// epoch; replacement cannot turn an old waiter's startup notice into success.
#[derive(Clone)]
pub struct ReplicaControl {
    mailbox: Arc<CheckpointMailbox>,
}
impl ReplicaControl {
    pub async fn checkpoint(&self) -> Result<()> {
        let _interval = crate::trace::Interval::new("replica.checkpoint");
        self.mailbox
            .request()?
            .await
            .map_err(|_| "replica subscription disconnected")?
            .map_err(Into::into)
    }
}
struct Checkpoints {
    mailbox: Arc<CheckpointMailbox>,
    requested: u64,
    observed: u64,
}
impl Checkpoints {
    fn new(timeout: Duration, credits: Arc<Semaphore>) -> Self {
        Self {
            mailbox: Arc::new(CheckpointMailbox {
                timeout,
                slot: Mutex::new(CheckpointSlot::default()),
                credits,
            }),
            requested: 1,
            observed: 0,
        }
    }
    fn receive_after_write_error(
        &mut self,
        error: &(dyn std::error::Error + 'static),
    ) -> Result<bool> {
        if !error
            .downcast_ref::<crate::network::ap::ClientError>()
            .is_some_and(crate::network::ap::ClientError::is_write_stopped)
        {
            return Ok(false);
        }
        self.stop_writing()?;
        Ok(true)
    }
    fn stop_writing(&mut self) -> Result<()> {
        let mut slot = self.mailbox.slot.lock().unwrap_or_else(|e| e.into_inner());
        assert_ne!(slot.phase, CheckpointPhase::Closed, "a retired stream cannot start draining");
        // AP can send a terminal and stop its read half before we send a
        // checkpoint. Preserve the ordered receive and any admitted reply until
        // that terminal arrives. Never extend an already-admitted deadline.
        if slot.deadline.is_none() {
            slot.deadline = Some(
                tokio::time::Instant::now()
                    .checked_add(self.mailbox.timeout)
                    .ok_or("replica checkpoint deadline overflow")?,
            );
        }
        slot.phase = CheckpointPhase::Draining;
        crate::trace::mark("replication.checkpoint_write_stopped");
        Ok(())
    }
    fn issue(&mut self) -> Result<()> {
        let ordinal =
            self.requested.checked_add(1).ok_or("replica checkpoint identity exhausted")?;
        self.requested = ordinal;
        Ok(())
    }
    fn fail_timeout(&mut self) {
        self.mailbox.close(CHECKPOINT_TIMEOUT);
    }
    /// Called only for a fresh, contiguous event, after prior durable applies.
    fn caught_up(&mut self) -> Result<bool> {
        let slot = self.mailbox.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Err(CHECKPOINT_TIMEOUT.into());
        }
        if self.observed >= self.requested {
            return Err("unsolicited replica checkpoint".into());
        }
        self.observed += 1;
        // There is at most one explicit request, always the last requested
        // ordinal. Do not retain a redundant ordinal in each subscription.
        Ok(slot.phase == CheckpointPhase::Open
            && self.observed == self.requested
            && self.requested > 1)
    }
}
impl Drop for Checkpoints {
    fn drop(&mut self) {
        self.mailbox.close("replica subscription disconnected");
    }
}

#[cfg(test)]
pub(crate) fn control_allocation_bytes(commands: usize) -> usize {
    assert!(commands <= 8);
    32 * (std::mem::size_of::<CheckpointMailbox>()
        + 2 * std::mem::size_of::<usize>()
        + std::mem::size_of::<Checkpoints>()
        + std::mem::size_of::<ReplicaControl>())
        + commands * 256
        + std::mem::size_of::<Semaphore>()
        + 2 * std::mem::size_of::<usize>()
}

// The frame future is pinned once by the caller and borrowed on every control
// turn. A mailbox wake never cancels its partial header/body reads.
async fn frame_or_control<F: Future>(
    mut frame: std::pin::Pin<&mut F>,
    mailbox: &CheckpointMailbox,
) -> Option<F::Output> {
    poll_fn(|cx| {
        mailbox.register_waker(cx.waker());
        if let Poll::Ready(result) = frame.as_mut().poll(cx) {
            return Poll::Ready(Some(result));
        }
        if mailbox.has_request() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

// A protocol response deadline, armed only for an admitted explicit command.
// The caller passes its existing pinned frame or borrowed JoinHandle: mailbox
// changes never cancel/restart framing, and timeout does not cancel a disk job.
async fn checkpoint_deadline<F: Future>(
    future: F,
    mailbox: &CheckpointMailbox,
    mut timer: std::pin::Pin<&mut tokio::time::Sleep>,
) -> std::result::Result<F::Output, &'static str> {
    tokio::pin!(future);
    poll_fn(|cx| {
        // Register before reading the deadline so admission cannot fall between
        // observing no command and suspending an in-progress send/read/apply.
        mailbox.register_waker(cx.waker());
        if let Some(deadline) = mailbox.deadline() {
            // Check before polling an already-ready late marker.
            if tokio::time::Instant::now() >= deadline {
                return Poll::Ready(Err(CHECKPOINT_TIMEOUT));
            }
            timer.as_mut().reset(deadline);
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(CHECKPOINT_TIMEOUT));
            }
        }
        // When idle, timer is never polled/registered with Tokio.
        future.as_mut().poll(cx).map(Ok)
    })
    .await
}

impl PreparedSubscription {
    pub fn epoch(&self) -> [u8; 16] {
        self.ack.epoch
    }

    /// Register synchronously after durable bootstrap, then spawn the returned
    /// future on the existing runtime. Dropping even an unpolled future closes
    /// its mailbox/notice; admitted applies retain their permits until completion.
    pub fn start(
        self,
        store: Arc<LocalStore>,
        completion: Arc<Wake>,
        notices: Arc<ReplicaNotices>,
        apply_slots: Arc<Semaphore>,
    ) -> Result<(ReplicaControl, impl Future<Output = Result<()>> + Send)> {
        notices.register(self.scope.source(), self.ack.epoch)?;
        let checkpoints = Checkpoints::new(self.stream.response_timeout(), notices.control_slots());
        let control = ReplicaControl { mailbox: checkpoints.mailbox.clone() };
        let mut notice = RunNotice {
            notices,
            completion,
            source: self.scope.source(),
            epoch: self.ack.epoch,
            terminal: false,
        };
        notice.state(ReplicaState::CatchingUp, None);
        Ok((control, self.run(store, apply_slots, notice, checkpoints)))
    }

    fn run(
        self,
        store: Arc<LocalStore>,
        apply_slots: Arc<Semaphore>,
        notice: RunNotice,
        checkpoints: Checkpoints,
    ) -> impl Future<Output = Result<()>> + Send {
        let PreparedSubscription { connection, scope, stream, ack } = self;
        let access = scope.access(store.account().id);
        let cursor =
            Cursor::new(&ack).expect("subscription acknowledgment validated before installation");
        let (send, receive) = stream.split();
        SubscriptionReceiver {
            connection,
            scope,
            store,
            apply_slots,
            access,
            cursor,
            send,
            receive,
            notice,
            checkpoints,
            apply: None,
        }
        .run()
    }
}

/// The installed receive future owns the existing stream and its apply handle.
/// The blocking apply itself owns credits through actual durable completion.
struct SubscriptionReceiver {
    connection: Arc<ApConnection>,
    scope: ReplicaScope,
    store: Arc<LocalStore>,
    apply_slots: Arc<Semaphore>,
    access: Access,
    cursor: Cursor,
    send: ApSendStream,
    receive: ApReceiveStream,
    notice: RunNotice,
    checkpoints: Checkpoints,
    apply: Option<tokio::task::JoinHandle<crate::store::Result<bool>>>,
}
impl SubscriptionReceiver {
    fn run(self) -> impl Future<Output = Result<()>> + Send {
        let mut receiver = self;
        async move {
            // One inline timer per subscription, unregistered until a checkpoint.
            let timer = tokio::time::sleep_until(tokio::time::Instant::now());
            tokio::pin!(timer);
            let result = receiver.receive_frames(timer.as_mut()).await;
            // Terminal exits reset partial framing. Normal controls retain the same
            // pinned read future inside receive_frame.
            receiver.send.cancel();
            receiver.receive.cancel();
            receiver.finish_receive(&result);
            // A deadline cannot undo an already admitted transaction. The original
            // job retains its apply permit, dirty notice and mailbox credit.
            if let Some(apply) = receiver.apply {
                let _ = apply.await;
            }
            result
        }
    }
    async fn receive_frames(
        &mut self,
        mut timer: std::pin::Pin<&mut tokio::time::Sleep>,
    ) -> Result<()> {
        send_checkpoint(&mut self.send, &mut self.checkpoints, &mut self.notice, timer.as_mut())
            .await?;
        loop {
            // No next read until the retained frame's apply ends.
            let bytes = receive_frame(
                &mut self.send,
                &mut self.receive,
                &mut self.checkpoints,
                &mut self.notice,
                timer.as_mut(),
            )
            .await?;
            let _receipt = crate::trace::Interval::new("replica.receive");
            let event = ReplicaEvent::decode(&bytes)?;
            if !accept_event(&mut self.cursor, &event, &mut self.notice)? {
                continue;
            }
            match event.kind {
                ReplicaEventKind::Gap => {
                    unreachable!("accept_event returns the terminal error")
                }
                ReplicaEventKind::CaughtUp => self.caught_up(&event)?,
                ReplicaEventKind::Bloblog { data: [], .. } => {
                    receive_bloblog(
                        &self.connection,
                        &self.scope,
                        &self.store,
                        &self.apply_slots,
                        &mut self.notice,
                        &self.checkpoints,
                        timer.as_mut(),
                    )
                    .await?;
                    self.cursor.applied(&event);
                }
                ReplicaEventKind::Changes { .. }
                | ReplicaEventKind::Namespace { .. }
                | ReplicaEventKind::Bloblog { .. }
                | ReplicaEventKind::Terminal { .. } => {
                    let terminal = match event.kind {
                        ReplicaEventKind::Terminal { cause, .. } => Some(cause),
                        _ => None,
                    };
                    let sequence = event.sequence;
                    let revision = event.policy_revision;
                    self.apply_frame(bytes, sequence, revision, terminal, timer.as_mut()).await?
                }
            }
        }
    }
    fn caught_up(&mut self, event: &ReplicaEvent<'_>) -> Result<()> {
        let reply = match self.checkpoints.caught_up() {
            Ok(reply) => reply,
            Err(error) => {
                self.notice.state(ReplicaState::Failed, Some(&error));
                return Err(error);
            }
        };
        self.cursor.applied(event);
        if self.checkpoints.mailbox.is_open() {
            self.notice.state(ReplicaState::Live, None);
        }
        if reply {
            self.checkpoints.mailbox.settle(Ok(()));
        }
        Ok(())
    }
    async fn apply_frame(
        &mut self,
        bytes: Vec<u8>,
        sequence: u64,
        revision: u64,
        terminal: Option<TerminalCause>,
        mut timer: std::pin::Pin<&mut tokio::time::Sleep>,
    ) -> Result<()> {
        // Wait on Tokio. The permit stays in the job if receiving is canceled.
        let permit = checkpoint_deadline(
            self.apply_slots.clone().acquire_owned(),
            &self.checkpoints.mailbox,
            timer.as_mut(),
        )
        .await??;
        self.apply = Some(start_apply(
            bytes,
            &self.store,
            &self.access,
            permit,
            &self.notice,
            &self.checkpoints.mailbox,
        )?);
        let result = checkpoint_deadline(
            self.apply.as_mut().expect("the apply branch requires an active checkpoint task"),
            &self.checkpoints.mailbox,
            timer,
        )
        .await?;
        self.apply = None;
        match result {
            Ok(Ok(_)) => {
                if let Some(cause) = terminal {
                    self.notice.state(
                        if cause == TerminalCause::ParticipantDenied {
                            ReplicaState::Revoked
                        } else {
                            ReplicaState::Gap
                        },
                        Some(&"replica authority boundary"),
                    );
                    return Err("replica authority boundary".into());
                }
                self.cursor.record_applied(sequence, revision);
                Ok(())
            }
            Ok(Err(error)) => {
                self.notice.state(ReplicaState::Failed, Some(&error));
                Err(error.into())
            }
            Err(error) => {
                self.notice.state(ReplicaState::Failed, Some(&error));
                Err(error.into())
            }
        }
    }
    fn finish_receive(&mut self, result: &Result<()>) {
        if result.is_err()
            && !self.notice.terminal
            && self
                .checkpoints
                .mailbox
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            self.checkpoints.fail_timeout();
            self.notice.state(ReplicaState::Failed, Some(&CHECKPOINT_TIMEOUT));
        }
        // A propagated decode/control/apply error is terminal. Drop must not
        // convert it into retryable cancellation or transport loss.
        if let Err(error) = result
            && !self.notice.terminal
        {
            self.notice.state(ReplicaState::Failed, Some(error));
        }
    }
}

fn accept_event(
    cursor: &mut Cursor,
    event: &ReplicaEvent<'_>,
    notice: &mut RunNotice,
) -> Result<bool> {
    match cursor.accept(event) {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(error) => {
            notice.state(
                if error == CursorError::Gap { ReplicaState::Gap } else { ReplicaState::Failed },
                Some(&error),
            );
            return Err(error.into());
        }
    }
    match event.kind {
        ReplicaEventKind::Gap => {
            notice.state(ReplicaState::Gap, Some(&"replica subscription requires gap recovery"));
            Err("replica subscription requires gap recovery".into())
        }
        _ => Ok(true),
    }
}

fn start_apply(
    bytes: Vec<u8>,
    store: &Arc<LocalStore>,
    access: &Access,
    permit: OwnedSemaphorePermit,
    notice: &RunNotice,
    mailbox: &Arc<CheckpointMailbox>,
) -> Result<tokio::task::JoinHandle<crate::store::Result<bool>>> {
    let store = store.clone();
    let access = access.clone();
    let changes = notice.notices.clone();
    let completion = notice.completion.clone();
    let source = notice.source;
    let epoch = notice.epoch;
    changes.begin_apply(source, epoch)?;
    let activity = ApplyNotice { notices: changes.clone(), completion: completion.clone(), source };
    let checkpoint_apply = mailbox.begin_apply();
    Ok(tokio::task::spawn_blocking(move || {
        let _checkpoint_apply = checkpoint_apply;
        let _activity = activity;
        let _permit = permit;
        apply_event(&bytes, &store, &access, &changes, &completion, source, epoch)
    }))
}

async fn send_checkpoint(
    send: &mut ApSendStream,
    checkpoints: &mut Checkpoints,
    notice: &mut RunNotice,
    timer: std::pin::Pin<&mut tokio::time::Sleep>,
) -> Result<()> {
    let result =
        checkpoint_deadline(wire::replica_checkpoint(send), &checkpoints.mailbox, timer).await?;
    checkpoint_written(result, checkpoints, notice)
}

fn checkpoint_written(
    result: Result<()>,
    checkpoints: &mut Checkpoints,
    notice: &mut RunNotice,
) -> Result<()> {
    // STOP_SENDING can precede the ordered terminal on an already-live
    // stream. Keep its generation state until that terminal is applied;
    // stop_writing already blocks new checkpoints and successful replies.
    if let Err(error) = result
        && !checkpoints.receive_after_write_error(&*error)?
    {
        notice.state(stream_failure(&*error), Some(&error));
        return Err(error);
    }
    Ok(())
}

async fn receive_frame(
    send: &mut ApSendStream,
    receive: &mut ApReceiveStream,
    checkpoints: &mut Checkpoints,
    notice: &mut RunNotice,
    mut timer: std::pin::Pin<&mut tokio::time::Sleep>,
) -> Result<Vec<u8>> {
    let frame = wire::receive_replica_event(receive);
    tokio::pin!(frame);
    loop {
        if checkpoints.mailbox.take() {
            checkpoints.issue()?;
            let _interval = crate::trace::Interval::new("replica.checkpoint.send");
            send_checkpoint(send, checkpoints, notice, timer.as_mut()).await?;
        }
        match checkpoint_deadline(
            frame_or_control(frame.as_mut(), &checkpoints.mailbox),
            &checkpoints.mailbox,
            timer.as_mut(),
        )
        .await?
        {
            None => continue,
            Some(Ok(bytes)) => return Ok(bytes),
            Some(Err(error)) => {
                notice.state(stream_failure(&*error), Some(&error));
                return Err(error);
            }
        }
    }
}

fn apply_event(
    bytes: &[u8],
    store: &LocalStore,
    access: &Access,
    changes: &ReplicaNotices,
    completion: &Wake,
    source: ReplicaSource,
    epoch: [u8; 16],
) -> crate::store::Result<bool> {
    // Move the original event allocation; don't clone its delta.
    let event = ReplicaEvent::decode(bytes).map_err(crate::store::Error::InvalidInput)?;
    let mut receipt_changed = false;
    let result = match (source, event.kind) {
        (
            ReplicaSource::Account,
            ReplicaEventKind::Changes { database: crate::store::DatabaseId::Index, delta },
        ) => store.receive_account_delta(delta),
        (ReplicaSource::Account, _) => {
            Err(crate::store::Error::InvalidInput("invalid event for account replica"))
        }
        (ReplicaSource::Entry(address), ReplicaEventKind::Bloblog { log, .. })
            if log != address.entry =>
        {
            Err(crate::store::Error::AccessDenied("log is outside selected entry"))
        }
        (ReplicaSource::Volume(volume), ReplicaEventKind::Bloblog { log, data })
        | (
            ReplicaSource::Entry(EntryAddress { volume, .. }),
            ReplicaEventKind::Bloblog { log, data },
        ) => apply_bloblog_event(store, access, volume, log, data, completion).map(|received| {
            receipt_changed = received.receipt_changed;
            received.data_changed
        }),
        (ReplicaSource::Volume(volume), ReplicaEventKind::Terminal { volume: changed, cause }) => {
            store.receive_authority_terminal(access, volume, Id::from_bytes(changed), cause)
        }
        (ReplicaSource::Entry(address), ReplicaEventKind::Terminal { volume: changed, cause }) => {
            store.receive_entry_authority_terminal(access, address, Id::from_bytes(changed), cause)
        }
        (ReplicaSource::Entry(address), ReplicaEventKind::Namespace { entry, delta, evidence })
            if entry == address.entry =>
        {
            store
                .merge_received_namespace_delta(access, address, delta, evidence)
                .map(|outcome| outcome.committed)
        }
        (_, ReplicaEventKind::Namespace { .. }) => {
            Err(crate::store::Error::AccessDenied("namespace evidence is outside selected stream"))
        }
        (ReplicaSource::Volume(volume), ReplicaEventKind::Changes { database, delta }) => {
            store.merge_received_delta(access, volume, database, delta).and_then(|outcome| {
                let receipt_changed = database == crate::store::DatabaseId::Index
                    && store.acknowledge_provider_delta(volume, delta)?;
                Ok(outcome.applied > 0 || receipt_changed)
            })
        }
        (ReplicaSource::Entry(address), ReplicaEventKind::Changes { database, delta }) => store
            .merge_received_entry_delta(access, address, database, delta)
            .map(|o| o.applied > 0),
        _ => unreachable!(),
    };
    if (matches!(&result, Ok(true)) && changes.update(source, epoch, true, None, None))
        || receipt_changed
    {
        completion.notify();
    }
    result
}

fn apply_bloblog_event(
    store: &LocalStore,
    access: &Access,
    volume: Id,
    log: crate::store::EntryId,
    data: &[u8],
    completion: &Wake,
) -> crate::store::Result<super::bloblog::Received> {
    let length = usize::from(
        *data
            .first()
            .ok_or_else(|| crate::store::Error::corrupt("bloblog replica frame is truncated"))?,
    );
    if !(1..=104).contains(&length) || data.len() < 1 + length + 4 {
        return Err(crate::store::Error::corrupt("bloblog replica frame is truncated"));
    }
    let cursor = crate::store::bloblog::Cursor::parse(
        std::str::from_utf8(&data[1..1 + length])
            .map_err(|_| crate::store::Error::corrupt("bloblog replica cursor is not UTF-8"))?,
    )?;
    let entries = crate::store::bloblog::decode(&data[1 + length..])?;
    let data_changed =
        super::bloblog::receive_entries(store, access, volume, log, &entries, completion)?;
    let receipt_changed = store.bloblog_set_checkpoint_changed(access, volume, log, &cursor)?;
    Ok(super::bloblog::Received { data_changed, receipt_changed })
}

async fn receive_bloblog(
    connection: &ApConnection,
    scope: &ReplicaScope,
    store: &Arc<LocalStore>,
    apply_slots: &Arc<Semaphore>,
    notice: &mut RunNotice,
    checkpoints: &Checkpoints,
    timer: std::pin::Pin<&mut tokio::time::Sleep>,
) -> Result<()> {
    let catch_up = match scope {
        ReplicaScope::Volume { volume, session } => Box::pin(crate::engine::bloblog::catch_up(
            connection,
            store.clone(),
            *volume,
            session.as_ref(),
            apply_slots.clone(),
            notice.completion.clone(),
        ))
            as std::pin::Pin<Box<dyn Future<Output = Result<bool>> + Send>>,
        ReplicaScope::Entry { address, session } => Box::pin(crate::engine::bloblog::catch_up_log(
            connection,
            store.clone(),
            address.volume,
            address.entry,
            session.as_ref(),
            apply_slots.clone(),
            notice.completion.clone(),
        )),
        ReplicaScope::Account => return Err("bloblog event on account subscription".into()),
    };
    let result = checkpoint_deadline(catch_up, &checkpoints.mailbox, timer).await?;
    match result {
        Ok(changed) => {
            if changed && notice.notices.update(notice.source, notice.epoch, true, None, None) {
                notice.completion.notify();
            }
        }
        Err(error) => {
            notice.state(stream_failure(&*error), Some(&error));
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[cfg(all(target_os = "macos", feature = "process"))]
    #[test]
    fn late_append_echo_refreshes_checkpoint_size_without_replaying_data() {
        // Cache the committed append before its echo, with no incidental engine work.
        // Only the newly stored checkpoint may wake Size, never guest data/Finder.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let access = Access::owner(store.account().id);
        let volume = store.create_volume("Late echo", true).unwrap().id;
        let log = store.create_bloblog(&access, volume, "Edits").unwrap().id;
        store.bloblog_put(&access, volume, log, &[b"retained".to_vec()]).unwrap();
        let page = store.bloblog_get(&access, volume, log, None).unwrap();
        let source = ReplicaSource::Volume(volume);
        let epoch = [1; 16];
        engine.network.received.test_state(source, epoch, ReplicaState::Live);
        engine.completion.wake.notify();
        engine.complete_background().unwrap();
        let before = engine.change_token();
        let receipt = store.delivery_provider_revision(None, None).0;
        let delivery = store.delivery_revision();
        let publications = store.publication_generation();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let bytes = store.payload_size(&access, volume, Some(log), &cancelled).unwrap();
        let mut data = vec![u8::try_from(page.cursor.as_str().len()).unwrap()];
        data.extend_from_slice(page.cursor.as_str().as_bytes());
        data.extend_from_slice(&crate::store::bloblog::encode(&page.entries).unwrap());
        let event = ReplicaEvent {
            epoch,
            sequence: 1,
            policy_revision: 1,
            kind: ReplicaEventKind::Bloblog { log, data: &data },
        }
        .encode()
        .unwrap();
        assert!(
            !apply_event(
                &event,
                &store,
                &access,
                &engine.network.received,
                &engine.completion.wake,
                source,
                epoch,
            )
            .unwrap(),
            "the own echo contains no new guest data"
        );
        assert!(store.payload_size(&access, volume, Some(log), &cancelled).unwrap() > bytes);
        assert_eq!(store.delivery_revision(), delivery);
        assert_eq!(store.publication_generation(), publications);
        assert!(engine.network.received.drain().is_empty());
        // No synthetic wake here: the real receive boundary must schedule this turn.
        engine.complete_background().unwrap();
        assert_ne!(engine.change_token(), before, "late checkpoint must refresh Size");
        assert_ne!(store.delivery_provider_revision(None, None).0, receipt);
        assert_eq!(store.delivery_provider_revision(receipt, None).1, None);
        let current = engine.change_token();
        let receipt = store.delivery_provider_revision(None, None).0;
        assert!(
            !apply_event(
                &event,
                &store,
                &access,
                &engine.network.received,
                &engine.completion.wake,
                source,
                epoch,
            )
            .unwrap()
        );
        assert!(!engine.completion.drain().unwrap(), "identical checkpoint must not wake");
        engine.complete_background().unwrap();
        assert_eq!(engine.change_token(), current);
        assert_eq!(store.delivery_provider_revision(None, None).0, receipt);
        assert!(engine.network.received.drain().is_empty());
    }

    #[test]
    fn entry_sources_share_capacity_without_aliasing_the_catalog_or_each_other() {
        let volume = Id::from_bytes([4; 16]);
        let first = ReplicaSource::Entry(EntryAddress {
            volume,
            entry: crate::store::EntryId::from_bytes([1; 15]).unwrap(),
        });
        let second = ReplicaSource::Entry(EntryAddress {
            volume,
            entry: crate::store::EntryId::from_bytes([2; 15]).unwrap(),
        });
        let notices = ReplicaNotices::new();
        for (source, epoch) in
            [(ReplicaSource::Volume(volume), [1; 16]), (first, [2; 16]), (second, [3; 16])]
        {
            notices.register(source, epoch).unwrap();
        }
        assert_eq!(notices.state(first).unwrap().0, [2; 16]);
        assert_eq!(notices.state(second).unwrap().0, [3; 16]);
        assert_eq!(notices.state(ReplicaSource::Volume(volume)).unwrap().0, [1; 16]);
        assert_eq!(notices.slots.lock().unwrap().len(), 3);
    }

    #[test]
    fn terminal_generation_cannot_return_to_live() {
        // A failed generation retains its disposition; only a new epoch may restart it.
        for terminal in [
            ReplicaState::Gap,
            ReplicaState::Revoked,
            ReplicaState::Disconnected,
            ReplicaState::Failed,
        ] {
            let notices = ReplicaNotices::new();
            notices.register(id(1), [1; 16]).unwrap();
            notices.update(id(1), [1; 16], false, Some(terminal), None);
            let rejected = std::panic::catch_unwind(|| {
                notices.update(id(1), [1; 16], false, Some(ReplicaState::Live), None);
            });
            assert!(rejected.is_err());
            assert_eq!(notices.state(id(1)), Some(([1; 16], terminal)));
            notices.stop_receiving(id(1), [1; 16]);
            notices.register(id(1), [2; 16]).unwrap();
            notices.update(id(1), [2; 16], false, Some(ReplicaState::Live), None);
            assert_eq!(notices.state(id(1)), Some(([2; 16], ReplicaState::Live)));
        }
    }

    #[test]
    fn live_generation_cannot_reenter_initial_catch_up() {
        let notices = ReplicaNotices::new();
        notices.register(id(1), [1; 16]).unwrap();
        notices.update(id(1), [1; 16], false, Some(ReplicaState::Live), None);
        assert!(
            std::panic::catch_unwind(|| {
                notices.update(id(1), [1; 16], false, Some(ReplicaState::CatchingUp), None);
            })
            .is_err()
        );
        assert_eq!(notices.state(id(1)), Some(([1; 16], ReplicaState::Live)));
    }

    #[test]
    fn apply_admission_cannot_overlap_or_release_twice() {
        // An in-flight durable apply owns the slot even after its receiver stops.
        let notices = ReplicaNotices::new();
        notices.register(id(1), [1; 16]).unwrap();
        notices.begin_apply(id(1), [1; 16]).unwrap();
        assert!(std::panic::catch_unwind(|| notices.begin_apply(id(1), [1; 16])).is_err());
        notices.stop_receiving(id(1), [1; 16]);
        assert!(notices.register(id(1), [2; 16]).is_err());
        notices.end_apply(id(1));
        assert!(std::panic::catch_unwind(|| notices.end_apply(id(1))).is_err());
        notices.register(id(1), [2; 16]).unwrap();
    }

    #[test]
    fn exhausted_gap_is_terminal_without_overwriting_other_epochs_or_revocation() {
        let notices = ReplicaNotices::new();
        let source = ReplicaSource::Volume(Id::from_bytes([1; 16]));
        notices.test_state(source, [1; 16], ReplicaState::Gap);
        notices.update(source, [1; 16], true, None, Some(&"gap detected"));
        assert!(!notices.fail_gap(source, [2; 16]));
        assert!(notices.fail_gap(source, [1; 16]));
        let drained = notices.drain();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].dirty, "a terminal failure cannot discard a durable edit");
        assert_eq!(drained[0].error.as_deref(), Some("gap detected"));
        assert_eq!(notices.state(source), Some(([1; 16], ReplicaState::Failed)));
        assert!(!notices.fail_gap(source, [1; 16]));
        assert!(notices.drain().is_empty());
        notices.test_state(source, [2; 16], ReplicaState::Revoked);
        assert!(!notices.fail_gap(source, [1; 16]));
        assert!(!notices.fail_gap(source, [2; 16]));
        assert_eq!(notices.state(source), Some(([2; 16], ReplicaState::Revoked)));
        notices.test_state(source, [3; 16], ReplicaState::Live);
        assert!(!notices.fail_gap(source, [3; 16]));
        assert_eq!(notices.state(source), Some(([3; 16], ReplicaState::Live)));
    }
    use std::io::Read;

    fn test_id(value: usize) -> Id {
        let mut bytes = [value as u8; 16];
        if value > 255 {
            bytes[0] = 0;
            bytes[14..].copy_from_slice(&(value as u16).to_le_bytes());
        }
        Id::from_bytes(bytes)
    }
    fn id(n: usize) -> ReplicaSource {
        ReplicaSource::Volume(test_id(n))
    }
    fn sockets() -> (Arc<Wake>, UnixStream) {
        let (write, read) = UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        read.set_nonblocking(true).unwrap();
        (Arc::new(Wake::unix(write)), read)
    }

    #[test]
    fn account_and_volume_share_capacity_without_aliasing_or_cross_retirement() {
        let notices = ReplicaNotices::new();
        notices.register(ReplicaSource::Account, [1; 16]).unwrap();
        notices.register(id(1), [1; 16]).unwrap();
        notices.update(ReplicaSource::Account, [1; 16], true, Some(ReplicaState::Live), None);
        assert_eq!(notices.state(id(1)), Some(([1; 16], ReplicaState::CatchingUp)));
        for n in 2..NOTICE_LIMIT {
            notices.register(id(n), [n as u8; 16]).unwrap();
        }
        assert!(notices.register(id(NOTICE_LIMIT), [32; 16]).is_err());
        notices.stop_receiving(ReplicaSource::Account, [1; 16]);
        assert_eq!(notices.state(ReplicaSource::Account), Some(([1; 16], ReplicaState::Live)));
        assert!(!notices.forget(ReplicaSource::Account, [1; 16])); // Undelivered dirty state.
        let dirty = notices.drain();
        assert!(dirty.iter().any(|notice| notice.source == ReplicaSource::Account && notice.dirty));
        assert!(notices.forget(ReplicaSource::Account, [1; 16]));
        assert!(notices.state(ReplicaSource::Account).is_none());
        assert_eq!(notices.state(id(1)), Some(([1; 16], ReplicaState::CatchingUp)));
        notices.register(id(NOTICE_LIMIT), [32; 16]).unwrap();
    }

    #[test]
    fn notices_coalesce_dirty_and_move_bounded_utf8_errors() {
        let notices = ReplicaNotices::new();
        notices.register(id(1), [1; 16]).unwrap();
        notices.update(id(1), [1; 16], true, None, None);
        notices.update(id(1), [1; 16], false, Some(ReplicaState::Live), None);
        let error = "é".repeat(1000);
        notices.update(id(1), [1; 16], false, Some(ReplicaState::Gap), Some(&error));
        let mut batch = notices.drain();
        assert_eq!(batch.len(), 1);
        let entry = batch.pop().unwrap();
        assert!(entry.dirty);
        assert_eq!(entry.state, ReplicaState::Gap);
        let error = entry.error.unwrap();
        assert!(error.len() <= ERROR_LIMIT);
        assert!(error.ends_with('…'));
        assert!(notices.drain().is_empty());
        notices.update(id(1), [1; 16], true, None, None);
        let entry = notices.drain().pop().unwrap();
        assert!(entry.dirty);
        assert!(entry.error.is_none());
        assert_eq!(entry.state, ReplicaState::Gap);
    }

    #[test]
    fn capacity_is_explicit_and_draining_does_not_forget_generation() {
        let notices = ReplicaNotices::new();
        for n in 1..=NOTICE_LIMIT {
            notices.register(id(n), [n as u8; 16]).unwrap();
        }
        assert!(notices.register(id(NOTICE_LIMIT + 1), [33; 16]).is_err());
        assert_eq!(notices.drain().len(), NOTICE_LIMIT);
        assert!(notices.register(id(NOTICE_LIMIT + 1), [33; 16]).is_err());
        assert!(!notices.forget(id(1), [2; 16]));
        assert!(!notices.forget(id(1), [1; 16]));
        notices.stop_receiving(id(1), [1; 16]);
        assert!(notices.forget(id(1), [1; 16]));
        notices.register(id(NOTICE_LIMIT + 1), [33; 16]).unwrap();
        assert_eq!(notices.drain().len(), 1);
        // Include both retained errors when an update races a drained batch.
        let notice_bytes = notice_allocation_bytes();
        crate::diagnostic!("receive notice allowance: {notice_bytes} bytes");
        assert!(notice_bytes < 256 * 1024);
    }

    #[test]
    fn cancellation_signals_and_old_generation_cannot_overwrite_replacement() {
        let notices = Arc::new(ReplicaNotices::new());
        let (completion, mut read) = sockets();
        notices.register(id(1), [1; 16]).unwrap();
        let guard = RunNotice {
            notices: notices.clone(),
            completion: completion.clone(),
            source: id(1),
            epoch: [1; 16],
            terminal: false,
        };
        drop(guard);
        assert_eq!(read.read(&mut [0; 1]).unwrap(), 1);
        assert_eq!(notices.drain()[0].state, ReplicaState::Disconnected);
        notices.register(id(1), [2; 16]).unwrap();
        // Late diagnostics cannot overwrite the replacement's state. Real
        // durable applies must finish before register admits this generation.
        notices.update(id(1), [1; 16], true, Some(ReplicaState::Gap), Some(&"old failure"));
        let entry = notices.drain().pop().unwrap();
        assert_eq!(entry.epoch, [2; 16]);
        assert_eq!(entry.state, ReplicaState::CatchingUp);
        assert!(entry.dirty);
        assert!(entry.error.is_none());
    }

    #[test]
    fn terminal_state_survives_drop_and_concurrent_dirty_notices() {
        for state in [ReplicaState::Revoked, ReplicaState::Failed, ReplicaState::Gap] {
            let notices = Arc::new(ReplicaNotices::new());
            let (completion, _) = sockets();
            notices.register(id(1), [1; 16]).unwrap();
            let mut guard = RunNotice {
                notices: notices.clone(),
                completion,
                source: id(1),
                epoch: [1; 16],
                terminal: false,
            };
            guard.state(state, Some(&"terminal failure"));
            drop(guard);
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| {
                        for _ in 0..100 {
                            notices.update(id(1), [1; 16], true, None, None);
                        }
                    });
                }
            });
            let entry = notices.drain().pop().unwrap();
            assert_eq!(entry.state, state);
            assert!(entry.dirty);
            assert_eq!(entry.error.as_deref(), Some("terminal failure"));
            assert_eq!(notices.state(id(1)), Some(([1; 16], state)));
            assert!(notices.drain().is_empty());
            notices.register(id(1), [2; 16]).unwrap();
            notices.update(id(1), [1; 16], false, Some(state), Some(&"late failure"));
            let replacement = notices.drain().pop().unwrap();
            assert_eq!(replacement.state, ReplicaState::CatchingUp);
            assert!(replacement.error.is_none());
        }
    }

    #[test]
    fn malformed_wire_local_errors_and_reply_timeouts_are_not_transport_loss() {
        let errors: Vec<Box<dyn std::error::Error + Send + Sync>> = vec![
            crate::network::ap::Error::InvalidHeader.into(),
            crate::network::ap::ClientError::Protocol(crate::network::ap::Error::FrameTooLarge)
                .into(),
            crate::network::ap::ClientError::ResponseTimeout.into(),
            crate::network::ap::ClientError::AuthenticationRequired.into(),
            std::io::Error::from(std::io::ErrorKind::ConnectionReset).into(),
            crate::store::Error::AccessDenied("session participant revoked").into(),
            "service rejected publication".into(),
        ];
        for error in errors {
            assert_eq!(stream_failure(&*error), ReplicaState::Failed, "{error}");
        }
    }

    #[test]
    fn notification_saturation_preserves_state_without_waiting() {
        let notices = ReplicaNotices::new();
        let (completion, mut read) = sockets();
        while completion.socket().unwrap().write(&[1; 1024]).is_ok() {}
        notices.register(id(1), [1; 16]).unwrap();
        notices.update(id(1), [1; 16], true, Some(ReplicaState::Gap), Some(&"gap"));
        completion.notify();
        assert!(read.read(&mut [0; 1]).unwrap() > 0);
        let entry = notices.drain().pop().unwrap();
        assert_eq!(entry.state, ReplicaState::Gap);
        assert!(entry.dirty);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod checkpoint_tests {
    use super::*;
    fn caught_up(checkpoints: &mut Checkpoints) {
        if checkpoints.caught_up().unwrap() {
            checkpoints.mailbox.settle(Ok(()));
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    #[test]
    fn checkpoint_requested_during_terminal_drain_waits_for_its_disposition() {
        // Stop the startup write before admitting an explicit checkpoint. Poll
        // it before releasing the ordered terminal; no scheduler timing is used.
        runtime().block_on(async {
            for terminal in [ReplicaState::Gap, ReplicaState::Revoked, ReplicaState::Failed] {
                let notices = Arc::new(ReplicaNotices::new());
                let source = ReplicaSource::Volume(Id::from_bytes([1; 16]));
                let epoch = [2; 16];
                notices.register(source, epoch).unwrap();
                let (socket, _reader) = UnixStream::pair().unwrap();
                socket.set_nonblocking(true).unwrap();
                let mut notice = RunNotice {
                    notices: notices.clone(),
                    completion: Arc::new(Wake::unix(socket)),
                    source,
                    epoch,
                    terminal: false,
                };
                let credits = Arc::new(Semaphore::new(8));
                let mut checkpoints = Checkpoints::new(Duration::from_secs(60), credits.clone());
                checkpoints.stop_writing().unwrap();
                let deadline = checkpoints.mailbox.deadline();
                let apply = checkpoints.mailbox.begin_apply();
                let control = ReplicaControl { mailbox: checkpoints.mailbox.clone() };
                let checkpoint = control.checkpoint();
                tokio::pin!(checkpoint);
                poll_fn(|cx| {
                    assert!(
                        checkpoint.as_mut().poll(cx).is_pending(),
                        "checkpoint escaped before the ordered terminal established its disposition"
                    );
                    Poll::Ready(())
                })
                .await;
                assert_eq!(checkpoints.mailbox.deadline(), deadline);
                assert!(!checkpoints.mailbox.take());
                assert!(!checkpoints.mailbox.has_request());
                assert!(checkpoints.mailbox.request().is_err());
                assert_eq!(credits.available_permits(), 7);
                caught_up(&mut checkpoints);
                drop(apply);
                poll_fn(|cx| {
                    assert!(checkpoint.as_mut().poll(cx).is_pending(), "apply released the reply");
                    Poll::Ready(())
                })
                .await;
                assert_eq!(credits.available_permits(), 7);
                assert_eq!(notices.state(source), Some((epoch, ReplicaState::CatchingUp)));
                notice.state(terminal, Some(&"ordered terminal"));
                drop(checkpoints);
                assert!(checkpoint.await.is_err());
                assert_eq!(notices.state(source), Some((epoch, terminal)));
                assert_eq!(credits.available_permits(), 8);
                // A checkpoint admitted after the terminal must fail immediately
                // without changing its disposition or retaining another credit.
                assert!(control.checkpoint().await.is_err());
                assert_eq!(notices.state(source), Some((epoch, terminal)));
                assert_eq!(credits.available_permits(), 8);
            }
        });
    }

    #[test]
    fn stopped_checkpoint_write_preserves_generation_until_ordered_terminal() {
        // Inject STOP_SENDING at the write boundary after an explicit request.
        // Both startup and already-live generations must drain their terminal
        // without regressing state or acknowledging the outstanding checkpoint.
        runtime().block_on(async {
            for live in [true, false] {
                let notices = Arc::new(ReplicaNotices::new());
                let source = ReplicaSource::Volume(Id::from_bytes([1; 16]));
                let epoch = [2; 16];
                notices.register(source, epoch).unwrap();
                let (socket, _reader) = UnixStream::pair().unwrap();
                socket.set_nonblocking(true).unwrap();
                let mut notice = RunNotice {
                    notices: notices.clone(),
                    completion: Arc::new(Wake::unix(socket)),
                    source,
                    epoch,
                    terminal: false,
                };
                let credits = Arc::new(Semaphore::new(1));
                let mut checkpoints = Checkpoints::new(Duration::from_secs(60), credits.clone());
                if live {
                    caught_up(&mut checkpoints);
                    notice.state(ReplicaState::Live, None);
                }
                let mut reply = checkpoints.mailbox.request().unwrap();
                assert!(checkpoints.mailbox.take());
                checkpoints.issue().unwrap();
                let deadline = checkpoints.mailbox.deadline();
                notices.drain();
                let stopped = crate::network::ap::ClientError::Write(quinn::WriteError::Stopped(
                    quinn::VarInt::from_u32(0),
                ));
                checkpoint_written(Err(stopped.into()), &mut checkpoints, &mut notice).unwrap();
                let expected = if live { ReplicaState::Live } else { ReplicaState::CatchingUp };
                assert_eq!(notices.state(source).unwrap(), (epoch, expected));
                assert!(notices.drain().is_empty());
                assert!(!checkpoints.mailbox.is_open());
                assert_eq!(checkpoints.mailbox.deadline(), deadline);
                assert!(checkpoints.mailbox.request().is_err());
                while checkpoints.observed < checkpoints.requested {
                    caught_up(&mut checkpoints);
                }
                assert!(matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
                assert_eq!(credits.available_permits(), 0);
                let apply = checkpoints.mailbox.begin_apply();
                drop(apply);
                assert!(
                    matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
                    "terminal apply completion must not precede disposition"
                );
                assert_eq!(credits.available_permits(), 0);
                // The ordered terminal determines the disposition, not the failed write.
                notice.state(ReplicaState::Revoked, Some(&"authority changed"));
                assert_eq!(notices.state(source).unwrap(), (epoch, ReplicaState::Revoked));
                drop(checkpoints);
                assert!(reply.try_recv().unwrap().is_err());
                assert_eq!(credits.available_permits(), 1);
                drop(notice);
                notices.wait_inactive(source, epoch).await.unwrap();
            }
        });
    }

    #[test]
    fn stopped_write_keeps_canceled_waiter_credit_until_terminal() {
        // Cancel the waiter during terminal apply. Its sole slot and shared
        // credit stay owned until the receiver records a terminal disposition.
        let credits = Arc::new(Semaphore::new(8));
        let mut checkpoints = Checkpoints::new(Duration::from_secs(60), credits.clone());
        let mut reply = checkpoints.mailbox.request().unwrap();
        assert!(checkpoints.mailbox.take());
        checkpoints.issue().unwrap();
        let deadline = checkpoints.mailbox.deadline();
        checkpoints.stop_writing().unwrap();
        assert_eq!(checkpoints.mailbox.deadline(), deadline);
        assert!(checkpoints.mailbox.request().is_err());
        assert!(!checkpoints.mailbox.take());
        caught_up(&mut checkpoints);
        caught_up(&mut checkpoints);
        assert!(matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert_eq!(credits.available_permits(), 7);
        assert!(checkpoints.caught_up().is_err()); // Still reject unsolicited markers.
        let apply = checkpoints.mailbox.begin_apply();
        drop(reply);
        drop(apply);
        assert_eq!(credits.available_permits(), 7);
        assert!(checkpoints.mailbox.request().is_err());
        drop(checkpoints); // Terminal receive / cancellation closes the waiter.
        assert_eq!(credits.available_permits(), 8);
    }

    #[test]
    fn stopped_startup_write_bounds_terminal_wait_before_and_after_checkpoint_admission() {
        // An expired drain deadline wins with or without a later waiter.
        // Admission cannot renew it; no wall-clock sleep or scheduler race is used.
        runtime().block_on(async {
            for admit in [false, true] {
                let credits = Arc::new(Semaphore::new(8));
                let mut checkpoints = Checkpoints::new(Duration::ZERO, credits.clone());
                assert!(checkpoints.mailbox.deadline().is_none());
                checkpoints.stop_writing().unwrap();
                let deadline = checkpoints.mailbox.deadline();
                let reply = admit.then(|| checkpoints.mailbox.request().unwrap());
                assert_eq!(checkpoints.mailbox.deadline(), deadline);
                let timer = tokio::time::sleep_until(tokio::time::Instant::now());
                tokio::pin!(timer);
                assert_eq!(
                    checkpoint_deadline(
                        std::future::pending::<()>(),
                        &checkpoints.mailbox,
                        timer.as_mut()
                    )
                    .await,
                    Err(CHECKPOINT_TIMEOUT)
                );
                checkpoints.fail_timeout();
                if let Some(reply) = reply {
                    assert_eq!(reply.await.unwrap(), Err(CHECKPOINT_TIMEOUT));
                }
                assert_eq!(credits.available_permits(), 8);
                assert!(checkpoints.caught_up().is_err());
            }
        });
    }

    #[test]
    fn other_write_errors_cannot_enter_terminal_drain() {
        let mut checkpoints =
            Checkpoints::new(Duration::from_secs(60), Arc::new(Semaphore::new(8)));
        assert!(
            !checkpoints
                .receive_after_write_error(&crate::network::ap::ClientError::ResponseTimeout)
                .unwrap()
        );
        assert!(
            !checkpoints
                .receive_after_write_error(&std::io::Error::from(
                    std::io::ErrorKind::ConnectionReset
                ))
                .unwrap()
        );
        assert!(checkpoints.mailbox.is_open());
        assert!(checkpoints.mailbox.deadline().is_none());
    }
    #[test]
    fn startup_marker_cannot_complete_an_explicit_checkpoint() {
        let mut checkpoints =
            Checkpoints::new(Duration::from_secs(60), Arc::new(Semaphore::new(8)));
        let mut reply = checkpoints.mailbox.request().unwrap();
        assert!(checkpoints.mailbox.take());
        checkpoints.issue().unwrap();
        caught_up(&mut checkpoints); // Startup ordinal one.
        assert!(matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        assert!(checkpoints.mailbox.request().is_err());
        caught_up(&mut checkpoints);
        assert_eq!(reply.try_recv().unwrap(), Ok(()));
        assert!(checkpoints.caught_up().is_err()); // Unrequested marker.
        let mut next = checkpoints.mailbox.request().unwrap();
        assert!(checkpoints.mailbox.take());
        checkpoints.issue().unwrap();
        caught_up(&mut checkpoints);
        assert_eq!(next.try_recv().unwrap(), Ok(()));
    }
    #[test]
    fn canceled_waiter_keeps_volume_and_shared_control_credits_until_settlement() {
        let credits = Arc::new(Semaphore::new(8));
        // Model seven publisher commands using the exact same shared credits.
        let publisher = credits.clone().try_acquire_many_owned(7).unwrap();
        let mut checkpoints = Checkpoints::new(Duration::from_secs(60), credits.clone());
        let reply = checkpoints.mailbox.request().unwrap();
        assert_eq!(credits.available_permits(), 0);
        drop(reply);
        assert!(checkpoints.mailbox.request().is_err());
        let other = Checkpoints::new(Duration::from_secs(60), credits.clone());
        assert!(other.mailbox.request().is_err());
        assert!(checkpoints.mailbox.take());
        checkpoints.issue().unwrap();
        caught_up(&mut checkpoints);
        assert_eq!(credits.available_permits(), 0);
        caught_up(&mut checkpoints);
        assert_eq!(credits.available_permits(), 1);
        drop(publisher);
        assert_eq!(credits.available_permits(), 8);
    }
    #[test]
    fn drop_closes_queued_and_sent_commands_and_retired_handle() {
        let credits = Arc::new(Semaphore::new(8));
        for sent in [false, true] {
            let mut checkpoints = Checkpoints::new(Duration::from_secs(60), credits.clone());
            let control = ReplicaControl { mailbox: checkpoints.mailbox.clone() };
            let reply = control.mailbox.request().unwrap();
            if sent {
                assert!(checkpoints.mailbox.take());
                checkpoints.issue().unwrap();
            }
            drop(checkpoints);
            assert!(runtime().block_on(reply).unwrap().is_err());
            assert!(runtime().block_on(control.checkpoint()).is_err());
            assert_eq!(credits.available_permits(), 8);
        }
    }
    #[test]
    fn duplicate_or_changed_epoch_cannot_advance_checkpoint_ordinal() {
        let ack = SubscriptionAck { epoch: [1; 16], sequence: 0, policy_revision: 1 };
        let mut cursor = Cursor::new(&ack).unwrap();
        let mut checkpoints =
            Checkpoints::new(Duration::from_secs(60), Arc::new(Semaphore::new(8)));
        let mut reply = checkpoints.mailbox.request().unwrap();
        assert!(checkpoints.mailbox.take());
        checkpoints.issue().unwrap();
        let startup = ReplicaEvent {
            epoch: ack.epoch,
            sequence: 1,
            policy_revision: 1,
            kind: ReplicaEventKind::CaughtUp,
        };
        assert_eq!(cursor.accept(&startup), Ok(true));
        cursor.applied(&startup);
        caught_up(&mut checkpoints);
        assert_eq!(cursor.accept(&startup), Ok(false));
        let foreign = ReplicaEvent { epoch: [2; 16], sequence: 2, ..startup };
        assert!(cursor.accept(&foreign).is_err());
        assert!(matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        drop(checkpoints);
        assert!(runtime().block_on(reply).unwrap().is_err());
    }
    #[test]
    fn control_wakes_preserve_the_same_partial_frame_future() {
        runtime().block_on(async {
            let mut checkpoints =
                Checkpoints::new(Duration::from_secs(60), Arc::new(Semaphore::new(8)));
            let (header, header_read) = oneshot::channel();
            let (body, body_read) = oneshot::channel();
            let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = progress.clone();
            let frame = async move {
                let prefix: Vec<u8> = header_read.await.unwrap();
                observed.store(1, std::sync::atomic::Ordering::SeqCst);
                let suffix: Vec<u8> = body_read.await.unwrap();
                [prefix, suffix].concat()
            };
            tokio::pin!(frame);
            header.send(vec![1, 2, 3]).unwrap();
            let reply = checkpoints.mailbox.request().unwrap();
            assert!(frame_or_control(frame.as_mut(), &checkpoints.mailbox).await.is_none());
            assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(checkpoints.mailbox.take());
            checkpoints.issue().unwrap();
            body.send(vec![4, 5]).unwrap();
            assert_eq!(
                frame_or_control(frame.as_mut(), &checkpoints.mailbox).await,
                Some(vec![1, 2, 3, 4, 5])
            );
            // Complete only after the caller's durable processing boundary.
            assert_eq!(checkpoints.observed, 0);
            caught_up(&mut checkpoints);
            caught_up(&mut checkpoints);
            assert_eq!(reply.await.unwrap(), Ok(()));
        });
    }
    #[test]
    fn withheld_marker_expires_and_cannot_accept_a_later_marker() {
        runtime().block_on(async {
            let timer = tokio::time::sleep_until(tokio::time::Instant::now());
            tokio::pin!(timer);
            let credits = Arc::new(Semaphore::new(8));
            let mut checkpoints = Checkpoints::new(Duration::from_millis(5), credits.clone());
            let mut reply = checkpoints.mailbox.request().unwrap();
            assert!(checkpoints.mailbox.take());
            checkpoints.issue().unwrap();
            // Keep a real unfinished receive operation alive through the timer.
            let never = std::future::pending::<()>();
            assert_eq!(
                checkpoint_deadline(never, &checkpoints.mailbox, timer.as_mut()).await,
                Err(CHECKPOINT_TIMEOUT)
            );
            checkpoints.fail_timeout();
            assert_eq!(reply.try_recv().unwrap(), Err(CHECKPOINT_TIMEOUT));
            assert_eq!(credits.available_permits(), 8);
            assert!(checkpoints.caught_up().is_err());
            assert!(checkpoints.mailbox.request().is_err());
        });
    }

    #[test]
    fn already_ready_late_marker_cannot_win_over_the_admission_deadline() {
        runtime().block_on(async {
            let timer = tokio::time::sleep_until(tokio::time::Instant::now());
            tokio::pin!(timer);
            let mut checkpoints = Checkpoints::new(Duration::ZERO, Arc::new(Semaphore::new(8)));
            let mut reply = checkpoints.mailbox.request().unwrap();
            assert!(checkpoints.mailbox.take());
            checkpoints.issue().unwrap();
            let polled = std::cell::Cell::new(false);
            let late = async {
                polled.set(true);
            };
            assert_eq!(
                checkpoint_deadline(late, &checkpoints.mailbox, timer.as_mut()).await,
                Err(CHECKPOINT_TIMEOUT)
            );
            assert!(!polled.get(), "expired marker was processed");
            checkpoints.fail_timeout();
            assert_eq!(reply.try_recv().unwrap(), Err(CHECKPOINT_TIMEOUT));
        });
    }

    #[test]
    fn request_during_apply_arms_deadline_and_canceled_waiter_keeps_credit() {
        runtime().block_on(async {
            let timer = tokio::time::sleep_until(tokio::time::Instant::now());
            tokio::pin!(timer);
            let credits = Arc::new(Semaphore::new(8));
            let seven = credits.clone().try_acquire_many_owned(7).unwrap();
            let mut checkpoints = Checkpoints::new(Duration::ZERO, credits.clone());
            let apply = checkpoints.mailbox.begin_apply();
            let (complete, unfinished_apply) = oneshot::channel::<()>();
            {
                let wait =
                    checkpoint_deadline(unfinished_apply, &checkpoints.mailbox, timer.as_mut());
                tokio::pin!(wait);
                poll_fn(|cx| {
                    assert!(wait.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                // Admission happens after the apply await began; cancellation
                // removes neither its deadline nor the global eighth credit.
                let reply = checkpoints.mailbox.request().unwrap();
                drop(reply);
                assert_eq!(wait.await, Err(CHECKPOINT_TIMEOUT));
            }
            checkpoints.fail_timeout();
            assert_eq!(credits.available_permits(), 0);
            // Even retiring the receiving future cannot free the active job.
            drop(checkpoints);
            assert_eq!(credits.available_permits(), 0);
            assert!(complete.send(()).is_err()); // The receive await was terminated.
            drop(apply); // The original admitted disk job's actual end.
            assert_eq!(credits.available_permits(), 1);
            drop(seven);
            assert_eq!(credits.available_permits(), 8);
        });
    }

    #[test]
    fn checkpoint_admission_wakes_the_registered_operation_without_idle_wakes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        runtime().block_on(async {
            let mut checkpoints = Checkpoints::new(Duration::ZERO, Arc::new(Semaphore::new(8)));
            let timer = tokio::time::sleep_until(tokio::time::Instant::now());
            tokio::pin!(timer);
            let count = Arc::new(Count(AtomicUsize::new(0)));
            let waker = std::task::Waker::from(count.clone());
            let mut cx = std::task::Context::from_waker(&waker);
            {
                let operation = checkpoint_deadline(
                    std::future::pending::<()>(),
                    &checkpoints.mailbox,
                    timer.as_mut(),
                );
                tokio::pin!(operation);
                assert!(operation.as_mut().poll(&mut cx).is_pending());
                assert_eq!(count.0.load(Ordering::SeqCst), 0);
                let _reply = checkpoints.mailbox.request().unwrap();
                assert_eq!(count.0.load(Ordering::SeqCst), 1);
                assert_eq!(operation.as_mut().poll(&mut cx), Poll::Ready(Err(CHECKPOINT_TIMEOUT)));
            }
            checkpoints.fail_timeout();
            assert!(checkpoints.mailbox.slot.lock().unwrap().waker.is_none());
        });
    }

    #[test]
    fn protocol_deadline_future_has_bounded_in_place_storage() {
        runtime().block_on(async {
            let timer = tokio::time::sleep_until(tokio::time::Instant::now());
            tokio::pin!(timer);
            let checkpoints = Checkpoints::new(Duration::from_secs(60), Arc::new(Semaphore::new(8)));
            let wait = checkpoint_deadline(std::future::pending::<()>(), &checkpoints.mailbox, timer.as_mut());
            let bytes = std::mem::size_of_val(&wait);
            // This is only the new helper, not proof of the entire account's
            // 64KiB framing/address allowance. Measure the concrete receive
            // coroutine too: coroutine layouts can overlap suspended awaits.
            assert!(bytes <= 512, "deadline supervisor uses {bytes} bytes");
            fn run_size<F: Future>(_: impl FnOnce(PreparedSubscription, Arc<LocalStore>,
                Arc<Semaphore>, RunNotice, Checkpoints) -> F) -> usize { std::mem::size_of::<F>() }
            let run_bytes = run_size(PreparedSubscription::run);
            assert!(32 * run_bytes <= 64 * 1024, "receive coroutines alone use {} bytes/account", 32 * run_bytes);
            crate::diagnostic!("deadline supervisor: {bytes} bytes/site; actual receive coroutine: {run_bytes} bytes; 32 coroutines: {} bytes; full framing/address proof is separate", 32 * run_bytes);
        });
    }

    #[test]
    fn checkpoint_control_storage_is_bounded() {
        // Tokio 1.53.1 oneshot::Inner contains an AtomicUsize, Option<T>
        // and two Wakers; its Arc has two additional usize counters. Charge
        // 256 bytes per admitted reply, including alignment/allocation slack.
        let reply_layout = std::mem::size_of::<usize>() * 3
            + std::mem::size_of::<Option<std::result::Result<(), &'static str>>>()
            + 2 * std::mem::size_of::<std::task::Waker>();
        assert!(reply_layout <= 256);
        // Include mailbox Arc counters and the shared semaphore's Arc. The
        // publisher asserts all partitions of the eight shared credits against
        // the aggregate16KiB bound. Deadline state is included in these types.
        let control_bytes = control_allocation_bytes(8);
        assert!(control_bytes <= 8 * 1024, "receive control uses {control_bytes} bytes");
        crate::diagnostic!(
            "receive control allowance: {control_bytes} bytes; oneshot layout: {reply_layout} bytes"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod apply_lifetime_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn aborted_receive_keeps_apply_permit_slot_and_late_dirty_until_real_completion() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let permits = Arc::new(Semaphore::new(2));
        let credits = Arc::new(Semaphore::new(8));
        let seven = credits.clone().try_acquire_many_owned(7).unwrap();
        let checkpoints = Checkpoints::new(Duration::from_secs(60), credits.clone());
        let reply = checkpoints.mailbox.request().unwrap();
        let notices = Arc::new(ReplicaNotices::new());
        let source = ReplicaSource::Volume(Id::from_bytes([8; 16]));
        let epoch = [4; 16];
        notices.register(source, epoch).unwrap();
        let (write, _read) = UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        let completion = Arc::new(Wake::unix(write));
        let (entered_send, entered) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let (finished_send, finished) = mpsc::sync_channel(1);
        let worker_notices = notices.clone();
        let worker_permits = permits.clone();
        let task = runtime.spawn(async move {
            let checkpoints = checkpoints;
            let _receive = RunNotice {
                notices: worker_notices.clone(),
                completion: completion.clone(),
                source,
                epoch,
                terminal: false,
            };
            let permit = worker_permits.acquire_owned().await.unwrap();
            worker_notices.begin_apply(source, epoch).unwrap();
            let activity = ApplyNotice { notices: worker_notices.clone(), completion, source };
            let checkpoint_apply = checkpoints.mailbox.begin_apply();
            tokio::task::spawn_blocking(move || {
                let checkpoint_apply = checkpoint_apply;
                let permit = permit;
                let activity = activity;
                entered_send.send(()).unwrap();
                released.recv().unwrap();
                worker_notices.update(source, epoch, true, None, None);
                drop(activity);
                drop(permit);
                drop(checkpoint_apply);
                finished_send.send(()).unwrap();
            })
            .await
            .unwrap();
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(permits.available_permits(), 1);
        assert!(notices.register(source, [5; 16]).is_err());
        task.abort();
        assert!(runtime.block_on(task).unwrap_err().is_cancelled());
        assert!(runtime.block_on(reply).unwrap().is_err());
        assert_eq!(
            credits.available_permits(),
            0,
            "aborted owner released an active job's control credit"
        );
        assert_eq!(notices.drain()[0].state, ReplicaState::Disconnected);
        assert!(!notices.forget(source, epoch));
        assert_eq!(permits.available_permits(), 1);
        assert!(notices.register(source, [5; 16]).is_err());
        let waiting_notices = notices.clone();
        let (waiting_send, waiting) = mpsc::sync_channel(1);
        let retirement = runtime.spawn(async move {
            use std::{future::Future, task::Poll};
            let inactive = waiting_notices.wait_inactive(source, epoch);
            tokio::pin!(inactive);
            std::future::poll_fn(|cx| {
                assert!(inactive.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            waiting_send.send(()).unwrap();
            inactive.await
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), retirement)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        });
        assert_eq!(permits.available_permits(), 2);
        assert_eq!(credits.available_permits(), 1);
        drop(seven);
        assert_eq!(credits.available_permits(), 8);
        notices.register(source, [5; 16]).unwrap();
        let entry = notices.drain().pop().unwrap();
        assert!(entry.dirty);
        assert_eq!(entry.epoch, [5; 16]);
        assert_eq!(entry.state, ReplicaState::CatchingUp);
        assert!(runtime.block_on(notices.wait_inactive(source, epoch)).is_err());
        notices.stop_receiving(source, [5; 16]);
        runtime.block_on(notices.wait_inactive(source, [5; 16])).unwrap();
        assert!(notices.forget(source, [5; 16]));
        assert!(runtime.block_on(notices.wait_inactive(source, [5; 16])).is_err());
    }
}

/// Replays fixed replication traces and checks their expected states.
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod regression_tests {
    use super::*;
    use std::time::Duration;

    // Existing traces exercise 32 competing sources; reserve the rest of the
    // current registry so the same lifecycle traces still reach real pressure.
    fn trace_notices() -> ReplicaNotices {
        let notices = ReplicaNotices::new();
        for n in 0..NOTICE_LIMIT - 32 {
            notices
                .register(
                    ReplicaSource::Volume(Id::from_bytes((10000 + n as u128).to_be_bytes())),
                    [1; 16],
                )
                .unwrap();
        }
        notices.drain();
        notices
    }
    fn epoch(word: &str) -> [u8; 16] {
        [word.parse().unwrap(); 16]
    }

    fn state_word(state: ReplicaState) -> &'static str {
        match state {
            ReplicaState::CatchingUp => "catchingup",
            ReplicaState::Live => "live",
            ReplicaState::Gap => "gap",
            ReplicaState::Revoked => "revoked",
            ReplicaState::Disconnected => "disconnected",
            ReplicaState::Failed => "failed",
        }
    }

    /// Each trace follows the live receive and apply guards and ends by
    /// reporting every source.
    #[test]
    fn notices_match_regression_cases() {
        let source = |word: &str| match word {
            "a" => ReplicaSource::Account,
            entry if entry.starts_with('e') => {
                let (volume, entry) = entry[1..].split_once(':').unwrap();
                ReplicaSource::Entry(EntryAddress {
                    volume: Id::from_bytes([volume.parse().unwrap(); 16]),
                    entry: crate::store::EntryId::from_bytes([entry.parse().unwrap(); 15]).unwrap(),
                })
            }
            volume => ReplicaSource::Volume(Id::from_bytes([volume[1..].parse().unwrap(); 16])),
        };
        let state = |word: &str| match word {
            "-" => None,
            "catchingup" => Some(ReplicaState::CatchingUp),
            "live" => Some(ReplicaState::Live),
            "gap" => Some(ReplicaState::Gap),
            "revoked" => Some(ReplicaState::Revoked),
            "disconnected" => Some(ReplicaState::Disconnected),
            "failed" => Some(ReplicaState::Failed),
            other => panic!("unknown replica state {other}"),
        };
        let error = |word: &str| (word != "-").then(|| word.parse::<u64>().unwrap());
        let source_word = |source: ReplicaSource| match source {
            ReplicaSource::Account => "a".to_string(),
            ReplicaSource::Volume(id) => format!("v{}", id.0[0]),
            ReplicaSource::Entry(address) => {
                format!("e{}:{}", address.volume.0[0], address.entry.as_bytes()[0])
            }
        };
        let mut notices = trace_notices();
        let mut traces = 0;
        for line in include_str!("../../tests/fixtures/notices.txt").lines() {
            let w: Vec<&str> = line.split_whitespace().collect();
            match w[0] {
                "register" => {
                    let actual = match notices.register(source(w[1]), epoch(w[2])) {
                        Ok(()) => "ok".to_string(),
                        Err(error) => match error.to_string().as_str() {
                            "replica subscription already started" => "started".into(),
                            "previous replica subscription has not stopped" => "busy".into(),
                            "replica notice capacity exceeded" => "capacity".into(),
                            other => other.into(),
                        },
                    };
                    assert_eq!(actual, w[3], "{line}");
                }
                "begin" => {
                    let actual = notices.begin_apply(source(w[1]), epoch(w[2]));
                    assert_eq!(if actual.is_ok() { "ok" } else { "replaced" }, w[3], "{line}");
                }
                "release" => notices.end_apply(source(w[1])),
                "stop" => {
                    let error = error(w[4]);
                    let display = error.as_ref().map(|error| error as &dyn fmt::Display);
                    notices.update(source(w[1]), epoch(w[2]), false, state(w[3]), display);
                    notices.stop_receiving(source(w[1]), epoch(w[2]));
                }
                "update" => {
                    let error = error(w[5]);
                    let display = error.as_ref().map(|error| error as &dyn fmt::Display);
                    let pending = notices.update(
                        source(w[1]),
                        epoch(w[2]),
                        w[3] == "true",
                        state(w[4]),
                        display,
                    );
                    assert_eq!(pending.to_string(), w[6], "{line}");
                }
                "failgap" => {
                    let failed = notices.fail_gap(source(w[1]), epoch(w[2]));
                    assert_eq!(failed.to_string(), w[3], "{line}");
                }
                "drain" => {
                    let mut actual = vec![];
                    let drained = notices.drain();
                    actual.push(drained.len().to_string());
                    for notice in drained {
                        actual.push(source_word(notice.source));
                        actual.push(notice.epoch[0].to_string());
                        actual.push(notice.dirty.to_string());
                        actual.push(state_word(notice.state).into());
                        actual.push(notice.error.unwrap_or_else(|| "-".into()));
                    }
                    assert_eq!(actual, w[1..], "{line}");
                }
                "state" => {
                    let actual = match notices.state(source(w[1])) {
                        None => vec!["-".to_string()],
                        Some((epoch, state)) => {
                            vec![epoch[0].to_string(), state_word(state).to_string()]
                        }
                    };
                    assert_eq!(actual, w[2..], "{line}");
                }
                "end" => {
                    notices = trace_notices();
                    traces += 1;
                }
                other => panic!("unknown operation {other}"),
            }
        }
        assert!(traces > 0);
    }

    #[test]
    fn checkpoints_match_regression_cases() {
        let credits = Arc::new(Semaphore::new(1));
        let fresh = || Checkpoints::new(Duration::from_secs(3600), credits.clone());
        let mut checkpoints = fresh();
        let vectors = include_str!("../../tests/fixtures/checkpoints.txt");
        for (index, line) in vectors.lines().enumerate() {
            match line {
                "issue" => checkpoints.issue().unwrap(),
                "stop" => checkpoints.stop_writing().unwrap(),
                "end" => checkpoints = fresh(),
                expected => {
                    let actual = match checkpoints.caught_up() {
                        Ok(reply) => format!("caughtup {reply}"),
                        Err(_) => "caughtup error".into(),
                    };
                    assert_eq!(actual, expected, "checkpoints.txt line {}", index + 1);
                }
            }
        }
    }
}
