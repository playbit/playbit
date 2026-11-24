//! One event-driven log observer for the selected session. Exact GUI owners
//! serve logs; the engine serves only ownership metadata and change readiness.
use crate::{
    engine::{Request, ipc, reply::Reply},
    ui::app_control,
};
use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
const MAX_OBSERVERS: usize = 2; // Current selection plus its canceling predecessor.
static OBSERVERS: AtomicUsize = AtomicUsize::new(0);
struct Credit;
impl Drop for Credit {
    fn drop(&mut self) {
        OBSERVERS.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Default)]
struct State {
    stopped: bool,
    socket: Option<UnixStream>,
}
pub(super) struct Watch(Arc<Mutex<State>>);
impl Drop for Watch {
    fn drop(&mut self) {
        let mut state = self.0.lock().expect("log observer lock poisoned");
        state.stopped = true;
        if let Some(socket) = state.socket.take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
fn register(state: &Arc<Mutex<State>>, socket: UnixStream) -> Result<(), String> {
    let mut state = state.lock().expect("log observer lock poisoned");
    if state.stopped {
        return Err("Log observation canceled".into());
    }
    state.socket = Some(socket);
    Ok(())
}
pub(super) enum Update {
    Batch(app_control::LogBatch),
    Notice(&'static str),
    Stopped,
    Running(crate::store::Id),
}

pub(super) fn retain(
    lines: &mut Vec<app_control::LogEntry>,
    cursor: &mut Option<app_control::LogCursor>,
    update: Update,
) -> usize {
    match update {
        // Connection and lifecycle notices are presented separately by LogView.
        Update::Stopped | Update::Running(_) | Update::Notice(_) => 0,
        Update::Batch(batch) => {
            // A new runtime resets its source cursor, not this session's launch-local
            // history. The transport already supplies only that source's unread suffix.
            if batch.dropped > 0 {
                lines.push(app_control::LogEntry::notice(format!(
                    "{} earlier log messages are no longer retained.",
                    batch.dropped
                )));
            }
            lines.extend(batch.lines);
            let mut bytes = lines.iter().map(|entry| entry.message.len()).sum::<usize>();
            let mut first = 0;
            while lines.len() - first > 256 || bytes > 64 * 1024 {
                bytes -= lines[first].message.len();
                first += 1;
            }
            lines.drain(..first);
            *cursor = Some(batch.cursor);
            first
        }
    }
}
impl Watch {
    pub fn start(
        profile: PathBuf,
        session: String,
        instance: Option<crate::store::Id>,
        retained_cursor: Option<app_control::LogCursor>,
    ) -> Result<(Self, async_channel::Receiver<Update>), String> {
        OBSERVERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_OBSERVERS).then_some(count + 1)
            })
            .map_err(|_| {
                "The previous log observer is closing. Select the session again.".to_string()
            })?;
        let credit = Credit;
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let (send, receive) = async_channel::bounded(1);
        std::thread::Builder::new().name("playbit-session-logs".into()).stack_size(128 * 1024).spawn(move || {
            let _credit = credit;
            let result = (|| -> Result<(), String> {
                let mut after = None;
                let mut retained_cursor = retained_cursor;
                loop {
                    let wait = ipc::ChangeWait::connect(&profile, after).map_err(|e| e.to_string())?;
                    register(&shared, wait.cancellation_handle().map_err(|e| e.to_string())?)?;
                    after = Some(wait.receive().map_err(|e| e.to_string())?.token);
                    if shared.lock().expect("log observer lock poisoned").stopped { return Ok(()); }
                    let response = ipc::call_archive(&profile, &Request::InstanceList { session: session.clone() }).and_then(ipc::Response::into_owned).map_err(|e| e.to_string())?;
                    let Reply::InstanceOwners(owners) = response else { return Err("Invalid app owner response".into()); };
                    let selected = match instance {
                        Some(id) => owners.iter().find(|owner| owner.instance.id == id),
                        None => owners.first(),
                    };
                    let owner = selected.filter(|owner| {
                        owner.process.is_some()
                            && owner.instance.state == crate::engine::session_instance::State::Owned
                    });
                    let Some(owner) = owner else {
                        send.send_blocking(Update::Stopped).map_err(|e| e.to_string())?;
                        continue;
                    };
                    send.send_blocking(Update::Running(owner.instance.id)).map_err(|e| e.to_string())?;
                    let mut cursor = retained_cursor.take().filter(|cursor| cursor.instance == owner.instance.id);
                    loop {
                        let reply = (|| -> Result<_, String> {
                            let exchange = app_control::Exchange::connect(&profile, owner, app_control::Operation::Logs { after: cursor }).map_err(|e| e.to_string())?;
                            register(&shared, exchange.cancellation_handle().map_err(|e| e.to_string())?)?;
                            exchange.receive().map_err(|e| e.to_string())
                        })();
                        if shared.lock().expect("log observer lock poisoned").stopped { return Ok(()); }
                        match reply {
                            Ok(app_control::Reply::Logs(batch)) => {
                                cursor = Some(batch.cursor);
                                send.send_blocking(Update::Batch(batch)).map_err(|e| e.to_string())?;
                            }
                            Err(_) => {
                                send.send_blocking(Update::Notice("App logs are unavailable. Earlier log messages may be missing.")).map_err(|e| e.to_string())?;
                                // Resolve again only after an actual engine change.
                                // A retired or rejected owner never causes polling.
                                break;
                            }
                            _ => return Err("Invalid app log response".into()),
                        }
                    }
                    retained_cursor = cursor;
                }
            })();
            if result.is_err() && !shared.lock().expect("log observer lock poisoned").stopped {
                let _ = send.send_blocking(Update::Notice("Log observation stopped. Select the session again to attach."));
            }
        }).map_err(|e| e.to_string())?;
        Ok((Self(state), receive))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests own isolated cancellation sockets and bounded batches."
)]
mod tests {
    use super::*;
    use crate::store::Id;
    use std::io::Read;
    fn batch(generation: u64, next: u64, lines: Vec<String>) -> Update {
        Update::Batch(app_control::LogBatch {
            cursor: app_control::LogCursor { instance: Id::from_bytes([1; 16]), generation, next },
            lines: lines.into_iter().map(app_control::LogEntry::notice).collect(),
            dropped: 0,
            reset: false,
        })
    }
    #[test]
    fn restart_and_owner_reconstruction_preserve_session_history() {
        let mut lines = vec![];
        let mut cursor = None;
        retain(&mut lines, &mut cursor, Update::Notice("No app owner"));
        retain(&mut lines, &mut cursor, batch(1, 1, vec!["first".into()]));
        assert_eq!(lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(), ["first"]);
        retain(&mut lines, &mut cursor, batch(1, 2, vec!["second".into()]));
        assert_eq!(
            lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        let mut reconstruction = batch(2, 1, vec!["reconstructed".into()]);
        let Update::Batch(ref mut data) = reconstruction else { unreachable!() };
        data.reset = true;
        retain(&mut lines, &mut cursor, reconstruction);
        assert_eq!(
            lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
            ["first", "second", "reconstructed"]
        );
        retain(&mut lines, &mut cursor, Update::Notice("Observation stopped"));
        let mut restart = batch(1, 1, vec!["restarted".into()]);
        let Update::Batch(ref mut data) = restart else { unreachable!() };
        data.cursor.instance = Id::from_bytes([2; 16]);
        let restarted_cursor = data.cursor;
        retain(&mut lines, &mut cursor, restart);
        assert_eq!(
            lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
            ["first", "second", "reconstructed", "restarted"]
        );
        assert_eq!(cursor, Some(restarted_cursor));
    }
    #[test]
    fn snapshots_bound_line_count_and_bytes_and_report_source_loss() {
        let mut lines = vec![];
        let mut cursor = None;
        retain(&mut lines, &mut cursor, batch(1, 256, vec!["a".repeat(256); 256]));
        retain(&mut lines, &mut cursor, batch(1, 512, vec!["b".repeat(512); 128]));
        assert_eq!(lines.len(), 128);
        assert_eq!(lines.iter().map(|entry| entry.message.len()).sum::<usize>(), 64 * 1024);
        let mut update = batch(2, 100, vec!["current".into()]);
        let Update::Batch(ref mut batch) = update else { unreachable!() };
        batch.dropped = 99;
        retain(&mut lines, &mut cursor, update);
        assert!(lines.iter().any(|line| line.message.contains("99 earlier log messages")));
    }
    #[test]
    fn dropping_a_watch_interrupts_a_registered_idle_socket() {
        let state = Arc::new(Mutex::new(State::default()));
        let (mut peer, observer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        register(&state, observer).unwrap();
        drop(Watch(state.clone()));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert!(state.lock().unwrap().stopped);
    }
    #[test]
    fn cancellation_before_socket_registration_prevents_an_idle_wait() {
        let state = Arc::new(Mutex::new(State::default()));
        drop(Watch(state.clone()));
        let (_peer, observer) = UnixStream::pair().unwrap();
        assert!(register(&state, observer).is_err());
        assert!(state.lock().unwrap().socket.is_none());
    }

    #[allow(unsafe_code, reason = "The bounded fixture readiness wait borrows its live listener.")]
    fn accept(listener: &std::os::unix::net::UnixListener) -> UnixStream {
        use std::os::fd::AsRawFd;
        let mut descriptor =
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: The listener and one initialized descriptor outlive poll.
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 5000) }, 1, "fixture request deadline");
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        ipc::verify_peer(&stream).unwrap();
        stream
    }
    fn engine_request(listener: &std::os::unix::net::UnixListener) -> (UnixStream, Request) {
        let mut stream = accept(listener);
        let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
            &ipc::read(&mut stream).unwrap(),
        )
        .unwrap();
        (stream, request)
    }
    fn engine_reply(stream: &mut UnixStream, reply: Reply) {
        ipc::write(stream, &ipc::codec::encode(&reply, 64 * 1024).unwrap()).unwrap();
    }
    fn log_request(
        listener: &std::os::unix::net::UnixListener,
        instance: Id,
        next: Option<u64>,
    ) -> UnixStream {
        let mut stream = accept(listener);
        let request = rkyv::api::low::from_bytes::<app_control::Request, rkyv::rancor::Error>(
            &ipc::codec::read(&mut stream, app_control::REQUEST_LIMIT).unwrap(),
        )
        .unwrap();
        assert_eq!(request.instance, instance);
        assert_eq!(request.generation, 1);
        let app_control::Operation::Logs { after } = request.operation else {
            panic!("log request")
        };
        assert_eq!(after.map(|cursor| cursor.next), next);
        stream
    }
    fn log_reply(stream: &mut UnixStream, instance: Id, line: &str) {
        let reply = app_control::Reply::Logs(app_control::LogBatch {
            cursor: app_control::LogCursor { instance, generation: 1, next: 1 },
            lines: vec![app_control::LogEntry::recorded(
                line.into(),
                app_control::LogSeverity::Info,
            )],
            dropped: 0,
            reset: false,
        });
        ipc::codec::write(stream, &ipc::codec::encode(&reply, app_control::REPLY_LIMIT).unwrap())
            .unwrap();
    }
    #[test]
    fn retired_log_endpoint_waits_for_engine_change_then_attaches_the_exact_new_owner() {
        use crate::engine::{
            ChangeToken,
            session_instance::{LaunchOwner, SessionInstance, State as InstanceState},
        };
        use std::{os::unix::net::UnixListener, sync::mpsc, time::Duration};
        for error_reply in [false, true] {
            let profile = tempfile::tempdir().unwrap();
            crate::paths::private_directory(profile.path(), "control").unwrap();
            let engine =
                UnixListener::bind(profile.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
            engine.set_nonblocking(true).unwrap();
            let token = ChangeToken { generation: Id::from_bytes([7; 16]), sequence: 1 };
            let replacement_token = ChangeToken { sequence: 2, ..token };
            let (peer, _other) = UnixStream::pair().unwrap();
            let process = crate::engine::process_owner::Owner::capture(&peer).unwrap().identity;
            let owner = |value| LaunchOwner {
                presentation: 0,
                instance: SessionInstance {
                    restore_host_mounts: false,
                    id: Id::from_bytes([value; 16]),
                    launch_request: Id::from_bytes([3; 16]),
                    account: Id::from_bytes([4; 16]),
                    session: "fixture-session".into(),
                    build: crate::identifier::encode(&[0xaa; 32]),
                    root: Id::from_bytes([8; 16]),

                    extra_volumes: vec![],
                    host_mounts: Box::default(),
                    owner_generation: 1,
                    state: InstanceState::Owned,
                },
                process: Some(process.clone()),
            };
            let old = owner(1);
            let new = owner(2);
            let old_id = old.instance.id;
            let new_id = new.instance.id;
            let old_path = app_control::socket_path(profile.path(), old_id).unwrap();
            let new_path = app_control::socket_path(profile.path(), new_id).unwrap();
            let old_app = UnixListener::bind(&old_path).unwrap();
            let new_app = UnixListener::bind(&new_path).unwrap();
            let (waiting, waited) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            let engine_thread = std::thread::spawn(move || {
                let (mut stream, request) = engine_request(&engine);
                assert!(matches!(request, Request::WaitChanges { after: None }));
                engine_reply(&mut stream, Reply::Changes(token));
                let (mut stream, request) = engine_request(&engine);
                assert!(
                    matches!(request, Request::InstanceList { session } if session == "fixture-session")
                );
                engine_reply(&mut stream, Reply::InstanceOwners(vec![old]));
                let (mut stream, request) = engine_request(&engine);
                assert!(
                    matches!(request, Request::WaitChanges { after: Some(actual) } if actual == token)
                );
                waiting.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(
                    matches!(engine.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                    "no polling or registry request while unchanged"
                );
                engine_reply(&mut stream, Reply::Changes(replacement_token));
                let (mut stream, request) = engine_request(&engine);
                assert!(
                    matches!(request, Request::InstanceList { session } if session == "fixture-session")
                );
                engine_reply(&mut stream, Reply::InstanceOwners(vec![new]));
            });
            let old_thread = std::thread::spawn(move || {
                log_reply(&mut log_request(&old_app, old_id, None), old_id, "old owner");
                let mut stream = log_request(&old_app, old_id, Some(1));
                if error_reply {
                    let reply = app_control::Reply::Error { error: "Owner retired".into() };
                    ipc::codec::write(&mut stream, &ipc::codec::encode(&reply, 4096).unwrap())
                        .unwrap();
                } // Otherwise dropping the actual socket supplies EOF.
            });
            let (attached, attach) = mpsc::sync_channel(1);
            let new_thread = std::thread::spawn(move || {
                log_reply(&mut log_request(&new_app, new_id, None), new_id, "replacement owner");
                let mut stream = log_request(&new_app, new_id, Some(1));
                attached.send(()).unwrap();
                assert_eq!(stream.read(&mut [0]).unwrap(), 0, "cancel wakes the idle endpoint");
            });
            let (watch, receive) =
                Watch::start(profile.path().into(), "fixture-session".into(), None, None).unwrap();
            let (updates, received) = mpsc::channel();
            let forward = std::thread::spawn(move || {
                while let Ok(update) = receive.recv_blocking() {
                    if matches!(update, Update::Running(_)) {
                        continue;
                    }
                    if updates.send(update).is_err() {
                        break;
                    }
                }
            });
            let mut lines = vec![];
            let mut cursor = None;
            retain(&mut lines, &mut cursor, received.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(
                lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
                ["old owner"]
            );
            retain(&mut lines, &mut cursor, received.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(
                lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
                ["old owner"],
                "an unavailable endpoint must not erase retained messages"
            );
            waited.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                received.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            release.send(()).unwrap();
            retain(&mut lines, &mut cursor, received.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(
                lines.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
                ["old owner", "replacement owner"]
            );
            assert_eq!(cursor.unwrap().instance, new_id);
            attach.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(watch);
            engine_thread.join().unwrap();
            old_thread.join().unwrap();
            new_thread.join().unwrap();
            forward.join().unwrap();
            std::fs::remove_file(old_path).unwrap();
            std::fs::remove_file(new_path).unwrap();
        }
    }
}
