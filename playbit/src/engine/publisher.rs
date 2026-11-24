//! Event-driven publication of durable local changes. The feed's allocation
//! credits remain attached through policy projection and AP acknowledgment.
// Error ownership: actor jobs retain feed/control credit through actual
// completion; API errors return to bootstrap/sync and failures retain health.
// Tests inject cancellation/rejection/closed replies, not every OS/store variant
// at every caller. Lost recipients do not release in-flight credit or trigger
// operation replay.
mod account;
mod account_name;
use super::blob::{BlobTransfers, BlobTransport};
use super::completion::Wake;
use super::remote::sync_failure::{self, Cause};
use crate::engine::Result;
use crate::network::ap::{
    ApConnection, ClientError,
    operation::{ServiceRequest, SyncDirection},
    replication::PublicationAck,
    service_client,
};
use crate::store::{
    Id, LocalStore, Session, WorldReplicaRoute,
    change_feed::{CommittedChange, CommittedChangeKind, CommittedChangeReceiver, FeedStatus},
};
#[cfg(test)]
use std::os::unix::net::UnixStream;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    future::{Future, poll_fn},
    io,
    os::fd::{AsRawFd, RawFd},
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};
use tokio::{
    io::unix::AsyncFd,
    runtime::Handle,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{AbortHandle, Id as TaskId, JoinSet},
};

pub(crate) const VOLUME_LIMIT: usize = super::live::SCOPE_LIMIT;
const JOB_LIMIT: usize = 4;
const COMMAND_LIMIT: usize = 8;
const FEED_LIMIT: usize = crate::store::change_feed::RECORD_LIMIT;
const BATCH_RECORD_LIMIT: usize = 32;
const BATCH_BYTE_LIMIT: usize = 32 * 1024;
const ERROR_LIMIT: usize = 128;

/// Bounded inline diagnostic storage. Returned errors are boxed at control boundaries
/// so successful Results stay small without allocating retained notices and commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublisherError {
    bytes: [u8; ERROR_LIMIT],
    len: u8,
    database: Option<crate::store::DatabaseId>,
}
impl PublisherError {
    fn new(value: impl fmt::Display) -> Self {
        Self::for_database(None, value)
    }
    pub(super) fn for_database(
        database: Option<crate::store::DatabaseId>,
        value: impl fmt::Display,
    ) -> Self {
        let mut result = Self { bytes: [0; ERROR_LIMIT], len: 0, database };
        let _ = fmt::write(&mut result, format_args!("{value}"));
        result
    }
    pub(super) fn at_scope(self, key: PublishKey) -> sync_failure::Cause {
        match self.database {
            Some(database) => sync_failure::target_database(key, database, self),
            None => sync_failure::target(key, self),
        }
    }
    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("publisher errors are written from strings at UTF-8 boundaries")
    }
}
impl fmt::Write for PublisherError {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let mut len = usize::from(self.len);
        let truncated = value.len() > self.bytes.len() - len;
        let capacity = self.bytes.len() - if truncated { "…".len() } else { 0 };
        if len > capacity {
            len = capacity;
            while !self.as_str().is_char_boundary(len) {
                len -= 1;
            }
        }
        let mut count = value.len().min(capacity - len);
        while !value.is_char_boundary(count) {
            count -= 1;
        }
        self.bytes[len..len + count].copy_from_slice(&value.as_bytes()[..count]);
        len += count;
        if truncated {
            self.bytes[len..len + "…".len()].copy_from_slice("…".as_bytes());
            len += "…".len();
        }
        self.len = len.try_into().expect("bounded publisher error length");
        if truncated { Err(fmt::Error) } else { Ok(()) }
    }
}
impl fmt::Display for PublisherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::error::Error for PublisherError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PublishKey {
    pub(crate) volume: Id,
    pub(crate) entry: Option<crate::store::EntryId>,
}
impl From<Id> for PublishKey {
    fn from(volume: Id) -> Self {
        Self { volume, entry: None }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PublishScope {
    pub(crate) volume: Id,
    pub(crate) entry: Option<crate::store::EntryId>,
    pub(super) route: WorldReplicaRoute,
    session: Option<([u8; 32], [u8; 32])>,
}
impl PublishScope {
    pub(crate) fn new(volume: Id, session: Option<&Session>) -> Result<Self> {
        fn id(value: &str) -> Result<[u8; 32]> {
            crate::identifier::decode(value).map_err(Into::into)
        }
        Ok(Self {
            volume,
            entry: None,
            route: WorldReplicaRoute::Ordinary,
            session: session
                .map(|s| {
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
                        id(&s.id)?,
                        id(&s.resolved_build_id)?,
                    ))
                })
                .transpose()?,
        })
    }
    pub(super) fn for_volume(
        store: &LocalStore,
        volume: Id,
        session: Option<&Session>,
    ) -> Result<Self> {
        let mut scope = Self::new(volume, session)?;
        scope.route = store.world_replica_route(volume, session)?;
        Ok(scope)
    }
    fn key(&self) -> PublishKey {
        PublishKey { volume: self.volume, entry: self.entry }
    }
    fn request(&self, kind: CommittedChangeKind) -> Result<ServiceRequest> {
        let operation_id = Id::generate()?.to_string();
        let volume_id = self.volume.to_string();
        let CommittedChangeKind::Database(database_id) = kind else {
            return Err("local capability discovery is not an AP publication".into());
        };
        if self.route == WorldReplicaRoute::PublisherWorld {
            let (session, build) =
                self.session.ok_or("publisher World projection requires its session")?;
            return Ok(ServiceRequest::WorldReplica {
                operation_id,
                volume_id,
                session_id: crate::identifier::encode(&session),
                build_id: crate::identifier::encode(&build),
                request: crate::network::ap::operation::WorldReplicaRequest::Publish {
                    database_id,
                },
            });
        }
        Ok(ServiceRequest::PublishReplica {
            operation_id,
            volume_id,
            database_id,
            session_id: self.session.as_ref().map(|(id, _)| crate::identifier::encode(id)),
            build_id: self.session.as_ref().map(|(_, id)| crate::identifier::encode(id)),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublishState {
    NeedsBootstrap,
    Bootstrapping,
    /// The service cannot read everything this scope's reader can. Nothing is
    /// published until an authorized peer supplies the excluded data. Not a
    /// failure: a later bootstrap replaces it once coverage exists.
    Excluded,
    Ready,
    Pending,
    NeedsRecovery,
    Disconnected,
    Failed,
}
/// Publisher transport/projection health only, not all-data synchronization or
/// proof that an incoming replica has no newer network events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublishHealth {
    Disconnected,
    Failed,
    Recovering,
    Pending,
    Private,
    Ready,
}
#[derive(Clone, Debug)]
pub(crate) struct PublishNotice {
    pub(crate) volume: Id,
    pub(crate) entry: Option<crate::store::EntryId>,
    pub(crate) account_ready: bool,
    pub(crate) generation: u64,
    pub(crate) state: PublishState,
    /// Locally settled by current service-exclusion policy, without an AP receipt.
    pub(crate) excluded_publication: Option<u64>,
    pub(crate) error: Option<PublisherError>,
}
#[derive(Default)]
pub(crate) struct PublisherNotices {
    pub(crate) volumes: Vec<PublishNotice>,
    pub(crate) error: Option<PublisherError>,
}
struct Notices {
    account: Option<account::Notice>,
    volumes: Vec<PublishSlot>,
    error: Option<PublisherError>,
    observed: Option<FeedStatus>,
    // Draining the diagnostic must not erase an unresolved global failure.
    faulted: bool,
    // Scope capacity is unresolved but recoverable; `retire_volumes` clears it.
    capacity_exhausted: bool,
}
struct PublishSlot {
    notice: PublishNotice,
    pending: bool,
    initialization_pending: bool,
}
impl Default for Notices {
    fn default() -> Self {
        Self {
            account: None,
            volumes: Vec::with_capacity(VOLUME_LIMIT),
            error: None,
            observed: None,
            faulted: false,
            capacity_exhausted: false,
        }
    }
}
struct Signals {
    notices: Mutex<Notices>,
    completion: Arc<Wake>,
}
impl Signals {
    fn wake(&self) {
        if let Err(failure) = self.completion.try_notify() {
            let mut notices = self.notices.lock().expect("mutex is not poisoned");
            notices.error = Some(PublisherError::new(failure));
            notices.faulted = true;
        }
    }
    fn volume(&self, notice: PublishNotice) {
        let mut notices = self.notices.lock().expect("mutex is not poisoned");
        assert!(notices.volumes.len() <= VOLUME_LIMIT);
        if let Some(slot) = notices
            .volumes
            .iter_mut()
            .find(|s| s.notice.volume == notice.volume && s.notice.entry == notice.entry)
        {
            if matches!(
                notice.state,
                PublishState::Bootstrapping | PublishState::Ready | PublishState::Pending
            ) || (notice.state == PublishState::Failed
                && slot.notice.state == PublishState::NeedsBootstrap)
            {
                // A failed attempt can retire and recreate its scope before
                // reaching Bootstrapping. Settle that replacement obligation.
                // Repeated Failed notices preserve an explicit transport retry.
                slot.initialization_pending = false;
            }
            slot.notice = notice;
            slot.pending = true;
        } else if notices.volumes.len() < VOLUME_LIMIT {
            let initialization_pending =
                notice.entry.is_none() && notice.state == PublishState::NeedsBootstrap;
            notices.volumes.push(PublishSlot { notice, pending: true, initialization_pending });
        } else {
            notices.error = Some(PublisherError::new("publication notice capacity exhausted"));
            notices.capacity_exhausted = true;
        }
        drop(notices);
        self.wake();
    }
    /// Terminal: nothing clears this short of a new publisher.
    fn error(&self, error: impl fmt::Display) {
        #[cfg(feature = "test-support")]
        crate::diagnostic!("publication failed: {error}");
        let mut notices = self.notices.lock().expect("mutex is not poisoned");
        notices.error = Some(PublisherError::new(error));
        notices.faulted = true;
        drop(notices);
        self.wake();
    }
    /// Scope capacity is unhealthy while it lasts, but forgetting a volume
    /// frees a slot, so `Actor::retire_volumes` can resolve it.
    fn capacity_error(&self, error: impl fmt::Display) {
        let mut notices = self.notices.lock().expect("mutex is not poisoned");
        notices.error = Some(PublisherError::new(error));
        notices.capacity_exhausted = true;
        drop(notices);
        self.wake();
    }
}

/// Keeps one exact publication generation alive without using command credits.
/// Dropping it wakes the actor and requires recovery; completed writes persist.
#[derive(Debug)]
pub(crate) struct PublicationLease {
    send: oneshot::Sender<()>,
}
impl PublicationLease {
    /// Give the generation up with the shared connection. The actor records a
    /// disconnect instead of a lost owner, whichever it observes first.
    pub(crate) fn disconnect(self) {
        let _ = self.send.send(());
    }
}

/// Only returned after older queued changes are covered by the upcoming snapshot.
/// Publication sequence is deliberately absent: it is not a global FIFO cursor.
#[derive(Debug)]
pub(crate) struct BootstrapToken {
    volume: PublishKey,
    generation: u64,
    epoch: Id,
    gap: u64,
    lease: PublicationLease,
}
impl BootstrapToken {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    /// Installs this generation's lease without an AP connection.
    #[cfg(test)]
    pub(crate) fn test_lease(self) -> PublicationLease {
        self.lease
    }
}

/// The outgoing prefix settled under this exact bootstrap generation. This is
/// service-readable publication coverage, not acknowledgment of excluded data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FencedVolume {
    pub(crate) volume: PublishKey,
    pub(crate) generation: u64,
}
struct FenceTarget {
    scope: FencedVolume,
    accepted: u64,
}
struct Fence {
    targets: Vec<FenceTarget>,
    remaining: usize,
    status: FeedStatus,
    reply: Reply<Vec<FencedVolume>, Cause>,
    _admission: FenceAdmission,
}
struct FenceAdmission(Arc<AtomicBool>);
impl Drop for FenceAdmission {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Fence {
    fn check(
        &self,
        status: FeedStatus,
        mut settled: impl FnMut(FencedVolume) -> Result<u64>,
    ) -> Result<bool> {
        if status.closed.is_some() || status != self.status {
            return Err(PublisherError::new(
                "committed-change feed changed during publication fence",
            )
            .into());
        }
        let mut complete = self.remaining == 0;
        for target in &self.targets {
            complete &= settled(target.scope)? >= target.accepted;
        }
        Ok(complete)
    }
}
struct Reply<T, E = PublisherError> {
    send: oneshot::Sender<std::result::Result<T, E>>,
    // Shared with incoming checkpoint commands; cancellation is not settlement.
    _permit: OwnedSemaphorePermit,
}
impl<T, E> Reply<T, E> {
    fn is_closed(&self) -> bool {
        self.send.is_closed()
    }
    fn send(
        self,
        result: std::result::Result<T, E>,
    ) -> std::result::Result<(), Box<std::result::Result<T, E>>> {
        let Self { send, _permit: permit } = self;
        // The caller may immediately submit its next bounded command.
        drop(permit);
        send.send(result).map_err(Box::new)
    }
}
enum Command {
    Account(account::Command),
    FailInitialization {
        volume: PublishKey,
        error: PublisherError,
        reply: Reply<()>,
    },
    Begin {
        volume: PublishKey,
        reply: Reply<BootstrapToken>,
    },
    Finish {
        token: BootstrapToken,
        scope: PublishScope,
        connection: Arc<ApConnection>,
        reply: Reply<PublicationLease>,
    },
    Forget {
        volume: PublishKey,
        reply: Reply<()>,
    },
    Abandon {
        token: BootstrapToken,
        reply: Reply<()>,
    },
    Fence {
        volumes: Box<[PublishKey; VOLUME_LIMIT]>,
        count: usize,
        reply: Reply<Vec<FencedVolume>, Cause>,
        admission: FenceAdmission,
    },
    Disconnect {
        _permit: OwnedSemaphorePermit,
    },
}

pub(crate) struct Publisher {
    actor: AbortHandle,
    shutdown: AtomicBool,
    commands: mpsc::Sender<Box<Command>>,
    signals: Arc<Signals>,
    control_slots: Arc<Semaphore>,
    fence_busy: Arc<AtomicBool>,
}
impl Publisher {
    pub(crate) fn start(
        handle: &Handle,
        store: Arc<LocalStore>,
        completion: Arc<Wake>,
        apply_slots: Arc<Semaphore>,
        control_slots: Arc<Semaphore>,
        blob_transfers: Arc<BlobTransfers>,
        scope: super::remote::SharedClientScope,
    ) -> Result<Self> {
        if apply_slots.available_permits() > 2 {
            return Err("publisher storage concurrency exceeds two slots".into());
        }
        if control_slots.available_permits() > COMMAND_LIMIT {
            return Err("publisher control concurrency exceeds eight slots".into());
        }
        let receiver =
            store.take_change_receiver().ok_or("committed-change receiver already acquired")?;
        let _runtime = handle.enter();
        // SAFETY: FeedFd owns the receiver's socket, which drain/sample_gauges
        // never replace or close. AsyncFd deregisters before dropping it.
        #[allow(unsafe_code, reason = "The feed owns its stable readiness socket.")]
        let feed = unsafe { AsyncFd::register(FeedFd(receiver)) }.map_err(io::Error::from)?;
        let signals = Arc::new(Signals { notices: Mutex::new(Notices::default()), completion });
        let (commands, receive) = mpsc::channel(COMMAND_LIMIT);
        let status = store.change_status();
        let actor = handle.spawn(
            Actor {
                store,
                scope,
                blob_transfers,
                feed,
                feed_pending: true,
                receive,
                signals: signals.clone(),
                apply_slots,
                volumes: BTreeMap::new(),
                account: None,
                account_generation: 0,
                jobs: JoinSet::new(),
                status,
                gauges: PublisherGauges::new(),
                cursor: None,
                event_cursor: 0,
                stopping: false,
                capacity_reported: false,
                generation: 0,
                fence: None,
            }
            .run(),
        );
        Ok(Self {
            actor: actor.abort_handle(),
            shutdown: AtomicBool::new(false),
            commands,
            signals,
            control_slots,
            fence_busy: Arc::new(AtomicBool::new(false)),
        })
    }
    #[allow(
        clippy::type_complexity,
        reason = "The one-use result pairs the permit-owning reply with its typed receiver."
    )]
    fn reply<T, E>(&self) -> Result<(Reply<T, E>, oneshot::Receiver<std::result::Result<T, E>>)> {
        let permit = self
            .control_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "replica control capacity is busy or closed")?;
        let (send, receive) = oneshot::channel();
        Ok((Reply { send, _permit: permit }, receive))
    }
    fn send(&self, command: Command) -> Result<()> {
        assert!(self.commands.capacity() <= COMMAND_LIMIT);
        self.commands.try_send(Box::new(command)).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                "replica publisher command mailbox is busy".into()
            }
            mpsc::error::TrySendError::Closed(_) => "replica publisher is closed".into(),
        })
    }
    pub(crate) async fn begin_bootstrap(
        &self,
        volume: impl Into<PublishKey>,
    ) -> Result<BootstrapToken> {
        let (reply, result) = self.reply()?;
        self.send(Command::Begin { volume: volume.into(), reply })?;
        result.await.map_err(|_| "replica publisher closed during bootstrap")?.map_err(Into::into)
    }
    pub(crate) async fn finish_bootstrap(
        &self,
        token: BootstrapToken,
        scope: PublishScope,
        connection: Arc<ApConnection>,
    ) -> Result<PublicationLease> {
        let (reply, result) = self.reply()?;
        self.send(Command::Finish { token, scope, connection, reply })?;
        result.await.map_err(|_| "replica publisher closed during bootstrap")?.map_err(Into::into)
    }
    /// Permanent owner-loss shutdown, independent of control-mailbox capacity.
    /// Cancellation drops queued replies, feed leases and child jobs when the
    /// actor next yields; already admitted durable or remote work is not undone.
    /// Ordinary connection loss uses disconnect so this actor can reconnect.
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.actor.abort();
    }
    pub(crate) fn disconnect(&self) -> Result<()> {
        let permit = self
            .control_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "replica control capacity is busy or closed")?;
        self.send(Command::Disconnect { _permit: permit })
    }
    /// Cover every feed enqueue completed before this call, plus at most the
    /// bounded concurrent FIFO prefix. Later edits cannot prolong this fence.
    /// No network or durable acknowledgment is awaited by a local producer.
    pub(crate) async fn fence(&self, volumes: &[Id]) -> Result<Vec<FencedVolume>> {
        if volumes.len() > VOLUME_LIMIT {
            return Err("publication fence volume limit exceeded".into());
        }
        let scopes: Vec<_> = volumes.iter().copied().map(Into::into).collect();
        self.fence_scopes(&scopes).await
    }
    pub(crate) async fn fence_scopes(&self, volumes: &[PublishKey]) -> Result<Vec<FencedVolume>> {
        let _timing = crate::trace::Interval::new("replication.fence");
        if volumes.len() > VOLUME_LIMIT {
            return Err("publication fence volume limit exceeded".into());
        }
        if volumes.iter().enumerate().any(|(index, volume)| volumes[..index].contains(volume)) {
            return Err("publication fence repeats a volume".into());
        }
        if volumes.is_empty() {
            return Ok(Vec::new());
        }
        self.fence_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "publication fence is busy")?;
        let admission = FenceAdmission(self.fence_busy.clone());
        let (reply, result) = self.reply()?;
        let mut fixed = Box::new([PublishKey::from(Id::from_bytes([0; 16])); VOLUME_LIMIT]);
        fixed[..volumes.len()].copy_from_slice(volumes);
        self.send(Command::Fence { volumes: fixed, count: volumes.len(), reply, admission })?;
        result.await.map_err(|_| "replica publisher closed during fence")?
    }
    /// A deliberate bootstrap abandonment, not a lost owner. The caller hands
    /// the token back instead of dropping it so this generation is retired as
    /// excluded rather than reported as a crashed publication owner.
    pub(crate) async fn abandon_bootstrap(&self, token: BootstrapToken) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(Command::Abandon { token, reply })?;
        result.await.map_err(|_| "replica publisher closed during bootstrap")?.map_err(Into::into)
    }
    /// Snapshot retained keys without consuming diagnostics or initialization
    /// notices. Provider selection already excludes these keys before cleanup.
    #[cfg(any(test, feature = "file-provider"))]
    pub(crate) fn retained_scopes(&self) -> Vec<PublishKey> {
        let notices = self.signals.notices.lock().expect("mutex is not poisoned");
        assert!(notices.volumes.len() <= VOLUME_LIMIT);
        notices
            .volumes
            .iter()
            .map(|slot| PublishKey { volume: slot.notice.volume, entry: slot.notice.entry })
            .collect()
    }
    /// The lifecycle owner decides whether this volume remains eligible. Waits
    /// outside UI work until its current job releases its lease; future local
    /// changes may re-admit it as unknown and request reconciliation again.
    /// No persistent suppression list or unbounded tombstones are retained.
    pub(crate) async fn forget_scope(&self, volume: PublishKey) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(Command::Forget { volume, reply })?;
        result
            .await
            .map_err(|_| "replica publisher closed during volume removal")?
            .map_err(Into::into)
    }
    pub(crate) fn drain_notices(&self) -> PublisherNotices {
        let mut notices = self.signals.notices.lock().expect("mutex is not poisoned");
        assert!(notices.volumes.len() <= VOLUME_LIMIT);
        let volumes = notices
            .volumes
            .iter_mut()
            .filter(|slot| slot.pending)
            .map(|slot| {
                slot.pending = false;
                // Draining delivers a transition once. Its terminal cause still
                // describes this generation until another notice replaces it.
                slot.notice.clone()
            })
            .collect();
        PublisherNotices { volumes, error: notices.error.take() }
    }
    /// Diagnostic notice reads cannot consume this lifecycle obligation.
    /// An unsuccessful initial attempt needs explicit retry or reconnect;
    /// subsequent local edits do not repeatedly schedule the same operation.
    pub(crate) fn needs_initialization(&self) -> bool {
        self.signals
            .notices
            .lock()
            .expect("mutex is not poisoned")
            .volumes
            .iter()
            .any(|slot| slot.initialization_pending)
    }
    pub(crate) async fn fail_initialization(&self, volume: Id, error: &str) -> Result<()> {
        let (reply, result) = self.reply()?;
        self.send(Command::FailInitialization {
            volume: volume.into(),
            error: PublisherError::new(error),
            reply,
        })?;
        result
            .await
            .map_err(|_| "publisher closed while recording initialization failure")?
            .map_err(Into::into)
    }
    pub(crate) fn take_initializations(&self) -> Vec<Id> {
        let _timing = crate::trace::Span::new("replication.initialization_claim");
        let mut notices = self.signals.notices.lock().expect("mutex is not poisoned");
        assert!(notices.volumes.len() <= VOLUME_LIMIT);
        let mut volumes = Vec::with_capacity(VOLUME_LIMIT);
        for slot in &mut notices.volumes {
            if slot.initialization_pending {
                slot.initialization_pending = false;
                volumes.push(slot.notice.volume);
            }
        }
        volumes
    }
    pub(crate) fn retry_initializations_after_transport_loss(&self, volumes: &[Id]) {
        let mut notices = self.signals.notices.lock().expect("mutex is not poisoned");
        for slot in &mut notices.volumes {
            if volumes.contains(&slot.notice.volume)
                && !matches!(slot.notice.state, PublishState::Ready | PublishState::Pending)
            {
                slot.initialization_pending = true;
            }
        }
    }
    /// A bounded observation: no database reads, allocations, or acknowledgment
    /// waits. Feed credits cover enqueued and retained publication records. A
    /// caller must obtain `current` and `pending` from this publisher's store.
    /// Scope generations remain in the retained slots; acknowledgments from an
    /// older generation never promote the current bootstrap/recovery state.
    pub(crate) fn health(&self, current: FeedStatus, pending: usize) -> PublishHealth {
        if self.shutdown.load(Ordering::Acquire) {
            return PublishHealth::Disconnected;
        }
        if self.actor.is_finished() || self.commands.is_closed() || current.closed.is_some() {
            return PublishHealth::Failed;
        }
        let notices = self.signals.notices.lock().expect("mutex is not poisoned");
        if notices.faulted || notices.capacity_exhausted {
            return PublishHealth::Failed;
        }
        let mut disconnected = false;
        let mut recovering = notices.observed != Some(current);
        let mut updating = pending != 0;
        let mut private = false;
        if let Some(account) = &notices.account {
            match account.state {
                PublishState::Failed => return PublishHealth::Failed,
                PublishState::Disconnected => disconnected = true,
                PublishState::NeedsBootstrap
                | PublishState::Bootstrapping
                | PublishState::NeedsRecovery => recovering = true,
                PublishState::Pending => updating = true,
                PublishState::Excluded => private = true,
                PublishState::Ready => {}
            }
        }
        for slot in &notices.volumes {
            let notice = &slot.notice;
            match notice.state {
                PublishState::Failed => return PublishHealth::Failed,
                PublishState::Disconnected => disconnected = true,
                PublishState::NeedsBootstrap
                | PublishState::Bootstrapping
                | PublishState::NeedsRecovery => recovering = true,
                PublishState::Pending => updating = true,
                PublishState::Excluded => private = true,
                PublishState::Ready => {}
            }
            recovering |= notice.generation == 0;
            private |= notice.excluded_publication.is_some();
        }
        if disconnected {
            PublishHealth::Disconnected
        } else if recovering {
            PublishHealth::Recovering
        } else if updating {
            PublishHealth::Pending
        } else if private {
            PublishHealth::Private
        } else {
            PublishHealth::Ready
        }
    }
    pub(crate) fn failed_publication(&self, volume: Id) -> Option<sync_failure::Cause> {
        let notices = self.signals.notices.lock().expect("mutex is not poisoned");
        let slot = notices.volumes.iter().find(|slot| {
            slot.notice.volume == volume
                && slot.notice.entry.is_none()
                && slot.notice.state == PublishState::Failed
        })?;
        let cause = slot.notice.error.clone()?;
        drop(notices);
        Some(cause.at_scope(volume.into()))
    }

    pub(crate) fn state(&self, volume: impl Into<PublishKey>) -> Option<(u64, PublishState)> {
        let volume = volume.into();
        self.signals
            .notices
            .lock()
            .expect("mutex is not poisoned")
            .volumes
            .iter()
            .find(|slot| slot.notice.volume == volume.volume && slot.notice.entry == volume.entry)
            .map(|slot| (slot.notice.generation, slot.notice.state))
    }

    #[cfg(test)]
    pub(super) fn test_database_failure(
        &self,
        volume: Id,
        generation: u64,
        database: crate::store::DatabaseId,
        reason: &str,
    ) {
        let cause = PublisherError::for_database(Some(database), reason);
        self.signals.volume(PublishNotice {
            volume,
            entry: None,
            account_ready: false,
            generation,
            state: PublishState::Failed,
            excluded_publication: None,
            error: Some(cause),
        });
    }

    #[cfg(test)]
    pub(crate) fn test_state(
        &self,
        volume: impl Into<PublishKey>,
        generation: u64,
        state: PublishState,
    ) {
        let volume = volume.into();
        self.signals.volume(PublishNotice {
            volume: volume.volume,
            entry: volume.entry,
            account_ready: false,
            generation,
            state,
            excluded_publication: None,
            error: None,
        });
    }
}

