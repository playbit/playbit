//! One readiness-driven tool socket per GUI session instance. Tool I/O never
//! runs on AppKit or the guest worker; bounded immutable snapshots stay local.
use super::worker::{Handle, Output};
use crate::{
    engine::{
        Result,
        ipc::{self, codec},
    },
    store::Id,
    ui::{
        app_control::{
            self, ArchivedOperation, ArchivedRequest, LogBatch, LogCursor, Operation, Reply,
        },
        desktop::scene::Input,
    },
};
use polling::{Event, Events, Poller};
use std::{
    io::{ErrorKind, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
const MAX_SOCKETS: usize = 8;
const MAX_LOG_WAITERS: usize = 4;
const MAX_WRITES: usize = 8 * 1024 * 1024;
const _: () = assert!(app_control::REPLY_LIMIT <= MAX_WRITES);
const _: () = assert!(app_control::REPLY_LIMIT <= codec::MAX_MESSAGE);
const IO_QUANTUM: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LOG_BYTES: usize = 64 * 1024;
const _: () = assert!(super::super::alert::MAX_FAILURE_BYTES <= MAX_LOG_BYTES);

struct Snapshot {
    generation: u64,
    build: String,
    scene: Arc<Vec<u8>>,
    logs: Vec<app_control::LogEntry>,
    next: u64,
    handle: Option<Handle>,
    failure: Option<String>,
    quitting: bool,
}
fn retain_logs(logs: &mut Vec<app_control::LogEntry>) {
    let mut bytes = logs.iter().map(|entry| entry.message.len()).sum::<usize>();
    let mut first = 0;
    while logs.len() - first > 256 || bytes > MAX_LOG_BYTES {
        bytes -= logs[first].message.len();
        first += 1;
    }
    logs.drain(..first);
}
#[derive(Clone)]
pub(super) struct Publisher {
    instance: Id,
    snapshot: Arc<Mutex<Snapshot>>,
    poller: Arc<Poller>,
}
impl Publisher {
    pub fn opened(&self, generation: u64, build: String, handle: Handle) {
        let mut state = self.snapshot.lock().expect("app control snapshot lock poisoned");
        *state = Snapshot {
            generation,
            build,
            scene: Arc::new(vec![]),
            logs: vec![],
            next: 0,
            handle: Some(handle),
            failure: None,
            quitting: false,
        };
        drop(state);
        self.notify();
    }
    pub fn frame(&self, scene: Option<Vec<u8>>, logs: &[String]) {
        if scene.is_none() && logs.is_empty() {
            return;
        }
        let mut state = self.snapshot.lock().expect("app control snapshot lock poisoned");
        if let Some(scene) = scene {
            assert!(
                scene.len() <= playbit_api::abi::syscall::MAX_CALL_BYTES,
                "runtime validated scene size"
            );
            state.scene = Arc::new(scene);
        }
        state.next =
            state.next.checked_add(logs.len() as u64).expect("app log sequence cannot wrap");
        state.logs.extend(logs.iter().map(|message| {
            app_control::LogEntry::recorded(message.clone(), app_control::LogSeverity::Info)
        }));
        retain_logs(&mut state.logs);
        drop(state);
        self.notify();
    }
    pub fn stopped(&self, error: Option<&str>) {
        let mut state = self.snapshot.lock().expect("app control snapshot lock poisoned");
        state.handle = None;
        state.failure = error.map(super::super::alert::bounded_failure_details);
        if let Some(message) = state.failure.clone() {
            state.next = state.next.checked_add(1).expect("app log sequence cannot wrap");
            state
                .logs
                .push(app_control::LogEntry::recorded(message, app_control::LogSeverity::Error));
            retain_logs(&mut state.logs);
        }
        drop(state);
        self.notify();
    }
    fn notify(&self) {
        self.poller.notify().expect("live app control readiness notification");
    }
    fn respond(
        &self,
        generation: u64,
        operation: Operation,
        outputs: &async_channel::Sender<Output>,
    ) -> Result<Option<Reply>> {
        let mut state = self.snapshot.lock().expect("app control snapshot lock poisoned");
        // Process claim precedes the first admitted turn. Initial logs may wait;
        // other controls refuse until the snapshot has an owner generation.
        if state.generation == 0 {
            if matches!(operation, Operation::Logs { after: None }) {
                return Ok(None);
            }
            return Err("The app controls are not ready yet; this request was not applied".into());
        }
        let follow_reset = matches!(operation, Operation::Logs { after: Some(_) })
            && state.generation > generation;
        if state.generation != generation && !follow_reset {
            return Err(
                "The session instance changed owner generation; no action was repeated".into()
            );
        }
        match operation {
            Operation::Logs { after } => {
                let cursor = LogCursor {
                    instance: self.instance,
                    generation: state.generation,
                    next: state.next,
                };
                let same = after.is_some_and(|old| {
                    old.instance == cursor.instance && old.generation == cursor.generation
                });
                let requested = if same { after.map(|cursor| cursor.next) } else { None };
                let Some((start, dropped)) =
                    app_control::log_window(cursor.next, state.logs.len(), requested)?
                else {
                    // The endpoint outlives a recoverable VM stop. A silent
                    // log follow waits for explicit reconstruction; terminal
                    // owner exit closes it when Server drops, without a timer.
                    return Ok(None);
                };
                Ok(Some(Reply::Logs(LogBatch {
                    cursor,
                    lines: state.logs[start..].to_vec(),
                    dropped,
                    reset: after.is_some() && !same,
                })))
            }
            Operation::Frame => {
                if let Some(error) = &state.failure {
                    return Err(error.clone().into());
                }
                if state.scene.is_empty() {
                    return Err("The app has not produced a scene".into());
                }
                if state.scene.len() > app_control::MAX_SCENE_BYTES {
                    return Err("The app scene exceeds the 1 MiB tool snapshot limit".into());
                }
                let scene = state.scene.clone();
                let build = state.build.clone();
                drop(state);
                Ok(Some(Reply::Frame {
                    instance: self.instance,
                    generation,
                    build,
                    scene: scene.as_ref().clone(),
                }))
            }
            Operation::Input { kind, target, data } => {
                if state.quitting {
                    return Err("The app is quitting; this input did not start".into());
                }
                let handle = state.handle.as_ref().ok_or("The app input owner has stopped")?;
                let kind = match kind {
                    2 => playbit_api::abi::syscall::EventKind::Click,
                    3 => playbit_api::abi::syscall::EventKind::Submit,
                    _ => return Err("App controls accept only click or submit input".into()),
                };
                if !handle.send(Input { kind, target, data, revision: None }, None) {
                    return Err(
                        "App input queue is full or closed; this input did not start".into()
                    );
                }
                Ok(Some(Reply::Accepted { instance: self.instance, generation }))
            }
            Operation::Show { presentation } => {
                if state.quitting {
                    return Err("The app is quitting; its interface was not restored".into());
                }
                outputs
                    .try_send(Output::ShowRequested { owner_generation: generation, presentation })
                    .map_err(|_| "The app lifecycle queue is busy; Show was not requested")?;
                Ok(Some(Reply::Accepted { instance: self.instance, generation }))
            }
            Operation::Quit => {
                if state.quitting {
                    return Err("A Quit intent is already accepted for this app".into());
                }
                // This asks the actual AppKit lifecycle to deactivate and drain.
                // Accepted is intent admission, never a drain/saved-work claim.
                outputs
                    .try_send(Output::QuitRequested)
                    .map_err(|_| "The app lifecycle queue is busy; Quit was not requested")?;
                state.quitting = true;
                Ok(Some(Reply::Accepted { instance: self.instance, generation }))
            }
        }
    }
}

pub(super) struct Server {
    publisher: Publisher,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    pub fn start(
        profile: &Path,
        instance: Id,
        outputs: async_channel::Sender<Output>,
    ) -> Result<Self> {
        let path = app_control::socket_path(profile, instance)?;
        // Never unlink another listener or infer that an existing endpoint is dead.
        let listener = UnixListener::bind(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let endpoint =
            Endpoint { path: path.clone(), device: metadata.dev(), inode: metadata.ino() };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let publisher = Publisher {
            instance,
            snapshot: Arc::new(Mutex::new(Snapshot {
                generation: 0,
                build: String::new(),
                scene: Arc::new(vec![]),
                logs: vec![],
                next: 0,
                handle: None,
                failure: None,
                quitting: false,
            })),
            poller: Arc::new(Poller::new()?),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_publisher = publisher.clone();
        let thread = std::thread::Builder::new()
            .name("guest-tool-io".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let mut reactor = Reactor {
                    listener,
                    publisher: worker_publisher,
                    outputs,
                    pending: Vec::with_capacity(MAX_SOCKETS),
                    token: 1,
                };
                if let Err(error) = reactor.run(&worker_stop) {
                    crate::diagnostic!("App control endpoint stopped: {error}");
                }
                drop(reactor);
                drop(endpoint);
            })?;
        Ok(Self { publisher, stop, thread: Some(thread) })
    }
    pub fn publisher(&self) -> Publisher {
        self.publisher.clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.publisher.notify();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("app control I/O owner does not panic");
        }
    }
}
struct Endpoint {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
enum State {
    Reading { header: [u8; codec::HEADER_LEN], received: usize, body: codec::Bytes },
    Logs { generation: u64, after: Option<LogCursor> },
    Writing { bytes: codec::Bytes, offset: usize },
}
struct Pending {
    stream: UnixStream,
    token: usize,
    state: State,
    deadline: Option<Instant>,
}
impl Pending {
    fn interest(&self) -> Event {
        match self.state {
            State::Writing { .. } => Event::writable(self.token),
            _ => Event::readable(self.token),
        }
    }
}
struct Reactor {
    listener: UnixListener,
    publisher: Publisher,
    outputs: async_channel::Sender<Output>,
    pending: Vec<Pending>,
    token: usize,
}
impl Reactor {
    #[allow(
        unsafe_code,
        reason = "Socket registrations are deleted before their owned descriptors close."
    )]
    fn run(&mut self, stop: &AtomicBool) -> Result<()> {
        // SAFETY: Reactor Drop deletes the registration before listener drop.
        unsafe {
            self.publisher.poller.add(&self.listener, Event::readable(0))?;
        }
        let mut events = Events::new();
        while !stop.load(Ordering::Acquire) {
            for index in (0..self.pending.len()).rev() {
                let expired =
                    self.pending[index].deadline.is_some_and(|deadline| deadline <= Instant::now());
                if expired || self.finish_logs(index).is_err() {
                    self.close(index)?;
                }
            }
            let timeout = self
                .pending
                .iter()
                .filter_map(|pending| pending.deadline)
                .min()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            events.clear();
            self.publisher.poller.wait(&mut events, timeout)?;
            for event in events.iter() {
                if event.key == 0 {
                    self.accept()?;
                } else if let Some(index) =
                    self.pending.iter().position(|pending| pending.token == event.key)
                {
                    match self.progress(index) {
                        Ok(true) => self
                            .publisher
                            .poller
                            .modify(&self.pending[index].stream, self.pending[index].interest())?,
                        Ok(false) | Err(_) => self.close(index)?,
                    }
                }
            }
        }
        Ok(())
    }
    fn close(&mut self, index: usize) -> Result<()> {
        self.publisher.poller.delete(&self.pending[index].stream)?;
        self.pending.swap_remove(index);
        Ok(())
    }
    #[allow(
        unsafe_code,
        reason = "Reactor owns every socket until its poll registration is removed."
    )]
    fn accept(&mut self) -> Result<()> {
        for _ in 0..MAX_SOCKETS {
            let (stream, _) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if self.pending.len() == MAX_SOCKETS || ipc::verify_peer(&stream).is_err() {
                continue;
            }
            stream.set_nonblocking(true)?;
            let token = self.token;
            self.token = token.checked_add(1).ok_or("App control connection tokens exhausted")?;
            let pending = Pending {
                stream,
                token,
                state: State::Reading {
                    header: [0; codec::HEADER_LEN],
                    received: 0,
                    body: codec::Bytes::new(),
                },
                deadline: Some(Instant::now() + IO_TIMEOUT),
            };
            // SAFETY: close()/Drop remove this registration before stream drop.
            unsafe {
                self.publisher.poller.add(&pending.stream, pending.interest())?;
            }
            self.pending.push(pending);
        }
        self.publisher.poller.modify(&self.listener, Event::readable(0))?;
        Ok(())
    }
    fn progress(&mut self, index: usize) -> Result<bool> {
        match &mut self.pending[index] {
            Pending { stream, state: State::Reading { header, received, body }, .. } => {
                if *received < header.len() {
                    match stream.read(&mut header[*received..]) {
                        Ok(0) => return Ok(false),
                        Ok(count) => *received += count,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(true),
                        Err(error) => return Err(error.into()),
                    }
                    if *received >= 4 {
                        let length = codec::archive_length(u32::from_le_bytes(
                            header[..4].try_into().expect("four-byte length"),
                        ))?;
                        if length > app_control::REQUEST_LIMIT {
                            return Err("App control request exceeds its limit".into());
                        }
                    }
                    if *received != header.len() {
                        return Ok(true);
                    }
                    codec::check_prefix(header[4..].try_into().expect("fixed prefix"))?;
                    let length = codec::archive_length(u32::from_le_bytes(
                        header[..4].try_into().expect("four-byte length"),
                    ))?;
                    body.resize(length, 0);
                }
                let start = *received - codec::HEADER_LEN;
                let end = (start + IO_QUANTUM).min(body.len());
                if start < end {
                    match stream.read(&mut body[start..end]) {
                        Ok(0) => return Ok(false),
                        Ok(count) => *received += count,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(true),
                        Err(error) => return Err(error.into()),
                    }
                }
                if *received - codec::HEADER_LEN == body.len() {
                    self.dispatch(index)?;
                }
                Ok(true)
            }
            Pending { stream, state: State::Logs { .. }, .. } => {
                // Only cancellation/closure is meaningful after one request.
                match stream.read(&mut [0]) {
                    Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(true),
                    _ => Ok(false),
                }
            }
            Pending { stream, state: State::Writing { bytes, offset }, .. } => {
                let end = (*offset + IO_QUANTUM).min(bytes.len());
                match stream.write(&bytes[*offset..end]) {
                    Ok(0) => Ok(false),
                    Ok(count) => {
                        *offset += count;

                        Ok(*offset != bytes.len())
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(true),
                    Err(error) => Err(error.into()),
                }
            }
        }
    }
    fn dispatch(&mut self, index: usize) -> Result<()> {
        let _span = crate::trace::Span::new("ui.control.dispatch");
        let State::Reading { body, .. } = &self.pending[index].state else {
            unreachable!("dispatch owns a complete request")
        };
        let request = codec::access::<ArchivedRequest>(body)?;
        if rkyv::api::low::deserialize::<Id, rkyv::rancor::Error>(&request.instance)?
            != self.publisher.instance
            || (matches!(&request.operation, ArchivedOperation::Input { data, .. } if data.len() > playbit_api::abi::syscall::MAX_EVENT_BYTES))
        {
            return self.reply(
                index,
                Reply::Error { error: "Invalid session-instance binding or input size".into() },
            );
        }
        // Reserve the largest single response before any effect admission. A
        // slow reader cannot turn a known pre-admission refusal into a mutation.
        if self.write_bytes() + app_control::REPLY_LIMIT > MAX_WRITES {
            return self.reply(
                index,
                Reply::Error {
                    error: "App control response budget is busy; this request did not start".into(),
                },
            );
        }
        let generation = request.generation.to_native();
        let operation =
            rkyv::api::low::deserialize::<Operation, rkyv::rancor::Error>(&request.operation)?;
        let wait = match &operation {
            Operation::Logs { after } => Some(*after),
            _ => None,
        };
        if wait.is_some()
            && self
                .pending
                .iter()
                .filter(|pending| matches!(pending.state, State::Logs { .. }))
                .count()
                == MAX_LOG_WAITERS
        {
            return self.reply(
                index,
                Reply::Error {
                    error: "App log wait capacity is full; this wait did not start".into(),
                },
            );
        }
        match self.publisher.respond(generation, operation, &self.outputs) {
            Ok(Some(reply)) => self.reply(index, reply),
            Ok(None) => {
                self.pending[index].state =
                    State::Logs { generation, after: wait.expect("only a log request can wait") };
                self.pending[index].deadline = None;
                Ok(())
            }
            Err(error) => self.reply(
                index,
                Reply::Error { error: error.to_string().chars().take(1024).collect() },
            ),
        }
    }
    fn finish_logs(&mut self, index: usize) -> Result<()> {
        let State::Logs { generation, after } = self.pending[index].state else {
            return Ok(());
        };
        if self.write_bytes() + app_control::REPLY_LIMIT > MAX_WRITES {
            return Ok(());
        }
        let reply =
            match self.publisher.respond(generation, Operation::Logs { after }, &self.outputs) {
                Ok(Some(reply)) => reply,
                Ok(None) => return Ok(()),
                Err(error) => {
                    Reply::Error { error: error.to_string().chars().take(1024).collect() }
                }
            };
        self.reply(index, reply)?;
        self.publisher
            .poller
            .modify(&self.pending[index].stream, self.pending[index].interest())?;
        Ok(())
    }
    fn write_bytes(&self) -> usize {
        self.pending
            .iter()
            .map(|pending| match &pending.state {
                State::Writing { bytes, .. } => bytes.capacity(),
                _ => 0,
            })
            .sum()
    }
    fn reply(&mut self, index: usize, reply: Reply) -> Result<()> {
        let archive = codec::encode(&reply, app_control::REPLY_LIMIT - codec::HEADER_LEN)?;
        let mut bytes = codec::Bytes::with_capacity(archive.len() + codec::HEADER_LEN);
        bytes.extend_from_slice(&codec::frame_header(archive.len())?);
        bytes.extend_from_slice(&archive);
        self.pending[index].state = State::Writing { bytes, offset: 0 };
        self.pending[index].deadline = Some(Instant::now() + IO_TIMEOUT);
        assert!(self.write_bytes() <= MAX_WRITES, "app control response credit");
        Ok(())
    }
}
impl Drop for Reactor {
    fn drop(&mut self) {
        for pending in &self.pending {
            let _ = self.publisher.poller.delete(&pending.stream);
        }
        let _ = self.publisher.poller.delete(&self.listener);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded snapshot and log replies.")]
mod tests {
    use super::*;

    #[test]
    fn crash_reports_survive_failure_transport_and_log_encoding() {
        // A complete report can consume the log ring, evicting older lines.
        // Oversized UTF-8 reports remain bounded and explicitly mark truncation.
        let publisher = Publisher {
            instance: Id::from_bytes([1; 16]),
            snapshot: Arc::new(Mutex::new(Snapshot {
                generation: 1,
                build: String::new(),
                scene: Arc::new(vec![]),
                logs: vec![],
                next: 0,
                handle: None,
                failure: None,
                quitting: false,
            })),
            poller: Arc::new(Poller::new().unwrap()),
        };
        let (outputs, _) = async_channel::bounded(1);
        for report in ["é".repeat(MAX_LOG_BYTES / 2), "🌱".repeat(MAX_LOG_BYTES)] {
            publisher.frame(None, &["before crash".into()]);
            let failure: super::super::super::alert::Failure = report.clone().into();
            publisher.stopped(Some(&failure.details));
            let reply =
                publisher.respond(1, Operation::Logs { after: None }, &outputs).unwrap().unwrap();
            let bytes =
                codec::encode(&reply, app_control::REPLY_LIMIT - codec::HEADER_LEN).unwrap();
            let archived = codec::access::<app_control::ArchivedReply>(&bytes).unwrap();
            let Reply::Logs(batch) =
                rkyv::deserialize::<Reply, rkyv::rancor::Error>(archived).unwrap()
            else {
                panic!("stopped owner retains readable crash logs")
            };
            assert!(batch.dropped > 0, "the crash report displaces older logs");
            assert_eq!(batch.lines.len(), 1);
            let crash = &batch.lines[0];
            assert_eq!(crash.severity, app_control::LogSeverity::Error);
            assert!(crash.message.len() <= MAX_LOG_BYTES);
            if report.len() <= MAX_LOG_BYTES {
                assert_eq!(crash.message, report);
            } else {
                assert!(crash.message.ends_with("\n[failure report truncated at 64 KiB]"));
            }
            assert_eq!(
                publisher.respond(1, Operation::Frame, &outputs).unwrap_err().to_string(),
                crash.message
            );
        }
    }

    #[test]
    fn large_presentations_keep_logs_available_and_fail_snapshot_export_explicitly() {
        let publisher = Publisher {
            instance: Id::from_bytes([1; 16]),
            snapshot: Arc::new(Mutex::new(Snapshot {
                generation: 1,
                build: String::new(),
                scene: Arc::new(vec![]),
                logs: vec![],
                next: 0,
                handle: None,
                failure: None,
                quitting: false,
            })),
            poller: Arc::new(Poller::new().unwrap()),
        };
        let (outputs, _) = async_channel::bounded(1);
        publisher.frame(
            Some(vec![0; app_control::MAX_SCENE_BYTES + 1]),
            &["large frame presented".to_owned()],
        );
        let error = publisher.respond(1, Operation::Frame, &outputs).unwrap_err();
        assert!(error.to_string().contains("tool snapshot limit"));
        let Some(Reply::Logs(batch)) =
            publisher.respond(1, Operation::Logs { after: None }, &outputs).unwrap()
        else {
            panic!("large presentations retain readable logs")
        };
        assert_eq!(batch.lines[0].message, "large frame presented");
        publisher.frame(Some(vec![0; app_control::MAX_SCENE_BYTES]), &[]);
        let Some(Reply::Frame { scene, .. }) =
            publisher.respond(1, Operation::Frame, &outputs).unwrap()
        else {
            panic!("the exact tool snapshot limit fits")
        };
        assert_eq!(scene.len(), app_control::MAX_SCENE_BYTES);
    }
}
