//! One AP connection owned by the local engine.
// Error ownership: public operation errors return through NetworkTask to
// Engine's retained status/request owner. Reconnect, authorization, gap and
// interrupted-bootstrap tests exercise representative failures, not every OS,
// RNG or store variant at every caller. Typed establishment failures and bounded
// initial source-readiness failures retry; partial durable work remains durable.
mod account;
mod entry;
mod initialization;
mod onboarding;
mod publisher;
pub(super) mod sync_failure;
mod synchronization;
mod volume_invitations;
pub(super) mod world;
use super::completion::Wake;
use crate::engine::{
    Result,
    reply::{ApConnectionState, TransferProgress},
    sync_progress::Progress,
};
use crate::network::ap::{
    ApConnection, ClientConfig, ClientError, ClientIdentity,
    operation::{BuildSelector, ServiceRequest, ServiceResponse, SyncDirection},
    service::ServiceFailure,
    service_client as wire,
};
use crate::store::{
    Access, DurableRevision, Id, LocalStore, PendingKind, ReceivedVolume, ReplicaReader, Session,
    object_contract::Permission,
};
use kvdb::{Lww, SyncReceiver, SyncSender, sync::SyncStatus};
#[cfg(all(test, feature = "app-host"))]
use std::os::unix::net::UnixStream;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(feature = "app-host")]
pub(crate) use sync_failure::sync_failure_parts;
pub(super) use synchronization::Synchronization;

/// Private automatic scope credentials never enter AP replication.
fn remote_volumes(store: &LocalStore) -> Result<Vec<crate::store::Volume>> {
    store
        .volumes()?
        .into_iter()
        .filter_map(|volume| match store.replication_supported(volume.id) {
            Ok(false) => None,
            Ok(true) => Some(Ok(volume)),
            Err(error) => Some(Err(sync_failure::volume(store, volume.id, error))),
        })
        .collect()
}

/// A recovered owner descriptor precedes its index. An invited publisher World
/// uses separate AP admission after the Shared session is ready.
#[cfg(feature = "app-host")]
pub(super) fn world_needs_recovery(store: &LocalStore, session: &Session) -> Result<bool> {
    if session.is_distinct_world(session.root_volume) && session.app.publisher != store.account().id
    {
        return Ok(false);
    }
    match store.volume(session.root_volume) {
        Ok(world) if session.shared_volume.is_some() && world.owner != store.account().id => {
            Ok(false)
        }
        Ok(world) => Ok(!store.has_volume_index(world.id)?),
        Err(crate::store::Error::NotFound(_)) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

// Every admitted app process and embedded guest can own a different session.
#[cfg(feature = "app-host")]
const SESSION_OBSERVER_LIMIT: usize =
    super::session_instance::MAX_PROCESSES + crate::ui::local_guest::MAX_APPS;

// Authentication runs on the existing network worker. Persist protocol failures
// here so both process-engine and standalone hosts retain the original diagnostic.
// ProtocolError contains only bounded codec variants; never format a request,
// identity, credential, or response body into the log.
fn log_authentication_protocol_error(
    identity: &Path,
    operation: &'static str,
    error: &ClientError,
) {
    let ClientError::Protocol(error) = error else { return };
    let profile = identity.parent().expect("instance identity belongs to a profile");
    crate::platform::failure::log(
        profile,
        &format!("authentication {operation}"),
        &format!("{error:?}: {error}"),
    );
}

/// Stable resources retained by the local owner while Remote is with a
/// background operation. They do not retain LocalStore before account binding.
#[derive(Clone)]
pub(crate) struct NetworkHandles {
    #[cfg(feature = "app-host")]
    pub(crate) builds: Arc<super::builds::Builds>,
    onboarding: Arc<std::sync::atomic::AtomicU8>,
    onboarding_connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
    #[cfg(feature = "app-host")]
    sync_connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
    #[cfg(feature = "app-host")]
    missing_owner_backings: Arc<parking_lot::Mutex<BTreeSet<Id>>>,
    onboarding_account: Arc<Mutex<Option<Id>>>,
    pub(crate) progress: Arc<Progress>,
    #[cfg(feature = "app-host")]
    pub(crate) connection_status: Arc<super::ap_status::Status>,
    account_ready: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "app-host")]
    pub(super) volume_invitations_changed: Arc<std::sync::atomic::AtomicBool>,
    #[allow(
        dead_code,
        reason = "Retain the shared resource while the background operation owns Remote."
    )]
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
    pub(crate) blobs: crate::engine::blob::BlobTransport,
    pub(crate) received: Arc<crate::engine::replication::ReplicaNotices>,
    #[allow(
        dead_code,
        reason = "Retain the shared resource while the background operation owns Remote."
    )]
    pub(crate) publisher: Arc<Mutex<Option<Arc<crate::engine::publisher::Publisher>>>>,
}

impl NetworkHandles {
    /// This observation affects scheduling only. Shared admission and World access
    /// still use the current store policies. It survives network worker handoffs.
    #[cfg(feature = "app-host")]
    pub(super) fn owner_backing_missing(&self, volume: Id) -> bool {
        self.missing_owner_backings.lock().contains(&volume)
    }

    #[cfg(feature = "app-host")]
    pub(crate) fn refresh_builds(&self, store: Arc<LocalStore>, wake: Arc<Wake>) -> Result<()> {
        let connection =
            self.sync_connection.lock().expect("connection lock is not poisoned").clone();
        self.builds.refresh(store, connection, self.runtime.handle(), wake)
    }
    /// Readiness remains visible while a bulk operation owns Remote. It cannot
    /// admit sessions, and known account-stream failures immediately revoke it.
    pub(crate) fn account_initialized(&self) -> bool {
        use crate::engine::{
            publisher::PublishState,
            replication::{ReplicaSource, ReplicaState},
        };
        self.account_ready.load(std::sync::atomic::Ordering::Acquire)
            && self
                .publisher
                .lock()
                .expect("publisher mutex is not poisoned")
                .as_ref()
                .and_then(|publisher| publisher.account_state())
                .is_some_and(|notice| {
                    matches!(notice.state, PublishState::Ready | PublishState::Pending)
                })
            && self
                .received
                .state(ReplicaSource::Account)
                .is_some_and(|(_, state)| matches!(state, ReplicaState::Live))
    }
}

const CANCELED: &str = "Canceled; the AP connection was not reopened";
const OFFLINE_MODE_ERROR: &str = "Work Offline is enabled. Turn it off to connect.";
pub(crate) const SESSION_BUILD_UNAVAILABLE: &str =
    "Playbit couldn’t verify the app build used by this session. Its saved data has been kept.";

#[derive(Debug)]
struct SessionBuildUnavailable;
impl std::fmt::Display for SessionBuildUnavailable {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(SESSION_BUILD_UNAVAILABLE)
    }
}
impl std::error::Error for SessionBuildUnavailable {}
impl SessionBuildUnavailable {
    fn reported_by(error: &(dyn std::error::Error + 'static)) -> bool {
        error
            .downcast_ref::<ServiceFailure>()
            .is_some_and(ServiceFailure::is_missing_build_identity)
    }
    fn from_service(
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        if Self::reported_by(error.as_ref()) { Self.into() } else { error }
    }
}

/// Ending an operation because it was canceled, as distinct from failing. A
/// caller that took recovery state before the cancellation has to put it back:
/// nothing about the transport or the account was established or ruled out.
#[derive(Debug)]
pub(crate) struct CanceledOperation;
impl std::fmt::Display for CanceledOperation {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(CANCELED)
    }
}
impl std::error::Error for CanceledOperation {}
fn canceled(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    error.downcast_ref::<CanceledOperation>().is_some()
}

mod gauges;

/// Provider admission is explicit: an empty provider selection discovers only
/// account metadata, while an ordinary client retains its complete behavior.
#[cfg(any(feature = "file-provider", test))]
pub(crate) const PROVIDER_VOLUME_ADMISSION: usize = 2;
#[derive(Clone, Debug, Default)]
pub(crate) enum ClientScope {
    #[default]
    FullClient,
    #[cfg(any(feature = "file-provider", test))]
    Provider(Vec<Id>),
}
impl ClientScope {
    pub(crate) fn provider(&self) -> bool {
        self.selected().is_some()
    }
    pub(crate) fn admits(&self, volume: Id) -> bool {
        self.selected().is_none_or(|ids| ids.contains(&volume))
    }
    pub(crate) fn selected(&self) -> Option<&[Id]> {
        match self {
            Self::FullClient => None,
            #[cfg(any(feature = "file-provider", test))]
            Self::Provider(ids) => {
                assert!(
                    ids.len() <= PROVIDER_VOLUME_ADMISSION,
                    "provider selection exceeds admission"
                );
                Some(ids)
            }
        }
    }
}
pub(crate) type SharedClientScope = Arc<parking_lot::RwLock<ClientScope>>;

pub struct Remote {
    channel: crate::release_channel::ReleaseChannel,
    #[cfg(feature = "app-host")]
    builds: Arc<super::builds::Builds>,
    #[cfg(feature = "app-host")]
    missing_owner_backings: Arc<parking_lot::Mutex<BTreeSet<Id>>>,
    onboarding_account: Arc<Mutex<Option<Id>>>,
    onboarding: Arc<std::sync::atomic::AtomicU8>,
    gauges: gauges::Gauges,
    progress: Arc<Progress>,
    connection_status: Arc<super::ap_status::Status>,
    account_ready: Arc<std::sync::atomic::AtomicBool>,
    pub(super) volume_invitations_changed: Arc<std::sync::atomic::AtomicBool>,
    runtime: Arc<tokio::runtime::Runtime>,
    received: Arc<crate::engine::replication::ReplicaNotices>,
    completion: Arc<Wake>,
    live: Option<crate::engine::live::LiveReplicas>,
    // A File Provider process owns one domain, without admitting unrelated
    // account volumes or app sessions to its replica workers.
    pub(super) scope: SharedClientScope,
    publisher: Arc<Mutex<Option<Arc<crate::engine::publisher::Publisher>>>>,
    config: Option<ClientConfig>,
    address_lookup:
        Option<tokio::task::JoinHandle<std::result::Result<std::net::SocketAddr, ClientError>>>,
    #[cfg(test)]
    lookup_error: Option<std::io::Error>,
    testing: Option<crate::network::ap::hello::TestingCredentials>,
    #[cfg(target_vendor = "apple")]
    remembered_account: Option<blake3::Hash>,
    // Only a service-confirmed agreement admits this transport to replica operations.
    connection: Option<Arc<ApConnection>>,
    /// The background close-drain spawned by the most recent `disconnect`.
    /// `Drop` joins it (bounded) so an exiting process does not orphan it
    /// along with the runtime it needs to finish sending the close.
    pending_drain: Option<tokio::task::JoinHandle<()>>,
    connection_watch: Option<crate::engine::connection_watch::ConnectionWatch>,
    reconnect: Option<Reconnect>,
    initializations: initialization::Initializations,
    volume_invitation_watch: Option<volume_invitations::Watch>,
    authenticated_connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
    // Published to data workers only after AP confirms the current document on this transport.
    sync_connection: Arc<Mutex<Option<Arc<ApConnection>>>>,
    blob_transfers: Arc<crate::engine::blob::BlobTransfers>,
    canceled: Option<Arc<crate::engine::cancellation::Canceled>>,
    listeners: Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>,
    traffic: BTreeMap<String, crate::engine::session_channels::SessionChannels>,
    selection_watches: BTreeSet<String>,
    availability_checked: bool,
    pending_availability: bool,
    private_availability: bool,
    private_peer_waiting: bool,
    private_invitation_waiting: bool,
    identity: ClientIdentity,
    replacement_required: bool,
    identity_path: PathBuf,
}
struct PullReceipt {
    ap_source: bool,
    revision: DurableRevision,
    source: crate::store::SnapshotToken,
    hosted: bool,
}
struct CapturedControls {
    index: PullReceipt,
    revocations: PullReceipt,
}
impl PullReceipt {
    /// Log snapshots qualify a source cut. Their plaintext projection cannot
    /// acknowledge an ordinary kvdb encrypted-at-rest root on AP.
    fn from_log(receipt: &crate::store::bloblog::ReplicaPage, ap_source: bool) -> Self {
        Self {
            ap_source,
            source: receipt.source,
            revision: DurableRevision {
                revision: receipt.database_clock,
                policy_revision: receipt.policy_revision,
                root: None,
            },
            hosted: false,
        }
    }
}
enum StagedBacking {
    Unreadable,
    Missing(crate::store::EntryId),
    Remote(PullReceipt),
    Local(crate::store::SnapshotToken),
}
enum ControlRepair<'a> {
    ServiceRead,
    CheckRetirement(crate::store::EntryId),
    MissingBackings {
        entries: &'a [crate::store::EntryId],
        first_unrepaired: &'a mut crate::store::EntryId,
    },
}
#[derive(Clone, Copy)]
enum ReceivedPull {
    Control,
    Backing {
        cut: crate::store::SnapshotCut,
        protocol: crate::store::volume_index::DatabaseProtocol,
    },
}
type BootstrapPull<'a> = dyn FnMut(
        crate::store::DatabaseId,
        &crate::store::Database,
        ReceivedPull,
    ) -> Result<Option<PullReceipt>>
    + 'a;
const _: () = assert!(4 * std::mem::size_of::<crate::trace::Span>() <= 512);

struct Reconnect {
    account: bool,
    scopes: Vec<crate::engine::live::ScopeKey>,
    after: Instant,
    failures: u8,
    bootstrap: bool,
}
impl Reconnect {
    fn new(scopes: Vec<crate::engine::live::ScopeKey>) -> Self {
        Self { account: false, scopes, after: Instant::now(), failures: 0, bootstrap: false }
    }
    fn failed(&mut self, now: Instant) {
        let delay = Duration::from_millis((250u64 << self.failures.min(7)).min(30_000));
        self.failures = self.failures.saturating_add(1);
        self.after = now + delay;
    }
}
impl Remote {
    pub(super) const fn channel(&self) -> crate::release_channel::ReleaseChannel {
        self.channel
    }
    pub(crate) fn offline(&self) -> bool {
        self.connection_status.offline()
    }

    pub(crate) fn set_offline(&mut self, enabled: bool) {
        self.connection_status.set_offline(enabled, &self.completion);
        if enabled {
            self.disconnect();
            self.progress.set(None, &self.completion);
        }
    }

    fn require_online(&self) -> Result<()> {
        if self.offline() { Err(OFFLINE_MODE_ERROR.into()) } else { Ok(()) }
    }
    pub(crate) fn network_handles(&self) -> NetworkHandles {
        NetworkHandles {
            #[cfg(feature = "app-host")]
            builds: self.builds.clone(),
            #[cfg(feature = "app-host")]
            missing_owner_backings: self.missing_owner_backings.clone(),
            onboarding: self.onboarding.clone(),
            onboarding_connection: self.authenticated_connection.clone(),
            #[cfg(feature = "app-host")]
            sync_connection: self.sync_connection.clone(),
            onboarding_account: self.onboarding_account.clone(),
            progress: self.progress.clone(),
            #[cfg(feature = "app-host")]
            connection_status: self.connection_status.clone(),
            account_ready: self.account_ready.clone(),
            #[cfg(feature = "app-host")]
            volume_invitations_changed: self.volume_invitations_changed.clone(),
            runtime: self.runtime.clone(),
            blobs: self.blob_transport(),
            received: self.received.clone(),
            publisher: self.publisher.clone(),
        }
    }

    /// Start the account-owned feed only after authentication and local account
    /// binding agree. It survives GUI closure and movement to a background worker.
    fn ensure_live(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        self.network_handles().report_onboarding(store.clone());
        if self.identity.auth_token.is_empty()
            || self.identity.account_id()? != *store.account().id.as_bytes()
        {
            return Err("live replication requires the bound authenticated account".into());
        }
        if self.live.is_none() {
            let live = crate::engine::live::LiveReplicas::new(
                self.runtime.handle(),
                store.clone(),
                self.completion.clone(),
                self.received.clone(),
                self.blob_transfers.clone(),
                self.scope.clone(),
            )?;
            *self.publisher.lock().expect("mutex is not poisoned") = Some(live.publisher());
            self.live = Some(live);
        }
        Ok(())
    }

    fn consume_publisher_failures(&mut self) -> Result<()> {
        let Some(live) = &mut self.live else {
            return Ok(());
        };
        let notices = live.publisher().drain_notices();
        for notice in notices.volumes {
            if matches!(
                notice.state,
                crate::engine::publisher::PublishState::NeedsRecovery
                    | crate::engine::publisher::PublishState::Disconnected
                    | crate::engine::publisher::PublishState::Failed
            ) {
                live.invalidate_publication(
                    crate::engine::publisher::PublishKey {
                        volume: notice.volume,
                        entry: notice.entry,
                    },
                    notice.generation,
                );
            }
        }
        if let Some(error) = notices.error {
            return Err(error.into());
        }
        Ok(())
    }

    #[cfg(feature = "file-provider")]
    pub(super) fn finish_provider_copy(
        &mut self,
        source: crate::store::volume_index::EntryAddress,
    ) -> Result<()> {
        let Some(live) = self.live.as_mut() else { return Ok(()) };
        self.runtime.block_on(live.finish_provider_copy(source))
    }

    /// Prefer owner authority, then retain an eligible installed exact session.
    /// A second session using the same root must not replace a healthy stream.
    fn has_live_volume(&self, store: &LocalStore, volume: Id) -> Result<bool> {
        let Some(live) = &self.live else {
            return Ok(false);
        };
        if store.volume(volume)?.owner == store.account().id
            || store.has_direct_volume_grant(volume)?
        {
            // Terminal application persists the fence before reporting Gap.
            // An installed stream cannot substitute for current authority.
            return Ok(
                !store.authority_refresh_required(volume)? && live.is_installed(volume, None)
            );
        }
        let Some((id, version)) = live.installed_scope(volume).and_then(|scope| scope.session)
        else {
            return Ok(false);
        };
        // A local build adoption does not revoke an already authorized stream.
        // Check its captured authority, not the app's newer execution version.
        Ok(store
            .authorize_replication(
                &Access::session_version(
                    store.account().id,
                    crate::identifier::encode(&id),
                    crate::identifier::encode(&version),
                ),
                volume,
                Permission::Read,
            )
            .is_ok())
    }

    fn live_fence(
        &self,
        store: &LocalStore,
        selected: Option<&[Id]>,
        unavailable_sessions: Option<&BTreeSet<String>>,
        account_dependencies_deferred: bool,
    ) -> Result<()> {
        let Some(live) = &self.live else {
            return Ok(());
        };
        let mut volumes = Vec::with_capacity(super::live::SCOPE_LIMIT);
        let selected = selected.map(|ids| ids.iter().copied().collect::<BTreeSet<_>>());
        for volume in remote_volumes(store)? {
            if !volume.share_key_with_service
                || selected.as_ref().is_some_and(|ids| !ids.contains(&volume.id))
                || !self.scope.read().admits(volume.id)
            {
                continue;
            }
            let required = if volume.owner == store.account().id
                || store.has_direct_volume_grant(volume.id)?
            {
                self.service_covers_reader(store, volume.id, None)
                    .map_err(|error| sync_failure::volume(store, volume.id, error))?
            } else {
                let mut required = false;
                for session in store.sessions()? {
                    if (session.replica_volume() != volume.id
                        && !session.is_distinct_world(volume.id))
                        || unavailable_sessions.is_some_and(|ids| ids.contains(&session.id))
                    {
                        continue;
                    }
                    if session.is_distinct_world(volume.id)
                        && !store
                            .invited_world_ready(&session.id, volume.id)
                            .map_err(|error| sync_failure::volume(store, volume.id, error))?
                    {
                        continue;
                    }
                    if self
                        .service_covers_reader(store, volume.id, Some(&session))
                        .map_err(|error| sync_failure::volume(store, volume.id, error))?
                    {
                        required = true;
                        break;
                    }
                }
                required
            };
            if !required {
                continue;
            }
            // Eligibility is independent of stream health: a failed required
            // scope must fail the fence, never disappear from its target set.
            if !self
                .has_live_volume(store, volume.id)
                .map_err(|error| sync_failure::volume(store, volume.id, error))?
            {
                if let Some(cause) = live.failed_receive(volume.id) {
                    return Err(cause);
                }
                if let Some(cause) = live.publisher().failed_publication(volume.id) {
                    return Err(live.resolve_failure(cause));
                }
                return Err(sync_failure::volume(
                    store,
                    volume.id,
                    "required live replica needs recovery before synchronization can complete",
                ));
            }
            if volumes.len() == super::live::SCOPE_LIMIT {
                return Err("live replica scope capacity exceeded".into());
            }
            volumes.push(volume.id);
        }
        self.runtime.block_on(async {
            let fenced =
                live.publisher().fence(&volumes).await.map_err(|e| live.resolve_failure(e))?;
            live.checkpoint(&fenced).await.map_err(|e| live.resolve_failure(e))?;
            if self.scope.read().provider() || account_dependencies_deferred {
                // Selected work can finish while independent account records
                // wait in the durable outbox for their own volume dependencies.
                live.account_receive_fence().await
            } else {
                live.account_fence().await
            }
        })
    }

