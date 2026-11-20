//! One cancelable, event-driven local observer per GUI worker.
use crate::desktop::worker_queue::Message;
use crate::engine::{ChangeToken, ipc::ChangeWait};
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};

struct State {
    stopped: bool,
    socket: Option<UnixStream>,
}
pub struct Watch {
    rearm: Option<mpsc::SyncSender<Option<ChangeToken>>>,
    state: Arc<Mutex<State>>,
    events: async_channel::Sender<Message>,
    thread: Option<JoinHandle<()>>,
}
impl Watch {
    pub fn start(
        profile: PathBuf,
        events: async_channel::Sender<Message>,
        gate: Arc<super::worker_queue::Gate>,
        child: Option<std::process::Child>,
    ) -> std::io::Result<Self> {
        let (send, receive) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State { stopped: false, socket: None }));
        let worker_state = state.clone();
        let notify = events.clone();
        let thread = std::thread::Builder::new()
            .name("playbit-local-watch".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                observe(profile, receive, worker_state, notify, gate, child);
            })?;
        let watch = Self { rearm: Some(send), state, events, thread: Some(thread) };
        watch.rearm(None).expect("new observer has one free command slot");
        Ok(watch)
    }
    pub fn rearm(&self, after: Option<ChangeToken>) -> Result<(), &'static str> {
        self.rearm
            .as_ref()
            .ok_or("local observer stopped")?
            .try_send(after)
            .map_err(|_| "local observer already has pending work")
    }
}
fn observe(
    profile: PathBuf,
    receive: mpsc::Receiver<Option<ChangeToken>>,
    state: Arc<Mutex<State>>,
    notify: async_channel::Sender<Message>,
    gate: Arc<super::worker_queue::Gate>,
    mut child: Option<std::process::Child>,
) {
    let mut backoff = crate::engine::restart::Backoff::default();
    while let Ok(after) = receive.recv() {
        let mut cursor = after;
        let mut reconnect = false;
        loop {
            let result = wait(&profile, &state, cursor, reconnect, &mut child);
            {
                let mut state = state.lock().expect("mutex is not poisoned");
                state.socket = None;
                if state.stopped {
                    return;
                }
            }
            match result {
                Ok(reply) => {
                    backoff.recovered(std::time::Instant::now());
                    if notify.send_blocking(Message::Changed(Ok(reply))).is_err() {
                        return;
                    }
                    break;
                }
                Err(error) => {
                    gate.pause();
                    crate::platform::failure::log(
                        &profile,
                        "local change observer / automatic engine restart",
                        &format!("{error:?}"),
                    );
                    let error = error.to_string();
                    if crate::engine::restart::terminal(&error) {
                        let _ = notify.send_blocking(Message::Fatal(error.into()));
                        return;
                    }
                    if !reconnect && notify.send_blocking(Message::Changed(Err(error))).is_err() {
                        return;
                    }
                    reconnect = true;
                    cursor = None;
                    let delay = backoff.failed(std::time::Instant::now());
                    if receive.recv_timeout(delay) == Err(mpsc::RecvTimeoutError::Disconnected) {
                        return;
                    }
                }
            }
        }
    }
}
fn wait(
    profile: &std::path::Path,
    state: &Mutex<State>,
    after: Option<ChangeToken>,
    reconnect: bool,
    child: &mut Option<std::process::Child>,
) -> crate::engine::Result<crate::engine::ipc::ChangeReply> {
    if reconnect {
        crate::engine::restart::reap(child, profile);
        // A live owned helper may still be shutting down. Do not spawn a second
        // child until it exits; a compatible replacement can already answer below.
        if child.is_none() {
            super::startup::start_local_engine(profile, true, child)?;
        }
    }
    let wait = ChangeWait::connect(profile, after)?;
    {
        let mut state = state.lock().expect("mutex is not poisoned");
        if state.stopped {
            return Err("local observer stopped".into());
        }
        state.socket = Some(wait.cancellation_handle()?);
    }
    wait.receive()
}
impl Drop for Watch {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().expect("mutex is not poisoned");
            state.stopped = true;
            if let Some(socket) = state.socket.take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        self.rearm.take();
        self.events.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::engine::{Request, ipc};
    use std::{
        io::Read,
        os::unix::net::UnixListener,
        time::{Duration, Instant, SystemTime},
    };

    #[test]
    fn watcher_rearms_once_and_shutdown_cancels_a_silent_engine() {
        let directory = PathBuf::from(format!(
            "/tmp/pb-watch-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        crate::paths::private_directory(&directory, "control").unwrap();
        let listener = UnixListener::bind(directory.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let token = ChangeToken { generation: crate::store::Id::from_bytes([1; 16]), sequence: 10 };
        let (armed, registered) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let request: Request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut first).unwrap(),
            )
            .unwrap();
            assert!(matches!(request, Request::WaitChanges { after: None }));
            ipc::write(
                &mut first,
                &ipc::codec::encode(&crate::engine::reply::Reply::Changes(token), 248).unwrap(),
            )
            .unwrap();
            let (mut second, _) = listener.accept().unwrap();
            let request: Request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::read(&mut second).unwrap(),
            )
            .unwrap();
            assert!(
                matches!(request, Request::WaitChanges { after: Some(after) } if after == token)
            );
            second.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            armed.send(()).unwrap();
            assert_eq!(second.read(&mut [0]).unwrap(), 0);
        });
        let (events, receive) = async_channel::bounded(1);
        let watch =
            Watch::start(directory.clone(), events, Arc::new(Default::default()), None).unwrap();
        let Message::Changed(Ok(after)) = receive.recv_blocking().unwrap() else {
            panic!("missing initial cursor");
        };
        assert_eq!(after.token, token);
        watch.rearm(Some(after.token)).unwrap();
        registered.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        drop(watch);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(receive.is_closed());
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