struct FeedFd(CommittedChangeReceiver);
impl AsRawFd for FeedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.readiness().expect("AP publisher is composed with a Unix change feed").as_raw_fd()
    }
}
struct Active {
    id: TaskId,
    cancel: Option<oneshot::Sender<()>>,
}
enum Phase {
    NeedsBootstrap,
    Preparing { remaining: Option<usize>, reply: Reply<BootstrapToken> },
    Bootstrapping { gap: u64 },
    Ready { scope: PublishScope, connection: Arc<ApConnection>, gap: u64 },
    Removing { reply: Reply<()> },
    // Bootstrap deliberately abandoned: the service is not authorized to read
    // everything this scope's reader can. The slot stays so the volume neither
    // reports a failure nor re-requests initialization.
    Excluded,
    NeedsRecovery,
    Disconnected,
    Failed,
}
struct Volume {
    account_ready: bool,
    generation: u64,
    phase: Phase,
    queued: VecDeque<Arc<CommittedChange>>,
    active: Option<Active>,
    // Receiver belongs only to this generation while Bootstrapping/Ready.
    cancellation: Option<oneshot::Receiver<()>>,
    excluded_publication: Option<u64>,
    error: Option<PublisherError>,
    // Actor-local FIFO counts within this generation, never allocated feed IDs.
    accepted: u64,
    settled: u64,
}
impl Volume {
    fn new() -> Self {
        Self {
            account_ready: false,
            generation: 0,
            phase: Phase::NeedsBootstrap,
            queued: VecDeque::new(),
            active: None,
            cancellation: None,
            excluded_publication: None,
            error: None,
            accepted: 0,
            settled: 0,
        }
    }
    fn notice(&self, volume: impl Into<PublishKey>) -> PublishNotice {
        let volume = volume.into();
        let state = match self.phase {
            Phase::NeedsBootstrap => PublishState::NeedsBootstrap,
            Phase::Preparing { .. } | Phase::Bootstrapping { .. } => PublishState::Bootstrapping,
            Phase::Ready { .. } if self.active.is_some() || !self.queued.is_empty() => {
                PublishState::Pending
            }
            Phase::Ready { .. } => PublishState::Ready,
            Phase::Excluded => PublishState::Excluded,
            Phase::NeedsRecovery => PublishState::NeedsRecovery,
            Phase::Disconnected | Phase::Removing { .. } => PublishState::Disconnected,
            Phase::Failed => PublishState::Failed,
        };
        PublishNotice {
            volume: volume.volume,
            entry: volume.entry,
            account_ready: self.account_ready,
            generation: self.generation,
            state,
            excluded_publication: self.excluded_publication,
            error: self.error.clone(),
        }
    }
    fn cancel(&mut self) {
        if let Some(cancel) = self.active.as_mut().and_then(|a| a.cancel.take()) {
            let _ = cancel.send(());
        }
    }
    fn invalidate(&mut self, phase: Phase, error: PublisherError) {
        #[cfg(feature = "test-support")]
        if matches!(phase, Phase::Failed) {
            crate::diagnostic!("publication generation {} failed: {error}", self.generation);
        }
        match std::mem::replace(&mut self.phase, phase) {
            Phase::Preparing { reply, .. } => {
                let _ = reply.send(Err(error.clone()));
            }
            Phase::Removing { reply } => {
                let _ = reply.send(Err(error.clone()));
            }
            _ => {}
        }
        self.error = Some(error);
        self.cancellation = None;
        self.cancel();
        // Already durable locally; explicit recovery is now mandatory.
        self.queued.clear();
    }
    /// One completed job of this volume's current publication generation.
    /// Returns a non-terminal diagnostic to report with the next notice: the
    /// lane keeps its phase and its remaining queued changes.
    fn settle(&mut self, result: VolumeJobResult) -> Option<PublisherError> {
        let mut diagnostic = result.diagnostic.map(|cause| *cause);
        let settled = match result.result {
            Ok(Some(_)) => true,
            Ok(None) => {
                self.excluded_publication = Some(result.publication);
                true
            }
            Err(error) => match error.phase() {
                (Some(phase), error) => {
                    self.invalidate(phase, error);
                    false
                }
                // Only the published record failed. It is dropped, not retried.
                (None, error) => {
                    diagnostic = Some(error);
                    true
                }
            },
        };
        if settled {
            // Single active job per volume preserves accepted FIFO order.
            assert!(self.settled <= self.accepted, "settlement cannot precede admission");
            assert!(
                result.records <= self.accepted - self.settled,
                "settlement covers accepted records only"
            );
            self.settled += result.records; // Never exceeds checked accepted count.
        }
        diagnostic
    }
}
// Only executing jobs need this discriminator. Retained notice/error storage
// keeps its existing size; the actor publishes the corresponding phase.
#[derive(Debug)]
enum JobError {
    Failed(PublisherError),
    Disconnected(PublisherError),
}
impl From<PublisherError> for JobError {
    fn from(error: PublisherError) -> Self {
        Self::Failed(error)
    }
}
impl From<Box<PublisherError>> for JobError {
    fn from(error: Box<PublisherError>) -> Self {
        Self::Failed(*error)
    }
}
impl JobError {
    /// The phase this failure puts the volume in, or `None` when only the
    /// published record failed and the lane keeps the phase it has.
    fn phase(self) -> (Option<Phase>, PublisherError) {
        match self {
            Self::Failed(error) => (Some(Phase::Failed), error),
            Self::Disconnected(error) => (Some(Phase::Disconnected), error),
        }
    }
}

// A released lease ends its generation with the shared connection; a dropped
// one means the generation lost its owner and stays terminal.
fn lease_loss(released: bool) -> (Phase, PublisherError) {
    if released {
        (Phase::Disconnected, PublisherError::new("AP connection disconnected; recovery required"))
    } else {
        (Phase::Failed, PublisherError::new("publication generation owner dropped"))
    }
}

// Application/protocol closure and request timeout alone do not prove a lost
// transport. In particular an AP rejection must never become a retry loop.
fn ap_failure(error: &(dyn std::error::Error + 'static)) -> JobError {
    let lost = error.downcast_ref::<ClientError>().is_some_and(ClientError::is_transport_lost);
    let error = PublisherError::new(error);
    if lost { JobError::Disconnected(error) } else { JobError::Failed(error) }
}

enum JobResult {
    Volume(VolumeJobResult),
    Account { generation: u64, result: std::result::Result<account::Progress, JobError> },
}
struct VolumeJobResult {
    volume: PublishKey,
    generation: u64,
    publication: u64,
    records: u64,
    result: std::result::Result<Option<PublicationAck>, JobError>,
    /// An independent bookkeeping error after settlement. Allocate only on failure.
    diagnostic: Option<Box<PublisherError>>,
}
#[allow(
    clippy::large_enum_variant,
    reason = "The actor selects one bounded event at a time without a per-completion allocation."
)]
enum Event {
    Command(Option<Box<Command>>),
    Feed,
    FeedError(io::Error),
    Cancelled { volume: PublishKey, generation: u64, released: bool },
    Job(std::result::Result<(TaskId, JobResult), tokio::task::JoinError>),
}
struct PublisherGauges {
    jobs: crate::trace::Gauge,
    queued: crate::trace::Gauge,
    commands: crate::trace::Gauge,
}
impl PublisherGauges {
    fn new() -> Self {
        use crate::trace::Gauge;
        Self {
            jobs: Gauge::new("publisher.sampled_jobs", 0),
            queued: Gauge::new("publisher.sampled_queued_records", 0),
            commands: Gauge::new("publisher.sampled_commands", 0),
        }
    }
}
const _: () = assert!(std::mem::size_of::<PublisherGauges>() <= 3 * 96);

