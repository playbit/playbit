//! Completion delivery chosen by the owner. Results remain in existing bounded
//! mailboxes; this channel carries only readiness, never data or a second queue.
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(any(test, feature = "app-host"))]
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
};

#[cfg(any(test, feature = "app-host"))]
const MAX_DRAIN_READS: usize = 16;
#[cfg(any(test, feature = "app-host"))]
const DRAIN_BYTES: usize = 64;

enum Delivery {
    #[cfg(any(test, feature = "app-host"))]
    Unix(UnixStream),
    Host {
        pending: AtomicBool,
        // The callback runs under this gate. Retirement takes it after any
        // in-flight callback returns, then no retained sender can call it again.
        callback: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    },
}

pub(crate) struct Wake {
    delivery: Delivery,
}

impl Wake {
    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn unix(socket: UnixStream) -> Self {
        Self { delivery: Delivery::Unix(socket) }
    }
    pub(crate) fn notify(&self) {
        // Unix failure closes the write half so the owner observes EOF. Hosts
        // must provide a bounded, nonpanicking callback that schedules work.
        let _ = self.try_notify();
    }
    pub(crate) fn try_notify(&self) -> io::Result<()> {
        match &self.delivery {
            #[cfg(any(test, feature = "app-host"))]
            Delivery::Unix(socket) => self.notify_with(|| (&*socket).write(&[1])),
            Delivery::Host { pending, callback } => {
                if !pending.swap(true, Ordering::AcqRel) {
                    let callback =
                        callback.lock().expect("completion callback gate is not poisoned");
                    if let Some(callback) = callback.as_ref() {
                        callback();
                    }
                }
                Ok(())
            }
        }
    }
    #[cfg(any(test, feature = "app-host"))]
    fn notify_with(&self, mut write: impl FnMut() -> io::Result<usize>) -> io::Result<()> {
        let mut failure = io::Error::from(io::ErrorKind::Interrupted);
        for _ in 0..2 {
            match write() {
                Ok(1) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => failure = error,
                Err(error) => {
                    failure = error;
                    break;
                }
                Ok(_) => {
                    failure = io::Error::from(io::ErrorKind::WriteZero);
                    break;
                }
            }
        }
        if let Delivery::Unix(socket) = &self.delivery {
            let _ = socket.shutdown(std::net::Shutdown::Write);
        }
        Err(failure)
    }
    /// A host mailbox consumer rearms delivery before draining its mailbox.
    /// This may run independently of a blocked network owner.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn rearm_host(&self) {
        match &self.delivery {
            Delivery::Host { pending, .. } => pending.store(false, Ordering::Release),
            #[cfg(any(test, feature = "app-host"))]
            Delivery::Unix(_) => panic!("host rearming requires host callback completion"),
        }
    }
    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn socket(&self) -> Option<&UnixStream> {
        match &self.delivery {
            #[cfg(any(test, feature = "app-host"))]
            Delivery::Unix(socket) => Some(socket),
            Delivery::Host { .. } => None,
        }
    }
}

