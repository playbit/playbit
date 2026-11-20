//! Engine ownership of the bounded publisher and installed subscriptions.
//! Bootstrap itself belongs to the caller. No GUI ownership, pull, or freshness timer.
// Error ownership: bootstrap/fence errors return to Remote's operation;
// retirement retains admitted applies even if its waiter disappears. Tests cover
// stale generations, cancellation and capacity. Every transport/store variant
// at each caller is not separately injected; an incomplete boundary is never
// reported live.
mod entry;
mod readiness;
use super::completion::Wake;
use crate::engine::{
    Result,
    publisher::{BootstrapToken, PublicationLease, PublishKey, PublishScope, Publisher},
    replication::{
        self, PreparedSubscription, ReplicaNotices, ReplicaScope, ReplicaSource, ReplicaState,
    },
};
use crate::network::ap::ApConnection;
use crate::store::{Id, LocalStore};
#[cfg(test)]
use std::os::unix::net::UnixStream;
use std::{
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
};
use tokio::{
    runtime::Handle,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};

pub(super) const SCOPE_LIMIT: usize = 512;
pub(super) const GAP_CAPTURE_LIMIT: u8 = 3;

/// AP's bounded subscription admission applies across every account device.
/// Retain only the classification, within the existing scope-registry budget.
#[derive(Clone, Copy)]
enum SubscriptionLimit {
    Account,
    Source,
    Service,
}
impl SubscriptionLimit {
    fn from_error(error: &str) -> Option<Self> {
        match error {
            "account replica subscription capacity reached" => Some(Self::Account),
            "replica source fanout capacity reached" => Some(Self::Source),
            "replica subscription capacity reached" | "replica buffer capacity reached" => {
                Some(Self::Service)
            }
            _ => None,
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::Account => {
                "Synchronization limit reached across your devices. Your local changes are saved. Reconnect after another device disconnects."
            }
            Self::Source => {
                "This shared data has reached its synchronization limit. Your local changes are saved. Reconnect after another participant disconnects."
            }
            Self::Service => {
                "The synchronization service is full. Your local changes are saved. Reconnect to try again later."
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct LiveGap;
impl std::fmt::Display for LiveGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("replica subscription requires gap recovery")
    }
}
impl std::error::Error for LiveGap {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScopeKey {
    pub(crate) volume: Id,
    pub(crate) entry: Option<crate::store::EntryId>,
    pub(crate) session: Option<([u8; 32], [u8; 32])>,
}
impl ScopeKey {
    fn new(scope: &ReplicaScope) -> Result<Self> {
        let ReplicaScope::Volume { volume, session } = scope else {
            return Err("account subscription requires account bootstrap".into());
        };
        // The publisher validates canonical IDs before they enter fixed storage.
        PublishScope::new(*volume, session.as_ref())?;
        Ok(Self {
            volume: *volume,
            entry: None,
            session: session.as_ref().map(|s| {
                (
                    crate::identifier::decode(&s.id).expect("validated session ID"),
                    crate::identifier::decode(&s.resolved_build_id).expect("validated build ID"),
                )
            }),
        })
    }
    fn matches(&self, volume: Id, session: Option<(&str, &str)>) -> bool {
        self.entry.is_none()
            && self.volume == volume
            && match (self.session.as_ref(), session) {
                (None, None) => true,
                (Some((id, version)), Some((candidate, candidate_version))) => {
                    crate::identifier::decode::<32>(candidate)
                        .is_ok_and(|candidate| *id == candidate)
                        && crate::identifier::decode::<32>(candidate_version)
                            .is_ok_and(|candidate| *version == candidate)
                }
                _ => false,
            }
    }
    fn source(self) -> ReplicaSource {
        match self.entry {
            Some(entry) => ReplicaSource::Entry(crate::store::volume_index::EntryAddress {
                volume: self.volume,
                entry,
            }),
            None => ReplicaSource::Volume(self.volume),
        }
    }
}

struct Receive {
    task: Option<JoinHandle<()>>,
    epoch: [u8; 16],
    previous_epoch: Option<[u8; 16]>,
    control: Option<replication::ReplicaControl>,
}
struct Entry {
    key: ScopeKey,
    subscription_limit: Option<SubscriptionLimit>,
    preparation: Option<u64>,
    publication_generation: Option<u64>,
    publication: Option<PublicationLease>,
    ready: bool,
    // Persists across replacement bootstrap generations until a verified live
    // boundary or an explicit manual retry. A failed bootstrap cannot loop.
    recovery_attempted: bool,
    readiness: Option<readiness::Readiness>,
    receive: Option<Receive>,
}
impl Entry {
    fn invalidate(&mut self) {
        self.ready = false;
        self.publication = None;
        self.preparation = None;
        self.readiness = None;
        if let Some(task) = self.receive.as_ref().and_then(|receive| receive.task.as_ref()) {
            task.abort();
        }
    }
}

/// Dropping a preparation cancels only its publication generation. The receiver
/// starts after final adoption; outgoing edits can drain after the seed is accepted.
pub(crate) struct PreparedLive {
    key: ScopeKey,
    generation: u64,
    scope: ReplicaScope,
    subscription: Option<PreparedSubscription>,
    publication: Option<BootstrapToken>,
    active_publication: Option<PublicationLease>,
    publish_scope: PublishScope,
    connection: Arc<ApConnection>,
}

struct AccountEntry {
    generation: u64,
    subscription_limit: Option<SubscriptionLimit>,
    recovery_attempted: bool,
    ready: bool,
    receive: Option<Receive>,
}

pub(crate) struct PreparedAccount {
    generation: u64,
    connection: Arc<ApConnection>,
    subscription: PreparedSubscription,
}

pub(crate) struct LiveReplicas {
    runtime: Handle,
    store: Arc<LocalStore>,
    completion: Arc<Wake>,
    notices: Arc<ReplicaNotices>,
    apply_slots: Arc<Semaphore>,
    publisher: Arc<Publisher>,
    entries: Vec<Entry>,
    account: Option<AccountEntry>,
    generation: u64,
}
#[derive(Clone, Copy)]
pub(crate) enum RecoveryKind {
    Gap,
    Transport,
}
impl LiveReplicas {
    /// Call only after account binding: the publisher retains the store Arc.
    pub(crate) fn new(
        runtime: &Handle,
        store: Arc<LocalStore>,
        completion: Arc<Wake>,
        notices: Arc<ReplicaNotices>,
        blob_transfers: Arc<crate::engine::blob::BlobTransfers>,
        scope: super::remote::SharedClientScope,
    ) -> Result<Self> {
        let apply_slots = Arc::new(Semaphore::new(2));
        let publisher = Arc::new(Publisher::start(
            runtime,
            store.clone(),
            completion.clone(),
            apply_slots.clone(),
            notices.control_slots(),
            blob_transfers.clone(),
            scope,
        )?);
        *blob_transfers.logs.publisher.lock().expect("bloblog publisher binding poisoned") =
            Arc::downgrade(&publisher);
        Ok(Self {
            runtime: runtime.clone(),
            store,
            completion,
            notices,
            apply_slots,
            publisher,
            entries: Vec::with_capacity(SCOPE_LIMIT),
            account: None,
            generation: 0,
        })
    }

    /// Retained receiver handles, including finished/retiring work; not live health.
    pub(super) fn receiver_owners(&self) -> usize {
        usize::from(
            self.account
                .as_ref()
                .is_some_and(|entry| entry.receive.as_ref().is_some_and(|r| r.task.is_some())),
        ) + self
            .entries
            .iter()
            .filter(|entry| entry.receive.as_ref().is_some_and(|r| r.task.is_some()))
            .count()
    }

    pub(crate) fn account_installed(&self) -> bool {
        self.account.as_ref().is_some_and(|entry| {
            entry.ready
                && self.publisher.account_state().is_some_and(|notice| {
                    notice.generation == entry.generation
                        && matches!(
                            notice.state,
                            crate::engine::publisher::PublishState::Ready
                                | crate::engine::publisher::PublishState::Pending
                        )
                })
                && entry.receive.as_ref().is_some_and(|receive| {
                    self.notices.state(ReplicaSource::Account).is_some_and(|(epoch, state)| {
                        epoch == receive.epoch
                            && matches!(state, ReplicaState::CatchingUp | ReplicaState::Live)
                    })
                })
        })
    }

    async fn retire_account(&mut self) -> Result<()> {
        assert!(self.entries.len() + usize::from(self.account.is_some()) <= SCOPE_LIMIT);
        let Some(entry) = &mut self.account else {
            return Ok(());
        };
        entry.ready = false;
        if let Some(receive) = &mut entry.receive {
            if let Some(task) = &mut receive.task {
                task.abort();
                if let Err(error) = task.await
                    && !error.is_cancelled()
                {
                    return Err(error.into());
                }
                receive.task = None;
            }
            let current = self.notices.state(ReplicaSource::Account).map(|(epoch, _)| epoch);
            if current == Some(receive.epoch) {
                self.notices.wait_inactive(ReplicaSource::Account, receive.epoch).await?;
            } else if current != receive.previous_epoch {
                return Err("account subscription changed before retirement".into());
            }
        }
        entry.receive = None;
        Ok(())
    }

    pub(crate) async fn begin_account(
        &mut self,
        connection: Arc<ApConnection>,
    ) -> Result<PreparedAccount> {
        assert!(self.entries.len() + usize::from(self.account.is_some()) <= SCOPE_LIMIT);
        if self.account.is_none() && self.entries.len() == SCOPE_LIMIT {
            return Err("account subscription source capacity exceeded".into());
        }
        self.retire_account().await?;
        let generation = self.publisher.begin_account().await?;
        let recovery_attempted =
            self.account.as_ref().is_some_and(|entry| entry.recovery_attempted);
        self.account = Some(AccountEntry {
            generation,
            subscription_limit: None,
            recovery_attempted,
            ready: false,
            receive: None,
        });
        let subscription = match replication::subscribe(&connection, ReplicaScope::Account).await {
            Ok(subscription) => subscription,
            Err(error) => {
                self.fail_account(&error.to_string()).await?;
                return Err(error);
            }
        };
        Ok(PreparedAccount { generation, connection, subscription })
    }

    pub(crate) async fn finish_account(&mut self, prepared: PreparedAccount) -> Result<()> {
        if !self
            .account
            .as_ref()
            .is_some_and(|entry| entry.generation == prepared.generation && !entry.ready)
        {
            return Err("account bootstrap generation replaced".into());
        }
        self.publisher.finish_account(prepared.generation, prepared.connection).await?;
        let epoch = prepared.subscription.epoch();
        let previous_epoch = self.notices.state(ReplicaSource::Account).map(|(epoch, _)| epoch);
        let (control, receiver) = prepared.subscription.start(
            self.store.clone(),
            self.completion.clone(),
            self.notices.clone(),
            self.apply_slots.clone(),
        )?;
        let (task, started) = spawn_receiver(&self.runtime, receiver);
        let mut startup = AbortStartup(Some(task.abort_handle()));
        self.account
            .as_mut()
            .expect("account initialization precedes account state transitions")
            .receive =
            Some(Receive { task: Some(task), epoch, previous_epoch, control: Some(control) });
        if let Err(error) = started.await.map_err(|_| "account receiver stopped before startup")? {
            self.retire_account().await?;
            return Err(error.into());
        }
        self.account
            .as_mut()
            .expect("account initialization precedes account state transitions")
            .ready = true;
        startup.0 = None;
        Ok(())
    }

    pub(crate) async fn fail_account(&mut self, error: &str) -> Result<()> {
        let Some(generation) = self.account.as_ref().map(|entry| entry.generation) else {
            return Ok(());
        };
        self.account
            .as_mut()
            .expect("account initialization precedes account state transitions")
            .subscription_limit = SubscriptionLimit::from_error(error);
        self.retire_account().await?;
        self.publisher.fail_account(generation, error).await
    }

    /// Retain the capacity refusal independently of publication-lease cleanup
    /// and drained notices. Only replacing this scope's bootstrap clears it.
    pub(crate) fn bootstrap_failure(&self) -> Option<&'static str> {
        self.account
            .as_ref()
            .and_then(|entry| entry.subscription_limit)
            .or_else(|| self.entries.iter().find_map(|entry| entry.subscription_limit))
            .map(SubscriptionLimit::message)
    }

    pub(crate) fn apply_slots(&self) -> Arc<Semaphore> {
        self.apply_slots.clone()
    }

    fn account_recoverable(&self, kind: RecoveryKind) -> bool {
        use crate::engine::publisher::PublishState;
        let Some(entry) = &self.account else {
            return false;
        };
        if !entry.ready {
            return false;
        }
        let Some(publication) = self.publisher.account_state() else {
            return false;
        };
        if publication.generation != entry.generation {
            return false;
        }
        let Some(receive) = &entry.receive else {
            return false;
        };
        let Some((epoch, incoming)) = self.notices.state(ReplicaSource::Account) else {
            return false;
        };
        if epoch != receive.epoch {
            return false;
        }
        let transport = matches!(kind, RecoveryKind::Transport);
        (matches!(publication.state, PublishState::Ready | PublishState::Pending)
            || (transport && publication.state == PublishState::Disconnected))
            && (matches!(
                incoming,
                ReplicaState::CatchingUp | ReplicaState::Live | ReplicaState::Gap
            ) || (transport && incoming == ReplicaState::Disconnected))
            && (transport || incoming == ReplicaState::Gap)
    }

    pub(crate) fn take_account_recovery(&mut self, kind: RecoveryKind) -> bool {
        if self.account.as_ref().is_none_or(|entry| entry.recovery_attempted)
            || !self.account_recoverable(kind)
        {
            return false;
        }
        self.account
            .as_mut()
            .expect("account initialization precedes account state transitions")
            .recovery_attempted = true;
        true
    }

    pub(crate) fn account_recovery_is_current(&self, kind: RecoveryKind) -> bool {
        self.account.as_ref().is_some_and(|entry| entry.recovery_attempted)
            && self.account_recoverable(kind)
    }

    pub(crate) fn complete_account_recovery(&mut self) -> Result<()> {
        if !self.account_installed()
            || self.account.as_ref().and_then(|entry| entry.receive.as_ref()).is_none_or(
                |receive| {
                    self.notices.state(ReplicaSource::Account)
                        != Some((receive.epoch, ReplicaState::Live))
                },
            )
        {
            return Err("account recovery has not reached its caught-up boundary".into());
        }
        self.account
            .as_mut()
            .expect("account initialization precedes account state transitions")
            .recovery_attempted = false;
        Ok(())
    }

    /// Incoming profile readiness does not require publishing records whose
    /// volume dependencies are still downloading.
    pub(crate) async fn account_receive_fence(&self) -> Result<()> {
        self.account
            .as_ref()
            .and_then(|entry| entry.receive.as_ref())
            .and_then(|receive| receive.control.as_ref())
            .ok_or("account checkpoint unavailable")?
            .checkpoint()
            .await?;
        Ok(())
    }

    pub(crate) async fn account_fence(&self) -> Result<()> {
        let Some(entry) = &self.account else {
            return Ok(());
        };
        if !self.account_installed() {
            return Err(self
                .publisher
                .account_state()
                .and_then(|notice| notice.error)
                .map_or_else(|| "account replica needs recovery".into(), |error| error.to_string())
                .into());
        }
        self.publisher.fence_account().await?;
        entry
            .receive
            .as_ref()
            .and_then(|receive| receive.control.as_ref())
            .ok_or("account checkpoint unavailable")?
            .checkpoint()
            .await?;
        Ok(())
    }

    /// Stable while Remote moves to its worker. Draining does not take ownership
    /// of the lifecycle controller or wait for network work.
    pub(crate) fn publisher(&self) -> Arc<Publisher> {
        self.publisher.clone()
    }

    /// A connection status observation, never a replacement for an explicit
    /// publication/checkpoint fence or a claim about physical presentation.
    pub(crate) fn availability(
        &self,
        feed: crate::store::change_feed::FeedStatus,
        pending: usize,
    ) -> &'static str {
        use crate::engine::publisher::PublishHealth;
        let health = self.publisher.health(feed, pending);
        if health == PublishHealth::Failed {
            return "Synchronization failed; local data is retained";
        }
        let mut catching_up = self.account.is_some() && !self.account_installed();
        let mut disconnected = false;
        if let Some((_, state)) = self.notices.state(ReplicaSource::Account) {
            match state {
                ReplicaState::Failed | ReplicaState::Revoked => {
                    return "Synchronization failed; local data is retained";
                }
                ReplicaState::Disconnected => disconnected = true,
                ReplicaState::CatchingUp | ReplicaState::Gap => catching_up = true,
                ReplicaState::Live => {}
            }
        }
        for entry in &self.entries {
            if self.readiness_exhausted(entry) {
                return "Synchronization failed; local data is retained";
            }
            if !entry.ready
                || self
                    .publisher
                    .state(PublishKey { volume: entry.key.volume, entry: entry.key.entry })
                    .is_none_or(|(generation, _)| entry.publication_generation != Some(generation))
            {
                catching_up = true;
            }
            let Some(receive) = &entry.receive else {
                catching_up = true;
                continue;
            };
            match self.notices.state(entry.key.source()) {
                Some((epoch, ReplicaState::Live)) if epoch == receive.epoch => {}
                Some((epoch, ReplicaState::Revoked)) if epoch == receive.epoch => {
                    return "Access revoked";
                }
                Some((epoch, ReplicaState::Failed)) if epoch == receive.epoch => {
                    return "Synchronization failed; local data is retained";
                }
                Some((epoch, ReplicaState::Disconnected)) if epoch == receive.epoch => {
                    disconnected = true
                }
                _ => catching_up = true,
            }
        }
        if disconnected || health == PublishHealth::Disconnected {
            return "Disconnected; local data is retained";
        }
        if catching_up || health == PublishHealth::Recovering {
            return "Catching up";
        }
        if health == PublishHealth::Pending {
            return "Pending synchronization";
        }
        if matches!(health, PublishHealth::Private) {
            "Connected; private data requires an authorized peer"
        } else {
            "Connected"
        }
    }

    fn reserve(&mut self, key: ScopeKey) -> Result<usize> {
        assert!(self.entries.len() + usize::from(self.account.is_some()) <= SCOPE_LIMIT);
        if let Some(index) =
            self.entries.iter().position(|e| e.key.volume == key.volume && e.key.entry == key.entry)
        {
            return Ok(index);
        }
        if self.entries.len() + usize::from(self.account.is_some()) == SCOPE_LIMIT {
            return Err("live replica scope capacity exceeded".into());
        }
        self.entries.push(Entry {
            key,
            subscription_limit: None,
            preparation: None,
            publication_generation: None,
            publication: None,
            ready: false,
            recovery_attempted: false,
            readiness: None,
            receive: None,
        });
        Ok(self.entries.len() - 1)
    }

    /// No mutex is held while joining or waiting for a durable blocking apply.
    /// Both the task and its epoch remain in the entry if this future is canceled.
    async fn retire(&mut self, index: usize) -> Result<()> {
        assert!(self.entries.len() + usize::from(self.account.is_some()) <= SCOPE_LIMIT);
        self.entries[index].ready = false;
        self.entries[index].publication = None;
        let Some(receive) = self.entries[index].receive.as_mut() else {
            return Ok(());
        };
        if let Some(task) = receive.task.as_mut() {
            task.abort();
            let result = task.await;
            receive.task = None;
            if let Err(error) = result
                && !error.is_cancelled()
            {
                return Err(error.into());
            }
        }
        let current = self.notices.state(self.entries[index].key.source()).map(|(epoch, _)| epoch);
        let receive = self.entries[index]
            .receive
            .as_ref()
            .expect("the checked live entry has a receive subscription");
        if current == Some(receive.epoch) {
            self.notices.wait_inactive(self.entries[index].key.source(), receive.epoch).await?;
        } else if current != receive.previous_epoch {
            return Err("replica subscription changed before retirement".into());
        }
        // An unchanged prior epoch means cancellation happened before registration.
        self.entries[index].receive = None;
        Ok(())
    }

    /// Retire the old receiver and publisher before reconciling offline writes.
    /// No feed credits are retained during initial admission. The caller rebases
    /// immediately before its seed snapshot, then subscribes before final pull.
    pub(crate) async fn begin(
        &mut self,
        connection: Arc<ApConnection>,
        scope: ReplicaScope,
    ) -> Result<PreparedLive> {
        let key = ScopeKey::new(&scope)?;
        let ReplicaScope::Volume { volume, session } = &scope else { unreachable!() };
        self.store.require_replication_supported(*volume)?;
        if let Some(session) = session
            && self.store.is_local_session(&session.id)?
            && session.shared_volume.is_none()
        {
            return Err("automatic session synchronization is not supported yet".into());
        }
        let publish_scope = PublishScope::new(*volume, session.as_ref())?;
        let index = self.reserve(key)?;
        self.entries[index].subscription_limit = None;
        self.entries[index].preparation = None;
        self.retire(index).await?;
        self.generation =
            self.generation.checked_add(1).ok_or("live replica generation exhausted")?;
        let generation = self.generation;
        if self.entries[index].key != key {
            self.entries[index].readiness = None;
        }
        self.entries[index].key = key;
        self.entries[index].preparation = Some(generation);
        self.entries[index].publication_generation = None;
        self.publisher.forget_scope((*volume).into()).await?;
        Ok(PreparedLive {
            key,
            generation,
            scope,
            subscription: None,
            publication: None,
            active_publication: None,
            publish_scope,
            connection,
        })
    }

    /// Discard the durable prefix immediately before the caller captures its
    /// outbound snapshot. Initial admission may take arbitrarily long; it must
    /// not consume the bounded queue reserved for changes after that snapshot.
    pub(crate) async fn rebase(&mut self, prepared: &mut PreparedLive) -> Result<()> {
        let index = self
            .entries
            .iter()
            .position(|entry| {
                entry.key == prepared.key && entry.preparation == Some(prepared.generation)
            })
            .ok_or("live replica preparation was replaced")?;
        if prepared.active_publication.is_some() {
            return Err("cannot rebase an activated replica preparation".into());
        }
        let publication = self.publisher.begin_bootstrap(prepared.key.volume).await?;
        self.entries[index].publication_generation = Some(publication.generation());
        prepared.publication = Some(publication);
        crate::trace::mark("replication.bootstrap_rebased");
        Ok(())
    }

    /// After the final receive capture and a fresh outbound seed, activate
    /// publication on the installed subscription. Publishing during capture
    /// would invalidate the shared source cut under continuous local input.
    pub(crate) async fn activate(&self, prepared: &mut PreparedLive) -> Result<()> {
        if !self.entries.iter().any(|entry| {
            entry.key == prepared.key && entry.preparation == Some(prepared.generation)
        }) {
            return Err("live replica preparation was replaced".into());
        }
        if prepared.subscription.is_none() {
            return Err("live replica must subscribe before publication activation".into());
        }
        let publication =
            prepared.publication.take().ok_or("live replica publication already activated")?;
        prepared.active_publication = Some(
            self.publisher
                .finish_bootstrap(
                    publication,
                    prepared.publish_scope.clone(),
                    prepared.connection.clone(),
                )
                .await?,
        );
        crate::trace::mark("replication.publication_activated");
        Ok(())
    }

    /// This must precede the final whole-volume pull/adoption. Changes after
    /// that boundary queue on this actual AP stream until finish starts apply.
    pub(crate) async fn subscribe(&mut self, prepared: &mut PreparedLive) -> Result<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| {
                entry.key == prepared.key && entry.preparation == Some(prepared.generation)
            })
            .ok_or("live replica preparation was replaced")?;
        if prepared.subscription.is_some() {
            return Err("live replica already subscribed".into());
        }
        match replication::subscribe(&prepared.connection, prepared.scope.clone()).await {
            Ok(subscription) => prepared.subscription = Some(subscription),
            Err(error) => {
                entry.subscription_limit = SubscriptionLimit::from_error(&error.to_string());
                return Err(error);
            }
        }
        Ok(())
    }

    /// The caller has now durably adopted the whole bootstrap. CatchingUp remains
    /// distinct from Live until the actual ordered AP checkpoint is received.
    pub(crate) async fn finish(&mut self, mut prepared: PreparedLive) -> Result<()> {
        let index = self
            .entries
            .iter()
            .position(|e| e.key == prepared.key && e.preparation == Some(prepared.generation))
            .ok_or("live replica preparation was replaced")?;
        if self.entries[index].receive.is_some() {
            return Err("live replica preparation already started".into());
        }
        if prepared.active_publication.is_none() {
            return Err("live replica publication must be activated before final bootstrap".into());
        }
        let subscription = prepared
            .subscription
            .take()
            .ok_or("live replica must subscribe before final bootstrap")?;
        let epoch = subscription.epoch();
        let previous_epoch = self.notices.state(prepared.key.source()).map(|(epoch, _)| epoch);
        let (control, receiver) = subscription.start(
            self.store.clone(),
            self.completion.clone(),
            self.notices.clone(),
            self.apply_slots.clone(),
        )?;
        let (task, started) = spawn_receiver(&self.runtime, receiver);
        let mut startup = AbortStartup(Some(task.abort_handle()));
        self.entries[index].receive =
            Some(Receive { task: Some(task), epoch, previous_epoch, control: Some(control) });
        let started = started.await.map_err(|_| "replica receive stopped before startup")?;
        if let Err(error) = started
            && self.notices.state(prepared.key.source()) != Some((epoch, ReplicaState::Gap))
        {
            self.retire(index).await?;
            return Err(error.into());
        }
        // A queued AP Gap can finish the first poll. Keep the installed
        // epoch and publication ownership so the bounded bootstrap caller
        // can distinguish this from cancellation or a failed publisher.
        if !self.publisher.state(prepared.key.volume).is_some_and(|(generation, state)| {
            self.entries[index].publication_generation == Some(generation)
                && matches!(
                    state,
                    crate::engine::publisher::PublishState::Ready
                        | crate::engine::publisher::PublishState::Pending
                )
        }) {
            self.retire(index).await?;
            return Err("live publication lost its bootstrap boundary; recovery required".into());
        }
        self.entries[index].publication = prepared.active_publication.take();
        self.entries[index].preparation = None;
        self.entries[index].ready = true;
        startup.0 = None;
        Ok(())
    }

    /// The service is not authorized to read everything this scope's reader
    /// can, so the caller abandons its preparation deliberately. Hand the
    /// publication generation back before the token drops: a dropped bootstrap
    /// token means a lost owner and fails the whole account's publication
    /// health. Removing the entry also keeps the scope out of availability.
    pub(crate) async fn abandon(&mut self, mut prepared: PreparedLive) -> Result<()> {
        let token = prepared
            .publication
            .take()
            .ok_or("an activated replica preparation cannot be abandoned")?;
        self.publisher.abandon_bootstrap(token).await?;
        let index = self
            .entries
            .iter()
            .position(|entry| {
                entry.key == prepared.key && entry.preparation == Some(prepared.generation)
            })
            .ok_or("live replica preparation was replaced")?;
        self.entries[index].preparation = None;
        self.retire(index).await?;
        self.entries.swap_remove(index);
        Ok(())
    }

    /// Catching up is installed work, not a reason to replace the subscription.
    /// Current scope selection must still check authorization before using this.
    pub(crate) fn is_installed(&self, volume: Id, session: Option<(&str, &str)>) -> bool {
        self.installed_scope(volume).is_some_and(|key| key.matches(volume, session))
    }

    pub(crate) fn installed_scope(&self, volume: Id) -> Option<ScopeKey> {
        self.entries
            .iter()
            .find(|entry| {
                entry.ready
                    && entry.key.entry.is_none()
                    && entry.key.volume == volume
                    && self.publisher.state(volume).is_some_and(|(generation, state)| {
                        entry.publication_generation == Some(generation)
                            && matches!(
                                state,
                                crate::engine::publisher::PublishState::Ready
                                    | crate::engine::publisher::PublishState::Pending
                            )
                    })
                    && entry.receive.as_ref().is_some_and(|receive| {
                        self.notices.state(ReplicaSource::Volume(volume)).is_some_and(
                            |(epoch, state)| {
                                epoch == receive.epoch
                                    && matches!(
                                        state,
                                        ReplicaState::CatchingUp | ReplicaState::Live
                                    )
                            },
                        )
                    })
            })
            .map(|entry| entry.key)
    }

    fn recoverable(&self, entry: &Entry, kind: RecoveryKind) -> bool {
        if entry.key.entry.is_some() && entry.publication_generation.is_none() {
            return read_recovery_eligible(
                entry.ready,
                entry.receive.as_ref().map(|receive| receive.epoch),
                self.notices.state(entry.key.source()),
                kind,
            );
        }
        recovery_eligible(
            entry.ready,
            entry.publication_generation,
            entry.receive.as_ref().map(|receive| receive.epoch),
            self.publisher.state(PublishKey { volume: entry.key.volume, entry: entry.key.entry }),
            self.notices.state(entry.key.source()),
            kind,
        )
    }

    /// Owner-side observation only: fixed registry scan, no allocation or I/O.
    pub(crate) fn needs_gap_recovery(&self) -> bool {
        (self.account.as_ref().is_some_and(|entry| !entry.recovery_attempted)
            && self.account_recoverable(RecoveryKind::Gap))
            || self.entries.iter().any(|entry| {
                !entry.recovery_attempted && self.recoverable(entry, RecoveryKind::Gap)
            })
    }

    /// Select once before retiring either side. Canceling the old publication
    /// subsequently reports a terminal cancellation, not a new recovery cause.
    /// The existing single network worker owns this bounded plan (at most32).
    pub(crate) fn take_recoveries(&mut self, kind: RecoveryKind) -> Vec<ScopeKey> {
        let mut scopes = Vec::with_capacity(self.entries.len());
        for index in 0..self.entries.len() {
            if !self.entries[index].recovery_attempted
                && self.recoverable(&self.entries[index], kind)
            {
                self.entries[index].recovery_attempted = true;
                scopes.push(self.entries[index].key);
            }
        }
        scopes
    }

    /// Earlier scopes in the same worker plan may take time. Recheck this
    /// scope's original live observations immediately before its own retirement.
    pub(crate) fn recovery_is_current(&self, scope: &ScopeKey, kind: RecoveryKind) -> bool {
        self.entries.iter().any(|entry| {
            entry.key == *scope && entry.recovery_attempted && self.recoverable(entry, kind)
        })
    }

    /// Caller has fenced the replacement. Clear the attempt only when that
    /// exact scope is installed, publication is current, and receive is Live.
    pub(crate) fn complete_recovery(&mut self, scope: &ScopeKey) -> Result<()> {
        use crate::engine::publisher::PublishState;
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.key == *scope)
            .ok_or("live recovery scope was replaced")?;
        let publication_ready = self
            .publisher
            .state(PublishKey { volume: scope.volume, entry: scope.entry })
            .is_some_and(|(generation, state)| {
                entry.publication_generation == Some(generation)
                    && matches!(state, PublishState::Ready | PublishState::Pending)
            });
        let receive_live = entry.receive.as_ref().is_some_and(|receive| {
            self.notices.state(scope.source()) == Some((receive.epoch, ReplicaState::Live))
        });
        if !entry.ready || !publication_ready || !receive_live {
            if entry.ready
                && publication_ready
                && entry.receive.as_ref().is_some_and(|receive| {
                    self.notices.state(scope.source()) == Some((receive.epoch, ReplicaState::Gap))
                })
            {
                return Err(LiveGap.into());
            }
            return Err("live recovery has not reached its current caught-up boundary".into());
        }
        entry.recovery_attempted = false;
        entry.readiness = None;
        Ok(())
    }

    /// Only an explicit manual synchronization may retry a failed episode.
    pub(crate) fn reset_recovery_attempts(&mut self) {
        if let Some(entry) = &mut self.account {
            entry.recovery_attempted = false;
        }
        for entry in &mut self.entries {
            entry.recovery_attempted = false;
            entry.readiness = None;
        }
    }

    /// Fence only the generations just acknowledged by the publisher. Each
    /// ordered checkpoint includes preceding durable incoming applies, not the
    /// presentation time on another device or data excluded from service access.
    pub(crate) async fn checkpoint_pending_changes(&self) -> Result<()> {
        let volumes: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.ready)
            .map(|entry| PublishKey { volume: entry.key.volume, entry: entry.key.entry })
            .collect();
        assert!(volumes.len() <= SCOPE_LIMIT);
        let fenced = self.publisher.fence_scopes(&volumes).await?;
        self.checkpoint(&fenced).await
    }

    /// Ordered receive barrier for the generations returned by the publisher.
    pub(crate) async fn checkpoint(
        &self,
        volumes: &[crate::engine::publisher::FencedVolume],
    ) -> Result<()> {
        for volume in volumes {
            let entry = self
                .entries
                .iter()
                .find(|entry| {
                    entry.key.volume == volume.volume.volume
                        && entry.key.entry == volume.volume.entry
                        && entry.ready
                        && entry.publication_generation == Some(volume.generation)
                })
                .ok_or("live replica changed during synchronization barrier")?;
            let receive = entry.receive.as_ref().ok_or("live replica receiver is not installed")?;
            let control =
                receive.control.as_ref().ok_or("live replica checkpoint is unavailable")?;
            let result = control.checkpoint().await;
            if self.notices.state(entry.key.source()) == Some((receive.epoch, ReplicaState::Gap)) {
                return Err(LiveGap.into());
            }
            result?;
            if self.notices.state(entry.key.source()) != Some((receive.epoch, ReplicaState::Live)) {
                return Err("live replica lost its caught-up boundary".into());
            }
        }
        Ok(())
    }

    /// A delayed notice from a replaced publisher must not cancel its successor.
    pub(crate) fn invalidate_publication(
        &mut self,
        volume: impl Into<PublishKey>,
        generation: u64,
    ) {
        let volume = volume.into();
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.key.volume == volume.volume
                && entry.key.entry == volume.entry
                && entry.publication_generation == Some(generation)
        }) {
            entry.invalidate();
        }
    }

    /// Incoming state is tagged by its subscription epoch, independently of the
    /// publisher's local preparation generation.
    #[cfg(test)]
    pub(crate) fn invalidate_receive(&mut self, volume: Id, epoch: [u8; 16]) {
        if self.entries.iter().any(|entry| {
            entry.key.volume == volume
                && entry.receive.as_ref().is_some_and(|receive| receive.epoch == epoch)
        }) {
            self.invalidate(volume);
        }
    }

    pub(crate) fn exhaust_recovery(&mut self, scope: &ScopeKey) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == *scope) else {
            return;
        };
        let Some(receive) = &entry.receive else {
            return;
        };
        if self.notices.fail_gap(scope.source(), receive.epoch) {
            self.completion.notify();
        }
        entry.invalidate();
    }

    /// Immediate cancellation feedback. Later begin/forget awaits exact durable
    /// retirement; abort never means an admitted write was rolled back.
    pub(crate) fn disconnect(&mut self) -> Result<()> {
        if let Some(entry) = &mut self.account {
            entry.ready = false;
            if let Some(task) = entry.receive.as_ref().and_then(|r| r.task.as_ref()) {
                task.abort();
            }
        }
        for entry in &mut self.entries {
            entry.ready = false;
            entry.readiness = None;
            // Release rather than drop: this generation ends with the connection,
            // not as an owner that vanished, so it stays eligible for bootstrap.
            if let Some(publication) = entry.publication.take() {
                publication.disconnect();
            }
            entry.preparation = None;
            if let Some(task) = entry.receive.as_ref().and_then(|r| r.task.as_ref()) {
                task.abort();
            }
        }
        self.publisher.disconnect()
    }

    /// Apply a publisher recovery notice before deciding whether to bootstrap.
    /// Engine may retain that notice while Remote is temporarily owned by a worker.
    #[cfg(test)]
    pub(crate) fn invalidate(&mut self, volume: Id) {
        for entry in self.entries.iter_mut().filter(|entry| entry.key.volume == volume) {
            entry.invalidate();
        }
    }

    /// The provider consumes dirty notifications through store catalog fences.
    /// An explicit copy may initialize one source entry, then retire it before
    /// admitting another source on the serial network lane.
    #[cfg(feature = "file-provider")]
    pub(crate) async fn finish_provider_copy(
        &mut self,
        address: crate::store::volume_index::EntryAddress,
    ) -> Result<()> {
        let Some(index) = self.entries.iter().position(|entry| {
            entry.key.volume == address.volume && entry.key.entry == Some(address.entry)
        }) else {
            return Ok(());
        };
        self.entries[index].preparation = None;
        self.retire(index).await?;
        self.publisher
            .forget_scope(crate::engine::publisher::PublishKey {
                volume: address.volume,
                entry: Some(address.entry),
            })
            .await?;
        // No provider renderer consumes these notices. Durable store fences
        // still expose every applied change to the next metadata snapshot.
        self.notices.drain();
        let source = ReplicaSource::Entry(address);
        if let Some((epoch, _)) = self.notices.state(source)
            && !self.notices.forget(source, epoch)
        {
            return Err("database copy source still active during retirement".into());
        }
        self.entries.swap_remove(index);
        Ok(())
    }

    /// Provider notifications are projected from durable store fences. Retire
    /// every excluded controller before consuming its final dirty notice; an
    /// admitted blocking apply may complete after its receiver was canceled.
    #[cfg(any(test, feature = "file-provider"))]
    pub(crate) async fn retain_provider_volumes(&mut self, selected: &[Id]) -> Result<()> {
        assert!(selected.len() <= super::remote::PROVIDER_VOLUME_ADMISSION);
        let mut index = 0;
        while index < self.entries.len() {
            let key = self.entries[index].key;
            if selected.contains(&key.volume) {
                index += 1;
                continue;
            }
            self.entries[index].preparation = None;
            self.retire(index).await?;
            self.publisher
                .forget_scope(PublishKey { volume: key.volume, entry: key.entry })
                .await?;
            self.notices.drain();
            if let Some((epoch, _)) = self.notices.state(key.source())
                && !self.notices.forget(key.source(), epoch)
            {
                return Err("provider replica still active during retirement".into());
            }
            self.entries.swap_remove(index);
        }
        // Failed bootstraps may retain account-ready publisher placeholders
        // without an installed controller. They share the same fixed capacity.
        for key in self.publisher.retained_scopes() {
            if !selected.contains(&key.volume) {
                self.publisher.forget_scope(key).await?;
            }
        }
        Ok(())
    }
}
impl Drop for LiveReplicas {
    fn drop(&mut self) {
        self.publisher.shutdown();
        if let Some(task) = self
            .account
            .as_ref()
            .and_then(|entry| entry.receive.as_ref())
            .and_then(|r| r.task.as_ref())
        {
            task.abort();
        }
        for entry in &self.entries {
            if let Some(task) = entry.receive.as_ref().and_then(|r| r.task.as_ref()) {
                task.abort();
            }
        }
    }
}

