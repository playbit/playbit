//! Authenticated one-shot local controls. The owner resolves selectors only
//! against live execution catalogs and returns one bounded acknowledged state.
#[cfg(not(target_os = "ios"))]
use crate::descriptors;
use crate::notify;
#[cfg(target_os = "ios")]
mod local;
#[cfg(not(target_os = "ios"))]
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
};
use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
};
pub const REQUEST_BYTES: usize = 32;
pub const STATUS_BYTES: usize = 64;
const CLIENTS: usize = 4;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Observe,
    Start,
    Stop,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Request {
    pub command: Command,
    pub selector: Option<[u8; 16]>,
    pub clean: bool,
}
impl Request {
    fn encode(self) -> [u8; REQUEST_BYTES] {
        let mut b = [0; REQUEST_BYTES];
        b[..8].copy_from_slice(b"PBTRCTL3");
        b[8] = match self.command {
            Command::Observe => 0,
            Command::Start => 1,
            Command::Stop => 2,
        };
        if let Some(id) = self.selector {
            b[9] = 1;
            b[16..].copy_from_slice(&id);
        }
        b[10] = u8::from(self.clean);
        b
    }
    fn decode(b: &[u8; REQUEST_BYTES]) -> io::Result<Self> {
        if &b[..8] != b"PBTRCTL3"
            || b[9] > 1
            || b[10] > 1
            || b[11..16] != [0; 5]
            || (b[9] == 0 && b[16..] != [0; 16])
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let command = match b[8] {
            0 => Command::Observe,
            1 => Command::Start,
            2 => Command::Stop,
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        if command == Command::Observe && b[10] != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Self {
            command,
            selector: (b[9] == 1).then(|| b[16..].try_into().expect("selector")),
            clean: b[10] != 0,
        })
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Status {
    pub request: u64,
    pub revision: u64,
    pub epoch: [u64; 2],
    pub recording: bool,
    pub incomplete: bool,
    pub unavailable: bool,
    pub sources: u8,
    pub pending_sources: u8,
    pub refused_sources: u64,
}
impl Status {
    pub fn decode(b: &[u8; STATUS_BYTES]) -> io::Result<Self> {
        if &b[..8] != b"PBTRSTA2"
            || b[8..11].iter().any(|v| *v > 1)
            || b[11] > crate::queue::MAX_SOURCES as u8
            || b[12] > b[11]
            || b[13..16] != [0; 3]
            || b[56..] != [0; 8]
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let word = |i| u64::from_le_bytes(b[i..i + 8].try_into().expect("status word"));
        let status = Self {
            recording: b[8] != 0,
            incomplete: b[9] != 0,
            unavailable: b[10] != 0,
            sources: b[11],
            pending_sources: b[12],
            revision: word(16),
            epoch: [word(24), word(32)],
            refused_sources: word(40),
            request: word(48),
        };
        if status.request == 0
            || status.revision == 0
            || (status.recording && status.epoch == [0, 0])
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if b[10] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "trace recording or target is unavailable; check the instance, profile and local diagnostics",
            ));
        }
        Ok(status)
    }
    fn encode(self) -> [u8; STATUS_BYTES] {
        let mut b = [0; STATUS_BYTES];
        b[..8].copy_from_slice(b"PBTRSTA2");
        b[8] = u8::from(self.recording);
        b[9] = u8::from(self.incomplete);
        b[10] = u8::from(self.unavailable);
        b[11] = self.sources;
        b[12] = self.pending_sources;
        for (i, v) in [
            (16, self.revision),
            (24, self.epoch[0]),
            (32, self.epoch[1]),
            (40, self.refused_sources),
            (48, self.request),
        ] {
            b[i..i + 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
}
pub fn connect(
    profile: &Path,
    command: Command,
    selector: Option<[u8; 16]>,
    clean: bool,
) -> io::Result<UnixStream> {
    #[cfg(not(target_os = "ios"))]
    let mut stream = {
        let stream = descriptors::connect(&profile.join(crate::profile_paths::CONTROL))?;
        descriptors::peer(&stream)?;
        stream
    };
    #[cfg(target_os = "ios")]
    let mut stream = local::connect(profile)?;
    notify::configure(&stream, 1024)?;
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
    stream.write_all(&Request { command, selector, clean }.encode())?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}
struct Incoming {
    stream: UnixStream,
    #[cfg(target_os = "ios")]
    credit: Option<local::Credit>,
}
enum Listener {
    #[cfg(not(target_os = "ios"))]
    Socket(UnixListener),
    #[cfg(target_os = "ios")]
    Local(local::Listener),
}
impl Listener {
    fn descriptor(&self) -> i32 {
        match self {
            #[cfg(not(target_os = "ios"))]
            Self::Socket(listener) => listener.as_raw_fd(),
            #[cfg(target_os = "ios")]
            Self::Local(listener) => listener.descriptor(),
        }
    }
    fn accept(&mut self) -> io::Result<Incoming> {
        match self {
            #[cfg(not(target_os = "ios"))]
            Self::Socket(listener) => {
                let (stream, _) = listener.accept()?;
                descriptors::peer(&stream).map_err(|_| io::ErrorKind::PermissionDenied)?;
                Ok(Incoming {
                    stream,
                    #[cfg(target_os = "ios")]
                    credit: None,
                })
            }
            #[cfg(target_os = "ios")]
            Self::Local(listener) => listener.accept(),
        }
    }
}

pub(crate) struct Server {
    listener: Listener,
    path: Option<PathBuf>,
    clients: [Option<Client>; CLIENTS],
}
struct Client {
    stream: UnixStream,
    #[cfg(target_os = "ios")]
    _credit: Option<local::Credit>,
    input: [u8; REQUEST_BYTES],
    received: usize,
    output: Option<[u8; STATUS_BYTES]>,
    sent: usize,
}
impl Server {
    pub fn bind(profile: &Path) -> io::Result<Self> {
        #[cfg(not(target_os = "ios"))]
        let (listener, path) = {
            crate::profile_paths::prepare_control(profile)?;
            let path = profile.join(crate::profile_paths::CONTROL);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let listener = UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            (Listener::Socket(listener), Some(path))
        };
        #[cfg(target_os = "ios")]
        let (listener, path) = (Listener::Local(local::Listener::new(profile)?), None);
        Ok(Self { listener, path, clients: std::array::from_fn(|_| None) })
    }
    pub fn descriptors(&self, fds: &mut [libc::pollfd]) {
        assert_eq!(fds.len(), 5);
        fds[0] = libc::pollfd { fd: self.listener.descriptor(), events: libc::POLLIN, revents: 0 };
        for (fd, client) in fds[1..].iter_mut().zip(&self.clients) {
            *fd = libc::pollfd { fd: -1, events: libc::POLLIN, revents: 0 };
            if let Some(client) = client {
                fd.fd = client.stream.as_raw_fd();
                if client.output.is_some() {
                    fd.events = libc::POLLOUT;
                }
            }
        }
    }
    pub fn advance(&mut self) -> io::Result<[Option<Request>; CLIENTS]> {
        match self.listener.accept() {
            Ok(incoming) => {
                if let Some(slot) = self.clients.iter_mut().find(|s| s.is_none()) {
                    notify::configure(&incoming.stream, 1024)?;
                    incoming.stream.set_nonblocking(true)?;
                    *slot = Some(Client {
                        stream: incoming.stream,
                        #[cfg(target_os = "ios")]
                        _credit: incoming.credit,
                        input: [0; REQUEST_BYTES],
                        received: 0,
                        output: None,
                        sent: 0,
                    });
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::PermissionDenied
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
        let mut requests = [None; CLIENTS];
        for (index, slot) in self.clients.iter_mut().enumerate() {
            let Some(client) = slot else {
                continue;
            };
            let result = if let Some(output) = client.output.as_ref() {
                match client.stream.write(&output[client.sent..]) {
                    Ok(0) => Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        client.sent += n;
                        if client.sent == STATUS_BYTES {
                            *slot = None;
                        }
                        Ok(())
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
                    Err(e) => Err(e),
                }
            } else if client.received < REQUEST_BYTES {
                match client.stream.read(&mut client.input[client.received..]) {
                    Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => {
                        client.received += n;
                        if client.received == REQUEST_BYTES {
                            Request::decode(&client.input)
                                .map(|request| requests[index] = Some(request))
                        } else {
                            Ok(())
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
                    Err(e) => Err(e),
                }
            } else {
                Ok(())
            };
            if result.is_err() {
                *slot = None;
            }
        }
        Ok(requests)
    }
    pub fn reply(&mut self, index: usize, status: Status) {
        if let Some(client) = &mut self.clients[index] {
            client.output = Some(status.encode());
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_requires_an_explicit_mutating_control() {
        for command in [Command::Start, Command::Stop] {
            for selector in [None, Some([7; 16])] {
                let request = Request { command, selector, clean: true };
                let decoded = Request::decode(&request.encode()).expect("cleanup request");
                assert_eq!(decoded.command, command);
                assert_eq!(decoded.selector, selector);
                assert!(decoded.clean);
            }
        }
        let request = Request { command: Command::Observe, selector: None, clean: true };
        assert_eq!(
            Request::decode(&request.encode()).expect_err("read-only cleanup").kind(),
            io::ErrorKind::InvalidData
        );
        let mut invalid =
            Request { command: Command::Start, selector: None, clean: false }.encode();
        invalid[10] = 2;
        assert_eq!(
            Request::decode(&invalid).expect_err("invalid cleanup flag").kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn status_frames_validate_counts_receipts_and_capture_identity() {
        // A valid maximum occupancy status round-trips; malformed fields fail
        // before an unavailable target can be interpreted as a normal error.
        let status = Status {
            request: 1,
            revision: 1,
            epoch: [1, 2],
            recording: true,
            sources: crate::queue::MAX_SOURCES as u8,
            pending_sources: crate::queue::MAX_SOURCES as u8,
            ..Default::default()
        };
        assert_eq!(Status::decode(&status.encode()).expect("valid status"), status);
        for invalid in [
            Status { sources: 9, ..status },
            Status { pending_sources: 9, ..status },
            Status { request: 0, ..status },
            Status { revision: 0, ..status },
            Status { epoch: [0, 0], ..status },
        ] {
            assert_eq!(
                Status::decode(&invalid.encode()).expect_err("invalid status field").kind(),
                io::ErrorKind::InvalidData
            );
        }
        let unavailable =
            Status { request: 1, revision: 1, unavailable: true, ..Default::default() };
        assert_eq!(
            Status::decode(&unavailable.encode()).expect_err("unavailable target").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            Status::decode(&Status { revision: 0, ..unavailable }.encode())
                .expect_err("malformed unavailable receipt")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            Status::decode(&Status { recording: false, epoch: [0, 0], ..status }.encode()).is_ok()
        );
    }
}
