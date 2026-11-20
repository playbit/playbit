//! One bounded guest-log consumer per attached CLI; source builds remain responsive.
use playbit::{
    engine::{Result, session_instance::LaunchOwner},
    ui::app_control::{Exchange, Operation, Reply},
};
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

struct Cancellation {
    stopped: AtomicBool,
    socket: Mutex<Option<UnixStream>>,
    exit_wait: UnixStream,
}
pub(super) struct Follow {
    cancellation: Arc<Cancellation>,
    worker: Option<JoinHandle<()>>,
}
impl Follow {
    pub fn start(profile: &Path, owner: &LaunchOwner) -> Result<Self> {
        let wait = Exchange::connect(profile, owner, Operation::Logs { after: None })?;
        let exit_watch = wait.exit_watch()?;
        let (cancel_exit_wait, exit_wait) = UnixStream::pair()?;
        let cancellation = Arc::new(Cancellation {
            stopped: AtomicBool::new(false),
            socket: Mutex::new(Some(wait.cancellation_handle()?)),
            exit_wait: cancel_exit_wait,
        });
        let control = cancellation.clone();
        let profile = profile.to_owned();
        let mut owner = owner.clone();
        let worker = std::thread::Builder::new()
            .name("playbit-app-logs".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let mut wait = wait;
                    loop {
                        let Reply::Logs(batch) = wait.receive()? else {
                            return Err("Unexpected app log reply".into());
                        };
                        if control.stopped.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        if batch.reset {
                            playbit::diagnostic!("App restarted; log stream reset.");
                        }
                        if batch.dropped != 0 {
                            playbit::diagnostic!(
                                "App log buffer overwrote {} lines.",
                                batch.dropped
                            );
                        }
                        for line in &batch.lines {
                            playbit::trace::diagnostic::stderr_record(&line.message);
                        }
                        if control.stopped.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        owner.instance.owner_generation = batch.cursor.generation;
                        wait = Exchange::connect(
                            &profile,
                            &owner,
                            Operation::Logs { after: Some(batch.cursor) },
                        )?;
                        let mut socket = control.socket.lock().expect("mutex is not poisoned");
                        if control.stopped.load(Ordering::Acquire) {
                            wait.cancellation_handle()?.shutdown(Shutdown::Both)?;
                            return Ok(());
                        }
                        *socket = Some(wait.cancellation_handle()?);
                    }
                })();
                if !control.stopped.load(Ordering::Acquire)
                    && let Err(error) = result
                {
                    let status = if error.downcast_ref::<std::io::Error>().is_some() {
                        exit_watch.wait(&exit_wait)
                    } else {
                        Ok(None)
                    };
                    if control.stopped.load(Ordering::Acquire) {
                        return;
                    }
                    match status {
                        Ok(Some(status)) => match status.code() {
                            Some(code) => playbit::diagnostic!("App exited with status {code}"),
                            None => playbit::diagnostic!("App exited: {status}"),
                        },
                        Ok(None) => playbit::diagnostic!("App log stream stopped: {error}"),
                        Err(status_error) => playbit::diagnostic!("App log stream stopped: {error}; could not read app exit status: {status_error}"),
                    }
                }
            })?;
        Ok(Self { cancellation, worker: Some(worker) })
    }

    #[cfg(feature = "desktop")]
    pub fn wait(mut self) {
        self.worker
            .take()
            .expect("log follower owns its worker")
            .join()
            .expect("log worker must not panic");
    }
}
impl Drop for Follow {
    fn drop(&mut self) {
        self.cancellation.stopped.store(true, Ordering::Release);
        let _ = self.cancellation.exit_wait.shutdown(Shutdown::Both);
        if let Some(socket) = self.cancellation.socket.lock().expect("mutex is not poisoned").take()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use playbit::{
        engine::{Request as EngineRequest, ipc, reply::Reply as EngineReply},
        store::Id,
        ui::app_control::{self, LogBatch, LogCursor, Request},
    };
    use std::{
        io::Read,
        os::unix::net::UnixListener,
        time::{Duration, Instant},
    };
    #[test]
    fn dropping_silent_log_consumer_closes_its_wait_without_a_timer() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = playbit::engine::Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Log fixture", false).unwrap();
        let package = playbit::package::Package::new(
            "test".into(),
            "Log fixture".into(),
            wat::parse_str("(module (memory (export \"memory\") 1) (func (export \"pb_main\")))")
                .unwrap(),
            Default::default(),
        )
        .unwrap();
        let session = engine.create_session(package, root.id).unwrap();
        drop(engine);
        let engine_listener =
            UnixListener::bind(directory.path().join("control/engine.sock")).unwrap();
        let profile = directory.path().to_owned();
        let (stop, stopping) = std::sync::mpsc::channel();
        let (ready, prepared) = std::sync::mpsc::sync_channel(1);
        let engine_thread = std::thread::spawn(move || {
            let mut engine = playbit::engine::Engine::open(&profile).unwrap();
            let mut queue =
                playbit::engine::admission::AdmissionQueue::new(engine_listener, &engine).unwrap();
            ready.send(()).unwrap();
            while matches!(stopping.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                queue.run_once(&mut engine).unwrap();
            }
        });
        prepared.recv_timeout(Duration::from_secs(5)).unwrap();
        let call =
            |request| ipc::call_archive(directory.path(), &request).unwrap().into_owned().unwrap();
        let EngineReply::InstanceStatus { incarnation, .. } =
            call(EngineRequest::InstanceInspect { request: None })
        else {
            panic!()
        };
        let EngineReply::Instance(instance) = call(EngineRequest::InstanceReserve {
            restore_host_mounts: false,
            request: Id::generate().unwrap(),
            incarnation,
            session: session.id.clone(),
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
        }) else {
            panic!()
        };
        assert!(matches!(
            call(EngineRequest::InstanceClaim { instance: instance.id }),
            EngineReply::InstanceAdmission(_)
        ));
        let EngineReply::InstanceOwners(mut owners) =
            call(EngineRequest::InstanceList { session: session.id })
        else {
            panic!()
        };
        let owner = owners.pop().unwrap();
        let cursor = LogCursor {
            instance: owner.instance.id,
            generation: owner.instance.owner_generation,
            next: 0,
        };
        let path = app_control::socket_path(directory.path(), owner.instance.id).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let (waiting, observed) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::codec::read(&mut first, app_control::REQUEST_LIMIT).unwrap(),
            )
            .unwrap();
            assert!(matches!(request.operation, Operation::Logs { after: None }));
            let batch = Reply::Logs(LogBatch { cursor, lines: vec![], dropped: 0, reset: false });
            ipc::codec::write(&mut first, &ipc::codec::encode(&batch, 512).unwrap()).unwrap();
            drop(first);
            let (mut next, _) = listener.accept().unwrap();
            next.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(
                &ipc::codec::read(&mut next, app_control::REQUEST_LIMIT).unwrap(),
            )
            .unwrap();
            assert!(
                matches!(request.operation, Operation::Logs { after: Some(after) } if after == cursor)
            );
            waiting.send(()).unwrap();
            assert_eq!(next.read(&mut [0]).unwrap(), 0);
        });
        let consumer = Follow::start(directory.path(), &owner).unwrap();
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        drop(consumer);
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
        stop.send(()).unwrap();
        drop(UnixStream::connect(directory.path().join("control/engine.sock")).unwrap());
        engine_thread.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
