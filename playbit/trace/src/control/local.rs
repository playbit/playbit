//! iOS has one embedded recorder and sandbox paths longer than sockaddr_un.
//! Admit private socket pairs in process, then use the same bounded owner control loop.
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
                "trace control profile path exceeds 4 KiB",
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
        // One queued peer retains one wake byte until its own admission.
        // Draining all bytes here would hide the remaining pending peers now
        // that the owner accepts only once per listener readiness event.
        match self.notification.read(&mut [0; 1]) {
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
            // wake. Retire this control endpoint; the recorder remains separate.
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