struct Actor {
    store: Arc<LocalStore>,
    scope: super::remote::SharedClientScope,
    blob_transfers: Arc<BlobTransfers>,
    feed: AsyncFd<FeedFd>,
    feed_pending: bool,
    receive: mpsc::Receiver<Box<Command>>,
    signals: Arc<Signals>,
    apply_slots: Arc<Semaphore>,
    volumes: BTreeMap<PublishKey, Volume>,
    account: Option<account::Account>,
    account_generation: u64,
    jobs: JoinSet<JobResult>,
    status: FeedStatus,
    gauges: PublisherGauges,
    cursor: Option<PublishKey>,
    event_cursor: usize,
    stopping: bool,
    capacity_reported: bool,
    // Tokens remain unique even after a volume state is removed and re-admitted.
    generation: u64,
    // One admitted fence, with at most 512 targets and one bounded reply.
    fence: Option<Fence>,
}
impl Actor {
    async fn run(mut self) {
        loop {
            self.check_fence();
            self.settle_account();
            self.retire_volumes();
            self.start_drains();
            self.start_jobs();
            self.sample_gauges();
            if self.stopping && self.jobs.is_empty() {
                break;
            }
            let event = poll_fn(|cx| {
                for offset in 0..4 {
                    let source = (self.event_cursor + offset) % 4;
                    let event = match source {
                        0 if !self.stopping => self.receive.poll_recv(cx).map(Event::Command),
                        1 if !self.jobs.is_empty() => {
                            self.jobs.poll_join_next_with_id(cx).map(|r| {
                                Event::Job(r.expect(
                                    "the job select arm is enabled only for a nonempty JoinSet",
                                ))
                            })
                        }
                        2 if !self.stopping => {
                            if self.feed_pending {
                                Poll::Ready(Event::Feed)
                            } else {
                                match self.feed.poll_read_ready(cx) {
                                    Poll::Ready(Ok(mut guard)) => {
                                        guard.clear_ready();
                                        Poll::Ready(Event::Feed)
                                    }
                                    Poll::Ready(Err(error)) => Poll::Ready(Event::FeedError(error)),
                                    Poll::Pending => Poll::Pending,
                                }
                            }
                        }
                        3 if !self.stopping => {
                            let mut cancelled = Poll::Pending;
                            for (volume, state) in &mut self.volumes {
                                if let Some(receive) = &mut state.cancellation
                                    && let Poll::Ready(result) =
                                        std::pin::Pin::new(receive).poll(cx)
                                {
                                    cancelled = Poll::Ready(Event::Cancelled {
                                        volume: *volume,
                                        generation: state.generation,
                                        released: result.is_ok(),
                                    });
                                    break;
                                }
                            }
                            cancelled
                        }
                        _ => Poll::Pending,
                    };
                    if event.is_ready() {
                        self.event_cursor = (source + 1) % 4;
                        return event;
                    }
                }
                Poll::Pending
            })
            .await;
            match event {
                Event::Command(Some(command)) => self.command(*command),
                Event::Command(None) => {
                    self.stopping = true;
                    self.disconnect();
                }
                Event::Feed => self.drain_feed(),
                Event::FeedError(error) => {
                    self.fail(error);
                }
                Event::Job(result) => self.completed(result),
                Event::Cancelled { volume, generation, released } => {
                    self.cancel_generation(volume, generation, released)
                }
            }
            self.sample_gauges();
            // Ready queues use bounded turns; they cannot monopolize a runtime worker.
            tokio::task::yield_now().await;
        }
    }
    fn sample_gauges(&mut self) {
        // At most 512 scope queues; no payload scan and no freshness timer.
        let queued: usize = self.volumes.values().map(|state| state.queued.len()).sum();
        self.gauges.jobs.set(self.jobs.len() as u64);
        self.gauges.queued.set(queued as u64);
        self.gauges.commands.set(self.receive.len() as u64);
        self.feed.get_mut().0.sample_gauges();
    }
    fn ensure_volume(
        &mut self,
        volume: PublishKey,
    ) -> std::result::Result<&mut Volume, Box<PublisherError>> {
        if !self.volumes.contains_key(&volume) {
            if self.volumes.len() + usize::from(self.account.is_some()) == VOLUME_LIMIT {
                return Err(PublisherError::new(
                    "live publication volume limit reached; bootstrap admission required",
                )
                .into());
            }
            let state = Volume::new();
            self.signals.volume(state.notice(volume));
            self.volumes.insert(volume, state);
        }
        Ok(self
            .volumes
            .get_mut(&volume)
            .expect("publication volume was initialized or checked above"))
    }
    fn command(&mut self, command: Command) {
        assert!(self.receive.len() <= COMMAND_LIMIT);
        self.cancel_dropped_leases();
        self.observe(self.store.change_status());
        match command {
            Command::Account(command) => self.account_command(command),
            Command::FailInitialization { volume, error, reply } => {
                if let Some(state) = self.volumes.get_mut(&volume) {
                    // A completed or newer bootstrap keeps its own state.
                    if matches!(state.phase, Phase::NeedsBootstrap) {
                        state.invalidate(Phase::Failed, error);
                        self.signals.volume(state.notice(volume));
                    }
                }
                let _ = reply.send(Ok(()));
            }
            Command::Begin { volume, reply } => {
                self.begin_bootstrap(volume, reply);
            }
            Command::Finish { token, scope, connection, reply } => {
                self.finish_bootstrap(token, scope, connection, reply);
            }
            Command::Forget { volume, reply } => {
                if let Some(state) = self.volumes.get_mut(&volume) {
                    if matches!(state.phase, Phase::Removing { .. }) {
                        let _ = reply.send(Err(PublisherError::new(
                            "publication volume removal is pending",
                        )));
                    } else {
                        state.invalidate(
                            Phase::Removing { reply },
                            PublisherError::new(
                                "publication volume removed; future use requires bootstrap",
                            ),
                        );
                    }
                } else {
                    let _ = reply.send(Ok(()));
                }
            }
            Command::Abandon { token, reply } => {
                if let Some(state) = self.volumes.get_mut(&token.volume) {
                    // Only this exact generation; a replacement bootstrap owns
                    // its own state. Dropping the cancellation receiver first
                    // retires the lease silently: abandonment is neither a
                    // release, which records a disconnect, nor a lost owner.
                    if state.generation == token.generation
                        && matches!(state.phase, Phase::Bootstrapping { gap } if gap == token.gap)
                    {
                        state.phase = Phase::Excluded;
                        state.cancellation = None;
                        state.error = None;
                        // Already durable locally; a later bootstrap reseeds them.
                        state.queued.clear();
                        self.signals.volume(state.notice(token.volume));
                    }
                }
                let _ = reply.send(Ok(()));
            }
            Command::Disconnect { _permit } => {
                self.disconnect();
                drop(_permit);
            }
            Command::Fence { volumes, count, reply, admission } => {
                self.begin_fence(volumes, count, reply, admission);
            }
        }
    }

    fn begin_bootstrap(&mut self, volume: PublishKey, reply: Reply<BootstrapToken>) {
        if let Err(error) = self.store.require_replication_supported(volume.volume) {
            let _ = reply.send(Err(PublisherError::new(error)));
            return;
        }
        if self.status.closed.is_some() {
            let _ = reply.send(Err(PublisherError::new("committed-change feed is closed")));
            return;
        }
        let Some(generation) = self.generation.checked_add(1) else {
            let error = PublisherError::new("publication generation exhausted");
            self.fail(&error);
            let _ = reply.send(Err(error));
            return;
        };
        let state = match self.ensure_volume(volume) {
            Ok(state) => state,
            Err(error) => {
                let _ = reply.send(Err(*error));
                return;
            }
        };
        if matches!(&state.phase, Phase::Preparing { reply, .. } if !reply.is_closed()) {
            let _ = reply.send(Err(PublisherError::new("volume bootstrap already preparing")));
            return;
        }
        if matches!(state.phase, Phase::Removing { .. }) {
            let _ = reply.send(Err(PublisherError::new("publication volume removal is pending")));
            return;
        }
        state.generation = generation;
        state.cancellation = None;
        state.accepted = 0;
        state.settled = 0;
        state.error = None;
        state.excluded_publication = None;
        state.phase = Phase::Preparing { remaining: None, reply };
        let notice = state.notice(volume);
        self.generation = generation;
        self.signals.volume(notice);
    }

    fn finish_bootstrap(
        &mut self,
        token: BootstrapToken,
        scope: PublishScope,
        connection: Arc<ApConnection>,
        reply: Reply<PublicationLease>,
    ) {
        let status = self.store.change_status();
        let recoverable_gap = scope.key() == token.volume
            && status.closed.is_none()
            && (status.epoch != token.epoch || status.gap_generation != token.gap);
        let result =
            self.check_bootstrap(&token, scope.key(), status).map_err(|error| *error).map(|()| {
                let state = self
                    .volumes
                    .get_mut(&token.volume)
                    .expect("the token was validated against this volume generation");
                state.phase = Phase::Ready { scope, connection, gap: token.gap };
                state.error = None;
                self.signals.volume(state.notice(token.volume));
                token.lease
            });
        if let Err(error) = &result
            && let Some(state) = self.volumes.get_mut(&token.volume)
        {
            // A stale completion must never invalidate a newer bootstrap.
            if state.generation == token.generation {
                state.invalidate(
                    if recoverable_gap { Phase::NeedsRecovery } else { Phase::Failed },
                    error.clone(),
                );
                self.signals.volume(state.notice(token.volume));
            }
        }
        let _ = reply.send(result);
    }

    fn begin_fence(
        &mut self,
        volumes: Box<[PublishKey; VOLUME_LIMIT]>,
        count: usize,
        reply: Reply<Vec<FencedVolume>, Cause>,
        admission: FenceAdmission,
    ) {
        if self.fence.is_some() {
            drop(admission);
            let _ = reply.send(Err("publication fence is busy".into()));
            return;
        }
        let targets = (*volumes)
            .into_iter()
            .take(count)
            .map(|volume| {
                let state = self.volumes.get(&volume).ok_or_else(|| {
                    sync_failure::target(volume, "publication fence volume is not initialized")
                })?;
                if !matches!(state.phase, Phase::Ready { .. }) {
                    let cause = state.error.clone().unwrap_or_else(|| {
                        PublisherError::new(
                            "publication fence volume requires bootstrap or recovery",
                        )
                    });
                    return Err(if matches!(state.phase, Phase::Disconnected) || self.stopping {
                        cause.into()
                    } else {
                        cause.at_scope(volume)
                    });
                }
                Ok(FenceTarget {
                    scope: FencedVolume { volume, generation: state.generation },
                    accepted: 0,
                })
            })
            .collect::<Result<Vec<_>>>();
        match targets {
            Ok(targets) => {
                self.fence = Some(Fence {
                    targets,
                    remaining: FEED_LIMIT,
                    status: self.status,
                    reply,
                    _admission: admission,
                });
                self.feed_pending = true;
            }
            Err(error) => {
                drop(admission);
                let _ = reply.send(Err(error));
            }
        }
    }

    fn check_bootstrap(
        &self,
        token: &BootstrapToken,
        volume: PublishKey,
        status: FeedStatus,
    ) -> std::result::Result<(), Box<PublisherError>> {
        if volume != token.volume
            || status.epoch != token.epoch
            || status.closed.is_some()
            || status.gap_generation != token.gap
        {
            return Err(PublisherError::new(
                "committed feed changed during bootstrap; recovery required",
            )
            .into());
        }
        let state = self
            .volumes
            .get(&token.volume)
            .ok_or_else(|| PublisherError::new("publication volume was not bootstrapped"))?;
        if state.generation != token.generation
            || !matches!(state.phase, Phase::Bootstrapping { gap } if gap == token.gap)
        {
            return Err(PublisherError::new("stale publication bootstrap token").into());
        }
        Ok(())
    }

    fn check_fence(&mut self) {
        self.cancel_dropped_leases();
        if self.fence.is_some() {
            self.observe(self.store.change_status());
        }
        let Some(fence) = self.fence.as_ref() else {
            return;
        };
        let result = fence.check(self.status, |scope| {
            let state = self.volumes.get(&scope.volume).ok_or_else(|| {
                sync_failure::target(scope.volume, "publication fence volume was removed")
            })?;
            if state.generation != scope.generation || !matches!(state.phase, Phase::Ready { .. }) {
                let cause = state.error.clone().unwrap_or_else(|| {
                    PublisherError::new("publication fence generation or readiness changed")
                });
                return Err(if matches!(state.phase, Phase::Disconnected) || self.stopping {
                    cause.into()
                } else {
                    cause.at_scope(scope.volume)
                });
            }
            Ok(state.settled)
        });
        match result {
            Ok(false) => {}
            result => {
                if result.is_ok() {
                    crate::trace::mark("replication.fence_settled");
                }
                let Fence { targets, reply, _admission: admission, .. } =
                    self.fence.take().expect("fence readiness was checked before consuming it");
                let result =
                    result.map(|_| targets.into_iter().map(|target| target.scope).collect());
                drop(admission);
                let _ = reply.send(result);
            }
        }
    }
    fn retire_volumes(&mut self) {
        let ready: Vec<_> = self
            .volumes
            .iter()
            .filter_map(|(id, state)| {
                (state.active.is_none() && matches!(state.phase, Phase::Removing { .. }))
                    .then_some(*id)
            })
            .collect();
        for id in ready {
            let state =
                self.volumes.remove(&id).expect("retired ID was collected from the volume map");
            let mut notices = self.signals.notices.lock().expect("mutex is not poisoned");
            notices.volumes.retain(|slot| {
                PublishKey { volume: slot.notice.volume, entry: slot.notice.entry } != id
            });
            // Both the actor scope and its notice slot are free again, so the
            // capacity diagnostic no longer describes the current state.
            notices.capacity_exhausted = false;
            drop(notices);
            let Phase::Removing { reply } = state.phase else { unreachable!() };
            let _ = reply.send(Ok(()));
            self.capacity_reported = false;
        }
    }
    fn disconnect(&mut self) {
        self.fail_account(
            PublishState::Disconnected,
            PublisherError::new("AP account connection disconnected"),
        );
        for (id, state) in &mut self.volumes {
            // Losing the shared connection does not make a terminal local or
            // authorization failure eligible for automatic bootstrap.
            if matches!(state.phase, Phase::Failed | Phase::Excluded) {
                continue;
            }
            state.invalidate(
                Phase::Disconnected,
                PublisherError::new("AP connection disconnected; recovery required"),
            );
            self.signals.volume(state.notice(*id));
        }
    }
    fn fail(&mut self, error: impl fmt::Display) {
        let error = PublisherError::new(error);
        self.signals.error(&error);
        self.fail_account(PublishState::Failed, error.clone());
        for (id, state) in &mut self.volumes {
            state.invalidate(Phase::Failed, error.clone());
            self.signals.volume(state.notice(*id));
        }
        self.stopping = true;
        self.receive.close();
    }
    fn observe(&mut self, status: FeedStatus) {
        if status.closed.is_some() {
            self.status = status;
            self.fail("committed-change feed closed; reopen required");
            self.signals.notices.lock().expect("mutex is not poisoned").observed = Some(status);
            return;
        }
        if status.epoch != self.status.epoch || status.gap_generation != self.status.gap_generation
        {
            for (id, state) in &mut self.volumes {
                // Preparing has not established its snapshot boundary yet.
                if matches!(state.phase, Phase::Ready { gap, .. } | Phase::Bootstrapping { gap } if status.epoch != self.status.epoch || gap != status.gap_generation)
                {
                    state.invalidate(
                        Phase::NeedsRecovery,
                        PublisherError::new("committed-change feed gap; recovery required"),
                    );
                    self.signals.volume(state.notice(*id));
                }
            }
        }
        self.status = status;
        // Publish this stamp only after every affected scope is invalidated.
        self.signals.notices.lock().expect("mutex is not poisoned").observed = Some(status);
    }
    fn start_drains(&mut self) {
        for state in self.volumes.values_mut() {
            if state.active.is_none()
                && let Phase::Preparing { remaining, .. } = &mut state.phase
                && remaining.is_none()
            {
                state.queued.clear();
                state.accepted = 0;
                state.settled = 0;
                *remaining = Some(FEED_LIMIT);
                self.feed_pending = true;
            }
        }
    }
    fn accept_record(&mut self, record: CommittedChange) {
        let volume = record.volume();
        match self.store.replication_supported(volume) {
            Ok(false) => return,
            Ok(true) => {}
            Err(error) => {
                self.fail(error);
                return;
            }
        }
        // Admission and bounded queue insertion form one local step. A provider
        // selection change waits for this step before forgetting old scopes.
        let scope = self.scope.clone();
        let admission = scope.read();
        if !admission.admits(volume) {
            return;
        }
        let record = Arc::new(record);
        let selected: Vec<_> = self
            .volumes
            .keys()
            .copied()
            .filter(|key| key.volume == volume && key.entry.is_some())
            .collect();
        // Existing catalog publication is independent of selected-entry queues.
        // Do not invent catalog admission when only a selected scope is installed.
        if selected.is_empty() || self.volumes.contains_key(&volume.into()) {
            self.accept_scoped_record(volume.into(), record.clone());
        }
        for key in selected {
            let matches = match record.kind() {
                CommittedChangeKind::Database(crate::store::DatabaseId::Index) => true,
                CommittedChangeKind::Database(crate::store::DatabaseId::Entry(id)) => {
                    key.entry == Some(id)
                }
                _ => false,
            };
            if matches {
                self.accept_scoped_record(key, record.clone());
            }
        }
    }
    fn accept_scoped_record(&mut self, volume: PublishKey, record: Arc<CommittedChange>) {
        let capacity_reported = self.capacity_reported;
        match self.ensure_volume(volume) {
            Ok(state) => {
                let before = state.notice(volume).state;
                // Only snapshot-bound or publishable generations need replay.
                // Inactive records are already durable and a later recovery
                // snapshot covers them; retaining their credits starves peers.
                if record.kind() != CommittedChangeKind::VolumeAvailable
                    && matches!(state.phase, Phase::Ready { .. } | Phase::Bootstrapping { .. })
                {
                    if let Some(accepted) = state.accepted.checked_add(1) {
                        state.accepted = accepted;
                        assert!(
                            state.queued.len() < FEED_LIMIT,
                            "retained records own finite feed credits"
                        );
                        state.queued.push_back(record);
                    } else {
                        state.invalidate(
                            Phase::Failed,
                            PublisherError::new("publication FIFO count exhausted"),
                        );
                    }
                }
                let notice = state.notice(volume);
                if notice.state != before {
                    self.signals.volume(notice);
                }
            }
            Err(error) if !capacity_reported => {
                self.capacity_reported = true;
                self.signals.capacity_error(error);
            }
            Err(_) => {}
        }
    }
    fn drain_feed(&mut self) {
        let batch = self.feed.get_mut().0.drain();
        let count = batch.records.len();
        if batch.account_generation != self.account_generation {
            self.account_generation = batch.account_generation;
            self.account_wake();
        }
        self.feed_pending = count == 8;
        self.observe(batch.status);
        if self.stopping {
            return;
        }
        for record in batch.records {
            self.accept_record(record);
        }
        if let Some(fence) = self.fence.as_mut()
            && fence.remaining != 0
        {
            fence.remaining = if count == 0 { 0 } else { fence.remaining.saturating_sub(count) };
            if fence.remaining == 0 {
                crate::trace::mark("replication.fence_drained");
                for target in &mut fence.targets {
                    if let Some(state) = self.volumes.get(&target.scope.volume) {
                        target.accepted = state.accepted;
                    }
                }
            } else {
                self.feed_pending = true;
            }
        }
        // FIFO capacity is the bound, never a maximum allocated publication ID.
        for (id, state) in &mut self.volumes {
            let Phase::Preparing { remaining: Some(remaining), .. } = &mut state.phase else {
                continue;
            };
            *remaining = remaining.saturating_sub(count);
            if count != 0 && *remaining != 0 {
                self.feed_pending = true;
                continue;
            }
            let status = self.store.change_status();
            let Phase::Preparing { reply, .. } = std::mem::replace(
                &mut state.phase,
                Phase::Bootstrapping { gap: status.gap_generation },
            ) else {
                unreachable!()
            };
            let result = if status.closed.is_some() {
                Err(PublisherError::new("committed-change feed closed during bootstrap"))
            } else {
                let (send, receive) = oneshot::channel();
                state.cancellation = Some(receive);
                Ok(BootstrapToken {
                    lease: PublicationLease { send },
                    volume: *id,
                    generation: state.generation,
                    epoch: status.epoch,
                    gap: status.gap_generation,
                })
            };
            if reply.send(result).is_err() {
                state.invalidate(Phase::Failed, PublisherError::new("bootstrap caller cancelled"));
            }
            self.signals.volume(state.notice(*id));
        }
    }
    fn cancel_generation(&mut self, volume: PublishKey, generation: u64, released: bool) {
        if let Some(state) = self.volumes.get_mut(&volume)
            && state.generation == generation
            && state.cancellation.is_some()
        {
            let (phase, error) = lease_loss(released);
            state.invalidate(phase, error);
            self.signals.volume(state.notice(volume));
        }
    }
    fn cancel_dropped_leases(&mut self) {
        // At most 512 retained receivers; readiness wakes the actor, never a timer.
        for (volume, state) in &mut self.volumes {
            let cancelled =
                state.cancellation.as_mut().and_then(|receive| match receive.try_recv() {
                    Err(oneshot::error::TryRecvError::Empty) => None,
                    result => Some(result.is_ok()),
                });
            if let Some(released) = cancelled {
                let (phase, error) = lease_loss(released);
                state.invalidate(phase, error);
                self.signals.volume(state.notice(*volume));
            }
        }
    }
    fn start_jobs(&mut self) {
        self.cancel_dropped_leases();
        if self.stopping {
            return;
        }
        // One account job shares the existing four slots; the other three
        // remain available to volume work even under continuous account edits.
        assert!(self.jobs.len() <= JOB_LIMIT, "publication admission keeps the job limit");
        self.start_account_job();
        // At most 512 candidates; round-robin prevents busy low-ID volumes winning
        // every newly available slot while another initialized volume waits.
        let keys: Vec<_> = self.volumes.keys().copied().collect();
        let start = self.cursor.map_or(0, |cursor| keys.partition_point(|id| *id <= cursor));
        for volume in keys[start..].iter().chain(&keys[..start]).copied() {
            if self.jobs.len() == JOB_LIMIT {
                break;
            }
            let state = self
                .volumes
                .get_mut(&volume)
                .expect("publication volume was initialized or checked above");
            if state.active.is_some() {
                continue;
            }
            let Phase::Ready { scope, connection, .. } = &state.phase else {
                continue;
            };
            let Some(records) = take_ready_publication(&mut state.queued) else {
                continue;
            };
            let (cancel, cancelled) = oneshot::channel();
            let generation = state.generation;
            let task = self.jobs.spawn(publish(
                self.store.clone(),
                self.blob_transfers.clone(),
                self.apply_slots.clone(),
                scope.clone(),
                connection.clone(),
                records,
                generation,
                cancelled,
            ));
            state.active = Some(Active { id: task.id(), cancel: Some(cancel) });
            self.cursor = Some(volume);
            self.signals.volume(state.notice(volume));
        }
    }
    fn completed(
        &mut self,
        result: std::result::Result<(TaskId, JobResult), tokio::task::JoinError>,
    ) {
        assert!(self.jobs.len() < JOB_LIMIT, "completion removes one admitted job");
        let (task, result) = match result {
            Ok(value) => value,
            Err(error) => {
                if self.account.as_ref().is_some_and(|account| {
                    account.active.as_ref().is_some_and(|job| job.id == error.id())
                }) {
                    self.account
                        .as_mut()
                        .expect("the completed account job belongs to an initialized account")
                        .active = None;
                    self.fail_account(PublishState::Failed, PublisherError::new(error));
                    return;
                }
                if let Some((id, state)) = self
                    .volumes
                    .iter_mut()
                    .find(|(_, s)| s.active.as_ref().is_some_and(|a| a.id == error.id()))
                {
                    state.active = None;
                    state.invalidate(Phase::Failed, PublisherError::new(error));
                    self.signals.volume(state.notice(*id));
                }
                return;
            }
        };
        let result = match result {
            JobResult::Volume(result) => result,
            JobResult::Account { generation, result } => {
                self.completed_account(task, generation, result);
                return;
            }
        };
        let Some(state) = self.volumes.get_mut(&result.volume) else {
            return;
        };
        if state.active.as_ref().is_none_or(|active| active.id != task) {
            return;
        }
        state.active = None;
        let volume = result.volume;
        let mut diagnostic = None;
        if state.generation == result.generation && matches!(state.phase, Phase::Ready { .. }) {
            diagnostic = state.settle(result);
        }
        let mut notice = state.notice(volume);
        notice.error = notice.error.or(diagnostic);
        self.signals.volume(notice);
    }
}

