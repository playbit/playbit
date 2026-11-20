//! Event-driven local cursor delivery. Four subscribers share the recorder's
//! admission slots. Socket backpressure pauses only that reader cursor; a
//! pending frame is preserved and retention expiry is explicit. Readers cannot
//! retain a rotated inode.
use crate::{
    format,
    log::{End, Position, ReadResult, Reader},
    notify, process,
};
use std::{
    fs,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
};

#[cfg(not(target_os = "ios"))]
use crate::descriptors;
#[cfg(not(target_os = "ios"))]
use std::os::unix::{fs::PermissionsExt, net::UnixListener};
#[cfg(any(target_os = "ios", test))]
mod local;

pub const REQUEST_BYTES: usize = 40;
pub const MAX_FRAME_BYTES: usize = format::MAX_BLOCK_BYTES + 5;
pub const HISTORY_EVENTS: u64 = 10_000;
const PENDING_BYTES: usize = MAX_FRAME_BYTES;
const CLIENTS: usize = 4;
const MAGIC: &[u8; 8] = b"PBTAIL03";

pub struct Service {
    stop: UnixStream,
    invalidation: UnixStream,
    thread: Option<JoinHandle<io::Result<()>>>,
    path: Option<PathBuf>,
}
struct Incoming {
    stream: UnixStream,
    #[cfg(any(target_os = "ios", test))]
    credit: Option<local::Credit>,
}
enum Admission {
    #[cfg(not(target_os = "ios"))]
    Socket(UnixListener),
    #[cfg(any(target_os = "ios", test))]
    Local(local::Listener),
}
impl Admission {
    fn descriptor(&self) -> i32 {
        match self {
            #[cfg(not(target_os = "ios"))]
            Self::Socket(listener) => listener.as_raw_fd(),
            #[cfg(any(target_os = "ios", test))]
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
                    #[cfg(test)]
                    credit: None,
                })
            }
            #[cfg(any(target_os = "ios", test))]
            Self::Local(listener) => listener.accept(),
        }
    }
}
struct Client {
    stream: UnixStream,
    #[cfg(any(target_os = "ios", test))]
    _credit: Option<local::Credit>,
    request: [u8; REQUEST_BYTES],
    received: usize,
    position: Position,
    pending: Vec<u8>,
    sent: usize,
    selector: Option<[u8; 16]>,
    scopes: Vec<([u64; 2], u32)>,
    since: Option<u64>,
    history: Option<History>,
}
#[derive(Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "Fixed cursor arrays stay inline for the bounded set of tail clients."
)]
enum History {
    Count { start: Position, end: End, group_end: End, events: u64 },
    Replay { end: End, skip: u64 },
}

impl Service {
    #[cfg(target_os = "ios")]
    pub fn start(profile: &Path, reader: Reader, notification: UnixStream) -> io::Result<Self> {
        Self::start_local(profile, reader, notification)
    }
    #[cfg(not(target_os = "ios"))]
    pub fn start(profile: &Path, reader: Reader, notification: UnixStream) -> io::Result<Self> {
        crate::profile_paths::prepare_control(profile)?;
        let path = profile.join(crate::profile_paths::TAIL);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Self::spawn(Admission::Socket(listener), Some(path), reader, notification)
    }
    #[cfg(any(target_os = "ios", test))]
    fn start_local(profile: &Path, reader: Reader, notification: UnixStream) -> io::Result<Self> {
        Self::spawn(Admission::Local(local::Listener::new(profile)?), None, reader, notification)
    }
    fn spawn(
        admission: Admission,
        path: Option<PathBuf>,
        reader: Reader,
        notification: UnixStream,
    ) -> io::Result<Self> {
        let (stop, receiver) = notify::pair()?;
        let (invalidation, invalidation_receiver) = notify::pair()?;
        invalidation.set_nonblocking(false)?;
        invalidation.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        invalidation.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
        let thread =
            thread::Builder::new().name("playbit-trace-tail".into()).stack_size(128 * 1024).spawn(
                move || serve(admission, receiver, invalidation_receiver, reader, notification),
            )?;
        Ok(Self { stop, invalidation, thread: Some(thread), path })
    }