/// Dropping a partially awaited finish must not leave a receive running behind
/// an unready controller entry. The entry still owns its join/retirement state.
struct AbortStartup(Option<tokio::task::AbortHandle>);
impl Drop for AbortStartup {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

/// One startup result per installed task, bounded by the scope registry. Poll
/// once before acknowledging so register errors cannot vanish in a detached task.
fn spawn_receiver(
    runtime: &Handle,
    receiver: impl Future<Output = Result<()>> + Send + 'static,
) -> (JoinHandle<()>, oneshot::Receiver<std::result::Result<(), String>>) {
    let (send, started) = oneshot::channel();
    let task = runtime.spawn(async move {
        let mut receiver = std::pin::pin!(receiver);
        let first = poll_fn(|cx| {
            Poll::Ready(match receiver.as_mut().poll(cx) {
                Poll::Pending => None,
                Poll::Ready(result) => Some(result),
            })
        })
        .await;
        if let Some(result) = first {
            let error = match result {
                Err(error) => startup_error(&error),
                Ok(()) => "replica receive ended during startup".into(),
            };
            let _ = send.send(Err(error));
            return;
        }
        if send.send(Ok(())).is_err() {
            return;
        }
        // After registration, the receiver's bounded notices carry terminal errors.
        let _ = receiver.await;
    });
    (task, started)
}

fn startup_error(error: &dyn std::fmt::Display) -> String {
    struct Bounded(String);
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            let mut length = value.len().min(512 - self.0.len());
            while !value.is_char_boundary(length) {
                length -= 1;
            }
            self.0.push_str(&value[..length]);
            if length == value.len() { Ok(()) } else { Err(std::fmt::Error) }
        }
    }
    let mut out = Bounded(String::with_capacity(512));
    let _ = std::fmt::write(&mut out, format_args!("{error}"));
    out.0
}

