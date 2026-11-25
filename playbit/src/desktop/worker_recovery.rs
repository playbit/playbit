//! Enrollment and recovery RPCs share one bounded worker, separate from interactive IPC.
use crate::desktop::{QueuedRequest, Request, WorkerResponse, ipc};
use crate::engine::reply::Reply;
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Prepare,
    Save,
    Restore,
    PrepareSignIn,
    SavedAccount,
    Backup,
    RequestCode,
    SignIn,
    SetDisplayName,
    AcceptAgreement,
}
impl Action {
    pub fn of(request: &Request) -> Option<Self> {
        match request {
            Request::SetDisplayName { .. } => Some(Self::SetDisplayName),
            Request::AcceptAgreement { .. } => Some(Self::AcceptAgreement),
            Request::PrepareSignIn => Some(Self::PrepareSignIn),
            Request::RetryAccountKeyBackup => Some(Self::Backup),
            Request::RequestCode { .. } => Some(Self::RequestCode),
            Request::SavedAccount => Some(Self::SavedAccount),
            Request::SignIn { .. } | Request::SignInWithKey { .. } => Some(Self::SignIn),
            Request::RecoveryStatus => Some(Self::Status),
            Request::PrepareRecovery => Some(Self::Prepare),
            Request::SaveRecovery => Some(Self::Save),
            Request::RecoverCredentials { .. } => Some(Self::Restore),
            _ => None,
        }
    }
}

struct State {
    stopped: bool,
    socket: Option<UnixStream>,
    offline: bool,
    mode_generation: u64,
}
struct Work {
    queued: QueuedRequest,
    automatic_generation: Option<u64>,
}
pub struct Worker {
    send: Option<mpsc::SyncSender<Work>>,
    state: Arc<Mutex<State>>,
    responses: async_channel::Sender<WorkerResponse>,
    thread: Option<std::thread::JoinHandle<()>>,
}
pub fn handles(request: &Request) -> bool {
    matches!(
        request,
        Request::GetAccountDisplayNames { .. }
            | Request::SetDisplayName { .. }
            | Request::AcceptAgreement { .. }
            | Request::SavedAccount
            | Request::RetryAccountKeyBackup
            | Request::PrepareSignIn
            | Request::RequestCode { .. }
            | Request::SignIn { .. }
            | Request::SignInWithKey { .. }
            | Request::RecoveryStatus
            | Request::PrepareRecovery
            | Request::SaveRecovery
            | Request::RecoverCredentials { .. }
    )
}
fn exchange(
    profile: &Path,
    state: &Mutex<State>,
    gate: &super::worker_queue::Gate,
    queued: &QueuedRequest,
) -> Result<Reply, String> {
    let result = (|| -> crate::engine::Result<Reply> {
        if queued.credit.is_some() && !gate.accepts(queued.epoch) {
            return Err(super::worker_queue::RETIRED.into());
        }
        {
            let state = state.lock().expect("mutex is not poisoned");
            if state.stopped {
                return Err("Recovery worker stopped".into());
            }
        }
        let mut socket = UnixStream::connect(profile.join(crate::paths::ENGINE_SOCKET))?;
        {
            let mut state = state.lock().expect("mutex is not poisoned");
            if state.stopped {
                return Err("Recovery worker stopped".into());
            }
            state.socket = Some(socket.try_clone()?);
        }
        ipc::call_stream_archive(&mut socket, &queued.request).and_then(ipc::Response::into_owned)
    })();
    result.map_err(|error| {
        crate::platform::failure::log(
            profile,
            "account / recovery IPC",
            &format!("action={:?} error={error:?}", std::mem::discriminant(&queued.request)),
        );
        let disconnected = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionRefused
            )
        });
        if disconnected { super::worker_queue::INTERRUPTED.into() } else { error.to_string() }
    })
}