    /// Fence clean against unsent observer copies. Producers never wait on this
    /// exchange; only the explicit owner control does, with a finite deadline.
    pub(crate) fn invalidate(&mut self) -> io::Result<()> {
        let result = (|| {
            self.invalidation.write_all(&[1])?;
            let mut ack = [0];
            self.invalidation.read_exact(&mut ack)?;
            if ack != [1] {
                return Err(io::Error::other("invalid trace cleanup acknowledgement"));
            }
            Ok(())
        })();
        if result.is_err() {
            // A timed-out exchange cannot be reused: its late acknowledgement
            // must never acknowledge a subsequent cleanup.
            let _ = self.invalidation.shutdown(std::net::Shutdown::Both);
        }
        result
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = notify::send(self.stop.as_raw_fd());
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => crate::diagnostic!("Playbit trace tail stopped: {e}"),
                Err(_) => crate::diagnostic!("Playbit trace tail failed"),
            }
        }
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

fn serve(
    mut admission: Admission,
    stop: UnixStream,
    mut invalidation: UnixStream,
    reader: Reader,
    mut notification: UnixStream,
) -> io::Result<()> {
    process::background()?;
    let mut clients: [Option<Client>; CLIENTS] = std::array::from_fn(|_| None);
    // Four single-frame queues + one shared copy and a 128-KiB stack stay
    // below the 1,280-KiB tail allocation, including bounded socket buffers.
    let mut copy = Vec::with_capacity(format::MAX_BLOCK_BYTES);
    let mut recorder_closed = false;
    loop {
        let mut ready_work = false;
        for client in &mut clients {
            if let Some(value) = client {
                match value.advance(&reader, &mut copy) {
                    Ok(ready) => ready_work |= ready,
                    Err(_) => *client = None,
                }
            }
        }
        admit_clients(&mut admission, &mut clients)?;
        let mut fds = [libc::pollfd { fd: -1, events: libc::POLLIN, revents: 0 }; 8];
        fds[0].fd = stop.as_raw_fd();
        fds[1].fd = admission.descriptor();
        fds[7].fd = invalidation.as_raw_fd();
        if !recorder_closed {
            fds[6].fd = notification.as_raw_fd();
        }
        for (index, client) in clients.iter().enumerate() {
            let Some(client) = client else {
                continue;
            };
            fds[2 + index].fd = client.stream.as_raw_fd();
            if client.received == REQUEST_BYTES {
                fds[2 + index].events = 0;
            }
            if client.sent < client.pending.len() {
                fds[2 + index].events |= libc::POLLOUT;
            }
        }
        process::wait(&mut fds, !ready_work)?;
        if fds[0].revents != 0 {
            return Ok(());
        }
        if fds[7].revents != 0 {
            let mut command = [0];
            invalidation.read_exact(&mut command)?;
            if command != [1] {
                return Err(io::ErrorKind::InvalidData.into());
            }
            for client in &mut clients {
                let Some(value) = client else {
                    continue;
                };
                if value.received < REQUEST_BYTES {
                    continue;
                }
                if value.sent != 0 {
                    // A partially delivered frame cannot be replaced with a
                    // gap frame. Disconnect rather than corrupt its framing.
                    *client = None;
                    continue;
                }
                value.pending.clear();
                value.scopes.clear();
                let (start, end) = if value.since.is_none() {
                    reader.recent_bounds(value.selector)?
                } else {
                    reader.bounds()?
                };
                value.position = start;
                value.history = value.since.is_none().then_some(History::Count {
                    start,
                    end,
                    group_end: end,
                    events: 0,
                });
                value.frame(2, &[])?;
            }
            invalidation.write_all(&[1])?;
        }
        for (index, client) in clients.iter_mut().enumerate() {
            let Some(_) = client else {
                continue;
            };
            if fds[2 + index].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                *client = None;
                continue;
            }
        }
        if fds[6].revents & libc::POLLIN != 0 {
            let mut discard = [0; 4096];
            let _ = notification.read(&mut discard);
        }
        if fds[6].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            recorder_closed = true;
        }
        if ready_work {
            thread::yield_now();
        }
    }
}

