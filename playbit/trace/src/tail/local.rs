//! iOS has one embedded recorder and sandbox paths longer than sockaddr_un.
//! Admit private socket pairs in process, then use the same bounded tail server.
use super::{CLIENTS, Incoming, notify};
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::AtomicBool,
        mpsc::{self, Receiver, SyncSender},
    },
};

const _: () = assert!(CLIENTS == crate::tail_admission::CAPACITY);

static ENDPOINT: Mutex<Weak<Endpoint>> = Mutex::new(Weak::new());

struct Endpoint {
    profile: PathBuf,
    pending: SyncSender<Incoming>,
    signal: UnixStream,
    slots: Arc<[AtomicBool; CLIENTS]>,
}

pub(super) struct Credit {
    slots: Arc<[AtomicBool; CLIENTS]>,
    index: usize,
}
impl Drop for Credit {
    fn drop(&mut self) {
        crate::tail_admission::release(&self.slots, self.index);
    }
}

pub(super) struct Listener {
    _endpoint: Arc<Endpoint>,
    pending: Receiver<Incoming>,
    notification: UnixStream,
}
impl Listener {
    pub(super) fn new(profile: &Path) -> io::Result<Self> {
        if profile.as_os_str().len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trace profile path exceeds 4 KiB",
            ));
        }
        let mut published = ENDPOINT.try_lock().map_err(|_| io::ErrorKind::WouldBlock)?;
        if published.upgrade().is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let (signal, notification) = notify::pair()?;
        let (sender, pending) = mpsc::sync_channel(CLIENTS);
        let endpoint = Arc::new(Endpoint {
            profile: profile.to_owned(),
            pending: sender,
            signal,
            slots: Arc::new(std::array::from_fn(|_| AtomicBool::new(false))),
        });
        *published = Arc::downgrade(&endpoint);
        Ok(Self { _endpoint: endpoint, pending, notification })
    }

    pub(super) fn descriptor(&self) -> i32 {
        self.notification.as_raw_fd()
    }

    pub(super) fn accept(&mut self) -> io::Result<Incoming> {
        // Drain before checking pending. Publication after that check leaves
        // its wake readable; publication before it is consumed without waiting.
        match self.notification.read(&mut [0; CLIENTS]) {
            Ok(0) => return Err(io::ErrorKind::BrokenPipe.into()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        self.pending.try_recv().map_err(|error| match error {
            mpsc::TryRecvError::Empty => io::ErrorKind::WouldBlock.into(),
            mpsc::TryRecvError::Disconnected => io::ErrorKind::BrokenPipe.into(),
        })
    }
}

impl Endpoint {
    fn connect(&self) -> io::Result<UnixStream> {
        // Reserve before allocating descriptors. Pending plus active readers
        // share exactly four credits, returned only by actual server retirement.
        let index = crate::tail_admission::claim(&self.slots).ok_or(io::ErrorKind::WouldBlock)?;
        let credit = Credit { slots: self.slots.clone(), index };
        let (client, server) = UnixStream::pair()?;
        for stream in [&client, &server] {
            notify::configure(stream, 4096)?;
        }
        self.pending.try_send(Incoming { stream: server, credit: Some(credit) }).map_err(
            |error| match error {
                mpsc::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                mpsc::TrySendError::Disconnected(_) => io::ErrorKind::BrokenPipe,
            },
        )?;
        if !notify::send(self.signal.as_raw_fd()) {
            // A refused connection must not leave a queued descriptor with no
            // wake. Retire this observer endpoint; the recorder remains separate.
            let _ = self.signal.shutdown(std::net::Shutdown::Both);
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(client)
    }
}

pub(super) fn connect(profile: &Path) -> io::Result<UnixStream> {
    let endpoint = ENDPOINT
        .try_lock()
        .map_err(|_| io::ErrorKind::WouldBlock)?
        .upgrade()
        .ok_or(io::ErrorKind::NotConnected)?;
    if endpoint.profile != profile {
        return Err(io::ErrorKind::NotFound.into());
    }
    endpoint.connect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_and_active_connections_share_four_credits_and_retire_with_listener() {
        let bookkeeping = std::mem::size_of::<Endpoint>()
            + std::mem::size_of::<Listener>()
            + CLIENTS * (std::mem::size_of::<Incoming>() + std::mem::size_of::<Credit>())
            + 4096
            + 4096; // Maximum path + bounded-channel allocation/slack.
        assert!(bookkeeping < 16 * 1024);
        crate::diagnostic!(
            "local tail admission bookkeeping allowance: {bookkeeping} bytes; socket buffers remain in the 1280-KiB observer budget"
        );
        assert!(Listener::new(Path::new(&"p".repeat(4097))).is_err());
        let profile = Path::new("/local-tail-admission-test");
        let mut listener = Listener::new(profile).expect("local listener");
        assert!(connect(Path::new("/another-profile")).is_err());
        let stale = listener._endpoint.clone();
        let clients =
            (0..CLIENTS).map(|_| connect(profile).expect("bounded connection")).collect::<Vec<_>>();
        assert_eq!(
            connect(profile).expect_err("four pending slots").kind(),
            io::ErrorKind::WouldBlock
        );
        let mut active =
            (0..CLIENTS).map(|_| listener.accept().expect("ready admission")).collect::<Vec<_>>();
        assert_eq!(
            connect(profile).expect_err("four active slots").kind(),
            io::ErrorKind::WouldBlock
        );
        drop(active.pop());
        let replacement = connect(profile).expect("retired slot reusable");
        let replacement_server = listener.accept().expect("replacement ready");
        drop(listener);
        drop(active);
        drop(replacement_server);
        assert_eq!(
            stale.connect().expect_err("old listener cannot accept").kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(stale);
        assert_eq!(connect(profile).expect_err("no recorder").kind(), io::ErrorKind::NotConnected);
        drop(clients);
        drop(replacement);

        // A paused connector can publish before sending its wake. The server
        // checks pending before parking; its eventual late wake is harmless.
        let mut listener = Listener::new(profile).expect("new endpoint");
        let endpoint = listener._endpoint.clone();
        let index = crate::tail_admission::claim(&endpoint.slots).expect("one credit");
        let (client, server) = UnixStream::pair().expect("observer pair");
        endpoint
            .pending
            .try_send(Incoming {
                stream: server,
                credit: Some(Credit { slots: endpoint.slots.clone(), index }),
            })
            .unwrap_or_else(|_| panic!("empty bounded pending queue"));
        drop(listener.accept().expect("published request visible before wake"));
        assert!(notify::send(endpoint.signal.as_raw_fd()));
        assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
        drop(client);
        endpoint.signal.shutdown(std::net::Shutdown::Both).expect("injected terminal wake failure");
        assert_eq!(
            endpoint.connect().expect_err("failed wake cannot admit success").kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::BrokenPipe));
        drop(listener); // Drops the undeliverable queued stream and its credit.
        assert!(endpoint.slots.iter().all(|s| !s.load(std::sync::atomic::Ordering::Acquire)));
    }
}