async fn cancellable<T>(
    future: impl Future<Output = T>,
    cancelled: &mut oneshot::Receiver<()>,
) -> std::result::Result<T, Box<PublisherError>> {
    let mut future = pin!(future);
    poll_fn(|cx| {
        if std::pin::Pin::new(&mut *cancelled).poll(cx).is_ready() {
            return Poll::Ready(Err(PublisherError::new("publication cancelled").into()));
        }
        future.as_mut().poll(cx).map(Ok)
    })
    .await
}
// One bounded ready prefix, retaining the original feed allocation credits.
fn take_ready_publication(
    queue: &mut VecDeque<Arc<CommittedChange>>,
) -> Option<Vec<Arc<CommittedChange>>> {
    assert!(queue.len() <= FEED_LIMIT, "publication drain retains the feed bound");
    let record = queue.pop_front()?;
    let volume = record.volume();
    let database = record.kind();
    let application =
        matches!(database, CommittedChangeKind::Database(crate::store::DatabaseId::Entry(_)));
    let mut bytes = record.bytes().len();
    let capacity = if application { (queue.len() + 1).min(BATCH_RECORD_LIMIT) } else { 1 };
    let mut records = Vec::with_capacity(capacity);
    records.push(record);
    // No batching delay. Index mutations remain ordering boundaries.
    if application {
        while records.len() < BATCH_RECORD_LIMIT
            && queue.front().is_some_and(|next| {
                next.volume() == volume
                    && next.kind() == database
                    && bytes.saturating_add(next.bytes().len()) <= BATCH_BYTE_LIMIT
            })
        {
            let next = queue.pop_front().expect("queue was checked before taking its front item");
            bytes += next.bytes().len();
            records.push(next);
        }
    }
    Some(records)
}

#[allow(
    clippy::too_many_arguments,
    reason = "One bounded job captures its complete publication context."
)]
async fn publish(
    store: Arc<LocalStore>,
    blob_transfers: Arc<BlobTransfers>,
    slots: Arc<Semaphore>,
    scope: PublishScope,
    connection: Arc<ApConnection>,
    records: Vec<Arc<CommittedChange>>,
    generation: u64,
    mut cancelled: oneshot::Receiver<()>,
) -> JobResult {
    let _timing = crate::trace::Interval::new("replication.publish_batch");
    let last = records.last().expect("publication batches contain at least one record");
    let publication = last.sequence();
    let kind = last.kind();
    let count = records.len() as u64;
    let mut diagnostic = None;
    let result = publish_records(
        store,
        blob_transfers,
        slots,
        &scope,
        &connection,
        records,
        &mut cancelled,
        &mut diagnostic,
    )
    .await
    .map_err(|mut error| {
        if let JobError::Failed(cause) = &mut error
            && let CommittedChangeKind::Database(database) = kind
        {
            cause.database = Some(database);
        }
        error
    });
    JobResult::Volume(VolumeJobResult {
        volume: scope.key(),
        generation,
        publication,
        records: count,
        result,
        diagnostic: diagnostic.map(Box::new),
    })
}

#[allow(
    clippy::result_large_err,
    reason = "Bounded inline diagnostics avoid allocation in publication jobs."
)]
#[allow(
    clippy::too_many_arguments,
    reason = "One bounded job captures its complete publication context."
)]
async fn publish_records(
    store: Arc<LocalStore>,
    blob_transfers: Arc<BlobTransfers>,
    slots: Arc<Semaphore>,
    scope: &PublishScope,
    connection: &Arc<ApConnection>,
    records: Vec<Arc<CommittedChange>>,
    cancelled: &mut oneshot::Receiver<()>,
    diagnostic: &mut Option<PublisherError>,
) -> std::result::Result<Option<PublicationAck>, JobError> {
    let volume = records[0].volume();
    let kind = records[0].kind();
    let (records, projected, access) =
        prepare_publication(&store, &slots, scope, records, cancelled).await?;
    let Some(ProjectedPublication { bytes: projected, blobs, acknowledge_appends }) = projected
    else {
        // The current service policy excludes this complete creation.
        // No network operation and no fabricated durable acknowledgment.
        return Ok(None);
    };
    let ack = publish_delta(connection, scope, kind, &projected, cancelled).await?;
    if let CommittedChangeKind::Database(database) = kind
        && (database == crate::store::DatabaseId::Index || acknowledge_appends)
    {
        let permit = cancellable(slots.clone().acquire_owned(), cancelled)
            .await?
            .map_err(PublisherError::new)?;
        let receipt_store = store.clone();
        let receipt = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if database == crate::store::DatabaseId::Index {
                receipt_store.acknowledge_provider_delta(volume, &projected)
            } else {
                receipt_store.acknowledge_world_kv_appends(volume, database, &projected)
            }
        })
        .await
        .map_err(PublisherError::new)?;
        if let Err(error) = receipt {
            *diagnostic = Some(PublisherError::new(error));
        }
    }
    if !blobs.is_empty() {
        let transport = BlobTransport::new(
            Handle::current(),
            Arc::new(Mutex::new(Some(connection.clone()))),
            blob_transfers,
            #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
            None,
        );
        upload_blobs(store, transport, access, scope, records, blobs, cancelled).await?;
        return Ok(Some(ack));
    }
    drop(records);
    Ok(Some(ack))
}

#[allow(clippy::result_large_err, reason = "Publication errors have bounded inline storage.")]
async fn publish_delta(
    connection: &ApConnection,
    scope: &PublishScope,
    kind: CommittedChangeKind,
    bytes: &[u8],
    cancelled: &mut oneshot::Receiver<()>,
) -> std::result::Result<PublicationAck, JobError> {
    let request = scope.request(kind).map_err(PublisherError::new)?;
    cancellable(
        async {
            match kind {
                CommittedChangeKind::VolumeAvailable => {
                    unreachable!("local discovery never enters a publication job")
                }
                CommittedChangeKind::Database(_) => {
                    service_client::publish_replica(connection, &request, bytes).await
                }
            }
        },
        cancelled,
    )
    .await?
    .map_err(|error| ap_failure(error.as_ref()))
}

struct ProjectedPublication {
    acknowledge_appends: bool,
    bytes: Vec<u8>,
    blobs: Vec<crate::store::volume_index::BlobVersion>,
}

#[allow(clippy::result_large_err, reason = "Publication errors have bounded inline storage.")]
async fn upload_blobs(
    store: Arc<LocalStore>,
    transport: BlobTransport,
    access: crate::store::Access,
    scope: &PublishScope,
    records: Vec<Arc<CommittedChange>>,
    blobs: Vec<crate::store::volume_index::BlobVersion>,
    cancelled: &mut oneshot::Receiver<()>,
) -> std::result::Result<(), JobError> {
    let volume = scope.volume;
    let session = scope.session.map(|(id, _)| id);
    // Reserve before spawning: canceled generations cannot accumulate blocked
    // OS workers while an earlier upload still owns the single upload slot.
    let credit =
        cancellable(transport.upload_credit(), cancelled).await?.map_err(PublisherError::new)?;

    let work = tokio::task::spawn_blocking(move || {
        // The feed credits outlive cancellation until blocking transfer work
        // actually ends. The transport remains bound to this AP connection.
        let _records = records;
        let session = session.as_ref().map(|id| crate::identifier::encode(id));
        transport.upload_pending_admitted(
            &store,
            &access,
            session.as_deref(),
            volume,
            blobs,
            credit,
        )
    });
    cancellable(work, cancelled)
        .await?
        .map_err(PublisherError::new)?
        .map_err(|error| ap_failure(error.as_ref()))
}

#[allow(
    clippy::result_large_err,
    reason = "Bounded inline diagnostics avoid allocation in publication jobs."
)]
async fn prepare_publication(
    store: &Arc<LocalStore>,
    slots: &Arc<Semaphore>,
    scope: &PublishScope,
    records: Vec<Arc<CommittedChange>>,
    cancelled: &mut oneshot::Receiver<()>,
) -> std::result::Result<
    (Vec<Arc<CommittedChange>>, Option<ProjectedPublication>, crate::store::Access),
    JobError,
> {
    let volume = records[0].volume();
    let kind = records[0].kind();
    let permit = cancellable(slots.clone().acquire_owned(), cancelled)
        .await?
        .map_err(PublisherError::new)?;
    // This closure owns both the feed lease and permit. Cancellation never
    // releases their credits while a started blocking operation still runs.
    let work_store = store.clone();
    let access = match &scope.session {
        Some((id, build)) => crate::store::Access::session_version(
            store.account().id,
            crate::identifier::encode(id),
            crate::identifier::encode(build),
        ),
        None => crate::store::Access::owner(store.account().id),
    };
    let work_access = access.clone();
    let selected = scope.entry;
    let route = scope.route;

    let (records, projected) = tokio::task::spawn_blocking(move || {
        let store = work_store;
        let projected =
            project_publication(&store, &work_access, volume, kind, &records, selected, route);
        drop(permit);
        (records, projected)
    })
    .await
    .map_err(PublisherError::new)?;
    Ok((records, projected.map_err(PublisherError::new)?, access))
}