fn admit_clients(
    admission: &mut Admission,
    clients: &mut [Option<Client>; CLIENTS],
) -> io::Result<()> {
    // Admission is nonblocking and bounded. Local iOS wake bytes are
    // drained before checking its queue, so publish/wakeup races leave
    // either a consumed request or a readable descriptor before parking.
    for _ in 0..CLIENTS {
        let incoming = match admission.accept() {
            Ok(incoming) => incoming,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied
                        | io::ErrorKind::InvalidData
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(slot) = clients.iter_mut().find(|slot| slot.is_none())
            && notify::configure(&incoming.stream, 4096).is_ok()
        {
            incoming.stream.set_nonblocking(true)?;
            *slot = Some(Client {
                stream: incoming.stream,
                #[cfg(any(target_os = "ios", test))]
                _credit: incoming.credit,
                request: [0; REQUEST_BYTES],
                received: 0,
                position: Position::new(None),
                pending: Vec::with_capacity(PENDING_BYTES),
                sent: 0,
                selector: None,
                scopes: Vec::with_capacity(40),
                since: None,
                history: None,
            });
        }
    }
    Ok(())
}

impl Client {
    fn receive_request(&mut self, reader: &Reader) -> io::Result<()> {
        match self.stream.read(&mut self.request[self.received..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => self.received += count,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
        if self.received < REQUEST_BYTES {
            return Ok(());
        }
        if &self.request[..8] != MAGIC
            || self.request[16] > 1
            || self.request[17] > 1
            || self.request[18..24] != [0; 6]
            || (self.request[16] == 0 && self.request[24..] != [0; 16])
            || (self.request[17] == 0 && self.request[8..16] != [0; 8])
        {
            return Err(io::Error::other("unsupported trace tail request"));
        }
        self.selector =
            (self.request[16] == 1).then(|| self.request[24..40].try_into().expect("selector"));
        self.since = (self.request[17] == 1)
            .then(|| u64::from_le_bytes(self.request[8..16].try_into().expect("timestamp")));
        if self.since.is_none() {
            let (start, end) = reader.recent_bounds(self.selector)?;
            self.position = start;
            self.history = Some(History::Count { start, end, group_end: end, events: 0 });
        }
        Ok(())
    }
    fn advance(&mut self, reader: &Reader, copy: &mut Vec<u8>) -> io::Result<bool> {
        if self.received < REQUEST_BYTES {
            self.receive_request(reader)?;
            if self.received < REQUEST_BYTES {
                return Ok(false);
            }
        }
        if !self.flush()? {
            return Ok(false);
        }
        if let Some(History::Count { start, end, group_end, events }) = self.history {
            if self.position.reached(group_end) {
                // Walk backward only until enough selected events are found,
                // then replay that suffix in collection order. New arrivals
                // remain beyond the original attachment snapshot.
                if events < HISTORY_EVENTS
                    && let Some((previous, previous_end)) =
                        reader.previous_bounds(start, self.selector)?
                {
                    self.position = previous;
                    self.history = Some(History::Count {
                        start: previous,
                        end,
                        group_end: previous_end,
                        events,
                    });
                    self.scopes.clear();
                    return Ok(true);
                }
                self.history =
                    Some(History::Replay { end, skip: events.saturating_sub(HISTORY_EVENTS) });
                self.position = start;
                self.scopes.clear();
                return Ok(true);
            }
        } else if let Some(History::Replay { end, .. }) = self.history
            && self.position.reached(end)
        {
            self.history = None;
        }
        let next = match reader.next(&mut self.position, copy) {
            Ok(value) => value,
            Err(error) => {
                self.frame(9, error.to_string().as_bytes())?;
                let _ = self.flush()?;
                return Ok(false);
            }
        };
        match next {
            ReadResult::Block(envelope) => {
                if !self.selected(envelope, copy)? {
                    return Ok(true);
                }
                if !self.include(envelope, copy)? {
                    return Ok(true);
                }
                self.frame(1, copy)?;
            }
            ReadResult::Metadata(envelope) => {
                if !self.selected(envelope, copy)? {
                    return Ok(true);
                }
                if !self.include(envelope, copy)? {
                    return Ok(true);
                }
                self.frame(7, copy)?;
            }
            ReadResult::Skipped => return Ok(true),
            ReadResult::Pending => return Ok(false),
            ReadResult::Expired => {
                self.scopes.clear();
                // Count and replay must describe the same retained prefix.
                // Restart from current bounds if rotation invalidates either
                // pass, before any missing events can consume its allowance.
                if self.history.is_some() {
                    let (start, end) = reader.recent_bounds(self.selector)?;
                    self.position = start;
                    self.history = Some(History::Count { start, end, group_end: end, events: 0 });
                }
                self.frame(2, &[])?;
            }
            ReadResult::Epoch(epoch) => {
                self.scopes.clear();
                let mut bytes = [0; 16];
                bytes[..8].copy_from_slice(&epoch[0].to_le_bytes());
                bytes[8..].copy_from_slice(&epoch[1].to_le_bytes());
                if !matches!(self.history, Some(History::Count { .. })) {
                    self.frame(3, &bytes)?;
                }
            }
            ReadResult::Incomplete => {
                self.frame(4, &[])?;
            }
            ReadResult::StreamGap => self.frame(5, &[])?,
            ReadResult::Stopped => self.frame(6, &[])?,
        }
        if !self.flush()? {
            return Ok(false);
        }
        Ok(true)
    }

    fn include(&mut self, envelope: format::Envelope, bytes: &[u8]) -> io::Result<bool> {
        if let Some(History::Count { events, .. }) = &mut self.history {
            if envelope.kind == format::BlockKind::Events {
                *events = events
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("event count exhausted"))?;
            }
            return Ok(false);
        }
        if let Some(History::Replay { skip, .. }) = &mut self.history
            && *skip != 0
        {
            if envelope.kind == format::BlockKind::Events {
                *skip -= 1;
            }
            return Ok(matches!(
                envelope.kind,
                format::BlockKind::Source
                    | format::BlockKind::AppDefinition
                    | format::BlockKind::AppState
            ));
        }
        let Some(since) = self.since else { return Ok(true) };
        if envelope.kind == format::BlockKind::Events {
            let (_, payload) =
                format::Envelope::decode(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
            let event = format::Events::decode(payload).map_err(|_| io::ErrorKind::InvalidData)?;
            return Ok(event.event.header.timestamp > since);
        }
        Ok(matches!(
            envelope.kind,
            format::BlockKind::Source
                | format::BlockKind::AppDefinition
                | format::BlockKind::AppState
        ) || envelope.time_base > since)
    }

    fn selected(&mut self, envelope: format::Envelope, bytes: &[u8]) -> io::Result<bool> {
        let Some(selector) = self.selector else {
            return Ok(true);
        };
        let (_, payload) =
            format::Envelope::decode(bytes).map_err(|_| io::ErrorKind::InvalidData)?;
        match envelope.kind {
            format::BlockKind::AppDefinition => {
                let app = format::AppDefinition::decode(payload)
                    .map_err(|_| io::ErrorKind::InvalidData)?;
                let key = (envelope.source, app.scope);
                if app.instance == selector {
                    if !self.scopes.contains(&key) {
                        if self.scopes.len() == 40 {
                            return Err(io::Error::other("trace target catalog capacity"));
                        }
                        self.scopes.push(key);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            format::BlockKind::Events => {
                let event =
                    format::Events::decode(payload).map_err(|_| io::ErrorKind::InvalidData)?;
                Ok(self.scopes.contains(&(envelope.source, event.scope)))
            }
            format::BlockKind::AppState => {
                let app =
                    format::AppState::decode(payload).map_err(|_| io::ErrorKind::InvalidData)?;
                let key = (envelope.source, app.scope);
                let selected = self.scopes.contains(&key);
                self.scopes.retain(|v| *v != key);
                Ok(selected)
            }
            format::BlockKind::Recording | format::BlockKind::Loss => {
                let mut input = payload;
                let scope =
                    format::read_integer(&mut input).map_err(|_| io::ErrorKind::InvalidData)?;
                Ok(scope == 0 || self.scopes.contains(&(envelope.source, scope as u32)))
            }
            _ => Ok(true),
        }
    }

    fn frame(&mut self, kind: u8, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() + 5 > PENDING_BYTES - self.pending.len() {
            return Err(io::Error::other("trace pending bound exceeded"));
        }
        self.pending.extend_from_slice(&((bytes.len() + 1) as u32).to_le_bytes());
        self.pending.push(kind);
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<bool> {
        if self.sent == self.pending.len() {
            self.pending.clear();
            self.sent = 0;
            return Ok(true);
        }
        let end = (self.sent + 64 * 1024).min(self.pending.len());
        match self.stream.write(&self.pending[self.sent..end]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => self.sent += count,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) => return Err(e),
        }
        if self.sent == self.pending.len() {
            self.pending.clear();
            self.sent = 0;
        }
        Ok(self.pending.is_empty())
    }
}

fn open(profile: &Path) -> io::Result<UnixStream> {
    #[cfg(target_os = "ios")]
    {
        local::connect(profile)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let stream = descriptors::connect(&profile.join(crate::profile_paths::TAIL))?;
        descriptors::peer(&stream)?;
        notify::configure(&stream, 16 * 1024)?;
        // Attachment never waits for a full listen backlog. Ordinary cursor
        // readers retain blocking reads; polling callers restore nonblocking.
        stream.set_nonblocking(false)?;
        Ok(stream)
    }
}

pub fn connect(
    profile: &Path,
    since: Option<u64>,
    selector: Option<[u8; 16]>,
) -> io::Result<UnixStream> {
    if selector.is_some() {
        let mut control =
            crate::control::connect(profile, crate::control::Command::Observe, selector, false)?;
        control.set_nonblocking(false)?;
        control.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        let mut status = [0; crate::control::STATUS_BYTES];
        control.read_exact(&mut status)?;
        crate::control::Status::decode(&status)?;
    }
    request(open(profile)?, since, selector)
}
fn request(
    mut stream: UnixStream,
    since: Option<u64>,
    selector: Option<[u8; 16]>,
) -> io::Result<UnixStream> {
    let mut request = [0; REQUEST_BYTES];
    request[..8].copy_from_slice(MAGIC);
    if let Some(timestamp) = since {
        request[8..16].copy_from_slice(&timestamp.to_le_bytes());
        request[17] = 1;
    }
    if let Some(id) = selector {
        request[16] = 1;
        request[24..40].copy_from_slice(&id);
    }
    // One bounded admission attempt on the fresh stream. A partial request is
    // abandoned with the socket; it must never become a blocking handshake.
    stream.set_nonblocking(true)?;
    match stream.write(&request)? {
        REQUEST_BYTES => {}
        _ => return Err(io::ErrorKind::WriteZero.into()),
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use playbit_api::abi::trace::{self as wire, TraceEvent};

    fn isolated(test: &str) -> bool {
        if std::env::var("PLAYBIT_TAIL_TEST").as_deref() == Ok(test) {
            return false;
        }
        assert!(
            std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", test, "--nocapture"])
                .env("PLAYBIT_TAIL_TEST", test)
                .status()
                .expect("isolated local endpoint")
                .success()
        );
        true
    }
    fn append_event(log: &mut crate::log::Log, sequence: &mut u64, scope: u32, timestamp: u64) {
        let mut payload = vec![0; 16 + wire::HEADER_BYTES];
        payload[..4].copy_from_slice(&scope.to_le_bytes());
        payload[4] = 1;
        payload[8] = 1;
        wire::encode_header(
            TraceEvent { timestamp, kind: wire::INSTANT, ..Default::default() },
            &mut payload[16..],
        )
        .expect("event header");
        append(log, sequence, format::BlockKind::Events, &payload);
    }
    fn append(
        log: &mut crate::log::Log,
        sequence: &mut u64,
        kind: format::BlockKind,
        payload: &[u8],
    ) {
        *sequence += 1;
        let envelope = format::Envelope {
            kind,
            epoch: log.epoch,
            first: *sequence,
            count: 1,
            source: [1, 2],
            time_base: 1,
        };
        let mut bytes = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        envelope.encode(payload, &mut bytes).expect("fixture block");
        let scope = match kind {
            format::BlockKind::Events => {
                u64::from(u32::from_le_bytes(payload[..4].try_into().expect("scope")))
            }
            format::BlockKind::AppDefinition => {
                format::read_integer(&mut &*payload).expect("scope")
            }
            _ => 0,
        };
        let instance = (scope != 0).then(|| [u8::try_from(scope).expect("fixture instance"); 16]);
        assert_eq!(
            log.append_to(&bytes, 0, instance).expect("retained block"),
            crate::log::Append::Written
        );
    }
    fn read_event(stream: &mut UnixStream) -> Option<(u32, u64)> {
        for _ in 0..128 {
            let mut length = [0; 4];
            stream.read_exact(&mut length).expect("frame header");
            let length = u32::from_le_bytes(length) as usize;
            assert!((1..=MAX_FRAME_BYTES).contains(&length));
            let mut frame = vec![0; length];
            stream.read_exact(&mut frame).expect("frame payload");
            if frame[0] == 6 {
                return None;
            }
            if frame[0] == 3 {
                continue;
            }
            assert!(matches!(frame[0], 1 | 7), "unexpected frame: {frame:?}");
            let (envelope, payload) = format::Envelope::decode(&frame[1..]).expect("block");
            if envelope.kind == format::BlockKind::Events {
                let event = format::Events::decode(payload).expect("event");
                return Some((event.scope, event.event.header.timestamp));
            }
        }
        panic!("event or stop within fixture metadata");
    }
    #[test]
    fn recent_history_counts_selected_events_and_follows_new_events() {
        if isolated("tail::tests::recent_history_counts_selected_events_and_follows_new_events") {
            return;
        }
        // Interleaving another instance must not consume this instance's 10k
        // history allowance. A new event must still arrive after that prefix.
        let profile = std::env::temp_dir().join(format!("pt-recent-{}", std::process::id()));
        fs::create_dir(&profile).expect("profile");
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("private profile");
        let mut log = crate::log::Log::open(&profile.join("trace"), false, 0).expect("log");
        let mut sequence = 0;
        for group in 0..5 {
            if group != 0 {
                log.close();
                log.begin_epoch(0).expect("next capture");
                sequence = 0;
            }
            for scope in [1u8, 2] {
                // scope, identity flags, instance, label, one mark name.
                let mut payload = vec![scope, 0];
                payload.extend_from_slice(&[scope; 16]);
                payload.extend_from_slice(&[1, b'A', 1, 2, 4, b't', b'e', b's', b't']);
                append(&mut log, &mut sequence, format::BlockKind::AppDefinition, &payload);
            }
            for timestamp in group * 2500 + 1..=((group + 1) * 2500).min(HISTORY_EVENTS + 7) {
                append_event(&mut log, &mut sequence, 1, timestamp);
                append_event(&mut log, &mut sequence, 2, timestamp);
            }
        }
        // A newer group without this instance must not hide its history or
        // prevent following a newly admitted stream in that same group.
        assert_eq!(log.rotate(0).expect("unrelated group"), crate::log::Append::Written);
        append_event(&mut log, &mut sequence, 2, HISTORY_EVENTS + 8);
        // Drive the large history scan by bounded turns, independently of
        // background-thread scheduling and the socket's per-read deadline.
        let mut admission = Admission::Local(local::Listener::new(&profile).expect("listener"));
        let mut stream = request(local::connect(&profile).expect("connect"), None, Some([1; 16]))
            .expect("recent request");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("deadline");
        let mut clients = std::array::from_fn(|_| None);
        admit_clients(&mut admission, &mut clients).expect("admit");
        let mut client = clients[0].take().expect("client");
        let reader = log.reader();
        let mut copy = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        // Two interleaved instances, plus definitions and group transitions.
        for _ in 0..2 * (HISTORY_EVENTS + 7) + 64 {
            assert!(client.advance(&reader, &mut copy).expect("count history"));
            if matches!(client.history, Some(History::Replay { .. })) {
                break;
            }
        }
        assert!(matches!(client.history, Some(History::Replay { skip: 7, .. })));
        let mut assert_event = |timestamp| {
            let mut flushed = false;
            for _ in 0..128 {
                assert!(client.advance(&reader, &mut copy).expect("advance event"));
                let (envelope, payload) = format::Envelope::decode(&copy).expect("fixture block");
                if envelope.kind == format::BlockKind::Events {
                    let event = format::Events::decode(payload).expect("fixture event");
                    if event.scope == 1 && event.event.header.timestamp == timestamp {
                        flushed = true;
                        break;
                    }
                }
            }
            assert!(flushed, "expected selected event within fixture metadata");
            assert_eq!(read_event(&mut stream), Some((1, timestamp)));
        };
        for timestamp in 8..=HISTORY_EVENTS + 7 {
            assert_event(timestamp);
        }
        append_event(&mut log, &mut sequence, 1, HISTORY_EVENTS + 8);
        assert_event(HISTORY_EVENTS + 8);
        log.close();
        assert!(client.advance(&reader, &mut copy).expect("stopped capture"));
        assert_eq!(read_event(&mut stream), None);
        drop(client);
        drop(stream);
        drop(admission);
        drop(reader);
        drop(log);
        fs::remove_dir_all(profile).expect("cleanup");
    }
    #[test]
    fn since_filters_each_timestamp_without_assuming_global_order_or_uniqueness() {
        if isolated(
            "tail::tests::since_filters_each_timestamp_without_assuming_global_order_or_uniqueness",
        ) {
            return;
        }
        // Buffered producers can deliver older timestamps after newer ones;
        // every matching event survives, including equal timestamps in others.
        let profile = std::env::temp_dir().join(format!("pt-since-{}", std::process::id()));
        fs::create_dir(&profile).expect("profile");
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("private profile");
        let mut log = crate::log::Log::open(&profile.join("trace"), false, 0).expect("log");
        let mut sequence = 0;
        for timestamp in [50, 100, 75, 100, 51] {
            append_event(&mut log, &mut sequence, 0, timestamp);
        }
        log.close();
        let service = Service::start_local(
            &profile,
            log.reader(),
            log.take_notification().expect("notification"),
        )
        .expect("tail service");
        for (since, expected) in
            [(50, vec![100, 75, 100, 51]), (0, vec![50, 100, 75, 100, 51]), (u64::MAX, vec![])]
        {
            let mut stream = request(local::connect(&profile).expect("connect"), Some(since), None)
                .expect("since request");
            stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).expect("deadline");
            for timestamp in expected {
                assert_eq!(read_event(&mut stream), Some((0, timestamp)));
            }
            assert_eq!(read_event(&mut stream), None);
        }
        drop(service);
        drop(log);
        fs::remove_dir_all(profile).expect("cleanup");
    }
    #[test]
    fn rotation_between_count_and_replay_does_not_skip_surviving_events() {
        if isolated("tail::tests::rotation_between_count_and_replay_does_not_skip_surviving_events")
        {
            return;
        }
        // Invalidate the counted prefix exactly between the two passes. The
        // old skip allowance must not consume events in the replacement prefix.
        let profile = std::env::temp_dir().join(format!("pt-recount-{}", std::process::id()));
        fs::create_dir(&profile).expect("profile");
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("private profile");
        let mut log = crate::log::Log::open(&profile.join("trace"), false, 0).expect("log");
        let mut sequence = 0;
        for timestamp in 1..=HISTORY_EVENTS + 7 {
            append_event(&mut log, &mut sequence, 0, timestamp);
        }
        let mut admission = Admission::Local(local::Listener::new(&profile).expect("listener"));
        let mut stream = request(local::connect(&profile).expect("connect"), None, None)
            .expect("recent request");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).expect("deadline");
        let mut clients = std::array::from_fn(|_| None);
        admit_clients(&mut admission, &mut clients).expect("admit");
        let mut client = clients[0].take().expect("client");
        let reader = log.reader();
        let mut copy = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        for _ in 0..HISTORY_EVENTS + 20 {
            client.advance(&reader, &mut copy).expect("count history");
            if matches!(client.history, Some(History::Replay { .. })) {
                break;
            }
        }
        assert!(matches!(client.history, Some(History::Replay { skip: 7, .. })));
        log.close();
        log.set_turn_budget(512 * 1024);
        log.begin_epoch(0).expect("evict old capture to reserve a new turn");
        sequence = 0;
        for timestamp in 1..=20 {
            append_event(&mut log, &mut sequence, 0, timestamp);
        }
        client.advance(&reader, &mut copy).expect("detect expiry");
        let mut gap = [0; 5];
        stream.read_exact(&mut gap).expect("explicit retention gap");
        assert_eq!(gap, [1, 0, 0, 0, 2]);
        for _ in 0..40 {
            client.advance(&reader, &mut copy).expect("recount surviving history");
            if matches!(client.history, Some(History::Replay { .. })) {
                break;
            }
        }
        client.advance(&reader, &mut copy).expect("first surviving event");
        assert_eq!(read_event(&mut stream), Some((0, 1)));
        drop(client);
        drop(stream);
        drop(admission);
        drop(reader);
        drop(log);
        fs::remove_dir_all(profile).expect("cleanup");
    }
    #[test]
    fn in_process_tail_serves_complete_blocks_at_long_ios_profile_paths() {
        const TEST: &str =
            "tail::tests::in_process_tail_serves_complete_blocks_at_long_ios_profile_paths";
        if std::env::var_os("PLAYBIT_LOCAL_EVENT_TEST").is_none() {
            assert!(
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--exact", TEST, "--nocapture"])
                    .env("PLAYBIT_LOCAL_EVENT_TEST", "1")
                    .status()
                    .expect("isolated embedded endpoint")
                    .success()
            );
            return;
        }
        // The embedded iOS socket-pair endpoint uses exactly the same framing
        // even when its profile exceeds the Unix sockaddr path limit.
        let profile = std::env::temp_dir().join(format!(
            "ptlocal-{}-{}",
            std::process::id(),
            "p".repeat(128)
        ));
        fs::create_dir(&profile).expect("long fixture profile");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("private profile");
        let mut log = crate::log::Log::open(&profile.join("trace"), true, 0).expect("bounded log");
        let service = Service::start_local(
            &profile,
            log.reader(),
            log.take_notification().expect("notification"),
        )
        .expect("embedded tail");
        let mut stream =
            request(local::connect(&profile).expect("private local admission"), None, None)
                .expect("tail request");
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).expect("read deadline");
        let envelope = format::Envelope {
            kind: format::BlockKind::Epoch,
            epoch: log.epoch,
            first: 1,
            count: 1,
            source: [0, 0],
            time_base: 0,
        };
        let mut bytes = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        envelope.encode(&[1], &mut bytes).expect("epoch block");
        assert_eq!(log.append(&bytes, 0).expect("retained block"), crate::log::Append::Written);
        let mut frame = vec![0; bytes.len() + 5];
        stream.read_exact(&mut frame).expect("complete local frame");
        assert_eq!(frame[4], 1);
        assert_eq!(&frame[5..], bytes);
        drop(stream);
        drop(service);
        drop(log);
        fs::remove_dir_all(profile).expect("fixture cleanup");
    }
}