pub(crate) struct Completion {
    #[cfg(any(test, feature = "app-host"))]
    read: Option<UnixStream>,
    pub(crate) wake: Arc<Wake>,
}
impl Completion {
    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn unix() -> io::Result<Self> {
        let (read, write) = UnixStream::pair()?;
        // UIKit does not ignore SIGPIPE. A late worker notification must fail
        // without terminating the app after its reader has closed.
        #[cfg(target_vendor = "apple")]
        rustix::net::sockopt::set_socket_nosigpipe(&write, true)?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok(Self { read: Some(read), wake: Arc::new(Wake::unix(write)) })
    }
    pub(crate) fn host(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            #[cfg(any(test, feature = "app-host"))]
            read: None,
            wake: Arc::new(Wake {
                delivery: Delivery::Host {
                    pending: AtomicBool::new(false),
                    callback: Mutex::new(Some(Box::new(wake))),
                },
            }),
        }
    }
    #[cfg(any(test, feature = "app-host"))]
    pub(crate) fn socket(&self) -> Option<&UnixStream> {
        self.read.as_ref()
    }
    /// Clear readiness before collecting mailboxes. A publication during
    /// collection must deliver another wake. Unix reads are bounded per turn;
    /// leftover bytes keep the descriptor readable for the owner's next turn.
    pub(crate) fn drain(&self) -> io::Result<bool> {
        match &self.wake.delivery {
            Delivery::Host { pending, .. } => Ok(pending.swap(false, Ordering::AcqRel)),
            #[cfg(any(test, feature = "app-host"))]
            Delivery::Unix(_) => {
                let mut read = self.read.as_ref().expect("Unix completion owns the read half");
                let mut notified = false;
                for _ in 0..MAX_DRAIN_READS {
                    match read.read(&mut [0; DRAIN_BYTES]) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "background completion channel closed",
                            ));
                        }
                        Ok(_) => notified = true,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(notified)
            }
        }
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        match &self.wake.delivery {
            #[cfg(any(test, feature = "app-host"))]
            Delivery::Unix(_) => {}
            Delivery::Host { callback, .. } => {
                // Drop captures after releasing the gate. Captures may own arbitrary
                // host resources; none is retained merely because a worker lives on.
                let captured =
                    callback.lock().expect("completion callback gate is not poisoned").take();
                drop(captured);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::sync::{Barrier, atomic::AtomicUsize, mpsc};
    use std::time::Duration;
    const WATCHDOG: Duration = Duration::from_secs(5);

    #[test]
    fn host_coalesces_rearms_and_retires_captures_with_senders_alive() {
        let count = Arc::new(AtomicUsize::new(0));
        let captured = count.clone();
        let completion = Completion::host(move || {
            captured.fetch_add(1, Ordering::Relaxed);
        });
        assert!(completion.socket().is_none());
        assert!(completion.wake.socket().is_none());
        let sender = completion.wake.clone();
        for _ in 0..100 {
            sender.notify();
        }
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(completion.drain().unwrap());
        assert!(!completion.drain().unwrap());
        sender.notify();
        assert_eq!(count.load(Ordering::Relaxed), 2);
        drop(completion);
        assert_eq!(Arc::strong_count(&count), 1, "retained worker cannot retain host captures");
        sender.notify();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn publication_during_collection_schedules_another_turn() {
        let (send, receive) = mpsc::sync_channel(1);
        let completion = Completion::host(move || {
            send.try_send(()).unwrap();
        });
        completion.wake.notify();
        receive.recv_timeout(WATCHDOG).unwrap();
        assert!(completion.drain().unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let sender = completion.wake.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            sender.notify();
        });
        // The owner has cleared readiness and is now collecting mailboxes.
        barrier.wait();
        receive.recv_timeout(WATCHDOG).unwrap();
        worker.join().unwrap();
        assert!(completion.drain().unwrap());
        assert!(!completion.drain().unwrap());
    }

    #[test]
    fn retirement_waits_for_in_flight_callback_and_disables_retained_senders() {
        let count = Arc::new(AtomicUsize::new(0));
        let captured = count.clone();
        let (entered, entry) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let completion = Completion::host(move || {
            captured.fetch_add(1, Ordering::Relaxed);
            entered.send(()).unwrap();
            released.lock().unwrap().recv_timeout(WATCHDOG).unwrap();
        });
        let sender = completion.wake.clone();
        let worker_sender = sender.clone();
        let worker = std::thread::spawn(move || worker_sender.notify());
        entry.recv_timeout(WATCHDOG).unwrap();
        // The callback really holds the retirement gate, not just the pending flag.
        if let Delivery::Host { callback, .. } = &sender.delivery {
            assert!(matches!(callback.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
        }
        let (retired, retirement) = mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            drop(completion);
            retired.send(()).unwrap();
        });
        assert!(retirement.try_recv().is_err());
        release.send(()).unwrap();
        retirement.recv_timeout(WATCHDOG).unwrap();
        owner.join().unwrap();
        worker.join().unwrap();
        assert_eq!(Arc::strong_count(&count), 1);
        sender.notify();
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unix_failure_arms_eof_and_limits_interrupted_retry() {
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::Other, io::ErrorKind::WriteZero] {
            let completion = Completion::unix().unwrap();
            let mut attempts = 0;
            let result = completion.wake.notify_with(|| {
                attempts += 1;
                if kind == io::ErrorKind::WriteZero { Ok(0) } else { Err(io::Error::from(kind)) }
            });
            assert_eq!(result.unwrap_err().kind(), kind);
            assert_eq!(attempts, if kind == io::ErrorKind::Interrupted { 2 } else { 1 });
            assert_eq!(completion.drain().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        }
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    #[allow(
        unsafe_code,
        reason = "Only the isolated child restores UIKit's default SIGPIPE disposition."
    )]
    fn retired_unix_notifications_do_not_signal() {
        // UIKit does not install Rust main's ignored SIGPIPE handler. A worker
        // finishing after its owner closes must report failure, never kill iOS.
        const CHILD: &str = "PLAYBIT_COMPLETION_SIGPIPE_TEST";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "engine::completion::tests::retired_unix_notifications_do_not_signal",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "retired completion sender terminated: {status}");
            return;
        }
        // SAFETY: this child runs only this test; no other test shares its signals.
        assert_ne!(unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) }, libc::SIG_ERR);
        let completion = Completion::unix().unwrap();
        let sender = completion.wake.clone();
        completion.socket().unwrap().shutdown(std::net::Shutdown::Both).unwrap();
        drop(completion);
        assert_eq!(sender.try_notify().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        sender.notify();

        // Closing the store after its consumer also sends a final notification.
        use crate::store::change_feed::{CommittedChangeFeed, FeedClosure};
        let feed = CommittedChangeFeed::new(crate::store::Id::from_bytes([1; 16])).unwrap();
        drop(feed.take_receiver().unwrap());
        assert_eq!(feed.status().closed, Some(FeedClosure::ReceiverDropped));
        drop(feed);
    }

    #[test]
    fn unix_full_channel_is_ready_and_each_drain_is_bounded() {
        let completion = Completion::unix().unwrap();
        let mut socket = completion.wake.socket().unwrap();
        let mut written = 0;
        loop {
            match socket.write(&[1; 2048]) {
                Ok(size) => written += size,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("filling completion channel: {error}"),
            }
        }
        assert!(written > MAX_DRAIN_READS * DRAIN_BYTES);
        completion.wake.try_notify().unwrap();
        assert!(completion.drain().unwrap());
        // A bounded turn leaves readiness for the next turn, even when full.
        assert!(completion.drain().unwrap());
    }
}