    fn service_covers_reader(
        &self,
        store: &LocalStore,
        volume: Id,
        session: Option<&Session>,
    ) -> Result<bool> {
        if let Some(session) = session
            && !store.volume(session.replica_volume())?.share_key_with_service
        {
            return Ok(false);
        }
        let access = session.map_or_else(
            || Access::owner(store.account().id),
            |session| {
                Access::session_version(store.account().id, &session.id, &session.resolved_build_id)
            },
        );
        match store.with_authorized_replica(
            &access,
            volume,
            crate::store::DatabaseId::Index,
            Some(Permission::Read),
            |_, policy| policy.authorize_reader(ReplicaReader::Service),
        ) {
            Ok(()) => Ok(true),
            Err(crate::store::Error::AccessDenied("waiting for an authorized peer")) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(feature = "app-host")]
    pub(crate) fn sync_connected(&self) -> bool {
        self.connection.as_ref().is_some_and(|connection| !connection.is_closed())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn has_invitation_connection(
        &self,
        connection: &Arc<crate::network::ap::ApConnection>,
    ) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection) && !current.is_closed())
    }

    pub(crate) fn cancel_handle(&self) -> Arc<Mutex<Option<Arc<ApConnection>>>> {
        self.authenticated_connection.clone()
    }

    /// The running background operation's cancellation, or None while the
    /// owner holds Remote. Global cancellation shuts the transport down;
    /// interleaved app cancellation interrupts only its own read/download.
    /// Neither may open a replacement the cancellation cannot reach.
    pub(crate) fn set_cancellation(
        &mut self,
        canceled: Option<Arc<crate::engine::cancellation::Canceled>>,
    ) {
        self.canceled = canceled;
    }
    fn canceled(&self) -> bool {
        self.canceled.as_ref().is_some_and(|canceled| canceled.raised())
    }
    #[cfg(feature = "app-host")]
    pub(super) fn check_cancellation(&self) -> Result<()> {
        if self.canceled() { Err(CanceledOperation.into()) } else { Ok(()) }
    }
    /// Wait for an ordinary authenticated connection, abandoning the attempt
    /// the moment this operation is canceled. Cancellation closes the transport
    /// the operation was handed; it cannot reach one that is still
    /// authenticating, and that exchange has only the AP's own timeout to end
    /// it. Dropping the attempt closes the half-open connection, which no
    /// other handle has yet.
    fn while_uncanceled<T>(
        runtime: &tokio::runtime::Runtime,
        canceled: Option<&Arc<crate::engine::cancellation::Canceled>>,
        attempt: impl std::future::Future<Output = std::result::Result<T, ClientError>>,
    ) -> Result<T> {
        runtime.block_on(async {
            let Some(canceled) = canceled else { return Ok(attempt.await?) };
            tokio::select! {
                biased;
                () = canceled.raising() => Err(CanceledOperation.into()),
                established = attempt => Ok(established?),
            }
        })
    }

    #[cfg(feature = "app-host")]
    pub(crate) fn adopted_session(&mut self, id: &str) {
        self.traffic.remove(id);
        self.selection_watches.remove(id);
        if let Some(task) = self.listeners.lock().expect("mutex is not poisoned").remove(id) {
            task.abort();
        }
    }
    fn service_data(
        &mut self,
        store: &LocalStore,
        request: &ServiceRequest,
    ) -> Result<serde_json::Value> {
        self.connect(store)?;
        let response = if matches!(
            request,
            ServiceRequest::ReadInvitation { .. } | ServiceRequest::ResolveSession { .. }
        ) {
            self.app_request(request)?
        } else {
            self.runtime.block_on(wire::request(
                self.connection.as_ref().ok_or("AP disconnected")?,
                request,
            ))?
        };
        response.data.ok_or_else(|| "missing AP service response data".into())
    }
    /// Abandon one app read/admission stream without closing unrelated live
    /// replication. Durable replica exchanges retain their completion boundary.
    fn app_request(&self, request: &ServiceRequest) -> Result<ServiceResponse> {
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        self.runtime.block_on(async {
            let Some(canceled) = &self.canceled else {
                return wire::request(connection, request).await;
            };
            let mut stream = tokio::select! {
                biased;
                () = canceled.raising() => return Err(CanceledOperation.into()),
                stream = wire::begin(connection, request) => stream?,
            };
            stream.finish()?;
            tokio::select! {
                biased;
                () = canceled.raising() => {
                    stream.cancel();
                    Err(CanceledOperation.into())
                }
                response = wire::response(&mut stream, request.operation_id()) => response,
            }
        })
    }
    pub fn resolve_volume_grantee(
        &mut self,
        store: &LocalStore,
        volume: Id,
        email: &str,
    ) -> Result<Id> {
        let result = self.service_data(
            store,
            &ServiceRequest::ResolveVolumeGrantee {
                operation_id: Id::generate()?.to_string(),
                volume_id: volume.to_string(),
                email: email.into(),
            },
        )?;
        Id::parse(result["account_id"].as_str().ok_or("invalid recipient account response")?)
            .map_err(Into::into)
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn get_account_display_names(
        &mut self,
        store: &LocalStore,
        account_ids: Vec<Id>,
    ) -> Result<Vec<crate::network::ap::operation::AccountDisplayName>> {
        use crate::network::ap::operation::AccountDisplayName;
        let request = ServiceRequest::GetAccountDisplayNames {
            operation_id: Id::generate()?.to_string(),
            account_ids,
        };
        let data = self.service_data(store, &request)?;
        let ServiceRequest::GetAccountDisplayNames { account_ids, .. } = request else {
            unreachable!("request constructed above");
        };
        let names: Vec<AccountDisplayName> = serde_json::from_value(data)?;
        if names.len() > account_ids.len() {
            return Err("too many account display names in reply".into());
        }
        for (index, name) in names.iter().enumerate() {
            if !account_ids.contains(&name.account_id)
                || names[..index].iter().any(|prior| prior.account_id == name.account_id)
            {
                return Err("unexpected account ID in display-name reply".into());
            }
            crate::store::account::validate_display_name(&name.display_name)?;
        }
        Ok(names)
    }
    pub fn get_app(
        &mut self,
        store: &LocalStore,
        app: &crate::package::AppIdentity,
    ) -> Result<serde_json::Value> {
        self.service_data(
            store,
            &ServiceRequest::GetApp { operation_id: Id::generate()?.to_string(), app: app.clone() },
        )
    }
    pub fn list_apps(
        &mut self,
        store: &LocalStore,
        after: Option<String>,
    ) -> Result<serde_json::Value> {
        let value = self.service_data(
            store,
            &ServiceRequest::ListApps {
                operation_id: Id::generate()?.to_string(),
                after: after.clone(),
            },
        )?;
        if value["apps"].as_array().ok_or("invalid app list")?.len() > 32
            || serde_json::to_vec(&value)?.len() > 128 * 1024
        {
            return Err("app list page exceeds response limit".into());
        }
        if let Some(next) = value["next"].as_str() {
            if !crate::package::valid_app_id(next)
                || after.as_deref().is_some_and(|after| next <= after)
            {
                return Err("app cursor did not advance".into());
            }
        } else if !value["next"].is_null() {
            return Err("invalid app cursor".into());
        }
        Ok(value)
    }
    /// Display payloads stay on the bounded background network lane.
    pub fn get_app_display(
        &mut self,
        store: &LocalStore,
        app: &crate::package::AppIdentity,
        build: &str,
    ) -> Result<crate::network::ap::operation::BuildDisplay> {
        let value = self.service_data(
            store,
            &ServiceRequest::GetAppDisplay {
                operation_id: Id::generate()?.to_string(),
                app: app.clone(),
                build_id: build.into(),
            },
        )?;
        let display: crate::network::ap::operation::BuildDisplay =
            serde_json::from_value(value["display"].clone())?;
        display.validate(build)?;
        Ok(display)
    }
    pub fn set_app_enabled(
        &mut self,
        store: &LocalStore,
        app: &crate::package::AppIdentity,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<serde_json::Value> {
        self.service_data(
            store,
            &ServiceRequest::SetAppEnabled {
                operation_id: Id::generate()?.to_string(),
                app: app.clone(),
                enabled,
                expected_revision,
            },
        )
    }
    pub fn delete_app(
        &mut self,
        store: &LocalStore,
        app: &crate::package::AppIdentity,
        expected_revision: u64,
    ) -> Result<serde_json::Value> {
        self.service_data(
            store,
            &ServiceRequest::DeleteApp {
                operation_id: Id::generate()?.to_string(),
                app: app.clone(),
                expected_revision,
            },
        )
    }
    /// The creator's sealed invitation for a pending link, once registered.
    pub fn read_invitation(&mut self, store: &LocalStore, session_id: &str) -> Result<Vec<u8>> {
        let data = self.service_data(
            store,
            &ServiceRequest::ReadInvitation {
                operation_id: Id::generate()?.to_string(),
                session_id: session_id.into(),
                proof: store.pending_invitation_proof(session_id, store.account().id)?,
            },
        )?;
        let envelope = data["envelope"].as_str().ok_or("invalid invitation envelope response")?;
        let envelope = crate::store::parse_envelope_text(envelope)?;
        let registered: Session = serde_json::from_value(data["session"].clone())?;
        store.check_invitation_registration(session_id, &envelope, &registered)?;
        Ok(envelope)
    }
    pub fn resolve_session(
        &mut self,
        store: &LocalStore,
        session_id: &str,
    ) -> Result<serde_json::Value> {
        self.connect(store)?;
        self.sync_registration_dependencies(
            store,
            store.session(session_id)?.replica_volume(),
            Some(session_id),
        )?;
        let resolution = self
            .service_data(
                store,
                &ServiceRequest::ResolveSession {
                    operation_id: Id::generate()?.to_string(),
                    session_id: session_id.into(),
                    proof: store.session_proof(session_id, store.account().id)?,
                },
            )
            .map_err(SessionBuildUnavailable::from_service)?;
        let registered: Session = serde_json::from_value(resolution["session"].clone())?;
        if !store.session(session_id)?.same_identity(&registered) {
            return Err("Retained session differs from authenticated session".into());
        }
        Ok(resolution)
    }
    pub fn build_app_identity(
        &mut self,
        store: &LocalStore,
        build: &str,
    ) -> Result<crate::package::AppIdentity> {
        let value = self.service_data(
            store,
            &ServiceRequest::GetBuildIdentity {
                operation_id: Id::generate()?.to_string(),
                build_id: build.into(),
            },
        )?;
        let app: crate::package::AppIdentity = serde_json::from_value(value["app"].clone())?;
        app.validate()?;
        let admission: Vec<u8> = serde_json::from_value(value["admission"].clone())?;
        store.cache_package_admission(
            &admission,
            crate::identifier::decode::<32>(build)?,
            crate::store::now_unix_secs(),
        )?;
        Ok(app)
    }
    pub fn publish_app(
        &mut self,
        store: &LocalStore,
        operation_id: Id,
        app: &crate::package::AppIdentity,
        build_id: &str,
        notes: &str,
        expected_revision: u64,
    ) -> Result<serde_json::Value> {
        self.service_data(
            store,
            &ServiceRequest::PublishApp {
                operation_id: operation_id.to_string(),
                app: app.clone(),
                build_id: build_id.into(),
                notes: notes.into(),
                expected_revision,
            },
        )
    }
    pub fn advance_session(
        &mut self,
        store: &LocalStore,
        operation_id: Id,
        session_id: &str,
        build_id: &str,
        expected_revision: u64,
    ) -> Result<serde_json::Value> {
        self.service_data(
            store,
            &ServiceRequest::AdvanceSession {
                operation_id: operation_id.to_string(),
                session_id: session_id.into(),
                build_id: build_id.into(),
                expected_revision,
            },
        )
    }
    pub(crate) fn credential_vault(
        &mut self,
        store: &LocalStore,
    ) -> Result<crate::engine::recovery::Vault> {
        self.authenticate(store)?;
        let operation =
            ServiceRequest::GetCredentialVault { operation_id: Id::generate()?.to_string() };
        let connection = self
            .authenticated_transport()
            .expect("the preceding connection check established the AP connection");
        let response = self.runtime.block_on(async {
            let request = wire::request(&connection, &operation);
            let Some(canceled) = &self.canceled else { return request.await };
            tokio::select! {
                biased;
                () = canceled.raising() => Err(CanceledOperation.into()),
                response = request => response,
            }
        })?;
        crate::engine::recovery::Vault::parse(
            response.data.ok_or("missing credential backup response")?,
        )
    }
    pub(crate) fn put_credential_vault(
        &mut self,
        store: &LocalStore,
        upload: &crate::store::RecoveryUpload,
    ) -> Result<crate::engine::recovery::Vault> {
        use crate::store::account_key;
        let state = store.recovery_state()?.ok_or("account key is missing")?;
        let public = account_key::public_key(&state.secret)?;
        let message = account_key::backup_message(
            store.account().id,
            upload.operation_id,
            upload.expected_revision,
            &upload.ciphertext,
        );
        let signature = account_key::sign(&state.secret, &message)?;
        self.authenticate(store)?;
        let response = self.runtime.block_on(wire::request(
            &self
                .authenticated_transport()
                .expect("the preceding connection check established the AP connection"),
            &ServiceRequest::PutCredentialVault {
                public_key: public.iter().map(|b| format!("{b:02x}")).collect(),
                signature: signature.iter().map(|b| format!("{b:02x}")).collect(),
                operation_id: upload.operation_id.to_string(),
                expected_revision: upload.expected_revision,
                ciphertext: upload.ciphertext.clone(),
            },
        ))?;
        crate::engine::recovery::Vault::parse(
            response.data.ok_or("missing credential backup acknowledgment")?,
        )
    }
    /// Durable authentication can precede local binding. Complete interrupted
    /// enrollment on relaunch, only with the same persisted device public key.
    pub(crate) fn restore_profile_binding(&mut self, store: &mut LocalStore) -> Result<()> {
        if self.identity.has_private_key() {
            let expected =
                ClientIdentity::from_seed("", "", store.account_seed()?)?.public_key()?;
            if self.identity.public_key()? != expected {
                return Err("saved authentication belongs to another device identity".into());
            }
        }
        if !self.identity.auth_token.is_empty() {
            let account = Id::from_bytes(self.identity.account_id()?);
            self.replacement_required = store.account_replacement_required(account)?;
            if !self.replacement_required {
                store.bind_authenticated_account(account)?;
            }
        }
        Ok(())
    }
    pub fn enrolled(&self) -> bool {
        !self.replacement_required
            && (self.testing.is_some() || !self.identity.auth_token.is_empty())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn require_replacement(&mut self) {
        self.replacement_required = true;
        self.disconnect();
    }
    pub fn replacement_required(&self) -> bool {
        self.replacement_required
    }
    pub fn email(&self) -> &str {
        &self.identity.email
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn start_invitation_watch(
        &self,
        session: String,
        proof: Vec<u8>,
        envelope: bool,
    ) -> Result<crate::engine::invitation::Watch> {
        crate::engine::invitation::Watch::start(
            self.runtime.handle(),
            self.connection
                .as_ref()
                .ok_or("invitation requires an approved sync connection")?
                .clone(),
            session,
            proof,
            envelope,
            self.completion.clone(),
        )
    }
    pub(crate) fn open(path: &Path, completion: Arc<Wake>) -> Result<Self> {
        let identity_path = path.join("instance.json");
        let channel = super::profile::for_profile(path)?;
        Ok(Self {
            channel,
            #[cfg(feature = "app-host")]
            builds: Arc::default(),
            runtime: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()?,
            ),
            received: Arc::new(crate::engine::replication::ReplicaNotices::new()),
            progress: Arc::new(Progress::default()),
            connection_status: Arc::new(super::ap_status::Status::new(channel.ap_hostname())),
            account_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            onboarding: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            onboarding_account: Arc::new(Mutex::new(None)),
            completion,
            live: None,
            scope: Default::default(),
            gauges: gauges::Gauges::new(),
            publisher: Arc::new(Mutex::new(None)),
            config: None,
            address_lookup: None,
            #[cfg(test)]
            lookup_error: None,
            testing: None,
            #[cfg(target_vendor = "apple")]
            remembered_account: None,
            connection: None,
            pending_drain: None,
            connection_watch: None,
            reconnect: None,
            initializations: Default::default(),
            volume_invitation_watch: None,
            volume_invitations_changed: Default::default(),
            #[cfg(feature = "app-host")]
            missing_owner_backings: Default::default(),
            authenticated_connection: Arc::new(Mutex::new(None)),
            sync_connection: Arc::new(Mutex::new(None)),
            blob_transfers: Arc::new(Default::default()),
            canceled: None,
            listeners: Mutex::new(BTreeMap::new()),
            traffic: BTreeMap::new(),
            selection_watches: BTreeSet::new(),
            availability_checked: false,
            pending_availability: false,
            private_availability: false,
            private_peer_waiting: false,
            private_invitation_waiting: false,
            identity: ClientIdentity::load(&identity_path).map_err(|error| {
                #[cfg(target_vendor = "apple")]
                {
                    Box::<dyn std::error::Error + Send + Sync>::from(
                        crate::store::Error::Credentials(error.to_string()),
                    )
                }
                #[cfg(not(target_vendor = "apple"))]
                Box::<dyn std::error::Error + Send + Sync>::from(error)
            })?,
            identity_path,
            replacement_required: false,
        })
    }
    pub(crate) fn configure_testing(
        &mut self,
        store: &LocalStore,
        credentials: crate::network::ap::hello::TestingCredentials,
    ) -> Result<()> {
        if self.channel != crate::release_channel::ReleaseChannel::Testing {
            return Err(
                "Integration credentials require an isolated testing-channel profile".into()
            );
        }
        if self.identity.auth_token.is_empty() {
            self.identity =
                ClientIdentity::from_seed("", "Playbit testing", store.account_seed()?)?;
        }
        self.testing = Some(credentials);
        self.refresh_connection_status();
        Ok(())
    }
    /// Explicit transport injection for private integration tests.
    pub fn set_test_transport(&mut self, config: ClientConfig) {
        self.disconnect();
        assert!(config.server_name.len() <= super::reply::ApConnectionStatus::HOSTNAME_BYTES_MAX);
        self.config = Some(config);
        self.refresh_connection_status();
    }
    fn config(&mut self) -> Result<&ClientConfig> {
        self.require_online()?;
        if self.config.is_none() {
            let host = self.connection_hostname().to_owned();
            let server_name = host.clone();
            if self.address_lookup.is_none() {
                #[cfg(test)]
                let lookup_error = self.lookup_error.take();
                self.address_lookup = Some(self.runtime.spawn(async move {
                    #[cfg(test)]
                    if let Some(error) = lookup_error {
                        return Err(ClientError::AddressLookup(error));
                    }
                    tokio::net::lookup_host((host.as_str(), 443))
                        .await
                        .map_err(ClientError::AddressLookup)?
                        .find(|address| address.is_ipv4())
                        .ok_or_else(|| {
                            ClientError::AddressLookup(std::io::Error::new(
                                std::io::ErrorKind::AddrNotAvailable,
                                "AP has no IPv4 address",
                            ))
                        })
                }));
            }
            // The platform resolver can outlive a timed-out wait. Retain its
            // task so retries cannot accumulate blocking resolver operations.
            let address = self.runtime.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    self.address_lookup.as_mut().expect("address lookup was started above"),
                )
                .await
            });
            let address = match address {
                Ok(result) => {
                    self.address_lookup = None;
                    result??
                }
                Err(_) => return Err(ClientError::ConnectTimeout.into()),
            };
            let mut config = ClientConfig::platform(address, &server_name);
            config.testing = self.testing.clone();
            self.config = Some(config);
        }
        Ok(self.config.as_ref().expect("configuration was initialized above"))
    }
    pub(crate) fn prepare_sign_in(&mut self) -> Result<()> {
        self.require_online()?;
        let _timing = crate::trace::Interval::new("engine.sign_in.prepare");
        if self.replacement_required {
            return Err("Confirm replacing existing data before signing in".into());
        }
        if self.enrolled() {
            return Err("account is already enrolled".into());
        }
        self.begin_connection();
        let config = self.config()?.clone();
        self.runtime.block_on(ApConnection::prepare_registration(&config))?;
        Ok(())
    }
    pub(crate) fn account_initialized(&self) -> bool {
        self.network_handles().account_initialized()
    }
    pub fn request_code(&mut self, store: &LocalStore, email: &str) -> Result<()> {
        self.require_online()?;
        if self.replacement_required {
            return Err("Confirm replacing existing data before signing in".into());
        }
        if self.testing.is_some() {
            return Err("testing profiles authenticate with their configured key".into());
        }
        if !self.identity.auth_token.is_empty() {
            return Err("account is already enrolled".into());
        }
        self.disconnect();
        self.identity = ClientIdentity::from_seed(email, "Playbit", store.account_seed()?)?;
        // The key precedes the first request and survives a lost registration response.
        self.identity.save(&self.identity_path)?;
        self.begin_connection();
        let config = self.config()?.clone();
        self.runtime
            .block_on(ApConnection::request_registration(&config, &self.identity))
            .inspect_err(|error| {
                log_authentication_protocol_error(&self.identity_path, "request-code", error)
            })?;
        Ok(())
    }
    pub fn sign_in_with_key(
        &mut self,
        store: &LocalStore,
        email: &str,
        seed: &[u8; 16],
        expected_account: Option<Id>,
    ) -> Result<Id> {
        self.require_online()?;
        if self.replacement_required || self.testing.is_some() || self.enrolled() {
            return Err("account is already enrolled".into());
        }
        self.disconnect();
        self.identity = ClientIdentity::from_seed(email, "Playbit", store.account_seed()?)?;
        self.identity.save(&self.identity_path)?;
        self.begin_connection();
        let config = self.config()?.clone();
        let mut identity = self.identity.clone();
        let connection = Self::while_uncanceled(
            &self.runtime,
            self.canceled.as_ref(),
            ApConnection::sign_in_with_key(&config, &mut identity, seed, expected_account),
        )
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
            if let Some(client) = error.downcast_ref::<ClientError>() {
                log_authentication_protocol_error(&self.identity_path, "sign-in-with-key", client);
                if matches!(client, ClientError::InvalidChallenge) {
                    return "Email or account key is incorrect".into();
                }
            }
            error
        })?;
        let account = Id::from_bytes(identity.account_id()?);
        if expected_account.is_some_and(|expected| expected != account) {
            return Err("saved account credential does not match the authenticated account".into());
        }
        let replacement_required = store.account_replacement_required(account)?;
        store.protect_account_key(account, seed)?;
        identity.save(&self.identity_path)?;
        self.identity = identity;
        self.replacement_required = replacement_required;
        // Once AP has issued and we saved credentials, the caller must finish
        // durable account binding even if cancellation won the publication race.
        // It must not publish a transport a concurrent Stop cannot reach.
        if let Err(error) = self.publish_connection(connection)
            && !canceled(error.as_ref())
            && !self.offline()
        {
            return Err(error);
        }
        if self.replacement_required {
            self.disconnect();
        }
        Ok(account)
    }
    #[cfg(target_vendor = "apple")]
    pub(crate) fn saved_account(
        &self,
        store: &LocalStore,
    ) -> Result<Option<super::reply::SavedAccount>> {
        if self.replacement_required {
            return Err("Confirm replacing existing data before recovering an account".into());
        }
        if !crate::credentials::available() {
            return Ok(None);
        }
        let credential = if self.enrolled() {
            crate::credentials::find_account_for(
                store.profile_path(),
                store.account().id,
                self.email(),
            )?
        } else {
            crate::credentials::find_account(store.profile_path())?
        };
        Ok(credential.map(|credential| super::reply::SavedAccount {
            email: credential.email.clone(),
            account_key: crate::store::account_key::format(&credential.secret),
        }))
    }
    #[cfg(target_vendor = "apple")]
    fn account_backup_fingerprint(&self, secret: &[u8; 16]) -> Result<blake3::Hash> {
        let mut fingerprint = blake3::Hasher::new();
        fingerprint.update(&self.identity.account_id()?);
        fingerprint.update(secret);
        Ok(fingerprint.finalize())
    }
    #[cfg(target_vendor = "apple")]
    pub(crate) fn account_backup_pending(&self, store: &LocalStore) -> Result<bool> {
        if !self.enrolled()
            || store.account_key_backup()? != crate::store::AccountKeyBackup::Pending
        {
            return Ok(false);
        }
        let state = store.recovery_state()?.ok_or("account key is missing")?;
        Ok(self.remembered_account != Some(self.account_backup_fingerprint(&state.secret)?))
    }
    #[cfg(target_vendor = "apple")]
    pub(crate) fn remember_account(&mut self, store: &LocalStore, secret: &[u8; 16]) -> Result<()> {
        if !self.account_backup_pending(store)? {
            return Ok(());
        }
        // Bound attempts even if publishing the result fails on local storage.
        self.remembered_account = Some(self.account_backup_fingerprint(secret)?);
        let profile = self.identity_path.parent().ok_or("identity has no profile")?;
        let status = match crate::credentials::remember_account(
            profile,
            Id::from_bytes(self.identity.account_id()?),
            self.email(),
            secret,
        ) {
            Ok(()) => crate::store::AccountKeyBackup::Saved,
            Err(error) => {
                crate::platform::failure::log(profile, "account key backup", &error.to_string());
                crate::store::AccountKeyBackup::Failed
            }
        };
        store.record_account_key_backup(
            Id::from_bytes(self.identity.account_id()?),
            secret,
            status,
        )?;
        Ok(())
    }
    #[cfg(target_vendor = "apple")]
    pub(crate) fn retry_account_key_backup(&mut self, store: &LocalStore) -> Result<()> {
        if !crate::credentials::available() {
            return Err("Saved account recovery is unavailable in this build".into());
        }
        let state = store
            .recovery_state()?
            .filter(|s| s.acknowledged && s.revision > 0)
            .ok_or("Confirm your account key before saving a recovery copy")?;
        store.record_account_key_backup(
            store.account().id,
            &state.secret,
            crate::store::AccountKeyBackup::Pending,
        )?;
        self.remembered_account = None;
        self.remember_account(store, &state.secret)
    }

    pub fn finish_code(&mut self, store: &LocalStore, code: &str) -> Result<Id> {
        self.require_online()?;
        if self.replacement_required {
            return Err("Confirm replacing existing data before signing in".into());
        }
        if self.testing.is_some() {
            return Err("testing profiles authenticate with their configured key".into());
        }
        self.disconnect();
        self.begin_connection();
        let config = self.config()?.clone();
        // Registration is deliberately not cancelable, unlike the ordinary
        // authenticated connect and reconnect: it mints durable account
        // credentials at the AP, and abandoning the exchange would discard
        // credentials the account already owns.
        let mut identity = self.identity.clone();
        let connection = self
            .runtime
            .block_on(ApConnection::finish_registration(&config, &mut identity, code))
            .inspect_err(|error| {
                log_authentication_protocol_error(&self.identity_path, "sign-in-with-code", error)
            })?;
        let account = Id::from_bytes(identity.account_id()?);
        let replacement_required = store.account_replacement_required(account)?;
        identity.save(&self.identity_path)?;
        self.identity = identity;
        self.replacement_required = replacement_required;
        // Enrollment owns the issued credentials even when offline intent
        // arrived during its uncancelable exchange. Retain binding, not transport.
        if let Err(error) = self.publish_connection(connection)
            && !canceled(error.as_ref())
            && !self.offline()
        {
            return Err(error);
        }
        if self.replacement_required {
            self.disconnect();
        }
        Ok(account)
    }
    fn connect(&mut self, store: &LocalStore) -> Result<()> {
        self.require_online()?;
        if self.connection.as_ref().is_some_and(|connection| !connection.is_closed()) {
            return Ok(());
        }
        self.require_sync_agreement(store)
    }
    fn authenticated_transport(&self) -> Option<Arc<ApConnection>> {
        self.authenticated_connection.lock().expect("connection lock is not poisoned").clone()
    }
    fn authenticate(&mut self, store: &LocalStore) -> Result<()> {
        self.require_online()?;
        if self.replacement_required {
            return Err("Confirm replacing existing data before signing in".into());
        }
        if let Some(connection) = self.authenticated_transport() {
            if !connection.is_closed() {
                return Ok(());
            }
            self.disconnect();
        }
        if self.canceled() {
            return Err(CanceledOperation.into());
        }
        self.begin_connection();
        let config = self.config()?.clone();
        if config.server_name.len() > super::reply::ApConnectionStatus::HOSTNAME_BYTES_MAX {
            return Err("AP hostname exceeds the 254-byte limit".into());
        }
        self.progress.set(
            Some(TransferProgress::Connecting { hostname: config.server_name.clone() }),
            &self.completion,
        );
        let result = Self::while_uncanceled(
            &self.runtime,
            self.canceled.as_ref(),
            ApConnection::connect_authenticated(&config, &mut self.identity),
        );
        self.progress.set(None, &self.completion);
        let connection = result?;
        self.identity.save(&self.identity_path)?;
        if self.identity.account_id()? != *store.account().id.as_bytes() {
            self.runtime.block_on(connection.close());
            return Err("AP account differs from local account".into());
        }
        self.publish_connection(connection)
    }
    /// Adopt a freshly authenticated transport. Publishing it under the cancel
    /// handle's lock orders it against cancellation: a cancellation that did
    /// not shut this transport down is one this check has already seen.
    fn publish_connection(&mut self, connection: ApConnection) -> Result<()> {
        let mut bound = self.onboarding_account.lock().expect("onboarding identity lock poisoned");
        let account = if self.testing.is_none() {
            Some(Id::from_bytes(self.identity.account_id()?))
        } else {
            None
        };
        if *bound != account {
            *bound = account;
            self.onboarding.fetch_and(!4, std::sync::atomic::Ordering::AcqRel);
        }
        let connection = Arc::new(connection);
        let mut published = self.authenticated_connection.lock().expect("mutex is not poisoned");
        if self.offline() {
            connection.shutdown();
            return Err(OFFLINE_MODE_ERROR.into());
        }
        if self.canceled() {
            connection.shutdown();
            return Err(CanceledOperation.into());
        }
        *self.sync_connection.lock().expect("connection lock is not poisoned") = None;
        self.connection = None;
        *published = Some(connection.clone());
        self.completion.notify();
        drop(published);
        drop(bound);
        self.watch_connection(connection);
        self.sample_gauges();
        Ok(())
    }
    pub fn blob_transport(&self) -> crate::engine::blob::BlobTransport {
        crate::engine::blob::BlobTransport::new(
            self.runtime.handle().clone(),
            self.sync_connection.clone(),
            self.blob_transfers.clone(),
            #[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
            Some(self.completion.clone()),
        )
    }
    fn connection_hostname(&self) -> &str {
        self.config
            .as_ref()
            .map_or(self.channel.ap_hostname(), |config| config.server_name.as_str())
    }

    pub(super) fn begin_connection(&self) {
        self.connection_status.set(
            ApConnectionState::Connecting,
            self.connection_hostname(),
            &self.completion,
        );
    }

    #[cfg(feature = "file-provider")]
    pub(crate) fn provider_connection_generation(&self) -> Option<u64> {
        self.connection_status.live_generation()
    }

    pub(crate) fn refresh_connection_status(&self) {
        use super::connection_watch::State;
        // A completed request can observe closure before its watcher is polled.
        // Classify the known close now without waiting for another worker turn.
        let transport = self.authenticated_transport();
        let lost = self.connection_watch.as_ref().is_some_and(|watch| match watch.state() {
            State::Lost => true,
            State::Open => transport
                .as_ref()
                .and_then(|connection| connection.close_reason())
                .is_some_and(|reason| ClientError::Connection(reason).is_transport_lost()),
            _ => false,
        });
        let state = if transport.as_ref().is_some_and(|connection| !connection.is_closed()) {
            ApConnectionState::Live
        } else if self.reconnect.is_some() || lost {
            ApConnectionState::Connecting
        } else {
            ApConnectionState::Offline
        };
        // A bulk request may finish while a healthy transport remains watched.
        // Preserve that watch's generation when it still owns the same Live state.
        if state != ApConnectionState::Live || self.connection_watch.is_none() {
            self.connection_status.set(state, self.connection_hostname(), &self.completion);
        }
    }

    fn watch_connection(&mut self, connection: Arc<ApConnection>) {
        let generation = self.connection_status.set(
            ApConnectionState::Live,
            self.connection_hostname(),
            &self.completion,
        );
        self.connection_watch = Some(crate::engine::connection_watch::ConnectionWatch::start(
            self.runtime.handle(),
            connection,
            self.completion.clone(),
            self.connection_status.clone(),
            generation,
        ));
    }
    pub fn disconnect(&mut self) {
        self.volume_invitation_watch = None;
        self.account_ready.store(false, std::sync::atomic::Ordering::Release);
        self.reconnect = None;
        self.initializations.clear();
        #[cfg(feature = "app-host")]
        self.missing_owner_backings.lock().clear();
        self.connection_watch = None;
        self.connection_status.set(
            ApConnectionState::Offline,
            self.connection_hostname(),
            &self.completion,
        );
        self.availability_checked = false;
        if let Some(live) = &mut self.live
            && let Err(error) = live.disconnect()
        {
            crate::diagnostic!("replica disconnect: {error}");
        }
        self.traffic.clear();
        self.selection_watches.clear();
        *self.sync_connection.lock().expect("connection lock is not poisoned") = None;
        self.connection = None;
        let transport = self.authenticated_connection.lock().expect("mutex is not poisoned").take();
        for (_, task) in std::mem::take(&mut *self.listeners.lock().expect("mutex is not poisoned"))
        {
            task.abort();
        }
        if let Some(connection) = transport {
            // Close locally now, so a shared handle held by a background task
            // stops admitting operations from this moment rather than when a
            // task happens to be scheduled. Only flushing the queued
            // CONNECTION_CLOSE frame goes to the network runtime: this runs
            // on synchronous callers such as the owner thread and iOS suspend
            // (which holds the bridge's mutex), so it must return regardless
            // of the network, and without that flush the AP keeps this device
            // marked connected until its own idle timeout. `Drop` joins the
            // handle so a process exiting right after this call does not tear
            // the runtime down before the flush gets to run.
            connection.shutdown();
            self.pending_drain = Some(self.runtime.spawn(async move {
                connection.wait_closed(Duration::from_millis(250)).await;
            }));
        }
        self.sample_gauges();
    }
    #[cfg(feature = "app-host")]
    fn listen_session(&mut self, store: &Arc<LocalStore>, id: &str) -> Result<()> {
        self.connect(store)?;
        let session = store.session(id)?;
        let app_updates_only =
            crate::engine::session_channels::SessionChannels::needs_public_app_observer(
                store, &session,
            )?;
        if self.traffic.get(id).is_none_or(|traffic| traffic.app_updates_only() != app_updates_only)
        {
            let _timing = crate::trace::Interval::new("session.observer.open");
            if !self.traffic.contains_key(id) && self.traffic.len() >= SESSION_OBSERVER_LIMIT {
                return Err(format!(
                    "The session update subscription capacity ({SESSION_OBSERVER_LIMIT}) is full"
                )
                .into());
            }
            let completion = self.completion.clone();
            let traffic =
                self.runtime.block_on(crate::engine::session_channels::SessionChannels::listen(
                    self.connection
                        .as_ref()
                        .expect("the preceding connection check established the AP connection")
                        .clone(),
                    store.clone(),
                    session,
                    move || completion.notify(),
                ))?;
            // Keep the old public observer until the full replacement has
            // acknowledged its subscription. Failed preparation loses no code wakeups.
            self.traffic.insert(id.into(), traffic);
        }
        Ok(())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn selection_updates(&self, active: &[String]) -> Vec<String> {
        if self.connection.as_ref().is_none_or(|connection| connection.is_closed()) {
            return vec![];
        }
        active
            .iter()
            .filter(|id| {
                !self.selection_watches.contains(*id)
                    || self.traffic.get(*id).is_some_and(|traffic| traffic.has_update())
            })
            .cloned()
            .collect()
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn retire_selection_watches(&mut self, active: &[String]) {
        self.selection_watches.retain(|id| {
            if active.contains(id) {
                true
            } else {
                self.traffic.remove(id);
                false
            }
        });
    }
    #[cfg(feature = "app-host")]
    fn rearm_selection_watches(&mut self, store: &LocalStore, volume: Id) -> Result<()> {
        // First use can publish a lazy session after its initial watch chose no
        // observer (Exact) or only public app updates. Revisit those decisions
        // once the backing is admitted, preserving established session observers.
        for id in self.selection_watches.clone() {
            if self.traffic.get(&id).is_some_and(|traffic| !traffic.app_updates_only()) {
                continue;
            }
            let session = match store.session(&id) {
                Ok(session) => session,
                Err(crate::store::Error::NotFound(_)) => {
                    // Local deletion can precede retirement by the active owner.
                    self.traffic.remove(&id);
                    self.selection_watches.remove(&id);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if session.replica_volume() == volume {
                // Keep public traffic until listen_session acknowledges its
                // replacement; the ordinary owner continuation arms that watch.
                self.selection_watches.remove(&id);
            }
        }
        Ok(())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn watch_selection(
        &mut self,
        store: &Arc<LocalStore>,
        id: &str,
        active: &[String],
    ) -> Result<()> {
        self.traffic.retain(|id, _| active.contains(id));
        self.selection_watches.retain(|id| active.contains(id));
        self.selection_watches.insert(id.into());
        let session = store.session(id)?;
        let app_updates_only =
            crate::engine::session_channels::SessionChannels::needs_public_app_observer(
                store, &session,
            )?;
        if app_updates_only && matches!(session.selector, crate::store::BuildSelector::Exact { .. })
        {
            return Ok(());
        }
        if let Some(traffic) = self.traffic.get(id)
            && traffic.app_updates_only() == app_updates_only
        {
            if traffic.take_update() & 2 != 0 {
                self.traffic.remove(id);
                return Err("Session update connection stopped; reconnect to resume updates".into());
            }
        } else {
            // New owner sessions have durable registration/upload work. Finish
            // that initialization before subscribing; no participant input waits.
            if store.pending_operations()?.iter().any(|operation|
                matches!(&operation.operation, PendingKind::RegisterSession { session, .. } if session.id == id)) {
                let publication = self.prepare_publication(store, Some(session.replica_volume()));
                // Publication can rearm this same watch. This attempt owns its
                // outcome, including failure; do not schedule a hot retry loop.
                self.selection_watches.insert(id.into());
                publication?;
            }
            self.listen_session(store, id)?;
        }
        Ok(())
    }
    pub fn usage(&mut self, store: &LocalStore, volume: Id) -> Result<super::reply::VolumeUsage> {
        self.connect(store)?;
        let response = self.runtime.block_on(wire::request(
            self.connection
                .as_ref()
                .expect("the preceding connection check established the AP connection"),
            &ServiceRequest::VolumeUsage {
                operation_id: Id::generate()?.to_string(),
                volume_id: volume.to_string(),
            },
        ))?;
        let data = response.data.ok_or("missing usage response")?;
        Ok(serde_json::from_value(data)?)
    }
    pub fn sync(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        let mut pending = self.synchronize_next(store, None)?;
        while let Some(continuation) = pending {
            pending = self.synchronize_next(store, Some(continuation))?;
        }
        Ok(())
    }
    pub(super) fn synchronize_next(
        &mut self,
        store: &Arc<LocalStore>,
        continuation: Option<Synchronization>,
    ) -> Result<Option<Synchronization>> {
        if let Some(mut continuation) = continuation {
            if self.canceled() {
                return Err(CanceledOperation.into());
            }
            return if continuation.advance(self, store)? {
                Ok(None)
            } else {
                Ok(Some(continuation))
            };
        }
        let timing = crate::trace::Interval::new("replication.sync");
        self.begin_synchronization(store)?;
        Ok(Some(Synchronization::new(store, self.scope.read().clone(), timing)?))
    }
    fn begin_synchronization(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        self.initializations.restart(Instant::now(), |volume| {
            self.live.as_ref().is_some_and(|live| live.installed_scope(volume).is_some())
        });
        // Before the first live driver exists, there is no connection watcher
        // to schedule recovery. Retain only a failed establishment attempt;
        // storage/authentication and bootstrap failures must not be replayed.
        let initial = self.enrolled() && self.live.is_none();
        let retry = if initial { self.reconnect.take() } else { None };
        if let Err(error) = self.require_sync_agreement(store) {
            if canceled(error.as_ref()) {
                // Nothing was attempted, so nothing was ruled out: give the
                // pending attempt back instead of retiring it with this error.
                // Only what this call took: `retry` is None unless `initial`.
                if let Some(retry) = retry {
                    self.reconnect = Some(retry);
                }
                return Err(error);
            }
            if initial
                && error
                    .downcast_ref::<crate::network::ap::ClientError>()
                    .is_some_and(|error| error.is_connection_retryable())
            {
                let mut retry = retry.unwrap_or_else(|| Reconnect::new(vec![]));
                retry.bootstrap = true;
                retry.failed(Instant::now());
                self.reconnect = Some(retry);
            }
            self.refresh_connection_status();
            return Err(error);
        }
        self.sync_registrations(
            store,
            self.connection.as_ref().expect("connect established the AP connection"),
            self.scope.read().selected(),
        )?;
        self.ensure_live(store)?;
        self.live
            .as_mut()
            .expect("account binding initialized live replication")
            .reset_recovery_attempts();
        self.consume_publisher_failures()?;
        self.availability_checked = false;
        self.progress.set(Some(TransferProgress::Account), &self.completion);
        // Explicit sync retries any exhausted owner deliveries. Preparing an app
        // or a publication can reuse the inbox without fetching all grants.
        if let Some(watch) = &self.volume_invitation_watch {
            watch.rescan.notify_one();
        }
        let account = self.begin_account_sync(store)?;
        self.finish_account_sync(store, account)?;
        Ok(())
    }
    fn finish_synchronization(
        &mut self,
        store: &LocalStore,
        volumes: Result<()>,
        targets: &[Id],
        unavailable_sessions: &BTreeSet<String>,
    ) -> Result<()> {
        self.progress.set(Some(TransferProgress::Finishing), &self.completion);
        let result = volumes
            .and_then(|_| self.live_fence(store, Some(targets), Some(unavailable_sessions), false))
            .and_then(|_| self.refresh_availability(store));
        match &result {
            Ok(()) => self.private_peer_waiting = false,
            Err(error) if waiting_peer(error.as_ref()) => {
                if sync_failure::volume_id(error.as_ref())
                    .is_some_and(|id| store.has_direct_volume_grant(id).unwrap_or(false))
                {
                    self.private_invitation_waiting = true;
                } else {
                    self.private_peer_waiting = true;
                }
            }
            Err(_) => {}
        }
        self.traffic.retain(|session, _| {
            store.session_denial(session).is_ok_and(|denial| denial.is_none())
        });
        // A denied or unavailable session must not tear down healthy sessions'
        // presence and peer listeners on the account connection.
        if self.connection.as_ref().is_some_and(|connection| connection.is_closed()) {
            self.disconnect();
        }
        result
    }

    pub(crate) fn needs_gap_recovery(&self) -> bool {
        self.reconnect.is_none()
            && self.connection.as_ref().is_some_and(|connection| !connection.is_closed())
            && self.live.as_ref().is_some_and(|live| {
                live.needs_gap_recovery()
                    || live.readiness_wake_after(Instant::now()) == Some(Duration::ZERO)
            })
    }

    pub(crate) fn reconnect_ready(&self, now: Instant) -> bool {
        if self.offline() {
            return false;
        }
        self.reconnect.as_ref().map_or_else(
            || {
                self.connection_watch.as_ref().is_some_and(|watch| {
                    watch.state() == crate::engine::connection_watch::State::Lost
                })
            },
            |retry| !retry.bootstrap && retry.after <= now,
        )
    }
    pub(crate) fn bootstrap_retry_ready(&self, now: Instant) -> bool {
        !self.offline()
            && self.reconnect.as_ref().is_some_and(|retry| retry.bootstrap && retry.after <= now)
    }
    pub(crate) fn reconnect_after(&self, now: Instant) -> Option<Duration> {
        self.reconnect.as_ref().map(|retry| retry.after.saturating_duration_since(now))
    }

    pub(crate) fn wake_after(&self, now: Instant) -> Option<Duration> {
        if self.offline() {
            return None;
        }
        let source = (self.reconnect.is_none()
            && self.connection.as_ref().is_some_and(|connection| !connection.is_closed()))
        .then(|| self.initializations.wake_after(now))
        .flatten();
        let readiness = (self.reconnect.is_none()
            && self.connection.as_ref().is_some_and(|connection| !connection.is_closed()))
        .then(|| self.live.as_ref().and_then(|live| live.readiness_wake_after(now)))
        .flatten();
        self.reconnect_after(now).into_iter().chain(source).chain(readiness).min()
    }

    /// Transport recovery retries connection establishment. Ordinary scope
    /// failures retain their latch; explicit initial source readiness has its
    /// own bounded episode, independent of transport-loss classification.
    pub(crate) fn reconnect(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        let _timing = crate::trace::Interval::new("replication.transport_recovery");
        // Claiming the episode below is destructive: it consumes the transport
        // loss and takes the scopes that need recovery out of the live driver.
        // A canceled operation must not claim it, or the cancellation silently
        // retires a recovery nothing will schedule again.
        if self.canceled() {
            return Err(CanceledOperation.into());
        }
        let mut retry = if let Some(retry) = self.reconnect.take() {
            retry
        } else {
            if !self.connection_watch.as_ref().is_some_and(|watch| watch.take_loss()) {
                return Err("AP transport loss was already handled or is terminal".into());
            }
            let mut retry = Reconnect::new(self.live.as_mut().map_or_else(Vec::new, |live| {
                live.take_recoveries(crate::engine::live::RecoveryKind::Transport)
            }));
            retry.account = self.live.as_mut().is_some_and(|live| {
                live.take_account_recovery(crate::engine::live::RecoveryKind::Transport)
            });
            retry
        };
        let connected = self.reconnect_transport(store);
        if let Err(error) = connected {
            if canceled(error.as_ref()) {
                // The episode outlives the cancellation, with no backoff: the
                // transport never answered, so nothing failed to be retried.
                self.reconnect = Some(retry);
            } else if error
                .downcast_ref::<crate::network::ap::ClientError>()
                .is_some_and(|error| error.is_connection_retryable())
            {
                retry.failed(Instant::now());
                self.reconnect = Some(retry);
            }
            self.refresh_connection_status();
            return Err(error);
        }
        let mut first_error = None;
        if retry.account
            && let Err(error) =
                self.recover_account(store, crate::engine::live::RecoveryKind::Transport)
        {
            first_error = Some(error);
        }
        for scope in retry.scopes {
            if let Err(error) =
                self.recover_scope(store, &scope, crate::engine::live::RecoveryKind::Transport)
            {
                first_error.get_or_insert(error);
            }
        }
        // Peer and session-event listeners are restored by their existing
        // lifecycle callers; a replica retry must not force unrelated admission.
        if let Err(error) = self.start_volume_invitations(store) {
            first_error.get_or_insert(error);
        }
        let availability = self.refresh_availability(store);
        first_error.map_or(availability, Err)
    }

    fn reconnect_transport(&mut self, store: &LocalStore) -> Result<()> {
        self.require_online()?;
        if self.canceled() {
            return Err(CanceledOperation.into());
        }
        self.begin_connection();
        let config = self.config()?.clone();
        let previous = self.authenticated_transport().ok_or("lost AP connection is unavailable")?;
        let connection = Self::while_uncanceled(
            &self.runtime,
            self.canceled.as_ref(),
            previous.reconnect_authenticated(&config, &mut self.identity),
        )?;
        if self.identity.account_id()? != *store.account().id.as_bytes() {
            connection.shutdown();
            return Err("AP account differs from local account".into());
        }
        self.identity.save(&self.identity_path)?;
        // Do not call LiveReplicas::disconnect here: its cancellation would
        // erase the eligible scope observations captured from transport loss.
        self.connection_watch = None;
        self.traffic.clear();
        self.selection_watches.clear();
        for (_, task) in std::mem::take(&mut *self.listeners.lock().expect("mutex is not poisoned"))
        {
            task.abort();
        }
        self.publish_connection(connection)?;
        self.confirm_sync_agreement(store)
    }

    /// One episode on the existing network worker. Never admit unrelated
    /// sessions, fetch apps, back up credentials or synchronize private peers.
    pub(crate) fn recover_gaps(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        let _timing = crate::trace::Interval::new("replication.gap_recovery");
        let live = self.live.as_mut().ok_or("live replication is unavailable")?;
        let mut scopes = live.take_recoveries(crate::engine::live::RecoveryKind::Gap);
        live.append_readiness_recoveries(&mut scopes, Instant::now());
        assert!(scopes.len() <= crate::engine::live::SCOPE_LIMIT);
        let mut first_error = None;
        if self
            .live
            .as_mut()
            .expect("account binding initialized live replication")
            .take_account_recovery(crate::engine::live::RecoveryKind::Gap)
            && let Err(error) = self.recover_account(store, crate::engine::live::RecoveryKind::Gap)
        {
            first_error = Some(error);
        }
        for scope in scopes {
            if let Err(error) =
                self.recover_scope(store, &scope, crate::engine::live::RecoveryKind::Gap)
            {
                initialization::retain_error(&mut first_error, error);
            }
        }
        if let Err(error) = self.refresh_availability(store) {
            initialization::retain_error(&mut first_error, error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn recover_scope(
        &mut self,
        store: &Arc<LocalStore>,
        scope: &crate::engine::live::ScopeKey,
        kind: crate::engine::live::RecoveryKind,
    ) -> Result<()> {
        self.recover_scope_with(store, scope, kind, Self::recover_scope_once)
            .map_err(|error| sync_failure::recovery(store, scope, error))
    }

    fn recover_scope_with(
        &mut self,
        store: &Arc<LocalStore>,
        scope: &crate::engine::live::ScopeKey,
        mut kind: crate::engine::live::RecoveryKind,
        mut capture: impl FnMut(
            &mut Self,
            &Arc<LocalStore>,
            &crate::engine::live::ScopeKey,
            crate::engine::live::RecoveryKind,
        ) -> Result<()>,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("replication.recover_scope");
        crate::trace::mark(match kind {
            crate::engine::live::RecoveryKind::Gap => "replication.recover_gap_scope",
            crate::engine::live::RecoveryKind::Transport => "replication.recover_transport_scope",
        });
        let Some(mut gaps) = self
            .live
            .as_mut()
            .ok_or("live replication is unavailable")?
            .start_readiness(scope, Instant::now())?
        else {
            return Ok(());
        };
        while gaps < crate::engine::live::GAP_CAPTURE_LIMIT {
            let result = capture(self, store, scope, kind);
            match result {
                Err(error)
                    if sync_failure::reason(error.as_ref())
                        .is::<crate::engine::live::LiveGap>()
                        && self.live.as_ref().is_some_and(|live| {
                            live.recovery_is_current(scope, crate::engine::live::RecoveryKind::Gap)
                        }) =>
                {
                    gaps += 1;
                    if gaps == crate::engine::live::GAP_CAPTURE_LIMIT {
                        self.live
                            .as_mut()
                            .expect("account binding initialized live replication")
                            .exhaust_recovery(scope);
                        return Err(sync_failure::exhausted_gap(error));
                    }
                    crate::trace::mark("replication.bootstrap_gap_restart");
                    kind = crate::engine::live::RecoveryKind::Gap;
                }
                Ok(()) => {
                    self.live
                        .as_mut()
                        .expect("live recovery owns its entry")
                        .clear_readiness(scope);
                    crate::trace::mark("replication.recovery_complete");
                    return Ok(());
                }
                Err(error)
                    if matches!(kind, crate::engine::live::RecoveryKind::Gap) && {
                        let reason = sync_failure::reason(error.as_ref());
                        !reason.is::<crate::engine::live::LiveGap>()
                            && matches!(
                                reason.to_string().as_str(),
                                "waiting for initial database synchronization"
                                    | "waiting for initial volume synchronization"
                            )
                    } =>
                {
                    let scheduled = self
                        .live
                        .as_mut()
                        .expect("live recovery owns its entry")
                        .defer_readiness(scope, gaps, Instant::now())?;
                    return Err(if scheduled {
                        Box::new(initialization::Pending(sync_failure::recovery(
                            store, scope, error,
                        )))
                    } else {
                        error
                    });
                }
                Err(error) => {
                    self.live
                        .as_mut()
                        .expect("live recovery owns its entry")
                        .clear_readiness(scope);
                    return Err(error);
                }
            }
        }
        unreachable!()
    }

    fn recover_scope_once(
        &mut self,
        store: &Arc<LocalStore>,
        scope: &crate::engine::live::ScopeKey,
        kind: crate::engine::live::RecoveryKind,
    ) -> Result<()> {
        if !self.live.as_ref().is_some_and(|live| {
            live.recovery_is_current(scope, kind) || live.readiness_running(scope)
        }) {
            return Err("replica recovery scope changed or failed before bootstrap".into());
        }
        let connection = self
            .connection
            .as_ref()
            .filter(|connection| !connection.is_closed())
            .ok_or("AP disconnected during replica recovery")?
            .clone();
        let volume = store.volume(scope.volume)?;
        if !volume.share_key_with_service {
            return Err("replica requires an authorized private peer".into());
        }
        let session = scope
            .session
            .as_ref()
            .map(|(id, version)| {
                let mut session = store.session(&crate::identifier::encode(id))?;
                if scope.entry.is_none()
                    && session.replica_volume() != volume.id
                    && !session.is_distinct_world(volume.id)
                {
                    return Err("replica root changed before recovery".into());
                }
                // Recover the captured stream's authority, including its build.
                session.resolved_build_id = crate::identifier::encode(version);
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(session)
            })
            .transpose()?;
        if let Some(session) = &session {
            let invited_world =
                session.is_distinct_world(volume.id) && session.app.publisher != store.account().id;
            if invited_world || scope.entry.is_none() {
                self.admit(store, session).map_err(|error| {
                    sync_failure::volume(store, session.replica_volume(), error)
                })?;
            }
            if invited_world {
                return self.with_invited_world(store, session, |remote| {
                    remote
                        .capture_recovery(store, scope, volume, Some(session), connection)
                        .map_err(|error| sync_failure::recovery(store, scope, error))
                });
            }
        }
        self.capture_recovery(store, scope, volume, session.as_ref(), connection)
    }

    fn capture_recovery(
        &mut self,
        store: &Arc<LocalStore>,
        scope: &crate::engine::live::ScopeKey,
        volume: crate::store::Volume,
        session: Option<&Session>,
        connection: Arc<ApConnection>,
    ) -> Result<()> {
        if let Some(entry) = scope.entry {
            return self.synchronize_entry(
                store,
                crate::store::volume_index::EntryAddress { volume: volume.id, entry },
                session,
            );
        }
        let pending = if let Some(session) = session {
            let _attempt = crate::trace::Span::new("replication.recover_root_attempt");
            self.recover_bound_volume(store, session, volume.id, connection)?
        } else {
            let _attempt = crate::trace::Span::new("replication.recover_owner_attempt");
            if volume.owner == store.account().id {
                self.bootstrap_owner_volume(store, volume)?
            } else if store.has_direct_volume_grant(volume.id)? {
                self.capture_direct_volume(store, volume.id)?
            } else {
                return Err("replica account no longer has a direct volume grant".into());
            }
        };
        let live = self.live.as_mut().expect("account binding initialized live replication");
        self.runtime.block_on(async {
            let fenced = live.publisher().fence(&[scope.volume]).await?;
            live.checkpoint(&fenced)
                .await
                .and_then(|()| live.complete_recovery(scope))
                .map_err(|error| sync_failure::captured_backing_gap(scope.volume, pending, error))
        })?;
        if session.is_none() {
            if pending.is_some() {
                self.initializations.failed(
                    scope.volume,
                    "waiting for initial database synchronization",
                    Instant::now(),
                )?;
            } else {
                self.initializations.forget(scope.volume);
            }
        }
        Ok(())
    }

    fn recover_bound_volume(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        volume: Id,
        connection: Arc<ApConnection>,
    ) -> Result<Option<crate::store::DatabaseId>> {
        if store.authority_refresh_required(volume)? {
            self.pull_bound_volume(store, session, volume)?;
        }
        let mut prepared = self.runtime.block_on(
            self.live.as_mut().expect("account binding initialized live replication").begin(
                connection,
                crate::engine::replication::ReplicaScope::Volume {
                    volume,
                    session: Some(session.clone()),
                },
            ),
        )?;
        // Root recovery receives current revocations before admitting data.
        let received = self.pull_bound_volume(store, session, volume)?;
        let access =
            Access::session_version(store.account().id, &session.id, &session.resolved_build_id);
        store.authorize_replication(&access, volume, Permission::Read)?;
        if !self.service_covers_reader(store, volume, Some(session))? {
            return Err("replica requires an authorized private peer".into());
        }
        self.runtime.block_on(
            self.live
                .as_mut()
                .expect("account binding initialized live replication")
                .rebase(&mut prepared),
        )?;
        if self.push_bootstrap_participant(store, session, received)? {
            self.runtime.block_on(
                self.live.as_mut().expect("live replication initialized").rebase(&mut prepared),
            )?;
        }
        self.runtime.block_on(
            self.live
                .as_mut()
                .expect("account binding initialized live replication")
                .subscribe(&mut prepared),
        )?;
        let received = self.pull_bound_volume(store, session, volume)?;
        let pending = received.pending_backing;
        self.finish_captured_live(store, Some((session, received)), volume, prepared)?;
        if pending.is_some() {
            // The installed stream can serve its ready siblings during bounded recovery.
            self.initializations.failed(
                volume,
                "waiting for initial database synchronization",
                Instant::now(),
            )?;
        }
        Ok(pending)
    }

    /// Non-replica facts are inspected only by the network worker. The owner
    /// reads these conservative limitations together with live driver state.
    fn refresh_availability(&mut self, store: &LocalStore) -> Result<()> {
        let _timing = crate::trace::Span::new("replication.availability_facts");
        let mut pending = !store.pending_operations()?.is_empty();
        let mut private = false;
        let mut invitation_waiting = false;
        for volume in remote_volumes(store)? {
            if !self.scope.read().admits(volume.id) {
                continue;
            }
            let scope_error = |error| sync_failure::volume(store, volume.id, error);
            if volume.personal
                && !volume.share_key_with_service
                && volume.owner != store.account().id
                && store.has_direct_volume_grant(volume.id).map_err(scope_error)?
            {
                invitation_waiting |= !self.private_invitation_connected(volume.id);
            }
            if store
                .replica_root(volume.id, crate::store::DatabaseId::Index)
                .map_err(scope_error)?
                .is_none()
            {
                // Credentials may arrive before authorization/data. Do not
                // inspect blob policy or fail unrelated initialized sessions.
                pending = true;
                private |= !volume.share_key_with_service;
                continue;
            }
            private |= !volume.share_key_with_service
                || store.has_service_excluded_data(volume.id).map_err(scope_error)?;
            if volume.personal
                && !volume.share_key_with_service
                && volume.owner == store.account().id
            {
                // The owner retains these bytes to serve authenticated peer reads.
                continue;
            }
            let access = Access::owner(store.account().id);
            match store.pending_entry_blobs(&access, volume.id, None) {
                Ok(page) => pending |= !page.versions.is_empty(),
                // A retained copy has no pending unscoped uploads after withdrawal.
                Err(crate::store::Error::AccessDenied("direct volume grant withdrawn")) => {}
                Err(crate::store::Error::AccessDenied(
                    "direct volume grant requires current permissions",
                )) => pending = true,
                Err(error) => return Err(scope_error(error)),
            }
        }
        self.pending_availability = pending;
        self.private_availability = private;
        self.private_invitation_waiting = invitation_waiting;
        self.availability_checked = true;
        Ok(())
    }

    pub(crate) fn availability(&self, store: &LocalStore) -> &'static str {
        if self.offline() {
            return "Offline mode; local data is retained";
        }
        if self.reconnect.is_some() {
            return "Waiting to reconnect; local data is retained";
        }
        match self.connection_watch.as_ref().map(|watch| watch.state()) {
            Some(
                crate::engine::connection_watch::State::Lost
                | crate::engine::connection_watch::State::HandledLoss,
            ) => return "Disconnected; local data is retained",
            Some(crate::engine::connection_watch::State::Failed) => {
                return "AP connection failed; local data is retained";
            }
            _ => {}
        }
        if self.connection.as_ref().is_none_or(|connection| connection.is_closed()) {
            return "Offline; local data is retained";
        }
        if let Some(error) = self.live.as_ref().and_then(|live| live.bootstrap_failure()) {
            return error;
        }
        let health = self
            .live
            .as_ref()
            .map(|live| live.availability(store.change_status(), store.pending_change_count()));
        if let Some(
            health @ ("Synchronization failed; local data is retained" | "Access revoked"),
        ) = health
        {
            return health;
        }
        // Unrelated hosted publication/recovery must preserve a known private
        // peer wait. Only a successful full synchronization clears this fact.
        if self.private_peer_waiting || self.private_invitation_waiting {
            return "Waiting for an authorized peer";
        }
        if !self.availability_checked {
            return "Waiting to synchronize";
        }
        if self.pending_availability {
            return "Pending remote availability";
        }
        let Some(health) = health else {
            return "Waiting to synchronize";
        };
        if health == "Connected" && self.private_availability {
            "Connected; private data requires an authorized peer"
        } else {
            health
        }
    }
    /// Publication needs registered app identities and current owner-granted
    /// service policy. It must not depend on another session's private peer.
    #[cfg(feature = "app-host")]
    pub(crate) fn prepare_publication(
        &mut self,
        store: &Arc<LocalStore>,
        target: Option<Id>,
    ) -> Result<()> {
        self.connect(store)?;
        self.ensure_live(store)?;
        self.consume_publisher_failures()?;
        let account = self.begin_account_sync(store)?;
        self.finish_account_sync(store, account)?;
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        let mut deferred = self.sync_registrations(store, connection, None)?;
        // An older service may still publish a build or a grant on another volume.
        // Fence the supported replicas. Keep account records that depend on a
        // deferred Shared volume in their durable outbox until it can be admitted.
        let owners = self.sync_registered_owners(store, None, &mut deferred);
        if let Some(id) = target.filter(|id| deferred.contains(id)) {
            return Err(sync_failure::volume(store, id, wire::SharedServiceUpgradeRequired));
        }
        owners?;
        let supported = if deferred.is_empty() {
            None
        } else {
            Some(
                remote_volumes(store)?
                    .into_iter()
                    .map(|volume| volume.id)
                    .filter(|id| !deferred.contains(id))
                    .collect::<Vec<_>>(),
            )
        };
        self.live_fence(store, supported.as_deref(), None, !deferred.is_empty())?;
        self.refresh_availability(store)
    }
    pub(crate) fn needs_initialization(&self) -> bool {
        self.reconnect.is_none()
            && self.connection.as_ref().is_some_and(|connection| !connection.is_closed())
            && (self.live.as_ref().is_some_and(|live| live.publisher().needs_initialization())
                || self.initializations.wake_after(Instant::now()) == Some(Duration::ZERO)
                || self.blob_transfers.logs.needs_preparation()
                || self.peer_invitations_ready())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn initialization_retrying(
        &self,
        error: &(dyn std::error::Error + 'static),
    ) -> bool {
        self.initializations.retrying(error)
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn gap_readiness_retrying(&self, error: &(dyn std::error::Error + 'static)) -> bool {
        error.is::<initialization::Pending>()
            && self
                .live
                .as_ref()
                .is_some_and(|live| live.readiness_wake_after(Instant::now()).is_some())
    }
    pub(crate) fn initialize_volumes(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        // Accepted bloblog calls wait here for registration and connection-bound
        // session admission, before sending an append that might commit remotely.
        let calls = self.blob_transfers.logs.clone();
        for _ in 0..super::bloblog::Limit::Calls as usize {
            let Some(request) = calls.take_preparation()? else { break };
            if request.canceled() {
                continue;
            }
            let result = (|| -> Result<()> {
                if self
                    .connection
                    .as_ref()
                    .is_none_or(|connection| !Arc::ptr_eq(connection, &request.connection))
                    || request.connection.is_closed()
                {
                    return Err("bloblog connection changed before preparation".into());
                }
                let mut volumes = vec![request.volume];
                if let Some(session) = &request.session
                    && session.replica_volume() != request.volume
                {
                    volumes.push(session.replica_volume());
                }
                if volumes.iter().any(|volume| !self.scope.read().admits(*volume)) {
                    return Err(
                        crate::store::Error::AccessDenied("bloblog preparation scope").into()
                    );
                }
                if let Some(session) = &request.session
                    && session.is_distinct_world(request.volume)
                {
                    // The ready log already has a local World binding. Its AP
                    // request checks the current entry grant under this session.
                    // Unrelated backings retain their own initialization retries.
                    return self.admit(store, session);
                }
                self.sync_owned(store, &volumes)?;
                if let Some(session) = &request.session {
                    self.sync_required_session(store, session)?;
                }
                Ok(())
            })();
            request.finish(&calls, result);
        }
        let publisher = self.live.as_ref().ok_or("live replication is unavailable")?.publisher();
        let controls = self.retry_world_controls(store);
        if controls.is_err()
            && self.connection.as_ref().is_none_or(|connection| connection.is_closed())
        {
            return controls.map(|_| ());
        }
        let mut volumes = publisher.take_initializations();
        for id in self.take_ready_peer_invitations(super::publisher::VOLUME_LIMIT - volumes.len()) {
            if store.has_direct_volume_grant(id)? {
                if !volumes.contains(&id) {
                    volumes.push(id);
                }
            } else if store.volume(id).is_ok_and(|v| v.owner != store.account().id && v.personal) {
                self.retire_direct_grant(id)?;
                volumes.retain(|v| *v != id);
            }
        }
        self.initializations.select(&mut volumes, Instant::now());
        volumes.retain(|volume| self.scope.read().admits(*volume));
        if volumes.is_empty() && matches!(controls, Ok(false)) {
            return Ok(());
        }
        let mut first_error = controls.err();
        for (index, volume) in volumes.iter().enumerate() {
            self.initializations.start(*volume);
            // Discovery can precede a tombstone in the same account journal.
            // Retire that stale notice instead of failing unrelated live data.
            if store.volume_deleted(*volume)? {
                self.initializations.forget(*volume);
                self.runtime.block_on(publisher.forget_scope((*volume).into()))?;
                continue;
            }
            let result = (|| -> Result<()> {
                let descriptor = store.volume(*volume)?;
                if descriptor.owner == store.account().id {
                    self.sync_owned(store, std::slice::from_ref(volume))
                } else if store.has_direct_volume_grant(*volume)? {
                    self.sync_direct_volume(store, *volume)
                } else if self.has_live_volume(store, *volume)?
                    && !self.initializations.tracked(*volume)
                {
                    Ok(())
                } else {
                    let captured = self
                        .live
                        .as_ref()
                        .and_then(|live| live.installed_scope(*volume))
                        .and_then(|scope| scope.session);
                    let session = Self::initialization_session(store, *volume, captured)?;
                    if session.is_distinct_world(*volume) {
                        self.sync_optional_world(store, &session)
                    } else {
                        self.sync_required_session(store, &session)
                    }
                }
            })();
            if let Err(error) = result {
                if error.is::<SessionBuildUnavailable>() {
                    // This root still lacks admission. Keep its local data and
                    // let a later explicit synchronization retry it.
                    self.initializations.forget(*volume);
                    continue;
                }
                if self.connection.as_ref().is_none_or(|connection| connection.is_closed())
                    && sync_failure::reason(error.as_ref())
                        .downcast_ref::<ClientError>()
                        .is_some_and(ClientError::is_transport_lost)
                {
                    publisher.retry_initializations_after_transport_loss(&volumes[index..]);
                    return Err(error);
                }
                let error = sync_failure::volume(store, *volume, error);
                if !self
                    .has_live_volume(store, *volume)
                    .map_err(|error| sync_failure::volume(store, *volume, error))?
                {
                    self.runtime.block_on(publisher.fail_initialization(
                        *volume,
                        &sync_failure::reason(error.as_ref()).to_string(),
                    ))?;
                }
                initialization::retain_error(&mut first_error, error);
            } else {
                self.initializations.forget(*volume);
                if !store.volume(*volume)?.share_key_with_service
                    && !store.has_direct_volume_grant(*volume)?
                {
                    self.private_peer_waiting = true;
                }
            }
        }
        if let Err(error) = self.refresh_availability(store) {
            initialization::retain_error(&mut first_error, error);
        }
        first_error.map_or(Ok(()), Err)
    }
    fn initialization_session(
        store: &LocalStore,
        volume: Id,
        captured: Option<([u8; 32], [u8; 32])>,
    ) -> Result<Session> {
        // A missing named backing shares its already installed World's exact authority.
        // A second session can open the same World with a different, unprovisioned build.
        if let Some((id, version)) = captured {
            let mut session = store.session(&crate::identifier::encode(&id))?;
            session.resolved_build_id = crate::identifier::encode(&version);
            let access = Access::session_version(
                store.account().id,
                &session.id,
                &session.resolved_build_id,
            );
            if store.authorize_replication(&access, volume, Permission::Read).is_ok() {
                return Ok(session);
            }
        }
        let mut uncaptured = None;
        for session in store.sessions()? {
            if store.session_denial(&session.id)?.is_some() {
                continue;
            }
            if session.replica_volume() == volume {
                return Ok(session);
            }
            if session.is_distinct_world(volume) && !store.world_denied(&session)? {
                if store.invited_world_ready(&session.id, volume)? {
                    return Ok(session);
                }
                // Account discovery can precede the initial World catalog. Open
                // will authorize this route if no previously qualified one exists.
                uncaptured.get_or_insert(session);
            }
        }
        uncaptured.ok_or_else(|| "received volume has no authorized invitation".into())
    }
    fn sync_owned(&mut self, store: &Arc<LocalStore>, selected: &[Id]) -> Result<()> {
        let connection = self.connection.as_ref().ok_or("AP disconnected")?.clone();
        let mut deferred = self.sync_registrations(store, &connection, Some(selected))?;
        self.sync_registered_owners(store, Some(selected), &mut deferred)?;
        deferred.first().map_or(Ok(()), |&id| {
            Err(sync_failure::volume(store, id, wire::SharedServiceUpgradeRequired))
        })
    }
    fn sync_registered_owners(
        &mut self,
        store: &Arc<LocalStore>,
        selected: Option<&[Id]>,
        deferred: &mut BTreeSet<Id>,
    ) -> Result<()> {
        // Owners initialize service replicas before invitation admission is possible.
        // One volume's bootstrap failure must not prevent the others from being tried.
        let mut first_error = None;
        let volumes: Vec<_> = remote_volumes(store)?
            .into_iter()
            .filter(|v| {
                v.owner == store.account().id
                    && selected.is_none_or(|ids| ids.contains(&v.id))
                    && !deferred.contains(&v.id)
                    && self.scope.read().admits(v.id)
            })
            .collect();
        let mut ready = Vec::with_capacity(volumes.len());
        let total = volumes.len();
        for (index, volume) in volumes.into_iter().enumerate() {
            let id = volume.id;
            self.progress.set(
                Some(TransferProgress::Volumes { current: index + 1, total }),
                &self.completion,
            );
            if volume.share_key_with_service
                && let Err(error) = self.sync_owned_volume(store, volume)
            {
                if error.is::<wire::SharedServiceUpgradeRequired>() {
                    // Local first use can materialize Shared after the registration pass.
                    deferred.insert(id);
                    continue;
                }
                initialization::retain_error(
                    &mut first_error,
                    sync_failure::volume(store, id, error),
                );
            }
            ready.push(id);
        }
        // Bootstrap hosted replicas before private account dependencies compete
        // for publisher slots. Signal only the owners in the checked snapshot.
        if let Some(live) = &self.live {
            for id in ready {
                self.runtime.block_on(live.publisher().account_volume_ready(id))?;
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    fn sync_owned_volume(
        &mut self,
        store: &Arc<LocalStore>,
        volume: crate::store::Volume,
    ) -> Result<()> {
        #[cfg(feature = "app-host")]
        let id = volume.id;
        let result = self.capture_owned_volume(store, volume);
        #[cfg(feature = "app-host")]
        {
            let mut missing = self.missing_owner_backings.lock();
            if result
                .as_ref()
                .is_err_and(|error| sync_failure::missing_entry_backing(id, error.as_ref()))
            {
                // Overflow falls back to a scoped bootstrap at launch.
                if missing.len() < crate::engine::publisher::VOLUME_LIMIT {
                    missing.insert(id);
                }
            } else {
                missing.remove(&id);
            }
        }
        result
    }

    fn capture_owned_volume(
        &mut self,
        store: &Arc<LocalStore>,
        volume: crate::store::Volume,
    ) -> Result<()> {
        let id = volume.id;
        store.require_replication_supported(id)?;
        self.sync_registration_dependencies(store, id, None)?;
        let result = (|| -> Result<()> {
            if volume.share_key_with_service {
                let pending = if !self.has_live_volume(store, id)? {
                    self.bootstrap_owner_volume(store, volume)?
                } else if self.initializations.tracked(id) {
                    self.pull_owner_volume(store, id)?
                } else {
                    None
                };
                if let Some(database) = pending {
                    return Err(sync_failure::database(
                        store,
                        &Access::owner(store.account().id),
                        id,
                        database,
                        "waiting for initial database synchronization",
                    ));
                }
            }
            Ok(())
        })();
        self.complete_volume_capture(store, id, result, false)?;
        #[cfg(feature = "app-host")]
        self.rearm_selection_watches(store, id)?;
        Ok(())
    }

    fn complete_volume_capture(
        &mut self,
        store: &LocalStore,
        id: Id,
        result: Result<()>,
        direct: bool,
    ) -> Result<()> {
        if let Err(error) = result {
            let receiving = self.live.as_ref().is_some_and(|live| live.has_receiver(id));
            let cause = sync_failure::reason(error.as_ref()).to_string();
            // Directory offers can arrive before the hosted grant and its initial data.
            // Each bounded bootstrap retry rechecks the actual ACL at the source.
            let waiting = direct
                && (matches!(
                    cause.as_str(),
                    super::peer::SUBSCRIPTION_BUSY
                        | "authorized peer is busy"
                        | "direct volume peer account capacity exhausted"
                        | "direct volume peer capacity exhausted"
                        | "direct volume permission required"
                        | "access denied: direct volume grant requires current permissions"
                ) || cause == "waiting for an authorized peer"
                    && store.volume(id)?.share_key_with_service);
            let reason =
                if waiting { "waiting for initial volume synchronization" } else { &cause };
            self.initializations.failed_capture(id, reason, Instant::now(), receiving)?;
            if self.initializations.scheduled(id) {
                return Err(Box::new(initialization::Pending(sync_failure::volume(
                    store, id, error,
                ))));
            }
            return Err(error);
        }
        self.initializations.forget(id);
        Ok(())
    }
    fn sync_registrations(
        &self,
        store: &LocalStore,
        connection: &ApConnection,
        selected: Option<&[Id]>,
    ) -> Result<BTreeSet<Id>> {
        // Signed build admission precedes registration of its session.
        // Replay retained operations without reallocating their identities.
        for session in store.sessions()? {
            store.publish_automatic_session(&session.id)?;
        }
        // An old AP may still replicate ordinary volumes and Root sessions.
        // Their common registration pass leaves incompatible Shared work pending;
        // the scoped owner/session pass reports the upgrade on that volume.
        let mut deferred = BTreeSet::new();
        self.publish_registrations(
            store,
            connection,
            store.pending_operations()?,
            selected,
            Some(&mut deferred),
        )?;
        Ok(deferred)
    }
    /// A local first use can promote a session after the full-sync registration
    /// cut. Its ordinary record and pending registration commit together. Finish
    /// those retained dependencies before using this volume remotely; never wait
    /// on AP from the local data lane or treat a skipped registration as admitted.
    fn sync_registration_dependencies(
        &self,
        store: &LocalStore,
        volume: Id,
        session: Option<&str>,
    ) -> Result<()> {
        let selected = &[volume];
        let mut pending = store.pending_operations()?;
        let mut build = None;
        for op in &pending {
            if let PendingKind::RegisterSession { session: registered, .. } = &op.operation
                && session == Some(registered.id.as_str())
            {
                if !Self::registration_selected(op, &pending, selected) {
                    return Err("session registration differs from selected volume".into());
                }
                build = Some(registered.resolved_build_id.clone());
            }
        }
        if session.is_none()
            && pending.iter().any(|op| {
                matches!(&op.operation,
            PendingKind::RegisterSession { session, .. } if session.shared_volume == Some(volume))
            })
        {
            self.runtime.block_on(wire::require_features(
                self.connection.as_ref().ok_or("AP disconnected")?,
                crate::network::ap::service::SHARED_REGISTRATION_FEATURES,
            ))?;
        }
        // Sessions sharing World are independent registrations. A failure in
        // another session must not hold this admission or owner-only file work.
        pending.retain(|op| match &op.operation {
            PendingKind::RegisterVolume { .. } => Self::registration_selected(op, &[], selected),
            PendingKind::RegisterSession { session: registered, .. } => {
                session == Some(registered.id.as_str())
            }
            PendingKind::UploadBuild { build_id, .. } => build.as_ref() == Some(build_id),
        });
        if pending.is_empty() {
            return Ok(());
        }
        if !self.scope.read().admits(volume) {
            return Err(
                crate::store::Error::AccessDenied("registration outside client scope").into()
            );
        }
        self.publish_registrations(
            store,
            self.connection.as_ref().ok_or("AP disconnected")?,
            pending,
            Some(selected),
            None,
        )
    }
    fn publish_registrations(
        &self,
        store: &LocalStore,
        connection: &ApConnection,
        mut pending: Vec<crate::store::PendingOperation>,
        selected: Option<&[Id]>,
        mut deferred: Option<&mut BTreeSet<Id>>,
    ) -> Result<()> {
        pending.sort_by_key(|op| match op.operation {
            PendingKind::RegisterVolume { .. } => 0,
            PendingKind::UploadBuild { .. } => 1,
            PendingKind::RegisterSession { .. } => 2,
        });
        for (index, op) in pending.iter().enumerate() {
            self.progress.set(
                Some(TransferProgress::Registrations { current: index + 1, total: pending.len() }),
                &self.completion,
            );
            if self
                .scope
                .read()
                .selected()
                .is_some_and(|volumes| !Self::registration_selected(op, &pending, volumes))
                || selected
                    .is_some_and(|volumes| !Self::registration_selected(op, &pending, volumes))
            {
                continue;
            }
            let known = matches!(&op.operation, PendingKind::UploadBuild { build_id, .. }
                if store.build_uploaded(build_id)?);
            if !known && let Err(error) = self.publish_registration(connection, op) {
                if error.is::<wire::SharedServiceUpgradeRequired>() {
                    if let Some(deferred) = deferred.as_deref_mut()
                        && let PendingKind::RegisterSession { session, .. } = &op.operation
                        && let Some(volume) = session.shared_volume
                    {
                        deferred.insert(volume);
                        continue;
                    }
                    return Err(error);
                }
                // A build upload has no unique volume, even when sessions share the build.
                return Err(match &op.operation {
                    PendingKind::RegisterVolume { volume } => {
                        sync_failure::volume(store, volume.id, error)
                    }
                    PendingKind::RegisterSession { session, .. } => {
                        sync_failure::volume(store, session.replica_volume(), error)
                    }
                    PendingKind::UploadBuild { .. } => error,
                });
            }
            store.acknowledge_pending(op)?;
        }
        Ok(())
    }
    fn registration_selected(
        op: &crate::store::PendingOperation,
        pending: &[crate::store::PendingOperation],
        volumes: &[Id],
    ) -> bool {
        let selected_session = |session: &Session| volumes.contains(&session.replica_volume());
        match &op.operation {
            PendingKind::RegisterVolume { volume } => {
                volumes.contains(&volume.id)
                    || pending.iter().any(|operation| {
                        matches!(&operation.operation,
                    PendingKind::RegisterSession { session, .. } if selected_session(session)
                        && session.replica_volume() == volume.id)
                    })
            }
            PendingKind::RegisterSession { session, .. } => selected_session(session),
            PendingKind::UploadBuild { build_id, .. } => pending.iter().any(|operation| {
                matches!(&operation.operation, PendingKind::RegisterSession { session, .. }
                    if selected_session(session) && &session.resolved_build_id == build_id)
            }),
        }
    }
    fn publish_registration(
        &self,
        connection: &ApConnection,
        op: &crate::store::PendingOperation,
    ) -> Result<()> {
        let operation_id = op.id.to_string();
        let request = match &op.operation {
            PendingKind::RegisterVolume { volume } => ServiceRequest::RegisterVolume {
                operation_id,
                volume_id: volume.id.to_string(),
                owner_account_id: volume.owner.to_string(),
            },
            PendingKind::RegisterSession { session, envelope } => ServiceRequest::RegisterSession {
                app: session.app.clone(),
                operation_id,
                session_id: session.id.clone(),

                root_volume_id: session.root_volume.to_string(),
                shared_volume_id: session.shared_volume.map(|id| id.to_string()),
                selector: match &session.selector {
                    crate::store::BuildSelector::Exact { build_id } => {
                        BuildSelector::Exact { build_id: build_id.clone() }
                    }
                    crate::store::BuildSelector::App { app } => {
                        BuildSelector::App { app: app.clone() }
                    }
                },
                resolved_build_id: session.resolved_build_id.clone(),
                creator_account_id: session.creator.to_string(),
                envelope: crate::store::envelope_text(envelope),
            },
            PendingKind::UploadBuild { build_id, path } => {
                return self.upload_pending_build(connection, operation_id, build_id, path);
            }
        };
        self.runtime.block_on(wire::request(connection, &request))?;
        Ok(())
    }
    fn upload_pending_build(
        &self,
        connection: &ApConnection,
        operation_id: String,
        build_id: &str,
        path: &Path,
    ) -> Result<()> {
        if path != Path::new("builds").join(build_id) {
            return Err("invalid retained build reference".into());
        }
        let profile = self.identity_path.parent().ok_or("identity has no profile")?;
        use std::{io::Seek, os::unix::fs::OpenOptionsExt};
        use tokio::io::AsyncReadExt;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(profile.join(path))?;
        if !file.metadata()?.is_file() {
            return Err("cached build must be a regular file".into());
        }
        let verified = crate::package::codec::verify(&mut file)?;
        if crate::identifier::encode(&verified.digest) != build_id {
            return Err("cached build identity mismatch".into());
        }
        file.rewind()?;
        let request = ServiceRequest::UploadBuild {
            operation_id,
            build_id: build_id.into(),
            content_length: verified.index.length,
        };
        self.runtime.block_on(async {
            let stream = wire::begin(connection, &request).await?;
            let (mut send, mut receive) = stream.split();
            let mut file = tokio::fs::File::from_std(file);
            let mut chunk = vec![0; 65536];
            let mut remaining = verified.index.length;
            while remaining != 0 {
                let n = remaining.min(chunk.len() as u64) as usize;
                file.read_exact(&mut chunk[..n]).await?;
                send.write_bytes(&chunk[..n]).await?;
                remaining -= n as u64;
            }
            if file.read(&mut [0]).await? != 0 {
                return Err("cached build changed while uploading".into());
            }
            send.finish()?;
            let frame = receive.answer().await?;
            let response: crate::network::ap::operation::ServiceResponse =
                serde_json::from_slice(&frame.body)?;
            if !response.ok || response.operation_id != request.operation_id() {
                return Err(response
                    .error
                    .unwrap_or_else(|| "app upload acknowledgment mismatch".into())
                    .into());
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
    }
    fn bootstrap_owner_volume(
        &mut self,
        store: &Arc<LocalStore>,
        volume: crate::store::Volume,
    ) -> Result<Option<crate::store::DatabaseId>> {
        store.require_replication_supported(volume.id)?;
        if store.authority_refresh_required(volume.id)? {
            self.pull_owner_volume(store, volume.id)?;
        }
        let connection = self.connection.as_ref().ok_or("AP disconnected")?.clone();
        let mut prepared = self.runtime.block_on(
            self.live.as_mut().ok_or("live replication is unavailable")?.begin(
                connection.clone(),
                crate::engine::replication::ReplicaScope::Volume {
                    volume: volume.id,
                    session: None,
                },
            ),
        )?;
        // Recovered owners have credentials but no local policy yet. Pull
        // the existing service replica before considering provisioning or writes.
        let recovered = store.replica_root(volume.id, crate::store::DatabaseId::Index)?.is_none();
        if recovered {
            self.pull_owner_volume(store, volume.id)?;
        }
        let provision = store.service_provisioning(volume.id)?;
        let request = ServiceRequest::ProvisionReplica {
            operation_id: stable_operation("provision", &volume.id.to_string()),
            volume_id: volume.id.to_string(),
            owner_account_id: volume.owner.to_string(),
            personal: volume.personal,
            database_key: provision.database_key,
        };
        self.runtime.block_on(wire::request(&connection, &request))?;
        if store.durable_revision(volume.id, crate::store::DatabaseId::Index)?.is_some() {
            let database = crate::store::DatabaseId::Revocations;
            if store.durable_revision(volume.id, database)?.is_none() {
                // An interrupted first upload can leave a hosted index without
                // revocations. Resume this owner's authorized push before the
                // full pull; never adopt an empty control stage as authority.
                let access = Access::owner(store.account().id);
                if !store.replica_upload_allowed(&access, volume.id, database)? {
                    return Err("owner revocations upload is not authorized".into());
                }
                let receipt = store.with_outbound_replica(
                    volume.id,
                    database,
                    ReplicaReader::Service,
                    |source| {
                        self.transfer_database(
                            store,
                            None,
                            volume.id,
                            database,
                            SyncDirection::Push,
                            source,
                        )
                    },
                )?;
                store.acknowledge_replica(volume.id, database, &receipt.durable)?;
            }
            // Recovery already pulled and qualified this volume before the
            // idempotent provision. The post-subscription pull below still
            // bridges changes made while establishing the live stream.
            if !recovered {
                self.pull_owner_volume(store, volume.id)?;
            }
        }
        self.runtime.block_on(
            self.live
                .as_mut()
                .expect("account binding initialized live replication")
                .rebase(&mut prepared),
        )?;
        self.push_volume(store, None, volume.id)?;
        // The service cannot serve this volume's own reader. Abandon the
        // preparation explicitly; dropping it would report a lost owner.
        if !self.service_covers_reader(store, volume.id, None)? {
            self.runtime.block_on(
                self.live
                    .as_mut()
                    .expect("account binding initialized live replication")
                    .abandon(prepared),
            )?;
            return Ok(None);
        }
        self.runtime.block_on(
            self.live
                .as_mut()
                .expect("account binding initialized live replication")
                .subscribe(&mut prepared),
        )?;
        let pending = self.pull_owner_volume(store, volume.id)?;
        self.finish_captured_live(store, None, volume.id, prepared)?;
        Ok(pending)
    }
    fn sync_blob_selected<'a>(
        &self,
        store: &LocalStore,
        volume: Id,
        synchronized: impl IntoIterator<Item = &'a Session>,
    ) -> Result<()> {
        let descriptor = store.volume(volume)?;
        self.sync_registration_dependencies(store, volume, None)?;
        if !descriptor.share_key_with_service
            && store.replica_root(volume, crate::store::DatabaseId::Index)?.is_none()
        {
            return Err("waiting for an authorized peer to provide private-volume data".into());
        }
        let session =
            if descriptor.owner == store.account().id || store.has_direct_volume_grant(volume)? {
                None
            } else {
                let Some(session) = self.sync_blob_session(store, volume, synchronized)? else {
                    return Ok(());
                };
                Some(session)
            };
        self.sync_blob_volume(store, volume, session)
    }

    fn sync_blob_session<'a>(
        &self,
        store: &LocalStore,
        volume: Id,
        synchronized: impl IntoIterator<Item = &'a Session>,
    ) -> Result<Option<&'a str>> {
        let mut denied = None;
        for session in synchronized {
            if session.replica_volume() != volume {
                continue;
            }
            match store.authorize_replication(
                &Access::session(store.account().id, &session.id),
                volume,
                Permission::Read,
            ) {
                Ok(()) => return Ok(Some(session.id.as_str())),
                Err(error) => {
                    denied.get_or_insert(error);
                }
            }
        }
        // No successfully admitted session may replay this volume.
        denied.map_or(Ok(None), |error| Err(error.into()))
    }

    fn sync_blob_volume(
        &self,
        store: &LocalStore,
        volume: Id,
        session: Option<&str>,
    ) -> Result<()> {
        let descriptor = store.volume(volume)?;
        let private_personal = descriptor.personal && !descriptor.share_key_with_service;
        if private_personal && descriptor.owner == store.account().id {
            return Ok(()); // Owner file bodies are served from their pinned local versions.
        }
        let access = session.map_or_else(
            || Access::owner(store.account().id),
            |id| Access::session(store.account().id, id),
        );
        let mut after = None;
        loop {
            let page = store.pending_entry_blobs(&access, volume, after)?;
            if !page.versions.is_empty() {
                if private_personal && session.is_none() {
                    return Err(
                        "private file version is available only on the device that created it"
                            .into(),
                    );
                }
                let live = self.live.as_ref().ok_or("live replication is unavailable")?;
                self.runtime
                    .block_on(live.publisher().fence(&[volume]))
                    .map_err(|e| live.resolve_failure(e))?;
                self.blob_transport().upload_pending(
                    store,
                    &access,
                    session,
                    volume,
                    page.versions,
                )?;
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        Ok(())
    }

    fn checkpoint_pending_changes(&mut self, store: &Arc<LocalStore>) -> Result<()> {
        let result = self.runtime.block_on(
            self.live
                .as_ref()
                .ok_or("live replication is unavailable")?
                .checkpoint_pending_changes(),
        );
        match result {
            Err(error)
                if sync_failure::reason(error.as_ref()).is::<crate::engine::live::LiveGap>() =>
            {
                // Only a confirmed delivery gap starts a fresh authority capture.
                // Already acknowledged publications and uncertain effects never replay.
                self.recover_gaps(store)?;
                self.runtime.block_on(
                    self.live
                        .as_ref()
                        .ok_or("live replication is unavailable")?
                        .checkpoint_pending_changes(),
                )
            }
            result => result,
        }
    }
    /// The serial provider lane owns admission changes. Retiring a stream never
    /// discards its durable outbox; a later admission rebases from that source.
    #[cfg(feature = "file-provider")]
    pub(crate) fn select_provider_volumes(
        &mut self,
        store: &Arc<LocalStore>,
        volumes: &[Id],
    ) -> Result<()> {
        if volumes.len() > PROVIDER_VOLUME_ADMISSION
            || (volumes.len() == 2 && volumes[0] == volumes[1])
        {
            return Err("File Provider admits at most two distinct volumes".into());
        }
        let scope = self.scope.read().clone();
        let previous = scope.selected().ok_or("File Provider requires a provider replica")?;
        let catalog = store.provider_volumes()?;
        if volumes.iter().any(|id| !catalog.iter().any(|volume| volume.id == *id)) {
            return Err("File Provider volume is not an owned personal volume".into());
        }
        let unchanged = previous == volumes;
        // Exclude retiring volumes before joining applies or sending Forget.
        // Late durable writes remain in their outboxes, but cannot re-admit an
        // already removed publisher slot through the committed-change feed.
        *self.scope.write() = ClientScope::Provider(volumes.to_vec());
        self.initializations.retain(volumes);
        if let Some(live) = &mut self.live {
            self.runtime.block_on(live.retain_provider_volumes(volumes))?;
        }
        // Cleanup uses retained controller/publisher keys, even when an earlier
        // canceled attempt already changed the selection before it finished.
        let mut initialized = unchanged;
        for volume in volumes {
            initialized &= self.has_live_volume(store, *volume)?;
        }
        if initialized {
            return Ok(());
        }
        if self.connection.as_ref().is_none_or(|connection| connection.is_closed()) {
            return Ok(());
        }
        self.sync_owned(store, volumes)?;
        self.live_fence(store, Some(volumes), None, false)
    }
    #[cfg(feature = "file-provider")]
    pub(crate) fn flush_provider(
        &mut self,
        store: &Arc<LocalStore>,
        mutations: &super::connected::ProviderMutations,
    ) -> Result<()> {
        if !self.scope.read().provider() {
            return Err("publication requires a provider domain".into());
        }
        store.provider_account()?;
        if self.connection.as_ref().is_none_or(|connection| connection.is_closed()) {
            return Err("AP disconnected; local provider changes remain pending".into());
        }
        let pending = store.provider_pending_publications()?;
        let mut first_error = None;
        for (volume, generation) in pending {
            let mutation_epoch = mutations.idle_epoch();
            let result = (|| -> Result<()> {
                self.select_provider_volumes(store, &[volume])?;
                self.checkpoint_pending_changes(store)?;
                self.sync_blob_volume(store, volume, None)?;
                let live = self.live.as_ref().ok_or("live replication is unavailable")?;
                self.runtime.block_on(live.account_fence())?;
                // A mutation may enqueue before committing, then finish after
                // our source capture. Both ends must be idle with no intervening
                // admission/completion before this marker can be acknowledged.
                if mutations.unchanged(mutation_epoch) {
                    store.provider_ack_publication(volume, generation)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    /// Selection and replica events use separate AP streams. A candidate can
    /// be resolved before its R1 entry arrives, especially after disconnect.
    /// The stream keeps its explicit authority; candidate execution is admitted
    /// separately against the received root policy.
    #[cfg(feature = "app-host")]
    pub(crate) fn prepare_update_replica(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
    ) -> Result<()> {
        let _timing = crate::trace::Interval::new("replication.update_boundary");
        self.connect(store)?;
        self.ensure_live(store)?;
        let account = self.begin_account_sync(store)?;
        self.finish_account_sync(store, account)?;
        self.sync_required_session(store, session)?;
        self.live_fence(store, Some(&[session.replica_volume()]), None, false)
    }

    fn sync_required_session(&mut self, store: &Arc<LocalStore>, session: &Session) -> Result<()> {
        store
            .provision_local_session_world(session)
            .map_err(|error| sync_failure::volume(store, session.root_volume, error))?;
        if (store.is_local_session(&session.id)? && session.shared_volume.is_none())
            || (session.shared_volume.is_some()
                && !store.shared_session_initialized(&session.id)?)
        {
            return Ok(());
        }
        let root = store.volume(session.replica_volume())?;
        let private_root = !root.share_key_with_service;
        if let Err(error) = self.admit(store, session) {
            if !waiting_peer(error.as_ref()) {
                return Err(error);
            }
            // First use can materialize Shared after the owned-volume pass.
            // Seed it with owner authority before retrying session admission.
            if session.shared_volume.is_some() {
                if root.owner != store.account().id || private_root {
                    return Err(error);
                }
                self.sync_owned_volume(store, root)?;
                self.admit(store, session)?;
            }
        }
        self.sync_session_volume(store, session, session.replica_volume(), private_root)
    }

    fn sync_optional_world(&mut self, store: &Arc<LocalStore>, session: &Session) -> Result<()> {
        if session.is_distinct_world(session.root_volume)
            && session.app.publisher != store.account().id
            && store.shared_session_initialized(&session.id)?
        {
            self.sync_invited_world(store, session)?;
        }
        Ok(())
    }

    fn sync_session_volume(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        volume: Id,
        private_root: bool,
    ) -> Result<()> {
        let result = self.capture_session_volume(store, session, volume, private_root);
        let world = session.is_distinct_world(volume);
        if let Err(error) = result {
            let cause = sync_failure::reason(error.as_ref()).to_string();
            // Account discovery and session bootstrap share AP request credits.
            // A pre-dispatch rejection is safe to retry for either kind of volume.
            // Only a distinct World owns retries for other source-readiness waits.
            if world || cause == initialization::AP_CAPACITY {
                let error = sync_failure::volume(store, volume, error);
                self.initializations.failed(volume, &cause, Instant::now())?;
                return Err(if self.initializations.scheduled(volume) {
                    Box::new(initialization::Pending(error))
                } else {
                    error
                });
            }
            return Err(error);
        }
        if world {
            self.initializations.forget(volume);
        }
        Ok(())
    }

    fn capture_session_volume(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        volume: Id,
        private_root: bool,
    ) -> Result<()> {
        let pending_error = |database| {
            let access = Access::session_version(
                store.account().id,
                &session.id,
                &session.resolved_build_id,
            );
            sync_failure::database(
                store,
                &access,
                volume,
                database,
                "waiting for initial database synchronization",
            )
        };
        if store.authority_refresh_required(volume)? {
            self.pull_bound_volume(store, session, volume)?;
        }
        let mut prepared = self.prepare_session_volume(store, session, volume)?;
        if prepared.is_none() && !private_root {
            if session.is_distinct_world(volume)
                && self.initializations.tracked(volume)
                && let Some(database) =
                    self.pull_bound_volume(store, session, volume)?.pending_backing
            {
                return Err(pending_error(database));
            }
            self.runtime.block_on(
                self.live
                    .as_ref()
                    .expect("live replication initialized")
                    .publisher()
                    .account_volume_ready(volume),
            )?;
            return Ok(());
        }
        // Establish current authority before any queued participant write leaves this device.
        let received = self.pull_bound_volume(store, session, volume)?;
        let mut pending = received.pending_backing;
        self.seed_session_volume(store, session, received, private_root, &mut prepared)?;
        if let Some(mut preparation) = prepared {
            self.runtime.block_on(
                self.live
                    .as_mut()
                    .expect("live replication initialized")
                    .subscribe(&mut preparation),
            )?;
            // Close the gap between the root seed snapshot and its live subscription.
            let received = self.pull_bound_volume(store, session, volume)?;
            pending = received.pending_backing;
            self.finish_captured_live(store, Some((session, received)), volume, preparation)?;
        }
        self.runtime.block_on(
            self.live
                .as_ref()
                .expect("live replication initialized")
                .publisher()
                .account_volume_ready(volume),
        )?;
        if let Some(database) = pending {
            return Err(pending_error(database));
        }
        Ok(())
    }
    fn prepare_session_volume(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        volume: Id,
    ) -> Result<Option<crate::engine::live::PreparedLive>> {
        let descriptor = store.volume(volume)?;
        if !descriptor.share_key_with_service || self.has_live_volume(store, volume)? {
            return Ok(None);
        }
        self.runtime
            .block_on(self.live.as_mut().ok_or("live replication is unavailable")?.begin(
                self.connection.as_ref().ok_or("AP disconnected")?.clone(),
                crate::engine::replication::ReplicaScope::Volume {
                    volume,
                    session: (descriptor.owner != store.account().id).then(|| session.clone()),
                },
            ))
            .map(Some)
    }
    fn seed_session_volume(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        received: ReceivedVolume,
        private_root: bool,
        prepared: &mut Option<crate::engine::live::PreparedLive>,
    ) -> Result<()> {
        let volume = received.volume();
        let access = Access::session(store.account().id, &session.id);
        store.authorize_replication(&access, volume, Permission::Read)?;
        if let Some(preparation) = prepared.as_mut() {
            self.runtime.block_on(
                self.live.as_mut().expect("live replication initialized").rebase(preparation),
            )?;
        }
        if (prepared.is_some() || private_root)
            && self.push_bootstrap_participant(store, session, received)?
            && let Some(preparation) = prepared.as_mut()
        {
            self.runtime.block_on(
                self.live.as_mut().expect("live replication initialized").rebase(preparation),
            )?;
        }
        if prepared.is_some() && !self.service_covers_reader(store, volume, Some(session))? {
            let preparation = prepared.take().expect("root preparation was present");
            self.runtime.block_on(
                self.live.as_mut().expect("live replication initialized").abandon(preparation),
            )?;
        }
        Ok(())
    }
    fn start_listeners(&self, store: &Arc<LocalStore>) -> Result<()> {
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        let mut first_error = None;
        for session in store.sessions()? {
            if session.shared_volume.is_some() || store.is_local_session(&session.id)? {
                continue;
            }
            if self
                .listeners
                .lock()
                .expect("mutex is not poisoned")
                .get(&session.id)
                .is_some_and(|task| !task.is_finished())
            {
                continue;
            }
            let access = Access::session(store.account().id, &session.id);
            if store
                .authorize_replication(
                    &access,
                    session.replica_volume(),
                    crate::store::object_contract::Permission::Read,
                )
                .is_err()
            {
                continue;
            }
            let id = session.id.clone();
            let volume = session.replica_volume();
            let result = self
                .sync_registration_dependencies(store, volume, Some(&session.id))
                .and_then(|()| {
                    self.runtime.block_on(crate::engine::peer::listen(
                        connection.clone(),
                        store.clone(),
                        session,
                    ))
                });
            match result {
                Ok(task) => {
                    self.listeners.lock().expect("mutex is not poisoned").insert(id, task);
                }
                Err(error)
                    if SessionBuildUnavailable::reported_by(sync_failure::reason(
                        error.as_ref(),
                    )) => {}
                Err(error) => {
                    first_error.get_or_insert_with(|| sync_failure::volume(store, volume, error));
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    pub(crate) fn admit(&self, store: &LocalStore, session: &Session) -> Result<()> {
        if store.is_local_session(&session.id)? && session.shared_volume.is_none() {
            return Err("sharing app sessions requires encrypted synchronization, \
                which is not supported yet"
                .into());
        }
        store.require_replication_supported(session.replica_volume())?;
        self.sync_registration_dependencies(store, session.replica_volume(), Some(&session.id))?;
        let retained = store.session(&session.id)?;
        let generation = store.session_denial_generation(&retained)?;
        let result = self.app_request(&ServiceRequest::OpenSession {
            operation_id: Id::generate()?.to_string(),
            session_id: session.id.clone(),
            build_id: session.resolved_build_id.clone(),
            proof: store.session_proof(&session.id, store.account().id)?,
        });
        if let Err(error) = &result
            && error
                .downcast_ref::<ServiceFailure>()
                .and_then(ServiceFailure::denial)
                .map(|denial| store.receive_ap_session_denial(denial, session))
                .transpose()?
                == Some(true)
        {
            return Err(crate::store::Error::AccessDenied("session participant revoked").into());
        }
        let response = result.map_err(SessionBuildUnavailable::from_service)?;
        let data = response.data.ok_or("missing session admission response")?;
        let registered: Session = serde_json::from_value(data["session"].clone())?;
        // A retained runtime can name an older admitted version than the
        // session's current exact selector. AP authorizes the requested build;
        // the immutable selector origin and publishing identity must still match.
        if !session.same_identity(&registered)
            || session.resolved_build_id != registered.resolved_build_id
        {
            return Err("Session admission differs from authenticated session".into());
        }
        // Candidate admission precedes adoption/consent. Its successful reply
        // cannot clear negative evidence for the retained build.
        if retained == *session && !store.clear_session_denial(&retained, generation)? {
            return Err(crate::store::Error::AccessDenied("session admission superseded").into());
        }
        Ok(())
    }
    /// Reuse the exact cached build, or download and validate it. Neither path
    /// admits or adopts a session; immutable build bytes are public by hash.
    pub fn ensure_exact_build(
        &mut self,
        store: &LocalStore,
        build: &str,
        app_path: &Path,
    ) -> Result<()> {
        if let Ok(package) = crate::package::Package::open(app_path)
            && package.id()? == build
        {
            self.authenticate_public_package(store, package)?;
            return Ok(());
        }
        self.connect(store)?;
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        let downloaded = self.runtime.block_on(super::build_download::download(
            connection,
            build,
            app_path,
            self.canceled.as_deref(),
        ))?;
        self.authenticate_public_package(store, crate::package::Package::open(app_path)?)?;
        if let Some(admission) = downloaded.admission {
            store.cache_package_admission(
                &admission,
                crate::identifier::decode::<32>(build)?,
                crate::store::now_unix_secs(),
            )?;
        }
        store.remember_uploaded_build(build)?;
        store.remember_app_name(build, &downloaded.name)?;
        Ok(())
    }

    pub fn ensure_build(
        &mut self,
        store: &LocalStore,
        session: &Session,
        app_path: &Path,
    ) -> Result<()> {
        let cached = (|| -> Result<Option<crate::package::Package>> {
            let package = crate::package::Package::open(app_path)?;
            Ok((package.id()? == session.resolved_build_id).then_some(package))
        })();
        if let Ok(Some(package)) = cached {
            let package = self.authenticate_public_package(store, package)?;
            store.remember_app_name(&session.resolved_build_id, package.name())?;
            #[cfg(feature = "app-host")]
            self.builds.installed(&session.resolved_build_id);
            self.completion.notify();
            return Ok(());
        }
        self.connect(store)?;
        let connection = self
            .connection
            .as_ref()
            .expect("the preceding connection check established the AP connection");
        let downloaded = self.runtime.block_on(super::build_download::download(
            connection,
            &session.resolved_build_id,
            app_path,
            self.canceled.as_deref(),
        ))?;
        self.authenticate_public_package(store, crate::package::Package::open(app_path)?)?;
        if let Some(admission) = downloaded.admission {
            store.cache_package_admission(
                &admission,
                crate::identifier::decode::<32>(&session.resolved_build_id)?,
                crate::store::now_unix_secs(),
            )?;
        }
        store.remember_uploaded_build(&session.resolved_build_id)?;
        store.remember_app_name(&session.resolved_build_id, &downloaded.name)?;
        #[cfg(feature = "app-host")]
        self.builds.installed(&session.resolved_build_id);
        self.completion.notify();
        Ok(())
    }
    /// Resolve a hosted selector before a cold device runs its guest. A lazy
    /// creator session retains its local selection. Both require local validation.
    #[cfg(feature = "app-host")]
    pub(crate) fn bootstrap_current(
        &mut self,
        store: &Arc<LocalStore>,
        id: &str,
        apps: &Path,
    ) -> Result<Option<serde_json::Value>> {
        self.check_cancellation()?;
        let mut candidate = store.session(id)?;
        if candidate.creator == store.account().id {
            // Cold startup may be the first network use of a locally allocated
            // session. Resolution cannot race its durable registration queue.
            self.connect(store)?;
            self.ensure_live(store)?;
            let account = self.begin_account_sync(store)?;
            self.finish_account_sync(store, account)?;
            self.recover_shared_world(store, &candidate)?;
            if candidate.shared_volume.is_some() && !store.shared_session_initialized(id)? {
                // Only a locally allocated lazy Shared lacks a stored invitation.
                // Its first access registers it. Until then validate the local build.
                return Ok(None);
            }
            self.sync_owned(store, &[candidate.replica_volume()])?;
            // A local session waiting for recovered World could not publish
            // before the owner bootstrap. Register that same session now.
            store.publish_automatic_session(id)?;
            self.live_fence(store, Some(&[candidate.replica_volume()]), None, false)?;
        }
        self.check_cancellation()?;
        let resolution = self.resolve_session(store, id)?;
        candidate.selector = serde_json::from_value(resolution["selector"].clone())?;
        candidate.resolved_build_id =
            resolution["resolved_build_id"].as_str().ok_or("missing resolved version")?.into();
        self.bootstrap(store, &candidate, &apps.join(&candidate.resolved_build_id))?;
        Ok(Some(resolution))
    }
    #[cfg(feature = "app-host")]
    fn recover_shared_world(&mut self, store: &Arc<LocalStore>, session: &Session) -> Result<()> {
        if session.shared_volume.is_some() && world_needs_recovery(store, session)? {
            if let Err(error) = self.sync_owned(store, &[session.root_volume]) {
                // A missing World backing stays in normal recovery. The existing
                // room still has to pass its own Shared admission and build checks.
                if sync_failure::missing_entry_backing(session.root_volume, error.as_ref())
                    && store.shared_session_initialized(&session.id)?
                {
                    return Ok(());
                }
                return Err(error);
            }
            if !store.has_volume_index(session.root_volume)? {
                return Err("World data is unavailable on this device".into());
            }
        }
        Ok(())
    }
    #[cfg(feature = "app-host")]
    fn bootstrap(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        app_path: &Path,
    ) -> Result<()> {
        self.connect(store)?;
        self.ensure_live(store)?;
        let account = self.begin_account_sync(store)?;
        self.finish_account_sync(store, account)?;
        let result = (|| {
            self.check_cancellation()?;
            if let Err(error) = self.admit(store, session)
                && !waiting_peer(error.as_ref())
            {
                return Err(error);
            }
            if session.creator != store.account().id {
                self.recover_shared_world(store, session)?;
            }
            if store.session(&session.id)?.resolved_build_id == session.resolved_build_id {
                self.sync_required_session(store, session)?;
                self.live_fence(store, Some(&[session.replica_volume()]), None, false)?;
            } else {
                // Candidate data may be prepared, but candidate live authority
                // starts only after actual adoption and personal-volume consent.
                self.pull_bound_volume(store, session, session.replica_volume())?;
            }
            self.check_cancellation()?;
            // A private root cannot receive hosted OpenSession admission. Its
            // existing peer bootstrap proves possession and current access on
            // this connection before the package download requires admission.
            self.ensure_build(store, session, app_path)?;
            if store.session(&session.id)?.resolved_build_id == session.resolved_build_id {
                // The room is admitted and its build is ready. World failures remain
                // observable via synchronization and never invalidate this room.
                if let Err(error) = self.sync_optional_world(store, session)
                    && (canceled(error.as_ref()) || store.session_denial(&session.id)?.is_some())
                {
                    return Err(error);
                }
            }
            self.check_cancellation()
        })();
        if let Err(error) = &result
            && !canceled(error.as_ref())
        {
            self.disconnect();
        }
        if result.is_ok()
            && store.session(&session.id)?.resolved_build_id == session.resolved_build_id
        {
            self.start_listeners(store)?;
            self.refresh_availability(store)?;
        }
        result
    }
    fn pull_into(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        database: crate::store::DatabaseId,
        target: &crate::store::Database,
    ) -> Result<PullReceipt> {
        // Private root data and its current revocations come from an authorized
        // peer; a hosted request cannot establish private-root admission.
        let private_root = session
            .map(|session| store.volume(session.replica_volume()))
            .transpose()?
            .is_some_and(|root| !root.share_key_with_service);
        let service = store.volume(volume)?.share_key_with_service && !private_root;
        if service {
            match self.transfer_database(
                store,
                session,
                volume,
                database,
                SyncDirection::Pull,
                target,
            ) {
                Ok(receipt) => {
                    return Ok(PullReceipt {
                        ap_source: true,
                        revision: receipt.durable,
                        source: receipt.source,
                        hosted: true,
                    });
                }
                Err(error)
                    if error.to_string().contains("waiting for an authorized peer")
                        && session.is_some_and(|s| s.shared_volume.is_none()) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let result = crate::engine::peer::pull_into(
            &self.runtime,
            self.connection.as_ref().ok_or("AP disconnected")?,
            store,
            session,
            volume,
            database,
            target,
        );
        if let Some(session) = session
            && result
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<super::peer::VerifiedDenial>().is_some())
        {
            store.record_session_denial(session, "session participant revoked")?;
        }
        // The peer's encrypted receipt confirms that transfer, not AP storage.
        // Only actual hosted receipts can advance hosted durability records.
        result.map(|receipt| PullReceipt {
            ap_source: false,
            revision: receipt.durable,
            source: receipt.source,
            hosted: false,
        })
    }

    fn receive_volume(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
    ) -> Result<crate::store::ReceivedVolume> {
        Self::receive_volume_using(
            store,
            session,
            volume,
            None,
            |database, target, request| {
                if let ReceivedPull::Backing {
                    cut,
                    protocol: crate::store::volume_index::DatabaseProtocol::BlobLog,
                } = request
                {
                    let crate::store::DatabaseId::Entry(log) = database else {
                        return Err("append log requires named database identity".into());
                    };
                    self.pull_received_log(store, session, volume, log, target, cut)
                } else {
                    self.pull_into(store, session, volume, database, target).map(Some)
                }
            },
            |index, repair| self.repair_received_controls(store, session, volume, index, repair),
        )
    }

    fn repair_received_controls(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        index: &crate::store::Database,
        repair: ControlRepair<'_>,
    ) -> Result<bool> {
        match repair {
            ControlRepair::ServiceRead
                if session.is_some() || store.volume(volume)?.owner != store.account().id =>
            {
                Ok(false)
            }
            ControlRepair::ServiceRead => store
                .restore_owner_service_read(volume, index, |bytes| {
                    let request = ServiceRequest::PublishReplica {
                        operation_id: Id::generate()?.to_string(),
                        session_id: None,
                        build_id: None,
                        volume_id: volume.to_string(),
                        database_id: crate::store::DatabaseId::Index,
                    };
                    let connection = self
                        .connection
                        .as_ref()
                        .ok_or(crate::store::Error::System("AP disconnected".into()))?;
                    self.runtime
                        .block_on(wire::publish_replica(connection, &request, bytes))
                        .map_err(|error| crate::store::Error::System(error.to_string()))?;
                    Ok(())
                })
                .map_err(Into::into),
            ControlRepair::CheckRetirement(entry) => {
                Ok(store.retirement_for_capture(session, volume, index, entry)?.is_some())
            }
            ControlRepair::MissingBackings { entries, first_unrepaired } => {
                let mut complete = true;
                for &entry in entries {
                    let Some(retirement) =
                        store.retirement_for_capture(session, volume, index, entry)?
                    else {
                        if complete {
                            *first_unrepaired = entry;
                        }
                        complete = false;
                        continue;
                    };
                    let source = store.project_retirement(index, &retirement)?;
                    // A scoped receipt covers only the replayed retirement.
                    // Re-capture the controls before acknowledging an Index.
                    self.transfer_scoped_database(
                        store,
                        session,
                        volume,
                        (crate::store::DatabaseId::Index, Some(entry)),
                        SyncDirection::Push,
                        source.database(),
                    )?;
                }
                Ok(complete)
            }
        }
    }

    #[cfg(test)]
    fn receive_volume_with(
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        mut pull: impl FnMut(crate::store::DatabaseId, &crate::store::Database) -> Result<PullReceipt>,
    ) -> Result<crate::store::ReceivedVolume> {
        Self::receive_volume_using(
            store,
            session,
            volume,
            None,
            |database, target, _| pull(database, target).map(Some),
            |_, _| Ok(false),
        )
    }

    fn receive_volume_using(
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        entry: Option<crate::store::EntryId>,
        mut pull: impl FnMut(
            crate::store::DatabaseId,
            &crate::store::Database,
            ReceivedPull,
        ) -> Result<Option<PullReceipt>>,
        mut restore_controls: impl FnMut(&crate::store::Database, ControlRepair<'_>) -> Result<bool>,
    ) -> Result<crate::store::ReceivedVolume> {
        use crate::store::DatabaseId;
        let _timing = crate::trace::Span::new("replication.receive_volume");
        let access = session.map_or_else(
            || Access::owner(store.account().id),
            |session| {
                Access::session_version(store.account().id, &session.id, &session.resolved_build_id)
            },
        );
        for _ in 0..3 {
            let _attempt = crate::trace::Span::new("replication.receive_volume_attempt");
            let mut stage = match entry {
                Some(entry) => {
                    store.prepare_received_entry(crate::store::volume_index::EntryAddress {
                        volume,
                        entry,
                    })?
                }
                None => store.prepare_received_volume(volume)?,
            };
            let scope_error =
                |database, error| sync_failure::database(store, &access, volume, database, error);
            let Some((cut, controls)) = Self::pull_controls(store, &access, &stage, &mut pull)?
            else {
                crate::trace::mark("replication.bootstrap_source_changed");
                continue;
            };
            let mut ap_source = controls.index.ap_source && controls.revocations.ap_source;
            let world = entry.is_none() && session.is_some_and(|s| s.is_distinct_world(volume));
            let mut repair = |stage: &crate::store::ReceivedVolume, request| {
                Self::restore_captured_controls(stage, &controls, |index| {
                    restore_controls(index, request)
                })
            };
            if !world
                && repair(&stage, ControlRepair::ServiceRead)
                    .map_err(|error| scope_error(DatabaseId::Index, error))?
            {
                continue;
            }
            let missing = Self::pull_received_backings(
                store,
                &access,
                &mut stage,
                (cut, world),
                &mut ap_source,
                &mut pull,
                |stage, entry| repair(stage, ControlRepair::CheckRetirement(entry)),
            )?;
            let Some(missing) = missing else {
                crate::trace::mark("replication.bootstrap_source_changed");
                continue;
            };
            if let Some(&first) = missing.first() {
                // When a partial repair succeeds for earlier entries, report the one
                // still missing. An ineligible capture retains its first missing entry.
                let mut waiting = first;
                if repair(
                    &stage,
                    ControlRepair::MissingBackings {
                        entries: &missing,
                        first_unrepaired: &mut waiting,
                    },
                )
                .map_err(|error| scope_error(DatabaseId::Index, error))?
                {
                    continue;
                }
                return Err(scope_error(
                    DatabaseId::Entry(waiting),
                    "waiting for initial database synchronization".into(),
                ));
            }
            if !ap_source
                || Self::qualify_received_authority(&mut stage, cut, &mut pull)
                    .map_err(|error| scope_error(DatabaseId::Index, error))?
            {
                crate::trace::mark("replication.bootstrap_snapshot_accepted");
                return Ok(stage);
            }
            crate::trace::mark("replication.bootstrap_source_changed");
        }
        Err(initialization::CHANGING_CUT.into())
    }

    fn pull_controls(
        store: &LocalStore,
        access: &Access,
        stage: &crate::store::ReceivedVolume,
        pull: &mut BootstrapPull<'_>,
    ) -> Result<Option<(crate::store::SnapshotCut, CapturedControls)>> {
        use crate::store::{DatabaseId, SnapshotCut};
        let scope_error = |database, error: sync_failure::Cause| {
            sync_failure::database(store, access, stage.volume(), database, error)
        };
        let mut control = |database, missing: &'static str| -> Result<PullReceipt> {
            stage
                .with_database(database, |db| {
                    pull(database, db, ReceivedPull::Control)
                        .map_err(|error| crate::store::Error::System(error.to_string()))
                })
                .map_err(|error| scope_error(database, error.into()))?
                .ok_or_else(|| scope_error(database, missing.into()))
        };
        let index = control(DatabaseId::Index, "index source changed before capture")?;
        let cut = SnapshotCut::new(index.source, index.revision.policy_revision)
            .map_err(|error| scope_error(DatabaseId::Index, error.into()))?;
        let revocations =
            control(DatabaseId::Revocations, "revocation source changed before capture")?;
        if !cut.accepts(revocations.source, revocations.revision.policy_revision) {
            return Ok(None);
        }
        if stage.entry_scope().is_none() {
            for (database, receipt) in
                [(DatabaseId::Index, &index), (DatabaseId::Revocations, &revocations)]
            {
                if receipt.hosted {
                    stage
                        .record_receipt(database, &receipt.revision)
                        .map_err(|error| scope_error(database, error.into()))?;
                }
            }
        }
        Ok(Some((cut, CapturedControls { index, revocations })))
    }

    /// The caller has matched both control receipts to one source cut. A true
    /// result requires discarding that stage and recapturing before adoption.
    fn restore_captured_controls(
        stage: &crate::store::ReceivedVolume,
        controls: &CapturedControls,
        restore: impl FnOnce(&crate::store::Database) -> Result<bool>,
    ) -> Result<bool> {
        if !controls.index.ap_source
            || !controls.revocations.ap_source
            || !controls.index.hosted
            || !controls.revocations.hosted
            || stage.entry_scope().is_some()
        {
            return Ok(false);
        }
        Ok(stage.with_database(crate::store::DatabaseId::Index, |database| {
            if database.begin_read()?.root_hash()? != controls.index.revision.root {
                return Err(crate::store::Error::AccessDenied("AP index capture differs"));
            }
            restore(database).map_err(|error| crate::store::Error::System(error.to_string()))
        })?)
    }

    fn qualify_received_authority(
        stage: &mut crate::store::ReceivedVolume,
        cut: crate::store::SnapshotCut,
        pull: &mut BootstrapPull<'_>,
    ) -> Result<bool> {
        use crate::store::DatabaseId;
        let _timing = crate::trace::Span::new("replication.qualify_received_authority");
        // Recheck current consent, Read and participants after the last backing,
        // through authenticated AP at the same source cut.
        let qualified = stage.with_database(DatabaseId::Index, |db| {
            let receipt = pull(DatabaseId::Index, db, ReceivedPull::Control)
                .map_err(|error| crate::store::Error::System(error.to_string()))?;
            let root = db.begin_read()?.root_hash()?;
            // AP binds this root to its filtered projection, not the complete
            // catalog. A matching source token alone cannot qualify extra local
            // facts that survived the transfer. The source clock may differ.
            Ok(receipt.is_some_and(|receipt| {
                receipt.ap_source
                    && cut.accepts(receipt.source, receipt.revision.policy_revision)
                    && receipt.revision.root == root
            }))
        })?;
        if !qualified {
            return Ok(false);
        }
        stage.qualify_authority();
        Ok(true)
    }

    /// One stable cut identifies every missing backing before any repair changes
    /// AP. Keep at most 1024 entry IDs and project one retirement at a time.
    fn pull_received_backings(
        store: &LocalStore,
        access: &Access,
        stage: &mut crate::store::ReceivedVolume,
        (cut, world): (crate::store::SnapshotCut, bool),
        ap_source: &mut bool,
        pull: &mut BootstrapPull<'_>,
        mut can_retire: impl FnMut(&crate::store::ReceivedVolume, crate::store::EntryId) -> Result<bool>,
    ) -> Result<Option<Vec<crate::store::EntryId>>> {
        let mut after = None;
        let mut local_cut = None;
        let mut missing = Vec::new();
        let volume = stage.volume();
        let owner_capture = access.session_id().is_none() && *ap_source;
        let retained_owner = owner_capture
            && stage.entry_scope().is_none()
            && store
                .durable_revision(volume, crate::store::DatabaseId::Index)
                .map_err(|error| {
                    sync_failure::database(
                        store,
                        access,
                        volume,
                        crate::store::DatabaseId::Index,
                        error,
                    )
                })?
                .is_some();
        let service_only = world || owner_capture;
        loop {
            let page = stage.database_page(after).map_err(|error| {
                sync_failure::database(
                    store,
                    access,
                    stage.volume(),
                    crate::store::DatabaseId::Index,
                    error,
                )
            })?;
            for database in page.databases {
                let scope_error =
                    |error| sync_failure::database(store, access, volume, database, error);
                let Some(receipt) = Self::pull_staged_backing(
                    store,
                    access,
                    stage,
                    database,
                    (cut, world || retained_owner),
                    service_only,
                    pull,
                )
                .map_err(scope_error)?
                else {
                    return Ok(None);
                };
                match receipt {
                    StagedBacking::Unreadable => continue,
                    StagedBacking::Missing(_) if world || retained_owner => {
                        // Adopt ready entries together. A pending retirement can depend
                        // on an edit to a different entry in this capture.
                        stage
                            .discard_pending_backing(database)
                            .map_err(|error| scope_error(error.into()))?;
                        continue;
                    }
                    StagedBacking::Missing(entry) => {
                        // A cold replica with no retirement must retain the readiness
                        // retry, even if later databases change their source cut.
                        if !can_retire(stage, entry).map_err(scope_error)? {
                            if missing.is_empty() {
                                return Err(scope_error(
                                    "waiting for initial database synchronization".into(),
                                ));
                            }
                            // Publish earlier eligible retirements before the next wait.
                            return Ok(Some(missing));
                        }
                        if missing.len() == crate::store::volume_index::namespace::MAX_ENTRIES {
                            return Err(crate::store::Error::LimitExceeded.into());
                        }
                        missing.push(entry);
                        continue;
                    }
                    StagedBacking::Remote(receipt) => {
                        *ap_source &= receipt.ap_source;
                        if !cut.accepts(receipt.source, receipt.revision.policy_revision) {
                            return Ok(None);
                        }
                        if receipt.hosted {
                            stage
                                .record_receipt(database, &receipt.revision)
                                .map_err(|error| scope_error(error.into()))?;
                        }
                    }
                    StagedBacking::Local(token) => {
                        *ap_source = false;
                        if local_cut.is_some_and(|cut| cut != token) {
                            return Ok(None);
                        }
                        local_cut = Some(token);
                    }
                }
                stage.finish_database(database).map_err(|error| scope_error(error.into()))?;
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        if let Some(token) = local_cut
            && !Self::local_backing_cut_is_current(store, access, stage, token)?
        {
            return Ok(None);
        }
        Ok(Some(missing))
    }

    fn local_backing_cut_is_current(
        store: &LocalStore,
        access: &Access,
        stage: &crate::store::ReceivedVolume,
        token: crate::store::SnapshotToken,
    ) -> Result<bool> {
        use crate::store::DatabaseId;
        let scope = stage.entry_scope().map_or(DatabaseId::Index, DatabaseId::Entry);
        store
            .with_authorized_replica(
                access,
                stage.volume(),
                scope,
                Some(Permission::Read),
                |_, _| Ok(store.source_snapshot_token(stage.volume())? == token),
            )
            .map_err(|error| sync_failure::database(store, access, stage.volume(), scope, error))
    }

    fn pull_staged_backing(
        store: &LocalStore,
        access: &Access,
        stage: &crate::store::ReceivedVolume,
        database: crate::store::DatabaseId,
        (cut, defer_missing): (crate::store::SnapshotCut, bool),
        service_only: bool,
        pull: &mut BootstrapPull<'_>,
    ) -> Result<Option<StagedBacking>> {
        let protocol = stage.with_database(crate::store::DatabaseId::Index, |index| {
            let policy = crate::store::DatabasePolicy::read_source(index, database)?;
            let actor = store.data_actor(access, policy.volume.volume())?;
            if service_only {
                policy.authorize_reader(ReplicaReader::Service)?;
            }
            policy.authorize_reader(ReplicaReader::SessionParticipant(actor))?;
            policy
                .entry
                .and_then(|entry| entry.protocol)
                .ok_or(crate::store::Error::InvalidInput("staged entry is not a database"))
        });
        let protocol = match protocol {
            Ok(protocol) => protocol,
            Err(crate::store::Error::AccessDenied(_)) => {
                return Ok(Some(StagedBacking::Unreadable));
            }
            Err(error) => return Err(error.into()),
        };
        let result = stage.with_database(database, |db| {
            pull(database, db, ReceivedPull::Backing { cut, protocol })
                .map_err(|error| crate::store::Error::System(error.to_string()))
        });
        match result {
            Ok(receipt) => Ok(receipt.map(StagedBacking::Remote)),
            Err(error) if error.to_string() == "waiting for initial database synchronization" => {
                let crate::store::DatabaseId::Entry(entry) = database else {
                    return Err(error.into());
                };
                if defer_missing {
                    return Ok(Some(StagedBacking::Missing(entry)));
                }
                Ok(Some(match store.stage_local_backing(access, stage, database) {
                    Ok(token) => StagedBacking::Local(token),
                    Err(_) => StagedBacking::Missing(entry),
                }))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn acknowledge_received(
        &self,
        store: &LocalStore,
        access: &Access,
        stage: &crate::store::ReceivedVolume,
    ) -> Result<()> {
        let mut after = None;
        loop {
            let page = stage.receipt_page(after).map_err(|error| {
                sync_failure::database(
                    store,
                    access,
                    stage.volume(),
                    crate::store::DatabaseId::Index,
                    error,
                )
            })?;
            for (database, receipt) in page.receipts {
                let scope_error =
                    |error| sync_failure::database(store, access, stage.volume(), database, error);
                if database == crate::store::DatabaseId::Index {
                    stage
                        .with_database(database, |source| {
                            store.acknowledge_provider_received(
                                stage.volume(),
                                source,
                                receipt.root,
                            )
                        })
                        .map_err(scope_error)?;
                }
                if matches!(database, crate::store::DatabaseId::Entry(_))
                    && store.has_world_attachment(stage.volume()).map_err(scope_error)?
                {
                    stage
                        .with_database(database, |source| {
                            store.acknowledge_world_append_snapshot(
                                stage.volume(),
                                database,
                                source,
                                receipt.root,
                            )
                        })
                        .map_err(scope_error)?;
                }
                match store.projection_root(stage.volume(), database, ReplicaReader::Service) {
                    Ok(root) if root == receipt.root => store
                        .acknowledge_replica(stage.volume(), database, &receipt)
                        .map_err(scope_error)?,
                    Ok(_)
                    | Err(
                        crate::store::Error::NotFound(_) | crate::store::Error::AccessDenied(_),
                    ) => (),
                    Err(error) => return Err(scope_error(error)),
                }
            }
            let Some(next) = page.next else { return Ok(()) };
            after = Some(next);
        }
    }

    fn pull_owner_volume(
        &self,
        store: &LocalStore,
        volume: Id,
    ) -> Result<Option<crate::store::DatabaseId>> {
        let _timing = crate::trace::Span::new("replication.pull_owner_volume");
        let baseline = if self.live.as_ref().is_some_and(|live| live.has_receiver(volume)) {
            match store.source_snapshot_token(volume) {
                Ok(token) => Some(token),
                Err(crate::store::Error::Store(kvdb::Error::Busy)) => {
                    return Err(initialization::CHANGING_CUT.into());
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };
        let mut stage = self.receive_volume(store, None, volume)?;
        stage.owner_baseline = baseline;
        let access = Access::owner(store.account().id);
        match store.adopt_received_volume(&access, &stage) {
            Err(crate::store::Error::LocalContention(
                crate::store::Error::OWNER_CAPTURE_CHANGED,
            )) => {
                return Err(initialization::CHANGING_CUT.into());
            }
            result => result?,
        };
        self.acknowledge_received(store, &access, &stage)?;
        Ok(stage.pending_backing)
    }

    fn pull_bound_volume(
        &self,
        store: &LocalStore,
        session: &Session,
        volume: Id,
    ) -> Result<ReceivedVolume> {
        if volume != session.replica_volume() && !session.is_distinct_world(volume) {
            return Err("volume does not belong to replica session".into());
        }
        let _timing = crate::trace::Span::new("replication.pull_bound_volume");
        let access =
            Access::session_version(store.account().id, &session.id, &session.resolved_build_id);
        let stage = self.receive_volume(store, Some(session), volume)?;
        store.adopt_received_revocations(&access, &stage).map_err(|error| {
            sync_failure::database(
                store,
                &access,
                volume,
                crate::store::DatabaseId::Revocations,
                error,
            )
        })?;
        store.adopt_received_volume(&access, &stage)?;
        self.acknowledge_received(store, &access, &stage)?;
        Ok(stage)
    }
    fn transfer_database(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        database: crate::store::DatabaseId,
        direction: SyncDirection,
        db: &crate::store::Database,
    ) -> crate::store::Result<crate::store::ReplicaReceipt> {
        self.transfer_scoped_database(store, session, volume, (database, None), direction, db)
    }

    fn transfer_scoped_database(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        target: (crate::store::DatabaseId, Option<crate::store::EntryId>),
        direction: SyncDirection,
        db: &crate::store::Database,
    ) -> crate::store::Result<crate::store::ReplicaReceipt> {
        let _timing = crate::trace::Span::new(match direction {
            SyncDirection::Pull => "replication.pull_database",
            SyncDirection::Push => "replication.push_database",
        });
        let (database, entry_scope) = target;
        let route = store.world_replica_route(volume, session)?;
        let wire_key = if route == crate::store::WorldReplicaRoute::CredentialedRoot {
            Some(store.replica_hash_key(volume, database)?)
        } else {
            None
        };
        let request = if route == crate::store::WorldReplicaRoute::PublisherWorld {
            let session = session
                .ok_or(crate::store::Error::AccessDenied("World projection requires a session"))?;
            ServiceRequest::WorldReplica {
                operation_id: Id::generate()?.to_string(),
                session_id: session.id.clone(),
                build_id: session.resolved_build_id.clone(),
                volume_id: volume.to_string(),
                request: crate::network::ap::operation::WorldReplicaRequest::Sync {
                    database_id: database,
                    direction,
                    entry_scope,
                },
            }
        } else {
            ServiceRequest::SyncDatabase {
                entry_scope,
                operation_id: Id::generate()?.to_string(),
                session_id: session.map(|s| s.id.clone()),
                build_id: session.map(|s| s.resolved_build_id.clone()),
                volume_id: volume.to_string(),
                database_id: database,
                direction,
            }
        };
        let connection = self
            .connection
            .as_ref()
            .ok_or(crate::store::Error::System("AP disconnected".into()))?;
        self.runtime
            .block_on(async {
                let mut stream = wire::begin(connection, &request).await?;
                match direction {
                    SyncDirection::Pull => {
                        pull_database_stream(
                            &self
                                .identity_path
                                .parent()
                                .ok_or("identity has no profile")?
                                .join(crate::paths::STAGING),
                            &mut stream,
                            db,
                            request.operation_id(),
                            wire_key,
                        )
                        .await?;
                    }
                    SyncDirection::Push => {
                        wire::response(&mut stream, request.operation_id()).await?;
                        push_database_stream(&mut stream, db, request.operation_id(), wire_key)
                            .await?;
                    }
                }
                let evidence = if direction == SyncDirection::Pull
                    && database == crate::store::DatabaseId::Index
                {
                    Some(wire::receive_namespace_evidence(&mut stream).await?)
                } else {
                    None
                };
                stream.finish()?;
                let response = wire::response(&mut stream, request.operation_id()).await?;
                let mut durable: crate::store::ReplicaReceipt =
                    serde_json::from_value(response.data.ok_or("missing durable revision")?)?;
                if let Some(key) = wire_key
                    && direction == SyncDirection::Pull
                {
                    // The receipt names the credentialed server tree. Verify that
                    // exact root before binding its evidence to the local tree.
                    let sender = SyncSender::new(db)?;
                    if world::wire_root(&sender, key).await? != durable.durable.root {
                        return Err("credentialed World receipt differs from delivered data".into());
                    }
                    durable.durable.root = sender.root_hash()?;
                }
                if let Some(evidence) = evidence {
                    crate::store::volume_index::namespace::receipt::retain(
                        db,
                        &durable,
                        entry_scope,
                        &evidence,
                    )?;
                }
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(durable)
            })
            .map_err(|error| {
                if entry_scope.is_some()
                    && direction == SyncDirection::Push
                    && error
                        .downcast_ref::<ServiceFailure>()
                        .is_some_and(ServiceFailure::is_kv_append_conflict)
                {
                    crate::store::Error::AlreadyExists("World KV append")
                } else {
                    crate::store::Error::System(error.to_string())
                }
            })
    }
    fn pull_received_log(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        volume: Id,
        log: crate::store::EntryId,
        target: &crate::store::Database,
        cut: crate::store::SnapshotCut,
    ) -> Result<Option<PullReceipt>> {
        let private_root = match session {
            Some(session) => !store.volume(session.replica_volume())?.share_key_with_service,
            None => false,
        };
        let private = !store.volume(volume)?.share_key_with_service;
        if private_root
            || private && (session.is_some() || !store.has_direct_volume_grant(volume)?)
        {
            return Err("waiting for initial database synchronization".into());
        }
        let connection = self.connection.as_ref().ok_or("AP disconnected")?;
        let mut cursor = None;
        loop {
            let receipt = if private {
                super::peer::pull_log_page(
                    &self.runtime,
                    connection,
                    store,
                    volume,
                    log,
                    cursor.as_ref(),
                )?
            } else {
                let request = ServiceRequest::GetBloblog {
                    operation_id: Id::generate()?.to_string(),
                    volume_id: volume.to_string(),
                    session_id: session.map(|s| s.id.clone()),
                    build_id: session.map(|s| s.resolved_build_id.clone()),
                    log_id: log,
                    cursor: cursor.as_ref().map(ToString::to_string),
                };
                let response = self.runtime.block_on(wire::request(connection, &request))?;
                serde_json::from_value(response.data.ok_or("missing bloblog page")?)?
            };
            receipt.cut()?;
            if !cut.accepts(receipt.source, receipt.policy_revision) {
                return Ok(None);
            }
            let pulled = PullReceipt::from_log(&receipt, !private);
            let page = receipt.page;
            if page.more && (page.entries.is_empty() || cursor.as_ref() == Some(&page.cursor)) {
                return Err("bloblog catch-up did not advance".into());
            }
            crate::store::bloblog::Log {
                database: target,
                volume,
                entry: log,
                encryption_key: None,
                now: now(),
            }
            .apply_page(&page)?;
            cursor = Some(page.cursor);
            if !page.more {
                return Ok(Some(pulled));
            }
        }
    }
    fn push_volume(
        &self,
        store: &LocalStore,
        participant: Option<(&Session, &ReceivedVolume)>,
        volume: Id,
    ) -> Result<()> {
        let session = participant.map(|(session, _)| session);
        let descriptor = store.volume(volume)?;
        let access = session.map_or_else(
            || Access::owner(store.account().id),
            |session| {
                Access::session_version(store.account().id, &session.id, &session.resolved_build_id)
            },
        );
        let world = match session {
            Some(session) => store.world_append_scope(session, volume)?,
            None => false,
        };
        let mut after = None;
        let mut service = descriptor.share_key_with_service;
        loop {
            let page = store.replication_database_page(volume, after)?;
            for database in page.databases {
                let scope_error =
                    |error| sync_failure::database(store, &access, volume, database, error);
                if !store.replica_upload_allowed(&access, volume, database).map_err(scope_error)? {
                    continue;
                }
                if world && let crate::store::DatabaseId::Entry(entry) = database {
                    let details =
                        store.entry_details(&access, volume, entry).map_err(scope_error)?;
                    if !details.backing_ready {
                        continue;
                    }
                    if details.readable
                        && let Some((_, received)) = participant
                        && received
                            .hosted_backing_matches(
                                entry,
                                store.replica_root(volume, database).map_err(scope_error)?,
                            )
                            .map_err(scope_error)?
                    {
                        // The freshly pulled AP cut already contains this exact local
                        // Merkle tree. The rebased live feed covers later local writes.
                        continue;
                    }
                    if !details.readable {
                        let submit = |source: &crate::store::Database| {
                            self.transfer_scoped_database(
                                store,
                                session,
                                volume,
                                (database, Some(entry)),
                                SyncDirection::Push,
                                source,
                            )
                            .map(|_| ())
                        };
                        let result = if !details.writable
                            && details.appendable
                            && matches!(
                                details.entry.info.kind,
                                crate::store::volume_index::EntryKind::KvDb {}
                            ) {
                            store.publish_pending_world_appends(volume, database, submit)
                        } else {
                            store.publish_pending_world_backing(volume, database, submit)
                        };
                        match result {
                            Ok(()) | Err(crate::store::Error::AccessDenied("entry permission")) => {
                                continue;
                            }
                            Err(error) => return Err(scope_error(error)),
                        }
                    }
                }
                if service {
                    let result = store.with_outbound_replica(
                        volume,
                        database,
                        ReplicaReader::Service,
                        |source| {
                            let receipt = self
                                .transfer_database(
                                    store,
                                    session,
                                    volume,
                                    database,
                                    SyncDirection::Push,
                                    source,
                                )
                                .map_err(|error| crate::store::Error::System(error.to_string()))?;
                            if matches!(database, crate::store::DatabaseId::Entry(_))
                                && store.has_world_attachment(volume)?
                            {
                                store.acknowledge_world_append_snapshot(
                                    volume,
                                    database,
                                    source,
                                    receipt.durable.root,
                                )?;
                            }
                            Ok(receipt)
                        },
                    );
                    match result {
                        Ok(receipt) => {
                            // A concurrently merged server tree can differ from
                            // this upload. The verified follow-up pull records
                            // its durable acknowledgment in the local hash context.
                            if store.world_replica_route(volume, session).map_err(scope_error)?
                                != crate::store::WorldReplicaRoute::CredentialedRoot
                            {
                                store
                                    .acknowledge_replica(volume, database, &receipt.durable)
                                    .map_err(scope_error)?;
                            }
                            continue;
                        }
                        Err(crate::store::Error::AccessDenied("entry permission")) => continue,
                        Err(error)
                            if error.to_string().contains("waiting for an authorized peer")
                                && session.is_some_and(|s| s.shared_volume.is_none()) =>
                        {
                            service = false
                        }
                        Err(error) => return Err(scope_error(error)),
                    }
                }
                crate::engine::peer::push(
                    &self.runtime,
                    self.connection.as_ref().ok_or("AP disconnected")?,
                    store,
                    session,
                    volume,
                    database,
                )
                .map_err(|error| sync_failure::database(store, &access, volume, database, error))?;
            }
            let Some(next) = page.next else { break };
            after = Some(next);
        }
        Ok(())
    }

    /// A push rejection cannot prevent an authorized incoming contraction.
    /// Refresh once, retain rejected data without ACK, and resume a read-only
    /// stream only after the complete fresh cut removes every upload right.
    /// Rebase away capture-time feed pressure only when its durable writes are
    /// included in the fresh outbound snapshots. The installed AP subscription
    /// covers those snapshots and every later live change.
    fn finish_captured_live(
        &mut self,
        store: &Arc<LocalStore>,
        participant: Option<(&Session, ReceivedVolume)>,
        volume: Id,
        mut prepared: crate::engine::live::PreparedLive,
    ) -> Result<()> {
        self.runtime.block_on(
            self.live.as_mut().ok_or("live replication is unavailable")?.rebase(&mut prepared),
        )?;
        let session = if let Some((session, received)) = participant {
            self.push_bootstrap_participant(store, session, received)?;
            Some(session)
        } else {
            self.push_volume(store, None, volume)?;
            None
        };
        self.runtime.block_on(
            self.live.as_ref().ok_or("live replication is unavailable")?.activate(&mut prepared),
        )?;
        self.runtime.block_on(
            self.live.as_mut().ok_or("live replication is unavailable")?.finish(prepared),
        )?;
        // A bootstrap can absorb edits made before this process or connection
        // started. Their feed records no longer exist to drive live uploads.
        self.sync_blob_volume(store, volume, session.map(|session| session.id.as_str()))
    }

    fn push_bootstrap_participant(
        &mut self,
        store: &Arc<LocalStore>,
        session: &Session,
        received: ReceivedVolume,
    ) -> Result<bool> {
        let volume = received.volume();
        let result = self.push_volume(store, Some((session, &received)), volume);
        drop(received);
        let Err(error) = result else {
            return Ok(false);
        };
        if !sync_failure::reason(error.as_ref())
            .to_string()
            .starts_with("unauthorized replica entry: access denied:")
        {
            return Err(error);
        }
        self.pull_bound_volume(store, session, volume)?;
        let access =
            Access::session_version(store.account().id, &session.id, &session.resolved_build_id);
        if store.replica_upload_allowed(&access, volume, crate::store::DatabaseId::Index)? {
            return Err(error);
        }
        Ok(true)
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.volume_invitation_watch = None;
        if let Some(lookup) = self.address_lookup.take() {
            lookup.abort();
        }
        self.connection_watch = None;
        self.connection_status.set(
            ApConnectionState::Offline,
            self.connection_hostname(),
            &self.completion,
        );
        // Stable GUI/blob handles must not keep publishing after their lifecycle
        // owner is lost (including an unwinding background worker).
        if let Some(live) = &mut self.live {
            live.publisher().shutdown();
        }
        *self.sync_connection.lock().unwrap_or_else(|error| error.into_inner()) = None;
        self.connection = None;
        let transport =
            self.authenticated_connection.lock().unwrap_or_else(|error| error.into_inner()).take();
        // Unlike `disconnect`, this runs on Engine teardown: no owner/UI mutex
        // is held here, and the network runtime this blocks on is the one
        // about to disappear. Waiting here (each bounded, so a dead network
        // cannot hang teardown) is what actually gets the CONNECTION_CLOSE
        // frame out before a short-lived process exits; otherwise quinn only
        // queues it, and the AP holds this device's slots reserved until its
        // own idle timeout.
        if let Some(drain) = self.pending_drain.take() {
            // A prior explicit `disconnect` already spawned this; without
            // joining it here, dropping the runtime below would abandon it
            // mid-flight just like an unbounded fire-and-forget close would.
            let _ = self
                .runtime
                .block_on(async { tokio::time::timeout(Duration::from_millis(300), drain).await });
        }
        if let Some(connection) = transport {
            connection.shutdown();
            self.runtime.block_on(connection.wait_closed(Duration::from_millis(250)));
        }
        for (_, task) in
            std::mem::take(self.listeners.get_mut().unwrap_or_else(|error| error.into_inner()))
        {
            task.abort();
        }
    }
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs()
}
fn stable_operation(kind: &str, identity: &str) -> String {
    // This ID is local and deterministic across process restart; entity bytes remain conflict checked remotely.
    let mut bytes = kind.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend(identity.as_bytes());
    crate::identifier::encode(&blake3::hash(&bytes).as_bytes()[..16])
}

fn waiting_peer(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    sync_failure::reason(error).to_string().contains("waiting for an authorized peer")
}

async fn pull_database_stream(
    scratch: &Path,
    stream: &mut crate::network::ap::ApStream,
    database: &crate::store::Database,
    operation: &str,
    wire_key: Option<[u8; 32]>,
) -> Result<()> {
    let mut output = Vec::new();
    let staging = crate::store::sync_staging(scratch)?;
    let mut receiver = match wire_key {
        Some(key) => SyncReceiver::new_projection(database, key, Lww, staging, now(), &mut output)?,
        None => SyncReceiver::new(database, Lww, staging, now(), &mut output)?,
    };
    // Queue the initial root with the AP request. The server still authorizes
    // the operation before processing it, and we check its response before
    // accepting any sync data. Waiting first adds a round trip to every pull.
    // A rejection can stop this write while its response remains readable.
    // Preserve that correlated error for peer and local-backing recovery.
    let initial_write = wire::send_data(stream, &output).await;
    wire::response(stream, operation).await?;
    initial_write?;
    loop {
        output.clear();
        let input = sync_payload(wire::receive_transfer(stream).await?, operation)?;
        if receiver.process(&input, &mut output)? == SyncStatus::Done {
            wire::send_data(stream, &output).await?;
            receiver.finish()?;
            return Ok(());
        }
        wire::send_data(stream, &output).await?;
    }
}

async fn push_database_stream(
    stream: &mut crate::network::ap::ApStream,
    database: &crate::store::Database,
    operation: &str,
    wire_key: Option<[u8; 32]>,
) -> Result<()> {
    let mut sender = SyncSender::new(database)?;
    if let Some(key) = wire_key {
        let root = world::wire_root(&sender, key).await?;
        sender.use_projection(key, root)?;
    }
    let mut output = vec![0; 256 * 1024];
    loop {
        let input = sync_payload(wire::receive_transfer(stream).await?, operation)?;
        let status = sender.process(&input)?;
        let mut sent = false;
        loop {
            let length = {
                let _timing = crate::trace::Span::new("replication.sender_output");
                sender.try_write_output(&mut output)?
            };
            let Some(length) = length else {
                tokio::task::yield_now().await;
                continue;
            };
            if length == 0 {
                break;
            }
            wire::send_data(stream, &output[..length]).await?;
            sent = true;
        }
        if status == SyncStatus::Done {
            // An empty/equal sender completes before the receiver's Bye.
            if sent {
                let terminal = sync_payload(wire::receive_transfer(stream).await?, operation)?;
                sender.process(&terminal)?;
            }
            return Ok(());
        }
    }
}

// A rejected transfer ends with its normal service response instead of another
// binary kvdb packet. Preserve the authenticated operation error before kvdb
// parsing, and never mistake an early success response for a completed transfer.
fn sync_payload(bytes: Vec<u8>, operation_id: &str) -> Result<Vec<u8>> {
    if let Ok(response) = serde_json::from_slice::<ServiceResponse>(&bytes) {
        crate::network::ap::service::validated_response(response, operation_id)?;
        return Err("AP ended database transfer before synchronization completed".into());
    }
    Ok(bytes)
}

#[cfg(all(test, feature = "app-host"))]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    mod ap_status;
    mod cancellation;
    mod work_offline;

    #[test]
    fn initial_readiness_completion_runs_until_recovery_is_exhausted() {
        // Exercise real background completion: a retained finite retry is in
        // progress, while terminal errors and exhausted episodes stay failed.
        use super::*;
        use crate::engine::{
            background::{NetworkResult, tests::transfer},
            reply::TransferKind,
        };
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let volume = Id::from_bytes([1; 16]);
        let waiting = "waiting for initial database synchronization";
        for waiting in [waiting, "AP request capacity exhausted"] {
            engine.remote.as_mut().unwrap().initializations.clear();
            let mut now = Instant::now();
            for attempt in 0..=6 {
                let (send, mut remote) = transfer(&mut engine);
                if attempt != 0 {
                    now += remote.initializations.wake_after(now).unwrap();
                    let mut due = Vec::new();
                    remote.initializations.select(&mut due, now);
                    assert_eq!(due, [volume]);
                    remote.initializations.start(volume);
                }
                remote.initializations.failed(volume, waiting, now).unwrap();
                let pending = initialization::Pending(waiting.into());
                assert_eq!(remote.initialization_retrying(&pending), attempt < 6);
                let result = if attempt < 6 {
                    Ok(NetworkResult::InitializationPending(waiting.into()))
                } else {
                    Err(waiting.into())
                };
                send.send((remote, result, BTreeMap::new())).unwrap();
                engine
                    .background
                    .as_ref()
                    .unwrap()
                    .ready
                    .store(true, std::sync::atomic::Ordering::Release);
                engine.complete_background().unwrap();
                assert_eq!(
                    engine.transfer_status().kind,
                    if attempt < 6 { TransferKind::Running } else { TransferKind::Failed }
                );
            }
        }
        engine.remote.as_mut().unwrap().initializations.clear();
        for message in
            ["storage failed", "permission denied", waiting, "AP request capacity exhausted"]
        {
            let (send, mut remote) = transfer(&mut engine);
            remote.initializations.failed(volume, waiting, Instant::now()).unwrap();
            // An unrelated wait with identical text has no owner retry identity.
            let error: Box<dyn std::error::Error + Send + Sync> = message.into();
            assert!(!remote.initialization_retrying(error.as_ref()));
            send.send((remote, Err(message.into()), BTreeMap::new())).unwrap();
            engine
                .background
                .as_ref()
                .unwrap()
                .ready
                .store(true, std::sync::atomic::Ordering::Release);
            engine.complete_background().unwrap();
            assert_eq!(engine.transfer_status().kind, TransferKind::Failed);
        }
        let (send, mut remote) = transfer(&mut engine);
        remote.initializations.forget(volume);
        send.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
        engine.background.as_ref().unwrap().ready.store(true, std::sync::atomic::Ordering::Release);
        engine.complete_background().unwrap();
        assert_eq!(engine.transfer_status().kind, TransferKind::Complete);
    }

    #[test]
    fn deleted_volume_discovery_does_not_fail_initialization() {
        use super::*;
        use std::{io::Read, os::unix::net::UnixStream};
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path().join("store")).unwrap());
        let (mut ready, completion) = UnixStream::pair().unwrap();
        ready.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        remote.identity = preissued_identity(&store);
        remote.ensure_live(&store).unwrap();
        let publisher = remote.live.as_ref().unwrap().publisher();
        let volume = store.create_volume("Deleted before discovery", false).unwrap();
        while !publisher.needs_initialization() {
            ready.read_exact(&mut [0]).unwrap();
        }
        // Account replay can announce a capability before its tombstone arrives.
        remote
            .initializations
            .failed(
                volume.id,
                "waiting for initial database synchronization",
                Instant::now() - Duration::from_secs(1),
            )
            .unwrap();
        store.delete_volume(volume.id).unwrap();
        remote.initialize_volumes(&store).unwrap();
        assert!(remote.initializations.wake_after(Instant::now()).is_none());
        assert!(publisher.state(volume.id).is_none());
        assert!(!publisher.needs_initialization());
        assert!(!matches!(
            publisher.health(store.change_status(), store.pending_change_count()),
            crate::engine::publisher::PublishHealth::Failed
        ));
        assert!(store.volume(volume.id).is_err());
    }

    #[test]
    fn exact_cached_build_needs_no_network_but_missing_corrupt_and_wrong_builds_do() {
        use super::*;
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Cached build", false).unwrap();
        let package = crate::package::signature::testing::signed(
            &engine.services.store,
            crate::engine::tests::button_package(),
        )
        .unwrap();
        let bytes = package.to_bytes().unwrap();
        let session = engine.create_session(package, root.id).unwrap();
        let path = directory.path().join(crate::paths::BUILDS).join(&session.resolved_build_id);
        let store = &engine.services.store;
        let remote = engine.remote.as_mut().unwrap();
        let canceled = Arc::new(crate::engine::cancellation::Canceled::default());
        canceled.raise();
        remote.set_cancellation(Some(canceled));
        // A canceled transport makes any attempted connection fail before DNS.
        // The exact local build remains reusable, repeatedly, without AP.
        for _ in 0..2 {
            remote.ensure_build(store, &session, &path).unwrap();
            assert!(remote.connection.is_none());
        }
        for contents in [
            None,
            Some(b"corrupt bundle".to_vec()),
            Some(
                crate::package::Package::new(
                    "test".into(),
                    "A different build".into(),
                    b"\0asm\x01\0\0\0".to_vec(),
                    BTreeMap::new(),
                )
                .unwrap()
                .to_bytes()
                .unwrap(),
            ),
        ] {
            if let Some(contents) = contents {
                fs::write(&path, contents).unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
            let error = remote.ensure_build(store, &session, &path).unwrap_err();
            assert!(error.downcast_ref::<CanceledOperation>().is_some());
        }
        fs::write(&path, bytes).unwrap();
        remote.ensure_build(store, &session, &path).unwrap();
    }

    fn gap_fixture() -> (
        tempfile::TempDir,
        crate::engine::Engine,
        crate::engine::live::ScopeKey,
        crate::engine::live::ScopeKey,
    ) {
        use crate::engine::live::{LiveReplicas, RecoveryKind, ScopeKey, tests::observed_entry};
        use crate::engine::publisher::PublishState;
        use crate::engine::replication::ReplicaState;
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let remote = engine.remote.as_mut().unwrap();
        let mut live = LiveReplicas::new(
            remote.runtime.handle(),
            store.clone(),
            remote.completion.clone(),
            remote.received.clone(),
            remote.blob_transfers.clone(),
            Default::default(),
        )
        .unwrap();
        let scope = ScopeKey { entry: None, volume: Id::from_bytes([1; 16]), session: None };
        let neighbor = ScopeKey { entry: None, volume: Id::from_bytes([2; 16]), session: None };
        observed_entry(&mut live, scope, 1, [1; 16], PublishState::Ready, ReplicaState::Gap);
        observed_entry(&mut live, neighbor, 1, [1; 16], PublishState::Ready, ReplicaState::Live);
        assert_eq!(live.take_recoveries(RecoveryKind::Gap), [scope]);
        remote.live = Some(live);
        (directory, engine, scope, neighbor)
    }

    #[test]
    fn persistent_capture_gaps_exhaust_one_episode_and_leave_other_scopes_live() {
        use crate::engine::live::{LiveGap, RecoveryKind, tests::observed_entry};
        use crate::engine::publisher::PublishState;
        use crate::engine::replication::ReplicaState;
        let entry = crate::store::EntryId::from_bytes([6; 15]).unwrap();
        let database = crate::store::DatabaseId::Entry(entry);
        let waiting = "replica bootstrap kept losing its live boundary after three captures\nAlso waiting for initial database synchronization";
        for (pending, apply, expected) in [
            (None, false, None),
            (Some(database), false, Some(waiting)),
            (Some(crate::store::DatabaseId::Index), true, Some("pending backing")),
        ] {
            let (_directory, mut engine, scope, neighbor) = gap_fixture();
            let store = engine.services.store.clone();
            let remote = engine.remote.as_mut().unwrap();
            let mut captures = 0;
            let error = remote
                .recover_scope_with(&store, &scope, RecoveryKind::Gap, |remote, _, key, _| {
                    assert_eq!(*key, scope);
                    captures += 1;
                    let live = remote.live.as_mut().unwrap();
                    // Each replacement has new identities but belongs to the same
                    // admitted episode. Its checkpoint again reports a current Gap.
                    observed_entry(
                        live,
                        scope,
                        captures + 1,
                        [captures as u8 + 1; 16],
                        PublishState::Ready,
                        ReplicaState::Gap,
                    );
                    assert!(live.recovery_is_current(&scope, RecoveryKind::Gap));
                    assert!(!live.needs_gap_recovery());
                    assert!(live.is_installed(neighbor.volume, None));
                    let gap = if apply {
                        sync_failure::apply_gap(sync_failure::target_database(
                            scope.volume,
                            database,
                            "pending backing",
                        ))
                    } else {
                        live.complete_recovery(&scope).unwrap_err()
                    };
                    Err(sync_failure::captured_backing_gap(scope.volume, pending, gap))
                })
                .unwrap_err();
            assert_eq!(captures, 3);
            if let Some(reason) = expected {
                assert_eq!(
                    error.to_string(),
                    format!(
                        "Couldn’t sync database {database} in volume {}\n{reason}",
                        scope.volume
                    )
                );
                assert!(!sync_failure::reason(error.as_ref()).is::<LiveGap>());
            } else {
                assert!(error.to_string().contains("after three captures"));
            }
            let live = remote.live.as_mut().unwrap();
            assert!(!live.needs_gap_recovery());
            assert!(live.take_recoveries(RecoveryKind::Gap).is_empty());
            assert!(!live.is_installed(scope.volume, None));
            assert!(live.is_installed(neighbor.volume, None));
            assert_eq!(
                live.availability(store.change_status(), store.pending_change_count()),
                "Synchronization failed; local data is retained"
            );
            // Consuming notification payloads must not erase terminal health.
            remote.received.drain();
            assert_eq!(
                live.availability(store.change_status(), store.pending_change_count()),
                "Synchronization failed; local data is retained"
            );
        }
    }

    #[test]
    fn readiness_recovery_stops_on_denied_storage_and_uncertain_errors() {
        use crate::engine::live::RecoveryKind;
        for message in [
            "access denied: volume permission",
            "disk full",
            "request timed out",
            "waiting for initial database synchronization: unexpected context",
        ] {
            let (_directory, mut engine, scope, neighbor) = gap_fixture();
            let store = engine.services.store.clone();
            let remote = engine.remote.as_mut().unwrap();
            let now = Instant::now();
            assert!(
                remote
                    .live
                    .as_mut()
                    .unwrap()
                    .defer_readiness(&scope, 0, now - Duration::from_millis(250))
                    .unwrap()
            );
            let mut captures = 0;
            let error = remote
                .recover_scope_with(&store, &scope, RecoveryKind::Gap, |_, _, _, _| {
                    captures += 1;
                    Err(message.into())
                })
                .unwrap_err();
            assert_eq!(error.to_string(), message);
            assert_eq!(captures, 1);
            let live = remote.live.as_ref().unwrap();
            assert!(live.readiness_wake_after(now).is_none());
            assert!(!live.needs_gap_recovery());
            assert!(live.is_installed(neighbor.volume, None));
        }
    }

    #[test]
    fn capture_retry_requires_a_current_gap_and_stops_on_success() {
        use crate::engine::live::{LiveGap, RecoveryKind, tests::observed_entry};
        use crate::engine::publisher::PublishState;
        use crate::engine::replication::{ReplicaSource, ReplicaState};
        let database = crate::store::DatabaseId::Entry(
            crate::store::EntryId::from_bytes([6; 15]).expect("valid fixture entry"),
        );
        for outcome in [
            "live",
            "revoked",
            "failed publisher",
            "stale epoch",
            "stale pending backing",
            "stale apply wait",
            "storage failed",
        ] {
            let (_directory, mut engine, scope, neighbor) = gap_fixture();
            let store = engine.services.store.clone();
            let remote = engine.remote.as_mut().unwrap();
            let mut captures = 0;
            let result = remote.recover_scope_with(
                &store,
                &scope,
                RecoveryKind::Gap,
                |remote, store, _, _| {
                    captures += 1;
                    let live = remote.live.as_mut().unwrap();
                    observed_entry(
                        live,
                        scope,
                        captures + 1,
                        [captures as u8 + 1; 16],
                        PublishState::Ready,
                        ReplicaState::Gap,
                    );
                    if captures == 1 {
                        return Err(super::sync_failure::volume(
                            store,
                            scope.volume,
                            LiveGap::Delivery,
                        ));
                    }
                    match outcome {
                        "live" => {
                            remote.received.test_state(
                                ReplicaSource::Volume(scope.volume),
                                [3; 16],
                                ReplicaState::Live,
                            );
                            live.complete_recovery(&scope)
                        }
                        "revoked" => {
                            remote.received.test_state(
                                ReplicaSource::Volume(scope.volume),
                                [3; 16],
                                ReplicaState::Revoked,
                            );
                            Err(LiveGap::Delivery.into())
                        }
                        "failed publisher" => {
                            live.publisher().test_state(scope.volume, 3, PublishState::Failed);
                            Err(LiveGap::Delivery.into())
                        }
                        "stale epoch" | "stale pending backing" | "stale apply wait" => {
                            let gap = live.complete_recovery(&scope).unwrap_err();
                            remote.received.test_state(
                                ReplicaSource::Volume(scope.volume),
                                [4; 16],
                                ReplicaState::Gap,
                            );
                            Err(match outcome {
                                "stale pending backing" => sync_failure::captured_backing_gap(
                                    scope.volume,
                                    Some(database),
                                    gap,
                                ),
                                "stale apply wait" => {
                                    sync_failure::apply_gap(sync_failure::target_database(
                                        scope.volume,
                                        database,
                                        "waiting for initial database synchronization",
                                    ))
                                }
                                _ => gap,
                            })
                        }
                        _ => Err("storage failed".into()),
                    }
                },
            );
            assert_eq!(captures, 2, "{outcome}");
            assert_eq!(result.is_ok(), outcome == "live");
            let live = remote.live.as_mut().unwrap();
            if matches!(outcome, "stale pending backing" | "stale apply wait") {
                let error = result.unwrap_err();
                assert!(!error.is::<initialization::Pending>());
                assert!(live.readiness_wake_after(Instant::now()).is_none());
                let gap = sync_failure::reason(error.as_ref()).downcast_ref::<LiveGap>().unwrap();
                if outcome == "stale pending backing" {
                    assert!(matches!(gap, LiveGap::PendingBacking));
                    assert!(error.to_string().contains(
                        "replica subscription requires gap recovery\n\
                         Also waiting for initial database synchronization"
                    ));
                } else {
                    assert!(matches!(gap, LiveGap::Apply(_)));
                }
            }
            assert!(!live.needs_gap_recovery());
            assert!(live.is_installed(neighbor.volume, None));
            if outcome == "live" {
                assert!(live.is_installed(scope.volume, None));
                assert!(!live.recovery_is_current(&scope, RecoveryKind::Gap));
            }
        }
    }

    fn receive_fixture()
    -> (tempfile::TempDir, LocalStore, Id, crate::store::volume_index::EntryAddress, Id, Access)
    {
        use crate::store::object_contract::{Schema, TypeDeclaration};
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.create_volume("Receive race", true).unwrap().id;
        let access = Access::owner(store.account().id);
        let type_id = Id::generate().unwrap();
        let entry = store
            .create_database(
                &access,
                volume,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb {
                    schema: Schema {
                        types: BTreeMap::from([(
                            type_id,
                            TypeDeclaration {
                                name: "Item".into(),
                                deprecated: false,
                                fields: BTreeMap::new(),
                            },
                        )]),
                    },
                },
            )
            .unwrap();
        let address = crate::store::volume_index::EntryAddress { volume, entry: entry.id };
        store.create_object_with_fields(&access, address, type_id, &BTreeMap::new()).unwrap();
        (directory, store, volume, address, type_id, access)
    }

    fn receive_snapshot(
        store: &LocalStore,
        access: &Access,
        volume: Id,
        database: crate::store::DatabaseId,
        target: &crate::store::Database,
        hosted: bool,
    ) -> Result<PullReceipt> {
        Ok(store.with_authorized_replica(
            access,
            volume,
            database,
            Some(Permission::Read),
            |source, policy| {
                crate::store::merge_replica(
                    &std::env::temp_dir(),
                    source,
                    target,
                    Lww,
                    |_| Ok(()),
                )?;
                if hosted && database == crate::store::DatabaseId::Index {
                    use crate::store::volume_index::namespace::{receipt, source::cut};
                    let snapshot = source.begin_read()?;
                    let accepted = cut::accepted(&snapshot)?;
                    let (namespace, evidence) = receipt::Receipt::new(None, &accepted)?;
                    let receipt = crate::store::ReplicaReceipt {
                        namespace,
                        source: store.source_snapshot_token(volume)?,
                        durable: DurableRevision {
                            revision: source.clock().as_u64(),
                            policy_revision: policy.volume.revision(),
                            root: snapshot.root_hash()?,
                        },
                    };
                    receipt::retain(target, &receipt, None, &evidence)?;
                }
                Ok(PullReceipt {
                    ap_source: hosted,
                    hosted,
                    source: store.source_snapshot_token(volume)?,
                    revision: DurableRevision {
                        revision: source.clock().as_u64(),
                        policy_revision: policy.volume.revision(),
                        root: source.begin_read()?.root_hash()?,
                    },
                })
            },
        )?)
    }

    #[test]
    fn final_ap_qualification_binds_the_staged_root_without_equating_source_clocks() {
        // A same-cut receipt must bind the actual filtered bytes. Its source
        // clock is not the projected receiver clock and cannot be compared.
        use crate::store::{DatabaseId, SnapshotCut};
        let (_directory, store, volume, _, _, access) = receive_fixture();
        let mut stage = store.prepare_received_volume(volume).unwrap();
        let initial = stage
            .with_database(DatabaseId::Index, |db| {
                receive_snapshot(&store, &access, volume, DatabaseId::Index, db, true)
                    .map_err(|error| crate::store::Error::System(error.to_string()))
            })
            .unwrap();
        let cut = SnapshotCut::new(initial.source, initial.revision.policy_revision).unwrap();
        for valid in [false, true] {
            let mut pull = |database, target: &crate::store::Database, _| {
                let mut receipt =
                    receive_snapshot(&store, &access, volume, database, target, true)?;
                if valid {
                    receipt.revision.revision = u64::MAX;
                } else {
                    let (_, hash) = receipt.revision.root.as_mut().unwrap();
                    hash[0] ^= 1;
                }
                Ok(Some(receipt))
            };
            assert_eq!(
                Remote::qualify_received_authority(&mut stage, cut, &mut pull).unwrap(),
                valid
            );
        }
    }

    #[test]
    fn bootstrap_complete_birth_needs_only_one_backing_snapshot() {
        use crate::store::DatabaseId;
        for hosted in [true, false] {
            let (_directory, store, volume, address, type_id, access) = receive_fixture();
            let created = store
                .create_object_with_fields(&access, address, type_id, &BTreeMap::new())
                .unwrap()
                .object_id;
            let mut calls = Vec::new();
            let received = Remote::receive_volume_with(&store, None, volume, |database, target| {
                calls.push(database);
                let receipt = receive_snapshot(&store, &access, volume, database, target, hosted)?;
                Ok(receipt)
            })
            .unwrap();
            let mut expected =
                vec![DatabaseId::Index, DatabaseId::Revocations, DatabaseId::Entry(address.entry)];
            if hosted {
                expected.push(DatabaseId::Index);
            }
            assert_eq!(calls, expected);
            received
                .with_database(DatabaseId::Entry(address.entry), |db| {
                    assert!(
                        crate::store::object_database::Snapshot::new(db.begin_read()?)
                            .read(created, None)
                            .is_ok()
                    );
                    Ok(())
                })
                .unwrap();
            for database in calls {
                assert_eq!(
                    received
                        .receipt_page(None)
                        .unwrap()
                        .receipts
                        .iter()
                        .any(|(id, _)| *id == database),
                    hosted,
                    "private receipts never acknowledge AP durability"
                );
            }
        }
    }

    // Mutate only a backing after index capture: policy clocks stay unchanged.
    // Hosted and private bootstraps discard the mixed attempt and retry once.
    #[test]
    fn bootstrap_retries_backing_edits_even_when_the_index_clock_is_unchanged() {
        use crate::store::DatabaseId;
        for hosted in [true, false] {
            let (_directory, store, volume, address, type_id, access) = receive_fixture();
            let mut created = None;
            let mut calls = Vec::new();
            let received = Remote::receive_volume_with(&store, None, volume, |database, target| {
                calls.push(database);
                let receipt = receive_snapshot(&store, &access, volume, database, target, hosted)?;
                if database == DatabaseId::Index && created.is_none() {
                    created = Some(
                        store
                            .create_object_with_fields(&access, address, type_id, &BTreeMap::new())?
                            .object_id,
                    );
                }
                Ok(receipt)
            })
            .unwrap();
            let mut expected = vec![
                DatabaseId::Index,
                DatabaseId::Revocations,
                DatabaseId::Index,
                DatabaseId::Revocations,
                DatabaseId::Entry(address.entry),
            ];
            if hosted {
                expected.push(DatabaseId::Index);
            }
            assert_eq!(calls, expected);
            received
                .with_database(DatabaseId::Entry(address.entry), |database| {
                    assert!(
                        crate::store::object_database::Snapshot::new(database.begin_read()?)
                            .read(created.unwrap(), None)
                            .is_ok()
                    );
                    Ok(())
                })
                .unwrap();
        }
    }

    // Change the already-copied backing before the next backing is captured.
    // A stable policy/index clock cannot admit those two different source cuts.
    #[test]
    fn bootstrap_retries_a_write_between_named_backing_snapshots() {
        use crate::store::{DatabaseId, volume_index::EntryKind};
        let (_directory, store, volume, address, type_id, access) = receive_fixture();
        let second = store
            .create_database(
                &access,
                volume,
                "second",
                EntryKind::ObjectDb { schema: store.schema(&access, address).unwrap() },
            )
            .unwrap();
        let mut captured = None;
        let mut created = None;
        let mut named_pulls = 0;
        let received = Remote::receive_volume_with(&store, None, volume, |database, target| {
            if let DatabaseId::Entry(entry) = database {
                named_pulls += 1;
                let copied = captured.get_or_insert(entry);
                if named_pulls == 2 {
                    created = Some(
                        store
                            .create_object_with_fields(
                                &access,
                                crate::store::volume_index::EntryAddress { volume, entry: *copied },
                                type_id,
                                &BTreeMap::new(),
                            )?
                            .object_id,
                    );
                }
            }
            receive_snapshot(&store, &access, volume, database, target, true)
        })
        .unwrap();
        assert_eq!(named_pulls, 4, "one mixed attempt and one stable attempt");
        assert!([address.entry, second.id].contains(&captured.unwrap()));
        received
            .with_database(DatabaseId::Entry(captured.unwrap()), |database| {
                assert!(
                    crate::store::object_database::Snapshot::new(database.begin_read()?)
                        .read(created.unwrap(), None)
                        .is_ok()
                );
                Ok(())
            })
            .unwrap();
    }

    // A replacement peer/fence cannot reuse epoch zero to qualify an old cut.
    // Keep changing source generations through every bounded attempt.
    #[test]
    fn bootstrap_rejects_recreated_source_generations_without_clock_changes() {
        let (_directory, store, volume, _, _, access) = receive_fixture();
        let mut calls = 0;
        let result = Remote::receive_volume_with(&store, None, volume, |database, target| {
            calls += 1;
            let mut receipt = receive_snapshot(&store, &access, volume, database, target, true)?;
            receipt.source.generation = Id::from_bytes([calls; 16]);
            Ok(receipt)
        });
        assert!(result.err().unwrap().to_string().contains("source kept changing"));
        assert_eq!(calls, 6);
    }

    #[test]
    fn interrupted_named_upload_uses_local_content_without_hosted_receipt() {
        use crate::store::DatabaseId;
        let (_directory, store, volume, address, _, access) = receive_fixture();
        let named = DatabaseId::Entry(address.entry);
        let received = Remote::receive_volume_with(&store, None, volume, |database, target| {
            if database == named {
                return Err("waiting for initial database synchronization".into());
            }
            receive_snapshot(&store, &access, volume, database, target, true)
        })
        .unwrap();
        received
            .with_database(named, |database| {
                assert!(database.begin_read()?.root_hash()?.is_some());
                Ok(())
            })
            .unwrap();
        // A different failure cannot take the missing-backing fallback. Name the
        // admitted local entry and keep its exact address through any outer volume pass.
        let cause = Remote::receive_volume_with(&store, None, volume, |database, target| {
            if database == named {
                return Err("database transfer failed".into());
            }
            receive_snapshot(&store, &access, volume, database, target, true)
        })
        .err()
        .unwrap();
        let cause = sync_failure::volume(&store, volume, cause);
        assert_eq!(
            cause.to_string(),
            format!(
                "Couldn’t sync database \"data\" ({named}) in volume \"Receive race\" ({volume})\ndatabase transfer failed"
            ),
        );
        let page = received.receipt_page(None).unwrap();
        let receipts = page.receipts;
        let next = page.next;
        assert!(next.is_none());
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().all(|(database, _)| *database != named));
        assert!(
            received
                .with_database(DatabaseId::Index, |index| {
                    // This is a detached stage, before namespace adoption.
                    // Check copied backing readiness without claiming app access.
                    crate::store::volume_index::IndexSnapshot::new(index.begin_read()?)
                        .source_backing_ready(address.entry)
                })
                .unwrap()
        );
    }

    #[test]
    fn retirement_repair_only_requests_backings_missing_from_ap() {
        // Freeze the hosted replicas before deleting both entries offline. Only
        // an actual missing-backing response may trigger a scoped retirement.
        use crate::store::{DatabaseId, volume_index::EntryKind};
        let (_directory, store, volume, first, _, access) = receive_fixture();
        let second = store.create_database(&access, volume, "scratch", EntryKind::KvDb {}).unwrap();
        let ap = Remote::receive_volume_with(&store, None, volume, |database, target| {
            receive_snapshot(&store, &access, volume, database, target, true)
        })
        .unwrap();
        let receipts = ap.receipt_page(None).unwrap().receipts;
        let source = store.source_snapshot_token(volume).unwrap();
        for entry in [first.entry, second.id] {
            store.remove_entry(&access, volume, entry).unwrap();
        }

        for (missing, can_retire, partial) in
            [(false, true, false), (true, true, false), (true, false, false), (true, true, true)]
        {
            let blocked = first.entry.min(second.id);
            let last = first.entry.max(second.id);
            let mut waited = false;
            let mut repairs = Vec::new();
            let received = Remote::receive_volume_using(
                &store,
                None,
                volume,
                None,
                |database, target, _| {
                    if missing
                        && (database == DatabaseId::Entry(blocked)
                            || partial && database == DatabaseId::Entry(last))
                    {
                        waited = true;
                        return Err("waiting for initial database synchronization".into());
                    }
                    assert!(!waited || can_retire, "an unrepairable missing entry ends the pull");
                    ap.with_database(database, |db| {
                        crate::store::merge_replica(&std::env::temp_dir(), db, target, Lww, |_| {
                            Ok(())
                        })
                    })?;
                    let (_, revision) = receipts.iter().find(|(id, _)| *id == database).unwrap();
                    Ok(Some(PullReceipt {
                        source,
                        revision: revision.clone(),
                        hosted: true,
                        ap_source: true,
                    }))
                },
                |_, repair| match repair {
                    ControlRepair::CheckRetirement(_) => Ok(can_retire),
                    ControlRepair::MissingBackings { entries, first_unrepaired } => {
                        repairs.extend_from_slice(entries);
                        if partial {
                            *first_unrepaired = last;
                        }
                        Ok(false)
                    }
                    ControlRepair::ServiceRead => Ok(false),
                },
            );
            if missing {
                assert_eq!(
                    repairs,
                    if partial {
                        vec![blocked, last]
                    } else if can_retire {
                        vec![blocked]
                    } else {
                        vec![]
                    }
                );
                let blocked = if partial { last } else { blocked };
                let error = received.err().unwrap();
                let waiting = "waiting for initial database synchronization";
                assert_eq!(super::sync_failure::reason(error.as_ref()).to_string(), waiting);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "Couldn’t sync database {blocked} in volume \"Receive race\" ({volume})\n{waiting}"
                    )
                );
            } else {
                received.unwrap();
                assert!(repairs.is_empty());
            }
        }
    }

    // Use an unchanged remote cut, mutate the first retained local backing while
    // copying the second, and require both cross-copy and final local checks.
    #[test]
    fn interrupted_upload_fallbacks_require_one_current_local_cut() {
        use crate::store::{DatabaseId, SnapshotToken, volume_index::EntryKind};
        for all_fallbacks in [true, false] {
            let (_directory, store, volume, address, type_id, access) = receive_fixture();
            store
                .create_database(
                    &access,
                    volume,
                    "second",
                    EntryKind::ObjectDb { schema: store.schema(&access, address).unwrap() },
                )
                .unwrap();
            let mut first = None;
            let mut created = None;
            let mut named_pulls = 0;
            let received = Remote::receive_volume_with(&store, None, volume, |database, target| {
                if let DatabaseId::Entry(entry) = database {
                    named_pulls += 1;
                    let copied = *first.get_or_insert(entry);
                    if named_pulls == 2 {
                        created = Some(
                            store
                                .create_object_with_fields(
                                    &access,
                                    crate::store::volume_index::EntryAddress {
                                        volume,
                                        entry: copied,
                                    },
                                    type_id,
                                    &BTreeMap::new(),
                                )?
                                .object_id,
                        );
                    }
                    if all_fallbacks || entry == copied {
                        return Err("waiting for initial database synchronization".into());
                    }
                }
                let mut receipt =
                    receive_snapshot(&store, &access, volume, database, target, true)?;
                // This fixture models an independent, unchanged AP source.
                receipt.source = SnapshotToken { generation: Id::from_bytes([9; 16]), epoch: 0 };
                Ok(receipt)
            })
            .unwrap();
            assert_eq!(named_pulls, 4, "one mixed attempt followed by one stable attempt");
            received
                .with_database(DatabaseId::Entry(first.unwrap()), |database| {
                    assert!(
                        crate::store::object_database::Snapshot::new(database.begin_read()?)
                            .read(created.unwrap(), None)
                            .is_ok()
                    );
                    Ok(())
                })
                .unwrap();
            assert!(
                received
                    .receipt_page(None)
                    .unwrap()
                    .receipts
                    .iter()
                    .all(|(database, _)| { *database != DatabaseId::Entry(first.unwrap()) })
            );
        }
    }

    #[test]
    fn local_bootstrap_rejects_missing_readiness_changed_birth_and_read_contraction() {
        use crate::store::{
            DatabaseId,
            object_contract::{Acl, EntryAcl},
            volume_index,
        };
        for case in 0..3 {
            let (_directory, store, volume, address, _, access) = receive_fixture();
            let stage = store.prepare_received_volume(volume).unwrap();
            stage
                .with_database(DatabaseId::Index, |target| {
                    receive_snapshot(&store, &access, volume, DatabaseId::Index, target, true)
                        .map(|_| ())
                        .map_err(|error| crate::store::Error::System(error.to_string()))
                })
                .unwrap();
            match case {
                0 => store
                    .with_replica(volume, DatabaseId::Index, |index| {
                        let mut key = vec![volume_index::META_BYTE, 6];
                        key.extend_from_slice(address.entry.as_bytes());
                        let mut tx = index.begin_write(now())?;
                        tx.local_indexes().del(&key)?;
                        tx.commit()?;
                        Ok(())
                    })
                    .unwrap(),
                1 => stage
                    .with_database(DatabaseId::Index, |index| {
                        let key = volume_index::entry_key(address.entry);
                        let bytes = index
                            .begin_read()?
                            .get(&key)?
                            .ok_or(crate::store::Error::corrupt(
                                "replica fixture value is missing",
                            ))?
                            .to_vec();
                        let mut tx = index.begin_write(now())?;
                        tx.put(&key, &bytes)?; // Same bytes, a different immutable origin clock.
                        tx.commit()?;
                        Ok(())
                    })
                    .unwrap(),
                2 => store
                    .set_entry_permissions(
                        &access,
                        volume,
                        address.entry,
                        EntryAcl::Replace(Acl::default()),
                    )
                    .unwrap(),
                _ => unreachable!(),
            }
            assert!(
                store
                    .stage_local_backing(&access, &stage, DatabaseId::Entry(address.entry))
                    .is_err()
            );
            assert!(stage.receipt_page(None).unwrap().receipts.is_empty());
        }
    }

    #[test]
    fn bootstrap_append_logs_require_the_same_cut_across_every_page() {
        use crate::store::{
            DatabaseId, MetadataPolicy, SnapshotCut, bloblog::Log, volume_index::DatabaseProtocol,
        };
        // Inject one append before the first page, between pages, or on every
        // attempt. Rejected detached pages must never qualify a mixed volume.
        for mode in 0..3 {
            let (_directory, store, volume, _address, _, access) = receive_fixture();
            let log = store.create_bloblog(&access, volume, "edits").unwrap().id;
            for batch in 0..2u8 {
                let entries: Vec<_> =
                    (0..35).map(|number| vec![batch * 35 + number; 1024]).collect();
                store.bloblog_put(&access, volume, log, &entries).unwrap();
            }
            let mut attempts = 0;
            let mut injected = false;
            let result = Remote::receive_volume_using(
                &store,
                None,
                volume,
                None,
                |database, target, request| {
                    if let ReceivedPull::Backing { cut, protocol: DatabaseProtocol::BlobLog } =
                        request
                    {
                        attempts += 1;
                        let mut cursor = None;
                        loop {
                            if (mode == 0 && !injected)
                                || (mode == 1 && cursor.is_some() && !injected)
                                || (mode == 2 && cursor.is_none())
                            {
                                store.bloblog_put(
                                    &access,
                                    volume,
                                    log,
                                    &[b"concurrent".to_vec()],
                                )?;
                                injected = true;
                            }
                            let mut page =
                                store.bloblog_get(&access, volume, log, cursor.as_ref())?;
                            let (source, policy_revision) =
                                store.with_replica(volume, DatabaseId::Index, |index| {
                                    Ok((
                                        store.source_snapshot_token(volume)?,
                                        MetadataPolicy::read(index)?.revision(),
                                    ))
                                })?;
                            SnapshotCut::new(source, policy_revision)?;
                            if !cut.accepts(source, policy_revision) {
                                return Ok(None);
                            }
                            // Duplicate/reordered delivery retains each append exactly once.
                            page.entries.reverse();
                            if let Some(first) = page.entries.first().cloned()
                                && crate::store::bloblog::encode(&page.entries)?.len()
                                    + 59
                                    + first.data.len()
                                    <= crate::store::bloblog::MAX_BYTES
                            {
                                page.entries.push(first);
                            }
                            Log {
                                database: target,
                                volume,
                                entry: log,
                                encryption_key: None,
                                now: now(),
                            }
                            .apply_page(&page)?;
                            if !page.more {
                                return Ok(Some(PullReceipt {
                                    ap_source: false,
                                    source,
                                    revision: DurableRevision {
                                        revision: target.clock().as_u64(),
                                        policy_revision,
                                        root: None,
                                    },
                                    hosted: false,
                                }));
                            }
                            cursor = Some(page.cursor);
                        }
                    }
                    receive_snapshot(&store, &access, volume, database, target, true).map(Some)
                },
                |_, _| Ok(false),
            );
            if mode == 2 {
                assert_eq!(attempts, 3);
                assert!(result.err().unwrap().to_string().contains("source kept changing"));
            } else {
                let stage = result.unwrap();
                assert_eq!(attempts, 2);
                stage
                    .with_database(DatabaseId::Entry(log), |database| {
                        let log =
                            Log { database, volume, entry: log, encryption_key: None, now: now() };
                        log.validate()?;
                        let first = log.get(None)?;
                        assert!(first.more);
                        let second = log.get(Some(&first.cursor))?;
                        assert!(!second.more);
                        assert_eq!(first.entries.len() + second.entries.len(), 71);
                        Ok(())
                    })
                    .unwrap();
                assert!(
                    stage
                        .receipt_page(None)
                        .unwrap()
                        .receipts
                        .iter()
                        .all(|(database, _)| *database != DatabaseId::Entry(log))
                );
            }
        }
    }

    #[test]
    fn bootstrap_policy_churn_is_bounded_and_does_not_adopt_partial_data() {
        let (_directory, store, volume, address, _type_id, access) = receive_fixture();
        let mut calls = 0;
        let result = Remote::receive_volume_with(&store, None, volume, |database, target| {
            calls += 1;
            let receipt = receive_snapshot(&store, &access, volume, database, target, true)?;
            if database == crate::store::DatabaseId::Index {
                store.rename_entry(&access, volume, address.entry, &format!("data-{calls}"))?;
            }
            Ok(receipt)
        });
        assert!(result.err().unwrap().to_string().contains("source kept changing"));
        assert_eq!(calls, 6);
        assert!(
            store
                .durable_revision(volume, crate::store::DatabaseId::Entry(address.entry))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_transferred_database_names_its_backing_when_local_validation_fails() {
        let (_directory, store, volume, address, _, access) = receive_fixture();
        let named = crate::store::DatabaseId::Entry(address.entry);
        let result = Remote::receive_volume_with(&store, None, volume, |database, target| {
            let receipt = receive_snapshot(&store, &access, volume, database, target, true)?;
            if database == named {
                let mut write = target.begin_write(now())?;
                write.put(b"object/x", b"malformed creation")?;
                write.commit()?;
            }
            Ok(receipt)
        });
        let error = result.err().expect("invalid transferred backing must not be adopted");
        let message = error.to_string();
        let (scope, cause) = sync_failure_parts(&message).expect("database scope");
        assert_eq!(
            scope,
            format!(
                "Couldn’t sync database \"data\" ({named}) in volume \"Receive race\" ({volume})"
            )
        );
        assert!(cause.contains("object creation key has the wrong ID width"), "{cause}");
    }

    #[test]
    fn bootstrap_does_not_retry_an_authorization_or_storage_error() {
        let (_directory, store, volume, _address, _type_id, _access) = receive_fixture();
        for message in ["access denied", "storage failed"] {
            let mut calls = 0;
            let result = Remote::receive_volume_with(&store, None, volume, |database, _| {
                assert_eq!(database, crate::store::DatabaseId::Index);
                calls += 1;
                Err(message.into())
            });
            assert_eq!(
                result.err().unwrap().to_string(),
                format!(
                    "Couldn’t sync database index in volume \"Receive race\" ({volume})\n{message}"
                ),
            );
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn private_session_log_bootstrap_requires_an_explicit_local_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Private bloblog bootstrap", false).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        assert!(
            !engine.services.store.volume(session.replica_volume()).unwrap().share_key_with_service
        );
        let access = Access::owner(engine.services.store.account().id);
        let remote = engine.remote.as_ref().unwrap();
        // A private root cannot grant AP session admission. Return the explicit
        // fallback condition instead of silently omitting a named log.
        {
            let volume = session.replica_volume();
            let store = &engine.services.store;
            let log = store.create_bloblog(&access, volume, "edits").unwrap().id;
            let stage = store.prepare_received_volume(volume).unwrap();
            let receipt = stage
                .with_database(crate::store::DatabaseId::Index, |target| {
                    receive_snapshot(
                        store,
                        &access,
                        volume,
                        crate::store::DatabaseId::Index,
                        target,
                        false,
                    )
                    .map_err(|error| crate::store::Error::System(error.to_string()))
                })
                .unwrap();
            let cut =
                crate::store::SnapshotCut::new(receipt.source, receipt.revision.policy_revision)
                    .unwrap();
            let error = stage
                .with_database(crate::store::DatabaseId::Entry(log), |target| {
                    remote
                        .pull_received_log(store, Some(&session), volume, log, target, cut)
                        .map_err(|error| crate::store::Error::System(error.to_string()))
                })
                .err()
                .unwrap();
            assert_eq!(error.to_string(), "waiting for initial database synchronization");
            store
                .stage_local_backing(&access, &stage, crate::store::DatabaseId::Entry(log))
                .unwrap();
            stage.finish_database(crate::store::DatabaseId::Entry(log)).unwrap();
            if volume == session.replica_volume() {
                let error = stage
                    .with_database(crate::store::DatabaseId::Entry(log), |target| {
                        remote
                            .pull_received_log(store, None, volume, log, target, cut)
                            .map_err(|error| crate::store::Error::System(error.to_string()))
                    })
                    .err()
                    .unwrap();
                assert_eq!(error.to_string(), "waiting for initial database synchronization");
            }
        }
    }

    #[test]
    fn deleted_watch_does_not_fail_another_sessions_initialization() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let deleted = store.create_volume("Deleted backing", false).unwrap();
        let retained = store.create_volume("Retained backing", false).unwrap();
        let deleted_session =
            engine.create_session(crate::engine::tests::button_package(), deleted.id).unwrap();
        let retained_session =
            engine.create_session(crate::engine::tests::button_package(), retained.id).unwrap();
        let remote = engine.remote.as_mut().unwrap();
        remote.selection_watches.insert(deleted_session.id.clone());
        remote.selection_watches.insert(retained_session.id.clone());
        store.delete_volume(deleted.id).unwrap();
        remote.rearm_selection_watches(&store, retained.id).unwrap();
        assert!(!remote.selection_watches.contains(&deleted_session.id));
        assert!(!remote.selection_watches.contains(&retained_session.id));
        assert!(store.session(&retained_session.id).is_ok());
    }

    #[test]
    fn unchanged_selection_preserves_observer_and_actual_adoption_retires_it() {
        use crate::engine::{Engine, updates::PreparedUpdate};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Stable observer", false).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        engine.remote.as_mut().unwrap().selection_watches.insert(session.id.clone());
        engine.finish_update(
            &session.id,
            Ok(PreparedUpdate {
                candidate: session.clone(),
                prior: session.clone(),
                grants: vec![],
            }),
        );
        assert!(
            engine.remote.as_ref().unwrap().selection_watches.contains(&session.id),
            "a no-op metadata inspection must not replace its presence/message observer"
        );
        assert!(engine.updates.is_empty());
        let mut candidate = session.clone();
        candidate.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
        candidate.selector =
            crate::store::BuildSelector::Exact { build_id: candidate.resolved_build_id.clone() };
        engine
            .services
            .store
            .provision_version(
                &crate::store::Access::owner(engine.services.store.account().id),
                root.id,
                &candidate.resolved_build_id,
            )
            .unwrap();
        engine.finish_update(
            &session.id,
            Ok(PreparedUpdate {
                candidate: candidate.clone(),
                prior: session.clone(),
                grants: vec![],
            }),
        );
        assert!(engine.updates.is_empty(), "validated candidate metadata must adopt");
        assert_eq!(engine.services.store.session(&session.id).unwrap(), candidate);
        assert!(!engine.remote.as_ref().unwrap().selection_watches.contains(&session.id));
    }

    #[test]
    fn all_admitted_apps_receive_updates_and_retired_observers_release_capacity() {
        exercise_observer_capacity(false);
    }

    #[test]
    #[ignore = "reference-device observer benchmark"]
    fn benchmark_observer_inspection_at_capacity() {
        exercise_observer_capacity(true);
    }

    fn exercise_observer_capacity(measure: bool) {
        use crate::network::ap::{Frame, frame_type, testing};
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Concurrent sessions", false).unwrap();
        let sessions: Vec<_> = (0..=SESSION_OBSERVER_LIMIT)
            .map(|_| {
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap()
            })
            .collect();
        let store = engine.services.store.clone();
        let (mut receive, completion) = UnixStream::pair().unwrap();
        receive.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let (runtime, endpoint, config) = testing::loopback_ap();
        let server = std::thread::spawn(move || {
            runtime.block_on(async move {
                for round in 0..2 {
                    // Each connection round has a deadlock watchdog. This is
                    // separate from the optimized observer-inspection budget.
                    tokio::time::timeout(Duration::from_secs(15), async {
                        let connection = endpoint.accept().await.unwrap().await.unwrap();
                        let (mut auth, auth_read) = connection.accept_bi().await.unwrap();
                        auth.write_all(&testing::authentication_wire(
                            Frame::new(frame_type::HELLO, 0, [], [], []).unwrap(),
                        ))
                        .await
                        .unwrap();
                        let mut streams = vec![(auth, auth_read)];
                        for index in 0..=SESSION_OBSERVER_LIMIT {
                            let (mut send, read, frame) =
                                testing::next_request_with_receipt(&connection).await.unwrap();
                            assert_eq!(frame.frame_type, frame_type::SUBSCRIBE);
                            assert_eq!(frame.destination.len(), 32);
                            let denied = round == 1 && index == SESSION_OBSERVER_LIMIT;
                            if denied {
                                send.finish().unwrap();
                                streams.push((send, read));
                                continue;
                            }
                            send.write_all(
                                &Frame::new(
                                    frame_type::SUBSCRIBE,
                                    0,
                                    [],
                                    frame.destination.clone(),
                                    [],
                                )
                                .unwrap()
                                .encode()
                                .unwrap(),
                            )
                            .await
                            .unwrap();
                            let presence =
                                serde_json::to_vec(&crate::session_channel::Control::Presence {
                                    participants: vec![],
                                })
                                .unwrap();
                            let update = serde_json::to_vec(
                                &crate::session_channel::Control::UpdateAvailable,
                            )
                            .unwrap();
                            let mut bodies =
                                vec![presence; crate::engine::session_channels::EVENT_LIMIT + 1];
                            bodies.extend([update.clone(), update]);
                            for body in bodies {
                                send.write_all(
                                    &Frame::new(
                                        frame_type::PUBLISH,
                                        0,
                                        [],
                                        frame.destination.clone(),
                                        body,
                                    )
                                    .unwrap()
                                    .encode()
                                    .unwrap(),
                                )
                                .await
                                .unwrap();
                            }
                            if round == 1 && index == 0 {
                                send.finish().unwrap();
                            }
                            streams.push((send, read));
                        }
                        connection.closed().await;
                        drop(streams);
                    })
                    .await
                    .unwrap();
                }
            })
        });
        remote.set_test_transport(config);
        remote.identity = preissued_identity(&store);
        let active: Vec<_> =
            sessions[..SESSION_OBSERVER_LIMIT].iter().map(|s| s.id.clone()).collect();
        for round in 0..2 {
            for session in &active {
                remote.listen_session(&store, session).unwrap();
                remote.selection_watches.insert(session.clone());
            }
            while remote.selection_updates(&active).len() < active.len() {
                assert!(receive.read(&mut [0; 64]).unwrap() > 0);
            }
            if measure {
                let mut samples = Vec::with_capacity(1000);
                for _ in 0..1000 {
                    let start = Instant::now();
                    std::hint::black_box(remote.selection_updates(&active));
                    samples.push(start.elapsed().as_secs_f64() * 1_000_000.);
                }
                samples.sort_by(f64::total_cmp);
                eprintln!(
                    "observer inspection: round={round} sessions={} n=1000 p50={:.3} p99={:.3} max={:.3} µs",
                    active.len(),
                    samples[499],
                    samples[989],
                    samples[999]
                );
            }
            if round == 1 {
                while remote.traffic[&active[0]].take_update() & 2 == 0 {
                    assert!(receive.read(&mut [0; 64]).unwrap() > 0);
                }
            }
            assert_eq!(remote.traffic.len(), SESSION_OBSERVER_LIMIT);
            let extra = &sessions[SESSION_OBSERVER_LIMIT].id;
            assert!(remote.listen_session(&store, extra).is_err());
            remote.retire_selection_watches(&active[1..]);
            if round == 0 {
                remote.listen_session(&store, extra).unwrap();
                assert_eq!(remote.traffic.len(), SESSION_OBSERVER_LIMIT);
                while !remote.traffic[extra].has_update() {
                    assert!(receive.read(&mut [0; 64]).unwrap() > 0);
                }
            } else {
                assert!(remote.listen_session(&store, extra).is_err());
                assert!(!remote.traffic.contains_key(extra));
                assert_eq!(remote.traffic.len(), SESSION_OBSERVER_LIMIT - 1);
            }
            assert!(!remote.traffic.contains_key(&active[0]));
            remote.disconnect();
            assert!(remote.traffic.is_empty());
        }
        server.join().unwrap();
    }

    #[test]
    fn embedding_selection_watch_follows_explicit_session_ownership() {
        use crate::engine::Engine;
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Observer lifetime", false).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        engine.embedded_sessions = vec![session.id.clone(), session.id.clone()];
        assert_eq!(engine.active_sessions(), vec![session.id.clone()]);
        engine.remote.as_mut().unwrap().selection_watches.insert(session.id.clone());
        engine.start_network_work();
        assert!(engine.remote.as_ref().unwrap().selection_watches.contains(&session.id));
        engine.embedded_sessions.clear();
        engine.start_network_work();
        assert!(!engine.remote.as_ref().unwrap().selection_watches.contains(&session.id));
    }

    #[test]
    fn network_handles_share_resources_and_retain_runtime_after_remote_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let remote =
            super::Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let handles = remote.network_handles();
        let another = remote.network_handles();
        assert!(std::sync::Arc::ptr_eq(&handles.runtime, &another.runtime));
        assert!(std::sync::Arc::ptr_eq(&handles.received, &another.received));
        assert!(std::sync::Arc::ptr_eq(&handles.publisher, &another.publisher));
        assert!(std::sync::Arc::ptr_eq(&handles.connection_status, &another.connection_status));
        remote.begin_connection();
        let weak = std::sync::Arc::downgrade(&handles.runtime);
        drop(remote);
        assert_eq!(handles.connection_status.snapshot().state, ApConnectionState::Offline);
        let job = handles.runtime.spawn(async { 7 });
        assert_eq!(handles.runtime.block_on(job).unwrap(), 7);
        drop(another);
        drop(handles);
        assert!(weak.upgrade().is_none());
    }

    use super::*;

    /// An already enrolled identity, so a test connects the way a signed-in
    /// device does instead of registering.
    fn preissued_identity(store: &LocalStore) -> ClientIdentity {
        let account = store.account().id;
        let mut identity = ClientIdentity::from_seed(
            "close@disconnect.invalid",
            "Test",
            store.account_seed().unwrap(),
        )
        .unwrap();
        identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
        identity.instance_id = crate::store::Id::from_bytes([1; 16]).to_string().into_bytes();
        identity.ca_certificate = vec![1];
        identity.instance_certificate = vec![1];
        identity
    }
    /// Connects `remote` for real (real QUIC, real AP authentication) to a
    /// fake AP, and returns a channel reporting whether that AP later observed
    /// this connection's close.
    fn connect_to_fake_ap(
        remote: &mut Remote,
        store: &LocalStore,
    ) -> std::sync::mpsc::Receiver<bool> {
        let identity = preissued_identity(store);
        let ap = crate::network::ap::testing::silent_authenticating_server();
        remote.set_test_transport(ap.config);
        remote.identity = identity;
        remote.authenticate(store).unwrap();
        assert!(
            remote.authenticated_transport().is_some(),
            "test setup must reach a real connected state"
        );
        assert_eq!(remote.connection_status.snapshot().state, ApConnectionState::Live);
        assert_eq!(remote.connection_status.snapshot().hostname, "localhost");
        ap.closed
    }

    #[test]
    fn unanswered_invitation_discovery_leaves_the_replica_worker_available() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let store = Arc::new(LocalStore::open(directory.path().join("store")).unwrap());
        let ap = crate::network::ap::testing::silent_authenticating_server();
        remote.set_test_transport(ap.config);
        remote.identity = preissued_identity(&store);
        remote.authenticate(&store).unwrap();
        remote.connection = remote.authenticated_transport();
        remote.ensure_live(&store).unwrap();
        store
            .save_recovery_state(&crate::store::RecoveryState {
                secret: [7; 16],
                acknowledged: true,
                revision: 1,
                synchronized_digest: None,
                pending: None,
            })
            .unwrap();

        remote.start_volume_invitations(&store).unwrap();
        ap.request_held.recv_timeout(Duration::from_secs(5)).unwrap();
        remote.refresh_availability(&store).unwrap();
        assert!(remote.availability_checked);
    }

    #[test]
    fn canceling_a_held_vault_read_keeps_the_issued_identity() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let store = LocalStore::open(directory.path().join("store")).unwrap();
        let ap = crate::network::ap::testing::silent_authenticating_server();
        remote.set_test_transport(ap.config);
        remote.identity = preissued_identity(&store);
        let token = remote.identity.auth_token.clone();
        remote.authenticate(&store).unwrap();
        let canceled = Arc::new(crate::engine::cancellation::Canceled::default());
        remote.set_cancellation(Some(canceled.clone()));
        let cancel = std::thread::spawn(move || {
            ap.request_held.recv_timeout(Duration::from_secs(5)).unwrap();
            canceled.raise();
        });
        let result = remote.credential_vault(&store);
        assert_eq!(result.err().unwrap().to_string(), super::CANCELED);
        cancel.join().unwrap();
        assert_eq!(remote.identity.auth_token, token);
        assert!(remote.enrolled());
        assert!(store.recovery_state().unwrap().is_none());
    }

    /// A connected `Remote` and its AP's close observer. `directory` must
    /// outlive the returned `Remote`.
    fn connected_remote_and_observer(
        directory: &tempfile::TempDir,
    ) -> (Remote, std::sync::mpsc::Receiver<bool>) {
        let (_receive, completion) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let store = LocalStore::open(directory.path().join("store")).unwrap();
        let observed = connect_to_fake_ap(&mut remote, &store);
        (remote, observed)
    }

    /// The same, for a whole `Engine`, so a test can issue owner requests the
    /// way the iOS bridge does. `directory` must outlive the returned `Engine`.
    fn connected_engine_and_observer(
        directory: &tempfile::TempDir,
    ) -> (crate::engine::Engine, std::sync::mpsc::Receiver<bool>) {
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let observed = connect_to_fake_ap(engine.remote.as_mut().unwrap(), &store);
        (engine, observed)
    }

    /// Reproduces the scenario behind the AP subscription-capacity fix in
    /// archipelago-service PR #22/#23: a device disconnects and its process
    /// exits (here, the `Remote` is simply dropped) while the AP never
    /// acknowledges anything. `Drop` must still get the CONNECTION_CLOSE
    /// frame out so the AP frees this device's slots promptly instead of
    /// only at its own idle timeout.
    #[test]
    fn dropped_remote_with_an_open_connection_flushes_its_close_to_the_ap() {
        let directory = tempfile::tempdir().unwrap();
        let (remote, observed) = connected_remote_and_observer(&directory);
        drop(remote);
        assert!(
            observed.recv_timeout(Duration::from_secs(5)).unwrap(),
            "a dropped Remote with an open connection must still flush its close"
        );
    }

    /// The exact shape of archipelago-service's
    /// `account_subscription_capacity_retains_local_edits_and_recovers_after_reconnect`:
    /// `second.request(Request::Disconnect); drop(second);`. Holding the close
    /// flush shows the ordering `disconnect` promises: it returns, and the
    /// runtime it shares stays usable, while the flush cannot finish; the
    /// connection is nevertheless already closed for every shared handle. The
    /// flush is then released and `Drop` must still get the close out instead
    /// of abandoning the spawned task along with the runtime, or the AP only
    /// frees this device's subscription slots at its own idle timeout.
    #[test]
    fn explicit_disconnect_then_immediate_drop_still_flushes_its_close() {
        let directory = tempfile::tempdir().unwrap();
        let (mut remote, observed) = connected_remote_and_observer(&directory);
        let shared = remote.authenticated_transport().unwrap();
        let held = crate::network::ap::testing::hold_close_flush(&shared);

        remote.disconnect();
        assert!(shared.is_closed(), "disconnect must close locally before it returns");
        assert!(
            remote.runtime.block_on(shared.open_stream()).is_err(),
            "a shared handle cannot admit a new operation after disconnect"
        );
        assert!(remote.authenticated_transport().is_none());
        assert!(
            !remote.pending_drain.as_ref().unwrap().is_finished(),
            "the held flush cannot have finished"
        );
        let job = remote.runtime.spawn(async { 7 });
        assert_eq!(
            remote.runtime.block_on(job).unwrap(),
            7,
            "the network runtime must stay usable"
        );

        // Released before the drop, so the bounded join in `Drop` is what has
        // to get the queued close out before the runtime disappears.
        drop(held);
        drop(shared);
        drop(remote);
        assert!(
            observed.recv_timeout(Duration::from_secs(5)).expect("the AP must report"),
            "dropping Remote right after an explicit disconnect must still flush the close"
        );
    }

    /// The connected-idle half of iOS suspend: `pb_ios_suspend` cancels any
    /// transfer and, when none is canceling, dispatches `Request::Disconnect`
    /// while it holds the bridge's mutex. Unlike the in-flight-transfer case
    /// (covered in archipelago-ios), `Engine` still owns `Remote` here, so that
    /// request reaches `Remote::disconnect` directly. It must return, and
    /// leave the owner free to serve further requests, while the close flush
    /// cannot finish; the close still reaches the AP once the flush runs.
    #[test]
    fn connected_idle_suspend_disconnects_without_waiting_for_the_close_flush() {
        use crate::engine::Request;
        let directory = tempfile::tempdir().unwrap();
        let (mut engine, observed) = connected_engine_and_observer(&directory);
        let account = engine.services.store.account().id.to_string();
        let shared = engine.remote.as_ref().unwrap().authenticated_transport().unwrap();
        let held = crate::network::ap::testing::hold_close_flush(&shared);

        let transfer =
            serde_json::to_value(engine.dispatch_reply(Request::CancelTransfer).unwrap()).unwrap();
        assert_ne!(transfer["transfer"]["kind"], "canceling", "this is the idle path");
        assert_eq!(
            serde_json::to_value(engine.dispatch_reply(Request::Disconnect).unwrap()).unwrap(),
            serde_json::json!({"disconnected":true})
        );

        assert_eq!(engine.network.connection_status.snapshot().state, ApConnectionState::Offline);
        let remote = engine.remote.as_ref().unwrap();
        assert!(shared.is_closed(), "suspend must close locally before it returns");
        assert!(
            remote.runtime.block_on(shared.open_stream()).is_err(),
            "a shared handle cannot admit a new operation after suspend"
        );
        assert!(remote.authenticated_transport().is_none());
        assert!(
            !remote.pending_drain.as_ref().unwrap().is_finished(),
            "the held flush cannot have finished"
        );
        assert_eq!(
            serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap()["account"]
                ["id"],
            account,
            "the owner must keep serving requests while the flush is held"
        );

        drop(held);
        drop(shared);
        drop(engine);
        assert!(
            observed.recv_timeout(Duration::from_secs(5)).expect("the AP must report"),
            "a suspended idle device must still flush its close"
        );
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_explicit_disconnect_removes_its_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let now = Instant::now();
        assert!(!remote.reconnect_ready(now));
        assert_eq!(remote.reconnect_after(now), None);
        let mut retry = Reconnect::new(vec![]);
        for millis in [250, 500, 1000, 2000, 4000, 8000, 16000, 30000, 30000] {
            retry.failed(now);
            assert_eq!(retry.after.duration_since(now), Duration::from_millis(millis));
        }
        remote.reconnect = Some(retry);
        assert!(!remote.reconnect_ready(now));
        assert!(remote.reconnect_ready(now + Duration::from_secs(30)));
        remote.reconnect.as_mut().unwrap().bootstrap = true;
        assert!(!remote.reconnect_ready(now + Duration::from_secs(30)));
        assert!(!remote.bootstrap_retry_ready(now));
        assert!(remote.bootstrap_retry_ready(now + Duration::from_secs(30)));
        remote
            .initializations
            .failed(Id::from_bytes([1; 16]), "waiting for initial volume synchronization", now)
            .unwrap();
        assert!(remote.initializations.wake_after(now).is_some());
        remote.disconnect();
        assert!(remote.initializations.wake_after(now).is_none());
        assert_eq!(remote.reconnect_after(now), None);
        assert!(!remote.reconnect_ready(now + Duration::from_secs(60)));
        assert!(!remote.bootstrap_retry_ready(now + Duration::from_secs(60)));
        assert!(
            std::mem::size_of::<Reconnect>()
                - std::mem::size_of::<Vec<crate::engine::live::ScopeKey>>()
                <= 64
        );
    }

    #[test]
    fn required_uninstalled_replica_cannot_disappear_from_fence_targets() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        store.create_volume("Required service replica", true).unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        remote.live = Some(
            crate::engine::live::LiveReplicas::new(
                remote.runtime.handle(),
                store.clone(),
                remote.completion.clone(),
                remote.received.clone(),
                remote.blob_transfers.clone(),
                Default::default(),
            )
            .unwrap(),
        );
        assert!(
            remote
                .live_fence(&store, None, None, false)
                .unwrap_err()
                .to_string()
                .contains("required live replica needs recovery")
        );
    }

    #[test]
    fn final_fence_reports_a_failed_publication_after_its_notice_was_drained() {
        use crate::engine::live::{ScopeKey, tests::observed_entry};
        use crate::engine::publisher::{PublishKey, PublishState};
        use crate::engine::replication::ReplicaState;
        let (directory, store, volume, address, _, _) = receive_fixture();
        // Open an empty change feed so the fixture setup cannot schedule a
        // bootstrap over the live generation installed below.
        drop(store);
        let store = Arc::new(LocalStore::open(directory.path()).unwrap());
        let (_reader, writer) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(writer))).unwrap();
        remote.identity = preissued_identity(&store);
        remote.ensure_live(&store).unwrap();
        let live = remote.live.as_mut().unwrap();
        let catalog = ScopeKey { volume, session: None, entry: None };
        observed_entry(live, catalog, 7, [1; 16], PublishState::Ready, ReplicaState::Live);
        let database = crate::store::DatabaseId::Entry(address.entry);
        live.publisher().test_database_failure(volume, 7, database, "write rejected");
        remote.consume_publisher_failures().unwrap();
        let error = remote.live_fence(&store, Some(&[volume]), None, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Couldn’t sync database \"data\" ({database}) in volume \"Receive race\" ({volume})\nwrite rejected"
            )
        );

        // An independent selected stream must read labels with its own captured grant.
        let live = remote.live.as_mut().unwrap();
        let selected =
            ScopeKey { volume, entry: Some(address.entry), session: Some(([8; 32], [9; 32])) };
        observed_entry(live, selected, 8, [2; 16], PublishState::Ready, ReplicaState::Live);
        let cause = sync_failure::target_database(
            PublishKey { volume, entry: selected.entry },
            database,
            "selected failure",
        );
        assert_eq!(
            live.resolve_failure(cause).to_string(),
            format!(
                "Couldn’t sync database {database} in volume \"Receive race\" ({volume})\nselected failure"
            )
        );
        live.publisher().test_state(volume, 8, PublishState::Ready);
        assert!(live.publisher().failed_publication(volume).is_none());
        let pending =
            crate::store::Error::NotFound(crate::store::Error::PENDING_BACKING).to_string();
        crate::engine::live::tests::observed_apply_failure(
            live,
            catalog,
            9,
            [3; 16],
            ReplicaState::Gap,
            database,
            &pending,
        );
        let error = remote.live_fence(&store, Some(&[volume]), None, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Couldn’t sync database \"data\" ({database}) in volume \"Receive race\" ({volume})\n{pending}"
            )
        );
        assert!(sync_failure::reason(error.as_ref()).is::<crate::engine::live::LiveGap>());
    }

    #[test]
    fn transfer_rejection_retains_operation_error_and_never_acknowledges_early() {
        let operation = crate::identifier::encode(&[0xab; 16]);
        let mut response = ServiceResponse {
            timing: Default::default(),
            ok: false,
            operation_id: operation.clone(),
            error: Some("unauthorized replica entry: schema removal".into()),
            code: Some("operation_failed".into()),
            denial: None,
            data: None,
        };
        let decode = |response: &ServiceResponse| {
            sync_payload(serde_json::to_vec(response).unwrap(), &operation)
        };
        assert_eq!(
            decode(&response).unwrap_err().to_string(),
            "unauthorized replica entry: schema removal"
        );
        response.operation_id = crate::identifier::encode(&[0xcd; 16]);
        assert!(decode(&response).unwrap_err().to_string().contains("identity differs"));
        response.operation_id = operation.clone();
        response.ok = true;
        assert!(
            decode(&response).unwrap_err().to_string().contains("before synchronization completed")
        );
        let binary = vec![0, 1, 2, 0xff];
        assert_eq!(sync_payload(binary.clone(), &operation).unwrap(), binary);
    }

    #[test]
    fn a_canceled_operation_never_opens_a_replacement_transport() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let remote = engine.remote.as_mut().unwrap();
        // An address nothing answers on: reaching the transport at all would
        // spend this operation's whole connect timeout inside the worker.
        let unanswered = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        remote.set_test_transport(crate::network::ap::ClientConfig::new(
            unanswered.local_addr().unwrap(),
            "localhost",
            Vec::new(),
        ));
        let canceled = Arc::new(crate::engine::cancellation::Canceled::default());
        canceled.raise();
        remote.set_cancellation(Some(canceled));
        let error = remote.credential_vault(&store).err().expect("canceled").to_string();
        assert_eq!(error, super::CANCELED);
        assert!(remote.authenticated_connection.lock().unwrap().is_none());
    }

    /// A correctness bound, not a budget: far below the fixture AP's
    /// authentication timeout, so only real cancellation can satisfy it.
    const RETURN_BY: Duration = Duration::from_secs(10);

    /// Collect worker results until `done`, waiting on the engine's completion
    /// notifications rather than re-checking on a timer.
    #[allow(
        unsafe_code,
        reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
    )]
    fn collect_until(
        engine: &mut crate::engine::Engine,
        done: impl Fn(&crate::engine::Engine) -> bool,
    ) {
        use polling::{Event, Events, Poller};
        let socket =
            engine.completion_socket().expect("Unix engine completion").try_clone().unwrap();
        let poller = Poller::new().unwrap();
        // The socket outlives the poller registration; it is deleted below.
        unsafe {
            poller.add(&socket, Event::readable(1)).unwrap();
        }
        let limit = Instant::now() + RETURN_BY;
        let mut events = Events::new();
        loop {
            engine.complete_background().unwrap();
            if done(engine) {
                break;
            }
            let left = limit.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the worker did not return within {RETURN_BY:?}");
            events.clear();
            poller.wait(&mut events, Some(left)).unwrap();
            poller.modify(&socket, Event::readable(1)).unwrap();
        }
        poller.delete(&socket).unwrap();
    }

    /// Cancellation reaches a running operation by closing the transport it
    /// was handed. An operation still authenticating has no transport to
    /// close, and that exchange ends only at the AP's own timeout, so the
    /// cancellation has to reach the exchange itself: otherwise the worker
    /// keeps Remote and no other network work on this engine can start.
    #[test]
    fn canceling_inside_authentication_returns_remote_before_the_ap_answers() {
        // A held authentication exposes the configured hostname and cancellation retires it.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let (config, observed, gate) = crate::network::ap::testing::held_authentication_server();
        let hostname = config.server_name.clone();
        let remote = engine.remote.as_mut().unwrap();
        remote.set_test_transport(config);
        remote.identity = preissued_identity(&store);

        engine.bootstrap_pending = true;
        engine.start_network_work();
        assert!(engine.remote.is_none(), "the background worker must own Remote");
        observed.recv_timeout(RETURN_BY).expect("the AP must read the worker's HELLO");
        let status = engine.status_snapshot().unwrap().ap_connection;
        assert_eq!(status.state, ApConnectionState::Connecting);
        assert_eq!(status.hostname, hostname);
        assert_eq!(
            engine.transfer_status().progress,
            Some(TransferProgress::Connecting { hostname })
        );

        engine.dispatch_reply(crate::engine::Request::CancelTransfer).unwrap();
        collect_until(&mut engine, |engine| engine.remote.is_some());
        assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Paused);
        assert!(engine.network.progress.snapshot().is_none());

        // Opened only now: the worker returned while the AP still owed a reply.
        gate.open();
        let remote = engine.remote.as_mut().unwrap();
        remote.authenticate(&store).unwrap();
        assert!(
            remote.authenticated_transport().is_some(),
            "a later operation must connect normally"
        );
    }

    /// Process exit has cancellation's need at a moment nobody chose:
    /// `Engine::shutdown` must get `Remote` back from a worker held in
    /// authentication, within its bound, so that dropping the Engine flushes
    /// whatever close there is instead of exiting under the worker. It is
    /// not a user's cancel, so it records no pause.
    #[test]
    fn shutdown_returns_remote_from_a_worker_held_in_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let (config, observed, gate) = crate::network::ap::testing::held_authentication_server();
        let remote = engine.remote.as_mut().unwrap();
        remote.set_test_transport(config);
        remote.identity = preissued_identity(&store);

        engine.bootstrap_pending = true;
        engine.start_network_work();
        assert!(engine.remote.is_none(), "the background worker must own Remote");
        observed.recv_timeout(RETURN_BY).expect("the AP must read the worker's HELLO");

        let started = Instant::now();
        assert_eq!(engine.shutdown(RETURN_BY).unwrap(), crate::engine::Shutdown::Done);
        assert!(
            started.elapsed() < RETURN_BY,
            "shutdown waited on the AP instead of the cancellation"
        );
        let transfer = engine.background.as_ref().unwrap();
        assert!(
            transfer.ready.load(std::sync::atomic::Ordering::Acquire),
            "the worker must have handed Remote back"
        );
        assert_eq!(
            engine.transfer_result.kind,
            crate::engine::reply::TransferKind::Running,
            "shutdown is not a user's cancel"
        );
        drop(gate);
    }

    /// Enrollment is the one exchange shutdown must not cut: the AP may have
    /// issued credentials this device has not saved yet. Shutdown waits its
    /// bound for that worker instead, and a bound that was not enough is
    /// reported as pending, not as a failure the caller may exit on.
    #[test]
    fn shutdown_leaves_an_enrollment_exchange_to_finish() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let (config, observed, gate) = crate::network::ap::testing::held_authentication_server();
        engine.remote.as_mut().unwrap().set_test_transport(config);
        let ticket = engine
            .begin_network_request(crate::engine::Request::RequestCode {
                email: "stop@test.invalid".into(),
            })
            .unwrap();
        assert!(engine.remote.is_none(), "the enrollment worker must own Remote");
        observed
            .recv_timeout(RETURN_BY)
            .expect("the AP must read the worker's registration request");
        assert_eq!(
            engine.status_snapshot().unwrap().ap_connection.state,
            ApConnectionState::Connecting
        );

        assert_eq!(
            engine.shutdown(Duration::from_millis(200)).unwrap(),
            crate::engine::Shutdown::EnrollmentPending
        );
        let transfer = engine.background.as_ref().unwrap();
        assert!(transfer.canceled.raised(), "post-enrollment reads must be cancelable");
        assert!(
            !transfer.ready.load(std::sync::atomic::Ordering::Acquire),
            "registration must ignore cancellation until the issued credentials are saved"
        );
        drop(ticket);
        gate.open();
    }

    #[test]
    fn connection_timeout_shows_reconnecting_while_retaining_a_bounded_retry() {
        for initial in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
            let remote = engine.remote.as_mut().unwrap();
            remote.identity = preissued_identity(&engine.services.store);
            let scope = crate::engine::live::ScopeKey {
                entry: None,
                volume: Id::from_bytes([3; 16]),
                session: None,
            };
            let gate = if !initial {
                let (config, observed, gate) =
                    crate::network::ap::testing::held_authentication_server();
                remote.set_test_transport(config);
                gate.open();
                remote.authenticate(&engine.services.store).unwrap();
                observed.recv_timeout(RETURN_BY).unwrap();
                let mut retry = Reconnect::new(vec![scope]);
                retry.account = true;
                remote.reconnect = Some(retry);
                Some(gate)
            } else {
                None
            };
            let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut config = crate::network::ap::ClientConfig::new(
                blackhole.local_addr().unwrap(),
                "localhost",
                vec![],
            );
            config.connect_timeout = Duration::from_millis(100);
            remote.config = Some(config);
            engine.bootstrap_pending = initial;
            engine.start_network_work();
            collect_until(&mut engine, |engine| engine.remote.is_some());
            assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Running);
            assert_eq!(engine.transfer_result.message.as_deref().unwrap_or(""), "Reconnecting…");
            assert_eq!(
                engine.network.connection_status.snapshot().state,
                ApConnectionState::Connecting
            );
            let remote = engine.remote.as_ref().unwrap();
            let retry = remote.reconnect.as_ref().unwrap();
            assert_eq!(retry.bootstrap, initial);
            if !initial {
                assert!(retry.account);
                assert_eq!(retry.scopes, [scope]);
            }
            assert!(remote.reconnect_after(Instant::now()).unwrap() <= Duration::from_millis(250));
            drop(gate);
        }
    }

    #[test]
    fn lookup_failure_retains_initial_and_transport_retries() {
        for initial in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
            let remote = engine.remote.as_mut().unwrap();
            remote.identity = preissued_identity(&engine.services.store);
            let scope = crate::engine::live::ScopeKey {
                entry: None,
                volume: Id::from_bytes([3; 16]),
                session: None,
            };
            let gate = if !initial {
                let (config, observed, gate) =
                    crate::network::ap::testing::held_authentication_server();
                remote.set_test_transport(config);
                gate.open();
                remote.authenticate(&engine.services.store).unwrap();
                observed.recv_timeout(RETURN_BY).unwrap();
                let mut retry = Reconnect::new(vec![scope]);
                retry.account = true;
                remote.reconnect = Some(retry);
                Some(gate)
            } else {
                None
            };
            remote.config = None;
            remote.lookup_error = Some(std::io::Error::other("offline address lookup"));
            engine.bootstrap_pending = initial;
            engine.start_network_work();
            collect_until(&mut engine, |engine| engine.remote.is_some());
            assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Running);
            assert_eq!(engine.transfer_result.message.as_deref(), Some("Reconnecting…"));
            let remote = engine.remote.as_ref().unwrap();
            let retry = remote.reconnect.as_ref().unwrap();
            assert_eq!(retry.bootstrap, initial);
            if !initial {
                assert!(retry.account);
                assert_eq!(retry.scopes, [scope]);
            }
            assert!(remote.reconnect_after(Instant::now()).unwrap() <= Duration::from_millis(250));
            drop(gate);
        }
    }

    #[test]
    fn lookup_timeouts_reuse_one_pending_resolver() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let lookup = remote.runtime.spawn(std::future::pending());
        let task = lookup.id();
        remote.address_lookup = Some(lookup);
        for _ in 0..2 {
            let error = remote.config().err().expect("pending lookup must time out");
            assert!(matches!(
                error.downcast_ref::<ClientError>(),
                Some(ClientError::ConnectTimeout)
            ));
            assert_eq!(remote.address_lookup.as_ref().unwrap().id(), task);
        }
        remote.address_lookup.as_ref().unwrap().abort();
    }

    #[test]
    fn authentication_timeout_remains_a_visible_failure_without_a_retry() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let (mut config, observed, gate) =
            crate::network::ap::testing::held_authentication_server();
        config.authentication_timeout = Duration::from_millis(100);
        let remote = engine.remote.as_mut().unwrap();
        remote.set_test_transport(config);
        remote.identity = preissued_identity(&engine.services.store);
        engine.bootstrap_pending = true;
        engine.start_network_work();
        observed.recv_timeout(RETURN_BY).unwrap();
        collect_until(&mut engine, |engine| engine.remote.is_some());
        assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Failed);
        assert_eq!(
            engine.transfer_result.message.as_deref().unwrap_or(""),
            "the AP did not answer authentication in time"
        );
        assert!(engine.remote.as_ref().unwrap().reconnect_after(Instant::now()).is_none());
        assert_eq!(engine.network.connection_status.snapshot().state, ApConnectionState::Offline);
        drop(gate);
    }

    #[test]
    fn refused_connection_retains_bootstrap_and_recovery_with_bounded_backoff() {
        for initial in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let (_receive, completion) = UnixStream::pair().unwrap();
            completion.set_nonblocking(true).unwrap();
            let mut remote =
                Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
            let store = Arc::new(LocalStore::open(directory.path().join("store")).unwrap());
            remote.identity = preissued_identity(&store);
            let scope = crate::engine::live::ScopeKey {
                entry: None,
                volume: Id::from_bytes([3; 16]),
                session: None,
            };
            let gate = if !initial {
                let (config, observed, gate) =
                    crate::network::ap::testing::held_authentication_server();
                remote.set_test_transport(config);
                gate.open();
                remote.authenticate(&store).unwrap();
                observed.recv_timeout(RETURN_BY).unwrap();
                let mut retry = Reconnect::new(vec![scope]);
                retry.account = true;
                remote.reconnect = Some(retry);
                Some(gate)
            } else {
                None
            };
            let (runtime, endpoint, config) = crate::network::ap::testing::loopback_ap();
            // Change the destination without disconnecting: recovery must keep
            // the previous endpoint and its already-claimed recovery scopes.
            remote.config = Some(config);
            let (done, finished) = tokio::sync::oneshot::channel();
            let server = std::thread::spawn(move || {
                runtime.block_on(async {
                    tokio::time::timeout(RETURN_BY, endpoint.accept())
                        .await
                        .unwrap()
                        .unwrap()
                        .refuse();
                    // Keep the endpoint's driver alive until the client has received
                    // the refusal, rather than racing a test-only endpoint drop.
                    let _ = tokio::time::timeout(RETURN_BY, finished).await.unwrap();
                })
            });
            let error = if initial { remote.sync(&store) } else { remote.reconnect(&store) }
                .expect_err("the connection was refused");
            done.send(()).unwrap();
            server.join().unwrap();
            assert!(
                error
                    .downcast_ref::<crate::network::ap::ClientError>()
                    .is_some_and(|error| error.is_connection_refused()),
                "{error}"
            );
            let retry = remote.reconnect.as_ref().expect("refusal must retain a retry");
            assert_eq!(retry.bootstrap, initial);
            if !initial {
                assert!(retry.account);
                assert_eq!(retry.scopes, [scope]);
            }
            let now = Instant::now();
            assert!(!remote.reconnect_ready(now) && !remote.bootstrap_retry_ready(now));
            assert!(remote.reconnect_after(now).unwrap() <= Duration::from_millis(250));
            assert!(
                remote.reconnect_ready(now + Duration::from_secs(1))
                    || remote.bootstrap_retry_ready(now + Duration::from_secs(1))
            );
            drop(gate);
        }
    }

    /// The same window on the transport-recovery path: a scheduled retry is
    /// authenticating its replacement connection when the owner cancels.
    #[test]
    fn canceling_inside_reconnect_authentication_returns_before_the_ap_answers() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let store = Arc::new(LocalStore::open(directory.path().join("store")).unwrap());
        let (config, observed, gate) = crate::network::ap::testing::held_authentication_server();
        remote.set_test_transport(config);
        remote.identity = preissued_identity(&store);

        // Connect once normally, then hold the next HELLO so the retry below
        // cannot finish authenticating its replacement.
        gate.open();
        remote.authenticate(&store).unwrap();
        observed.recv_timeout(RETURN_BY).expect("the AP must read the first HELLO");
        gate.close();

        // An armed retry that already claimed a scope needing transport
        // recovery, as a real transport-loss episode does.
        let scope = crate::engine::live::ScopeKey {
            entry: None,
            volume: Id::from_bytes([3; 16]),
            session: None,
        };
        remote.reconnect = Some(Reconnect::new(vec![scope]));
        let canceled = Arc::new(crate::engine::cancellation::Canceled::default());
        remote.set_cancellation(Some(canceled.clone()));
        let owner = std::thread::spawn(move || {
            observed.recv_timeout(RETURN_BY).expect("the AP must read the retry's HELLO");
            canceled.raise();
        });
        let started = Instant::now();
        let error = remote.reconnect(&store).expect_err("canceled").to_string();
        owner.join().unwrap();
        assert_eq!(error, super::CANCELED);
        assert!(
            started.elapsed() < RETURN_BY,
            "the retry waited on the AP instead of the cancellation"
        );
        // Cancelling interrupted the attempt; it did not decide anything about
        // the transport. The episode has to survive, or the scopes it claimed
        // are retired with it and nothing schedules their recovery again.
        assert_eq!(
            remote.reconnect.as_ref().map(|retry| retry.scopes.as_slice()),
            Some(&[scope][..])
        );
        assert!(remote.reconnect_ready(Instant::now()), "a canceled retry stays ready");

        gate.open();
        remote.set_cancellation(None);
        remote.disconnect();
        remote.authenticate(&store).unwrap();
        assert!(
            remote.authenticated_transport().is_some(),
            "a later operation must connect normally"
        );
    }

    /// The same rule on the synchronize path, which also claims the pending
    /// attempt before connecting: a cancellation must hand it back, or the
    /// device is left with nothing scheduled and no reason to try again.
    #[test]
    fn canceling_a_synchronize_keeps_its_pending_connection_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let (_receive, completion) = UnixStream::pair().unwrap();
        completion.set_nonblocking(true).unwrap();
        let mut remote = Remote::open(directory.path(), Arc::new(Wake::unix(completion))).unwrap();
        let store = Arc::new(LocalStore::open(directory.path().join("store")).unwrap());
        let unanswered = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        remote.set_test_transport(crate::network::ap::ClientConfig::new(
            unanswered.local_addr().unwrap(),
            "localhost",
            Vec::new(),
        ));
        remote.identity = preissued_identity(&store);

        let mut armed = Reconnect::new(Vec::new());
        armed.bootstrap = true;
        armed.failed(Instant::now());
        let after = armed.after;
        remote.reconnect = Some(armed);
        let canceled = Arc::new(crate::engine::cancellation::Canceled::default());
        canceled.raise();
        remote.set_cancellation(Some(canceled));

        let error = remote.sync(&store).expect_err("canceled").to_string();
        assert_eq!(error, super::CANCELED);
        let retry =
            remote.reconnect.as_ref().expect("the pending attempt must survive cancellation");
        assert!(retry.bootstrap && retry.after == after, "cancellation must not add backoff");
    }

    /// Checks reconnect delays against fixed backoff regression cases.
    #[test]
    fn reconnect_backoff_matches_regression_cases() {
        let mut cases = 0;
        for line in include_str!("../../tests/fixtures/recovery.txt").lines() {
            let w: Vec<&str> = line.split_whitespace().collect();
            if w[0] != "backoff" {
                continue;
            }
            let mut retry = Reconnect::new(vec![]);
            retry.failures = w[1].parse().unwrap();
            let now = Instant::now();
            retry.failed(now);
            assert_eq!(retry.after - now, Duration::from_millis(w[2].parse().unwrap()), "{line}");
            assert_eq!(retry.failures.to_string(), w[3], "{line}");
            cases += 1;
        }
        assert_eq!(cases, 12);
    }
}

#[cfg(test)]
mod credential_persistence_tests {
    use super::*;

    #[test]
    fn relaunch_completes_interrupted_enrollment_without_changing_device_key() {
        let directory = tempfile::tempdir().expect("profile");
        let engine = crate::engine::Engine::open(directory.path()).expect("engine");
        let seed = engine.services.store.account_seed().expect("seed");
        drop(engine);
        let mut account_bytes = [0; 16];
        getrandom::fill(&mut account_bytes).expect("account ID");
        let account = Id::from_bytes(account_bytes);
        let mut identity =
            ClientIdentity::from_seed("relaunch@example.invalid", "Test", seed).expect("identity");
        identity.auth_token = account.as_bytes().iter().copied().chain([0; 56]).collect();
        identity.instance_id = crate::store::Id::from_bytes([1; 16]).to_string().into_bytes();
        identity.ca_certificate = vec![1];
        identity.instance_certificate = vec![1];
        identity.save(&directory.path().join("instance.json")).expect("persist before bind");
        let reopened = crate::engine::Engine::open(directory.path()).expect("finish binding");
        assert_eq!(reopened.services.store.account().id, account);
        assert_eq!(reopened.services.store.account_seed().expect("seed"), seed);
        assert!(reopened.enrolled());
        drop(reopened);
        let again = crate::engine::Engine::open(directory.path()).expect("relaunch");
        assert_eq!(again.services.store.account().id, account);
    }

    #[test]
    fn relaunch_rejects_authentication_for_a_different_device_key() {
        let directory = tempfile::tempdir().expect("profile");
        drop(crate::engine::Engine::open(directory.path()).expect("engine"));
        let identity = ClientIdentity::from_seed("other@example.invalid", "Other", [88; 32])
            .expect("identity");
        identity.save(&directory.path().join("instance.json")).expect("different identity");
        assert!(crate::engine::Engine::open(directory.path()).is_err());
    }
}

#[cfg(test)]
mod authentication_log_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn protocol_failure_is_appended_to_private_profile_log() {
        let directory = tempfile::tempdir().expect("fixture");
        let profile = directory.path().join("profile");
        fs::create_dir(&profile).expect("profile");
        let identity = profile.join("instance.json");
        let log = crate::paths::Paths::for_profile(&profile).expect("paths").engine_log;
        let error = ClientError::Protocol(crate::network::ap::Error::InvalidTiming);
        for operation in ["request-code", "sign-in-with-key", "sign-in-with-code"] {
            log_authentication_protocol_error(&identity, operation, &error);
        }
        let text = fs::read_to_string(&log).expect("persisted diagnostic");
        assert_eq!(text.lines().count(), 3);
        for operation in ["request-code", "sign-in-with-key", "sign-in-with-code"] {
            assert!(text.contains(&format!(
                "authentication {operation}: InvalidTiming: \
                 AP authentication timing envelope is invalid"
            )));
        }
        assert_eq!(fs::metadata(&log).expect("log mode").permissions().mode() & 0o777, 0o600);
        log_authentication_protocol_error(
            &identity,
            "sign-in-with-code",
            &ClientError::InvalidChallenge,
        );
        assert_eq!(fs::read_to_string(log).expect("unchanged log"), text);
    }
}