fn project_publication(
    store: &LocalStore,
    access: &crate::store::Access,
    volume: Id,
    kind: CommittedChangeKind,
    records: &[Arc<CommittedChange>],
    selected: Option<crate::store::EntryId>,
    route: WorldReplicaRoute,
) -> crate::store::Result<Option<ProjectedPublication>> {
    let mut combined = None;
    if records.len() > 1 {
        let _timing = crate::trace::Span::new("replication.combine_deltas");
        for record in &records[1..] {
            combined = Some(kvdb::sync::combine_deltas(
                combined.as_deref().unwrap_or(records[0].bytes()),
                record.bytes(),
                BATCH_BYTE_LIMIT,
            )?);
        }
    }
    let bytes = combined.as_deref().unwrap_or(records[0].bytes());
    let filtered;
    let bytes = if let Some(entry) = selected
        && kind == CommittedChangeKind::Database(crate::store::DatabaseId::Index)
    {
        let mut retained = false;
        filtered =
            kvdb::sync::project_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key| {
                let include = crate::store::volume_index::source_entry_id(key).ok() == Some(entry);
                retained |= include;
                include
            })?;
        if !retained {
            return Ok(None);
        }
        &filtered
    } else {
        bytes
    };
    match kind {
        CommittedChangeKind::VolumeAvailable => {
            Err(crate::store::Error::InvalidInput("local discovery is not a publication"))
        }
        CommittedChangeKind::Database(database) => {
            let Some(mut bytes) = store.publication_delta(access, volume, database, bytes)? else {
                return Ok(None);
            };
            let blobs = if database == crate::store::DatabaseId::Index {
                crate::store::volume_index::published_blob_versions(volume, &bytes)?
            } else {
                Vec::new()
            };
            if route == WorldReplicaRoute::CredentialedRoot {
                super::replication::rekey_credentialed_root_delta(
                    store,
                    volume,
                    database,
                    SyncDirection::Push,
                    &mut bytes,
                )?;
            }
            let acknowledge_appends = matches!(database, crate::store::DatabaseId::Entry(_))
                && store.has_world_attachment(volume)?;
            Ok(Some(ProjectedPublication { bytes, blobs, acknowledge_appends }))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::store::change_feed::CommittedChangeFeed;
    use kvdb::sync::CommittedDelta;
    use std::io::Read;

    fn test_id(value: usize) -> Id {
        let mut bytes = [value as u8; 16];
        if value > 255 {
            bytes[0] = 0;
            bytes[14..].copy_from_slice(&(value as u16).to_le_bytes());
        }
        Id::from_bytes(bytes)
    }
    fn id(byte: usize) -> Id {
        test_id(byte)
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    fn inactive_actor() -> AbortHandle {
        match Handle::try_current() {
            Ok(handle) => handle.spawn(async {}).abort_handle(),
            Err(_) => runtime().spawn(async {}).abort_handle(),
        }
    }
    fn test_reply<T, E>() -> (Reply<T, E>, oneshot::Receiver<std::result::Result<T, E>>) {
        let (send, receive) = oneshot::channel();
        (
            Reply {
                send,
                _permit: Arc::new(Semaphore::new(COMMAND_LIMIT)).try_acquire_owned().unwrap(),
            },
            receive,
        )
    }
    fn actor() -> (tempfile::TempDir, CommittedChangeFeed, Actor, UnixStream) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let status = store.change_status();
        let source = CommittedChangeFeed::new(status.epoch).unwrap();
        // SAFETY: The receiver owns its socket and never replaces or closes it
        // while registered; AsyncFd deregisters before dropping the receiver.
        #[allow(unsafe_code, reason = "The feed owns its stable readiness socket.")]
        let feed = unsafe { AsyncFd::register(FeedFd(source.take_receiver().unwrap())) }.unwrap();
        let (completion, read) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        read.set_nonblocking(true).unwrap();
        let signals = Arc::new(Signals {
            notices: Mutex::new(Notices::default()),
            completion: Arc::new(Wake::unix(completion)),
        });
        let (_commands, receive) = mpsc::channel(COMMAND_LIMIT);
        let actor = Actor {
            store,
            scope: Default::default(),
            blob_transfers: Arc::new(BlobTransfers::default()),
            feed,
            feed_pending: true,
            receive,
            signals,
            apply_slots: Arc::new(Semaphore::new(2)),
            volumes: BTreeMap::new(),
            account: None,
            account_generation: 0,
            jobs: JoinSet::new(),
            status,
            gauges: PublisherGauges::new(),
            cursor: None,
            event_cursor: 0,
            stopping: false,
            capacity_reported: false,
            generation: 0,
            fence: None,
        };
        (directory, source, actor, read)
    }
    #[test]
    fn local_only_changes_release_credits_while_shared_enters_publication() {
        // Deferred scopes stay local while creator Shared enters the existing
        // discovery, account-dependency and durable live-publication paths.
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let package = crate::package::Package::new(
                "test".into(),
                "Test".into(),
                b"\0asm\x01\0\0\0".to_vec(),
                BTreeMap::new(),
            )
            .unwrap();
            let package =
                crate::package::signature::testing::signed(&actor.store, package).unwrap();
            let app = package.identity().unwrap().clone();
            let path = actor.store.profile_path().join(crate::paths::BUILDS);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(package.id().unwrap()), package.to_bytes().unwrap()).unwrap();
            actor.store.remember_imported_app(&package).unwrap();
            let local = actor
                .store
                .open_app_volume(&app, crate::package::AutomaticVolumeRole::Local)
                .unwrap();
            source.publish(
                local.id,
                crate::store::DatabaseId::Index,
                CommittedDelta::Bytes(vec![1]),
            );
            actor.drain_feed();
            assert!(actor.volumes.is_empty());
            assert_eq!(source.pending_count(), 0);
            assert!(actor.signals.notices.lock().unwrap().volumes.is_empty());
            let (reply, mut result) = test_reply();
            actor.begin_bootstrap(local.id.into(), reply);
            assert!(result.try_recv().unwrap().is_err());
            assert!(actor.volumes.is_empty());
            let (reply, mut result) = test_reply();
            actor.command(Command::Account(account::Command::ReadyVolume {
                volume: local.id,
                reply,
            }));
            result.try_recv().unwrap().unwrap();
            assert!(actor.volumes.is_empty());
            let ordinary = actor.store.create_volume("Ordinary", true).unwrap();
            source.publish(
                ordinary.id,
                crate::store::DatabaseId::Index,
                CommittedDelta::Bytes(vec![2]),
            );
            actor.drain_feed();
            assert!(actor.volumes.contains_key(&ordinary.id.into()));
            assert_eq!(source.pending_count(), 0);

            // From here the actor consumes the store's actual committed feed.
            let mut receiver = actor.store.take_change_receiver().unwrap();
            while !receiver.drain().records.is_empty() {}
            // SAFETY: FeedFd owns the stable socket; AsyncFd deregisters before
            // dropping this receiver, matching the ordinary publisher startup.
            #[allow(unsafe_code, reason = "The feed owns its stable readiness socket.")]
            {
                actor.feed = unsafe { AsyncFd::register(FeedFd(receiver)) }.unwrap();
            }
            actor
                .store
                .save_recovery_state(&crate::store::RecoveryState {
                    secret: [7; 16],
                    acknowledged: true,
                    revision: 1,
                    synchronized_digest: None,
                    pending: None,
                })
                .unwrap();
            let build = package.id().unwrap();
            let session = actor
                .store
                .create_session_binding(crate::store::SessionCreation {
                    root_volume: app
                        .volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)
                        .unwrap(),
                    app,
                    selector: crate::store::BuildSelector::Exact { build_id: build.clone() },
                    resolved_build_id: build,
                    sync_shared: true,
                    user_volume: None,
                })
                .unwrap();
            let shared = actor
                .store
                .open_automatic_volume(&session.id, crate::package::AutomaticVolumeRole::Session)
                .unwrap();
            // Initialization emits only the late, local VolumeAvailable notice;
            // that notification must discover Shared without an unrelated edit.
            assert_eq!(actor.store.pending_change_count(), 1);
            actor.drain_feed();
            assert!(matches!(actor.volumes[&shared.id.into()].phase, Phase::NeedsBootstrap));
            assert_eq!(actor.store.pending_change_count(), 0);
            let invitation = actor.store.pending_account_record(None).unwrap().unwrap();
            let dependencies = actor.store.account_record_dependencies(&invitation).unwrap();
            assert!(dependencies.into_iter().flatten().all(|volume| volume == shared.id));
            assert!(dependencies.into_iter().flatten().next().is_some());
            let (publisher, _commands) = health_publisher(&actor);
            assert!(publisher.account_ready_volumes().is_empty());
            let (reply, mut result) = test_reply();
            actor.command(Command::Account(account::Command::ReadyVolume {
                volume: shared.id,
                reply,
            }));
            result.try_recv().unwrap().unwrap();
            assert_eq!(publisher.account_ready_volumes(), [shared.id]);
            assert!(
                dependencies
                    .into_iter()
                    .flatten()
                    .all(|volume| publisher.account_ready_volumes().contains(&volume))
            );

            let owner = crate::store::Access::owner(actor.store.account().id);
            let database = actor
                .store
                .create_database(
                    &owner,
                    shared.id,
                    "live-db",
                    crate::store::volume_index::EntryKind::KvDb {},
                )
                .unwrap();
            actor.drain_feed();
            assert!(actor.volumes[&shared.id.into()].queued.is_empty());
            retain_for_replay(&mut actor, &[shared.id]);
            actor
                .store
                .kv_put(
                    &owner,
                    crate::store::volume_index::EntryAddress {
                        volume: shared.id,
                        entry: database.id,
                    },
                    b"edit",
                    b"Shared database edit",
                )
                .unwrap();
            actor.drain_feed();
            let queued = &actor.volumes[&shared.id.into()].queued;
            assert_eq!(queued.len(), 1);
            assert_eq!(
                queued[0].kind(),
                CommittedChangeKind::Database(crate::store::DatabaseId::Entry(database.id))
            );
            assert!(queued.iter().all(|record| !record.bytes().is_empty()));
            assert_eq!(actor.store.pending_change_count(), 1);
            actor.volumes.get_mut(&shared.id.into()).unwrap().queued.clear();
            assert_eq!(actor.store.pending_change_count(), 0);
        });
    }

    #[test]
    fn index_publication_captures_only_its_own_file_versions() {
        // Queue several revisions before projecting any of them. Each upload
        // must follow its own declaration, even when newer bytes are pending.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let access = crate::store::Access::owner(store.account().id);
        let volume = store.create_volume("File declarations", true).unwrap().id;
        let mut receiver = store.take_change_receiver().unwrap();
        while !receiver.drain().records.is_empty() {}
        let file = store.create_blob(&access, volume, "note.txt", b"first").unwrap();
        let address = crate::store::volume_index::EntryAddress { volume, entry: file.id };
        let mut expected = Vec::new();
        let mut batches = Vec::new();
        for body in [None, Some(&b"second"[..]), Some(&b"third"[..])] {
            if let Some(body) = body {
                store.write_blob(&access, volume, file.id, body).unwrap();
            }
            expected.push(store.blob_version(&access, address, None).unwrap());
            batches.push(receiver.drain().records.into_iter().map(Arc::new).collect::<Vec<_>>());
        }
        for (records, version) in batches.iter().zip(expected) {
            let projected = project_publication(
                &store,
                &access,
                volume,
                CommittedChangeKind::Database(crate::store::DatabaseId::Index),
                records,
                None,
                WorldReplicaRoute::Ordinary,
            )
            .unwrap()
            .unwrap();
            assert_eq!(projected.blobs, [version], "birth/head/history duplicates upload once");
        }
    }

    fn publish(feed: &CommittedChangeFeed, volume: Id, byte: u8) {
        feed.publish(
            volume,
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            CommittedDelta::Bytes(vec![byte]),
        );
    }

    #[test]
    fn selected_bootstrap_preserves_sibling_queues_and_shared_feed_credits() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let volume = id(2);
            let first = crate::store::EntryId::from_bytes([3; 15]).unwrap();
            let second = crate::store::EntryId::from_bytes([4; 15]).unwrap();
            let first_key = PublishKey { volume, entry: Some(first) };
            let second_key = PublishKey { volume, entry: Some(second) };
            for key in [volume.into(), first_key, second_key] {
                actor.ensure_volume(key).unwrap().phase = Phase::Bootstrapping { gap: 0 };
            }
            for database in [
                crate::store::DatabaseId::Index,
                crate::store::DatabaseId::Entry(first),
                crate::store::DatabaseId::Entry(second),
            ] {
                source.publish(volume, database, CommittedDelta::Bytes(vec![1]));
            }
            actor.drain_feed();
            assert_eq!(source.pending_count(), 3, "fanout shares payload credits");
            assert_eq!(actor.volumes[&first_key].queued.len(), 2);
            assert_eq!(actor.volumes[&second_key].queued.len(), 2);
            let (reply, _receive) = test_reply();
            actor.begin_bootstrap(first_key, reply);
            actor.start_drains();
            assert!(actor.volumes[&first_key].queued.is_empty());
            assert_eq!(actor.volumes[&second_key].queued.len(), 2);
            assert_eq!(actor.volumes[&volume.into()].queued.len(), 3);
            actor.volumes.get_mut(&volume.into()).unwrap().queued.clear();
            assert_eq!(source.pending_count(), 2);
            actor.volumes.get_mut(&second_key).unwrap().queued.clear();
            assert_eq!(source.pending_count(), 0);
        });
    }

    #[test]
    fn selected_publication_filters_siblings_without_changing_source_clocks() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let owner = crate::store::Access::owner(store.account().id);
        let volume = store.create_volume("Selected publication", true).unwrap();
        let first = store
            .create_database(
                &owner,
                volume.id,
                "one",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let second = store
            .create_database(
                &owner,
                volume.id,
                "two",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let mut receiver = store.take_change_receiver().unwrap();
        while !receiver.drain().records.is_empty() {}
        store.rename_entry(&owner, volume.id, first.id, "first").unwrap();
        store.rename_entry(&owner, volume.id, second.id, "second").unwrap();
        let records: Vec<_> = receiver.drain().records.into_iter().map(Arc::new).collect();
        assert_eq!(records.len(), 2);
        let kind = CommittedChangeKind::Database(crate::store::DatabaseId::Index);
        let ProjectedPublication { bytes: projected, .. } = project_publication(
            &store,
            &owner,
            volume.id,
            kind,
            &records,
            Some(first.id),
            WorldReplicaRoute::Ordinary,
        )
        .unwrap()
        .unwrap();
        let source_entries = |bytes: &[u8]| {
            let mut entries = Vec::new();
            kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
                if let kvdb::sync::EntState::Present { clock, value } = state {
                    entries.push((key.to_vec(), clock, value.to_vec()));
                } else {
                    panic!("rename retains present source facts");
                }
            })
            .unwrap();
            entries
        };
        assert_eq!(
            source_entries(&projected),
            source_entries(records[0].bytes()),
            "projection retains originating clocks and canonical bytes"
        );
        assert!(
            project_publication(
                &store,
                &owner,
                volume.id,
                kind,
                &records[1..],
                Some(first.id),
                WorldReplicaRoute::Ordinary
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn ready_publications_preserve_boundaries_limits_and_feed_credits() {
        let source = CommittedChangeFeed::new(id(1)).unwrap();
        let mut receiver = source.take_receiver().unwrap();
        let mut queue = VecDeque::new();
        for _ in 0..33 {
            source.publish(
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                CommittedDelta::Bytes(vec![1]),
            );
        }
        while queue.len() < 33 {
            queue.extend(receiver.drain().records.into_iter().map(Arc::new));
        }
        let batch = take_ready_publication(&mut queue).unwrap();
        assert_eq!(batch.len(), BATCH_RECORD_LIMIT);
        assert_eq!(source.pending_count(), 33, "in-flight originals retain all credits");
        drop(batch);
        assert_eq!(source.pending_count(), 1);
        queue.clear();
        assert!(take_ready_publication(&mut queue).is_none());
        for (volume, database, size) in [
            (
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                20_000,
            ),
            (
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                20_000,
            ),
            (id(2), crate::store::DatabaseId::Index, 1),
            (
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                1,
            ),
            (
                id(3),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                1,
            ),
        ] {
            source.publish(volume, database, CommittedDelta::Bytes(vec![1; size]));
        }
        queue.extend(receiver.drain().records.into_iter().map(Arc::new));
        for expected in [
            (
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                20_000,
            ),
            (
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                20_000,
            ),
            (id(2), crate::store::DatabaseId::Index, 1),
            (
                id(2),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                1,
            ),
            (
                id(3),
                crate::store::DatabaseId::Entry(
                    crate::store::EntryId::from_bytes([3; 15]).unwrap(),
                ),
                1,
            ),
        ] {
            let batch = take_ready_publication(&mut queue).unwrap();
            assert_eq!(batch.len(), 1);
            assert_eq!(
                (batch[0].volume(), batch[0].kind(), batch[0].bytes().len()),
                (expected.0, CommittedChangeKind::Database(expected.1), expected.2)
            );
        }
        assert_eq!(source.pending_count(), 0);
        source.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            CommittedDelta::Bytes(vec![1]),
        );
        source.publish(id(2), crate::store::DatabaseId::Index, CommittedDelta::Bytes(vec![2, 3]));
        source.publish(
            id(2),
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([3; 15]).unwrap()),
            CommittedDelta::Bytes(vec![4]),
        );
        queue.extend(receiver.drain().records.into_iter().map(Arc::new));
        for kind in [
            CommittedChangeKind::Database(crate::store::DatabaseId::Entry(
                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
            )),
            CommittedChangeKind::Database(crate::store::DatabaseId::Index),
            CommittedChangeKind::Database(crate::store::DatabaseId::Entry(
                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
            )),
        ] {
            let batch = take_ready_publication(&mut queue).unwrap();
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].kind(), kind);
        }
        assert_eq!(source.pending_count(), 0);
        // Combining, selected-index filtering, and policy projection may each
        // retain a buffer. Account conservatively at the full delta ceiling;
        // original payload bytes remain covered by the shared feed credits.
        let batch_scratch = JOB_LIMIT
            * (3 * crate::store::MAX_REPLICA_DELTA_BYTES
                + BATCH_RECORD_LIMIT * std::mem::size_of::<Arc<CommittedChange>>());
        assert!(batch_scratch < 800 * 1024);
    }

    fn retain_for_replay(actor: &mut Actor, volumes: &[Id]) {
        for volume in volumes {
            actor.ensure_volume((*volume).into()).unwrap().phase =
                Phase::Bootstrapping { gap: actor.status.gap_generation };
        }
    }

    fn fence_command(volumes: &[Id], reply: Reply<Vec<FencedVolume>, Cause>) -> Command {
        let mut fixed = Box::new([PublishKey::from(Id::from_bytes([0; 16])); VOLUME_LIMIT]);
        for (slot, volume) in fixed.iter_mut().zip(volumes) {
            *slot = (*volume).into();
        }
        Command::Fence {
            volumes: fixed,
            count: volumes.len(),
            reply,
            admission: FenceAdmission(Arc::new(AtomicBool::new(true))),
        }
    }
    fn fence_fixture(
        actor: &Actor,
        volumes: &[Id],
    ) -> (Fence, oneshot::Receiver<Result<Vec<FencedVolume>>>) {
        let (reply, receive) = test_reply();
        (
            Fence {
                targets: volumes
                    .iter()
                    .map(|volume| FenceTarget {
                        scope: FencedVolume {
                            volume: (*volume).into(),
                            generation: actor.volumes[&(*volume).into()].generation,
                        },
                        accepted: 0,
                    })
                    .collect(),
                remaining: FEED_LIMIT,
                status: actor.status,
                reply,
                _admission: FenceAdmission(Arc::new(AtomicBool::new(true))),
            },
            receive,
        )
    }

    #[test]
    fn publication_errors_do_not_retry_rejections_timeouts_or_local_failures() {
        let malformed = serde_json::from_str::<PublicationAck>("{invalid").unwrap_err();
        let errors: Vec<Box<dyn std::error::Error + Send + Sync>> = vec![
            Box::new(ClientError::ResponseTimeout),
            Box::new(ClientError::AuthenticationRequired),
            Box::new(ClientError::AccountDisabled),
            Box::new(ClientError::Serialization(malformed)),
            Box::new(ClientError::Io(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "unclassified I/O is not a typed QUIC transport loss",
            ))),
            "session participant revoked".into(),
            // Strings are untrusted diagnostics, never a classification API.
            "connection reset; timed out; recovery required".into(),
        ];
        for error in errors {
            let (phase, retained) = ap_failure(error.as_ref()).phase();
            assert!(matches!(phase, Some(Phase::Failed)));
            assert!(!retained.as_str().is_empty());
        }
        let (phase, _) = JobError::from(PublisherError::new("local projection failed")).phase();
        assert!(matches!(phase, Some(Phase::Failed)));
        // A discriminator lives only in the four executing job results, not
        // every notice, command reply, or retained per-volume error buffer.
        assert!(std::mem::size_of::<JobError>() <= std::mem::size_of::<PublisherError>() + 8);
    }

    #[test]
    fn failed_publication_task_releases_replay_without_requesting_gap_recovery() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(2), id(3)]);
            publish(&source, id(2), 1);
            publish(&source, id(3), 2);
            actor.drain_feed();
            let task = actor.jobs.spawn(async { panic!("injected publication task failure") });
            actor.volumes.get_mut(&(id(2)).into()).unwrap().active =
                Some(Active { id: task.id(), cancel: None });
            let result = actor.jobs.join_next_with_id().await.unwrap();
            assert!(result.is_err());
            actor.completed(result);
            let failed = &actor.volumes[&(id(2)).into()];
            assert!(matches!(failed.phase, Phase::Failed));
            assert!(failed.queued.is_empty());
            assert!(failed.active.is_none());
            assert_eq!(source.pending_count(), 1);
            // A later feed gap must not turn a terminal local failure into an
            // automatic bootstrap request; the other live generation does recover.
            actor.observe(FeedStatus {
                gap_generation: actor.status.gap_generation + 1,
                ..actor.status
            });
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Failed));
            assert!(matches!(actor.volumes[&(id(3)).into()].phase, Phase::NeedsRecovery));
            assert_eq!(source.pending_count(), 0);
        });
    }

    #[test]
    fn fence_counts_fifo_entries_not_cross_volume_publication_ids() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(2), id(3)]);
            publish(&source, id(2), 1);
            publish(&source, id(3), 2);
            let mut records = actor.feed.get_mut().0.drain().records;
            // Model different producers enqueueing after allocating their IDs:
            // identity 2 arrives before identity 1. Neither is a global watermark.
            let later = records.pop().unwrap();
            let earlier = records.pop().unwrap();
            assert!(later.sequence() > earlier.sequence());
            actor.accept_record(later);
            actor.accept_record(earlier);
            let (fence, _result) = fence_fixture(&actor, &[id(2), id(3)]);
            actor.fence = Some(fence);
            actor.drain_feed(); // Empty FIFO seals the two independent counters.
            let fence = actor.fence.as_ref().unwrap();
            assert_eq!(fence.targets.iter().map(|t| t.accepted).collect::<Vec<_>>(), [1, 1]);
            assert!(
                !fence
                    .check(actor.status, |scope| Ok(u64::from(scope.volume == (id(3)).into())))
                    .unwrap()
            );
            assert!(fence.check(actor.status, |_| Ok(1)).unwrap());
        });
    }

    #[test]
    fn fence_seals_bounded_prefix_and_later_edits_do_not_extend_it() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(2)]);
            for _ in 0..FEED_LIMIT {
                publish(&source, id(2), 1);
            }
            let (fence, _result) = fence_fixture(&actor, &[id(2)]);
            actor.fence = Some(fence);
            for turn in 0..FEED_LIMIT / 8 {
                actor.drain_feed();
                assert_eq!(actor.fence.as_ref().unwrap().remaining, FEED_LIMIT - (turn + 1) * 8);
                // Release the actual records as acknowledged work would, keeping
                // feed credit available to continuously arriving later edits.
                actor.volumes.get_mut(&(id(2)).into()).unwrap().queued.clear();
                for _ in 0..8 {
                    publish(&source, id(2), 2);
                }
            }
            let target = actor.fence.as_ref().unwrap().targets[0].accepted;
            assert_eq!(target, FEED_LIMIT as u64);
            actor.drain_feed();
            assert!(actor.volumes[&(id(2)).into()].accepted > target);
            let fence = actor.fence.as_ref().unwrap();
            assert_eq!(fence.targets[0].accepted, target);
            assert!(!fence.check(actor.status, |_| Ok(target - 1)).unwrap());
            assert!(fence.check(actor.status, |_| Ok(target)).unwrap());
        });
    }

    #[test]
    fn fence_rejects_gap_closure_changed_generation_and_failed_target() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            actor.ensure_volume((id(2)).into()).unwrap().generation = 7;
            let (mut fence, _result) = fence_fixture(&actor, &[id(2)]);
            fence.remaining = 0;
            assert!(
                fence
                    .check(actor.status, |scope| {
                        assert_eq!(scope.generation, 7);
                        Ok(0)
                    })
                    .unwrap()
            );
            assert!(
                fence.check(FeedStatus { gap_generation: 1, ..actor.status }, |_| Ok(0)).is_err()
            );
            assert!(
                fence
                    .check(
                        FeedStatus {
                            closed: Some(crate::store::change_feed::FeedClosure::SourceDropped),
                            ..actor.status
                        },
                        |_| Ok(0)
                    )
                    .is_err()
            );
            actor.volumes.get_mut(&(id(2)).into()).unwrap().generation = 8;
            actor.fence = Some(fence);
            actor.check_fence();
            let error = _result.await.unwrap().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "Couldn’t sync volume {}\npublication fence generation or readiness changed",
                    id(2)
                )
            );

            let (fence, result) = fence_fixture(&actor, &[id(2)]);
            actor.fence = Some(fence);
            let database = crate::store::DatabaseId::Entry(
                crate::store::EntryId::from_bytes([7; 15]).unwrap(),
            );
            let mut failure = PublisherError::new("uncertain AP acknowledgment");
            failure.database = Some(database);
            actor.volumes.get_mut(&(id(2)).into()).unwrap().invalidate(Phase::Failed, failure);
            actor.check_fence();
            assert_eq!(
                result.await.unwrap().unwrap_err().to_string(),
                format!(
                    "Couldn’t sync database {database} in volume {}\nuncertain AP acknowledgment",
                    id(2)
                )
            );
        });
    }

    #[test]
    fn fence_unknown_and_bootstrapping_targets_are_explicit_errors() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            for phase in [None, Some(Phase::NeedsBootstrap), Some(Phase::Bootstrapping { gap: 0 })]
            {
                if let Some(phase) = phase {
                    actor.ensure_volume((id(2)).into()).unwrap().phase = phase;
                }
                let expected_cause = if actor.volumes.contains_key(&id(2).into()) {
                    "publication fence volume requires bootstrap or recovery"
                } else {
                    "publication fence volume is not initialized"
                };
                let (reply, result) = test_reply();
                actor.command(fence_command(&[id(2)], reply));
                assert_eq!(
                    result.await.unwrap().unwrap_err().to_string(),
                    format!("Couldn’t sync volume {}\n{expected_cause}", id(2))
                );
                assert!(actor.fence.is_none());
            }
            // An uninitialized entry is distinct from the existing volume-wide target.
            let volume = id(2);
            let missing = crate::store::EntryId::from_bytes([9; 15]).unwrap();
            let mut keys = Box::new([PublishKey::from(volume); VOLUME_LIMIT]);
            keys[0] = PublishKey { volume, entry: Some(missing) };
            let (reply, result) = test_reply();
            actor.begin_fence(keys, 2, reply, FenceAdmission(Arc::new(AtomicBool::new(true))));
            assert_eq!(
                result.await.unwrap().unwrap_err().to_string(),
                format!("Couldn’t sync database {missing} in volume {volume}\npublication fence volume is not initialized"),
            );
            actor.disconnect();
            let (reply, result) = test_reply();
            actor.command(fence_command(&[volume], reply));
            assert_eq!(result.await.unwrap().unwrap_err().to_string(), "AP connection disconnected; recovery required");
        });
    }

    #[test]
    fn fence_completion_releases_admission_and_control_before_waking_its_caller() {
        struct AdmissionWaker {
            busy: Arc<AtomicBool>,
            slots: Arc<Semaphore>,
            woke: AtomicBool,
        }
        impl std::task::Wake for AdmissionWaker {
            fn wake(self: Arc<Self>) {
                assert!(!self.busy.load(Ordering::Acquire), "completed fence still owns admission");
                assert_eq!(self.slots.available_permits(), 1, "completed reply still owns control");
                self.woke.store(true, Ordering::Release);
            }
        }
        runtime().block_on(async {
            for outcome in ["settled", "failed", "rejected"] {
                let (_directory, _source, mut actor, _read) = actor();
                let busy = Arc::new(AtomicBool::new(true));
                let slots = Arc::new(Semaphore::new(1));
                let (send, mut result) = oneshot::channel();
                let reply = Reply { send, _permit: slots.clone().try_acquire_owned().unwrap() };
                let wake = Arc::new(AdmissionWaker {
                    busy: busy.clone(),
                    slots,
                    woke: AtomicBool::new(false),
                });
                let waker = std::task::Waker::from(wake.clone());
                let mut context = std::task::Context::from_waker(&waker);
                assert!(
                    std::future::Future::poll(std::pin::Pin::new(&mut result), &mut context)
                        .is_pending()
                );
                let admission = FenceAdmission(busy);
                if outcome == "rejected" {
                    let mut volumes = Box::new([PublishKey::from(id(2)); VOLUME_LIMIT]);
                    volumes[0] = id(3).into();
                    actor.begin_fence(volumes, 1, reply, admission);
                } else {
                    // An empty settled prefix isolates completion from network I/O.
                    // The other prefix fails because its target needs bootstrap.
                    actor.ensure_volume(id(2).into()).unwrap();
                    let (mut fence, _unused) = fence_fixture(&actor, &[id(2)]);
                    if outcome == "settled" {
                        fence.targets.clear();
                    }
                    fence.remaining = 0;
                    fence.reply = reply;
                    fence._admission = admission;
                    actor.fence = Some(fence);
                    actor.check_fence();
                }
                assert!(wake.woke.load(Ordering::Acquire));
                assert_eq!(result.try_recv().unwrap().is_ok(), outcome == "settled");
                assert!(actor.fence.is_none());
            }
        });
    }

    #[test]
    fn canceled_fence_retains_shared_control_credit_until_actual_failure() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            actor.ensure_volume((id(2)).into()).unwrap();
            let slots = Arc::new(Semaphore::new(COMMAND_LIMIT));
            let (send, result) = oneshot::channel();
            let (mut fence, _unused) = fence_fixture(&actor, &[id(2)]);
            fence.reply = Reply { send, _permit: slots.clone().try_acquire_owned().unwrap() };
            actor.fence = Some(fence);
            drop(result);
            assert_eq!(slots.available_permits(), COMMAND_LIMIT - 1);
            let (reply, busy) = test_reply();
            actor.command(fence_command(&[id(2)], reply));
            assert_eq!(busy.await.unwrap().unwrap_err().to_string(), "publication fence is busy");
            assert_eq!(slots.available_permits(), COMMAND_LIMIT - 1);
            actor.check_fence(); // Explicit target-readiness failure settles it.
            assert_eq!(slots.available_permits(), COMMAND_LIMIT);
        });
    }

    #[test]
    fn fence_empty_and_invalid_inputs_use_no_command_or_control_credit() {
        runtime().block_on(async {
            let (commands, mut receive) = mpsc::channel(COMMAND_LIMIT);
            let (completion, _read) = UnixStream::pair().unwrap();
            let publisher = Publisher {
                actor: inactive_actor(),
                shutdown: AtomicBool::new(false),
                commands,
                fence_busy: Arc::new(AtomicBool::new(false)),
                control_slots: Arc::new(Semaphore::new(0)),
                signals: Arc::new(Signals {
                    notices: Mutex::new(Notices::default()),
                    completion: Arc::new(Wake::unix(completion)),
                }),
            };
            assert!(publisher.fence(&[]).await.unwrap().is_empty());
            assert!(
                publisher.fence(&[id(1), id(1)]).await.unwrap_err().to_string().contains("repeats")
            );
            assert!(
                publisher
                    .fence(&[id(1); VOLUME_LIMIT + 1])
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("limit")
            );
            assert!(publisher.fence(&[id(1)]).await.unwrap_err().to_string().contains("busy"));
            assert!(receive.try_recv().is_err());
        });
    }

    #[test]
    fn queued_canceled_fence_keeps_single_admission_and_shared_credit_until_dispatch() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (commands, mut receive) = mpsc::channel(COMMAND_LIMIT);
            let slots = Arc::new(Semaphore::new(COMMAND_LIMIT));
            let _incoming = slots.clone().try_acquire_many_owned(7).unwrap();
            let publisher = Publisher {
                actor: inactive_actor(),
                shutdown: AtomicBool::new(false),
                commands,
                signals: actor.signals.clone(),
                control_slots: slots.clone(),
                fence_busy: Arc::new(AtomicBool::new(false)),
            };
            let volumes = [id(2)];
            let mut pending = Box::pin(publisher.fence(&volumes));
            assert!(poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_pending())).await);
            assert_eq!(slots.available_permits(), 0);
            drop(pending);
            assert!(publisher.fence(&volumes).await.unwrap_err().to_string().contains("busy"));
            assert!(publisher.disconnect().unwrap_err().to_string().contains("busy"));
            assert!(publisher.fence_busy.load(Ordering::Acquire));
            actor.command(*receive.try_recv().unwrap()); // Unknown scope settles with an explicit failure.
            assert_eq!(slots.available_permits(), 1);
            assert!(!publisher.fence_busy.load(Ordering::Acquire));
            publisher.disconnect().unwrap();
            assert_eq!(slots.available_permits(), 0);
            actor.command(*receive.try_recv().unwrap());
            assert_eq!(slots.available_permits(), 1);
        });
    }

    #[test]
    fn provider_domain_discovery_does_not_admit_other_account_volumes() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            *actor.scope.write() = super::super::remote::ClientScope::Provider(vec![id(2)]);
            for index in 1..=128 {
                source.volume_available(id(index));
            }
            for _ in 0..4 {
                actor.drain_feed();
            }
            assert_eq!(actor.volumes.len(), 1);
            assert!(actor.volumes.contains_key(&id(2).into()));
            let notices = actor.signals.notices.lock().unwrap();
            assert!(!notices.capacity_exhausted);
            assert_eq!(notices.volumes.len(), 1);
            assert!(notices.volumes[0].initialization_pending);
        });
    }

    #[test]
    fn provider_selection_excludes_commits_before_and_after_scope_removal() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (publisher, _commands) = health_publisher(&actor);
            // Delayed feed consumption models a committed write racing with
            // retirement. Root and selected-entry placeholders both retire.
            for iteration in 0..64 {
                let (old, next) = if iteration % 2 == 0 { (id(2), id(3)) } else { (id(3), id(2)) };
                *actor.scope.write() = super::super::remote::ClientScope::Provider(vec![old]);
                let selected = PublishKey {
                    volume: old,
                    entry: Some(crate::store::EntryId::from_bytes([4; 15]).unwrap()),
                };
                actor.ensure_volume(old.into()).unwrap();
                actor.ensure_volume(selected).unwrap();
                assert_eq!(publisher.retained_scopes().len(), 2);
                publish(&source, old, 1);
                *actor.scope.write() = super::super::remote::ClientScope::Provider(vec![next]);
                for key in publisher.retained_scopes() {
                    let (reply, removed) = test_reply();
                    actor.command(Command::Forget { volume: key, reply });
                    actor.drain_feed();
                    actor.retire_volumes();
                    removed.await.unwrap().unwrap();
                    publish(&source, old, 2);
                    actor.drain_feed();
                    assert!(!actor.volumes.contains_key(&key));
                }
                assert!(actor.volumes.is_empty());
                assert!(publisher.retained_scopes().is_empty());
                assert!(!actor.signals.notices.lock().unwrap().capacity_exhausted);
                assert_eq!(source.pending_count(), 0);
            }
        });
    }

    #[test]
    fn initializations_survive_diagnostic_reads_and_are_claimed_once_until_transport_retry() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (commands, _receive) = mpsc::channel(COMMAND_LIMIT);
            let publisher = Publisher { actor: inactive_actor(), shutdown: AtomicBool::new(false), commands,
                signals: actor.signals.clone(), control_slots: Arc::new(Semaphore::new(COMMAND_LIMIT)),
                fence_busy: Arc::new(AtomicBool::new(false)) };
            for index in 1..=VOLUME_LIMIT { actor.ensure_volume((id(index)).into()).unwrap(); }
            assert!(publisher.needs_initialization());
            assert_eq!(publisher.drain_notices().volumes.len(), VOLUME_LIMIT);
            assert!(publisher.needs_initialization());
            let volumes = publisher.take_initializations();
            assert_eq!(volumes.len(), VOLUME_LIMIT);
            assert!(volumes.capacity() * std::mem::size_of::<Id>() <= VOLUME_LIMIT * std::mem::size_of::<Id>());
            assert!(!publisher.needs_initialization());
            assert!(publisher.take_initializations().is_empty());
            for volume in &volumes { actor.signals.volume(actor.volumes[&(*volume).into()].notice(*volume)); }
            assert!(!publisher.needs_initialization(), "diagnostic notifications cannot schedule another attempt");
            publisher.retry_initializations_after_transport_loss(&volumes[..1]);
            assert!(publisher.needs_initialization());
            assert_eq!(publisher.take_initializations(), volumes[..1]);
            publisher.test_state(volumes[0], 1, PublishState::Failed);
            assert!(!publisher.needs_initialization(), "ordinary failure does not schedule a retry");
            publisher.retry_initializations_after_transport_loss(&volumes[..1]);
            assert!(publisher.needs_initialization(), "a confirmed lost connection may retry a failed preparation");
            let mut samples = Vec::with_capacity(1000);
            for _ in 0..1000 {
                publisher.retry_initializations_after_transport_loss(&volumes);
                let started = std::time::Instant::now();
                let ready = publisher.needs_initialization();
                let claimed = publisher.take_initializations();
                std::hint::black_box(&claimed);
                samples.push(started.elapsed().as_nanos() as u64);
                assert!(ready);
                assert_eq!(claimed.len(), VOLUME_LIMIT);
            }
            let mut sorted = samples.clone(); sorted.sort_unstable();
            crate::diagnostic!("initialization admission: {}", serde_json::json!({"boundary":"publisher readiness observation plus claim; 32 retained slots; isolated component",
                "samples_ns":samples,"p99_ns":sorted[989],"max_ns":sorted[999],"optimized":!cfg!(debug_assertions)}));
        });
    }

    #[test]
    fn failed_initialization_after_scope_retirement_waits_for_explicit_retry() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (publisher, _commands) = health_publisher(&actor);
            let volume = id(2);
            actor.ensure_volume(volume.into()).unwrap();
            assert_eq!(publisher.take_initializations(), [volume]);

            // LiveReplicas::begin retires the old scope before bootstrap I/O.
            let (reply, retired) = test_reply();
            actor.command(Command::Forget { volume: volume.into(), reply });
            actor.retire_volumes();
            retired.await.unwrap().unwrap();
            assert!(publisher.state(volume).is_none());

            // sync_owned admits the account dependency even when bootstrap I/O
            // fails before the replacement publisher reaches Bootstrapping.
            let (reply, admitted) = test_reply();
            actor.command(Command::Account(account::Command::ReadyVolume { volume, reply }));
            admitted.await.unwrap().unwrap();
            let (reply, failed) = test_reply();
            actor.command(Command::FailInitialization {
                volume: volume.into(),
                error: PublisherError::new("initial transfer failed"),
                reply,
            });
            failed.await.unwrap().unwrap();
            assert_eq!(publisher.state(volume), Some((0, PublishState::Failed)));
            assert!(
                !publisher.needs_initialization(),
                "failure must settle the replacement notice"
            );
            assert!(publisher.take_initializations().is_empty());

            for byte in 0..8 {
                publish(&source, volume, byte);
                actor.drain_feed();
                publisher.drain_notices();
                assert!(!publisher.needs_initialization());
            }
            assert_eq!(publisher.state(volume), Some((0, PublishState::Failed)));
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
            publisher.retry_initializations_after_transport_loss(&[volume]);
            actor.signals.volume(actor.volumes[&volume.into()].notice(volume));
            assert_eq!(publisher.take_initializations(), [volume]);
            assert!(!publisher.needs_initialization());
        });
    }

    #[test]
    fn draining_notices_preserves_current_generation_without_repeating_notifications() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (commands, _receive) = mpsc::channel(COMMAND_LIMIT);
            let publisher = Publisher {
                actor: inactive_actor(),
                shutdown: AtomicBool::new(false),
                commands,
                signals: actor.signals.clone(),
                control_slots: Arc::new(Semaphore::new(COMMAND_LIMIT)),
                fence_busy: Arc::new(AtomicBool::new(false)),
            };
            let state = actor.ensure_volume((id(2)).into()).unwrap();
            state.generation = 4;
            state.error = Some(PublisherError::new("older connection failed"));
            let old = state.notice(id(2));
            actor.signals.volume(old);
            assert_eq!(publisher.state(id(2)), Some((4, PublishState::NeedsBootstrap)));
            let drained = publisher.drain_notices();
            assert_eq!(drained.volumes[0].generation, 4);
            assert!(drained.volumes[0].error.is_some());
            assert!(publisher.drain_notices().volumes.is_empty());
            assert_eq!(publisher.state(id(2)), Some((4, PublishState::NeedsBootstrap)));
            let state = actor.volumes.get_mut(&(id(2)).into()).unwrap();
            state.generation = 5;
            state.phase = Phase::Bootstrapping { gap: 0 };
            state.error = None;
            actor.signals.volume(state.notice(id(2)));
            assert_eq!(publisher.state(id(2)), Some((5, PublishState::Bootstrapping)));
            assert_eq!(drained.volumes[0].generation, 4); // Stale consumed notice cannot change current state.
            assert_eq!(publisher.drain_notices().volumes[0].generation, 5);
        });
    }

    #[test]
    fn bootstrap_clears_only_its_volume_and_retains_post_boundary_changes() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(2), id(3)]);
            for byte in 0..20 {
                publish(&source, id(2 + byte % 2), byte as u8);
            }
            actor.drain_feed();
            assert_eq!(actor.volumes[&(id(2)).into()].queued.len(), 4);
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            while actor.feed_pending {
                actor.drain_feed();
            }
            let token = result.await.unwrap().unwrap();
            assert_eq!(token.volume, (id(2)).into());
            assert_eq!(token.gap, 0);
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
            assert_eq!(
                actor.volumes[&(id(3)).into()]
                    .queued
                    .iter()
                    .map(|r| r.bytes()[0])
                    .collect::<Vec<_>>(),
                (0..20).filter(|v| v % 2 == 1).collect::<Vec<_>>()
            );
            publish(&source, id(2), 20);
            publish(&source, id(3), 21);
            actor.drain_feed();
            assert_eq!(actor.volumes[&(id(2)).into()].queued[0].bytes(), [20]);
            assert_eq!(actor.volumes[&(id(3)).into()].queued.back().unwrap().bytes(), [21]);
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Bootstrapping { .. }));
        });
    }

    #[test]
    fn continuously_replenished_feed_cannot_extend_bootstrap_barrier() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            for _ in 0..FEED_LIMIT {
                publish(&source, id(2), 1);
            }
            let (reply, mut result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            for turn in 0..FEED_LIMIT / 8 {
                actor.drain_feed();
                if turn < FEED_LIMIT / 8 - 1 {
                    assert!(matches!(result.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
                }
                for _ in 0..8 {
                    publish(&source, id(2), 2);
                }
            }
            let token = result.try_recv().unwrap().unwrap();
            assert_eq!(token.gap, 0);
            assert_eq!(source.status().gap_generation, 0);
            actor.drain_feed();
            assert_eq!(actor.volumes[&(id(2)).into()].queued.len(), 8);
            assert!(actor.volumes[&(id(2)).into()].queued.iter().all(|r| r.bytes() == [2]));
        });
    }

    #[test]
    fn unknown_volume_notice_is_not_reissued_for_each_offline_edit() {
        runtime().block_on(async {
            let (_directory, source, mut actor, mut read) = actor();
            publish(&source, id(2), 1);
            actor.drain_feed();
            let mut bytes = [0; 256];
            assert_eq!(read.read(&mut bytes).unwrap(), 1);
            actor.signals.notices.lock().unwrap().volumes.clear();
            for _ in 0..8 {
                publish(&source, id(2), 2);
            }
            actor.drain_feed();
            assert_eq!(read.read(&mut bytes).unwrap_err().kind(), io::ErrorKind::WouldBlock);
            assert!(actor.signals.notices.lock().unwrap().volumes.is_empty());
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
            assert_eq!(source.pending_count(), 0);
        });
    }

    #[test]
    fn retained_bootstrap_records_keep_feed_credits_and_gap_invalidates_bootstrap() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (reply, token) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let token = token.await.unwrap().unwrap();
            for _ in 0..FEED_LIMIT {
                publish(&source, id(2), 1);
            }
            actor.drain_feed(); // Model the readable feed event after bootstrap.
            while actor.feed_pending {
                actor.drain_feed();
            }
            assert_eq!(actor.volumes[&(id(2)).into()].queued.len(), FEED_LIMIT);
            publish(&source, id(2), 2);
            actor.drain_feed();
            assert_eq!(source.status().gap_generation, 1);
            assert!(
                actor
                    .check_bootstrap(&token, (id(2)).into(), source.status())
                    .unwrap_err()
                    .as_str()
                    .contains("committed feed changed")
            );
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::NeedsRecovery));
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
            assert_eq!(
                actor
                    .signals
                    .notices
                    .lock()
                    .unwrap()
                    .volumes
                    .iter()
                    .find(|slot| slot.notice.volume == id(2))
                    .unwrap()
                    .notice
                    .state,
                PublishState::NeedsRecovery
            );
        });
    }

    #[test]
    fn inactive_records_release_global_credits_without_starving_replay() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(3)]);
            let (reply, _removed) = test_reply();
            for phase in [
                Phase::NeedsBootstrap,
                Phase::NeedsRecovery,
                Phase::Disconnected,
                Phase::Failed,
                Phase::Removing { reply },
            ] {
                let inactive = actor.ensure_volume((id(2)).into()).unwrap();
                inactive.phase = phase;
                for value in 0..FEED_LIMIT * 2 {
                    publish(&source, id(2), value as u8);
                    publish(&source, id(3), value as u8);
                    actor.drain_feed();
                    assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
                    assert_eq!(actor.volumes[&(id(2)).into()].accepted, 0);
                    let other = actor.volumes.get_mut(&(id(3)).into()).unwrap();
                    assert_eq!(other.queued.pop_front().unwrap().bytes(), [value as u8]);
                    assert_eq!(source.pending_count(), 0);
                    assert_eq!(source.status().gap_generation, 0);
                }
            }
            assert_eq!(actor.volumes[&(id(3)).into()].accepted, (FEED_LIMIT * 2 * 5) as u64);
        });
    }

    #[test]
    fn preparing_releases_records_before_and_during_snapshot_barrier() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (reply, token) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            for _ in 0..FEED_LIMIT * 2 {
                publish(&source, id(2), 1);
                actor.drain_feed();
                assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
                assert_eq!(source.pending_count(), 0);
            }
            assert_eq!(source.status().gap_generation, 0);
            actor.start_drains();
            assert_eq!(source.pending_count(), 0);
            publish(&source, id(2), 2);
            actor.drain_feed();
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
            actor.drain_feed();
            let _token = token.await.unwrap().unwrap();
            publish(&source, id(2), 3);
            actor.drain_feed();
            assert_eq!(actor.volumes[&(id(2)).into()].queued[0].bytes(), [3]);
        });
    }

    #[test]
    fn dropping_bootstrap_lease_discards_replay_and_reports_terminal_cancellation() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (reply, token) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let token = token.await.unwrap().unwrap();
            publish(&source, id(2), 1);
            actor.drain_feed();
            assert_eq!(source.pending_count(), 1);
            drop(token);
            actor.start_jobs(); // Must observe cancellation before starting work.
            assert!(actor.jobs.is_empty());
            assert_eq!(source.pending_count(), 0);
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Failed));
            assert_eq!(actor.volumes[&(id(2)).into()].notice(id(2)).state, PublishState::Failed);
            assert!(actor.volumes[&(id(2)).into()].cancellation.is_none());
        });
    }

    #[test]
    fn dropped_lease_wakes_idle_actor_without_command_credit() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, read) = actor();
            let (commands, receive) = mpsc::channel(COMMAND_LIMIT);
            actor.receive = receive;
            let signals = actor.signals.clone();
            let task = tokio::spawn(actor.run());
            let slots = Arc::new(Semaphore::new(COMMAND_LIMIT));
            let publisher = Publisher {
                actor: task.abort_handle(),
                shutdown: AtomicBool::new(false),
                commands,
                signals,
                control_slots: slots.clone(),
                fence_busy: Arc::new(AtomicBool::new(false)),
            };
            let token = publisher.begin_bootstrap(id(2)).await.unwrap();
            let _all_credits = slots.clone().try_acquire_many_owned(COMMAND_LIMIT as u32).unwrap();
            // SAFETY: AsyncFd owns the socket, which is never replaced or closed
            // before deregistration.
            #[allow(unsafe_code, reason = "The registration owns its unchanged socket.")]
            let read = unsafe { AsyncFd::register(read) }.unwrap();
            drop(token);
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while publisher.state(id(2)).map(|(_, state)| state) != Some(PublishState::Failed) {
                    let mut ready = read.readable().await.unwrap();
                    let mut bytes = [0; 64];
                    let _ = ready.try_io(|fd| fd.get_ref().read(&mut bytes));
                }
            })
            .await
            .unwrap();
            assert_eq!(slots.available_permits(), 0);
            publisher.shutdown();
            assert!(task.await.unwrap_err().is_cancelled());
        });
    }

    #[test]
    fn dropped_lease_reply_and_stale_generation_cannot_cancel_replacement() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (reply, old) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let old = old.await.unwrap().unwrap();
            let (reply, new) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let new = new.await.unwrap().unwrap();
            let old_generation = old.generation;
            assert!(
                actor
                    .check_bootstrap(&old, (id(2)).into(), actor.status)
                    .unwrap_err()
                    .as_str()
                    .contains("stale")
            );
            assert!(actor.check_bootstrap(&new, (id(2)).into(), actor.status).is_ok());
            drop(old);
            actor.cancel_generation((id(2)).into(), old_generation, false);
            actor.cancel_dropped_leases();
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Bootstrapping { .. }));
            assert_eq!(actor.volumes[&(id(2)).into()].generation, new.generation);
            // Finish transfers the same lease through this reply type. If the
            // requester vanishes after settlement, the queued lease is dropped.
            let (reply, finished) = test_reply::<PublicationLease, PublisherError>();
            reply.send(Ok(new.lease)).unwrap();
            drop(finished);
            actor.cancel_dropped_leases();
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Failed));
        });
    }

    #[test]
    fn abandoned_bootstrap_is_excluded_instead_of_a_lost_owner() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let token = result.await.unwrap().unwrap();
            publish(&source, id(2), 1);
            actor.drain_feed();
            assert_eq!(actor.volumes[&(id(2)).into()].queued.len(), 1);
            let (reply, abandoned) = test_reply();
            actor.command(Command::Abandon { token, reply });
            abandoned.await.unwrap().unwrap();
            // The token's lease drops inside the command; without the explicit
            // handback this is indistinguishable from a crashed owner.
            actor.cancel_dropped_leases();
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Excluded));
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
            assert!(actor.volumes[&(id(2)).into()].error.is_none());
            let (publisher, _commands) = health_publisher(&actor);
            assert_eq!(publisher.state(id(2)), Some((1, PublishState::Excluded)));
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Private);
            // Waiting for a peer is not a bootstrap request; retrying it here
            // would reopen the same publication on every wake.
            assert!(!publisher.needs_initialization());
            // Dropping a token remains a lost owner, which fails every scope's
            // health together. That is what an abandoned scope must not do.
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(3)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            drop(result.await.unwrap().unwrap());
            actor.cancel_dropped_leases();
            assert!(matches!(actor.volumes[&(id(3)).into()].phase, Phase::Failed));
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
        });
    }

    #[test]
    fn observing_new_gap_does_not_invalidate_bootstrap_that_captured_it() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            actor.ensure_volume((id(2)).into()).unwrap().phase = Phase::Bootstrapping { gap: 4 };
            actor.observe(FeedStatus { gap_generation: 4, ..actor.status });
            assert!(matches!(
                actor.volumes[&(id(2)).into()].phase,
                Phase::Bootstrapping { gap: 4 }
            ));
            actor.observe(FeedStatus { gap_generation: 5, ..actor.status });
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::NeedsRecovery));
        });
    }

    #[test]
    fn volume_capacity_is_explicit_without_retaining_unknown_records() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            for byte in 1..=VOLUME_LIMIT + 1 {
                publish(&source, id(byte), byte as u8);
            }
            while actor.feed_pending {
                actor.drain_feed();
            }
            assert_eq!(actor.volumes.len(), VOLUME_LIMIT);
            assert_eq!(actor.volumes.values().map(|v| v.queued.len()).sum::<usize>(), 0);
            let notices = actor.signals.notices.lock().unwrap();
            assert_eq!(notices.volumes.len(), VOLUME_LIMIT);
            assert!(notices.error.as_ref().unwrap().as_str().contains("volume limit"));
        });
    }

    #[test]
    fn forgetting_a_volume_clears_the_capacity_failure() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (publisher, _receive) = health_publisher(&actor);
            for byte in 1..=VOLUME_LIMIT + 1 {
                publish(&source, id(byte), byte as u8);
            }
            while actor.feed_pending {
                actor.drain_feed();
            }
            assert_eq!(actor.volumes.len(), VOLUME_LIMIT);
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
            // Reading the diagnostic must not resolve the capacity condition.
            assert!(publisher.drain_notices().error.is_some());
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
            let (reply, removed) = test_reply();
            actor.command(Command::Forget { volume: (id(1)).into(), reply });
            actor.retire_volumes();
            removed.await.unwrap().unwrap();
            assert_ne!(publisher.health(actor.status, 0), PublishHealth::Failed);
            publisher.shutdown();
        });
    }

    #[test]
    fn cancelled_bootstrap_caller_does_not_leave_volume_ready() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            drop(result);
            actor.start_drains();
            actor.drain_feed();
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Failed));
        });
    }

    #[test]
    fn disconnect_invalidates_bootstrap_and_does_not_wait_for_ack() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            publish(&source, id(2), 1);
            actor.drain_feed();
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.disconnect();
            assert!(result.await.unwrap().is_err());
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::Disconnected));
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
        });
    }

    #[test]
    fn removal_waits_for_started_storage_work_and_preserves_other_volume_order() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(2), id(3)]);
            publish(&source, id(2), 1);
            publish(&source, id(3), 2);
            publish(&source, id(3), 3);
            actor.drain_feed();
            let record =
                actor.volumes.get_mut(&(id(2)).into()).unwrap().queued.pop_front().unwrap();
            let permit = actor.apply_slots.clone().try_acquire_owned().unwrap();
            let (started, started_rx) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let task = actor.jobs.spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let _ = started.send(());
                    held.recv().unwrap();
                    drop(record);
                    drop(permit);
                })
                .await
                .unwrap();
                JobResult::Volume(VolumeJobResult {
                    volume: (id(2)).into(),
                    generation: 0,
                    publication: 1,
                    records: 1,
                    result: Err(PublisherError::new("cancelled after storage completed").into()),
                    diagnostic: None,
                })
            });
            let (cancel, cancelled) = oneshot::channel();
            actor.volumes.get_mut(&(id(2)).into()).unwrap().active =
                Some(Active { id: task.id(), cancel: Some(cancel) });
            started_rx.await.unwrap();
            let (reply, mut result) = test_reply();
            actor.command(Command::Forget { volume: (id(2)).into(), reply });
            actor.retire_volumes();
            assert!(matches!(result.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
            assert!(actor.volumes.contains_key(&(id(2)).into()));
            assert_eq!(actor.apply_slots.available_permits(), 1);
            cancelled.await.unwrap();
            publish(&source, id(2), 4);
            actor.drain_feed();
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
            release.send(()).unwrap();
            let completion = actor.jobs.join_next_with_id().await.unwrap();
            actor.completed(completion);
            actor.retire_volumes();
            assert!(result.try_recv().unwrap().is_ok());
            assert!(!actor.volumes.contains_key(&(id(2)).into()));
            assert_eq!(actor.apply_slots.available_permits(), 2);
            assert_eq!(
                actor.volumes[&(id(3)).into()]
                    .queued
                    .iter()
                    .map(|r| r.bytes()[0])
                    .collect::<Vec<_>>(),
                [2, 3]
            );
            // A later committed change explicitly asks lifecycle reconciliation
            // again, rather than consulting a permanent suppression tombstone.
            publish(&source, id(2), 5);
            actor.drain_feed();
            assert!(matches!(actor.volumes[&(id(2)).into()].phase, Phase::NeedsBootstrap));
            assert!(actor.volumes[&(id(2)).into()].queued.is_empty());
        });
    }

    #[test]
    fn removed_and_readmitted_volume_cannot_reuse_an_old_bootstrap_token() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let old = result.await.unwrap().unwrap();
            let (reply, removed) = test_reply();
            actor.command(Command::Forget { volume: (id(2)).into(), reply });
            actor.retire_volumes();
            removed.await.unwrap().unwrap();
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            let new = result.await.unwrap().unwrap();
            assert_eq!(old.volume, new.volume);
            assert_eq!(old.epoch, new.epoch);
            assert_eq!(old.gap, new.gap);
            assert!(new.generation > old.generation);
        });
    }

    #[test]
    fn complete_birth_delta_is_not_split_during_bootstrap_replay() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (reply, result) = test_reply();
            actor.command(Command::Begin { volume: (id(2)).into(), reply });
            actor.start_drains();
            actor.drain_feed();
            result.await.unwrap().unwrap();
            let database = crate::store::DatabaseId::Entry(
                crate::store::EntryId::from_bytes([3; 15]).unwrap(),
            );
            source.publish(id(2), database, CommittedDelta::Bytes(vec![1, 2, 3, 4]));
            actor.drain_feed();
            assert_eq!(actor.volumes[&(id(2)).into()].queued.len(), 1);
            let record = &actor.volumes[&(id(2)).into()].queued[0];
            assert_eq!(record.kind(), CommittedChangeKind::Database(database));
            assert_eq!(record.bytes(), &[1, 2, 3, 4]);
            assert!(matches!(
                PublishScope::new(id(2), None).unwrap().request(record.kind()).unwrap(),
                ServiceRequest::PublishReplica { database_id, .. } if database_id == database
            ));
        });
    }

    #[test]
    fn local_discovery_cannot_be_serialized_as_an_ap_publication() {
        let scope = PublishScope::new(id(2), None).unwrap();
        assert!(scope.request(CommittedChangeKind::VolumeAvailable).is_err());
    }

    #[test]
    fn account_bootstrap_cancellation_capacity_and_stale_failure_are_bounded() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (reply, result) = test_reply();
            actor.command(Command::Account(account::Command::Begin { reply }));
            drop(result);
            actor.settle_account();
            assert_eq!(
                actor.signals.notices.lock().unwrap().account.as_ref().unwrap().state,
                PublishState::Failed,
                "a canceled bootstrap must not leave an apparently preparing source"
            );
            let (reply, result) = test_reply();
            actor.command(Command::Account(account::Command::Begin { reply }));
            actor.settle_account();
            let generation = result.await.unwrap().unwrap();
            assert_eq!(generation, 2);
            let (reply, result) = test_reply();
            actor.command(Command::Account(account::Command::Fail {
                generation: generation - 1,
                error: PublisherError::new("stale failure"),
                reply,
            }));
            result.await.unwrap().unwrap();
            assert_eq!(
                actor.signals.notices.lock().unwrap().account.as_ref().unwrap().state,
                PublishState::Bootstrapping
            );
            for byte in 0..VOLUME_LIMIT - 1 {
                actor.ensure_volume((id(byte)).into()).unwrap();
            }
            assert!(
                actor.ensure_volume((id(VOLUME_LIMIT - 1)).into()).is_err(),
                "account consumes one source slot"
            );
            let (reply, result) = test_reply();
            actor.command(Command::Account(account::Command::Fail {
                generation,
                error: PublisherError::new("bootstrap failed"),
                reply,
            }));
            result.await.unwrap().unwrap();
            assert_eq!(
                actor.signals.notices.lock().unwrap().account.as_ref().unwrap().state,
                PublishState::Failed
            );
        });
    }

    #[test]
    fn mailbox_and_error_storage_are_bounded() {
        let (commands, _receive) = mpsc::channel(COMMAND_LIMIT);
        let (completion, _read) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let publisher = Publisher {
            actor: inactive_actor(),
            shutdown: AtomicBool::new(false),
            commands,
            fence_busy: Arc::new(AtomicBool::new(false)),
            control_slots: Arc::new(Semaphore::new(COMMAND_LIMIT)),
            signals: Arc::new(Signals {
                notices: Mutex::new(Notices::default()),
                completion: Arc::new(Wake::unix(completion)),
            }),
        };
        for _ in 0..COMMAND_LIMIT {
            publisher.disconnect().unwrap();
        }
        assert!(publisher.disconnect().unwrap_err().to_string().contains("busy"));
        // Tokio1.53 uses32 slots/block on64bit, irrespective of capacity8.
        // Boxed bodies keep physical blocks small. Charge two list blocks plus
        // one transient grow allocation per admitted producer, not just8slots.
        let block_bytes =
            32 * std::mem::size_of::<Box<Command>>() + 4 * std::mem::size_of::<usize>();
        // oneshot Inner: Option<Result>, state, two Wakers, Arc counters.
        // mpsc Chan's padded pointers/semaphore/Notify reserve512bytes.
        let reply_bytes = std::mem::size_of::<
            Option<std::result::Result<BootstrapToken, PublisherError>>,
        >() + 3 * std::mem::size_of::<usize>()
            + 2 * std::mem::size_of::<std::task::Waker>();
        // The actor returns at most one boxed error/rejected reply at a time.
        // BootstrapToken is the largest reply payload; include that transient
        // allocation as well as the original inline retained-error storage.
        let transient_error_bytes =
            std::mem::size_of::<std::result::Result<BootstrapToken, PublisherError>>();
        let control_bytes = transient_error_bytes + block_bytes * (COMMAND_LIMIT + 2)
            + std::mem::size_of::<Command>() * COMMAND_LIMIT
            + reply_bytes * COMMAND_LIMIT + 512
            + 2 * VOLUME_LIMIT * std::mem::size_of::<PublishKey>() // Bounded input conversion and one fence.
            + std::mem::size_of::<Semaphore>() + 5 * std::mem::size_of::<usize>()
            + std::mem::size_of::<AbortHandle>() // Reuses the existing actor task allocation.
            + std::mem::size_of::<usize>() // Shutdown AtomicBool including alignment padding.
            + std::mem::size_of::<Fence>()
            + VOLUME_LIMIT * (std::mem::size_of::<FenceTarget>()
                + std::mem::size_of::<FencedVolume>() + 2 * std::mem::size_of::<u64>());
        // Entry-qualified fence identities add at most 3 KiB to control storage.
        assert!(control_bytes <= 256 * 1024, "publisher control uses {control_bytes} bytes");
        crate::diagnostic!("publisher command/fence upper allocation: {control_bytes} bytes");
        let per_command = std::mem::size_of::<Command>() + reply_bytes;
        let fixed_control = control_bytes - per_command * COMMAND_LIMIT;
        for publishers in 0..=COMMAND_LIMIT {
            let combined = fixed_control
                + publishers * per_command
                + crate::engine::replication::control_allocation_bytes(COMMAND_LIMIT - publishers);
            assert!(
                combined <= 256 * 1024,
                "combined control uses {combined} bytes with {publishers} publisher commands"
            );
        }
        // Tokio1.53 oneshot Inner<()>: two Wakers, state, aligned Option<()>,
        // plus Arc counts. These per-generation channels belong to driver
        // lifetime state, not the command/notice allocation above. This counts
        // current generations only; superseded tokens/replies and allocator
        // overhead still require the full aggregate driver proof.
        let lease_channel =
            2 * std::mem::size_of::<std::task::Waker>() + 4 * std::mem::size_of::<usize>();
        let lease_state = VOLUME_LIMIT
            * (lease_channel
                + std::mem::size_of::<PublicationLease>()
                + std::mem::size_of::<Option<oneshot::Receiver<()>>>());
        assert!(lease_state <= 48 * 1024);
        crate::diagnostic!(
            "publisher current-generation lifetime subtotal: {lease_state} bytes; channel {lease_channel} bytes"
        );
        assert!(std::mem::size_of::<PublishNotice>() * VOLUME_LIMIT <= 512 * 1024);
        let notice_bytes = VOLUME_LIMIT
            * (std::mem::size_of::<PublishSlot>() + std::mem::size_of::<PublishNotice>())
            + std::mem::size_of::<Notices>()
            + std::mem::size_of::<PublisherNotices>()
            + std::mem::size_of::<Signals>()
            + 2 * std::mem::size_of::<usize>();
        crate::diagnostic!(
            "publisher pending+drained notice upper allocation: {notice_bytes} bytes"
        );
        let combined_notices = notice_bytes + crate::engine::replication::notice_allocation_bytes();
        assert!(
            combined_notices <= 512 * 1024,
            "combined live notices use {combined_notices} bytes"
        );
        crate::diagnostic!(
            "combined live pending+drained notice upper allocation: {combined_notices} bytes"
        );
        let error = PublisherError::new("界".repeat(512));
        assert_eq!(error.as_str().len(), ERROR_LIMIT / 3 * 3);
        assert_eq!(error.as_str().chars().count(), ERROR_LIMIT / 3);
        assert!(error.as_str().ends_with('…'));
    }

    #[test]
    fn failed_notification_faults_publication() {
        let (completion, read) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        drop(read);
        let signals = Signals {
            notices: Mutex::new(Notices::default()),
            completion: Arc::new(Wake::unix(completion)),
        };
        signals.wake();
        let notices = signals.notices.lock().unwrap();
        assert!(notices.error.is_some());
        assert!(notices.faulted);
    }

    #[test]
    fn cancellation_wakes_a_publication_waiting_for_storage_admission() {
        runtime().block_on(async {
            let slots = Arc::new(Semaphore::new(0));
            let (cancel, mut cancelled) = oneshot::channel();
            let job = tokio::spawn(async move {
                cancellable(slots.clone().acquire_owned(), &mut cancelled).await
            });
            tokio::task::yield_now().await;
            cancel.send(()).unwrap();
            assert!(job.await.unwrap().unwrap_err().as_str().contains("cancelled"));
        });
    }

    #[test]
    fn cancellation_precedes_ready_publication_work() {
        // A sent or dropped cancellation channel must stop even ready work before polling it.
        runtime().block_on(async {
            for sent in [false, true] {
                let (cancel, mut cancelled) = oneshot::channel();
                if sent {
                    cancel.send(()).unwrap();
                } else {
                    drop(cancel);
                }

                let mut polled = false;
                let work = poll_fn(|_| {
                    polled = true;
                    Poll::Ready(())
                });
                let error = cancellable(work, &mut cancelled).await.unwrap_err();
                assert_eq!(error.as_str(), "publication cancelled");
                assert!(!polled, "cancelled publication must not begin ready work");
            }
        });
    }

    fn health_publisher(actor: &Actor) -> (Publisher, mpsc::Receiver<Box<Command>>) {
        let (commands, receive) = mpsc::channel(COMMAND_LIMIT);
        let task = tokio::spawn(std::future::pending::<()>());
        (
            Publisher {
                actor: task.abort_handle(),
                shutdown: AtomicBool::new(false),
                commands,
                signals: actor.signals.clone(),
                control_slots: Arc::new(Semaphore::new(COMMAND_LIMIT)),
                fence_busy: Arc::new(AtomicBool::new(false)),
            },
            receive,
        )
    }
    fn health_ready_notice(actor: &mut Actor, volume: Id) {
        let state = actor.ensure_volume((volume).into()).unwrap();
        state.generation = 7;
        // Health inspects the retained notice, not a fabricated AP connection.
        // Bootstrapping keeps the actor-side gap invalidation path testable.
        state.phase = Phase::Bootstrapping { gap: 0 };
        let mut notice = state.notice(volume);
        notice.state = PublishState::Ready;
        actor.signals.volume(notice);
        actor.observe(actor.status);
    }

    #[test]
    fn health_rejects_unobserved_gap_and_stale_generation() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (publisher, _receive) = health_publisher(&actor);
            health_ready_notice(&mut actor, id(2));
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Ready);
            let changed =
                FeedStatus { gap_generation: actor.status.gap_generation + 1, ..actor.status };
            assert_eq!(publisher.health(changed, 0), PublishHealth::Recovering);
            actor.observe(changed);
            assert_eq!(publisher.signals.notices.lock().unwrap().observed, Some(changed));
            assert_eq!(publisher.state(id(2)), Some((7, PublishState::NeedsRecovery)));
            assert_eq!(publisher.health(changed, 0), PublishHealth::Recovering);
            // A new generation still has to finish bootstrap before it is ready again.
            let state = actor.volumes.get_mut(&(id(2)).into()).unwrap();
            state.generation = 8;
            state.phase = Phase::Bootstrapping { gap: changed.gap_generation };
            actor.signals.volume(state.notice(id(2)));
            assert_eq!(publisher.health(changed, 0), PublishHealth::Recovering);
            publisher.shutdown();
        });
    }

    #[test]
    fn terminal_publication_health_survives_notice_drain_and_disconnect() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (publisher, _receive) = health_publisher(&actor);
            health_ready_notice(&mut actor, id(2));
            let state = actor.volumes.get_mut(&(id(2)).into()).unwrap();
            state.invalidate(Phase::Failed, PublisherError::new("permission denied"));
            actor.signals.volume(state.notice(id(2)));
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
            assert_eq!(publisher.drain_notices().volumes[0].state, PublishState::Failed);
            actor.disconnect();
            let state = actor.ensure_volume((id(1)).into()).unwrap();
            state.phase = Phase::Disconnected;
            let notice = state.notice(id(1));
            actor.signals.volume(notice);
            assert_eq!(publisher.state(id(2)), Some((7, PublishState::Failed)));
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
            assert_eq!(
                publisher.health(
                    FeedStatus { gap_generation: actor.status.gap_generation + 1, ..actor.status },
                    1
                ),
                PublishHealth::Failed
            );
            publisher.shutdown();
        });
    }

    #[test]
    fn health_counts_retained_feed_credits_and_reports_private_settlement() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            let (publisher, _receive) = health_publisher(&actor);
            health_ready_notice(&mut actor, id(2));
            publish(&source, id(2), 1);
            let retained = actor.feed.get_mut().0.drain().records.pop().unwrap();
            assert_eq!(source.pending_count(), 1); // FIFO is empty; lease is still held.
            assert_eq!(
                publisher.health(source.status(), source.pending_count()),
                PublishHealth::Pending
            );
            drop(retained);
            assert_eq!(source.pending_count(), 0);
            assert_eq!(
                publisher.health(source.status(), source.pending_count()),
                PublishHealth::Ready
            );
            publisher.signals.notices.lock().unwrap().volumes[0].notice.excluded_publication =
                Some(2);
            assert_eq!(publisher.health(source.status(), 0), PublishHealth::Private);
            assert_eq!(publisher.health(source.status(), 1), PublishHealth::Pending);
            publisher.drain_notices();
            assert_eq!(publisher.health(source.status(), 0), PublishHealth::Private);
            publisher.shutdown();
        });
    }

    #[test]
    fn health_scans_all_scopes_and_retains_consumed_global_failure() {
        runtime().block_on(async {
            let (_directory, _source, mut actor, _read) = actor();
            let (publisher, _receive) = health_publisher(&actor);
            // A live, observed empty feed has no outstanding scope work. The
            // Remote owner determines whether the account transport is open.
            actor.observe(actor.status);
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Ready);
            for byte in 1..=VOLUME_LIMIT {
                health_ready_notice(&mut actor, id(byte));
            }
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Ready);
            {
                let mut notices = publisher.signals.notices.lock().unwrap();
                notices.volumes.last_mut().unwrap().notice.state = PublishState::Pending;
            }
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Pending);
            {
                let mut notices = publisher.signals.notices.lock().unwrap();
                notices.volumes.last_mut().unwrap().notice.state = PublishState::Ready;
                notices.volumes.last_mut().unwrap().notice.generation = 0;
            }
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Recovering);
            publisher
                .signals
                .notices
                .lock()
                .unwrap()
                .volumes
                .last_mut()
                .unwrap()
                .notice
                .generation = 7;
            publisher.signals.error("publication volume capacity exhausted");
            assert!(publisher.drain_notices().error.is_some());
            assert!(publisher.drain_notices().error.is_none());
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Failed);
            publisher.shutdown();
            // Feedback does not wait for Tokio to poll the canceled actor.
            assert_eq!(publisher.health(actor.status, 0), PublishHealth::Disconnected);
        });
    }

    #[test]
    fn owner_shutdown_fails_queued_fence_and_replies_without_control_credit() {
        runtime().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(LocalStore::open(directory.path()).unwrap());
            let (completion, _read) = UnixStream::pair().unwrap();
            completion.set_nonblocking(true).unwrap();
            let slots = Arc::new(Semaphore::new(COMMAND_LIMIT));
            let publisher = Publisher::start(
                &Handle::current(),
                store.clone(),
                Arc::new(Wake::unix(completion)),
                Arc::new(Semaphore::new(2)),
                slots.clone(),
                Arc::new(BlobTransfers::default()),
                Default::default(),
            )
            .unwrap();
            let volume = store.create_volume("Retained after owner loss", true).unwrap();
            publisher.begin_bootstrap(volume.id).await.unwrap(); // Actor has run.
            let volumes = [volume.id];
            let mut fence = Box::pin(publisher.fence(&volumes));
            assert!(poll_fn(|cx| Poll::Ready(fence.as_mut().poll(cx).is_pending())).await);
            let mut pending = Vec::new();
            for _ in 1..COMMAND_LIMIT {
                let (reply, result) = publisher.reply().unwrap();
                publisher.send(Command::Begin { volume: (volume.id).into(), reply }).unwrap();
                pending.push(result);
            }
            assert_eq!(slots.available_permits(), 0);
            assert!(publisher.disconnect().is_err());
            publisher.shutdown();
            assert_eq!(
                publisher.health(store.change_status(), store.pending_change_count()),
                PublishHealth::Disconnected
            );
            publisher.shutdown(); // Idempotent; never acquires another credit.
            assert!(fence.await.unwrap_err().to_string().contains("closed"));
            for result in pending {
                assert!(result.await.is_err());
            }
            assert_eq!(slots.available_permits(), COMMAND_LIMIT);
            assert_eq!(
                store.change_status().closed,
                Some(crate::store::change_feed::FeedClosure::ReceiverDropped)
            );
            assert_eq!(store.volume(volume.id).unwrap(), volume);
            // Closing live delivery does not roll back or prohibit local durability.
            let later = store.create_volume("Offline after owner loss", true).unwrap();
            assert_eq!(store.volume(later.id).unwrap(), later);
            assert!(publisher.begin_bootstrap(volume.id).await.is_err());
        });
    }

    #[test]
    fn owner_shutdown_keeps_started_storage_lease_until_work_finishes() {
        runtime().block_on(async {
            let (_directory, source, mut actor, _read) = actor();
            retain_for_replay(&mut actor, &[id(2)]);
            publish(&source, id(2), 1);
            actor.drain_feed();
            let record =
                actor.volumes.get_mut(&(id(2)).into()).unwrap().queued.pop_front().unwrap();
            let apply_slots = actor.apply_slots.clone();
            let permit = apply_slots.clone().try_acquire_owned().unwrap();
            let (started, started_rx) = oneshot::channel();
            let (finished, finished_rx) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let job = actor.jobs.spawn(async move {
                tokio::task::spawn_blocking(move || {
                    let _ = started.send(());
                    held.recv().unwrap();
                    assert_eq!(record.bytes(), [1]);
                    drop(record);
                    drop(permit);
                    let _ = finished.send(());
                })
                .await
                .unwrap();
                JobResult::Volume(VolumeJobResult {
                    volume: (id(2)).into(),
                    generation: 0,
                    publication: 1,
                    records: 1,
                    result: Err(PublisherError::new("storage completed after cancellation").into()),
                    diagnostic: None,
                })
            });
            actor.volumes.get_mut(&(id(2)).into()).unwrap().active =
                Some(Active { id: job.id(), cancel: None });
            let (commands, receive) = mpsc::channel(COMMAND_LIMIT);
            actor.receive = receive;
            let signals = actor.signals.clone();
            let task = tokio::spawn(actor.run());
            let publisher = Publisher {
                actor: task.abort_handle(),
                shutdown: AtomicBool::new(false),
                commands,
                signals,
                control_slots: Arc::new(Semaphore::new(COMMAND_LIMIT)),
                fence_busy: Arc::new(AtomicBool::new(false)),
            };
            started_rx.await.unwrap();
            let mut bootstrap = Box::pin(publisher.begin_bootstrap(id(2)));
            assert!(poll_fn(|cx| Poll::Ready(bootstrap.as_mut().poll(cx).is_pending())).await);
            tokio::task::yield_now().await; // Bootstrap remains held behind the active job.
            publisher.shutdown();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(bootstrap.await.is_err());
            assert_eq!(
                source.status().closed,
                Some(crate::store::change_feed::FeedClosure::ReceiverDropped)
            );
            assert_eq!(apply_slots.available_permits(), 1);
            release.send(()).unwrap();
            finished_rx.await.unwrap();
            assert_eq!(apply_slots.available_permits(), 2);
        });
    }

    #[test]
    fn real_store_publication_wakes_idle_actor_without_gui_or_timer() {
        runtime().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(LocalStore::open(directory.path()).unwrap());
            let (completion, read) = UnixStream::pair().unwrap();
            completion.set_nonblocking(true).unwrap();
            read.set_nonblocking(true).unwrap();
            // SAFETY: AsyncFd owns the socket, which is never replaced or closed
            // before deregistration.
            #[allow(unsafe_code, reason = "The registration owns its unchanged socket.")]
            let ready = unsafe { AsyncFd::register(read) }.unwrap();
            let publisher = Publisher::start(
                &Handle::current(),
                store.clone(),
                Arc::new(Wake::unix(completion)),
                Arc::new(Semaphore::new(2)),
                Arc::new(Semaphore::new(COMMAND_LIMIT)),
                Arc::new(BlobTransfers::default()),
                Default::default(),
            )
            .unwrap();
            tokio::task::yield_now().await;
            let volume = store.create_volume("Offline publisher test", true).unwrap();
            let mut wake = ready.readable().await.unwrap();
            wake.clear_ready();
            let notices = publisher.drain_notices();
            assert!(notices.error.is_none());
            assert_eq!(notices.volumes.len(), 1);
            assert_eq!(notices.volumes[0].volume, volume.id);
            assert_eq!(notices.volumes[0].state, PublishState::NeedsBootstrap);
            let token = publisher.begin_bootstrap(volume.id).await.unwrap();
            assert_eq!(token.volume, (volume.id).into());
            assert_eq!(token.gap, store.change_status().gap_generation);
            publisher.disconnect().unwrap();
            drop(publisher);
            tokio::task::yield_now().await;
        });
    }
}