/// Whether an installed volume scope may start `kind` recovery, given the
/// publisher's and the replica notices' current state for it. Only the
/// generation and epoch the entry installed count.
fn read_recovery_eligible(
    ready: bool,
    receive: Option<[u8; 16]>,
    incoming: Option<([u8; 16], ReplicaState)>,
    kind: RecoveryKind,
) -> bool {
    ready
        && incoming.is_some_and(|(epoch, state)| {
            receive == Some(epoch)
                && match kind {
                    RecoveryKind::Gap => state == ReplicaState::Gap,
                    RecoveryKind::Transport => {
                        matches!(
                            state,
                            ReplicaState::CatchingUp
                                | ReplicaState::Live
                                | ReplicaState::Gap
                                | ReplicaState::Disconnected
                        )
                    }
                }
        })
}

fn recovery_eligible(
    ready: bool,
    publication_generation: Option<u64>,
    receive_epoch: Option<[u8; 16]>,
    publication: Option<(u64, crate::engine::publisher::PublishState)>,
    incoming: Option<([u8; 16], ReplicaState)>,
    kind: RecoveryKind,
) -> bool {
    use crate::engine::publisher::PublishState;
    let (true, Some((generation, publication)), Some(receive_epoch), Some((epoch, incoming))) =
        (ready, publication, receive_epoch, incoming)
    else {
        return false;
    };
    if publication_generation != Some(generation) || epoch != receive_epoch {
        return false;
    }
    // A lost account connection never overrides a terminal scope failure.
    let transport = matches!(kind, RecoveryKind::Transport);
    (matches!(
        publication,
        PublishState::Ready | PublishState::Pending | PublishState::NeedsRecovery
    ) || (transport && publication == PublishState::Disconnected))
        && (matches!(incoming, ReplicaState::CatchingUp | ReplicaState::Live | ReplicaState::Gap)
            || (transport && incoming == ReplicaState::Disconnected))
        && (transport
            || publication == PublishState::NeedsRecovery
            || incoming == ReplicaState::Gap)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
pub(crate) mod tests {
    use super::*;
    #[cfg(feature = "file-provider")]
    use crate::engine::publisher::PublishState;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    fn test_id(value: usize) -> Id {
        let mut bytes = [value as u8; 16];
        if value > 255 {
            bytes[0] = 0;
            bytes[13..15].copy_from_slice(&(value as u16).to_le_bytes());
        }
        Id::from_bytes(bytes)
    }
    fn key(byte: usize) -> ScopeKey {
        ScopeKey { entry: None, volume: test_id(byte), session: None }
    }

    #[test]
    fn selected_failure_keeps_siblings_and_catalog_and_shares_the_registry_bound() {
        let runtime = runtime();
        let (_directory, mut live, _wake) = controller(runtime.handle());
        let full = key(4);
        let first =
            ScopeKey { entry: Some(crate::store::EntryId::from_bytes([1; 15]).unwrap()), ..full };
        let second =
            ScopeKey { entry: Some(crate::store::EntryId::from_bytes([2; 15]).unwrap()), ..full };
        let catalog = live.reserve(full).unwrap();
        live.entries[catalog].ready = true;
        live.entries[catalog].publication_generation = Some(9);
        for (scope, epoch, state) in
            [(first, [1; 16], ReplicaState::Gap), (second, [2; 16], ReplicaState::Live)]
        {
            let index = live.reserve(scope).unwrap();
            live.entries[index].ready = true;
            live.entries[index].receive =
                Some(Receive { task: None, epoch, previous_epoch: None, control: None });
            live.notices.test_state(scope.source(), epoch, state);
        }
        live.exhaust_recovery(&first);
        let selected = live.reserve(first).unwrap();
        assert!(!live.entries[selected].ready);
        assert!(live.entries[catalog].ready);
        let sibling = live.reserve(second).unwrap();
        assert!(live.entries[sibling].ready);
        live.invalidate_publication(full.volume, 9);
        assert!(!live.entries[catalog].ready);
        assert!(live.entries[sibling].ready);
        for byte in 3..SCOPE_LIMIT {
            live.reserve(ScopeKey {
                entry: Some(
                    crate::store::EntryId::from_bytes(
                        test_id(byte).as_bytes()[..15].try_into().unwrap(),
                    )
                    .unwrap(),
                ),
                ..full
            })
            .unwrap();
        }
        assert_eq!(live.entries.len(), SCOPE_LIMIT);
        assert!(live.reserve(key(5)).is_err());
    }
    fn controller(runtime: &Handle) -> (tempfile::TempDir, LiveReplicas, UnixStream) {
        controller_with_scope(runtime, Default::default())
    }
    fn controller_with_scope(
        runtime: &Handle,
        scope: super::super::remote::SharedClientScope,
    ) -> (tempfile::TempDir, LiveReplicas, UnixStream) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let (completion, reader) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let live = LiveReplicas::new(
            runtime,
            store,
            Arc::new(Wake::unix(completion)),
            Arc::new(ReplicaNotices::new()),
            Arc::new(crate::engine::blob::BlobTransfers::default()),
            scope,
        )
        .unwrap();
        (directory, live, reader)
    }

    #[cfg(feature = "file-provider")]
    #[test]
    fn provider_selection_joins_late_dirty_apply_and_retries_canceled_retirement() {
        // The old receiver is canceled while an apply still owns its slot.
        // Cancel one cleanup attempt, finish the dirty apply and a local write,
        // then retry from retained controllers with the selection already changed.
        runtime().block_on(async {
            let scope = Arc::new(parking_lot::RwLock::new(
                super::super::remote::ClientScope::Provider(vec![]),
            ));
            let (_directory, mut live, _reader) =
                controller_with_scope(&Handle::current(), scope.clone());
            let store = live.store.clone();
            let first = store.create_volume("First", true).unwrap();
            let second = store.create_volume("Second", true).unwrap();
            *scope.write() = super::super::remote::ClientScope::Provider(vec![first.id]);
            let token = live.publisher.begin_bootstrap(first.id).await.unwrap();
            live.publisher.abandon_bootstrap(token).await.unwrap();
            let key = ScopeKey { volume: first.id, entry: None, session: None };
            let epoch = [7; 16];
            let finish_apply =
                live.notices.test_held_apply(key.source(), epoch, live.completion.clone());
            let index = live.reserve(key).unwrap();
            live.entries[index].receive =
                Some(Receive { task: None, epoch, previous_epoch: None, control: None });
            *scope.write() = super::super::remote::ClientScope::Provider(vec![second.id]);
            let selected = [second.id];
            let mut retirement = Box::pin(live.retain_provider_volumes(&selected));
            std::future::poll_fn(|cx| {
                assert!(retirement.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(retirement);
            store.provider_enqueue_publication(first.id).unwrap();
            let file = store
                .create_blob(
                    &crate::store::Access::owner(store.account().id),
                    first.id,
                    "late.bin",
                    b"retained",
                )
                .unwrap();
            finish_apply();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                live.retain_provider_volumes(&selected),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(live.entries.is_empty());
            assert!(live.notices.state(key.source()).is_none());
            assert!(live.publisher.retained_scopes().is_empty());
            assert!(
                store
                    .provider_pending_publications()
                    .unwrap()
                    .iter()
                    .any(|(id, _)| *id == first.id)
            );
            assert_eq!(
                store
                    .entry(&crate::store::Access::owner(store.account().id), first.id, file.id)
                    .unwrap()
                    .info
                    .name,
                "late.bin"
            );
        });
    }

    #[cfg(feature = "file-provider")]
    #[test]
    fn provider_selection_reclaims_publisher_placeholders_across_repeated_switches() {
        // An unavailable bootstrap can leave only a publisher placeholder.
        // More switches than the registry capacity, including same-selection
        // retries after cleanup, must release the actual retained keys.
        runtime().block_on(async {
            let scope = Arc::new(parking_lot::RwLock::new(
                super::super::remote::ClientScope::Provider(vec![]),
            ));
            let (_directory, mut live, _reader) =
                controller_with_scope(&Handle::current(), scope.clone());
            let store = live.store.clone();
            let first = store.create_volume("First", true).unwrap();
            let second = store.create_volume("Second", true).unwrap();
            for iteration in 0..64 {
                let (old, next) =
                    if iteration % 2 == 0 { (first.id, second.id) } else { (second.id, first.id) };
                *scope.write() = super::super::remote::ClientScope::Provider(vec![old]);
                let token = live.publisher.begin_bootstrap(old).await.unwrap();
                live.publisher.abandon_bootstrap(token).await.unwrap();
                let token = live
                    .publisher
                    .begin_bootstrap(PublishKey {
                        volume: old,
                        entry: Some(crate::store::EntryId::from_bytes([4; 15]).unwrap()),
                    })
                    .await
                    .unwrap();
                live.publisher.abandon_bootstrap(token).await.unwrap();
                assert_eq!(live.publisher.retained_scopes().len(), 2);
                *scope.write() = super::super::remote::ClientScope::Provider(vec![next]);
                store.provider_enqueue_publication(old).unwrap();
                live.retain_provider_volumes(&[next]).await.unwrap();
                live.retain_provider_volumes(&[next]).await.unwrap();
                assert!(live.entries.is_empty());
                assert!(live.publisher.retained_scopes().is_empty());
                assert!(live.publisher.drain_notices().error.is_none());
            }
            assert_eq!(store.provider_pending_publications().unwrap().len(), 2);
        });
    }

    #[cfg(feature = "file-provider")]
    #[test]
    fn provider_copy_sources_release_scope_and_notice_capacity_after_failed_bootstrap() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (_directory, mut live, _reader) = controller(runtime.handle());
            let destination = key(1);
            observed_entry(
                &mut live,
                destination,
                1,
                [1; 16],
                PublishState::Ready,
                ReplicaState::Live,
            );
            for byte in 2..66 {
                let address = crate::store::volume_index::EntryAddress {
                    volume: key(2).volume,
                    entry: crate::store::EntryId::from_bytes(
                        test_id(byte).as_bytes()[..15].try_into().unwrap(),
                    )
                    .unwrap(),
                };
                let source =
                    ScopeKey { volume: address.volume, entry: Some(address.entry), session: None };
                let token = live
                    .publisher
                    .begin_bootstrap(PublishKey {
                        volume: address.volume,
                        entry: Some(address.entry),
                    })
                    .await
                    .unwrap();
                let generation = token.generation();
                live.publisher.abandon_bootstrap(token).await.unwrap();
                observed_entry(
                    &mut live,
                    source,
                    generation,
                    [byte as u8; 16],
                    PublishState::Excluded,
                    ReplicaState::Live,
                );
                live.finish_provider_copy(address).await.unwrap();
                assert_eq!(live.entries.len(), 1);
                assert_eq!(live.entries[0].key, destination);
                assert!(live.notices.state(ReplicaSource::Entry(address)).is_none());
            }
        });
    }

    pub(crate) fn observed_entry(
        live: &mut LiveReplicas,
        scope: ScopeKey,
        generation: u64,
        epoch: [u8; 16],
        publication: crate::engine::publisher::PublishState,
        incoming: ReplicaState,
    ) -> usize {
        let index = live.reserve(scope).unwrap();
        let entry = &mut live.entries[index];
        entry.key = scope;
        entry.ready = true;
        entry.publication_generation = Some(generation);
        entry.receive = Some(Receive { task: None, epoch, previous_epoch: None, control: None });
        live.publisher.test_state(
            PublishKey { volume: scope.volume, entry: scope.entry },
            generation,
            publication,
        );
        live.notices.test_state(scope.source(), epoch, incoming);
        index
    }

    fn observed_account(
        live: &mut LiveReplicas,
        publication: crate::engine::publisher::PublishState,
        incoming: ReplicaState,
    ) {
        live.account = Some(AccountEntry {
            generation: 7,
            subscription_limit: None,
            ready: true,
            recovery_attempted: false,
            receive: Some(Receive {
                task: None,
                epoch: [9; 16],
                previous_epoch: None,
                control: None,
            }),
        });
        live.publisher.test_account_state(7, publication);
        live.notices.test_state(ReplicaSource::Account, [9; 16], incoming);
    }

    #[test]
    fn receiver_owner_samples_include_finished_and_canceled_handles_until_retired() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        observed_account(&mut live, PublishState::Ready, ReplicaState::Live);
        live.account.as_mut().unwrap().receive.as_mut().unwrap().task =
            Some(runtime.spawn(async {}));
        for byte in 1..SCOPE_LIMIT {
            let index = observed_entry(
                &mut live,
                key(byte),
                byte as u64,
                [byte as u8; 16],
                PublishState::Ready,
                ReplicaState::Live,
            );
            live.entries[index].receive.as_mut().unwrap().task = Some(runtime.spawn(async {
                std::future::pending::<()>().await;
            }));
        }
        runtime.block_on(tokio::task::yield_now());
        assert!(
            live.account
                .as_ref()
                .unwrap()
                .receive
                .as_ref()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .is_finished()
        );
        assert_eq!(live.receiver_owners(), SCOPE_LIMIT);
        assert!(live.reserve(key(SCOPE_LIMIT + 1)).is_err());
        live.disconnect().unwrap();
        assert_eq!(live.receiver_owners(), SCOPE_LIMIT, "abort does not release the owned handle");
        runtime.block_on(async {
            live.retire_account().await.unwrap();
            for index in 0..live.entries.len() {
                live.retire(index).await.unwrap();
            }
        });
        assert_eq!(live.receiver_owners(), 0);
    }

    #[test]
    fn installed_scope_retains_exact_stream_authority_and_rejects_unhealthy_state() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let id = crate::identifier::encode(&[0xaa; 32]);
        let original = crate::identifier::encode(&[0xbb; 32]);
        let updated = crate::identifier::encode(&[0xcc; 32]);
        let scope = ScopeKey {
            entry: None,
            volume: key(1).volume,
            session: Some((
                crate::identifier::decode(&id).unwrap(),
                crate::identifier::decode(&original).unwrap(),
            )),
        };
        observed_entry(&mut live, scope, 1, [1; 16], PublishState::Ready, ReplicaState::Live);
        assert_eq!(live.installed_scope(scope.volume), Some(scope));
        assert!(live.is_installed(scope.volume, Some((&id, &original))));
        assert!(
            !live.is_installed(scope.volume, Some((&id, &updated))),
            "a new app build does not rewrite stream authority"
        );
        for state in [
            ReplicaState::Gap,
            ReplicaState::Revoked,
            ReplicaState::Disconnected,
            ReplicaState::Failed,
        ] {
            live.notices.test_state(scope.source(), [1; 16], state);
            assert!(live.installed_scope(scope.volume).is_none());
        }
        live.notices.test_state(scope.source(), [2; 16], ReplicaState::Live);
        assert!(
            live.installed_scope(scope.volume).is_none(),
            "a different epoch is not this receiver"
        );
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Live);
        live.publisher.test_state(scope.volume, 1, PublishState::Failed);
        assert!(live.installed_scope(scope.volume).is_none());
    }

    #[test]
    fn account_recovery_is_once_per_episode_and_rejects_terminal_or_stale_state() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        for (publication, incoming) in [
            (PublishState::Failed, ReplicaState::Gap),
            (PublishState::Ready, ReplicaState::Failed),
            (PublishState::Disconnected, ReplicaState::Revoked),
        ] {
            observed_account(&mut live, publication, incoming);
            assert!(!live.needs_gap_recovery());
            assert!(!live.take_account_recovery(RecoveryKind::Transport));
        }
        observed_account(&mut live, PublishState::Ready, ReplicaState::Gap);
        live.publisher.test_account_state(6, PublishState::Ready);
        assert!(!live.take_account_recovery(RecoveryKind::Gap));
        live.publisher.test_account_state(7, PublishState::Ready);
        assert!(live.needs_gap_recovery());
        assert!(live.take_account_recovery(RecoveryKind::Gap));
        assert!(!live.needs_gap_recovery());
        assert!(!live.take_account_recovery(RecoveryKind::Gap));
        assert!(live.account_recovery_is_current(RecoveryKind::Gap));
        assert!(live.complete_account_recovery().is_err());
        live.notices.test_state(ReplicaSource::Account, [9; 16], ReplicaState::Live);
        live.complete_account_recovery().unwrap();
        assert!(
            live.take_account_recovery(RecoveryKind::Transport),
            "transport loss also recovers a not-yet-observed closed stream"
        );
        live.notices.test_state(ReplicaSource::Account, [9; 16], ReplicaState::Failed);
        assert!(!live.account_recovery_is_current(RecoveryKind::Transport));
        live.reset_recovery_attempts();
        assert!(
            !live.take_account_recovery(RecoveryKind::Transport),
            "resetting the latch never authorizes a terminal failure"
        );
    }

    #[test]
    fn account_and_volume_controllers_share_capacity_and_disconnect_retirement() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        observed_account(&mut live, PublishState::Ready, ReplicaState::Live);
        for byte in 0..SCOPE_LIMIT - 1 {
            live.reserve(key(byte)).unwrap();
        }
        assert!(live.reserve(key(SCOPE_LIMIT - 1)).is_err());
        assert!(live.account_installed());
        live.disconnect().unwrap();
        assert!(!live.account_installed());
        assert!(!live.take_account_recovery(RecoveryKind::Transport));
        runtime.block_on(live.retire_account()).unwrap();
        assert!(live.account.as_ref().unwrap().receive.is_none());
    }

    #[test]
    fn disconnect_reports_connection_loss_and_keeps_the_volume_bootstrappable() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        runtime.block_on(async {
            let held = live.publisher.begin_bootstrap(key(1).volume).await.unwrap();
            let generation = held.generation();
            let index = live.reserve(key(1)).unwrap();
            live.entries[index].ready = true;
            live.entries[index].publication_generation = Some(generation);
            live.entries[index].publication = Some(held.test_lease());
            // A second scope whose owner really did drop its lease first.
            let dropped = live.publisher.begin_bootstrap(key(2).volume).await.unwrap();
            let dropped_generation = dropped.generation();
            live.reserve(key(2)).unwrap();
            drop(dropped);
            live.disconnect().unwrap();
            // A later awaited command proves the actor handled the queued disconnect.
            live.publisher.forget_scope(key(3).volume.into()).await.unwrap();
            assert_eq!(
                live.publisher.state(key(1).volume),
                Some((generation, PublishState::Disconnected))
            );
            assert_eq!(
                live.publisher.state(key(2).volume),
                Some((dropped_generation, PublishState::Failed)),
                "a lease dropped without release stays terminal"
            );
            let again = live.publisher.begin_bootstrap(key(1).volume).await.unwrap();
            assert_eq!(
                live.publisher.state(key(1).volume),
                Some((again.generation(), PublishState::Bootstrapping))
            );
        });
    }

    #[test]
    fn transport_plan_preserves_terminal_siblings_and_rechecks_late_failure() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scope = key(1);
        observed_entry(
            &mut live,
            scope,
            7,
            [9; 16],
            PublishState::Disconnected,
            ReplicaState::Disconnected,
        );
        observed_entry(
            &mut live,
            key(2),
            8,
            [10; 16],
            PublishState::Failed,
            ReplicaState::Disconnected,
        );
        observed_entry(
            &mut live,
            key(3),
            9,
            [11; 16],
            PublishState::Disconnected,
            ReplicaState::Revoked,
        );
        assert!(!live.needs_gap_recovery());
        assert_eq!(live.take_recoveries(RecoveryKind::Transport), vec![scope]);
        assert!(live.recovery_is_current(&scope, RecoveryKind::Transport));
        assert!(live.take_recoveries(RecoveryKind::Transport).is_empty());
        live.notices.test_state(scope.source(), [9; 16], ReplicaState::Failed);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Transport));
        assert_eq!(live.publisher.state(key(2).volume), Some((8, PublishState::Failed)));
        assert_eq!(
            live.notices.state(ReplicaSource::Volume(key(3).volume)),
            Some(([11; 16], ReplicaState::Revoked))
        );
    }

    #[test]
    fn selected_transport_plan_precedes_receive_notice_and_keeps_sibling_latches() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scopes = [1, 2, 3, 4].map(|byte| ScopeKey {
            entry: Some(
                crate::store::EntryId::from_bytes(
                    test_id(byte).as_bytes()[..15].try_into().unwrap(),
                )
                .unwrap(),
            ),
            ..key(1)
        });
        live.publisher.test_state(scopes[0].volume, 7, PublishState::Failed);
        for (scope, state) in scopes.into_iter().zip([
            ReplicaState::Live,
            ReplicaState::Failed,
            ReplicaState::Revoked,
            ReplicaState::CatchingUp,
        ]) {
            // Selected recovery depends on its own publisher, not the catalog's.
            observed_entry(&mut live, scope, 7, [9; 16], PublishState::Ready, state);
        }
        live.notices.test_state(scopes[3].source(), [8; 16], ReplicaState::CatchingUp);
        assert!(!live.needs_gap_recovery());
        assert_eq!(live.take_recoveries(RecoveryKind::Transport), [scopes[0]]);
        assert!(live.recovery_is_current(&scopes[0], RecoveryKind::Transport));
        assert!(live.take_recoveries(RecoveryKind::Transport).is_empty());
        assert!(!live.entries[1].recovery_attempted);
        assert!(!live.entries[2].recovery_attempted);
        assert!(!live.entries[3].recovery_attempted);
        live.entries[0].ready = false;
        assert!(live.complete_recovery(&scopes[0]).is_err());
        assert!(live.entries[0].recovery_attempted);
        live.entries[0].ready = true;
        live.notices.test_state(scopes[0].source(), [9; 16], ReplicaState::Gap);
        assert!(live.complete_recovery(&scopes[0]).is_err());
        live.notices.test_state(scopes[0].source(), [9; 16], ReplicaState::Live);
        live.complete_recovery(&scopes[0]).unwrap();
        assert!(!live.entries[0].recovery_attempted);
    }

    #[test]
    fn readiness_recovery_survives_retirement_without_resetting_its_budgets() {
        use std::time::{Duration, Instant};
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scope = key(1);
        let index =
            observed_entry(&mut live, scope, 1, [1; 16], PublishState::Ready, ReplicaState::Gap);
        observed_entry(&mut live, key(2), 2, [2; 16], PublishState::Ready, ReplicaState::Live);
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
        // A real capture retires the old stream before pulling named backings.
        // The AP integration test exercises that retirement and guarded pull.
        live.entries[index].ready = false;
        live.entries[index].receive = None;
        live.entries[index].publication_generation = None;
        live.entries[index].preparation = Some(3);
        let mut now = Instant::now();
        for attempt in 0u32..=6 {
            assert_eq!(live.defer_readiness(&scope, 2, now).unwrap(), attempt < 6);
            assert!(!live.needs_gap_recovery());
            assert!(live.is_installed(key(2).volume, None));
            let mut plan = live.take_recoveries(RecoveryKind::Gap);
            live.append_readiness_recoveries(&mut plan, now);
            assert!(plan.is_empty());
            if attempt == 6 {
                assert!(live.readiness_wake_after(now).is_none());
                assert!(live.start_readiness(&scope, now + Duration::from_secs(60)).is_err());
                assert_eq!(
                    live.availability(live.store.change_status(), 0),
                    "Synchronization failed; local data is retained"
                );
                let preparation = live.entries[index].preparation;
                live.entries[index].preparation = Some(99);
                assert_eq!(live.availability(live.store.change_status(), 0), "Catching up");
                live.entries[index].preparation = preparation;
                live.reset_recovery_attempts();
                assert_eq!(live.availability(live.store.change_status(), 0), "Catching up");
                break;
            }
            assert_eq!(live.availability(live.store.change_status(), 0), "Catching up");
            let delay = Duration::from_millis(250) * (1 << attempt);
            assert_eq!(live.readiness_wake_after(now), Some(delay));
            now += delay;
            live.append_readiness_recoveries(&mut plan, now);
            assert_eq!(plan, [scope]);
            assert_eq!(live.start_readiness(&scope, now).unwrap(), 2);
            assert!(live.readiness_running(&scope));
            // The next capture has a fresh preparation. Only its own failure
            // can retain it; the previous ticket cannot admit that generation.
            live.entries[index].preparation = Some(4 + u64::from(attempt));
            assert!(!live.readiness_running(&scope));
        }
    }

    #[test]
    fn readiness_recovery_rejects_replacement_revocation_and_retirement() {
        use std::time::{Duration, Instant};
        for change in ["preparation", "scope", "revoked", "epoch", "manual", "disconnect", "live"] {
            let runtime = runtime();
            let (_directory, mut live, _reader) = controller(runtime.handle());
            let scope = key(1);
            let index = observed_entry(
                &mut live,
                scope,
                1,
                [1; 16],
                PublishState::Ready,
                ReplicaState::Gap,
            );
            assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
            if matches!(change, "preparation" | "scope") {
                live.entries[index].ready = false;
                live.entries[index].receive = None;
                live.entries[index].publication_generation = None;
                live.entries[index].preparation = Some(2);
            }
            let now = Instant::now();
            assert!(live.defer_readiness(&scope, 0, now).unwrap());
            match change {
                "preparation" => live.entries[index].preparation = Some(3),
                "scope" => live.entries[index].key.session = Some(([3; 32], [4; 32])),
                "revoked" => {
                    live.notices.test_state(scope.source(), [1; 16], ReplicaState::Revoked)
                }
                "epoch" => live.notices.test_state(scope.source(), [2; 16], ReplicaState::Gap),
                "manual" => live.reset_recovery_attempts(),
                "disconnect" => live.disconnect().unwrap(),
                "live" => {
                    live.notices.test_state(scope.source(), [1; 16], ReplicaState::Live);
                    live.complete_recovery(&scope).unwrap();
                }
                _ => unreachable!(),
            }
            let mut plan = Vec::new();
            live.append_readiness_recoveries(&mut plan, now + Duration::from_secs(60));
            assert!(plan.is_empty(), "{change}");
            assert!(live.readiness_wake_after(now).is_none(), "{change}");
            assert!(!live.readiness_running(&scope), "{change}");
        }
    }

    #[test]
    fn gap_selection_requires_current_epochs_generations_and_ready_entry() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scope = key(1);
        let index =
            observed_entry(&mut live, scope, 7, [9; 16], PublishState::Ready, ReplicaState::Gap);
        assert!(live.needs_gap_recovery());
        live.publisher.test_state(scope.volume, 6, PublishState::NeedsRecovery);
        assert!(!live.needs_gap_recovery());
        live.publisher.test_state(scope.volume, 7, PublishState::Ready);
        live.notices.test_state(scope.source(), [8; 16], ReplicaState::Gap);
        assert!(!live.needs_gap_recovery());
        live.notices.test_state(scope.source(), [9; 16], ReplicaState::Gap);
        live.entries[index].ready = false;
        assert!(!live.needs_gap_recovery());
        live.entries[index].ready = true;
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
        assert!(live.entries[index].ready); // Selection does not retire either side.
        assert_eq!(live.entries[index].receive.as_ref().unwrap().epoch, [9; 16]);
        assert!(!live.needs_gap_recovery());
        assert!(live.take_recoveries(RecoveryKind::Gap).is_empty());
    }

    #[test]
    fn gap_selection_never_overrides_terminal_or_disconnected_peer_state() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scope = key(1);
        observed_entry(
            &mut live,
            scope,
            1,
            [1; 16],
            PublishState::NeedsRecovery,
            ReplicaState::Live,
        );
        for incoming in [ReplicaState::Failed, ReplicaState::Revoked, ReplicaState::Disconnected] {
            live.notices.test_state(scope.source(), [1; 16], incoming);
            assert!(!live.needs_gap_recovery(), "{incoming:?}");
            assert!(live.take_recoveries(RecoveryKind::Gap).is_empty());
        }
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Gap);
        for publication in [
            PublishState::Failed,
            PublishState::Disconnected,
            PublishState::NeedsBootstrap,
            PublishState::Bootstrapping,
        ] {
            live.publisher.test_state(scope.volume, 1, publication);
            assert!(!live.needs_gap_recovery(), "{publication:?}");
        }
        live.publisher.test_state(scope.volume, 1, PublishState::NeedsRecovery);
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Live);
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
    }

    #[test]
    fn recovery_attempt_survives_retirement_and_replacement_until_current_live_boundary() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scope = ScopeKey {
            entry: None,
            volume: key(1).volume,
            session: Some(([0xaa; 32], [0xbb; 32])),
        };
        let index =
            observed_entry(&mut live, scope, 1, [1; 16], PublishState::Ready, ReplicaState::Gap);
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
        runtime.block_on(live.retire(index)).unwrap();
        assert!(live.entries[index].recovery_attempted);
        assert!(live.complete_recovery(&scope).is_err());
        // begin reuses reserve's entry; finish installs replacement observation
        // identities without resetting the episode. Network/bootstrap behavior
        // itself is covered by the actual AP integration tests.
        assert_eq!(
            observed_entry(
                &mut live,
                scope,
                2,
                [2; 16],
                PublishState::NeedsRecovery,
                ReplicaState::Gap
            ),
            index
        );
        assert!(!live.needs_gap_recovery());
        assert!(live.take_recoveries(RecoveryKind::Gap).is_empty());
        assert!(live.complete_recovery(&scope).is_err());
        live.reset_recovery_attempts();
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
        live.publisher.test_state(scope.volume, 2, PublishState::Pending);
        live.notices.test_state(scope.source(), [2; 16], ReplicaState::Gap);
        assert!(live.complete_recovery(&scope).unwrap_err().is::<LiveGap>());
        assert!(live.recovery_is_current(&scope, RecoveryKind::Gap));
        assert!(live.entries[index].recovery_attempted);
        for publication in [PublishState::Failed, PublishState::Disconnected] {
            live.publisher.test_state(scope.volume, 2, publication);
            assert!(!live.complete_recovery(&scope).unwrap_err().is::<LiveGap>());
            assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        }
        live.publisher.test_state(scope.volume, 2, PublishState::Pending);
        for incoming in [ReplicaState::Failed, ReplicaState::Revoked] {
            live.notices.test_state(scope.source(), [2; 16], incoming);
            assert!(!live.complete_recovery(&scope).unwrap_err().is::<LiveGap>());
        }
        live.notices.test_state(scope.source(), [2; 16], ReplicaState::CatchingUp);
        assert!(live.complete_recovery(&scope).is_err());
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Live);
        assert!(live.complete_recovery(&scope).is_err());
        live.notices.test_state(scope.source(), [2; 16], ReplicaState::Live);
        live.publisher.test_state(scope.volume, 1, PublishState::Ready);
        assert!(live.complete_recovery(&scope).is_err());
        live.publisher.test_state(scope.volume, 2, PublishState::Pending);
        let stale = ScopeKey { session: Some(([0xaa; 32], [0xcc; 32])), ..scope };
        assert!(live.complete_recovery(&stale).is_err());
        assert!(live.entries[index].recovery_attempted);
        live.complete_recovery(&scope).unwrap();
        assert!(!live.entries[index].recovery_attempted);
        live.notices.test_state(scope.source(), [2; 16], ReplicaState::Gap);
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
    }

    #[test]
    fn captured_recovery_plan_rechecks_late_failure_and_exact_scope_before_retirement() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let scope = ScopeKey {
            entry: None,
            volume: key(1).volume,
            session: Some(([0xaa; 32], [0xbb; 32])),
        };
        let index =
            observed_entry(&mut live, scope, 1, [1; 16], PublishState::Ready, ReplicaState::Gap);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
        assert!(live.recovery_is_current(&scope, RecoveryKind::Gap));
        let stale = ScopeKey { session: Some(([0xaa; 32], [0xcc; 32])), ..scope };
        assert!(!live.recovery_is_current(&stale, RecoveryKind::Gap));
        live.publisher.test_state(scope.volume, 1, PublishState::Failed);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        live.publisher.test_state(scope.volume, 1, PublishState::Ready);
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Revoked);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        live.notices.test_state(scope.source(), [2; 16], ReplicaState::Gap);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Gap);
        live.publisher.test_state(scope.volume, 2, PublishState::NeedsRecovery);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        live.publisher.test_state(scope.volume, 1, PublishState::Ready);
        live.notices.test_state(scope.source(), [1; 16], ReplicaState::Live);
        assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
        assert!(live.entries[index].recovery_attempted);
    }

    #[test]
    fn gap_plan_is_bounded_by_existing_scope_capacity() {
        use crate::engine::publisher::PublishState;
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        for byte in 1..=SCOPE_LIMIT {
            observed_entry(
                &mut live,
                key(byte),
                byte as u64,
                [byte as u8; 16],
                PublishState::Ready,
                ReplicaState::Gap,
            );
        }
        assert!(live.reserve(key(SCOPE_LIMIT + 1)).is_err());
        let plan = live.take_recoveries(RecoveryKind::Gap);
        assert_eq!(plan.len(), SCOPE_LIMIT);
        assert_eq!(plan.capacity(), SCOPE_LIMIT);
        assert!(
            plan.capacity() * std::mem::size_of::<ScopeKey>()
                + std::mem::size_of::<Vec<ScopeKey>>()
                <= 96 * 1024
        );
        assert!(plan.iter().enumerate().all(|(index, scope)| *scope == key(index + 1)));
        assert!(live.take_recoveries(RecoveryKind::Gap).is_empty());
        assert!(!live.needs_gap_recovery());
    }

    #[test]
    fn registry_bounds_fixed_keys_and_allows_replacing_existing_volume() {
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        for byte in 1..=SCOPE_LIMIT {
            assert_eq!(live.reserve(key(byte)).unwrap(), byte - 1);
        }
        assert!(live.reserve(key(SCOPE_LIMIT + 1)).is_err());
        assert_eq!(live.reserve(key(4)).unwrap(), 3);
        assert_eq!(live.entries.capacity(), SCOPE_LIMIT);
        assert!(
            SCOPE_LIMIT * std::mem::size_of::<Entry>() + std::mem::size_of::<LiveReplicas>()
                < 192 * 1024
        );
        let scoped = ScopeKey {
            entry: None,
            volume: key(1).volume,
            session: Some(([0xaa; 32], [0xbb; 32])),
        };
        assert!(scoped.matches(
            key(1).volume,
            Some((
                &crate::identifier::encode(&[0xaa; 32]),
                &crate::identifier::encode(&[0xbb; 32])
            ))
        ));
        assert!(!scoped.matches(key(1).volume, Some((&"a".repeat(64), &"c".repeat(64)))));
        assert!(!scoped.matches(key(1).volume, None));
        assert!(!scoped.matches(key(2).volume, Some((&"a".repeat(64), &"b".repeat(64)))));
    }

    #[test]
    fn disconnect_retires_task_canceled_before_first_poll_without_a_notice_slot() {
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let index = live.reserve(key(1)).unwrap();
        let polled = Arc::new(AtomicBool::new(false));
        let observed = polled.clone();
        let (task, started) = spawn_receiver(runtime.handle(), async move {
            observed.store(true, Ordering::SeqCst);
            std::future::pending::<Result<()>>().await
        });
        live.entries[index].receive =
            Some(Receive { task: Some(task), epoch: [1; 16], previous_epoch: None, control: None });
        live.entries[index].ready = true;
        live.entries[index].preparation = Some(7);
        live.disconnect().unwrap();
        assert!(!live.entries[index].ready);
        assert!(live.entries[index].preparation.is_none());
        runtime.block_on(async {
            live.retire(index).await.unwrap();
            assert!(started.await.is_err());
        });
        assert!(!polled.load(Ordering::SeqCst));
        assert!(live.entries[index].receive.is_none());
        assert!(live.notices.drain().is_empty());
    }

    #[test]
    fn receiver_reports_early_failure_without_unbounded_diagnostic_storage() {
        let runtime = runtime();
        runtime.block_on(async {
            let (task, started) =
                spawn_receiver(runtime.handle(), async { Err("é".repeat(1000).into()) });
            let error = started.await.unwrap().unwrap_err();
            assert_eq!(error.len(), 512);
            assert_eq!(error.capacity(), 512);
            task.await.unwrap();
            let (task, started) = spawn_receiver(runtime.handle(), async { Ok(()) });
            assert!(started.await.unwrap().unwrap_err().contains("ended during startup"));
            task.await.unwrap();
        });
    }

    #[test]
    fn canceled_startup_caller_drops_receive_and_retirement_releases_capacity() {
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let index = live.reserve(key(1)).unwrap();
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let (task, started) = spawn_receiver(runtime.handle(), async move {
            let _guard = guard;
            std::future::pending::<Result<()>>().await
        });
        drop(started);
        live.entries[index].receive =
            Some(Receive { task: Some(task), epoch: [1; 16], previous_epoch: None, control: None });
        runtime.block_on(async {
            live.retain_provider_volumes(&[]).await.unwrap();
        });
        assert!(dropped.load(Ordering::SeqCst));
        assert!(live.entries.is_empty());
    }

    #[test]
    fn publisher_failure_invalidates_only_its_scope() {
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let first = live.reserve(key(1)).unwrap();
        let second = live.reserve(key(2)).unwrap();
        live.entries[first].ready = true;
        live.entries[first].preparation = Some(1);
        live.entries[second].ready = true;
        live.entries[second].preparation = Some(2);
        live.invalidate(key(1).volume);
        assert!(!live.entries[first].ready);
        assert!(live.entries[first].preparation.is_none());
        assert!(live.entries[second].ready);
        assert_eq!(live.entries[second].preparation, Some(2));
    }

    #[test]
    fn delayed_failure_cannot_invalidate_replacement_generation() {
        let runtime = runtime();
        let (_directory, mut live, _reader) = controller(runtime.handle());
        let index = live.reserve(key(1)).unwrap();
        live.entries[index].ready = true;
        live.entries[index].publication_generation = Some(9);
        live.invalidate_publication(key(1).volume, 8);
        assert!(live.entries[index].ready);
        live.invalidate_receive(key(1).volume, [8; 16]);
        assert!(live.entries[index].ready);
        live.invalidate_publication(key(1).volume, 9);
        assert!(!live.entries[index].ready);
    }

    /// Checks recovery eligibility against fixed combinations of publisher
    /// and replica state, including stale generations and epochs.
    #[test]
    fn recovery_eligibility_matches_regression_cases() {
        use crate::engine::publisher::PublishState;
        let number = |word: &str| (word != "-").then(|| word.parse::<u64>().unwrap());
        let publish = |word: &str| match word {
            "needsbootstrap" => PublishState::NeedsBootstrap,
            "bootstrapping" => PublishState::Bootstrapping,
            "excluded" => PublishState::Excluded,
            "ready" => PublishState::Ready,
            "pending" => PublishState::Pending,
            "needsrecovery" => PublishState::NeedsRecovery,
            "disconnected" => PublishState::Disconnected,
            "failed" => PublishState::Failed,
            _ => panic!("unknown publish state {word}"),
        };
        let replica = |word: &str| match word {
            "catchingup" => ReplicaState::CatchingUp,
            "live" => ReplicaState::Live,
            "gap" => ReplicaState::Gap,
            "revoked" => ReplicaState::Revoked,
            "disconnected" => ReplicaState::Disconnected,
            "failed" => ReplicaState::Failed,
            _ => panic!("unknown replica state {word}"),
        };
        let epoch = |value: u64| [u8::try_from(value).unwrap(); 16];
        let mut cases = 0;
        for line in include_str!("../../tests/fixtures/recovery.txt").lines() {
            let w: Vec<&str> = line.split_whitespace().collect();
            if w[0] != "recoverable" {
                continue;
            }
            let publication = number(w[5]).map(|generation| (generation, publish(w[6])));
            let incoming = number(w[7]).map(|value| (epoch(value), replica(w[8])));
            let kind = if w[9] == "gap" { RecoveryKind::Gap } else { RecoveryKind::Transport };
            let actual = if w[1] == "true" {
                read_recovery_eligible(w[2] == "true", number(w[4]).map(epoch), incoming, kind)
            } else {
                recovery_eligible(
                    w[2] == "true",
                    number(w[3]),
                    number(w[4]).map(epoch),
                    publication,
                    incoming,
                    kind,
                )
            };
            assert_eq!(actual.to_string(), w[10], "{line}");
            cases += 1;
        }
        assert_eq!(cases, 2560);
    }
}