impl Worker {
    pub fn start(
        profile: PathBuf,
        responses: async_channel::Sender<WorkerResponse>,
        gate: Arc<super::worker_queue::Gate>,
    ) -> std::io::Result<Self> {
        let (send, receive) = mpsc::sync_channel::<Work>(1);
        let state = Arc::new(Mutex::new(State {
            stopped: false,
            socket: None,
            offline: false,
            mode_generation: 0,
        }));
        let worker_state = state.clone();
        let reply = responses.clone();
        let thread = std::thread::Builder::new()
            .name("playbit-recovery-ipc".into())
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                while let Ok(Work { mut queued, automatic_generation }) = receive.recv() {
                    if automatic_generation.is_some_and(|generation| {
                        let state = worker_state.lock().expect("mutex is not poisoned");
                        state.offline || state.mode_generation != generation
                    }) {
                        continue;
                    }
                    let _timing = crate::trace::Interval::new("ui.recovery_rpc");
                    let result = exchange(&profile, &worker_state, &gate, &queued);
                    let obsolete = {
                        let mut state = worker_state.lock().expect("mutex is not poisoned");
                        state.socket = None;
                        if state.stopped {
                            break;
                        }
                        automatic_generation.is_some_and(|generation| {
                            state.offline || state.mode_generation != generation
                        })
                    };
                    // Offline intent can reach the engine before its next
                    // Status reaches this worker. Only the automatic probe's
                    // exact policy refusal is non-actionable; explicit errors
                    // retain their normal response and admission identity.
                    if obsolete
                        || (automatic_generation.is_some()
                            && result.as_ref().err().is_some_and(|error| {
                                error == "Work Offline is enabled. Turn it off to connect."
                            }))
                    {
                        continue;
                    }
                    let mut response =
                        if matches!(queued.request, Request::GetAccountDisplayNames { .. }) {
                            use crate::engine::reply::DeveloperReply;
                            let names = match result {
                                Ok(Reply::Developer(DeveloperReply::AccountDisplayNames {
                                    names,
                                })) => Some(names),
                                _ => None,
                            };
                            let mut response = WorkerResponse::new(Ok(()));
                            response.permission_names =
                                queued.credit.as_ref().map(|credit| (credit.admission(), names));
                            response.credit = queued.credit.take();
                            response
                        } else {
                            WorkerResponse::from_reply(result)
                        };
                    // The change observer alone owns engine recovery state.
                    response.sign_in = queued.sign_in;
                    response.recovery_epoch = Some(queued.epoch);
                    response.recovery_action = Action::of(&queued.request);
                    if reply.send_blocking(response).is_err() {
                        break;
                    }
                    // Original admission credit stays held through response enqueue.
                    drop(queued);
                }
            })?;
        Ok(Self { send: Some(send), state, responses, thread: Some(thread) })
    }
    pub fn send(&self, queued: QueuedRequest) -> Result<(), &'static str> {
        self.send_inner(queued, None)
    }

    pub fn offline(&self) -> bool {
        self.state.lock().expect("mutex is not poisoned").offline
    }

    pub fn set_offline(&self, enabled: bool) {
        let mut state = self.state.lock().expect("mutex is not poisoned");
        let previous = state.offline;
        if previous != enabled {
            state.offline = enabled;
            state.mode_generation =
                state.mode_generation.checked_add(1).expect("recovery mode generation exhausted");
        }
    }

    pub fn observe_status(&self, response: &WorkerResponse, internal: bool) -> bool {
        let previous = self.offline();
        if let Some(status) = &response.status_ap_connection {
            self.set_offline(status.state == crate::engine::reply::ApConnectionState::OfflineMode);
        }
        !self.offline()
            && response.status_enrolled == Some(true)
            && response.status_recovery.is_none()
            && (!internal || previous)
    }

    pub fn send_automatic_status(&self, queued: QueuedRequest) -> Result<(), &'static str> {
        if !matches!(queued.request, Request::RecoveryStatus)
            || queued.credit.is_some()
            || queued.sign_in.is_some()
        {
            return Err("Automatic recovery requires an internal status request");
        }
        let generation = {
            let state = self.state.lock().expect("mutex is not poisoned");
            if state.offline {
                return Ok(());
            }
            state.mode_generation
        };
        self.send_inner(queued, Some(generation))
    }

    fn send_inner(
        &self,
        queued: QueuedRequest,
        automatic_generation: Option<u64>,
    ) -> Result<(), &'static str> {
        let length = match &queued.request {
            Request::RequestCode { email } => email.capacity(),
            Request::SignIn { code, .. } => code.capacity(),
            Request::SignInWithKey { email, account_key } => {
                email.capacity().saturating_add(account_key.capacity())
            }
            Request::SetDisplayName { name } => name.capacity(),
            Request::GetAccountDisplayNames { account_ids, .. } => {
                account_ids.capacity() * std::mem::size_of::<crate::store::Id>()
            }
            Request::RecoverCredentials { secret } => secret.capacity(),
            Request::SavedAccount
            | Request::AcceptAgreement { .. }
            | Request::RetryAccountKeyBackup
            | Request::PrepareSignIn
            | Request::RecoveryStatus
            | Request::PrepareRecovery
            | Request::SaveRecovery => 0,
            _ => return Err("This operation does not use the account worker"),
        };
        if length + std::mem::size_of::<Work>() > 1024 {
            return Err("Account request is too long; this action did not start");
        }
        self.send
            .as_ref()
            .ok_or("Recovery worker stopped")?
            .try_send(Work { queued, automatic_generation })
            .map_err(|_| "Account operation is busy; this action did not start")
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().expect("mutex is not poisoned");
            state.stopped = true;
            if let Some(socket) = state.socket.take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        self.send.take();
        self.responses.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::engine::reply::RecoveryStatus;
    use std::{
        os::unix::net::UnixListener,
        time::{Duration, SystemTime},
    };

    use crate::engine::reply::{PreparedRecovery, SignInReply};
    fn read_request(socket: &mut UnixStream) -> Request {
        rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&ipc::read(socket).unwrap())
            .unwrap()
    }
    fn write_reply(socket: &mut UnixStream, reply: &Reply) {
        ipc::write(socket, &ipc::codec::encode(reply, ipc::codec::MAX_ARCHIVE).unwrap()).unwrap();
    }
    fn status_reply() -> Reply {
        let directory = tempfile::tempdir().unwrap();
        crate::engine::Engine::open(directory.path())
            .unwrap()
            .dispatch_reply(Request::Status)
            .unwrap()
    }

    #[test]
    fn automatic_recovery_respects_offline_startup_and_resumes_on_online_status() {
        use crate::engine::reply::{ApConnectionState, ApConnectionStatus};
        let directory = tempfile::tempdir().unwrap();
        let (responses, _receive) = async_channel::bounded(1);
        let worker =
            Worker::start(directory.path().into(), responses, Arc::new(Default::default()))
                .unwrap();
        let mut status = WorkerResponse::new(Ok(()));
        status.status_enrolled = Some(true);
        status.status_ap_connection = Some(ApConnectionStatus {
            state: ApConnectionState::OfflineMode,
            hostname: "ap.test".into(),
        });
        assert!(!worker.observe_status(&status, false));
        assert!(worker.offline());
        status.status_recovery = Some(RecoveryStatus::Ready {
            revision: 7,
            pending_save: false,
            restored_sessions: None,
            restored_volumes: None,
        });
        assert!(!worker.observe_status(&status, true));
        assert!(
            matches!(status.status_recovery, Some(RecoveryStatus::Ready { revision: 7, .. })),
            "the local status remains intact"
        );
        status.status_recovery = None;
        status.status_ap_connection.as_mut().unwrap().state = ApConnectionState::Connecting;
        assert!(
            worker.observe_status(&status, true),
            "the online transition resumes automatic recovery"
        );
        assert!(
            !worker.observe_status(&status, true),
            "ordinary internal refresh does not repeat recovery"
        );
        assert!(
            worker.observe_status(&status, false),
            "ordinary enrolled startup retains recovery"
        );
    }

    #[test]
    fn late_automatic_offline_failures_do_not_hide_explicit_recovery_errors() {
        // Exercise both notification ordering and an offline/online round trip
        // while the automatic RPC is already on its separate worker.
        for observe_mode in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            crate::paths::private_directory(directory.path(), "control").unwrap();
            let listener =
                UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
            let (started, waiting) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            let server = std::thread::spawn(move || {
                let (mut automatic, _) = listener.accept().unwrap();
                assert!(matches!(read_request(&mut automatic), Request::RecoveryStatus));
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(2)).unwrap();
                let refusal = Reply::Error {
                    error: "Work Offline is enabled. Turn it off to connect.".into(),
                };
                write_reply(&mut automatic, &refusal);
                let (mut explicit, _) = listener.accept().unwrap();
                assert!(matches!(read_request(&mut explicit), Request::RecoveryStatus));
                write_reply(&mut explicit, &refusal);
            });
            let (responses, receive) = async_channel::bounded(2);
            let worker =
                Worker::start(directory.path().into(), responses, Arc::new(Default::default()))
                    .unwrap();
            worker.send_automatic_status(QueuedRequest::internal(Request::RecoveryStatus)).unwrap();
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            if observe_mode {
                worker.set_offline(true);
                worker.set_offline(false);
            }
            release.send(()).unwrap();
            let ticket = crate::ui::desktop::sign_in::flow::Ticket { generation: 9, operation: 1 };
            let mut explicit = QueuedRequest::internal(Request::RecoveryStatus);
            explicit.sign_in = Some(ticket);
            worker.send(explicit).unwrap();
            let response = receive.recv_blocking().unwrap();
            assert_eq!(response.sign_in, Some(ticket));
            assert!(response.recovery_action == Some(Action::Status));
            assert_eq!(
                response.result.unwrap_err(),
                "Work Offline is enabled. Turn it off to connect."
            );
            assert!(receive.try_recv().is_err());
            drop(worker);
            server.join().unwrap();
        }
    }
    fn recovery_reply(request: &Request) -> Reply {
        match request {
            Request::RecoveryStatus
            | Request::SaveRecovery
            | Request::RecoverCredentials { .. } => Reply::Recovery(RecoveryStatus::Ready {
                revision: 1,
                pending_save: false,
                restored_sessions: None,
                restored_volumes: None,
            }),
            Request::PrepareRecovery => {
                Reply::PreparedRecovery(PreparedRecovery { secret: "fixture-secret".into() })
            }
            Request::RequestCode { .. } => Reply::CodeSent { code_sent: true },
            Request::AcceptAgreement { .. } => Reply::AgreementAccepted,
            Request::SignIn { .. } => Reply::SignedIn(SignInReply {
                replacement_required: false,
                signed_in: true,
                recovery: None,
                recovery_error: None,
            }),
            _ => panic!("unexpected recovery fixture request"),
        }
    }

    #[test]
    fn lost_engine_during_recovery_reports_restart_without_repeating_request() {
        let directory = tempfile::tempdir().unwrap();
        crate::paths::private_directory(directory.path(), "control").unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            assert!(matches!(read_request(&mut socket), Request::SaveRecovery));
            drop(socket); // Engine exits with this RPC's outcome still unknown.
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let (responses, receive) = async_channel::bounded(1);
        let worker =
            Worker::start(directory.path().into(), responses, Arc::new(Default::default()))
                .unwrap();
        worker.send(QueuedRequest::internal(Request::SaveRecovery)).unwrap();
        let response = receive.recv_blocking().unwrap();
        assert!(!response.engine_lost);
        assert_eq!(response.result.unwrap_err(), super::super::worker_queue::INTERRUPTED);
        let listener = server.join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        drop(worker);
    }

    #[test]
    fn delayed_recovery_reply_keeps_its_admitted_epoch() {
        // Hold the old RPC across an observer-owned pause and reconnect.
        let directory = tempfile::tempdir().unwrap();
        crate::paths::private_directory(directory.path(), "control").unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let (started, waiting) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut old, _) = listener.accept().unwrap();
            assert!(matches!(read_request(&mut old), Request::SaveRecovery));
            started.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(old);
            let (mut current, _) = listener.accept().unwrap();
            let request = read_request(&mut current);
            assert!(matches!(request, Request::RecoveryStatus));
            write_reply(&mut current, &recovery_reply(&request));
        });
        let (send, requests, _) = super::super::worker_queue::channel();
        let gate = send.gate();
        let (responses, receive) = async_channel::bounded(1);
        let worker = Worker::start(directory.path().into(), responses, gate.clone()).unwrap();
        let old_ticket = crate::ui::desktop::sign_in::flow::Ticket { generation: 1, operation: 1 };
        send.sign_in(Request::SaveRecovery, old_ticket).unwrap();
        let super::super::Message::Request(old) = requests.try_recv().unwrap() else {
            panic!("old admitted request");
        };
        let old_epoch = old.epoch;
        worker.send(old).unwrap();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        gate.pause();
        gate.resume();
        let current_ticket =
            crate::ui::desktop::sign_in::flow::Ticket { generation: 1, operation: 2 };
        send.sign_in(Request::RecoveryStatus, current_ticket).unwrap();
        let super::super::Message::Request(current) = requests.try_recv().unwrap() else {
            panic!("current admitted request");
        };
        let current_epoch = current.epoch;
        assert_ne!(old_epoch, current_epoch);
        worker.send(current).unwrap();
        assert!(receive.try_recv().is_err());
        release.send(()).unwrap();
        let old = receive.recv_blocking().unwrap();
        assert_eq!(old.recovery_epoch, Some(old_epoch));
        assert_eq!(old.sign_in, Some(old_ticket));
        assert!(!old.engine_lost, "only the observer owns recovery");
        assert_eq!(old.result.unwrap_err(), super::super::worker_queue::INTERRUPTED);
        let current = receive.recv_blocking().unwrap();
        assert_eq!(current.recovery_epoch, Some(current_epoch));
        assert_eq!(current.sign_in, Some(current_ticket));
        assert!(!current.engine_lost);
        assert!(current.result.is_ok());
        drop(worker);
        server.join().unwrap();
    }

    #[test]
    fn wrong_reply_variant_reports_error_without_replaying_recovery() {
        let directory = tempfile::tempdir().unwrap();
        crate::paths::private_directory(directory.path(), "control").unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            assert!(matches!(read_request(&mut socket), Request::SaveRecovery));
            write_reply(&mut socket, &Reply::CodeSent { code_sent: true });
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let (responses, receive) = async_channel::bounded(1);
        let worker =
            Worker::start(directory.path().into(), responses, Arc::new(Default::default()))
                .unwrap();
        worker.send(QueuedRequest::internal(Request::SaveRecovery)).unwrap();
        let response = receive.recv_blocking().unwrap();
        assert!(!response.engine_lost);
        assert!(response.recovery_action == Some(Action::Save));
        assert!(response.result.is_err());
        let listener = server.join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        drop(worker);
    }

    #[test]
    fn secret_controls_leave_interactive_ipc_free_and_keep_their_response_shape() {
        let directory = PathBuf::from(format!(
            "/tmp/pb-secret-controls-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        crate::paths::private_directory(&directory, "control").unwrap();
        let listener = UnixListener::bind(directory.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let (send, started) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            for expected in [
                "recovery-status",
                "prepare-recovery",
                "save-recovery",
                "recover-credentials",
                "request-code",
                "sign-in",
                "accept-agreement",
            ] {
                let (mut recovery, _) = listener.accept().unwrap();
                let request = read_request(&mut recovery);
                assert_eq!(serde_json::to_value(&request).unwrap()["op"], expected);
                send.send(()).unwrap();
                let (mut interactive, _) = listener.accept().unwrap();
                assert!(matches!(read_request(&mut interactive), Request::Status));
                write_reply(&mut interactive, &status_reply());
                write_reply(&mut recovery, &recovery_reply(&request));
            }
        });
        let (responses, receive) = async_channel::bounded(1);
        let worker =
            Worker::start(directory.clone(), responses, Arc::new(Default::default())).unwrap();
        for request in [
            Request::RecoveryStatus,
            Request::PrepareRecovery,
            Request::SaveRecovery,
            Request::RecoverCredentials { secret: "fixture-secret".into() },
            Request::RequestCode { email: "fixture@example.invalid".into() },
            Request::SignIn { code: "123456".into() },
            Request::AcceptAgreement {
                account: crate::store::Id::from_bytes([1; 16]),
                digest: crate::network::ap::registration::terms_digest(),
            },
        ] {
            assert!(handles(&request));
            let expected = recovery_reply(&request);
            worker.send(QueuedRequest::internal(request)).unwrap();
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(receive.try_recv().is_err());
            assert!(matches!(
                ipc::call_archive(&directory, &Request::Status).unwrap().into_owned().unwrap(),
                Reply::Status(_)
            ));
            let response = receive.recv_blocking().unwrap();
            assert!(response.result.is_ok());
            use crate::desktop::AccountResponse;
            match (response.account_response.unwrap(), expected) {
                (AccountResponse::Recovery(actual), Reply::Recovery(expected)) => {
                    assert_eq!(actual, expected)
                }
                (AccountResponse::Prepared(actual), Reply::PreparedRecovery(expected)) => {
                    assert_eq!(actual, expected.secret)
                }
                (AccountResponse::CodeSent(actual), Reply::CodeSent { code_sent }) => {
                    assert_eq!(actual, code_sent)
                }
                (AccountResponse::SignedIn(actual), Reply::SignedIn(expected)) => {
                    assert_eq!(actual.signed_in, expected.signed_in);
                    assert_eq!(actual.recovery, expected.recovery);
                    assert_eq!(actual.recovery_error, expected.recovery_error);
                }
                (AccountResponse::AgreementAccepted, Reply::AgreementAccepted) => {}
                _ => panic!("incorrect account response"),
            }
        }
        drop(worker);
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
