//! One cancelable copy or size RPC per GUI, with no additional work queue.
//! Its fixed completion slot retains the original admission through UI delivery.
use super::{Message, QueuedRequest, WorkerResponse, ipc, worker_queue};
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const STACK_BYTES: usize = 512 * 1024;

struct State {
    stopped: bool,
    interrupted: bool,
    socket: Option<UnixStream>,
    queued: Option<QueuedRequest>,
    response: Option<WorkerResponse>,
}
pub(super) struct Task {
    state: Arc<Mutex<State>>,
    thread: Option<std::thread::JoinHandle<()>>,
    size: bool,
    cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
}
impl Task {
    pub fn start(
        profile: PathBuf,
        queued: QueuedRequest,
        events: async_channel::Sender<Message>,
        gate: Arc<worker_queue::Gate>,
    ) -> Result<Self, Box<(QueuedRequest, std::io::Error)>> {
        let size = matches!(queued.request, crate::engine::Request::PayloadSize { .. });
        let cancelled = queued.credit.as_ref().and_then(|credit| credit.read_cancelled.clone());
        let state = Arc::new(Mutex::new(State {
            stopped: false,
            interrupted: false,
            socket: None,
            queued: Some(queued),
            response: None,
        }));
        let worker_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("playbit-copy-ipc".into())
            .stack_size(STACK_BYTES)
            .spawn(move || run(&profile, worker_state, events, gate));
        match thread {
            Ok(thread) => Ok(Self { state, thread: Some(thread), size, cancelled }),
            Err(error) => {
                let queued = state
                    .lock()
                    .expect("mutex is not poisoned")
                    .queued
                    .take()
                    .expect("a failed spawn never consumed its request");
                Err(Box::new((queued, error)))
            }
        }
    }
    /// A new read or copy supersedes optional measurement, never a durable copy.
    /// Join only this GUI's socket exchange, retaining the actual admission
    /// credit in its response. The engine independently cancels its read work.
    pub fn interrupt_size(&mut self, gate: &worker_queue::Gate) -> Option<WorkerResponse> {
        if !self.size {
            return None;
        }
        {
            let mut state = self.state.lock().expect("mutex is not poisoned");
            state.interrupted = true;
            if let Some(socket) = &state.socket {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.take(gate)
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    }
    pub fn take(&self, gate: &worker_queue::Gate) -> Option<WorkerResponse> {
        let mut response = self.state.lock().expect("mutex is not poisoned").response.take()?;
        if response.recovery_epoch.is_some_and(|epoch| !gate.accepts(epoch)) {
            response.volume_created = None;
            response.browser_reply = None;
            response.browser_failure = None;
            response.result = Err(worker_queue::INTERRUPTED.into());
        }
        Some(response)
    }
}

fn run(
    profile: &Path,
    state: Arc<Mutex<State>>,
    events: async_channel::Sender<Message>,
    gate: Arc<worker_queue::Gate>,
) {
    let queued = state
        .lock()
        .expect("mutex is not poisoned")
        .queued
        .take()
        .expect("one copy thread consumes its one request");
    let result = exchange(profile, &queued, &state, &gate);
    let mut response = if gate.accepts(queued.epoch) {
        WorkerResponse::from_archive(result)
    } else {
        WorkerResponse::new(Err(worker_queue::INTERRUPTED.into()))
    };
    response.recovery_epoch = Some(queued.epoch);
    if response.result.is_err() && gate.accepts(queued.epoch) {
        response.browser_failure = super::browser::Failure::for_request(&queued.request);
    }
    response.credit = queued.credit;
    {
        let mut state = state.lock().expect("mutex is not poisoned");
        state.socket = None;
        if state.stopped {
            return;
        }
        state.response = Some(response);
    }
    // Never block on notification: a full queue already has ready work, and
    // the owner drains this completion before its next readiness wait.
    let _ = events.try_send(Message::CopyComplete);
}

fn exchange(
    profile: &Path,
    queued: &QueuedRequest,
    state: &Mutex<State>,
    gate: &worker_queue::Gate,
) -> Result<ipc::Response, String> {
    if !gate.accepts(queued.epoch)
        || queued.credit.as_ref().is_some_and(worker_queue::Credit::cancelled)
    {
        return Err(worker_queue::RETIRED.into());
    }
    let connect = || -> crate::engine::Result<UnixStream> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        socket.connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?)?;
        let stream: UnixStream = socket.into();
        stream.set_nonblocking(false)?;
        Ok(stream)
    };
    let mut socket = connect().map_err(|error| format!("Copy did not start: {error}"))?;
    {
        let mut state = state.lock().expect("mutex is not poisoned");
        if state.stopped || state.interrupted || !gate.accepts(queued.epoch) {
            return Err(worker_queue::RETIRED.into());
        }
        state.socket = Some(socket.try_clone().map_err(|error| error.to_string())?);
    }
    // The ordinary IPC deadlines bound this call (30s write / 300s read).
    // Retirement shuts down the exact socket. Accepted copies stay durable;
    // read-only size work is cancelled by the engine when its socket closes.
    ipc::call_stream_archive(&mut socket, &queued.request).map_err(|error| {
        if state.lock().expect("mutex is not poisoned").interrupted {
            return "Size calculation cancelled".into();
        }
        if error.is::<ipc::RejectedReply>() {
            return error.to_string();
        }
        crate::platform::failure::log(profile, "copy IPC", &format!("{error:?}"));
        worker_queue::INTERRUPTED.into()
    })
}
impl Drop for Task {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().expect("mutex is not poisoned");
            state.stopped = true;
            if let Some(socket) = state.socket.take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded one-shot RPC ownership.")]
mod tests {
    use super::*;
    use crate::{
        engine::{
            Request,
            reply::{DiagnosticReply, Reply},
        },
        store::LocalStore,
    };
    use std::{
        os::unix::net::UnixListener,
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn request(socket: &mut UnixStream) -> Request {
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&ipc::read(socket).unwrap())
            .unwrap()
    }
    fn completed(task: &Task, gate: &worker_queue::Gate) -> WorkerResponse {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(response) = task.take(gate) {
                return response;
            }
            assert!(Instant::now() < deadline, "one-shot copy completes");
            std::thread::yield_now();
        }
    }
    #[test]
    fn entry_copy_delivery_fences_retired_results_and_correlates_current_errors() {
        // Retire after a real reply is stored: neither its entry nor failure may reach a new epoch.
        use crate::store::{
            Access,
            volume_index::{EntryAddress, EntryKind},
        };
        for (failed, retire) in [(false, true), (true, false), (true, true)] {
            let directory = tempfile::tempdir().unwrap();
            let store = LocalStore::open(directory.path()).unwrap();
            let access = Access::owner(store.account().id);
            let volume = store.create_volume("Copies", false).unwrap();
            let entry =
                store.create_database(&access, volume.id, "Settings", EntryKind::KvDb {}).unwrap();
            let address = EntryAddress { volume: volume.id, entry: entry.id };
            let copy = store.duplicate_entry(&access, address).unwrap();
            crate::paths::private_directory(directory.path(), "control").unwrap();
            let listener =
                UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                assert!(
                    matches!(request(&mut socket), Request::DuplicateEntry { address: source, .. } if source == address)
                );
                let reply = if failed {
                    Reply::Error { error: "Fixture copy failed".into() }
                } else {
                    Reply::Diagnostic(DiagnosticReply::EntryDuplicated {
                        source: address,
                        entry: copy,
                    })
                };
                ipc::write(
                    &mut socket,
                    &ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap(),
                )
                .unwrap();
            });
            let (send, requests, _) = worker_queue::channel();
            let gate = send.gate();
            send.send(Request::DuplicateEntry { address }).unwrap();
            let Message::Request(queued) = requests.try_recv().unwrap() else {
                panic!("copy admission")
            };
            let epoch = queued.epoch;
            let admission = queued.credit.as_ref().unwrap().admission();
            let task =
                Task::start(directory.path().into(), queued, send.observer_sender(), gate.clone())
                    .unwrap_or_else(|_| panic!("copy RPC starts"));
            server.join().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while task.state.lock().unwrap().response.is_none() {
                assert!(Instant::now() < deadline, "copy response stored before retirement");
                std::thread::yield_now();
            }
            if retire {
                gate.pause();
                gate.resume();
            }
            let response = completed(&task, &gate);
            assert!(response.volume_created.is_none());
            assert_eq!(response.recovery_epoch, Some(epoch));
            assert_eq!(response.credit.as_ref().unwrap().admission(), admission);
            if retire {
                assert_eq!(response.result.unwrap_err(), worker_queue::INTERRUPTED);
                assert!(response.browser_reply.is_none());
                assert!(response.browser_failure.is_none());
            } else {
                assert_eq!(response.result.unwrap_err(), "Fixture copy failed");
                assert!(
                    response.browser_failure.is_some(),
                    "current entry failure retires matching FilesPending"
                );
            }
        }
    }
    #[test]
    fn stale_copy_cannot_adopt_and_full_or_closed_wake_queue_cannot_block_join() {
        // Complete a real RPC after retirement. Its result remains collectable
        // without a wake slot, and carries no adoptable new volume.
        for closed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = LocalStore::open(directory.path()).unwrap();
            let volume = store.create_volume("Original", false).unwrap();
            let copy = store.duplicate_volume(volume.id).unwrap();
            crate::paths::private_directory(directory.path(), "control").unwrap();
            let listener =
                UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
            let (started, waiting) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            let source = volume.id;
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                assert!(
                    matches!(request(&mut socket), Request::DuplicateVolume { volume } if volume == source)
                );
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(2)).unwrap();
                let reply =
                    Reply::Diagnostic(DiagnosticReply::VolumeDuplicated { source, volume: copy });
                ipc::write(
                    &mut socket,
                    &ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap(),
                )
                .unwrap();
                listener.set_nonblocking(true).unwrap();
                assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            });
            let (send, requests, _) = worker_queue::channel();
            let gate = send.gate();
            send.send(Request::DuplicateVolume { volume: source }).unwrap();
            let Message::Request(queued) = requests.try_recv().unwrap() else {
                panic!("copy admission")
            };
            let epoch = queued.epoch;
            let admission = queued.credit.as_ref().unwrap().admission();
            let (events, receive) = async_channel::bounded(1);
            assert!(events.try_send(Message::CopyComplete).is_ok());
            if closed {
                events.close();
            }
            let task = Task::start(directory.path().into(), queued, events, gate.clone())
                .unwrap_or_else(|_| panic!("copy RPC starts"));
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            gate.pause();
            gate.resume();
            release.send(()).unwrap();
            server.join().unwrap();
            let response = completed(&task, &gate);
            assert_eq!(response.result.unwrap_err(), worker_queue::INTERRUPTED);
            assert!(response.volume_created.is_none());
            assert_eq!(response.recovery_epoch, Some(epoch));
            assert!(!gate.accepts(epoch));
            assert_eq!(response.credit.as_ref().unwrap().admission(), admission);
            let started = Instant::now();
            drop(task);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "notification cannot retain the thread"
            );
            drop(receive);
        }
    }
    #[test]
    fn retiring_copy_or_size_shuts_down_its_socket_without_replaying_the_request() {
        // Leave the accepted response unread, then retire this GUI's one socket.
        // The server sees EOF and no replacement mutation.
        use std::io::Read;
        for size in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            crate::paths::private_directory(directory.path(), "control").unwrap();
            let listener =
                UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
            let (started, waiting) = mpsc::sync_channel(1);
            let source = crate::store::Id::generate().unwrap();
            let next = crate::store::Id::generate().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let received = request(&mut socket);
                if size {
                    assert!(
                        matches!(received, Request::PayloadSize { volume, entry: None, .. } if volume == source)
                    );
                } else {
                    assert!(
                        matches!(received, Request::DuplicateVolume { volume } if volume == source)
                    );
                }
                started.send(()).unwrap();
                assert_eq!(socket.read(&mut [0]).unwrap(), 0);
                if size {
                    let (mut next_socket, _) = listener.accept().unwrap();
                    assert!(
                        matches!(request(&mut next_socket), Request::PayloadSize { volume, entry: None, .. } if volume == next)
                    );
                    let reply = Reply::Diagnostic(DiagnosticReply::PayloadSize {
                        volume: next,
                        entry: None,
                        bytes: 42,
                    });
                    ipc::write(
                        &mut next_socket,
                        &ipc::codec::encode(&reply, ipc::codec::MAX_ARCHIVE).unwrap(),
                    )
                    .unwrap();
                }
                listener.set_nonblocking(true).unwrap();
                assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            });
            let (send, requests, _) = worker_queue::channel();
            let gate = send.gate();
            let size_read = if size {
                Some(send.payload_size(source, None).unwrap())
            } else {
                send.send(Request::DuplicateVolume { volume: source }).unwrap();
                None
            };
            let Message::Request(queued) = requests.try_recv().unwrap() else {
                panic!("copy admission")
            };
            let admission = queued.credit.as_ref().unwrap().admission();
            let mut task =
                Task::start(directory.path().into(), queued, send.observer_sender(), gate.clone())
                    .unwrap_or_else(|_| panic!("copy RPC starts"));
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            let started = Instant::now();
            if size {
                size_read.as_ref().unwrap().cancel();
                assert!(task.cancelled());
                let retired = task.interrupt_size(&gate).unwrap();
                assert!(retired.browser_failure.is_some());
                assert!(retired.result.is_err());
                assert_eq!(retired.credit.as_ref().unwrap().admission(), admission);
            } else {
                assert!(task.interrupt_size(&gate).is_none());
            }
            drop(task);
            assert!(started.elapsed() < Duration::from_secs(2), "retirement cancels its RPC wait");
            if size {
                // A replacement browser's different target reuses the single
                // slot and retains its own admission after the old socket exits.
                let next_read = send.payload_size(next, None).unwrap();
                let next_admission = next_read.admission;
                let queued = loop {
                    match requests.try_recv().unwrap() {
                        Message::Request(queued) => break queued,
                        Message::CopyComplete => {}
                        _ => panic!("only the replacement read is queued"),
                    }
                };
                let task = Task::start(
                    directory.path().into(),
                    queued,
                    send.observer_sender(),
                    gate.clone(),
                )
                .unwrap_or_else(|_| panic!("replacement size starts"));
                size_read.as_ref().unwrap().cancel();
                assert!(!task.cancelled(), "an old token cannot retire its replacement");
                let response = completed(&task, &gate);
                assert_eq!(response.credit.as_ref().unwrap().admission(), next_admission);
                assert_ne!(next_admission, admission);
                assert!(matches!(response.browser_reply,
                    Some(Reply::Diagnostic(DiagnosticReply::PayloadSize { volume, entry: None, bytes: 42 })) if volume == next));
                drop(task);
            }
            gate.pause();
            server.join().unwrap();
            while let Ok(message) = requests.try_recv() {
                assert!(matches!(message, Message::CopyComplete));
            }
        }
    }
}
