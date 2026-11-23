//! Bounded background requests, engine recovery, and change notifications.
use super::*;
use crate::engine::ChangeToken;
use std::{collections::VecDeque, path::PathBuf};

pub(super) fn start(
    focus_window: bool,
) -> (RequestSender, async_channel::Receiver<WorkerResponse>, async_channel::Receiver<&'static str>)
{
    let (send, requests, errors) = worker_queue::channel();
    let events = send.observer_sender();
    let gate = send.gate();
    let (responses, receive) = async_channel::bounded::<WorkerResponse>(1);
    let profile = profile_path();
    std::thread::spawn(move || run(profile, requests, responses, events, gate, focus_window));
    (send, receive, errors)
}

fn run(
    profile: PathBuf,
    requests: async_channel::Receiver<Message>,
    responses: async_channel::Sender<WorkerResponse>,
    events: async_channel::Sender<Message>,
    gate: std::sync::Arc<worker_queue::Gate>,
    focus_window: bool,
) {
    let Ok(child) = startup(&profile, &responses, &gate) else {
        return;
    };
    let refresh_profile = profile.clone();
    std::thread::spawn(move || {
        if let Err(error) = guest_app::refresh_wrappers(&refresh_profile) {
            crate::platform::failure::log(
                &refresh_profile,
                "refresh guest applications",
                &error.to_string(),
            );
        }
    });
    let watch =
        match worker_watch::Watch::start(profile.clone(), events.clone(), gate.clone(), child) {
            Ok(watch) => watch,
            Err(error) => {
                crate::platform::failure::log(
                    &profile,
                    "start management observer",
                    &format!("{error:?}"),
                );
                let response = WorkerResponse::critical(error.to_string().into());
                let _ = responses.send_blocking(response);
                return;
            }
        };
    let recovery =
        match worker_recovery::Worker::start(profile.clone(), responses.clone(), gate.clone()) {
            Ok(worker) => worker,
            Err(error) => {
                crate::platform::failure::log(
                    &profile,
                    "start account IPC worker",
                    &format!("{error:?}"),
                );
                let response = WorkerResponse::critical(error.to_string().into());
                let _ = responses.send_blocking(response);
                return;
            }
        };
    Worker {
        focus_window,
        refresh: worker_refresh::Refresh::default(),
        deferred: VecDeque::new(),
        copy: None,
        recovery,
        watch,
        profile,
        requests,
        responses,
        events,
        transfer: false,
        rearm_after: None,
        watch_failed: !gate.accepts(0),
        gate,
        recovery_pending: false,
    }
    .run();
}
fn launch_completion(
    responses: async_channel::Sender<WorkerResponse>,
    gate: Arc<worker_queue::Gate>,
    epoch: u64,
) -> guest_app::LaunchCompletion {
    Box::new(move |instance, result| {
        if let Err(error) = result {
            if !gate.accepts(epoch) {
                return;
            }
            let mut response = WorkerResponse::new(Err(error));
            response.launched = Some(instance);
            response.launch_failed = true;
            response.recovery_epoch = Some(epoch);
            let _ = responses.send_blocking(response);
        }
    })
}

fn startup(
    profile: &std::path::Path,
    responses: &async_channel::Sender<WorkerResponse>,
    gate: &worker_queue::Gate,
) -> Result<Option<std::process::Child>, ()> {
    let started = std::time::Instant::now();
    let mut child = None;
    match startup::start_local_engine(profile, false, &mut child) {
        Ok(()) => Ok(child),
        Err(error) => {
            crate::platform::failure::log(profile, "engine startup", &error);
            let mut response = if crate::engine::restart::terminal(&error) {
                WorkerResponse::critical(error.clone().into())
            } else {
                WorkerResponse::new(Err(error.clone()))
            };
            response.engine_lost = !response.critical;
            response.recovery_started = Some(started);
            let terminal = response.critical;
            gate.pause();
            let _ = responses.send_blocking(response);
            if terminal {
                return Err(());
            }
            Ok(child)
        }
    }
}
// Preserve the original local destruction order: queued credits are released
// before copy/recovery IPC stops, and both stop before the change observer.
struct Worker {
    focus_window: bool,
    refresh: worker_refresh::Refresh,
    deferred: VecDeque<QueuedRequest>,
    copy: Option<worker_copy::Task>,
    recovery: worker_recovery::Worker,
    watch: worker_watch::Watch,
    profile: PathBuf,
    requests: async_channel::Receiver<Message>,
    responses: async_channel::Sender<WorkerResponse>,
    events: async_channel::Sender<Message>,
    transfer: bool,
    rearm_after: Option<ChangeToken>,
    watch_failed: bool,
    gate: std::sync::Arc<worker_queue::Gate>,
    recovery_pending: bool,
}

impl Worker {
    fn respond(&self, response: WorkerResponse) -> bool {
        self.responses.send_blocking(response).is_ok()
    }
    fn run(mut self) {
        while self.finish_copy() {
            let Some(message) = self.next_message() else { break };
            if self.requests.is_closed() {
                break;
            }
            let queued = match message {
                Message::CopyComplete => continue,
                Message::Request(request) => request,
                Message::Data(request) => {
                    let lost = !self.gate.accepts(request.epoch);
                    let response = worker_data::execute(&self.profile, request, lost);
                    if !self.respond(response) {
                        break;
                    }
                    continue;
                }
                Message::Launch(queued) => {
                    let result = if self.gate.accepts(queued.epoch) {
                        new_session::execute(
                            &self.profile,
                            queued.launch,
                            self.focus_window,
                            |row| {
                                // Share status delivery order so an older snapshot cannot
                                // erase the new row and its selection after this notice.
                                let mut response = WorkerResponse::new(Ok(()));
                                response.session_created = Some((queued.form, row));
                                response.recovery_epoch = Some(queued.epoch);
                                let _ = self.respond(response);
                            },
                            Some(launch_completion(
                                self.responses.clone(),
                                self.gate.clone(),
                                queued.epoch,
                            )),
                        )
                    } else {
                        Err(new_session::Failure {
                            message: worker_queue::RETIRED.into(),
                            uncertain: false,
                        })
                    };
                    let _ = queued
                        .reply
                        .try_send(new_session::Completion { result, _credit: queued.credit });
                    continue;
                }
                Message::Fatal(error) => {
                    let response = WorkerResponse::critical(error);
                    let _ = self.respond(response);
                    break;
                }
                Message::Changed(result) => {
                    if !self.changed(result) {
                        break;
                    }
                    continue;
                }
            };
            if !self.execute(queued) {
                break;
            }
        }
    }
    fn finish_copy(&mut self) -> bool {
        let Some(response) = self.copy.as_mut().and_then(|copy| {
            if copy.cancelled() { copy.interrupt_size(&self.gate) } else { copy.take(&self.gate) }
        }) else {
            return true;
        };
        self.copy.take();
        self.respond(response)
    }
    fn next_message(&mut self) -> Option<Message> {
        if self.refresh.is_empty()
            && let Some(after) = self.rearm_after.take()
            && let Err(error) = self.watch.rearm(Some(after))
        {
            let _ = self.respond(WorkerResponse::new(Err(error.into())));
            return None;
        }
        if let Some(message) = self.refresh.next(|| {
            if !self.transfer && !self.deferred.is_empty() {
                self.deferred.pop_front().map(Message::Request)
            } else {
                self.requests.try_recv().ok()
            }
        }) {
            Some(message)
        } else if !self.transfer && !self.deferred.is_empty() {
            Some(Message::Request(
                self.deferred.pop_front().expect("deferred queue was checked as nonempty"),
            ))
        } else if !self.transfer && self.recovery_pending {
            self.recovery_pending = false;
            Some(Message::Request(QueuedRequest::internal(Request::RecoveryStatus)))
        } else {
            self.requests.recv_blocking().ok()
        }
    }
    fn changed(&mut self, result: Result<crate::engine::ipc::ChangeReply, String>) -> bool {
        match result {
            Ok(reply) => {
                if self.watch_failed {
                    self.watch_failed = false;
                    self.gate.resume();
                    self.transfer = false;
                    self.refresh = Default::default();
                    let mut response = WorkerResponse::new(Ok(()));
                    response.engine_recovered = true;
                    if !self.respond(response) {
                        return false;
                    }
                }
                self.rearm_after = Some(reply.token);
                self.refresh.begin();
                true
            }
            Err(error) => {
                self.copy.take();
                self.watch_failed = true;
                self.refresh = Default::default();
                self.rearm_after = None;
                crate::diagnostic!("Local change observer stopped: {error}");
                let mut response = WorkerResponse::new(Err(error));
                response.engine_lost = true;
                self.respond(response)
            }
        }
    }
    fn execute(&mut self, mut queued: QueuedRequest) -> bool {
        if queued.credit.is_some() && !self.gate.accepts(queued.epoch) {
            // A retired startup/status read is superseded by reconnect refresh.
            if matches!(queued.request, Request::Status) && queued.sign_in.is_none() {
                return true;
            }
            // An old admission must never run in the replacement engine.
            let _ = self.respond(retired_response(queued));
            return true;
        }
        if self.watch_failed {
            return true;
        }
        if matches!(
            queued.request,
            Request::DuplicateVolume { .. }
                | Request::DuplicateEntry { .. }
                | Request::PayloadSize { .. }
        ) {
            return self.copy(queued);
        }
        if worker_recovery::handles(&queued.request) {
            let automatic = queued.credit.is_none()
                && queued.sign_in.is_none()
                && matches!(queued.request, Request::RecoveryStatus);
            if queued.credit.is_none() {
                queued.epoch = self.gate.epoch();
            }
            let action = worker_recovery::Action::of(&queued.request);
            let sign_in = queued.sign_in;
            let sent = if automatic {
                self.recovery.send_automatic_status(queued)
            } else {
                self.recovery.send(queued)
            };
            return match sent {
                Ok(()) => true,
                Err(_) if automatic => true,
                Err(error) => {
                    let mut response = WorkerResponse::new(Err(error.into()));
                    response.recovery_action = action;
                    response.sign_in = sign_in;
                    self.respond(response)
                }
            };
        }
        self.execute_local(queued)
    }
    fn copy(&mut self, queued: QueuedRequest) -> bool {
        if queued.credit.as_ref().is_some_and(worker_queue::Credit::cancelled) {
            let epoch = queued.epoch;
            let mut response = retired_response(queued);
            response.result = Err("Size calculation cancelled".into());
            response.recovery_epoch = Some(epoch);
            return self.respond(response);
        }
        if let Some(response) = self.copy.as_mut().and_then(|task| task.interrupt_size(&self.gate))
        {
            self.copy.take();
            if !self.respond(response) {
                return false;
            }
        }
        let result = if self.copy.is_some() {
            Err(Box::new((
                queued,
                std::io::Error::other(
                    "A copy is already running. Wait for it to finish, then try again.",
                ),
            )))
        } else {
            worker_copy::Task::start(
                self.profile.clone(),
                queued,
                self.events.clone(),
                self.gate.clone(),
            )
        };
        match result {
            Ok(task) => {
                self.copy = Some(task);
                true
            }
            Err(failure) => {
                let (queued, error) = *failure;
                let mut response = WorkerResponse::new(Err(error.to_string()));
                if self.gate.accepts(queued.epoch) {
                    response.browser_failure = browser::Failure::for_request(&queued.request);
                }
                response.recovery_epoch = Some(queued.epoch);
                response.credit = queued.credit;
                self.respond(response)
            }
        }
    }
    fn call(
        profile: &std::path::Path,
        focus_window: bool,
        request: &Request,
        completion: Option<guest_app::LaunchCompletion>,
    ) -> (WorkerResponse, Option<TransferKind>) {
        let mut browser_failure = None;
        let result = ipc::call_archive(profile, request).map_err(|error| {
            browser_failure = browser::Failure::for_name_collision(request, error.as_ref());
            let detail = format!("request={:?} error={error:?}", std::mem::discriminant(request));
            crate::platform::failure::log(profile, "management IPC", &detail);
            error.to_string()
        });
        let mut transfer_kind = None;
        let result = result.and_then(|archive| {
            use crate::engine::reply::{ArchivedReply, DeveloperReply};
            // Local bundle creation has already installed its app and initialized its data.
            // Prepare through the ordinary local admission checks before opening its window.
            let archive = if matches!(request, Request::CreateSession { .. }) {
                let session = match archive.into_owned().map_err(|e| e.to_string())? {
                    Reply::Developer(DeveloperReply::Session(session)) => session,
                    Reply::Error { error } => return Err(error),
                    _ => return Err("Expected a newly created local session".into()),
                };
                ipc::call_archive(profile, &Request::PrepareSession { session: session.id })
                    .map_err(|e| e.to_string())?
            } else {
                archive
            };
            let kind = match archive.archived() {
                ArchivedReply::Transfer { transfer } => Some(&transfer.kind),
                ArchivedReply::Status(status) => Some(&status.transfer.kind),
                _ => None,
            };
            if let Some(kind) = kind {
                transfer_kind = Some(
                    rkyv::api::low::deserialize::<TransferKind, rkyv::rancor::Error>(kind)
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok(archive)
        });
        let mut response = WorkerResponse::from_archive(result);
        response.browser_failure = browser_failure;
        if let Some(prepared) = &response.prepared {
            let result = match completion {
                Some(completion) => guest_app::launch_async(
                    profile,
                    &prepared.session,
                    false,
                    focus_window,
                    vec![],
                    true,
                    completion,
                ),
                None => guest_app::launch_confirmed(
                    profile,
                    &prepared.session,
                    false,
                    focus_window,
                    vec![],
                    true,
                ),
            };
            match result {
                Ok(instance) => response.launched = Some(instance),
                Err(error) => {
                    response.result = Err(error.to_string());
                    response.launch_failed = true;
                    response.prepared = None;
                }
            }
        }
        (response, transfer_kind)
    }
    fn status_response(
        &mut self,
        request: &Request,
        response: &mut WorkerResponse,
        internal: bool,
    ) -> bool {
        if !matches!(request, Request::Status) {
            return false;
        }
        let recover = self.recovery.observe_status(response, internal);
        if self.recovery.offline() {
            self.recovery_pending = false;
        }
        crate::trace::mark(
            if response.session_rows.as_ref().is_some_and(|sessions| !sessions.is_empty()) {
                "ui.status_received_retained"
            } else {
                "ui.status_received_empty_or_failed"
            },
        );
        recover
    }
    fn execute_local(&mut self, queued: QueuedRequest) -> bool {
        let QueuedRequest { request, credit, epoch, sign_in } = queued;
        let internal = credit.is_none();
        // Retire automatic probes before the engine can cancel their RPC.
        // A later Status reconciles the mode even if this save/acknowledgment fails.
        if matches!(&request, Request::WorkOffline { enabled: true }) {
            self.recovery.set_offline(true);
            self.recovery_pending = false;
        }
        worker_refresh::retire_deferred_starts(&mut self.deferred, &request);
        let _timing = crate::trace::Span::new("ui.worker.request");
        let (mut response, transfer_kind) = Self::call(
            &self.profile,
            self.focus_window,
            &request,
            Some(launch_completion(self.responses.clone(), self.gate.clone(), epoch)),
        );
        response.sign_in = sign_in;
        // LabelVolume returns the same Volume reply as creation. Only fresh
        // identities enter the create-and-name journey.
        if let Request::LabelVolume { volume, .. } = &request {
            response.volume_label = Some(*volume);
        } else if !matches!(
            &request,
            Request::CreateVolume { .. } | Request::DuplicateVolume { .. }
        ) {
            response.volume_created = None;
        }

        if response.result.is_err() && response.browser_failure.is_none() {
            response.browser_failure = browser::Failure::for_request(&request);
        }
        // A cold launch retains its original admission when another transfer runs.
        if let (Request::PrepareSession { session }, Some(state)) =
            (&request, &response.transfer_status)
            && matches!(state.kind, TransferKind::Running | TransferKind::Canceling)
            && state.session.as_deref() != Some(session.as_str())
        {
            self.transfer = true;
            self.deferred.push_back(QueuedRequest { request, credit, epoch, sign_in });
            return true;
        }
        let status_enrolled = self.status_response(&request, &mut response, internal);
        if response.result.is_ok()
            && let Some(kind) = transfer_kind
        {
            self.transfer = matches!(kind, TransferKind::Running | TransferKind::Canceling);
        }
        if let Request::SelectPersonalVolumes { session, .. } = &request {
            response.personal_session = Some(session.clone());
        }
        response.session = match &request {
            Request::PrepareSession { session } | Request::ForgetSession { session } => {
                Some(session.clone())
            }
            _ => None,
        };
        response.credit = credit;
        if !self.respond(response) {
            return false;
        }
        if status_enrolled {
            self.recovery_pending = true;
        }
        true
    }
}

pub(super) fn retired_response(queued: QueuedRequest) -> WorkerResponse {
    let mut response = WorkerResponse::new(Err(worker_queue::RETIRED.into()));
    response.recovery_action = worker_recovery::Action::of(&queued.request);
    response.browser_failure = browser::Failure::for_request(&queued.request);
    response.sign_in = queued.sign_in;
    // Return the original identity and credit so the UI can finish this admission
    // without replaying it. Moving the strings adds no retained allocation.
    match queued.request {
        Request::LabelVolume { volume, .. } => response.volume_label = Some(volume),
        Request::SelectPersonalVolumes { session, .. } => {
            response.personal_session = Some(session);
        }
        Request::PrepareSession { session } | Request::ForgetSession { session } => {
            response.session = Some(session);
        }
        _ => {}
    }
    response.credit = queued.credit;
    response
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::{fs, os::unix::net::UnixListener};

    fn package_name(bytes: &[u8]) -> String {
        crate::package::Package::read_display_name(&mut std::io::Cursor::new(&bytes)).unwrap()
    }

    #[test]
    fn creation_name_rejection_retains_its_type_before_error_formatting() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path();
        crate::paths::private_directory(profile, "control").unwrap();
        let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let server = std::thread::spawn(move || {
            for error in ["entry name already exists", "storage failure"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                ipc::read(&mut stream).unwrap();
                ipc::write(
                    &mut stream,
                    &ipc::codec::encode(&Reply::Error { error: error.into() }, 4096).unwrap(),
                )
                .unwrap();
            }
        });
        let request = Request::BloblogCreate {
            session: None,
            volume: crate::store::Id::from_bytes([1; 16]),
            name: "Untitled".into(),
        };
        for (message, collision) in
            [("entry name already exists", true), ("storage failure", false)]
        {
            let (response, _) = Worker::call(profile, false, &request, None);
            assert_eq!(response.result.unwrap_err(), message);
            assert_eq!(response.browser_failure.is_some(), collision);
        }
        server.join().unwrap();
    }

    #[test]
    fn build_transfer_failure_and_launch_failure_retain_the_persisted_name() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path();
        let mut engine = crate::engine::Engine::open(profile).unwrap();
        let root = engine.services.store.create_volume("Downloaded app", false).unwrap();
        let package = crate::engine::tests::button_package();
        let session = engine.create_session(package, root.id).unwrap();
        let request = Request::PrepareSession { session: session.id.clone() };
        let prepared = engine.dispatch_reply(request.clone()).unwrap();
        assert!(matches!(prepared, Reply::SessionPrepared(_)));
        let path = profile.join(crate::paths::BUILDS).join(&session.resolved_build_id);
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            engine.services.store.session_app_names().unwrap()[&session.id],
            package_name(&bytes)
        );

        crate::paths::private_directory(profile, "control").unwrap();

        let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let id = session.id.clone();
        let server = std::thread::spawn(move || {
            // Bytes arriving alone are not a successful preparation. Preserve
            // progress/failure until the engine returns its admitted session.
            fs::write(path, bytes).unwrap();
            for reply in [
                Reply::Transfer {
                    transfer: TransferStatus {
                        kind: TransferKind::Running,
                        session: Some(id.clone()),
                        ..Default::default()
                    },
                },
                Reply::Transfer {
                    transfer: TransferStatus {
                        kind: TransferKind::Failed,
                        session: Some(id),
                        message: Some("Verification failed".into()),
                        ..Default::default()
                    },
                },
                prepared,
                Reply::Error { error: "Fixture window launch failed".into() },
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let bytes = ipc::read(&mut stream).unwrap();
                let request = ipc::codec::access::<crate::engine::ArchivedRequest>(&bytes).unwrap();
                if matches!(reply, Reply::Error { .. }) {
                    assert!(matches!(
                        request,
                        crate::engine::ArchivedRequest::InstanceInspect { .. }
                    ));
                } else {
                    assert!(matches!(
                        request,
                        crate::engine::ArchivedRequest::PrepareSession { .. }
                    ));
                }
                ipc::write(
                    &mut stream,
                    &ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap(),
                )
                .unwrap();
            }
        });
        for kind in [TransferKind::Running, TransferKind::Failed] {
            let (response, transfer) = Worker::call(profile, false, &request, None);
            assert_eq!(transfer, Some(kind));
            assert!(response.prepared.is_none());
        }
        let (response, transfer) = Worker::call(profile, false, &request, None);
        assert_eq!(response.result.unwrap_err(), "Fixture window launch failed");
        assert!(response.prepared.is_none());
        assert!(transfer.is_none());
        server.join().unwrap();
    }
    #[test]
    fn held_copy_or_size_reply_leaves_local_creation_and_kv_available() {
        // Hold the copy RPC reply while the ordinary management call and both
        // typed KV exchanges complete; they must not enter the copy task.
        use crate::runtime::host::{DataCall, DataOperation, DataResult};
        use crate::store::{
            Access,
            volume_index::{EntryAddress, EntryKind},
        };
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };
        for size in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let profile = directory.path();
            let mut engine = crate::engine::Engine::open(profile).unwrap();
            let store = engine.services.store.clone();
            let source = store.create_volume("Original", false).unwrap();
            let access = Access::owner(store.account().id);
            let entry =
                store.create_database(&access, source.id, "Settings", EntryKind::KvDb {}).unwrap();
            let address = EntryAddress { volume: source.id, entry: entry.id };
            let copy = store.duplicate_entry(&access, address).unwrap();
            crate::paths::private_directory(profile, "control").unwrap();
            let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
            let (started, waiting) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            let context = crate::engine::service_call::Context::Tool {
                handle: crate::store::Id::generate().unwrap(),
                incarnation: crate::store::Id::generate().unwrap(),
            };
            let source_id = source.id;
            let copied = copy.clone();
            let server = std::thread::spawn(move || {
                let mut held = None;
                // Copy + create + (context open / syscall / close) for put and read.
                for _ in 0..8 {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let bytes = ipc::read(&mut socket).unwrap();
                    let request: Request =
                        rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).unwrap();
                    let reply = match request {
                        Request::PayloadSize { volume, entry, .. } => {
                            assert!(size);
                            assert_eq!((volume, entry), (address.volume, Some(address.entry)));
                            held = Some(socket);
                            started.send(()).unwrap();
                            continue;
                        }
                        Request::DuplicateEntry { address: source, .. } => {
                            assert!(!size);
                            assert_eq!(source, address);
                            held = Some(socket);
                            started.send(()).unwrap();
                            continue;
                        }
                        Request::ToolContextOpen { .. } => Reply::ToolContext { context },
                        Request::ToolContextClose { context } => {
                            Reply::ToolContextClosed { context }
                        }
                        Request::Syscall { context, operation, mutation, call } => {
                            let result = match &call.operation {
                                DataOperation::KvPut { key, value, .. } => {
                                    engine
                                        .services
                                        .store
                                        .kv_put(&access, address, key, value)
                                        .unwrap();
                                    DataResult::KvWritten
                                }
                                DataOperation::KvRead { key, offset, limit, root, .. } => {
                                    DataResult::KvRead(
                                        engine
                                            .services
                                            .store
                                            .kv_read(&access, address, key, *offset, *limit, *root)
                                            .unwrap(),
                                    )
                                }
                                _ => panic!("expected synthetic KV exchange"),
                            };
                            Reply::Syscall(crate::engine::service_call::Outcome {
                                context,
                                volume: source_id,
                                operation,
                                mutation,
                                kind: call.operation.kind() as u32,
                                revision: Some((source_id, 1)),
                                status: 0,
                                result: Some(result),
                            })
                        }
                        request => engine.dispatch_reply(request).unwrap(),
                    };
                    ipc::write(
                        &mut socket,
                        &ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap(),
                    )
                    .unwrap();
                }
                released.recv_timeout(Duration::from_secs(2)).unwrap();
                let reply = Reply::Diagnostic(if size {
                    crate::engine::reply::DiagnosticReply::PayloadSize {
                        volume: address.volume,
                        entry: Some(address.entry),
                        bytes: 5,
                    }
                } else {
                    crate::engine::reply::DiagnosticReply::EntryDuplicated {
                        source: address,
                        entry: copied,
                    }
                });
                ipc::write(
                    &mut held.unwrap(),
                    &ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap(),
                )
                .unwrap();
            });
            let (send, requests, _) = worker_queue::channel();
            let gate = send.gate();
            send.send(if size {
                Request::PayloadSize {
                    session: None,
                    volume: address.volume,
                    entry: Some(address.entry),
                }
            } else {
                Request::DuplicateEntry { address }
            })
            .unwrap();
            let Message::Request(queued) = requests.try_recv().unwrap() else {
                panic!("copy admission")
            };
            let admission = queued.credit.as_ref().unwrap().admission();
            let task = worker_copy::Task::start(
                profile.into(),
                queued,
                send.observer_sender(),
                gate.clone(),
            )
            .unwrap_or_else(|_| panic!("copy RPC starts"));
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            let (created, _) = Worker::call(
                profile,
                false,
                &Request::CreateVolume {
                    name: "Independent".into(),
                    share_key_with_service: false,
                },
                None,
            );
            assert!(created.result.is_ok());
            assert_eq!(created.volume_created.unwrap().name, "Independent");
            for operation in [
                DataOperation::KvPut {
                    entry_id: *entry.id.as_bytes(),
                    key: b"a".to_vec(),
                    value: b"anne".to_vec(),
                },
                DataOperation::KvRead {
                    entry_id: *entry.id.as_bytes(),
                    key: b"a".to_vec(),
                    offset: 0,
                    limit: 64,
                    root: None,
                },
            ] {
                send.data(1, 1, DataCall { volume: *source.id.as_bytes(), operation }).unwrap();
                let Message::Data(queued) = requests.try_recv().unwrap() else {
                    panic!("interactive KV")
                };
                let response = worker_data::execute(profile, queued, false);
                match response
                    .data_reply
                    .unwrap()
                    .result
                    .unwrap_or_else(|_| panic!("KV exchange succeeds"))
                {
                    DataResult::KvWritten => {}
                    DataResult::KvRead(chunk) => assert_eq!(chunk.bytes, b"anne"),
                    _ => panic!("expected KV result"),
                }
            }
            assert!(task.take(&gate).is_none());
            release.send(()).unwrap();
            server.join().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let response = loop {
                if let Some(response) = task.take(&gate) {
                    break response;
                }
                assert!(Instant::now() < deadline, "copy completion is delivered");
                std::thread::yield_now();
            };
            if size {
                assert!(matches!(response.browser_reply,
                    Some(Reply::Diagnostic(crate::engine::reply::DiagnosticReply::PayloadSize { volume, entry, bytes: 5 }))
                        if volume == address.volume && entry == Some(address.entry)));
            } else {
                assert!(matches!(response.browser_reply,
                    Some(Reply::Diagnostic(crate::engine::reply::DiagnosticReply::EntryDuplicated { source, entry }))
                        if source == address && entry == copy));
            }
            assert_eq!(response.credit.as_ref().unwrap().admission(), admission);
            drop(task);
        }
    }
}
