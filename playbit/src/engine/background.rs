// Error ownership: NetworkTask returns both Remote and its Result to the
// completion owner; canceled replies do not undo durable work. Worker loss and
// late cancellation are injected in this module's tests. Per-call OS/storage
// failure variants are propagated, not individually injected; never report
// success or replay a lost input.
use super::reply::{Reply, SignInReply, TransferKind, TransferStatus};
use super::*;
#[cfg(test)]
use serde_json::{Value, json};
use std::{
    os::unix::net::UnixStream,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use super::cancellation::Canceled;

struct NotifyCompletion(Arc<Wake>, Arc<AtomicBool>);
impl Drop for NotifyCompletion {
    fn drop(&mut self) {
        self.1.store(true, Ordering::Release);
        self.0.notify();
    }
}

/// One waiting IPC caller; dropping it never repeats or rolls back dispatched
/// network work. Its completion signal releases retained result admission.
pub(crate) struct NetworkTicket {
    abandoned: Arc<AtomicBool>,
    completion: Arc<Wake>,
    armed: bool,
    control: bool,
}
impl Drop for NetworkTicket {
    fn drop(&mut self) {
        if self.armed {
            drop(NotifyCompletion(self.completion.clone(), self.abandoned.clone()));
        }
    }
}

type TransferResult = (
    remote::Remote,
    std::result::Result<NetworkResult, String>,
    BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
);

pub(crate) struct Transfer {
    pub(super) ready: Arc<AtomicBool>,
    receiver: mpsc::Receiver<TransferResult>,
    remaining_sessions: Vec<String>,
    cancel: Arc<Mutex<Option<Arc<crate::network::ap::ApConnection>>>>,
    session: Option<String>,
    pub(super) canceled: Arc<Canceled>,
    disconnected: bool,
    enrolled: bool,
    replacement_required: bool,
    email: Option<String>,
    recovery_control: Option<recovery::control::Kind>,
    developer: bool,
    enrollment: bool,
    diagnostic: bool,
    pub(super) diagnostic_read: bool,
}

impl Transfer {
    fn reads_app_display(&self) -> bool {
        self.developer && self.diagnostic_read
    }
}

/// What a bounded `Engine::shutdown` reached.
#[derive(Debug, Eq, PartialEq)]
pub enum Shutdown {
    /// No worker owns `Remote` any more: dropping the Engine flushes its close.
    Done,
    /// The bound passed while an enrollment exchange was still in flight. Its
    /// credentials may already exist at the AP, so the process must keep the
    /// Engine alive and ask again instead of exiting.
    EnrollmentPending,
    /// Accepted local data work still owns commit/abort. Keep the engine alive.
    AcceptedWorkPending,
}

pub(crate) enum NetworkResult {
    Enrollment(enrollment::Outcome),
    Synchronized,
    OwnerSessionPrepared,
    InitializationPending(String),
    SynchronizationPending(remote::Synchronization),
    CheckedUpdate,
    SelectionUpdates,
    Control(recovery::control::Outcome),
    Developer(developer::Outcome),
    Diagnostic(Box<reply::DiagnosticReply>),
}
enum NetworkWork {
    Enrollment(enrollment::Action),
    SynchronizeSelections(Vec<String>),
    CheckUpdate(String),
    SelectionUpdates(Vec<String>),
    Developer(Request),
    Diagnostic(Request),
    Synchronize,
    ContinueSynchronization(remote::Synchronization),
    InitializeVolumes,
    Join(String),
    PrepareOwnerSession(String),
    RecoverGaps,
    Reconnect,
    Control(recovery::control::Control),
}

/// Owned input of the sole network worker. Remote returns to Engine only after
/// this task has finished; prepared guests and the continuation remain bounded.
struct NetworkTask {
    remote: remote::Remote,
    store: Arc<LocalStore>,
    runtime: Runtime,
    path: PathBuf,
    prepared: Option<Session>,
    sessions: Vec<String>,
    active: Vec<String>,
    canceled: Arc<Canceled>,
}
impl NetworkTask {
    fn run(mut self, work: NetworkWork) -> TransferResult {
        let mut updates = BTreeMap::new();
        let is_developer = matches!(work, NetworkWork::Developer(_));
        let initializes_sources = matches!(
            work,
            NetworkWork::InitializeVolumes
                | NetworkWork::Synchronize
                | NetworkWork::ContinueSynchronization(_)
        );
        let recovers_gaps = matches!(work, NetworkWork::RecoverGaps);
        let apps = self.path.join(crate::paths::BUILDS);
        let result = match work {
            NetworkWork::SynchronizeSelections(_) => Ok(NetworkResult::Synchronized),
            NetworkWork::CheckUpdate(_) => Ok(NetworkResult::CheckedUpdate),
            NetworkWork::SelectionUpdates(_) => Ok(NetworkResult::SelectionUpdates),
            NetworkWork::Enrollment(action) => {
                action.execute(&mut self.remote, &self.store).map(NetworkResult::Enrollment)
            }
            NetworkWork::Developer(request) => developer::execute(
                request,
                &mut self.remote,
                &self.store,
                &self.runtime,
                &self.path,
            )
            .map(NetworkResult::Developer),
            NetworkWork::Diagnostic(request) => {
                diagnostics::execute(request, &mut self.remote, &self.store)
                    .map(|reply| NetworkResult::Diagnostic(Box::new(reply)))
            }
            NetworkWork::Control(control) => control
                .execute(&mut self.remote, &self.store, &self.canceled)
                .map(NetworkResult::Control),
            NetworkWork::Reconnect => {
                self.remote.reconnect(&self.store).map(|()| NetworkResult::Synchronized)
            }
            NetworkWork::InitializeVolumes => {
                self.remote.initialize_volumes(&self.store).map(|()| NetworkResult::Synchronized)
            }
            NetworkWork::RecoverGaps => {
                self.remote.recover_gaps(&self.store).map(|()| NetworkResult::Synchronized)
            }
            NetworkWork::Synchronize | NetworkWork::ContinueSynchronization(_) => {
                let continuation = if let NetworkWork::ContinueSynchronization(state) = work {
                    Some(state)
                } else {
                    None
                };
                self.remote.synchronize_next(&self.store, continuation).map(|next| {
                    next.map_or(NetworkResult::Synchronized, NetworkResult::SynchronizationPending)
                })
            }
            NetworkWork::Join(id) => {
                self.join(&id, &apps, &mut updates).map(|()| NetworkResult::Synchronized)
            }
            NetworkWork::PrepareOwnerSession(id) => {
                self.join(&id, &apps, &mut updates).map(|()| NetworkResult::OwnerSessionPrepared)
            }
        };
        let result = match result {
            Err(error)
                if (initializes_sources && self.remote.initialization_retrying(error.as_ref()))
                    || (recovers_gaps && self.remote.gap_readiness_retrying(error.as_ref())) =>
            {
                Ok(NetworkResult::InitializationPending(error.to_string()))
            }
            result => result.map_err(|error| super::failure::message(&self.path, error.as_ref())),
        };
        for id in self.sessions {
            if matches!(result, Ok(NetworkResult::SynchronizationPending(_))) {
                break;
            }
            // Denied bootstrap must not start session discovery. A confirmed
            // transport can still discover apps while another volume waits for a peer.
            if self.canceled.raised()
                || (result.is_err()
                    && (is_developer || (initializes_sources && !self.remote.sync_connected())))
            {
                break;
            }
            // Discovery must not bypass initial connection backoff for each app.
            if self.remote.reconnect_after(std::time::Instant::now()).is_some() {
                break;
            }
            if !is_developer
                && let Err(error) = self.remote.watch_selection(&self.store, &id, &self.active)
            {
                updates.insert(id, Err(error.to_string()));
                continue;
            }
            updates.insert(
                id.clone(),
                updates::prepare_update(&mut self.remote, &self.store, &self.runtime, &id, &apps)
                    .map_err(|error| super::failure::message(&self.path, error.as_ref())),
            );
        }
        self.remote.sample_gauges();
        self.remote.refresh_connection_status();
        (self.remote, result, updates)
    }
    fn join(
        &mut self,
        id: &str,
        apps: &Path,
        updates: &mut BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
    ) -> Result<()> {
        let session = match self.prepared.take() {
            Some(session) => session,
            None => {
                let envelope = self.remote.read_invitation(&self.store, id)?;
                self.store.receive_invitation(id, &envelope)?;
                if self.canceled.raised() {
                    return Err("Join canceled; your invitation is retained".into());
                }
                self.store.import_invitation(id)?
            }
        };
        self.remote.check_cancellation()?;
        let resolution = self.remote.bootstrap_current(&self.store, &session.id, apps)?;
        self.remote.check_cancellation()?;
        let prepared =
            updates::prepare_candidate(&self.runtime, &self.store, &session.id, apps, resolution)?;
        self.remote.check_cancellation()?;
        updates.insert(session.id, Ok(prepared));
        Ok(())
    }
}

impl Engine {
    /// Finite network preparation; interactive owners remain in their hosts.
    /// Direct owners drive this with completion_socket(), never a freshness timer.
    pub fn background_pending(&self) -> bool {
        self.background.is_some() || self.synchronization.is_some()
    }
    fn refresh_session_builds(&mut self) -> bool {
        if let Err(error) =
            self.network.refresh_builds(self.services.store.clone(), self.completion.wake.clone())
        {
            let error = error.to_string();
            if self.availability_cache != error {
                self.availability_cache = error;
                self.state_changed();
            }
            return false;
        }
        true
    }
    pub(crate) fn start_network_work(&mut self) {
        if !self.refresh_session_builds() {
            return;
        }
        if self.network.connection_status.offline() {
            return;
        }
        if self.background.is_some()
            || (self.network_request_waiter.is_some() && self.synchronization.is_none())
        {
            return;
        }
        let active = self.active_sessions();
        if let Some(remote) = self.remote.as_mut() {
            remote.retire_selection_watches(&active);
        }
        let now = std::time::Instant::now();
        #[cfg(target_vendor = "apple")]
        if self.synchronization.is_none()
            && self.remote.as_ref().is_some_and(|remote| {
                remote.account_backup_pending(&self.services.store).unwrap_or(false)
            })
        {
            if let Err(error) =
                self.begin_network_work(NetworkWork::Enrollment(enrollment::Action::Backup {
                    retry: false,
                }))
            {
                self.availability_cache = error.to_string();
                self.state_changed();
            }
            return;
        }
        if self.remote.as_ref().is_some_and(remote::Remote::sync_connected)
            && self.network.builds.take_uploads_pending()
        {
            self.bootstrap_pending = true;
        }
        let work = if let Some(session) = &self.pending_join {
            NetworkWork::Join(session.clone())
        } else if let Some(continuation) = self.synchronization.take() {
            NetworkWork::ContinueSynchronization(continuation)
        } else if self.bootstrap_pending
            || self.remote.as_ref().is_some_and(|remote| remote.bootstrap_retry_ready(now))
        {
            NetworkWork::Synchronize
        } else if self.remote.as_ref().is_some_and(|remote| remote.reconnect_ready(now)) {
            NetworkWork::Reconnect
        } else if self.remote.as_ref().is_some_and(remote::Remote::needs_gap_recovery) {
            NetworkWork::RecoverGaps
        } else if self.remote.as_ref().is_some_and(remote::Remote::needs_initialization) {
            NetworkWork::InitializeVolumes
        } else {
            let sessions = self
                .remote
                .as_ref()
                .map(|remote| remote.selection_updates(&active))
                .unwrap_or_default();
            if sessions.is_empty() {
                return;
            }
            NetworkWork::SelectionUpdates(sessions.into_iter().take(1).collect())
        };
        if let Err(error) = self.begin_network_work(work) {
            self.transfer_result = TransferStatus {
                kind: TransferKind::Failed,
                message: Some(error.to_string()),
                ..Default::default()
            };
            self.state_changed();
        }
    }
    pub(crate) fn network_wake_after(
        &self,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        if self.background_pending() || self.network_request_waiter.is_some() {
            return None;
        }
        self.remote.as_ref().and_then(|remote| remote.wake_after(now))
    }
    pub(crate) fn network_request_available(&self) -> bool {
        !self.background_pending() && self.network_request_waiter.is_none() && self.remote.is_some()
    }
    pub(crate) fn network_request_ready(&self) -> bool {
        self.network_request_available()
    }

    pub(crate) fn begin_network_request(&mut self, request: Request) -> Result<NetworkTicket> {
        if !self.network_request_ready() {
            return Err("network operation is busy; request did not start".into());
        }
        let control = !matches!(request, Request::Sync);
        let work = if enrollment::is_request(&request) {
            let action = enrollment::Action::prepare(request)?;
            NetworkWork::Enrollment(action)
        } else if developer::is_request(&request) {
            developer::validate(&request)?;
            NetworkWork::Developer(request)
        } else if diagnostics::is_request(&request) {
            NetworkWork::Diagnostic(request)
        } else if control {
            NetworkWork::Control(self.prepare_recovery_control(request)?)
        } else {
            NetworkWork::Synchronize
        };
        self.begin_network_work(work)?;
        if self.background.as_ref().is_some_and(|transfer| {
            !transfer.diagnostic
                && !transfer.diagnostic_read
                && transfer.recovery_control != Some(recovery::control::Kind::Status)
        }) {
            self.joined_for_open = None;
        }
        Ok(self.network_request_ticket(control))
    }

    fn network_request_ticket(&mut self, control: bool) -> NetworkTicket {
        let abandoned = Arc::new(AtomicBool::new(false));
        self.network_request_waiter = Some(abandoned.clone());
        NetworkTicket { abandoned, completion: self.completion.wake.clone(), armed: true, control }
    }

    pub(crate) fn network_request_complete(&self, ticket: &NetworkTicket) -> bool {
        !self.background_pending()
            && self
                .network_request_waiter
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &ticket.abandoned))
    }

    pub(crate) fn take_network_request(&mut self, mut ticket: NetworkTicket) -> Result<Reply> {
        if !self.network_request_complete(&ticket) {
            return Err("network operation has not completed".into());
        }
        ticket.armed = false;
        self.network_request_waiter = None;
        if ticket.control {
            return self
                .network_response
                .take()
                .ok_or("missing network control response")?
                .map_err(|error| error.into());
        }
        if self.transfer_result.kind == TransferKind::Complete {
            return Ok(Reply::Availability { availability: self.availability_cache.clone() });
        }
        let message =
            self.transfer_result.message.as_deref().unwrap_or("Synchronization did not complete");
        let mut end = message.len().min(512);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        Err(message[..end].to_owned().into())
    }

    pub(crate) fn background_ready(&self) -> bool {
        self.background.as_ref().is_some_and(|transfer| transfer.ready.load(Ordering::Acquire))
    }
    pub(crate) fn enrolled(&self) -> bool {
        self.remote
            .as_ref()
            .map_or_else(|| self.background.as_ref().is_some_and(|b| b.enrolled), |r| r.enrolled())
    }
    pub(crate) fn replacement_required(&self) -> bool {
        self.remote.as_ref().map_or_else(
            || self.background.as_ref().is_some_and(|work| work.replacement_required),
            |remote| remote.replacement_required(),
        )
    }
    pub(super) fn email(&self) -> Option<String> {
        self.remote
            .as_ref()
            .map(|r| r.email().to_owned())
            .or_else(|| self.background.as_ref().and_then(|b| b.email.clone()))
    }
    pub(super) fn cancel_session_open(&mut self, id: &str) -> Result<()> {
        self.finish_join_open(id);
        if self.pending_join.as_deref() == Some(id) {
            self.pending_join = None;
        }
        if let Some(work) =
            self.background.as_mut().filter(|work| work.session.as_deref() == Some(id))
        {
            // Cancel this download without cancel_all_creations(), which can
            // contain a different, already-local app being initialized.
            work.canceled.raise();
            if self.synchronization.is_none()
                && let Some(connection) =
                    work.cancel.lock().map_err(|_| "connection lock poisoned")?.as_ref()
            {
                connection.shutdown();
            }
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some(("Session left on this device").to_string()),
                ..Default::default()
            };
            self.availability_cache = self.availability()?;
        }
        Ok(())
    }
    pub(super) fn prepare_session(&mut self, id: &str) -> Result<Reply> {
        self.finish_join_open(id);
        self.services.store.retain_session(id)?;
        self.prepare_existing_session(self.services.store.session(id)?, false)
    }

    // A surviving process on another device keeps its session authority after
    // account removal. Its recovery cannot join an invitation or re-list the app.
    pub(super) fn prepare_existing_session(
        &mut self,
        session: Session,
        recovering_owner: bool,
    ) -> Result<Reply> {
        let id = session.id.as_str();
        if self.starting_session(id) {
            return Ok(Reply::Transfer { transfer: self.transfer_status() });
        }
        let local = self.services.store.is_local_session(id)?;
        if !local {
            self.network.builds.retry(&session.resolved_build_id);
        }
        if !self.session_data_ready(&session, local)? {
            if recovering_owner {
                return self.begin_network_work(NetworkWork::PrepareOwnerSession(id.into()));
            }
            self.services.store.prepare_retained_invitation(id)?;
            return self.start_transfer(Some(id.into()));
        }
        if local {
            let path =
                self.services.path.join(crate::paths::BUILDS).join(&session.resolved_build_id);
            let package = self.services.store.authenticate_package(
                crate::package::Package::open(&path)?,
                crate::store::now_unix_secs(),
            )?;
            if package.id()? != session.resolved_build_id || package.identity()? != &session.app {
                return Err("The cached package differs from the retained app session".into());
            }
        }
        if let Some(reason) = self.services.store.session_denial(id)? {
            return Err(reason.into());
        }
        self.services.store.admit_binding(
            &Access::session_version(
                self.services.store.account().id,
                id,
                &session.resolved_build_id,
            ),
            self.session_admission_volume(&session)?,
        )?;
        if !recovering_owner {
            self.services.store.record_session_use(id)?;
        }
        Ok(Reply::SessionPrepared(super::reply::PreparedSession {
            invitation: self.services.store.session_reference(id)?.link(self.channel()),
            availability: self.availability_cache.clone(),
            update: self.updates.get(id).cloned(),
            session,
        }))
    }
    pub(super) fn session_data_ready(&self, session: &Session, local: bool) -> Result<bool> {
        let store = &self.services.store;
        if (local || session.shared_volume.is_some())
            && remote::world_needs_recovery(store, session)?
        {
            let room_can_open = session.shared_volume.is_some()
                && self.network.owner_backing_missing(session.root_volume)
                && store.shared_session_initialized(&session.id)?;
            if !room_can_open {
                return Ok(false);
            }
        }
        if local
            && (session.shared_volume.is_none()
                || !store.shared_session_initialized(&session.id)?)
        {
            return Ok(true);
        }
        Ok(self.services.path.join(crate::paths::BUILDS).join(&session.resolved_build_id).is_file()
            && store
                .replica_root(session.replica_volume(), crate::store::DatabaseId::Index)?
                .is_some())
    }
    /// Before Shared's first use the local session is admitted against World.
    /// Once initialized, the replicated Shared policy is the session capability.
    pub(super) fn session_admission_volume(&self, session: &Session) -> Result<Id> {
        if session.shared_volume.is_some()
            && !self.services.store.shared_session_initialized(&session.id)?
        {
            return Ok(session.root_volume);
        }
        Ok(session.replica_volume())
    }
    pub(super) fn starting_session(&self, id: &str) -> bool {
        self.background.as_ref().is_some_and(|work| work.session.as_deref() == Some(id))
    }
    pub(super) fn start_transfer(&mut self, session: Option<String>) -> Result<Reply> {
        if self.network.connection_status.offline() {
            return Err("Work Offline is enabled. Turn it off to connect.".into());
        }
        if let Some(id) = &session
            && (self.background_pending() || self.network_request_waiter.is_some())
        {
            if self.pending_join.as_ref().is_some_and(|pending| pending != id) {
                return Err(
                    "Another invitation is waiting to open; cancel it before opening this one"
                        .into(),
                );
            }
            self.joined_for_open = None;
            self.pending_join = session;
            self.transfer_result = TransferStatus {
                kind: TransferKind::Running,
                session: self.pending_join.clone(),
                message: Some("Waiting to open the app…".into()),
                ..Default::default()
            };
            self.state_changed();
            return Ok(Reply::Transfer { transfer: self.transfer_status() });
        }
        let reply =
            self.begin_network_work(session.map_or(NetworkWork::Synchronize, NetworkWork::Join))?;
        self.joined_for_open = None;
        Ok(reply)
    }
    pub(super) fn start_update_check(&mut self, session: String) -> Result<Reply> {
        let _timing = crate::trace::Span::new("engine.update.request");
        // This is explicit metadata management, also valid for a closed app.
        self.services.store.session(&session)?;
        if self.background_pending() || self.network_request_waiter.is_some() {
            return Err(
                "Another network operation is in progress. Retry the update when it finishes."
                    .into(),
            );
        }
        // The same one-session continuation used by a multi-session transfer:
        // resolve/prepare this selection and report completion, without sync.
        let reply = self.begin_network_work(NetworkWork::CheckUpdate(session))?;
        self.joined_for_open = None;
        Ok(reply)
    }
    fn begin_network_work(&mut self, work: NetworkWork) -> Result<Reply> {
        // This lane also serves local file/copy/recovery operations. Those
        // remain admitted; their remote fallbacks enforce connection intent.
        if self.network.connection_status.offline() && !matches!(work, NetworkWork::Diagnostic(_)) {
            return Err("Work Offline is enabled. Turn it off to connect.".into());
        }
        let continue_sync = matches!(
            work,
            NetworkWork::SynchronizeSelections(_) | NetworkWork::ContinueSynchronization(_)
        ) || (self.synchronization.is_some()
            && matches!(work, NetworkWork::Join(_) | NetworkWork::PrepareOwnerSession(_)));
        if self.background.is_some() || (!continue_sync && self.network_request_waiter.is_some()) {
            return Ok(Reply::Transfer { transfer: self.transfer_status() });
        }
        let starts_bootstrap = matches!(work, NetworkWork::Reconnect | NetworkWork::Synchronize);
        let recovery = matches!(
            work,
            NetworkWork::Reconnect | NetworkWork::InitializeVolumes | NetworkWork::RecoverGaps
        );
        let session = match &work {
            NetworkWork::Join(id) | NetworkWork::PrepareOwnerSession(id) => Some(id.clone()),
            _ => None,
        };
        let recovery_control = match &work {
            NetworkWork::Control(control) => Some(control.kind()),
            _ => None,
        };
        let prepared = match &work {
            NetworkWork::PrepareOwnerSession(id) => Some(self.services.store.session(id)?),
            _ => self.prepare_join(session.as_deref())?,
        };
        let mut remote = self.remote.take().ok_or("A network operation is already in progress")?;
        self.network.progress.set(None, &self.completion.wake);
        if starts_bootstrap {
            self.bootstrap_pending = false;
        }
        let cancel = remote.cancel_handle();
        let enrolled = remote.enrolled();
        let replacement_required = remote.replacement_required();
        let email = Some(remote.email().to_owned());
        let store = self.services.store.clone();
        let path = self.services.path.clone();
        let is_developer = matches!(work, NetworkWork::Developer(_));
        let is_enrollment = matches!(work, NetworkWork::Enrollment(_));
        let is_diagnostic = matches!(work, NetworkWork::Diagnostic(_));
        let diagnostic_read = matches!(
            work,
            NetworkWork::Diagnostic(
                Request::RejectedEntries { .. } | Request::Usage { .. } | Request::FileRead { .. }
            ) | NetworkWork::Developer(Request::GetAppDisplay { .. })
        );
        let selection_only = matches!(
            work,
            NetworkWork::SelectionUpdates(_) | NetworkWork::ContinueSynchronization(_)
        );
        let runtime = self.runtime.clone();
        let active = self.active_sessions();
        let mut sessions = if let NetworkWork::SelectionUpdates(ids)
        | NetworkWork::SynchronizeSelections(ids) = &work
        {
            ids.clone()
        } else if let NetworkWork::CheckUpdate(session) = &work {
            vec![session.clone()]
        } else if let NetworkWork::Developer(Request::UpdateSession { session, .. }) = &work {
            vec![session.clone()]
        } else if !is_developer
            && !is_enrollment
            && !is_diagnostic
            && session.is_none()
            && !recovery
            && recovery_control.is_none()
        {
            active.clone()
        } else {
            vec![]
        };
        // Sync keeps a bounded continuation list, not several compiled modules.
        // Its original request completes only after every selected session.
        let remaining_sessions = sessions.split_off(sessions.len().min(1));
        let canceled = Arc::new(Canceled::default());
        let worker_canceled = canceled.clone();
        // Cancellation reaches this worker through the published transport,
        // which it may not have opened yet. Carry the signal too, so it neither
        // replaces a shut-down transport nor waits out an exchange that has no
        // transport to close.
        remote.set_cancellation(Some(canceled.clone()));
        let (send, receiver) = mpsc::sync_channel(1);
        let completion_wake = self.completion.wake.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();

        std::thread::spawn(move || {
            // Locals drop in reverse order: even on panic the result sender
            // closes before its notification wakes the owner.
            let _notify = NotifyCompletion(completion_wake, worker_ready);
            let send = send;
            let result = NetworkTask {
                remote,
                store,
                runtime,
                path,
                prepared,
                sessions,
                active,
                canceled: worker_canceled,
            }
            .run(work);
            let _ = send.send(result);
            drop(send);
            drop(_notify);
        });
        self.background = Some(Transfer {
            ready,
            receiver,
            remaining_sessions,
            cancel,
            session,
            canceled,
            disconnected: false,
            enrolled,
            replacement_required,
            email,
            recovery_control,
            developer: is_developer,
            enrollment: is_enrollment,
            diagnostic: is_diagnostic,
            diagnostic_read,
        });
        self.sample_gauges();
        self.network_started(selection_only)
    }
    fn prepare_join(&mut self, session: Option<&str>) -> Result<Option<Session>> {
        // Only local work runs on the owner. A link whose envelope has not been
        // read yet is read by the network worker, which then adopts it.
        Ok(if let Some(id) = session {
            if self.pending_join.as_deref() == Some(id) {
                self.pending_join = None;
            }
            self.services.store.pause_invitation(id, false)?;
            if self.services.store.is_local_session(id)? {
                self.stop_invitation_watch(id);
                return Ok(Some(self.services.store.session(id)?));
            }
            let received = self.services.store.pending_invitation_preview(id)?.received.is_some();
            let session =
                if received { Some(self.services.store.import_invitation(id)?) } else { None };
            self.stop_invitation_watch(id);
            session
        } else {
            None
        })
    }
    fn network_started(&mut self, selection_only: bool) -> Result<Reply> {
        let transfer = self
            .background
            .as_ref()
            .expect("background operation is installed before reporting its state");
        // Reads belong to their caller. Looking up recovery state must not
        // erase a failed account bootstrap while reopening the sign-in view.
        if !selection_only
            && !transfer.diagnostic
            && !transfer.diagnostic_read
            && transfer.recovery_control != Some(recovery::control::Kind::Status)
        {
            self.transfer_result = TransferStatus {
                kind: TransferKind::Running,
                session: transfer.session.clone(),
                message: Some(
                    (if transfer.developer {
                        "Updating development state…"
                    } else if transfer.enrollment {
                        "Waiting for sign-in…"
                    } else if transfer.recovery_control.is_some() {
                        "Updating recovery…"
                    } else if transfer.session.is_some() {
                        "Downloading app and synchronizing shared data…"
                    } else {
                        "Synchronizing…"
                    })
                    .into(),
                ),
                ..Default::default()
            };
        }
        let availability = self.availability()?;
        let changed = availability != self.availability_cache;
        self.availability_cache = availability;
        if (!transfer.diagnostic_read) || changed {
            self.state_changed();
        }
        Ok(Reply::Transfer { transfer: self.transfer_status() })
    }
    pub(crate) fn cancel_invitation_transfer(&mut self, session: &str) -> Result<()> {
        self.finish_join_open(session);
        if self.pending_join.as_deref() == Some(session) {
            self.pending_join = None;
            self.services.store.pause_invitation(session, true)?;
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                session: Some(session.into()),
                ..Default::default()
            };
            self.state_changed();
            return Ok(());
        }
        if let Some(transfer) = self.background.as_ref()
            && transfer.session.as_deref() == Some(session)
        {
            if self.synchronization.is_some() {
                self.services.store.pause_invitation(session, true)?;
                transfer.canceled.raise();
                self.transfer_result = TransferStatus {
                    kind: TransferKind::Canceling,
                    session: Some(session.into()),
                    ..Default::default()
                };
                self.state_changed();
            } else {
                self.cancel_transfer()?;
            }
        }
        Ok(())
    }
    pub(super) fn disconnect_network(&mut self) -> Result<()> {
        self.joined_for_open = None;
        if let Some(session) = self.pending_join.take() {
            self.services.store.pause_invitation(&session, true)?;
        }
        self.bootstrap_pending = false;
        if self.synchronization.take().is_some() {
            // A foreground app may have completed while the original Sync
            // still owns this continuation. Disconnect cancels that caller too.
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some("Synchronization disconnected; local data is retained".into()),
                ..Default::default()
            };
            self.state_changed();
        }
        if let Some(remote) = &mut self.remote {
            remote.disconnect();
        } else {
            if let Some(work) = &mut self.background {
                work.disconnected = true;
            }
            self.cancel_transfer()?;
        }
        let hostname = self.network.connection_status.snapshot().hostname;
        self.network.connection_status.set(
            reply::ApConnectionState::Offline,
            &hostname,
            &self.completion.wake,
        );
        self.availability_cache = self.availability()?;
        if self.background.is_none()
            && matches!(self.transfer_result.kind, TransferKind::Running | TransferKind::Canceling)
        {
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some("Synchronization disconnected; local data is retained".into()),
                ..Default::default()
            };
        }
        Ok(())
    }

    pub(super) fn set_work_offline(&mut self, enabled: bool) -> Result<()> {
        if self.network.connection_status.offline() == enabled {
            return Ok(());
        }
        // Persist before changing runtime policy; a failed save leaves the
        // current mode intact. Every process reopening this profile sees it.
        self.services.store.set_work_offline(enabled)?;
        self.network.connection_status.set_offline(enabled, &self.completion.wake);
        if enabled {
            self.disconnect_network()?;
            self.network.progress.set(None, &self.completion.wake);
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some("Offline mode; local data is retained".into()),
                ..Default::default()
            };
        } else {
            self.bootstrap_pending =
                self.enrolled() || self.background.as_ref().is_some_and(|work| work.enrollment);
            if self.bootstrap_pending
                && let Some(remote) = &self.remote
            {
                remote.begin_connection();
            }
        }
        self.availability_cache = self.availability()?;
        Ok(())
    }
    /// Bounded teardown for a process about to exit. A background worker may
    /// own `Remote`, and dropping this Engine can only flush the AP close once
    /// the worker has handed it back: shut that worker's transport down (what
    /// a cancellation does, minus the durable pause a user's cancel records)
    /// and wait for its result. Errs when such a worker still owns `Remote`
    /// at the deadline; the caller exits regardless.
    ///
    /// Enrollment is the one exchange never cut, since the AP may already
    /// have issued credentials this device has not saved. It is waited for
    /// and collected, so that its account binding is durable too, and a
    /// deadline reached with it unfinished is reported as `EnrollmentPending`
    /// rather than an error: the caller is expected to stay alive and ask
    /// again, not to exit under it.
    pub fn shutdown(&mut self, bound: std::time::Duration) -> Result<Shutdown> {
        if self.completion.socket().is_none() {
            return Err(
                "host callback owners must drive poll_shutdown from their event loop".into()
            );
        }
        let deadline = std::time::Instant::now() + bound;
        loop {
            if self.poll_shutdown()? {
                return Ok(Shutdown::Done);
            }
            let socket = self.completion.socket().expect("Unix shutdown checked its delivery");
            if !crate::engine::readiness::wait_readable(socket, deadline)? {
                #[cfg(all(not(target_os = "ios"), feature = "process"))]
                if self.service_executor.pending() {
                    return Ok(Shutdown::AcceptedWorkPending);
                }
                if self.background.as_ref().is_some_and(|work| work.enrollment) {
                    return Ok(Shutdown::EnrollmentPending);
                }
                return Err("background network work did not return before exit".into());
            }
        }
    }
    /// Begin/advance network shutdown without blocking the host event loop.
    /// False means keep the Engine alive and call again after a completion wake.
    /// Enrollment is collected durably before true; its transport is never cut.
    /// True means no network worker owns Remote. This is not a guest-worker join.
    pub fn poll_shutdown(&mut self) -> Result<bool> {
        let enrollment = self.background.as_ref().is_some_and(|transfer| transfer.enrollment);
        if enrollment {
            self.background.as_ref().expect("enrollment owns a transfer").canceled.raise();
        }
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        let collect = enrollment || self.service_executor.pending();
        #[cfg(not(all(not(target_os = "ios"), feature = "process")))]
        let collect = enrollment;
        // Drain before checking any work mailbox. A completion published after
        // that check must retain its notification for the next shutdown turn.
        if collect {
            self.complete_background()?;
        } else {
            self.completion.drain()?;
        }
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        let service_pending = {
            self.maintain_service()?;
            self.service_executor.pending()
        };
        #[cfg(not(all(not(target_os = "ios"), feature = "process")))]
        let service_pending = false;
        let Some(transfer) = &self.background else {
            self.synchronization = None;
            return Ok(!service_pending);
        };
        if transfer.enrollment {
            return Ok(false);
        }
        if transfer.ready.load(Ordering::Acquire) {
            return Ok(!service_pending);
        }
        // Accepted data effects reach their own terminal result. Ordinary
        // background work can be cut to release the lane for queued effects.
        if !transfer.canceled.raised() {
            transfer.canceled.raise();
            if let Some(connection) =
                transfer.cancel.lock().map_err(|_| "connection lock poisoned")?.as_ref()
            {
                connection.shutdown();
            }
        }
        Ok(false)
    }
    // Opening clients follow StartJoin with TransferStatus, then PrepareSession.
    // Keep that handoff available while World initialization reports its own
    // progress and failures through the account status and explicit Sync ticket.
    pub(super) fn opening_transfer_status(&self) -> TransferStatus {
        if let Some(session) = &self.joined_for_open {
            return TransferStatus {
                kind: TransferKind::Complete,
                session: Some(session.clone()),
                ..Default::default()
            };
        }
        self.transfer_status()
    }

    fn finish_join_open(&mut self, session: &str) {
        if self.joined_for_open.as_deref() == Some(session) {
            self.joined_for_open = None;
            self.state_changed();
        }
    }

    pub(super) fn transfer_status(&self) -> TransferStatus {
        if self.network.connection_status.offline() {
            return TransferStatus {
                kind: TransferKind::Paused,
                message: Some("Offline mode; local data is retained".into()),
                ..Default::default()
            };
        }
        let mut transfer = self.pending_join.as_ref().map_or_else(
            || self.transfer_result.clone(),
            |session| TransferStatus {
                kind: TransferKind::Running,
                session: Some(session.clone()),
                message: Some("Waiting to open the app…".into()),
                ..Default::default()
            },
        );
        if transfer.kind == TransferKind::Running
            && self.background.as_ref().is_some_and(|work| !work.reads_app_display())
            && let Some(progress) = self.network.progress.snapshot()
        {
            transfer.message = Some(progress.message());
            transfer.progress = Some(progress);
        }
        transfer
    }
    pub(super) fn cancel_transfer(&mut self) -> Result<()> {
        self.joined_for_open = None;
        if let Some(session) = self.pending_join.clone() {
            return self.cancel_invitation_transfer(&session);
        }
        if self.synchronization.is_some()
            && let Some(session) = self.background.as_ref().and_then(|work| work.session.clone())
        {
            return self.cancel_invitation_transfer(&session);
        }
        if self.background.is_none() && self.synchronization.take().is_some() {
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some("Synchronization paused; local data is retained".into()),
                ..Default::default()
            };
            self.state_changed();
            return Ok(());
        }
        if let Some(transfer) = &mut self.background {
            transfer.canceled.raise();
            if let Some(connection) =
                transfer.cancel.lock().map_err(|_| "connection lock poisoned")?.as_ref()
            {
                connection.shutdown();
            }
            if let Some(session) = &transfer.session {
                self.services.store.pause_invitation(session, true)?;
            }
            if transfer.reads_app_display() {
                return Ok(());
            }
            self.transfer_result = TransferStatus {
                kind: TransferKind::Canceling,
                message: Some(
                    ("Canceling; your invitation and saved data are retained").to_string(),
                ),
                ..Default::default()
            };
            self.availability_cache = self.availability()?;
            self.state_changed();
        }
        Ok(())
    }
    /// Register this socket with the platform owner. It signals result changes,
    /// never periodic freshness. A clone refers to the same notification stream.
    /// Callback-owned engines return None.
    pub fn completion_socket(&self) -> Option<&UnixStream> {
        self.completion.socket()
    }
    /// The write end of that stream, for wakes from contexts that can only
    /// make a raw `write`, such as a signal handler. One byte written here
    /// returns the owner from its current `run_once` (as a spurious completion).
    /// Callback-owned engines return None.
    pub fn wake_fd(&self) -> Option<std::os::fd::RawFd> {
        self.completion.wake.socket().map(std::os::fd::AsRawFd::as_raw_fd)
    }

    /// Called by the owning worker after notification readiness, never by an
    /// incidental request. Local transaction/runtime ownership stays here.
    pub fn complete_background(&mut self) -> Result<()> {
        let had_background = self.background.is_some();
        let result = self.complete_background_inner();
        if had_background && self.background.is_none() && self.network.connection_status.offline() {
            self.bootstrap_pending = false;
            self.synchronization = None;
            self.network.progress.set(None, &self.completion.wake);
            if let Some(remote) = &mut self.remote {
                remote.disconnect();
            }
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some("Offline mode; local data is retained".into()),
                ..Default::default()
            };
        }
        self.sample_gauges();
        result
    }
    fn complete_background_inner(&mut self) -> Result<()> {
        // A host may have consumed the readiness byte before calling us, or a
        // received replacement may be waiting for an admitted response. The
        // published completion flag remains authoritative until collection.
        let mut notified = self.background_ready();
        notified |= self.completion.drain()?;
        if !notified {
            return Ok(());
        }
        let _timing = crate::trace::Span::new("engine.background_completion");
        self.collect_invitation();
        if self.network_request_waiter.as_ref().is_some_and(|waiter| waiter.load(Ordering::Acquire))
        {
            self.network_request_waiter = None;
            self.network_response = None;
        }
        // An incoming-data failure must not consume the worker's only wakeup
        // before its result and Remote ownership have been collected.
        let incoming_result = self.apply_received_changes();
        let Some(transfer) = &mut self.background else {
            return incoming_result;
        };

        let received = transfer.receiver.try_recv();
        let (remote, result, updates) = match received {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return incoming_result,
            Err(error) => {
                let message = match error {
                    mpsc::TryRecvError::Disconnected => {
                        "The background worker stopped without a result. Your local data is retained; restart Playbit to reconnect."
                    }
                    mpsc::TryRecvError::Empty => {
                        "The background worker signaled completion without a result. Your local data is retained; restart Playbit to reconnect."
                    }
                };
                if (transfer.recovery_control.is_some()
                    || transfer.developer
                    || transfer.enrollment
                    || transfer.diagnostic)
                    && self.network_request_waiter.is_some()
                {
                    self.network_response = Some(Err(message.into()));
                }
                self.background = None;
                self.synchronization = None;
                self.network.progress.set(None, &self.completion.wake);
                self.transfer_result = TransferStatus {
                    kind: TransferKind::Failed,
                    message: Some(message.to_string()),
                    ..Default::default()
                };
                self.state_changed();
                return Err(message.into());
            }
        };
        let transfer = self
            .background
            .as_ref()
            .expect("background operation was installed before reporting its state");
        let canceled = transfer.canceled.raised();
        self.complete_network_result(remote, result, updates, canceled, incoming_result)
    }

    fn complete_enrollment(
        &mut self,
        result: std::result::Result<NetworkResult, String>,
        canceled: bool,
        disconnected: bool,
    ) -> Result<()> {
        let result = result.and_then(|result| {
            (|| -> Result<Reply> {
                match result {
                    #[cfg(target_vendor = "apple")]
                    NetworkResult::Enrollment(enrollment::Outcome::Backup(status)) => {
                        Ok(Reply::AccountKeyBackup { account_key_backup: status })
                    }
                    #[cfg(target_vendor = "apple")]
                    NetworkResult::Enrollment(enrollment::Outcome::SavedAccount(saved_account)) => {
                        if canceled || disconnected {
                            return Err("Saved account lookup canceled".into());
                        }
                        Ok(Reply::SavedAccount { saved_account })
                    }
                    NetworkResult::Enrollment(enrollment::Outcome::Available) => {
                        if canceled {
                            return Err("Sign-in preparation canceled".into());
                        }
                        Ok(Reply::SignInAvailable { sign_in_available: true })
                    }
                    NetworkResult::Enrollment(enrollment::Outcome::CodeSent) => {
                        if canceled {
                            return Err(
                                "Email request canceled; a sign-in code may still arrive".into()
                            );
                        }
                        Ok(Reply::CodeSent { code_sent: true })
                    }
                    NetworkResult::Enrollment(enrollment::Outcome::SignedIn {
                        account,
                        agreement,
                        vault,
                    }) => {
                        if self.services.store.account_replacement_required(account)? {
                            self.remote
                                .as_mut()
                                .ok_or("Enrollment network owner is unavailable")?
                                .require_replacement();
                            return Ok(Reply::SignedIn(SignInReply {
                                replacement_required: true,
                                signed_in: false,
                                recovery: None,
                                recovery_error: None,
                            }));
                        }
                        if account != self.services.store.account().id {
                            self.services.bind_authenticated_account(account)?;
                        }
                        if let Some(recorded) = agreement {
                            self.services.store.set_agreement_required(!recorded)?;
                        }
                        // A late successful enrollment still owns its durable
                        // credentials. Cancellation must not pretend to undo it.
                        self.bootstrap_pending = !self.network.connection_status.offline()
                            && ((!canceled && !disconnected) || self.bootstrap_pending);
                        let mut response = SignInReply {
                            replacement_required: false,
                            signed_in: true,
                            recovery: None,
                            recovery_error: None,
                        };
                        if let Some(vault) = vault {
                            match vault.and_then(|vault| {
                                recovery::after_sign_in(&self.services.store, vault).map_err(
                                    |error| {
                                        super::failure::message(&self.services.path, error.as_ref())
                                    },
                                )
                            }) {
                                Ok(state) => response.recovery = Some(state),
                                Err(error) => response.recovery_error = Some(error),
                            }
                        }
                        Ok(Reply::SignedIn(response))
                    }
                    _ => Err("missing enrollment result".into()),
                }
            })()
            .map_err(|error| super::failure::message(&self.services.path, error.as_ref()))
        });
        self.transfer_result = match &result {
            Ok(_) => TransferStatus { kind: TransferKind::Complete, ..Default::default() },
            Err(error) => TransferStatus {
                kind: TransferKind::Failed,
                message: Some(error.to_string()),
                ..Default::default()
            },
        };
        if self.network_request_waiter.is_some() {
            self.network_response = Some(result);
        }
        self.availability_cache = self.availability()?;
        self.resume_invitation_watch()?;
        Ok(())
    }

    fn complete_developer(
        &mut self,
        result: std::result::Result<NetworkResult, String>,
        updates: BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
        canceled: bool,
        diagnostic_read: bool,
        paused_intent: Option<bool>,
    ) -> Result<()> {
        let result = result.and_then(|result| match result {
            NetworkResult::Developer(mut outcome) => {
                for (id, prepared) in updates {
                    if canceled {
                        self.updates.insert(
                            id,
                            UpdateStatus {
                                kind: UpdateKind::Paused,
                                build_id: None,
                                message:
                                    "Local update canceled; publication may already have completed"
                                        .into(),
                            },
                        );
                    } else {
                        self.finish_update(&id, prepared);
                    }
                }
                if let reply::DeveloperReply::Updated {
                    session, update, resolved_build_id, ..
                } = &mut outcome.value
                {
                    // Publication and local adoption have distinct outcomes.
                    *update = self.updates.get(session).cloned();
                    *resolved_build_id = Some(
                        self.services
                            .store
                            .session(session)
                            .map_err(|error| super::failure::message(&self.services.path, &error))?
                            .resolved_build_id,
                    );
                }
                Ok(Reply::Developer(outcome.value))
            }
            _ => Err("missing developer result".into()),
        });
        if !diagnostic_read {
            self.transfer_result = match &result {
                Ok(_) if canceled => TransferStatus {
                    kind: TransferKind::Paused,
                    message: Some(
                        "Local update canceled; publication may already have completed".into(),
                    ),
                    ..Default::default()
                },
                Ok(_) => TransferStatus { kind: TransferKind::Complete, ..Default::default() },
                Err(error) => TransferStatus {
                    kind: TransferKind::Failed,
                    message: Some(error.to_string()),
                    ..Default::default()
                },
            };
        }
        if self.network_request_waiter.is_some() {
            self.network_response = Some(result);
        }
        if let Some(bootstrap) = paused_intent {
            self.bootstrap_pending = bootstrap;
        }
        Ok(())
    }

    fn complete_diagnostic(
        &mut self,
        result: std::result::Result<NetworkResult, String>,
        paused_intent: Option<bool>,
    ) -> Result<()> {
        let result = result.and_then(|result| match result {
            NetworkResult::Diagnostic(value) => Ok(Reply::Diagnostic(*value)),
            _ => Err("missing diagnostic result".into()),
        });
        if self.network_request_waiter.is_some() {
            self.network_response = Some(result);
        }
        if let Some(bootstrap) = paused_intent {
            self.bootstrap_pending = bootstrap;
        }
        Ok(())
    }

    fn complete_recovery_control(
        &mut self,
        result: std::result::Result<NetworkResult, String>,
        paused_intent: Option<bool>,
        kind: recovery::control::Kind,
    ) -> Result<()> {
        let result = result
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })
            .and_then(|result| match result {
                NetworkResult::Control(outcome) => self.apply_recovery_control(outcome),
                _ => Err("missing recovery control result".into()),
            })
            .map_err(|error| super::failure::message(&self.services.path, error.as_ref()));
        if kind != recovery::control::Kind::Status {
            self.transfer_result = match &result {
                Ok(Reply::Recovery(reply::RecoveryStatus::Canceled)) => TransferStatus {
                    kind: TransferKind::Paused,
                    message: Some(("Recovery canceled; saved data is retained").to_string()),
                    ..Default::default()
                },
                Ok(_) => TransferStatus { kind: TransferKind::Complete, ..Default::default() },
                Err(error) => TransferStatus {
                    kind: TransferKind::Failed,
                    message: Some({
                        let mut end = error.len().min(512);
                        while !error.is_char_boundary(end) {
                            end -= 1;
                        }
                        error[..end].to_owned()
                    }),
                    ..Default::default()
                },
            };
        }
        if self.network_request_waiter.is_some() {
            self.network_response = Some(result);
        }
        if let Some(bootstrap) = paused_intent {
            self.bootstrap_pending = bootstrap;
        }
        self.availability_cache = self.availability()?;
        Ok(())
    }

    fn complete_synchronization(
        &mut self,
        transfer: Transfer,
        result: std::result::Result<NetworkResult, String>,
        updates: BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
        incoming_result: Result<()>,
    ) -> Result<()> {
        if transfer.canceled.raised() {
            self.transfer_result = TransferStatus {
                kind: TransferKind::Paused,
                message: Some(("Paused. Resume when you’re ready").to_string()),
                ..Default::default()
            };
            return incoming_result;
        }
        if let Ok(NetworkResult::SynchronizationPending(continuation)) = result {
            self.synchronization = Some(continuation);
            return incoming_result;
        }
        self.finish_synchronization_updates(&transfer, &result, updates);
        if let Ok(NetworkResult::InitializationPending(message)) = result {
            // Initial source readiness already has a finite retry episode.
            // Keep it in progress until success or exhausted recovery.
            self.transfer_result = TransferStatus {
                kind: TransferKind::Running,
                message: Some(message),
                ..Default::default()
            };
            return incoming_result;
        }
        if result.is_ok() && !transfer.remaining_sessions.is_empty() {
            let remaining = transfer
                .remaining_sessions
                .into_iter()
                .filter(|id| self.active_sessions().contains(id))
                .collect::<Vec<_>>();
            if !remaining.is_empty() {
                self.begin_network_work(NetworkWork::SynchronizeSelections(remaining))?;
                return incoming_result;
            }
        }
        let result = result
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })
            .and_then(|result| {
                self.finish_synchronized_session(
                    &transfer.session,
                    !matches!(result, NetworkResult::OwnerSessionPrepared),
                )
            });
        self.transfer_result = self.synchronization_status(result, &transfer.session)?;
        incoming_result
    }

    fn finish_synchronization_updates(
        &mut self,
        transfer: &Transfer,
        result: &std::result::Result<NetworkResult, String>,
        updates: BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
    ) {
        let _timing = crate::trace::Span::new("engine.background_updates");
        for (id, prepared) in updates {
            // Explicit metadata checks are valid while an app is closed.
            // Automatic selection delivery still belongs to active owners.
            if matches!(result, Ok(NetworkResult::CheckedUpdate))
                || transfer.session.as_ref() == Some(&id)
                || self.active_sessions().contains(&id)
            {
                self.finish_update(&id, prepared);
            }
        }
    }

    fn finish_synchronized_session(
        &mut self,
        session: &Option<String>,
        complete_invitation: bool,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("engine.background_refresh");
        if let Some(session) = session {
            if let Some(update) = self.updates.get(session) {
                return Err(update.message.clone().into());
            }
            let retained = self.services.store.session(session)?;
            self.services.store.admit_binding(
                &Access::session_version(
                    self.services.store.account().id,
                    session,
                    &retained.resolved_build_id,
                ),
                self.session_admission_volume(&retained)?,
            )?;
            if complete_invitation {
                self.services.store.complete_invitation(session)?;
                self.stop_invitation_watch(session);
                if self.pending_join.is_none() {
                    self.joined_for_open = Some(session.clone());
                }
            }
        }
        Ok(())
    }

    fn synchronization_status(
        &mut self,
        result: Result<()>,
        session: &Option<String>,
    ) -> Result<TransferStatus> {
        Ok(match result {
            Ok(()) => {
                self.availability_cache = self.availability()?;
                TransferStatus {
                    kind: TransferKind::Complete,
                    session: session.clone(),
                    ..Default::default()
                }
            }
            Err(_)
                if session.is_none()
                    && self.remote.as_ref().is_some_and(|remote| {
                        remote.reconnect_after(std::time::Instant::now()).is_some()
                    }) =>
            {
                // Establishment retained a bounded retry. Keep ordinary
                // offline recovery visible without presenting it as a failure.
                TransferStatus {
                    kind: TransferKind::Running,
                    message: Some(("Reconnecting…").to_string()),
                    ..Default::default()
                }
            }
            Err(error) => {
                if let Some(id) = session {
                    // Unrelated synchronization may replace the global transfer
                    // result. Retain this session's failure until a retry adopts
                    // a ready candidate, just like consent/validation failures.
                    self.updates.entry(id.clone()).or_insert_with(|| UpdateStatus {
                        kind: UpdateKind::Failed,
                        build_id: None,
                        message: error.to_string(),
                    });
                }
                TransferStatus {
                    kind: TransferKind::Failed,
                    progress: None,
                    session: session.clone(),
                    message: Some(super::failure::message(&self.services.path, error.as_ref())),
                    update: session.as_ref().and_then(|id| self.updates.get(id)).cloned(),
                }
            }
        })
    }

    fn complete_network_result(
        &mut self,
        remote: remote::Remote,
        result: std::result::Result<NetworkResult, String>,
        updates: BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
        canceled: bool,
        incoming_result: Result<()>,
    ) -> Result<()> {
        let mut remote = remote;
        let transfer =
            self.background.take().expect("background completion has an active operation");
        self.network.progress.set(None, &self.completion.wake);
        // This operation is over; its cancellation no longer governs Remote.
        remote.set_cancellation(None);
        // Completion may partially change local state before reporting an
        // error. The owner publishes one cursor after this turn returns.
        if !transfer.diagnostic_read {
            self.state_changed();
        }
        let interleaved_join = self.synchronization.is_some() && transfer.session.is_some();
        if transfer.canceled.raised() && !interleaved_join {
            remote.disconnect();
            if !transfer.disconnected && transfer.recovery_control.is_some() {
                // Canceling this control wait closed the AP transport. Resume
                // account replication after cancellation work, without pb sync.
                self.bootstrap_pending = true;
            }
        }
        // An old result must not resume work paused by Disconnect. Preserve
        // any explicit bootstrap intent admitted after that Disconnect;
        // only flags introduced by this late result are suppressed.
        let paused_intent = transfer.disconnected.then_some(self.bootstrap_pending);
        self.remote = Some(remote);
        self.sample_gauges();
        if transfer.enrollment {
            self.complete_enrollment(result, canceled, transfer.disconnected)?;
            return incoming_result;
        }
        // The completion was observed while Remote still belonged to the
        // worker. Publish its returned health to cached frames on this same
        // event, including failures and cancellation with no later input.
        let availability = self.availability()?;
        if availability != self.availability_cache {
            self.availability_cache = availability;
            self.state_changed();
        }
        // Every returned connection must resume an invitation observer which
        // was opened while the worker owned Remote, including data operations.
        self.resume_invitation_watch()?;
        if matches!(&result, Ok(NetworkResult::SelectionUpdates)) {
            if transfer.canceled.raised() {
                return incoming_result;
            }
            for (id, prepared) in updates {
                if self.active_sessions().contains(&id) {
                    self.finish_update(&id, prepared);
                }
            }
            return incoming_result;
        }
        if transfer.developer {
            self.complete_developer(
                result,
                updates,
                canceled,
                transfer.diagnostic_read,
                paused_intent,
            )?;
            return incoming_result;
        }
        if transfer.diagnostic {
            self.complete_diagnostic(result, paused_intent)?;
            return incoming_result;
        }
        if let Some(kind) = transfer.recovery_control {
            self.complete_recovery_control(result, paused_intent, kind)?;
            return incoming_result;
        }
        self.complete_synchronization(transfer, result, updates, incoming_result)
    }

    /// Incoming durable applies use the same owner wakeup while Remote is held
    /// by a transfer. Coalesced dirty flags survive until all executing guests
    /// have returned, so no database polling or incidental request is needed.
    fn apply_received_changes(&mut self) -> Result<()> {
        if self.services.store.take_session_activity_changed() {
            self.state_changed();
        }
        if self.network.builds.take_changed() {
            self.state_changed();
        }
        #[cfg(all(target_os = "macos", feature = "process"))]
        if self.provider_revision != self.services.store.delivery_provider_revision(None, None).0 {
            self.provider_revision = self.services.store.delivery_provider_revision(None, None).0;
            self.state_changed();
        }
        let account_ready = self.network.account_initialized();
        if account_ready != self.account_ready_cache {
            self.account_ready_cache = account_ready;
            self.state_changed();
        }
        if self.network.progress.take_changed() {
            self.state_changed();
        }
        if self.network.connection_status.take_changed() {
            self.state_changed();
        }
        let notices = self.network.received.drain();
        if let Some(observer) = &self.service_observer {
            observer(&notices);
        }
        if !notices.is_empty() {
            self.state_changed();
        }
        let _timing = crate::trace::Span::new("engine.received_refresh");
        // Publisher ACK/failure also signals this completion socket without
        // an incoming database notice. Read stable health, without consuming
        // the generation-tagged recovery notices owned by Remote.
        let availability = self.availability()?;
        if availability != self.availability_cache {
            self.availability_cache = availability;
            self.state_changed();
        }
        for notice in &notices {
            if matches!(
                notice.state,
                replication::ReplicaState::Gap
                    | replication::ReplicaState::Revoked
                    | replication::ReplicaState::Disconnected
                    | replication::ReplicaState::Failed
            ) {
                self.availability_cache = notice
                    .error
                    .clone()
                    .unwrap_or_else(|| "Live replication needs reconnection".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
pub(crate) mod tests {
    use super::*;
    use crate::engine::local_owner_test_support::LocalApps;
    use crate::engine::{admission::AdmissionQueue, ipc};
    use std::collections::BTreeSet;
    use std::os::unix::net::UnixListener;

    fn drain_updates(engine: &mut Engine) {
        drain_until(engine, |engine| engine.background.is_none());
    }
    #[allow(
        unsafe_code,
        reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
    )]
    fn drain_until(engine: &mut Engine, done: impl Fn(&Engine) -> bool) {
        use polling::{Event, Events, Poller};
        let socket =
            engine.completion_socket().expect("Unix engine completion").try_clone().unwrap();
        let poller = Poller::new().unwrap();
        unsafe {
            poller.add(&socket, Event::readable(1)).unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut events = Events::new();
        while !done(engine) {
            assert!(std::time::Instant::now() < deadline, "update did not complete");
            poller
                .wait(
                    &mut events,
                    Some(deadline.saturating_duration_since(std::time::Instant::now())),
                )
                .unwrap();
            engine.complete_background().unwrap();
            events.clear();
            poller.modify(&socket, Event::readable(1)).unwrap();
        }
        poller.delete(&socket).unwrap();
    }

    #[allow(
        clippy::type_complexity,
        reason = "The fixture exposes the actual transfer completion payload."
    )]
    pub(crate) fn transfer(
        engine: &mut Engine,
    ) -> (
        mpsc::SyncSender<(
            remote::Remote,
            std::result::Result<NetworkResult, String>,
            BTreeMap<String, std::result::Result<updates::PreparedUpdate, String>>,
        )>,
        remote::Remote,
    ) {
        let remote = engine.remote.take().unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        engine.background = Some(Transfer {
            ready: Arc::new(AtomicBool::new(false)),
            receiver,
            remaining_sessions: vec![],
            cancel: remote.cancel_handle(),
            session: None,
            canceled: Arc::new(Canceled::default()),
            disconnected: false,
            enrolled: remote.enrolled(),
            replacement_required: remote.replacement_required(),
            email: Some(remote.email().into()),
            recovery_control: None,
            developer: false,
            enrollment: false,
            diagnostic: false,
            diagnostic_read: false,
        });
        engine.transfer_result =
            TransferStatus { kind: TransferKind::Running, ..Default::default() };
        (sender, remote)
    }

    pub(crate) fn diagnostic_ticket(engine: &mut Engine) -> NetworkTicket {
        assert!(engine.background.is_some());
        engine.network_request_ticket(false)
    }

    pub(crate) fn notify(engine: &Engine) {
        drop(NotifyCompletion(
            engine.completion.wake.clone(),
            engine.background.as_ref().unwrap().ready.clone(),
        ));
    }

    #[test]
    fn repeated_preparation_waits_for_the_same_join() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Join", true).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (_sender, _remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().session = Some(session.id.clone());
        for _ in 0..2 {
            assert_eq!(
                serde_json::to_value(engine.prepare_session(&session.id).unwrap()).unwrap()["transfer"]
                    ["kind"],
                "running"
            );
        }
        assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
    }

    #[test]
    fn failed_join_remains_visible_after_unrelated_synchronization() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Join", true).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (sender, remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().session = Some(session.id.clone());
        sender.send((remote, Err("build content unavailable".into()), BTreeMap::new())).unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        let (sender, remote) = transfer(&mut engine);
        sender.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert_eq!(engine.transfer_status().kind, TransferKind::Complete);
        assert_eq!(
            serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap()["updates"]
                [&session.id]["message"],
            "build content unavailable"
        );
    }

    #[test]
    fn leaving_pending_open_cannot_restore_it_after_background_completion() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Leave", false).unwrap();
        let a = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let b = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let mut owners = LocalApps::default();
        let ib = owners.open(&mut engine, &b.id).unwrap();
        owners.settle();
        let c = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let ic = owners.open(&mut engine, &c.id).unwrap();
        owners.settle();
        engine
            .services
            .store
            .preview_invitation(
                &engine
                    .services
                    .store
                    .invitation(&a.id, crate::release_channel::ReleaseChannel::Public)
                    .unwrap(),
                crate::release_channel::ReleaseChannel::Public,
            )
            .unwrap();
        let (sender, remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().session = Some(a.id.clone());
        engine.dispatch_reply(Request::ForgetSession { session: a.id.clone() }).unwrap();
        assert!(engine.background.as_ref().unwrap().canceled.raised());
        assert!(engine.services.store.pending_invitations().unwrap().is_empty());
        sender.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert!(owners.apps.generation(ib).is_ok());
        assert!(owners.apps.generation(ic).is_ok());
        owners.quit(&mut engine, ib).unwrap();
        owners.quit(&mut engine, ic).unwrap();
        let visible: BTreeSet<_> =
            engine.services.store.retained_sessions().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(visible, BTreeSet::from([b.id, c.id]));
        assert_eq!(engine.services.store.session(&a.id).unwrap(), a);
    }

    #[test]
    fn completed_join_retires_its_observer_but_preserves_another_preview() {
        for observing_join in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = engine.services.store.create_volume("Join", true).unwrap();
            let session =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
            let mut owners = LocalApps::default();
            let instance = owners.open(&mut engine, &session.id).unwrap();
            owners.settle();
            owners.quit(&mut engine, instance).unwrap();
            let other =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
            let observed = if observing_join { &session.id } else { &other.id };
            for id in [&session.id, &other.id] {
                let invitation = engine
                    .services
                    .store
                    .invitation(id, crate::release_channel::ReleaseChannel::Public)
                    .unwrap();
                engine
                    .services
                    .store
                    .preview_invitation(&invitation, crate::release_channel::ReleaseChannel::Public)
                    .unwrap();
            }
            let (sender, remote) = transfer(&mut engine);
            engine.background.as_mut().unwrap().session = Some(session.id.clone());
            // A window can start observing after bootstrap has already begun.
            engine.watch_invitation(observed.clone(), false).unwrap();
            sender.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            assert_eq!(engine.transfer_status().kind, TransferKind::Complete);
            assert_eq!(engine.services.store.pending_invitations().unwrap().len(), 1);
            if observing_join {
                assert!(
                    engine.invitation_status().is_none(),
                    "completed joins cannot retain a pending-record observer"
                );
            } else {
                assert_eq!(engine.invitation_status().unwrap().session, other.id);
            }
        }
    }

    #[cfg(all(target_os = "macos", feature = "process"))]
    fn enrollment_provider(engine: &Engine) -> super::super::file_provider::Server {
        let path = engine.services.path.clone();
        let wake = engine.completion.wake.clone();
        super::super::file_provider::Server::start(
            &path,
            &engine.services,
            engine.network.blobs.clone(),
            move || wake.notify(),
        )
        .unwrap()
    }

    #[test]
    fn sign_in_returns_setup_before_bootstrap_and_keeps_authentication_on_vault_failure() {
        // Compose the real provider workers before enrollment, as Mac startup does.
        for vault_failure in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            #[cfg(all(target_os = "macos", feature = "process"))]
            let _provider = enrollment_provider(&engine);
            let account = Id::from_bytes([42; 16]);
            let (sender, remote) = transfer(&mut engine);
            engine.background.as_mut().unwrap().enrollment = true;
            let ticket = engine.network_request_ticket(true);
            let vault = if vault_failure {
                Err("vault unavailable".into())
            } else {
                Ok(recovery::Vault::parse(json!({"revision":0})).unwrap())
            };
            sender
                .send((
                    remote,
                    Ok(NetworkResult::Enrollment(enrollment::Outcome::SignedIn {
                        agreement: None,
                        account,
                        vault: Some(vault),
                    })),
                    BTreeMap::new(),
                ))
                .unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            let response =
                serde_json::to_value(engine.take_network_request(ticket).unwrap()).unwrap();
            assert_eq!(response["signed_in"], true);
            assert_eq!(engine.services.store.account().id, account);
            assert!(engine.bootstrap_pending, "sync remains scheduled independently");
            if vault_failure {
                assert_eq!(response["recovery_error"], "vault unavailable");
                assert!(response.get("recovery").is_none());
                assert!(engine.services.store.recovery_state().unwrap().is_none());
            } else {
                assert_eq!(response["recovery"]["kind"], "setup");
                assert_eq!(response["recovery"]["secret"].as_str().unwrap().len(), 23);
                assert!(!engine.services.store.recovery_state().unwrap().unwrap().acknowledged);
                assert!(
                    serde_json::to_value(engine.dispatch_reply(Request::Status).unwrap()).unwrap()
                        ["recovery"]
                        .is_null()
                );
            }
        }
    }

    #[test]
    fn enrollment_late_results_preserve_account_binding_and_disconnect_intent() {
        // Late authentication must bind durably even with provider workers alive.
        for signed_in in [false, true] {
            for disconnected in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut engine = Engine::open(directory.path()).unwrap();
                #[cfg(all(target_os = "macos", feature = "process"))]
                let provider = enrollment_provider(&engine);
                let account = Id::from_bytes([42; 16]);
                let (sender, remote) = transfer(&mut engine);
                engine.background.as_mut().unwrap().enrollment = true;
                let ticket = engine.network_request_ticket(true);
                if disconnected {
                    engine.disconnect_network().unwrap();
                } else {
                    engine.cancel_transfer().unwrap();
                }
                let outcome = if signed_in {
                    enrollment::Outcome::SignedIn { account, agreement: None, vault: None }
                } else {
                    enrollment::Outcome::CodeSent
                };
                sender
                    .send((remote, Ok(NetworkResult::Enrollment(outcome)), BTreeMap::new()))
                    .unwrap();
                notify(&engine);
                engine.complete_background().unwrap();
                let result = engine.take_network_request(ticket);
                if signed_in {
                    assert_eq!(serde_json::to_value(result.unwrap()).unwrap()["signed_in"], true);
                    assert_eq!(engine.services.store.account().id, account);
                } else {
                    assert!(result.unwrap_err().to_string().contains("may still arrive"));
                }
                assert!(
                    !engine.bootstrap_pending,
                    "late completion cannot undo explicit cancellation"
                );
                assert!(engine.background.is_none());
                #[cfg(all(target_os = "macos", feature = "process"))]
                drop(provider);
                drop(engine);
                if signed_in {
                    assert_eq!(LocalStore::open(directory.path()).unwrap().account().id, account);
                }
            }
        }
    }

    #[test]
    fn enrollment_cannot_rebind_retained_data_or_a_running_guest() {
        // Provider leases do not authorize replacing retained data or guest owners.
        for running in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            #[cfg(all(target_os = "macos", feature = "process"))]
            let _provider = enrollment_provider(&engine);
            let original = engine.services.store.account().id;
            let root = engine.services.store.create_volume("Retained", true).unwrap();
            let session =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
            let mut owners = LocalApps::default();
            let instance = running.then(|| owners.open(&mut engine, &session.id).unwrap());
            owners.settle();
            let (sender, remote) = transfer(&mut engine);
            engine.background.as_mut().unwrap().enrollment = true;
            let ticket = engine.network_request_ticket(true);
            sender
                .send((
                    remote,
                    Ok(NetworkResult::Enrollment(enrollment::Outcome::SignedIn {
                        agreement: None,
                        account: Id::from_bytes([42; 16]),
                        vault: None,
                    })),
                    BTreeMap::new(),
                ))
                .unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            let Reply::SignedIn(result) = engine.take_network_request(ticket).unwrap() else {
                panic!("replacement decision");
            };
            assert!(result.replacement_required);
            assert!(!result.signed_in);
            assert!(!engine.enrolled());
            assert_eq!(engine.services.store.account().id, original);
            assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
            if let Some(id) = instance {
                owners.quit(&mut engine, id).unwrap();
            }
        }
    }

    #[test]
    fn canceled_developer_publication_does_not_initialize_or_adopt_a_late_candidate() {
        {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = engine.services.store.create_volume("Developer cancellation", true).unwrap();
            let old = engine
                .create_session(
                    crate::package::signature::testing::signed(
                        &engine.services.store,
                        crate::engine::tests::button_package(),
                    )
                    .unwrap(),
                    root.id,
                )
                .unwrap();
            let mut owners = LocalApps::default();
            let instance = owners.open(&mut engine, &old.id).unwrap();
            owners.settle();
            let generation = owners.apps.generation(instance).unwrap();
            let mut package = crate::engine::tests::button_package();
            package = package.renamed("Published candidate").unwrap();
            let candidate = engine
                .create_session(
                    crate::package::signature::testing::signed(&engine.services.store, package)
                        .unwrap(),
                    root.id,
                )
                .unwrap();
            let prepared = updates::prepare_candidate(
                &engine.runtime,
                &engine.services.store,
                &old.id,
                &engine.services.path.join(crate::paths::BUILDS),
                Some(json!({"resolved_build_id":candidate.resolved_build_id,
                    "selector":candidate.selector})),
            )
            .unwrap();
            let (sender, remote) = transfer(&mut engine);
            engine.background.as_mut().unwrap().developer = true;
            let ticket = engine.network_request_ticket(true);
            engine.cancel_transfer().unwrap();
            sender
                .send((
                    remote,
                    Ok(NetworkResult::Developer(developer::Outcome {
                        value: reply::DeveloperReply::Updated {
                            session: old.id.clone(),
                            build_id: candidate.resolved_build_id.clone(),
                            update: None,
                            resolved_build_id: None,
                        },
                    })),
                    BTreeMap::from([(old.id.clone(), Ok(prepared))]),
                ))
                .unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            assert!(
                engine.background.is_none(),
                "cancellation must not start candidate initialization"
            );
            drain_updates(&mut engine);
            assert_eq!(owners.apps.generation(instance).unwrap(), generation);
            owners.quit(&mut engine, instance).unwrap();
            assert_eq!(engine.services.store.session(&old.id).unwrap(), old);
            let outcome =
                serde_json::to_value(engine.take_network_request(ticket).unwrap()).unwrap();
            assert_eq!(
                outcome["build_id"], candidate.resolved_build_id,
                "cancellation does not undo AP publication"
            );
            assert_eq!(outcome["update"]["kind"], "paused");
            assert_eq!(outcome["resolved_build_id"], old.resolved_build_id);
        }
    }

    #[test]
    fn sync_continuations_skip_stopped_sessions_and_stop_on_cancellation() {
        for canceled in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = engine.services.store.create_volume("Root", true).unwrap();
            let session =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
            let mut owners = LocalApps::default();
            let instance = owners.open(&mut engine, &session.id).unwrap();
            owners.settle();
            let (send, remote) = transfer(&mut engine);
            engine.background.as_mut().unwrap().remaining_sessions = vec![session.id.clone()];
            if canceled {
                engine.cancel_transfer().unwrap();
            } else {
                owners.quit(&mut engine, instance).unwrap();
            }
            send.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            assert!(engine.background.is_none());
            assert!(engine.remote.is_some());
            assert_eq!(
                engine.transfer_result.kind,
                if canceled { TransferKind::Paused } else { TransferKind::Complete }
            );
        }
    }

    #[test]
    fn a_selection_completion_cannot_restart_a_stopped_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Root", true).unwrap();
        let session = engine
            .create_session(
                crate::package::signature::testing::signed(
                    &engine.services.store,
                    crate::engine::tests::button_package(),
                )
                .unwrap(),
                root.id,
            )
            .unwrap();
        let mut owners = LocalApps::default();
        let instance = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        let mut package = crate::engine::tests::button_package();
        package = package.renamed("Candidate").unwrap();
        let candidate = engine
            .create_session(
                crate::package::signature::testing::signed(&engine.services.store, package)
                    .unwrap(),
                root.id,
            )
            .unwrap();
        let (send, remote) = transfer(&mut engine);
        owners.quit(&mut engine, instance).unwrap();
        let mut updates = BTreeMap::new();
        updates.insert(
            session.id.clone(),
            Ok(updates::prepare_candidate(
                &engine.runtime,
                &engine.services.store,
                &session.id,
                &engine.services.path.join(crate::paths::BUILDS),
                Some(json!({
                    "selector":BuildSelector::Exact {build_id:candidate.resolved_build_id.clone()},
                    "resolved_build_id":candidate.resolved_build_id,
                })),
            )
            .unwrap()),
        );
        send.send((remote, Ok(NetworkResult::SelectionUpdates), updates)).unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
        assert!(!engine.updates.contains_key(&session.id));
    }

    #[test]
    fn explicit_closed_app_update_adopts_metadata_without_starting_or_replaying_a_guest() {
        for explicit in [false, true] {
            for canceled in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut engine = Engine::open(directory.path()).unwrap();
                let root = engine.services.store.create_volume("Closed app", true).unwrap();
                let session = engine
                    .create_session(
                        crate::package::signature::testing::signed(
                            &engine.services.store,
                            crate::engine::tests::button_package(),
                        )
                        .unwrap(),
                        root.id,
                    )
                    .unwrap();
                let mut package = crate::engine::tests::button_package();
                package = package.renamed("Candidate").unwrap();
                let candidate = engine
                    .create_session(
                        crate::package::signature::testing::signed(&engine.services.store, package)
                            .unwrap(),
                        root.id,
                    )
                    .unwrap();
                let prepared = updates::prepare_candidate(
                    &engine.runtime, &engine.services.store, &session.id,
                    &engine.services.path.join(crate::paths::BUILDS),
                    Some(json!({
                        "selector": BuildSelector::Exact { build_id: candidate.resolved_build_id.clone() },
                        "resolved_build_id": candidate.resolved_build_id,
                    })),
                ).unwrap();
                let (send, remote) = transfer(&mut engine);
                if canceled {
                    engine.cancel_transfer().unwrap();
                }
                send.send((
                    remote,
                    Ok(if explicit {
                        NetworkResult::CheckedUpdate
                    } else {
                        NetworkResult::SelectionUpdates
                    }),
                    BTreeMap::from([(session.id.clone(), Ok(prepared))]),
                ))
                .unwrap();
                notify(&engine);
                engine.complete_background().unwrap();
                let selected = engine.services.store.session(&session.id).unwrap();
                assert_eq!(
                    selected.resolved_build_id,
                    if explicit && !canceled {
                        candidate.resolved_build_id
                    } else {
                        session.resolved_build_id
                    }
                );
                assert!(engine.active_sessions().is_empty());
            }
        }
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[test]
    fn selection_completion_adopts_for_a_claimed_gui_without_an_engine_vm() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("GUI selection", false).unwrap();
        let session = engine
            .create_session(
                crate::package::signature::testing::signed(
                    &engine.services.store,
                    crate::engine::tests::button_package(),
                )
                .unwrap(),
                root.id,
            )
            .unwrap();
        let (peer, _client) = std::os::unix::net::UnixStream::pair().unwrap();
        let Reply::Instance(instance) = engine
            .dispatch_instance(
                Request::InstanceReserve {
                    restore_host_mounts: false,
                    request: Id::generate().unwrap(),
                    incarnation: engine.session_instances.incarnation(),
                    session: session.id.clone(),
                    extra_volumes: Box::default(),
                    host_mounts: Box::default(),
                },
                &peer,
            )
            .unwrap()
        else {
            panic!("reserved GUI owner");
        };
        assert!(engine.active_sessions().is_empty(), "reservation cannot consume a watch");
        engine.dispatch_instance(Request::InstanceClaim { instance: instance.id }, &peer).unwrap();
        assert_eq!(engine.active_sessions(), vec![session.id.clone()]);
        let mut package = crate::engine::tests::button_package();
        package = package.renamed("Selected GUI update").unwrap();
        let target = engine
            .create_session(
                crate::package::signature::testing::signed(&engine.services.store, package)
                    .unwrap(),
                root.id,
            )
            .unwrap();
        let prepared = updates::prepare_candidate(
            &engine.runtime,
            &engine.services.store,
            &session.id,
            &engine.services.path.join(crate::paths::BUILDS),
            Some(json!({
                "selector":BuildSelector::Exact {build_id:target.resolved_build_id.clone()},
                "resolved_build_id":target.resolved_build_id,
            })),
        )
        .unwrap();
        let (send, remote) = transfer(&mut engine);
        send.send((
            remote,
            Ok(NetworkResult::SelectionUpdates),
            BTreeMap::from([(session.id.clone(), Ok(prepared))]),
        ))
        .unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert_eq!(
            engine.services.store.session(&session.id).unwrap().resolved_build_id,
            target.resolved_build_id
        );
        assert_eq!(
            engine.session_instances.descriptor(instance.id).unwrap().build,
            session.resolved_build_id
        );
    }

    #[test]
    fn canceling_a_queued_cold_open_keeps_unrelated_background_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Root", true).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        fs::remove_file(
            directory.path().join(crate::paths::BUILDS).join(&session.resolved_build_id),
        )
        .unwrap();
        let (_send, _remote) = transfer(&mut engine);
        let waiting = serde_json::to_value(engine.prepare_session(&session.id).unwrap()).unwrap();
        assert_eq!(waiting["transfer"]["session"], session.id);
        assert_eq!(engine.pending_join.as_ref(), Some(&session.id));
        engine.cancel_transfer().unwrap();
        assert!(engine.pending_join.is_none());
        assert!(engine.services.store.pending_invitation_paused(&session.id).unwrap());
        assert!(!engine.background.as_ref().unwrap().canceled.raised());
        assert_eq!(engine.transfer_status().kind, TransferKind::Paused);
    }

    #[test]
    fn sync_ticket_waits_through_yielded_units_and_an_interleaved_open_failure() {
        // A foreground app result must not complete or replace the original Sync reply.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Root", true).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (send, remote) = transfer(&mut engine);
        let ticket = engine.network_request_ticket(false);
        let state = remote::Synchronization::new(
            &engine.services.store,
            remote::ClientScope::FullClient,
            crate::trace::Interval::new("replication.sync"),
        )
        .unwrap();
        send.send((remote, Ok(NetworkResult::SynchronizationPending(state)), BTreeMap::new()))
            .unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert!(!engine.network_request_complete(&ticket));
        assert!(engine.background_pending());
        let (send, remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().session = Some(session.id);
        send.send((remote, Err("foreground open failed".into()), BTreeMap::new())).unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert!(!engine.network_request_complete(&ticket));
        assert!(engine.synchronization.take().is_some());
        let (send, remote) = transfer(&mut engine);
        send.send((remote, Err("original sync failed".into()), BTreeMap::new())).unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert!(engine.network_request_complete(&ticket));
        assert_eq!(
            engine.take_network_request(ticket).unwrap_err().to_string(),
            "original sync failed"
        );
        assert!(engine.network_request_waiter.is_none());
    }

    #[test]
    fn canceling_a_suspended_sync_terminates_its_original_ticket() {
        // Cancellation between units must retire the continuation without scheduling another unit.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let (send, remote) = transfer(&mut engine);
        let ticket = engine.network_request_ticket(false);
        let state = remote::Synchronization::new(
            &engine.services.store,
            remote::ClientScope::FullClient,
            crate::trace::Interval::new("replication.sync"),
        )
        .unwrap();
        send.send((remote, Ok(NetworkResult::SynchronizationPending(state)), BTreeMap::new()))
            .unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        engine.cancel_transfer().unwrap();
        assert!(!engine.background_pending());
        assert!(engine.network_request_complete(&ticket));
        assert!(engine.take_network_request(ticket).is_err());
    }

    #[test]
    fn disconnect_after_foreground_success_cancels_the_original_sync_ticket() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        engine.synchronization = Some(
            remote::Synchronization::new(
                &engine.services.store,
                remote::ClientScope::FullClient,
                crate::trace::Interval::new("replication.sync"),
            )
            .unwrap(),
        );
        let ticket = engine.network_request_ticket(false);
        engine.transfer_result = TransferStatus {
            kind: TransferKind::Complete,
            session: Some("foreground".into()),
            ..Default::default()
        };
        assert!(!engine.network_request_complete(&ticket));
        engine.disconnect_network().unwrap();
        assert!(engine.network_request_complete(&ticket));
        assert_eq!(engine.transfer_result.kind, TransferKind::Paused);
        assert_eq!(
            engine.take_network_request(ticket).unwrap_err().to_string(),
            "Synchronization disconnected; local data is retained"
        );
        assert!(engine.network_request_waiter.is_none());
    }

    #[test]
    fn canceling_an_interleaved_app_preserves_the_transport_and_original_sync() {
        // Exercise each correlated cancellation with a real shared QUIC handle;
        // the fake worker completion lets the test choose the exact unit boundary.
        use crate::network::ap::{ApConnection, ClientIdentity, testing};
        for action in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let root = engine.services.store.create_volume("Root", true).unwrap();
            let session =
                engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
            let invitation = engine.invitation(&session.id).unwrap();
            engine.services.store.preview_invitation(&invitation, engine.channel()).unwrap();
            engine.synchronization = Some(
                remote::Synchronization::new(
                    &engine.services.store,
                    remote::ClientScope::FullClient,
                    crate::trace::Interval::new("replication.sync"),
                )
                .unwrap(),
            );
            let ap = testing::silent_authenticating_server();
            let runtime =
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut identity = ClientIdentity::from_seed(
                "cancel@sync.invalid",
                "Test",
                engine.services.store.account_seed().unwrap(),
            )
            .unwrap();
            identity.auth_token = engine
                .services
                .store
                .account()
                .id
                .as_bytes()
                .iter()
                .copied()
                .chain([0; 56])
                .collect();
            identity.instance_id = Id::from_bytes([1; 16]).to_string().into_bytes();
            identity.ca_certificate = vec![1];
            identity.instance_certificate = vec![1];
            let connection = Arc::new(
                runtime
                    .block_on(ApConnection::connect_authenticated(&ap.config, &mut identity))
                    .unwrap(),
            );
            let (send, remote) = transfer(&mut engine);
            let work = engine.background.as_mut().unwrap();
            work.session = Some(session.id.clone());
            *work.cancel.lock().unwrap() = Some(connection.clone());
            let ticket = engine.network_request_ticket(false);
            let request = match action {
                0 => Request::CancelTransfer,
                1 => Request::PauseInvitation { session: session.id.clone() },
                _ => Request::ForgetSession { session: session.id },
            };
            engine.dispatch_reply(request).unwrap();
            assert!(engine.background.as_ref().unwrap().canceled.raised());
            assert!(
                !connection.is_closed(),
                "canceling the app closed the suspended sync's transport"
            );
            send.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            assert!(!engine.network_request_complete(&ticket));
            assert!(engine.synchronization.take().is_some());
            let (send, remote) = transfer(&mut engine);
            send.send((remote, Ok(NetworkResult::Synchronized), BTreeMap::new())).unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            assert!(engine.network_request_complete(&ticket));
            engine.take_network_request(ticket).unwrap();
            assert!(!connection.is_closed());
            connection.shutdown();
            runtime.block_on(connection.wait_closed(std::time::Duration::from_millis(250)));
            assert!(ap.closed.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        }
    }

    #[test]
    fn losing_an_interleaved_worker_terminates_the_original_sync_ticket() {
        // A lost Remote cannot leave the original caller waiting on a continuation forever.
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        engine.synchronization = Some(
            remote::Synchronization::new(
                &engine.services.store,
                remote::ClientScope::FullClient,
                crate::trace::Interval::new("replication.sync"),
            )
            .unwrap(),
        );
        let (send, remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().session = Some("foreground".into());
        let ticket = engine.network_request_ticket(false);
        drop(remote);
        drop(send);
        notify(&engine);
        assert!(engine.complete_background().is_err());
        assert!(engine.synchronization.is_none());
        assert!(engine.network_request_complete(&ticket));
        assert!(engine.take_network_request(ticket).is_err());
    }

    #[test]
    fn recovery_status_preserves_failed_account_bootstrap() {
        // Reopening sign-in reads the saved key state. Neither admission nor
        // completion of that read may hide the synchronization error and retry.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let mut identity = crate::network::ap::ClientIdentity::from_seed(
            "person@example.invalid",
            "Test",
            store.account_seed().unwrap(),
        )
        .unwrap();
        identity.auth_token =
            store.account().id.as_bytes().iter().copied().chain([0; 56]).collect();
        identity.save(&directory.path().join("instance.json")).unwrap();
        store
            .save_recovery_state(&crate::store::RecoveryState {
                secret: [7; 16],
                acknowledged: true,
                revision: 1,
                synchronized_digest: None,
                pending: None,
            })
            .unwrap();
        drop(store);
        let mut engine = Engine::open(directory.path()).unwrap();
        engine.bootstrap_pending = false;
        engine.transfer_result = TransferStatus {
            kind: TransferKind::Failed,
            message: Some("Account record does not match the expected schema".into()),
            ..Default::default()
        };
        let failed = serde_json::to_value(engine.transfer_status()).unwrap();
        let ticket = engine.begin_network_request(Request::RecoveryStatus).unwrap();
        assert_eq!(serde_json::to_value(engine.transfer_status()).unwrap(), failed);
        drain_updates(&mut engine);
        assert!(matches!(
            engine.take_network_request(ticket).unwrap(),
            Reply::Recovery(reply::RecoveryStatus::Ready { .. })
        ));
        assert_eq!(serde_json::to_value(engine.transfer_status()).unwrap(), failed);
        assert!(!engine.network.account_initialized());
        assert!(!engine.bootstrap_pending);
    }

    #[test]
    fn explicit_disconnect_does_not_resume_canceled_recovery_network_work() {
        for disconnect in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut engine = Engine::open(directory.path()).unwrap();
            let (send, remote) = transfer(&mut engine);
            engine.background.as_mut().unwrap().recovery_control =
                Some(recovery::control::Kind::Status);
            engine
                .dispatch_reply(if disconnect {
                    Request::Disconnect
                } else {
                    Request::CancelTransfer
                })
                .unwrap();
            send.send((remote, Err("fixture control was interrupted".into()), BTreeMap::new()))
                .unwrap();
            notify(&engine);
            engine.complete_background().unwrap();
            assert_eq!(
                engine.bootstrap_pending, !disconnect,
                "only cancellation of a control may resume account replication; explicit disconnect must stay disconnected"
            );
            if disconnect {
                engine.start_network_work();
                assert!(engine.background.is_none());
            }
        }
    }

    #[test]
    fn disconnect_responds_before_worker_completion_and_completion_still_wakes_owner() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path().join("profile")).unwrap();
        let socket = directory.path().join("socket");
        let mut queue = AdmissionQueue::new(UnixListener::bind(&socket).unwrap(), &engine).unwrap();
        let (send, remote) = transfer(&mut engine);
        let mut client = UnixStream::connect(&socket).unwrap();
        client.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        ipc::write(
            &mut client,
            &ipc::codec::encode(&Request::Disconnect, ipc::codec::MAX_ARCHIVE).unwrap(),
        )
        .unwrap();
        // Disconnect acknowledges cancellation while Remote stays with its worker.
        queue.run_once(&mut engine).unwrap();
        queue.run_once(&mut engine).unwrap();
        let result: Value = serde_json::to_value(
            ipc::Response::new(ipc::read(&mut client).unwrap()).unwrap().into_owned().unwrap(),
        )
        .unwrap();
        assert_eq!(result["disconnected"], true);
        assert!(engine.background.is_some());
        send.send((remote, Err("test transfer ended".into()), BTreeMap::new())).unwrap();
        drop(send);
        drop(NotifyCompletion(
            engine.completion.wake.clone(),
            engine.background.as_ref().unwrap().ready.clone(),
        ));
        // Ordinary requests no longer collect worker completion incidentally.
        let before =
            serde_json::to_value(engine.dispatch_reply(Request::TransferStatus).unwrap()).unwrap();
        assert_eq!(before["transfer"]["kind"], "canceling");
        queue.run_once(&mut engine).unwrap();
        assert!(engine.background.is_none());
        assert!(engine.remote.is_some());
        assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Paused);
    }

    #[test]
    fn cold_join_completion_adopts_metadata_without_creating_a_guest() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Cold join", true).unwrap();
        let session = engine
            .create_session(
                crate::package::signature::testing::signed(
                    &engine.services.store,
                    crate::engine::tests::button_package(),
                )
                .unwrap(),
                root.id,
            )
            .unwrap();
        let invitation = engine
            .services
            .store
            .invitation(&session.id, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        engine
            .services
            .store
            .preview_invitation(&invitation, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        let prepared = updates::prepare_candidate(
            &engine.runtime,
            &engine.services.store,
            &session.id,
            &engine.services.path.join(crate::paths::BUILDS),
            Some(json!({"resolved_build_id":session.resolved_build_id,
                "selector":session.selector})),
        )
        .unwrap();
        let (sender, remote) = transfer(&mut engine);
        engine.background.as_mut().unwrap().session = Some(session.id.clone());
        sender
            .send((
                remote,
                Ok(NetworkResult::Synchronized),
                BTreeMap::from([(session.id.clone(), Ok(prepared))]),
            ))
            .unwrap();
        notify(&engine);
        engine.complete_background().unwrap();
        assert_eq!(engine.transfer_status().kind, TransferKind::Complete);
        assert!(matches!(engine.prepare_session(&session.id).unwrap(), Reply::SessionPrepared(_)));
        assert!(engine.services.store.pending_invitations().unwrap().is_empty());
    }

    #[test]
    fn session_preparation_admits_metadata_without_executing_a_guest() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Prepared root", false).unwrap();
        let package = Package::new(
            "test".into(),
            "Never initialized by services".into(),
            wat::parse_str(
                "(module (memory (export \"memory\") 1) (func (export \"pb_main\") unreachable))",
            )
            .unwrap(),
            BTreeMap::new(),
        )
        .unwrap();
        let session = engine.create_session(package, root.id).unwrap();
        let request = Request::PrepareSession { session: session.id.clone() };
        let Reply::SessionPrepared(prepared) = engine.dispatch_reply(request.clone()).unwrap()
        else {
            panic!("expected prepared metadata")
        };
        assert_eq!(prepared.session, session);
        let bytes = super::super::ipc::codec::encode(
            &Reply::SessionPrepared(prepared),
            super::super::ipc::codec::MAX_ARCHIVE,
        )
        .unwrap();
        let archive =
            super::super::ipc::codec::access::<super::super::reply::ArchivedReply>(&bytes).unwrap();
        assert!(archive.matches_request(&request));
        engine.services.store.record_session_denial(&session, "revoked preparation").unwrap();
        assert!(
            engine.dispatch_reply(request).unwrap_err().to_string().contains("revoked preparation")
        );
    }

    #[test]
    fn local_open_runs_while_transfer_owns_remote_and_after_worker_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Root", true).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let invitation = engine
            .services
            .store
            .invitation(&session.id, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        engine
            .services
            .store
            .preview_invitation(&invitation, crate::release_channel::ReleaseChannel::Public)
            .unwrap();
        engine.services.store.pause_invitation(&session.id, true).unwrap();
        let mut owners = LocalApps::default();
        let instance = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        let first = owners.apps.frame_bytes(instance).unwrap().to_vec();
        let (send, remote) = transfer(&mut engine);
        owners.input(instance, input_event(2, 1, vec![]).unwrap()).unwrap();
        assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn"]);
        assert!(engine.background.is_some());
        drop(send);
        drop(remote);
        notify(&engine);
        assert!(engine.complete_background().is_err());
        owners.input(instance, input_event(2, 1, vec![]).unwrap()).unwrap();
        assert_eq!(owners.apps.frame_bytes(instance).unwrap(), first);
        assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn", "turn"]);
        assert_eq!(engine.services.store.session(&session.id).unwrap(), session);
        assert!(engine.services.store.pending_invitation_paused(&session.id).unwrap());
        owners.quit(&mut engine, instance).unwrap();
        drop(engine);
        let mut engine = Engine::open(directory.path()).unwrap();
        let reopened = owners.open(&mut engine, &session.id).unwrap();
        owners.settle();
        assert_ne!(reopened, instance);
        assert_eq!(owners.apps.frame_bytes(reopened).unwrap(), first);
        owners.quit(&mut engine, reopened).unwrap();
        engine.services.store.record_session_denial(&session, "revoked during transfer").unwrap();
        assert!(owners.open(&mut engine, &session.id).is_err());
    }

    #[test]
    fn initial_replication_runs_without_gui_and_does_not_repeat_after_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        // An unenrolled store does not start network work. Exercise the same
        // one-shot flag that an enrolled reopen or successful sign-in sets;
        // the real worker then reports its missing credentials locally.
        engine.start_network_work();
        assert!(engine.background.is_none());
        engine.bootstrap_pending = true;
        engine.start_network_work();
        assert!(engine.background.is_some());
        let listener = UnixListener::bind(directory.path().join("owner.sock")).unwrap();
        let mut queue = AdmissionQueue::new(listener, &engine).unwrap();
        while engine.background.is_some() {
            queue.run_once(&mut engine).unwrap();
        }
        assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Failed);
        for _ in 0..10 {
            engine.start_network_work();
        }
        assert!(engine.background.is_none());
        assert!(!engine.bootstrap_pending);
    }

    #[test]
    fn app_display_lookup_preserves_transfer_status_and_returns_its_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        // Reject the metadata read locally, without depending on an AP fixture.
        engine.remote.as_mut().unwrap().require_replacement();
        engine.availability_cache = engine.availability().unwrap();
        let transfer = TransferStatus {
            kind: TransferKind::Running,
            message: Some("Reconnecting…".into()),
            ..Default::default()
        };
        engine.transfer_result = transfer.clone();
        let request = Request::GetAppDisplay {
            app: crate::package::AppIdentity {
                publisher: engine.services.store.account().id,
                app_id: "test-app".into(),
            },
            build: crate::identifier::encode(&[1; 32]),
        };
        for cancel in [false, true] {
            let ticket = engine.begin_network_request(request.clone()).unwrap();
            engine.network.progress.set(
                Some(reply::TransferProgress::Connecting { hostname: "example.invalid".into() }),
                &engine.completion.wake,
            );
            assert_eq!(engine.transfer_status(), transfer);
            if cancel {
                engine.cancel_transfer().unwrap();
                assert_eq!(engine.transfer_status(), transfer);
            }
            drain_updates(&mut engine);
            assert_eq!(
                engine.take_network_request(ticket).unwrap_err().to_string(),
                "Confirm replacing existing data before signing in"
            );
            assert_eq!(engine.transfer_status(), transfer);
        }
    }

    #[test]
    fn rejected_catalog_reads_do_not_invalidate_the_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let volume = engine.services.store.create_volume("Offline edits", false).unwrap();
        engine.complete_background().unwrap();
        // Settle the direct store mutation before isolating query completion.
        engine.apply_received_changes().unwrap();
        engine.availability_cache = engine.availability().unwrap();
        let before = engine.change_token();
        let ticket = engine
            .begin_network_request(Request::RejectedEntries {
                volume: volume.id,
                after: None,
                limit: 32,
            })
            .unwrap();
        assert_eq!(engine.change_token(), before, "starting a query must not refresh itself");
        drain_updates(&mut engine);
        assert!(matches!(engine.network_response.take(), Some(Ok(Reply::Diagnostic(
            reply::DiagnosticReply::RejectedEntries { page, .. }
        ))) if page.entries.is_empty()));
        assert_eq!(engine.change_token(), before, "completing a query must not refresh itself");
        drop(ticket);
    }

    #[test]
    fn status_observation_preserves_notification_when_availability_returns_to_previous_value() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let initial = engine.availability().unwrap();
        engine.availability_cache = initial.clone();

        // A status read can observe transient worker health between owner turns.
        engine.bootstrap_pending = true;
        let Reply::Status(status) = engine.dispatch_reply(Request::Status).unwrap() else {
            panic!("status request returns status");
        };
        assert_eq!(status.availability, "Waiting to synchronize");
        let observed = engine.change_token();

        engine.bootstrap_pending = false;
        engine.completion.wake.notify();
        engine.complete_background().unwrap();
        assert_eq!(engine.availability().unwrap(), initial);
        assert_ne!(engine.change_token(), observed, "returning from observed health must notify");
        let current = engine.change_token();
        engine.dispatch_reply(Request::Status).unwrap();
        assert_eq!(engine.change_token(), current, "unchanged status reads must not invalidate");
    }

    #[test]
    fn roster_changes_wake_the_session_inspector() {
        // Verify that real roster changes reach the inspector and an empty
        // observation does not request another roster update.
        use crate::session_channel::{Participant, PresentParticipant, SOURCE_BYTES};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Roster", false).unwrap();
        let session =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        engine.complete_background().unwrap();
        engine.apply_received_changes().unwrap();
        let before = engine.change_token();

        let mut member = PresentParticipant {
            participant: Participant::decode(&[4; SOURCE_BYTES]).unwrap(),
            display_name: "Robin".into(),
        };
        engine
            .services
            .store
            .observe_session_participants(&session.id, std::slice::from_ref(&member));
        member.display_name = "Robin Fernández".into();
        engine.services.store.observe_session_participants(&session.id, &[member]);
        engine.complete_background().unwrap();
        let after = engine.change_token();
        assert_eq!(after.sequence, before.sequence + 1);
        let Reply::Status(status) = engine.dispatch_reply(Request::Status).unwrap() else {
            panic!("status request returns status");
        };
        let roster = &status.session_activity[&session.id].participants;
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].name, "Robin Fernández");
        engine.services.store.observe_session_participants(&session.id, &[]);
        assert!(!engine.services.store.take_session_activity_changed());
    }

    #[test]
    fn completion_refreshes_stable_availability_without_incoming_database_notice() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        engine.availability_cache = "Pending synchronization".into();
        let initial = engine.change_token();
        engine.completion.wake.notify();
        engine.complete_background().unwrap();
        assert_eq!(engine.availability_cache, engine.availability().unwrap());
        assert_ne!(engine.change_token(), initial);
        let current = engine.change_token();
        engine.completion.wake.notify();
        engine.complete_background().unwrap();
        assert_eq!(engine.change_token(), current, "unchanged health must not invalidate again");
    }

    #[test]
    fn disconnected_worker_reports_failure_and_retains_local_data() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let account = engine.services.store.account().id;
        let (send, remote) = transfer(&mut engine);
        let initial = engine.change_token();
        drop(send);
        drop(remote);
        drop(NotifyCompletion(
            engine.completion.wake.clone(),
            engine.background.as_ref().unwrap().ready.clone(),
        ));
        let error = engine.complete_background().unwrap_err();
        assert!(error.to_string().contains("without a result"));
        assert!(engine.background.is_none());
        assert_eq!(engine.transfer_result.kind, crate::engine::reply::TransferKind::Failed);
        assert_ne!(engine.change_token(), initial);
        assert_eq!(engine.services.store.account().id, account);
        assert!(engine.dispatch_reply(Request::Status).is_ok());
    }
}

#[cfg(test)]
#[path = "completion_shutdown_tests.rs"]
mod completion_shutdown_tests;

#[cfg(all(test, target_vendor = "apple"))]
#[path = "apple_sign_in_tests.rs"]
mod apple_sign_in_tests;
