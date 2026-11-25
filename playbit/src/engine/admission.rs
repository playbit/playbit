//! Readiness-driven local admission on the Engine's existing owning thread.
//! Management and data work retain bounded admission through actual completion.
use crate::engine::{ArchivedRequest, Engine, Request, Result, ipc, reply::Reply};
use ipc::{codec, request_memory};
use polling::{Event, Events, Poller};
#[cfg(test)]
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    time::{Duration, Instant},
};

mod bundle;
mod gauges;

const MAX_PENDING: usize = 32;
const MAX_CHANGE_WAITERS: usize = 4;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
// Keep serializer scratch inside the aggregate 16-MiB IPC budget, including
// while a reply archive and its source request are both alive.
const MAX_BUFFERED: usize = 16 * 1024 * 1024 - codec::MAX_SCRATCH;
const MAX_RECEIVED: usize = MAX_BUFFERED - MAX_MESSAGE;
const IO_QUANTUM: usize = 64 * 1024;
const TURN_BYTES: usize = 256 * 1024;
const QUEUE_TIMEOUT: Duration = Duration::from_secs(240);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const LISTENER: usize = 0;
const COMPLETION: usize = 1;

// Queued requests retain a validated archive and bounded routing metadata.
// Reserve owned request storage before eventual materialization.
struct Route {
    op: Op,
}
impl Route {
    fn ready(&self, engine: &Engine) -> bool {
        route_ready(self.op, engine)
    }
}

impl Op {
    fn local_preparation(self) -> bool {
        matches!(
            self,
            Self::SignPackage
                | Self::AuthenticatePackage
                | Self::PrepareApp
                | Self::InstanceCompile
                | Self::CreateSession
                | Self::OpenAppVolume
                | Self::OpenAutomaticVolume
                | Self::DuplicateVolume
                | Self::DuplicateEntry
                | Self::AppRemovalImpact
                | Self::RemoveApp
                | Self::PayloadSize
        )
    }
}

// Owned and archived requests have the same variants but different field types.
// Define their exhaustive opcode/readiness classification once for both transports.
#[derive(Clone, Copy)]
enum Readiness {
    Concurrent,
    Network,
    IdleOnly,
}
macro_rules! request_routes {
    ($($variant:ident $({ $($fields:tt)* })? => $session:expr, $readiness:ident),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Op { $($variant),* }
        impl Op {
            fn readiness(self) -> Readiness {
                match self { $(Self::$variant => Readiness::$readiness),* }
            }
        }

        fn classify_request(request: &Request) -> (Op, Option<&str>) {
            use Request as R;
            match request {
                $(R::$variant $({ $($fields)* })? => (Op::$variant, $session)),*
            }
        }
        fn classify_archive(request: &ArchivedRequest) -> (Op, Option<&str>) {
            use ArchivedRequest as R;
            match request {
                $(R::$variant $({ $($fields)* })? => (Op::$variant, $session)),*
            }
        }
    };
}
request_routes! {
    ToolContextOpen { .. } => None, Concurrent,
    ToolContextClose { .. } => None, Concurrent,
    Syscall { .. } => None, Concurrent,
    FileCall { .. } => None, Concurrent,
    InstanceChannelsAck { .. } => None, Concurrent,
    InstanceChannelsClose { .. } => None, Concurrent,
    InstanceEvents { .. } => None, Concurrent,
    InstanceInspect { .. } => None, Concurrent,
    InstanceStopReason { .. } => None, Concurrent,
    InstanceList { .. } => None, Concurrent,
    InstanceConfigurations { .. } => None, Concurrent,
    InstanceConfiguration { .. } => None, Concurrent,
    InstanceRestore { .. } => None, Concurrent,
    InstanceMounts { .. } => None, Concurrent,
    InstanceReserve { .. } => None, Concurrent,
    InstanceStartup { .. } => None, Concurrent,
    InstanceShow { .. } => None, Concurrent,
    InstanceCompile { .. } => None, Network,
    InstanceCompiled { .. } => None, Concurrent,
    InstanceClaim { .. } => None, Concurrent,
    InstanceReconstruct { .. } => None, Concurrent,
    InstanceLaunchFailed { .. } => None, Concurrent,
    LocalProtocol => None, Concurrent,
    Status => None, Concurrent,
    DesktopLaunch => None, Concurrent,
    UiZoom { .. } => None, Concurrent,
    BrowserDrafts { .. } => None, Concurrent,
    DecideVolumeInvitation { .. } => None, Concurrent,
    DeleteVolume { .. } => None, Concurrent,
    DuplicateVolume { .. } => None, Network,
    DuplicateEntry { .. } => None, Network,
    Entries { .. } => None, Concurrent,
    DirectoryEntries { .. } => None, Concurrent,
    VolumeDetails { .. } => None, Concurrent,
    EntryDetails { .. } => None, Concurrent,
    EntryRevisions { .. } => None, Concurrent,
    RecoverEntry { .. } => None, Network,
    RejectedEntries { .. } => None, Network,
    Schema { .. } => None, Concurrent,
    CreateObject { .. } => None, Concurrent,
    Objects { .. } => None, Concurrent,
    ReadObject { .. } => None, Concurrent,
    EditObject { .. } => None, Concurrent,
    RemoveObject { .. } => None, Concurrent,
    CreateDatabase { .. } => None, Concurrent,
    CreateDatabaseIn { .. } => None, Concurrent,
    RenameEntry { .. } => None, Concurrent,
    RemoveEntry { .. } => None, Concurrent,
    EntryPermissions { .. } => None, Concurrent,
    BloblogCreate { .. } => None, Concurrent,
    BloblogCreateIn { .. } => None, Concurrent,
    OrderSessions { .. } => None, Concurrent,
    ForgetSession { .. } => None, Concurrent,
    WaitChanges { .. } => None, Concurrent,
    FileCreate { .. } => None, Network,
    FileCreateIn { .. } => None, Network,
    FileWrite { .. } => None, Network,
    FileRead { .. } => None, Network,
    CacheBudget { .. } => None, Concurrent,
    SetDisplayName { .. } => None, Concurrent,
    AcceptAgreement { .. } => None, Concurrent,
    Usage { .. } => None, Network,
    PayloadSize { .. } => None, Network,
    PrepareSignIn => None, Network,
    SavedAccount => None, Network,
    RetryAccountKeyBackup => None, Network,
    RequestCode { .. } => None, Network,
    SignIn { .. } => None, Network,
    SignInWithKey { .. } => None, Concurrent,
    RecoveryStatus => None, Network,
    PrepareRecovery => None, Network,
    SaveRecovery => None, Network,
    RecoverCredentials { .. } => None, Network,
    Sync => None, Network,
    StartSync => None, Concurrent,
    StartJoin { .. } => None, IdleOnly,
    PrepareSession { session } => Some(session.as_str()), Concurrent,
    RetrySessionInfo { session } => Some(session.as_str()), Concurrent,
    TransferStatus => None, Concurrent,
    CancelTransfer => None, Concurrent,
    Disconnect => None, Concurrent,
    WorkOffline { .. } => None, Concurrent,
    Join { .. } => None, IdleOnly,
    PreviewInvitation { .. } => None, Concurrent,
    PreviewDesktopInvitation { .. } => None, Concurrent,
    RetainDesktopInvitation { .. } => None, Concurrent,
    WatchInvitation { .. } => None, Concurrent,
    ResumeInvitation { .. } => None, Concurrent,
    PauseInvitation { .. } => None, Concurrent,
    Provision { .. } => None, Concurrent,
    Permissions { .. } => None, Concurrent,
    Revoke { .. } => None, Concurrent,
    GrantVolume { .. } => None, Network,
    LabelVolume { .. } => None, Concurrent,
    CreateVolume { .. } => None, Concurrent,
    InitializeRoot { .. } => None, Concurrent,
    InitializeApp { .. } => None, Concurrent,
    AppVolumes { .. } => None, Concurrent,
    AppRemovalImpact { .. } => None, Concurrent,
    RemoveApp { .. } => None, Concurrent,
    AutomaticVolumes { .. } => None, Concurrent,
    OpenAppVolume { .. } => None, Concurrent,
    OpenAutomaticVolume { .. } => None, Concurrent,
    SessionReference { .. } => None, Concurrent,
    SignPackage { .. } => None, Network,
    AuthenticatePackage { .. } => None, Network,
    ListApps { .. } => None, Network,
    GetAppDisplay { .. } => None, Network,
    SetAppEnabled { .. } => None, Network,
    DeleteApp { .. } => None, Network,
    PublishApp { .. } => None, Network,
    UpdateSession { .. } => None, Network,
    CreateAppSession { .. } => None, Network,
    CreateBuildSession { .. } => None, Network,
    PersonalVolumes { .. } => None, Concurrent,
    SelectPersonalVolumes { .. } => None, Concurrent,
    AddPersonalVolumes { .. } => None, Concurrent,
    CheckUpdates { .. } => None, Concurrent,
    PrepareApp { .. } => None, Network,
    CreateSession { .. } => None, Network,
    Invitation { .. } => None, Concurrent,
    RenameEntryComponent { .. } => None, Concurrent,
    KvRename { .. } => None, Concurrent,
}

pub(crate) fn request_ready(request: &Request, engine: &Engine) -> bool {
    let (op, _) = classify_request(request);
    route_ready(op, engine)
}

fn archived_route(request: &ArchivedRequest) -> Result<Route> {
    let (op, session) = classify_archive(request);
    if session.is_some_and(|s| s.len() > 256) {
        return Err("local routing fields exceed limit; request did not start".into());
    }
    Ok(Route { op })
}

fn route_ready(op: Op, engine: &Engine) -> bool {
    match op.readiness() {
        Readiness::Concurrent => true,
        Readiness::Network => engine.network_request_ready(),
        Readiness::IdleOnly => !engine.background_pending(),
    }
}

enum State {
    Reading {
        header: [u8; codec::HEADER_LEN],
        header_read: usize,
        body: codec::Bytes,
        read: usize,
    },
    Ready {
        body: codec::Bytes,
        route: Route,
        owned_bytes: usize,
    },
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    Service {
        ticket: crate::engine::service_call::executor::Ticket,
    },
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    FileService {
        ticket: crate::engine::file_service::Ticket,
    },
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    InstanceEvents {
        channel: Box<crate::engine::service_events::channel::Channel>,
    },
    NetworkRequest {
        ticket: crate::engine::background::NetworkTicket,
        owned_bytes: usize,
    },
    LocalBundle {
        owned_bytes: usize,
    },
    PreparedNetwork {
        request: Box<Request>,
        owned_bytes: usize,
    },
    WaitingChanges {
        after: crate::engine::ChangeToken,
    },
    Writing {
        header: [u8; codec::HEADER_LEN],
        body: codec::Bytes,
        written: usize,
        _timing: crate::trace::Interval,
    },
}
struct Pending {
    token: usize,
    stream: UnixStream,
    state: State,
    accepted: Instant,
    deadline: Instant,
}
impl Pending {
    fn is_events(&self) -> bool {
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        if matches!(self.state, State::InstanceEvents { .. }) {
            return true;
        }
        false
    }
    fn new(token: usize, stream: UnixStream) -> Self {
        let accepted = Instant::now();
        Self {
            token,
            stream,
            accepted,
            deadline: accepted + READ_TIMEOUT,
            state: State::Reading {
                header: [0; codec::HEADER_LEN],
                header_read: 0,
                body: codec::Bytes::new(),
                read: 0,
            },
        }
    }
    fn allocated(&self) -> usize {
        match &self.state {
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            State::InstanceEvents { .. } => 0,
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            State::Service { .. } | State::FileService { .. } => 0,
            State::Reading { body, header_read, .. } => {
                body.capacity() + if *header_read >= 4 { 256 } else { 0 }
            }
            State::Ready { body, owned_bytes, .. } => body.capacity() + owned_bytes,
            State::Writing { body, .. } => body.capacity(),
            State::NetworkRequest { owned_bytes, .. }
            | State::LocalBundle { owned_bytes }
            | State::PreparedNetwork { owned_bytes, .. } => 1024 + owned_bytes,
            State::WaitingChanges { .. } => 1024,
        }
    }
    fn receiving_channels(&self) -> bool {
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        if matches!(&self.state, State::Service { ticket }
            if ticket.kind == crate::runtime::host::DataKind::ChannelEvents as u32)
        {
            return true;
        }
        false
    }
    fn timeout(&self, now: Instant) -> Option<Duration> {
        if self.is_events()
            || self.receiving_channels()
            || matches!(self.state, State::WaitingChanges { .. })
        {
            None
        } else {
            Some(self.deadline.saturating_duration_since(now))
        }
    }
    fn dispatched(&self) -> bool {
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        if matches!(self.state, State::Service { .. } | State::FileService { .. }) {
            return true;
        }
        if self.is_events() {
            return true;
        }
        matches!(
            self.state,
            State::Writing { .. } | State::NetworkRequest { .. } | State::LocalBundle { .. }
        )
    }
    fn expired(&self) -> bool {
        !self.is_events()
            && !self.receiving_channels()
            && !matches!(self.state, State::WaitingChanges { .. })
            && self.deadline <= Instant::now()
    }
    fn interest(&self) -> Event {
        match self.state {
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            State::InstanceEvents { ref channel } => {
                Event::new(self.token, true, channel.writing())
            }
            State::Writing { .. } => Event::writable(self.token),
            _ => Event::readable(self.token),
        }
    }
    // Check the connection immediately before dispatch. EOF or another frame
    // cancels a queued operation; no mutation has started at this point.
    fn still_connected(&mut self) -> Result<bool> {
        let mut byte = [0];
        loop {
            match self.stream.read(&mut byte) {
                Ok(0) => return Ok(false),
                Ok(_) => return Err("one request per local connection".into()),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
    fn read(&mut self, credit: usize, budget: &mut usize) -> Result<bool> {
        if self.is_events() {
            return self.still_connected();
        }
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        if matches!(self.state, State::Service { .. } | State::FileService { .. }) {
            return self.still_connected();
        }
        if matches!(
            self.state,
            State::Ready { .. }
                | State::NetworkRequest { .. }
                | State::LocalBundle { .. }
                | State::PreparedNetwork { .. }
                | State::WaitingChanges { .. }
        ) {
            return self.still_connected();
        }
        let ceiling = self.allocated().checked_add(credit).ok_or("request credit overflow")?;
        let State::Reading { header, header_read, body, read } = &mut self.state else {
            return Ok(true);
        };
        let mut remaining = IO_QUANTUM.min(*budget);
        while remaining > 0 {
            let target = if *header_read < 4 {
                &mut header[*header_read..4]
            } else if *header_read < codec::HEADER_LEN {
                &mut header[*header_read..]
            } else {
                &mut body[*read..]
            };
            if target.is_empty() {
                break;
            }
            let length = target.len().min(remaining);
            match self.stream.read(&mut target[..length]) {
                Ok(0) => return Ok(false),
                Ok(count) => {
                    remaining -= count;
                    *budget -= count;
                    if *header_read < codec::HEADER_LEN {
                        *header_read += count;
                        if *header_read == 4 {
                            codec::archive_length(u32::from_le_bytes(header[..4].try_into()?))?;
                        }
                        if *header_read == codec::HEADER_LEN {
                            codec::check_prefix(header[4..].try_into()?)?;
                            let length =
                                codec::archive_length(u32::from_le_bytes(header[..4].try_into()?))?;
                            if length.checked_add(256).is_none_or(|n| n > ceiling) {
                                return Err(
                                    "local request byte limit reached; request did not start"
                                        .into(),
                                );
                            }
                            *body = codec::Bytes::with_capacity(length);
                            body.resize(length, 0);
                        }
                    } else {
                        *read += count;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
            if *header_read == codec::HEADER_LEN && *read == body.len() {
                let _timing = crate::trace::Span::new("ipc.request_receive");
                let request = codec::access::<ArchivedRequest>(body)?;
                let owned_bytes = request_memory::required(request)?;
                let route = archived_route(request)?;
                let retained = body.capacity().checked_add(owned_bytes);
                if retained.is_none_or(|n| n > ceiling) {
                    return Err("local request byte limit reached; request did not start".into());
                }
                crate::trace::mark("ipc.request_complete");
                self.state = State::Ready { body: std::mem::take(body), route, owned_bytes };
                self.deadline = self.accepted + QUEUE_TIMEOUT;
                return self.still_connected();
            }
        }
        Ok(true)
    }
    fn reply(&mut self, response: &Reply, credit: usize) -> Result<()> {
        let timing = crate::trace::Interval::new("ipc.reply");
        let limit = credit.min(MAX_MESSAGE).saturating_sub(codec::PREFIX_LEN);
        let body = codec::encode(response, limit).or_else(|_| {
            codec::encode(&Reply::Error { error: ipc::OVERSIZED_REPLY.into() }, limit)
        })?;
        let header = codec::frame_header(body.len())?;
        self.state = State::Writing { header, body, written: 0, _timing: timing };
        self.deadline = Instant::now() + WRITE_TIMEOUT;
        Ok(())
    }
    fn reply_changes(&mut self, engine: &Engine) -> Result<()> {
        let current = engine.change_token();
        self.reply(&Reply::Changes(current), ipc::CHANGE_REPLY_CREDIT)
    }

    fn write(&mut self, budget: &mut usize) -> Result<bool> {
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        if let State::InstanceEvents { channel } = &mut self.state {
            return channel.write(&mut self.stream, budget);
        }
        let State::Writing { header, body, written, _timing } = &mut self.state else {
            return Ok(true);
        };
        let mut remaining = IO_QUANTUM.min(*budget);
        while remaining > 0 && *written < codec::HEADER_LEN + body.len() {
            let bytes = if *written < codec::HEADER_LEN {
                &header[*written..]
            } else {
                &body[*written - codec::HEADER_LEN..]
            };
            match self.stream.write(&bytes[..bytes.len().min(remaining)]) {
                Ok(0) => return Err("local reply closed before completion".into()),
                Ok(count) => {
                    *written += count;
                    remaining -= count;
                    *budget -= count;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let incomplete = *written != codec::HEADER_LEN + body.len();
        Ok(incomplete)
    }
}
/// Owns all registered descriptors until they are deleted from the poller.
/// Remote dispatch/hydration isolation remains a separate repair: readiness
/// alone cannot make a synchronous network operation fair.
pub struct AdmissionQueue {
    poller: Poller,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    endpoint_watch: Option<(usize, std::os::fd::OwnedFd)>,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    endpoint_changed: bool,
    events: Events,
    listener: UnixListener,
    completion: UnixStream,
    pending: VecDeque<Pending>,
    bundle: Option<bundle::Task>,
    next_token: usize,
    last_io_token: usize,
    gauges: gauges::Gauges,
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    owner_watches: Vec<(usize, crate::engine::process_owner::Watch, std::os::fd::OwnedFd)>,
}
impl AdmissionQueue {
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[allow(unsafe_code, reason = "The owned duplicate outlives its poll registration.")]
    pub(crate) fn watch_endpoint(&mut self, changes: &std::os::fd::OwnedFd) -> Result<()> {
        assert!(self.endpoint_watch.is_none(), "one endpoint watch per owner");
        let changes = changes.try_clone()?;
        let token = self.next_token;
        self.next_token = token.checked_add(1).ok_or("Local endpoint event token exhausted")?;
        // SAFETY: Drop removes registration before closing the descriptor.
        unsafe {
            self.poller.add(&changes, Event::readable(token))?;
        }
        self.endpoint_watch = Some((token, changes));
        self.endpoint_changed = true;
        Ok(())
    }
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn endpoint_changed(&mut self) -> bool {
        std::mem::take(&mut self.endpoint_changed)
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[allow(unsafe_code, reason = "Owned duplicates outlive their poll registrations.")]
    fn watch_owners(&mut self, engine: &Engine) -> Result<()> {
        use crate::engine::process_owner::Watch;
        let owners: Vec<_> = engine
            .session_instances
            .exit_descriptors()
            .map(|(id, exit)| (Watch::SessionInstance(id), exit))
            .chain(engine.tool_contexts.exits().map(|(id, exit)| (Watch::Tool(id), exit)))
            .collect();
        // Explicitly closed tool contexts do not retain kernel descriptors until
        // their process eventually exits. Remove registration before its FD.
        let mut index = 0;
        while index < self.owner_watches.len() {
            if owners.iter().any(|(owner, _)| *owner == self.owner_watches[index].1) {
                index += 1;
            } else {
                let (_, _, descriptor) = self.owner_watches.remove(index);
                self.poller.delete(&descriptor)?;
            }
        }
        for (instance, exit) in owners {
            if self.owner_watches.iter().any(|watch| watch.1 == instance) {
                continue;
            }
            let descriptor = exit.try_clone()?;
            let token = self.next_token;
            self.next_token = token.checked_add(1).ok_or("Local owner event token exhausted")?;
            // SAFETY: Drop or exit readiness removes registration before close.
            unsafe {
                self.poller.add(&descriptor, Event::readable(token))?;
            }
            self.owner_watches.push((token, instance, descriptor));
        }
        Ok(())
    }
    #[allow(
        unsafe_code,
        reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
    )]
    pub fn new(listener: UnixListener, engine: &Engine) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let completion = engine
            .completion_socket()
            .ok_or("IPC admission requires Unix completion delivery")?
            .try_clone()?;
        let poller = Poller::new()?;
        // SAFETY: these descriptors remain owned until Drop deletes them.
        unsafe {
            poller.add(&listener, Event::readable(LISTENER))?;
        }
        if let Err(error) = unsafe { poller.add(&completion, Event::readable(COMPLETION)) } {
            poller.delete(&listener)?;
            return Err(error.into());
        }
        Ok(Self {
            poller,
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            endpoint_watch: None,
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            endpoint_changed: false,
            events: Events::new(),
            listener,
            completion,
            pending: VecDeque::new(),
            bundle: None,
            next_token: 2,
            last_io_token: COMPLETION,
            gauges: gauges::Gauges::new(),
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            owner_watches: Vec::new(),
        })
    }
    fn allocated(&self) -> usize {
        self.pending.iter().map(Pending::allocated).sum()
    }
    fn requests(&self) -> usize {
        self.pending.iter().filter(|pending| !pending.is_events()).count()
    }
    fn close(&mut self, index: usize, completed_reply: bool) -> Result<()> {
        assert!(
            self.requests() <= MAX_PENDING,
            "admitted data socket queue stays bounded at drain"
        );
        if let Some(task) = &self.bundle
            && task.token == self.pending[index].token
        {
            task.cancel_read();
        }
        self.poller.delete(&self.pending[index].stream)?;
        drop(self.pending.remove(index));
        if completed_reply {
            crate::trace::mark("ipc.reply");
        }
        Ok(())
    }
    fn rearm(&self, index: usize) -> Result<()> {
        let pending = &self.pending[index];
        self.poller.modify(&pending.stream, pending.interest())?;
        Ok(())
    }
    fn error_reply(&mut self, index: usize, error: String) -> Result<()> {
        let credit = MAX_BUFFERED.saturating_sub(self.allocated());
        if self.pending[index].reply(&Reply::Error { error }, credit).is_err() {
            self.close(index, false)?;
        } else {
            self.rearm(index)?;
        }
        Ok(())
    }
    #[allow(
        unsafe_code,
        reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
    )]
    fn accept(&mut self) -> Result<()> {
        for _ in 0..8 {
            let (mut stream, _) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            stream.set_nonblocking(true)?;
            if ipc::verify_peer(&stream).is_err() {
                continue;
            }
            if self.requests() == MAX_PENDING {
                // Best effort once: rejecting a client may never stall Engine or
                // allocate another queued reply outside the admission bound.
                let error = Reply::Error { error: ipc::REQUEST_QUEUE_FULL.into() };
                if let Ok(archive) = codec::encode(&error, 512 - codec::HEADER_LEN) {
                    let mut frame = [0; 512];
                    let header = codec::frame_header(archive.len())?;
                    frame[..codec::HEADER_LEN].copy_from_slice(&header);
                    frame[codec::HEADER_LEN..codec::HEADER_LEN + archive.len()]
                        .copy_from_slice(&archive);
                    let length = codec::HEADER_LEN + archive.len();
                    let _ = stream.write(&frame[..length]);
                }
                continue;
            }
            let token = self.next_token;
            self.next_token = token.checked_add(1).ok_or("local connection token exhausted")?;
            let pending = Pending::new(token, stream);
            // SAFETY: close()/Drop delete the registration before its owner.
            unsafe {
                self.poller.add(&pending.stream, pending.interest())?;
            }
            assert!(self.requests() < MAX_PENDING, "socket admission owns a pending slot");
            self.pending.push_back(pending);
        }
        self.poller.modify(&self.listener, Event::readable(LISTENER))?;
        Ok(())
    }
    fn dispatchable(&self, engine: &Engine) -> Option<usize> {
        if self.allocated() > MAX_BUFFERED - MAX_MESSAGE {
            return None;
        }
        let preparation_waiting = self.pending.iter().any(|pending| {
            matches!(&pending.state, State::Ready { route, .. }
                if route.op != Op::PayloadSize && route.op.local_preparation())
        });
        if preparation_waiting && let Some(task) = &self.bundle {
            // Optional measurement yields its slot to app opening/import/copy.
            // Accepted mutations have no read cancellation.
            task.cancel_read();
        }
        for (index, pending) in self.pending.iter().enumerate() {
            let State::Ready { route, .. } = &pending.state else { continue };
            let ready = if route.op.local_preparation() {
                self.bundle.is_none() && (route.op != Op::PayloadSize || !preparation_waiting)
            } else {
                route.ready(engine)
            };
            if ready {
                return Some(index);
            }
        }
        None
    }
    fn finish_completions(&mut self, engine: &mut Engine) -> Result<()> {
        if MAX_BUFFERED.saturating_sub(self.allocated()) >= MAX_MESSAGE
            && let Some(task) = &self.bundle
            && let Some(preparation) = task.take()
        {
            let token = task.token;
            let changed = task.changes_state();
            self.bundle = None;
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            {
                engine.purging_app = None;
            }
            if changed {
                engine.state_changed();
            }
            if let Some(index) = self.pending.iter().position(|pending| pending.token == token) {
                match preparation {
                    bundle::Preparation::Reply(response) => {
                        let credit = MAX_BUFFERED - self.allocated();
                        self.pending[index].reply(&response, credit)?;
                    }
                    bundle::Preparation::Network(request) => {
                        let State::LocalBundle { owned_bytes } = self.pending[index].state else {
                            return Err("local preparation lost its admission reservation".into());
                        };
                        self.pending[index].state = State::PreparedNetwork { request, owned_bytes };
                    }
                }
                self.rearm(index)?;
            }
        }
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        engine.maintain_service()?;
        for index in 0..self.pending.len() {
            if matches!(self.pending[index].state, State::PreparedNetwork { .. })
                && engine.network_request_ready()
            {
                match self.pending[index].still_connected() {
                    Ok(false) => {
                        self.close(index, false)?;
                        break;
                    }
                    Err(error) => {
                        self.error_reply(index, error.to_string())?;
                        continue;
                    }
                    Ok(true) => {}
                }
                if self.pending[index].deadline <= Instant::now() {
                    self.error_reply(
                        index,
                        "Local request timed out waiting for publisher trust; \
                         this request did not start"
                            .into(),
                    )?;
                    continue;
                }
                let state = std::mem::replace(
                    &mut self.pending[index].state,
                    State::LocalBundle { owned_bytes: 0 },
                );
                let State::PreparedNetwork { request, owned_bytes } = state else {
                    unreachable!("checked prepared network request")
                };
                match engine.begin_network_request(*request) {
                    Ok(ticket) => {
                        self.pending[index].state = State::NetworkRequest { ticket, owned_bytes }
                    }
                    Err(error) => self.error_reply(index, error.to_string())?,
                }
                self.rearm(index)?;
            }
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if matches!(&self.pending[index].state, State::Service { ticket } if ticket.ready()) {
                let credit = MAX_BUFFERED.saturating_sub(self.allocated());
                if credit < MAX_MESSAGE {
                    break;
                }
                let State::Service { ticket } = std::mem::replace(
                    &mut self.pending[index].state,
                    State::Reading {
                        header: [0; codec::HEADER_LEN],
                        header_read: 0,
                        body: codec::Bytes::new(),
                        read: 0,
                    },
                ) else {
                    unreachable!()
                };
                let mut response =
                    engine.service_executor.take(ticket).map(Reply::Syscall).unwrap_or_else(
                        |error| Reply::Error {
                            error: format!(
                                "Storage completion failed: {error:?}; \
                                 accepted mutations must not be replayed"
                            ),
                        },
                    );
                if let Reply::Syscall(outcome) = &mut response {
                    engine.fence_service(outcome);
                }
                self.pending[index].reply(&response, credit)?;
                self.rearm(index)?;
                continue;
            }
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if matches!(&self.pending[index].state, State::FileService { ticket } if ticket.ready())
            {
                let credit = MAX_BUFFERED.saturating_sub(self.allocated());
                if credit < MAX_MESSAGE {
                    break;
                }
                let State::FileService { ticket } = std::mem::replace(
                    &mut self.pending[index].state,
                    State::Reading {
                        header: [0; codec::HEADER_LEN],
                        header_read: 0,
                        body: codec::Bytes::new(),
                        read: 0,
                    },
                ) else {
                    unreachable!()
                };
                let outcome = engine
                    .file_service
                    .as_mut()
                    .expect("file ticket retains its executor")
                    .take(&ticket, engine.services.store.delivery_revision());
                let Some(outcome) = outcome else {
                    // A recovery grant can be revoked after readiness. Keep the
                    // receipt and ownership until its worker cleanup completes.
                    self.pending[index].state = State::FileService { ticket };
                    continue;
                };
                self.pending[index].reply(&Reply::FileCall(outcome), credit)?;
                self.rearm(index)?;
                continue;
            }
            if let State::NetworkRequest { ticket, .. } = &self.pending[index].state {
                if !engine.network_request_complete(ticket) {
                    continue;
                }
                let credit = MAX_BUFFERED.saturating_sub(self.allocated());
                if credit < MAX_MESSAGE {
                    break;
                }
                let State::NetworkRequest { ticket, .. } = std::mem::replace(
                    &mut self.pending[index].state,
                    State::Reading {
                        header: [0; codec::HEADER_LEN],
                        header_read: 0,
                        body: codec::Bytes::new(),
                        read: 0,
                    },
                ) else {
                    unreachable!()
                };
                let response = engine.take_network_request(ticket).unwrap_or_else(|error| {
                    super::reply::Reply::Error {
                        error: super::failure::message(&engine.services.path, error.as_ref()),
                    }
                });
                self.pending[index].reply(&response, credit)?;
                self.rearm(index)?;
                continue;
            }
        }
        Ok(())
    }
    fn finish_change_waits(&mut self, engine: &mut Engine) -> Result<()> {
        let current = engine.change_token();
        for index in 0..self.pending.len() {
            let State::WaitingChanges { after } = self.pending[index].state else { continue };
            if after == current {
                continue;
            }
            let _timing = crate::trace::Span::new("ipc.change_ready");
            self.pending[index].reply_changes(engine)?;
            self.rearm(index)?;
        }
        Ok(())
    }
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    fn finish_instance_events(&mut self, engine: &Engine) -> Result<()> {
        let revision = engine.services.store.delivery_revision();
        let mut bytes = 0usize;
        let mut count = 0usize;
        for index in 0..self.pending.len() {
            if let State::InstanceEvents { channel } = &mut self.pending[index].state {
                count += 1;
                let stop = (!engine.session_instances.owns_context(channel.context))
                    .then_some(crate::engine::service_events::StopReason::ContextUnavailable);
                let mount_revision =
                    engine.session_instances.mount_revision(channel.context.instance);
                let mounts_changed = mount_revision != channel.mount_revision();
                if channel.needs_update(revision, mount_revision, stop) {
                    // Keep omitted personal roots in the dependency scope: a
                    // later authority change can make them eligible again.
                    let binding = engine.session_instances.descriptor(channel.context.instance);
                    let scope = binding.as_ref().ok().and_then(|binding| {
                        super::service_call::instance_delivery_volumes(
                            &engine.services.store,
                            binding,
                        )
                        .ok()
                    });
                    let ((revision, changed), (authority_revision, authority_changed)) = match scope
                    {
                        Some(scope) => (
                            engine
                                .services
                                .store
                                .delivery_scope_revision(channel.observed(), &scope),
                            engine
                                .services
                                .store
                                .delivery_snapshot_revision(channel.observed(), &scope),
                        ),
                        None => ((None, None), (None, None)),
                    };
                    // Never advance to the second query's frontier: changes
                    // arriving between queries remain pending for the next pass.
                    let revision =
                        revision.filter(|_| authority_revision.is_some() && binding.is_ok());
                    let data_revision = changed.unwrap_or(channel.data_revision());
                    let volumes = if revision.is_none() {
                        Err("Guest context delivery is unavailable".into())
                    } else if authority_changed.is_some() || mounts_changed || stop.is_some() {
                        engine.instance_volumes(channel.context)
                    } else {
                        Ok(channel.mounts())
                    };
                    if let Err(error) = &volumes {
                        crate::diagnostic!(
                            "Guest context {} revoked: {error}",
                            channel.context.instance
                        );
                    }

                    channel.observe(
                        revision,
                        data_revision,
                        mount_revision,
                        stop.or_else(|| {
                            volumes.is_err().then(|| engine.instance_stop_reason(channel.context))
                        }),
                        volumes.unwrap_or_default(),
                        engine.services.store.guest_display_name().clone(),
                    )?;
                }
                bytes += channel.allocated();
                self.rearm(index)?;
            }
        }
        assert!(
            count <= crate::engine::service_events::MAX_CHANNELS,
            "guest event channel count is bounded"
        );
        assert!(
            bytes <= crate::engine::service_events::MAX_PROFILE_BYTES,
            "fixed-size guest events fit profile credit"
        );
        Ok(())
    }
    #[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
    pub(crate) fn connections(&self) -> (bool, usize) {
        (!self.pending.is_empty() || self.bundle.is_some(), self.next_token)
    }
    pub fn run_once(&mut self, engine: &mut Engine) -> Result<()> {
        self.run_until(engine, None)
    }
    /// An owner lifecycle or local UI deadline shares the existing readiness
    /// wait. Pass only an actual scheduled deadline, never a freshness timer.
    pub fn run_until(&mut self, engine: &mut Engine, deadline: Option<Instant>) -> Result<()> {
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        self.watch_owners(engine)?;
        self.finish_completions(engine)?;
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        self.finish_instance_events(engine)?;
        engine.start_network_work();
        self.finish_change_waits(engine)?;
        let now = Instant::now();
        let timeout = if self.dispatchable(engine).is_some() {
            Some(Duration::ZERO)
        } else {
            self.pending
                .iter()
                .filter_map(|p| p.timeout(now))
                .chain(engine.network_wake_after(now))
                .chain(deadline.map(|deadline| deadline.saturating_duration_since(now)))
                .min()
        };
        self.sample_gauges();
        engine.sample_gauges();
        let mut budget = TURN_BYTES;
        self.poll_io(engine, timeout, &mut budget)?;
        self.finish_completions(engine)?;
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        self.finish_instance_events(engine)?;
        self.finish_change_waits(engine)?;
        self.expire_requests()?;
        self.dispatch_ready(engine, &mut budget)?;
        debug_assert!(self.allocated() <= MAX_BUFFERED);
        self.sample_gauges();
        engine.sample_gauges();
        Ok(())
    }
    fn poll_io(
        &mut self,
        engine: &mut Engine,
        timeout: Option<Duration>,
        budget: &mut usize,
    ) -> Result<()> {
        self.events.clear();
        self.poller.wait(&mut self.events, timeout)?;
        let mut events: Vec<_> = self.events.iter().collect();
        // Kernel event order is not a fairness policy. Rotate socket turns so
        // the same bulk readers cannot consume every aggregate byte quantum.
        events.sort_by_key(|event| {
            (
                if event.key < 2 {
                    0
                } else if event.key > self.last_io_token {
                    1
                } else {
                    2
                },
                event.key,
            )
        });
        for event in events {
            match event.key {
                LISTENER => self.accept()?,
                COMPLETION => {
                    if let Err(error) = engine.complete_background() {
                        crate::diagnostic!("background completion: {error}");
                    }
                    self.poller.modify(&self.completion, Event::readable(COMPLETION))?;
                }
                token => {
                    #[cfg(all(not(target_os = "ios"), feature = "process"))]
                    if let Some((watched, descriptor)) = &self.endpoint_watch
                        && token == *watched
                    {
                        self.endpoint_changed = true;
                        self.poller.modify(descriptor, Event::readable(token))?;
                        continue;
                    }
                    #[cfg(all(not(target_os = "ios"), feature = "process"))]
                    if let Some(index) =
                        self.owner_watches.iter().position(|watch| watch.0 == token)
                    {
                        let (_, instance, descriptor) = self.owner_watches.remove(index);
                        self.poller.delete(&descriptor)?;
                        match instance {
                            crate::engine::process_owner::Watch::SessionInstance(id) => {
                                engine.session_instances.owner_exited(id)?;
                                engine.network.blobs.retire_channels(*id.as_bytes());
                            }
                            crate::engine::process_owner::Watch::Tool(id) => {
                                engine.tool_contexts.owner_exited(id)
                            }
                        }
                        engine.state_changed();
                        continue;
                    }
                    let Some(index) = self.pending.iter().position(|p| p.token == token) else {
                        continue;
                    };
                    if self.pending[index].expired() {
                        if self.pending[index].dispatched() {
                            self.close(index, false)?;
                        } else {
                            self.error_reply(
                                index,
                                "Local request timed out while waiting; this request did not start"
                                    .into(),
                            )?;
                        }
                        continue;
                    }
                    if *budget == 0 {
                        self.rearm(index)?;
                        continue;
                    }
                    self.last_io_token = token;
                    let persistent = self.pending[index].is_events();
                    #[cfg(all(not(target_os = "ios"), feature = "process"))]
                    let event_writing = matches!(&self.pending[index].state, State::InstanceEvents { channel } if channel.writing());
                    #[cfg(not(all(not(target_os = "ios"), feature = "process")))]
                    let event_writing = false;
                    let writing = matches!(self.pending[index].state, State::Writing { .. })
                        || (event_writing && event.writable);
                    let credit = MAX_RECEIVED.saturating_sub(self.allocated());
                    let result = if writing {
                        self.pending[index].write(budget)
                    } else {
                        self.pending[index].read(credit, budget)
                    };
                    match result {
                        Ok(false) => self.close(index, writing && !persistent)?,
                        Ok(true) => self.rearm(index)?,
                        Err(_) if persistent => self.close(index, false)?,
                        Err(error) if !writing => self.error_reply(
                            index,
                            super::failure::message(&engine.services.path, error.as_ref()),
                        )?,
                        Err(error) => {
                            crate::diagnostic!("local reply: {error}");
                            self.close(index, false)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn expire_requests(&mut self) -> Result<()> {
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].expired() {
                if self.pending[index].dispatched() {
                    self.close(index, false)?;
                    continue;
                }
                self.error_reply(
                    index,
                    "Local request timed out while waiting; this request did not start".into(),
                )?;
            }
            index += 1;
        }
        Ok(())
    }
    fn dispatch_ready(&mut self, engine: &mut Engine, budget: &mut usize) -> Result<()> {
        // One non-retriable synchronous operation per turn. Ready socket work
        // gets another turn before selecting the next oldest eligible request.
        if let Some(index) = self.dispatchable(engine) {
            match self.pending[index].still_connected() {
                Ok(false) => {
                    self.close(index, false)?;
                    return Ok(());
                }
                Err(error) => {
                    self.error_reply(
                        index,
                        super::failure::message(&engine.services.path, error.as_ref()),
                    )?;
                    return Ok(());
                }
                Ok(true) => (),
            }
            let State::Ready { body, owned_bytes, .. } = &self.pending[index].state else {
                unreachable!()
            };
            let owned_bytes = *owned_bytes;
            let request: Request = match codec::access::<ArchivedRequest>(body)
                .and_then(rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>)
            {
                Ok(request) => request,
                Err(error) => {
                    self.error_reply(
                        index,
                        format!("Invalid local request; request did not start: {error}"),
                    )?;
                    return Ok(());
                }
            };
            if self.pending[index].deadline <= Instant::now() {
                self.error_reply(
                    index,
                    "Local request timed out while waiting; this request did not start".into(),
                )?;
                return Ok(());
            }
            crate::trace::mark("ipc.dispatch_start");
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if let Request::InstanceEvents { context } = request {
                let channels: Vec<_> = self
                    .pending
                    .iter()
                    .filter_map(|pending| match &pending.state {
                        State::InstanceEvents { channel } => Some(channel.context),
                        _ => None,
                    })
                    .collect();
                let result = if channels.len() >= crate::engine::service_events::MAX_CHANNELS {
                    Err("Guest event channel limit reached".into())
                } else if channels.contains(&context) {
                    Err("This guest context already owns an event channel".into())
                } else {
                    engine
                        .session_instances
                        .authorize(context, &self.pending[index].stream)
                        .map_err(|error| error.to_string().into())
                        .and_then(|_| {
                            crate::engine::service_events::channel::Channel::new(
                                context,
                                engine.services.store.delivery_revision(),
                                engine.session_instances.mount_revision(context.instance),
                                engine.instance_volumes(context)?,
                                engine.services.store.guest_display_name().clone(),
                            )
                        })
                };
                match result {
                    Ok(channel) => {
                        self.pending[index].state =
                            State::InstanceEvents { channel: Box::new(channel) };
                        self.rearm(index)?;
                    }
                    Err(error) => self.error_reply(
                        index,
                        super::failure::message(&engine.services.path, error.as_ref()),
                    )?,
                }
                return Ok(());
            }
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if matches!(
                &request,
                Request::ToolContextOpen { .. } | Request::ToolContextClose { .. }
            ) {
                let stream = &self.pending[index].stream;
                let response = match request {
                    Request::ToolContextOpen { scope } => engine
                        .tool_contexts
                        .open(&engine.services.store, scope, stream)
                        .map(|context| Reply::ToolContext { context }),
                    Request::ToolContextClose { context } => engine
                        .tool_contexts
                        .close(context, stream)
                        .map(|()| Reply::ToolContextClosed { context })
                        .map_err(|error| format!("Tool context close failed: {error:?}").into()),
                    _ => unreachable!("tool context request classified above"),
                }
                .unwrap_or_else(|error| Reply::Error {
                    error: super::failure::message(&engine.services.path, error.as_ref()),
                });
                let credit = MAX_BUFFERED - self.allocated();
                self.pending[index].reply(&response, credit)?;
                self.rearm(index)?;
                return Ok(());
            }
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if let Request::FileCall { context, operation, request } = request {
                let kind = request.kind();
                let result =
                    engine.begin_file(context, operation, *request, &self.pending[index].stream);
                match result {
                    Ok(ticket) => self.pending[index].state = State::FileService { ticket },
                    Err(error) => {
                        let result = Err(error);
                        let credit = MAX_BUFFERED - self.allocated();
                        self.pending[index].reply(
                            &Reply::FileCall(super::file_service::Outcome::result(
                                context,
                                operation,
                                kind,
                                result,
                                engine.services.store.delivery_revision(),
                            )),
                            credit,
                        )?;
                    }
                }
                self.rearm(index)?;
                return Ok(());
            }
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if let Request::Syscall { context, operation, mutation, call } = request {
                let kind = call.operation.kind() as u32;
                let volume = crate::store::Id::from_bytes(call.volume);
                match engine.begin_service(
                    context,
                    operation,
                    mutation,
                    *call,
                    &self.pending[index].stream,
                ) {
                    Ok(ticket) => self.pending[index].state = State::Service { ticket },
                    Err(error) => {
                        let credit = MAX_BUFFERED - self.allocated();
                        self.pending[index].reply(
                            &Reply::Syscall(crate::engine::service_call::Outcome {
                                context,
                                volume,
                                operation,
                                mutation,
                                kind,
                                revision: None,
                                status: error as i32,
                                result: None,
                            }),
                            credit,
                        )?;
                    }
                }
                self.rearm(index)?;
                return Ok(());
            }
            #[cfg(all(not(target_os = "ios"), feature = "process"))]
            if matches!(
                request,
                Request::InstanceInspect { .. }
                    | Request::PrepareSession { .. }
                    | Request::InstanceStopReason { .. }
                    | Request::InstanceList { .. }
                    | Request::InstanceConfigurations { .. }
                    | Request::InstanceConfiguration { .. }
                    | Request::InstanceRestore { .. }
                    | Request::InstanceMounts { .. }
                    | Request::InstanceLaunchFailed { .. }
                    | Request::InstanceReserve { .. }
                    | Request::InstanceStartup { .. }
                    | Request::InstanceShow { .. }
                    | Request::InstanceCompiled { .. }
                    | Request::InstanceClaim { .. }
                    | Request::InstanceReconstruct { .. }
                    | Request::InstanceChannelsAck { .. }
                    | Request::InstanceChannelsClose { .. }
            ) {
                let response = engine
                    .dispatch_instance(request, &self.pending[index].stream)
                    .unwrap_or_else(|error| Reply::Error {
                        error: super::failure::message(&engine.services.path, error.as_ref()),
                    });
                let credit = MAX_BUFFERED - self.allocated();
                self.pending[index].reply(&response, credit)?;
                self.rearm(index)?;
                return Ok(());
            }
            if self.dispatch_wait(index, engine, &request)? {
                return Ok(());
            }
            if matches!(
                request,
                Request::SignPackage { .. }
                    | Request::AuthenticatePackage { .. }
                    | Request::PrepareApp { .. }
                    | Request::InstanceCompile { .. }
                    | Request::CreateSession { .. }
                    | Request::OpenAppVolume { .. }
                    | Request::OpenAutomaticVolume { .. }
                    | Request::DuplicateVolume { .. }
                    | Request::AppRemovalImpact { .. }
                    | Request::RemoveApp { .. }
                    | Request::DuplicateEntry { .. }
                    | Request::PayloadSize { .. }
            ) {
                assert!(self.bundle.is_none(), "one local preparation at a time");
                #[cfg(all(not(target_os = "ios"), feature = "process"))]
                if let Request::InstanceCompile { context, .. } = &request
                    && let Err(error) =
                        engine.session_instances.authorize(*context, &self.pending[index].stream)
                {
                    self.error_reply(index, error.to_string())?;
                    return Ok(());
                }
                #[cfg(not(all(not(target_os = "ios"), feature = "process")))]
                if matches!(request, Request::InstanceCompile { .. }) {
                    self.error_reply(
                        index,
                        "Process compilation admission is unavailable in this build".into(),
                    )?;
                    return Ok(());
                }
                #[cfg(all(not(target_os = "ios"), feature = "process"))]
                let purging = match &request {
                    Request::RemoveApp {
                        app, confirmed: true, delete_local_data: true, ..
                    } => Some(app.clone()),
                    _ => None,
                };
                match bundle::Task::start(engine, self.pending[index].token, request) {
                    Ok(task) => {
                        #[cfg(all(not(target_os = "ios"), feature = "process"))]
                        {
                            engine.purging_app = purging;
                        }
                        self.bundle = Some(task);
                        self.pending[index].state = State::LocalBundle { owned_bytes };
                        self.rearm(index)?;
                    }
                    Err(error) => self.error_reply(
                        index,
                        super::failure::message(&engine.services.path, error.as_ref()),
                    )?,
                }
                return Ok(());
            }
            if crate::engine::developer::is_request(&request)
                || crate::engine::enrollment::is_request(&request)
                || crate::engine::diagnostics::is_request(&request)
                || matches!(
                    request,
                    Request::Sync
                        | Request::RecoveryStatus
                        | Request::PrepareRecovery
                        | Request::SaveRecovery
                        | Request::RecoverCredentials { .. }
                )
            {
                match engine.begin_network_request(request) {
                    Ok(ticket) => {
                        self.pending[index].state = State::NetworkRequest { ticket, owned_bytes }
                    }
                    Err(error) => {
                        self.error_reply(
                            index,
                            super::failure::message(&engine.services.path, error.as_ref()),
                        )?;
                        return Ok(());
                    }
                }
                self.rearm(index)?;
                return Ok(());
            }
            let response = engine.dispatch_reply(request).unwrap_or_else(|error| Reply::Error {
                error: super::failure::message(&engine.services.path, error.as_ref()),
            });
            let credit = MAX_BUFFERED - self.allocated();
            self.pending[index].reply(&response, credit)?;
            match self.pending[index].write(budget) {
                Ok(true) => self.rearm(index)?,
                Ok(false) => self.close(index, true)?,
                Err(error) => {
                    crate::diagnostic!("local reply: {error}");
                    self.close(index, false)?;
                }
            }
        }
        Ok(())
    }
    fn dispatch_wait(&mut self, index: usize, engine: &Engine, request: &Request) -> Result<bool> {
        if let Request::WaitChanges { after } = request {
            let current = engine.change_token();
            if after.is_some_and(|after| {
                after.generation == current.generation && after.sequence > current.sequence
            }) {
                self.error_reply(index, "Local change cursor is ahead of this engine".into())?;
            } else if *after != Some(current) {
                self.pending[index].reply_changes(engine)?;
                self.rearm(index)?;
            } else if self
                .pending
                .iter()
                .filter(|p| matches!(p.state, State::WaitingChanges { .. }))
                .count()
                == MAX_CHANGE_WAITERS
            {
                self.error_reply(index, "Local change waiter limit reached".into())?;
            } else {
                self.pending[index].state = State::WaitingChanges { after: current };
                self.rearm(index)?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}
impl Drop for AdmissionQueue {
    fn drop(&mut self) {
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        if let Some((_, descriptor)) = &self.endpoint_watch {
            let _ = self.poller.delete(descriptor);
        }
        #[cfg(all(not(target_os = "ios"), feature = "process"))]
        for (_, _, descriptor) in &self.owner_watches {
            let _ = self.poller.delete(descriptor);
        }
        for pending in &self.pending {
            let _ = self.poller.delete(&pending.stream);
        }
        let _ = self.poller.delete(&self.completion);
        let _ = self.poller.delete(&self.listener);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    mod files;
    mod kv_rename;
    use std::io;
    fn encode_request(request: &Request) -> Result<codec::Bytes> {
        Ok(codec::encode(request, codec::MAX_ARCHIVE)?)
    }
    fn decode_reply<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
        let reply = rkyv::api::low::from_bytes::<Reply, rkyv::rancor::Error>(bytes).unwrap();
        serde_json::from_value(serde_json::to_value(reply).unwrap())
    }

    #[test]
    fn idle_deadline_accepts_connections_and_counts_short_lived_clients() {
        let mut f = Fixture::new();
        let (_, before) = f.queue.connections();
        let client = f.client();
        f.queue.run_until(&mut f.engine, Some(Instant::now())).unwrap();
        let (connected, after) = f.queue.connections();
        assert!(connected);
        assert!(after > before);
        drop(client);
        for _ in 0..4 {
            f.turn();
        }
        assert_eq!(f.queue.connections(), (false, after));
        // No requests or network work: a lifecycle deadline wakes the owner.
        let start = Instant::now();
        f.queue.run_until(&mut f.engine, Some(start + Duration::from_millis(20))).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn component_rename_ipc_retains_the_nested_parent_without_waiting_for_remote() {
        // Exercise archive admission and dispatch for directory/leaf names, then the unchanged path move.
        let mut fixture = Fixture::new();
        let store = fixture.engine.services.store.clone();
        let access = crate::store::Access::owner(store.account().id);
        let volume = store.create_volume("Names", false).unwrap();
        let file =
            store.create_blob(&access, volume.id, "foo/bar/Sample Copy.bin", b"contents").unwrap();
        let bar = store.resolve_entry(&access, volume.id, "foo/bar").unwrap();
        fixture.engine.remote = None;
        for (entry, name) in [(bar.id, "bar2"), (file.id, "Sample.bin")] {
            assert!(matches!(fixture.exchange(Request::RenameEntryComponent { session: None,
                volume: volume.id, entry, name: name.into(),
            }), Reply::EntryChanged { volume: got_volume, entry: got_entry }
                if got_volume == volume.id && got_entry == entry));
        }
        assert_eq!(store.resolve_entry(&access, volume.id, "foo/bar2").unwrap().id, bar.id);
        assert_eq!(
            store.resolve_entry(&access, volume.id, "foo/bar2/Sample.bin").unwrap().id,
            file.id
        );
        assert!(store.resolve_entry(&access, volume.id, "bar2").is_err());
        assert!(store.resolve_entry(&access, volume.id, "Sample.bin").is_err());
        assert_eq!(store.read_blob_entry(&access, volume.id, file.id).unwrap(), b"contents");
        assert!(matches!(
            fixture.exchange(Request::RenameEntryComponent {
                session: None,
                volume: volume.id,
                entry: file.id,
                name: "invalid/path.bin".into(),
            }),
            Reply::Error { .. }
        ));
        assert!(matches!(
            fixture.exchange(Request::RenameEntry {
                session: None,
                volume: volume.id,
                entry: file.id,
                name: "moved/explicit.bin".into(),
            }),
            Reply::EntryChanged { .. }
        ));
        assert_eq!(
            store.resolve_entry(&access, volume.id, "moved/explicit.bin").unwrap().id,
            file.id
        );
    }

    #[test]
    fn archived_nonfinite_fields_are_rejected_without_creating_objects() {
        use crate::store::{Access, Id, object_contract::*};
        use std::collections::BTreeMap;
        let mut fixture = Fixture::new();
        let root = fixture.engine.services.store.create_volume("Float values", false).unwrap();
        let access = Access::owner(fixture.engine.services.store.account().id);
        let type_id = Id::from_bytes([2; 16]);
        let field_id = Id::from_bytes([3; 16]);
        let schema = Schema {
            types: BTreeMap::from([(
                type_id,
                TypeDeclaration {
                    name: "Measurement".into(),
                    deprecated: false,
                    fields: BTreeMap::from([(
                        field_id,
                        FieldDeclaration {
                            name: "value".into(),
                            value_type: ValueType::Float,
                            repeated: false,
                            deprecated: false,
                        },
                    )]),
                },
            )]),
        };
        let entry = fixture
            .engine
            .services
            .store
            .create_database(
                &access,
                root.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema },
            )
            .unwrap();
        let database =
            crate::store::volume_index::EntryAddress { volume: root.id, entry: entry.id };
        let Reply::ToolContext { context } = fixture.exchange(Request::ToolContextOpen {
            scope: crate::engine::service_call::ToolScope::Account,
        }) else {
            panic!("tool context")
        };
        for (index, value) in
            [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.5].into_iter().enumerate()
        {
            let call = crate::runtime::host::DataCall {
                volume: *root.id.as_bytes(),
                operation: crate::runtime::host::DataOperation::Create {
                    entry_id: *database.entry.as_bytes(),
                    type_id: *type_id.as_bytes(),
                    fields: vec![crate::runtime::host::FieldChange {
                        field_id: *field_id.as_bytes(),
                        values: Some(vec![crate::runtime::host::Value::Float(value)]),
                    }],
                },
            };
            let operation = index as u64 + 1;
            let Reply::Syscall(outcome) = fixture.exchange(Request::Syscall {
                context,
                operation,
                mutation: operation,
                call: Box::new(call.clone()),
            }) else {
                panic!("typed data outcome")
            };
            let result = outcome.take(context, operation, operation, &call);
            assert_eq!(result.is_ok(), value.is_finite());
            assert!(
                fixture
                    .engine
                    .services
                    .store
                    .query_objects(&access, database, type_id, None, 64)
                    .unwrap()
                    .objects
                    .len()
                    == usize::from(value.is_finite())
            );
        }
        assert_eq!(
            fixture
                .engine
                .services
                .store
                .query_objects(&access, database, type_id, None, 64)
                .unwrap()
                .objects
                .len(),
            1
        );
    }

    #[test]
    fn same_name_object_database_recovery_preserves_content_through_ipc_and_restart() {
        use crate::engine::reply::DiagnosticReply;
        use crate::store::initialization::InitialDatabaseKind;
        use crate::store::volume_index::{EntryAddress, EntryKind};
        use crate::store::{Access, Id, LocalStore, object_contract::Value};
        use std::collections::BTreeMap;

        let mut fixture = Fixture::new();
        let store = fixture.engine.services.store.clone();
        let access = Access::owner(store.account().id);
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        let volume =
            store.initialize_root(Id::generate().unwrap(), "My app", false, &declaration).unwrap();
        let InitialDatabaseKind::ObjectDb { schema, .. } = declaration.databases[0].kind.clone()
        else {
            panic!("object database fixture")
        };
        let original = store
            .create_database(
                &access,
                volume.id,
                "data2",
                EntryKind::ObjectDb { schema: Default::default() },
            )
            .unwrap();
        let source = EntryAddress { volume: volume.id, entry: original.id };
        assert!(matches!(
            fixture.exchange(Request::Schema {
                session: None,
                database: source,
                schema: schema.clone()
            }),
            Reply::Schema { ok: true, .. }
        ));
        let record = store
            .create_object_with_fields(
                &access,
                source,
                Id::from_bytes([1; 16]),
                &BTreeMap::from([(
                    Id::from_bytes([2; 16]),
                    Some(vec![Value::Text("retained through recovery".into())]),
                )]),
            )
            .unwrap();
        assert!(matches!(
            fixture.exchange(Request::RemoveEntry {
                session: None,
                volume: volume.id,
                entry: original.id
            }),
            Reply::EntryChanged { .. }
        ));
        let reply = fixture.exchange(Request::RecoverEntry {
            address: source,
            name: "data2".into(),
            revision: None,
            rejected_cut: None,
        });
        let Reply::Diagnostic(DiagnosticReply::EntryRecovered { entry, .. }) = reply else {
            panic!("same-name recovery failed: {reply:?}")
        };
        assert_ne!(entry.id, original.id);
        let recovered = EntryAddress { volume: volume.id, entry: entry.id };
        assert_eq!(store.schema(&access, recovered).unwrap(), schema);
        assert_eq!(store.read_object(&access, recovered, record.object_id).unwrap(), record);
        assert!(store.entry_details(&access, volume.id, original.id).unwrap().entry.deleted);
        assert!(matches!(
            fixture.exchange(Request::RecoverEntry {
                address: source,
                name: "data2".into(),
                revision: None,
                rejected_cut: None,
            }),
            Reply::Error { .. }
        ));
        drop(store);
        let Fixture { _directory: directory, engine, queue, .. } = fixture;
        drop(queue);
        drop(engine);
        let reopened = LocalStore::open(directory.path().join("profile")).unwrap();
        assert_eq!(reopened.resolve_entry(&access, volume.id, "data2").unwrap().id, entry.id);
        assert_eq!(reopened.read_object(&access, recovered, record.object_id).unwrap(), record);
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        engine: Engine,
        queue: AdmissionQueue,
        socket: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let engine = Engine::open(directory.path().join("profile")).unwrap();
            let socket = directory.path().join("socket");
            let queue = AdmissionQueue::new(UnixListener::bind(&socket).unwrap(), &engine).unwrap();
            Self { _directory: directory, engine, queue, socket }
        }
        fn turn(&mut self) {
            // An explicit test notification avoids blocking a unit test after
            // it has already drained all work; production has no such scans.
            self.queue.poller.notify().unwrap();
            self.queue.run_once(&mut self.engine).unwrap();
        }
        fn client(&self) -> UnixStream {
            let stream = UnixStream::connect(&self.socket).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            stream
        }
        fn request(&mut self, request: Request) -> Value {
            let mut client = self.client();
            ipc::write(&mut client, &encode_request(&request).unwrap()).unwrap();
            for _ in 0..8 {
                self.turn();
            }
            let bytes = ipc::read(&mut client).unwrap();
            let reply = codec::access::<crate::engine::reply::ArchivedReply>(&bytes).unwrap();
            assert!(
                matches!(reply, crate::engine::reply::ArchivedReply::Error { .. })
                    || reply.matches_request(&request),
                "unexpected successful reply for {request:?}"
            );
            decode_reply(&bytes).unwrap()
        }
        fn exchange(&mut self, request: Request) -> Reply {
            let socket = self.socket.clone();
            let worker = std::thread::spawn(move || {
                let mut stream = UnixStream::connect(socket).unwrap();
                ipc::write(&mut stream, &encode_request(&request).unwrap()).unwrap();
                let bytes = ipc::read(&mut stream).unwrap();
                let reply = codec::access::<crate::engine::reply::ArchivedReply>(&bytes).unwrap();
                assert!(
                    matches!(reply, crate::engine::reply::ArchivedReply::Error { .. })
                        || reply.matches_request(&request)
                );
                rkyv::api::low::deserialize::<Reply, rkyv::rancor::Error>(reply).unwrap()
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while !worker.is_finished() {
                assert!(Instant::now() < deadline, "isolated service exchange completes");
                self.turn();
                std::thread::yield_now();
            }
            worker.join().unwrap()
        }
        #[allow(
            unsafe_code,
            reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
        )]
        fn inject(&mut self, stream: UnixStream) -> usize {
            stream.set_nonblocking(true).unwrap();
            let token = self.queue.next_token;
            self.queue.next_token += 1;
            let pending = Pending::new(token, stream);
            unsafe {
                self.queue.poller.add(&pending.stream, pending.interest()).unwrap();
            }
            self.queue.pending.push_back(pending);
            self.queue.pending.len() - 1
        }
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[test]
    fn app_removal_checks_account_and_prevents_an_earlier_reservation_from_launching() {
        // A row and an unclaimed launch both predate removal of this app.
        use crate::store::{AppRemovalImpact, Id};
        let mut fixture = Fixture::new();
        let root = fixture.engine.services.store.create_volume("Personal data", false).unwrap();
        let session =
            fixture.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let account = fixture.engine.services.store.account().id;
        let app = session.app;
        let Reply::Instance(reserved) = fixture.exchange(Request::InstanceReserve {
            request: Id::generate().unwrap(),
            incarnation: fixture.engine.session_instances.incarnation(),
            session: session.id,
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
            restore_host_mounts: false,
        }) else {
            panic!("expected a launch reservation")
        };
        let remove = |account, confirmed| Request::RemoveApp {
            account,
            app: app.clone(),
            confirmed,
            delete_local_data: false,
        };
        assert!(matches!(
            fixture.exchange(remove(Id::generate().unwrap(), true)),
            Reply::Error { .. }
        ));
        let impact = AppRemovalImpact { user_volumes: true, local_data: false };
        assert!(
            matches!(fixture.exchange(Request::AppRemovalImpact { account, app: app.clone() }),
            Reply::AppRemovalImpact { impact: found } if found == impact)
        );
        assert!(matches!(fixture.exchange(remove(account, false)),
            Reply::AppRemoved(crate::store::AppRemovalResult::Confirmation(found)) if found == impact));
        assert!(matches!(
            fixture.exchange(remove(account, true)),
            Reply::AppRemoved(crate::store::AppRemovalResult::Removed { cleanup_error: None, .. })
        ));
        assert!(matches!(
            fixture.exchange(Request::InstanceClaim { instance: reserved.id }),
            Reply::Error { .. }
        ));
        assert_eq!(fixture.engine.services.store.volume(root.id).unwrap(), root);
        assert!(fixture.engine.services.store.retained_sessions().unwrap().is_empty());
    }

    #[test]
    fn app_removal_purges_retained_data_for_a_new_pending_only_invitation() {
        use crate::package::AutomaticVolumeRole;
        use crate::store::{Access, BuildSelector, LocalStore};
        let mut fixture = Fixture::new();
        let store = fixture.engine.services.store.clone();
        let package = crate::package::signature::testing::signed(
            &store,
            crate::engine::tests::button_package(),
        )
        .unwrap();
        let bundle = fixture._directory.path().join("button.pbapp");
        std::fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
        let Reply::Developer(crate::engine::reply::DeveloperReply::Session(local)) =
            fixture.exchange(Request::CreateSession { bundle, root: None, user: None })
        else {
            panic!("local import creates a listed app");
        };
        let volume = store.open_app_volume(&local.app, AutomaticVolumeRole::Local).unwrap();
        let owner = Access::owner(store.account().id);
        let blob = store.create_blob(&owner, volume.id, "saved", b"keep").unwrap();
        let remove = |delete_local_data| Request::RemoveApp {
            account: store.account().id,
            app: local.app.clone(),
            confirmed: true,
            delete_local_data,
        };
        assert!(matches!(
            fixture.exchange(remove(false)),
            Reply::AppRemoved(crate::store::AppRemovalResult::Removed { cleanup_error: None, .. })
        ));
        assert!(matches!(
            fixture.exchange(remove(true)),
            Reply::AppRemoved(crate::store::AppRemovalResult::Removed { cleanup_error: None, .. })
        ));
        assert_eq!(store.read_blob_entry(&owner, volume.id, blob.id).unwrap(), b"keep");

        // A new invitation from another account lists this app again before joining.
        let sender = LocalStore::open(fixture._directory.path().join("sender")).unwrap();
        let root = sender.create_volume("Shared", true).unwrap();
        let build = local.resolved_build_id;
        let invitation = sender
            .create_session(
                &local.app,
                BuildSelector::App { app: local.app.clone() },
                &build,
                root.id,
            )
            .unwrap();
        let channel = fixture.engine.channel();
        store
            .preview_invitation(&sender.invitation(&invitation.id, channel).unwrap(), channel)
            .unwrap();
        store
            .receive_invitation(&invitation.id, &sender.session_envelope(&invitation.id).unwrap())
            .unwrap();
        assert!(store.app_removed(&local.app).unwrap());
        assert!(store.app_removal_impact(&local.app).unwrap().local_data);
        assert!(matches!(
            fixture.exchange(remove(true)),
            Reply::AppRemoved(crate::store::AppRemovalResult::Removed { cleanup_error: None, .. })
        ));
        assert!(store.pending_invitations().unwrap().is_empty());
        assert!(matches!(store.volume(volume.id), Err(crate::store::Error::NotFound(_))));
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[test]
    fn binary_instance_admission_retains_owner_after_ordinary_reply_close() {
        use crate::engine::session_instance::{Context, SessionInstance, State};
        use crate::store::Id;
        let mut fixture = Fixture::new();
        let root =
            fixture.engine.services.store.create_volume("Independent instances", false).unwrap();
        let session =
            fixture.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let incarnation = fixture.engine.session_instances.incarnation();
        let nonce = Id::generate().unwrap();
        let request = Request::InstanceReserve {
            restore_host_mounts: false,
            request: nonce,
            incarnation,
            session: session.id.clone(),
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
        };
        let first: SessionInstance =
            serde_json::from_value(fixture.request(request.clone())).unwrap();
        let again: SessionInstance = serde_json::from_value(fixture.request(request)).unwrap();
        assert_eq!(first, again);
        let second: SessionInstance =
            serde_json::from_value(fixture.request(Request::InstanceReserve {
                restore_host_mounts: false,
                request: Id::generate().unwrap(),
                incarnation,
                session: session.id,
                extra_volumes: Box::default(),
                host_mounts: Box::default(),
            }))
            .unwrap();
        assert_ne!(first.id, second.id);
        let reply = fixture.request(Request::InstanceClaim { instance: first.id });
        let context: Context = serde_json::from_value(reply["context"].clone()).unwrap();
        assert_eq!(context.instance, first.id);
        assert_eq!(context.owner_generation, 1);
        assert!(!fixture.queue.connections().0, "each completed reply closed its operation socket");
        assert_eq!(fixture.queue.owner_watches.len(), 1);
        assert_eq!(
            fixture.engine.session_instances.inspect(nonce).unwrap().unwrap().state,
            State::Owned
        );
        let (_client, peer) = UnixStream::pair().unwrap();
        assert_eq!(
            fixture.engine.session_instances.authorize(context, &peer).unwrap().id,
            first.id
        );
        let rejected = fixture.request(Request::InstanceClaim { instance: second.id });
        assert!(rejected["error"].as_str().unwrap().contains("already owns"));
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[test]
    fn persistent_guest_events_have_independent_request_credit_and_eof_preserves_owner() {
        use crate::{
            engine::session_instance::{Context, SessionInstance},
            store::Id,
        };
        let mut f = Fixture::new();
        let root = f.engine.services.store.create_volume("Event fixture", true).unwrap();
        let other = f.engine.services.store.create_volume("Unrelated events", false).unwrap();
        let owner = crate::store::Access::owner(f.engine.services.store.account().id);
        let log = f.engine.services.store.create_bloblog(&owner, root.id, "edits").unwrap().id;
        let session =
            f.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let instance: SessionInstance =
            serde_json::from_value(f.request(Request::InstanceReserve {
                restore_host_mounts: false,
                request: Id::generate().unwrap(),
                incarnation: f.engine.session_instances.incarnation(),
                session: session.id.clone(),
                extra_volumes: Box::default(),
                host_mounts: Box::default(),
            }))
            .unwrap();
        let context: Context = serde_json::from_value(
            f.request(Request::InstanceClaim { instance: instance.id })["context"].clone(),
        )
        .unwrap();
        let mut client = f.client();
        ipc::write(&mut client, &encode_request(&Request::InstanceEvents { context }).unwrap())
            .unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(first) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("first notice");
        };
        assert_eq!(first.sequence, 1);
        assert!(first.stop.is_none());
        let schema = crate::store::object_contract::Schema {
            types: std::collections::BTreeMap::from([(
                Id::from_bytes([91; 16]),
                crate::store::object_contract::TypeDeclaration {
                    name: "Scoped".into(),
                    deprecated: false,
                    fields: Default::default(),
                },
            )]),
        };
        let owner = crate::store::Access::owner(f.engine.services.store.account().id);
        f.engine
            .services
            .store
            .create_database(
                &owner,
                other.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
            )
            .unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(unrelated) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("unrelated fence");
        };
        assert_ne!(unrelated.revision, first.revision);
        assert_eq!(unrelated.data_revision, first.data_revision);
        let entry = f
            .engine
            .services
            .store
            .create_database(
                &owner,
                root.id,
                "data",
                crate::store::volume_index::EntryKind::ObjectDb { schema },
            )
            .unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(mounted) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("mounted change");
        };
        assert!(mounted.data_revision > unrelated.data_revision);
        let before = mounted;
        f.engine
            .services
            .store
            .create_object(
                &owner,
                crate::store::volume_index::EntryAddress { volume: root.id, entry: entry.id },
                Id::from_bytes([91; 16]),
            )
            .unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(backing) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("backing change");
        };
        assert!(backing.data_revision > before.data_revision);
        assert_eq!(backing.volumes, before.volumes);
        let entries = crate::store::bloblog::append_entries(
            f.engine.services.store.account().id,
            root.id,
            log,
            Id::generate().unwrap(),
            &[b"incoming edit".to_vec()],
        )
        .unwrap();
        crate::engine::bloblog::receive_entries(
            &f.engine.services.store,
            &owner,
            root.id,
            log,
            &entries,
            &f.engine.completion.wake,
        )
        .unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(bloblog) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("bloblog content notice");
        };
        assert!(bloblog.data_revision > backing.data_revision);
        assert_eq!(bloblog.volumes, backing.volumes);
        f.engine.services.store.label_volume(&owner, root.id, "Renamed root").unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(mounted) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("renamed mount");
        };
        assert!(mounted.data_revision > bloblog.data_revision);
        assert_eq!(
            mounted.volumes.iter().find(|v| v.id == *root.id.as_bytes()).unwrap().name,
            "Renamed root"
        );
        assert_eq!(f.queue.requests(), 0);
        assert_eq!(f.queue.pending.len(), 1);
        assert!(
            f.request(Request::InstanceEvents { context })["error"]
                .as_str()
                .unwrap()
                .contains("already owns")
        );
        let mut waiting = Vec::new();
        for _ in 0..MAX_PENDING {
            waiting.push(f.client());
        }
        for _ in 0..8 {
            f.turn();
        }
        assert_eq!(f.queue.requests(), MAX_PENDING);
        assert_eq!(f.queue.pending.len(), MAX_PENDING + 1);
        f.engine.services.store.record_session_denial(&session, "Removed").unwrap();
        for _ in 0..8 {
            f.turn();
        }
        let Reply::InstanceEvents(next) = ({
            let bytes = codec::read(&mut client, codec::MAX_ARCHIVE).unwrap();
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        }) else {
            panic!("policy notice");
        };
        assert_eq!(next.sequence, mounted.sequence + 1);
        assert_ne!(next.revision, first.revision);
        assert_eq!(next.stop, Some(crate::engine::service_events::StopReason::ContextUnavailable));
        assert!(next.volumes.is_empty());
        drop(client);
        drop(waiting);
        for _ in 0..8 {
            f.turn();
        }
        assert!(
            f.queue.pending.is_empty(),
            "remaining sockets: {}, requests: {}",
            f.queue.pending.len(),
            f.queue.requests()
        );
        assert!(f.engine.session_instances.owns_context(context));
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[test]
    fn channel_socket_keeps_idle_receives_and_routes_acknowledgements() {
        use crate::{
            engine::session_instance::Context,
            runtime::host::{DataCall, DataOperation},
            store::Id,
        };
        let mut f = Fixture::new();
        let root = crate::engine::tests::object_volume(&f.engine.services.store, "Channels", false);
        let session =
            f.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let reserved = f.request(Request::InstanceReserve {
            restore_host_mounts: false,
            request: Id::generate().unwrap(),
            incarnation: f.engine.session_instances.incarnation(),
            session: session.id,
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
        });
        let instance = serde_json::from_value(reserved["id"].clone()).unwrap();
        let claimed = f.request(Request::InstanceClaim { instance });
        let context: Context = serde_json::from_value(claimed["context"].clone()).unwrap();
        let delivery = crate::engine::ChannelDelivery {
            account: f.engine.services.store.account().id,
            sender: instance,
            message: Id::generate().unwrap(),
        };
        assert!(matches!(
            f.exchange(Request::InstanceChannelsAck { context, delivery: Box::new(delivery) }),
            Reply::Instance(owner) if owner.id == instance
        ));
        let client = f.client();
        let ticket = f
            .engine
            .begin_service(
                context,
                crate::engine::service_call::CHANNEL_BIT | 1,
                0,
                DataCall {
                    volume: [0; 16],
                    operation: DataOperation::ChannelEvents {
                        instance: *context.instance.as_bytes(),
                    },
                },
                &client,
            )
            .unwrap();
        let mut pending = Pending::new(999, client.try_clone().unwrap());
        pending.state = State::Service { ticket };
        pending.deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(pending.timeout(Instant::now()), None);
        assert!(!pending.expired());
        let State::Service { ticket } = &mut pending.state else { panic!("service") };
        ticket.kind = crate::runtime::host::DataKind::ChannelCommand as u32;
        assert!(pending.expired());
        // Ordinary output still has a bounded write deadline.
        pending.reply(&Reply::Error { error: "closed".into() }, 4096).unwrap();
        assert!(pending.timeout(Instant::now()).is_some());
        assert!(matches!(
            f.exchange(Request::InstanceChannelsClose { context }),
            Reply::Instance(owner) if owner.id == instance
        ));
    }

    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    #[test]
    fn binary_data_exchange_loss_preserves_owner_and_never_replays_creation() {
        use crate::{
            engine::session_instance::Context,
            runtime::host::{
                DataCall, DataOperation, DataResult, FieldChange, FieldDeclaration, Schema,
                TypeDeclaration, Value as HostValue, ValueType,
            },
            store::{Access, Id},
        };
        let mut f = Fixture::new();
        let root =
            crate::engine::tests::object_volume(&f.engine.services.store, "Lost data reply", false);
        let session =
            f.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let reserved = f.request(Request::InstanceReserve {
            restore_host_mounts: false,
            request: Id::generate().unwrap(),
            incarnation: f.engine.session_instances.incarnation(),
            session: session.id,
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
        });
        let instance = serde_json::from_value(reserved["id"].clone()).unwrap();
        let claimed = f.request(Request::InstanceClaim { instance });
        let context: Context = serde_json::from_value(claimed["context"].clone()).unwrap();
        let exchange = |f: &mut Fixture, request: &Request| {
            let mut client = f.client();
            ipc::write(&mut client, &encode_request(request).unwrap()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            f.turn();
            while !f.queue.pending.is_empty() {
                assert!(Instant::now() < deadline);
                f.turn();
                std::thread::yield_now();
            }
            let bytes = ipc::read(&mut client).unwrap();
            let archived = codec::access::<crate::engine::reply::ArchivedReply>(&bytes).unwrap();
            assert!(archived.matches_request(request));
            rkyv::api::low::deserialize::<Reply, rkyv::rancor::Error>(archived).unwrap()
        };
        let schema = DataCall {
            volume: *root.id.as_bytes(),
            operation: DataOperation::SchemaEstablish {
                entry_id: [5; 15],
                schema: Schema {
                    types: vec![TypeDeclaration {
                        id: [1; 16],
                        name: "Task".into(),
                        deprecated: false,
                        fields: vec![FieldDeclaration {
                            id: [2; 16],
                            name: "Title".into(),
                            value_type: ValueType::Text,
                            repeated: false,
                            deprecated: false,
                        }],
                    }],
                },
            },
        };
        let request = Request::Syscall {
            context: context.into(),
            operation: 1,
            mutation: 1,
            call: Box::new(schema.clone()),
        };
        let Reply::Syscall(outcome) = exchange(&mut f, &request) else { panic!("data completion") };
        assert_eq!(outcome.take(context, 1, 1, &schema), Ok(DataResult::SchemaEstablished));
        let call = DataCall {
            volume: *root.id.as_bytes(),
            operation: DataOperation::Create {
                entry_id: [5; 15],
                type_id: [1; 16],
                fields: vec![FieldChange {
                    field_id: [2; 16],
                    values: Some(vec![HostValue::Text("Exactly once".into())]),
                }],
            },
        };
        let request = Request::Syscall {
            context: context.into(),
            operation: 2,
            mutation: 2,
            call: Box::new(call),
        };
        let mut client = f.client();
        ipc::write(&mut client, &encode_request(&request).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f.queue.pending.iter().any(|pending| matches!(pending.state, State::Service { .. }))
        {
            assert!(Instant::now() < deadline);
            f.turn();
        }
        // Actual EOF after admission, before the next scheduling turn.
        drop(client);
        while !f.queue.pending.is_empty() || f.engine.service_executor.pending() {
            assert!(Instant::now() < deadline);
            f.turn();
            std::thread::yield_now();
        }
        assert!(f.engine.session_instances.owns_context(context));
        let page = f
            .engine
            .services
            .store
            .query_objects(
                &Access::owner(f.engine.services.store.account().id),
                crate::engine::tests::object_address(root.id),
                Id::from_bytes([1; 16]),
                None,
                8,
            )
            .unwrap();
        assert_eq!(page.objects.len(), 1);
        let Reply::Syscall(outcome) = exchange(&mut f, &request) else {
            panic!("duplicate rejection")
        };
        assert_eq!(outcome.status, playbit_api::abi::syscall::Error::StaleHandle as i32);
        assert!(outcome.result.is_none());
        let page = f
            .engine
            .services
            .store
            .query_objects(
                &Access::owner(f.engine.services.store.account().id),
                crate::engine::tests::object_address(root.id),
                Id::from_bytes([1; 16]),
                None,
                8,
            )
            .unwrap();
        assert_eq!(page.objects.len(), 1);
    }

    #[test]
    #[ignore = "isolates log copying, serde encoding and bounded output; no IPC or rendering"]
    fn benchmark_log_encoding_parts() {
        struct CountWrites {
            writes: usize,
            bytes: usize,
        }
        impl Write for CountWrites {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for (name, count, line) in [
            ("normal", 32, "x".repeat(128)),
            ("maximum", 256, "x".repeat(256)),
            ("maximum_escaped", 256, "\u{0001}".repeat(256)),
        ] {
            let batch = crate::engine::LogBatch {
                cursor: crate::engine::LogCursor {
                    engine: crate::store::Id::from_bytes([0x12; 16]),
                    instance: 1,
                    next: count,
                },
                lines: vec![line; count as usize],
                dropped: 0,
                reset: false,
            };
            let mut calls = CountWrites { writes: 0, bytes: 0 };
            serde_json::to_writer(&mut calls, &batch).unwrap();
            for part in ["copy", "vec_encoding", "bounded_encoding"] {
                let mut samples = Vec::with_capacity(1000);
                for _ in 0..1000 {
                    let start = Instant::now();
                    match part {
                        "copy" => {
                            std::hint::black_box(batch.lines.clone());
                        }
                        "vec_encoding" => {
                            std::hint::black_box(serde_json::to_vec(&batch).unwrap());
                        }
                        _ => {
                            std::hint::black_box(codec::encode(&batch, 512 * 1024).unwrap());
                        }
                    }
                    samples.push(start.elapsed().as_nanos());
                }
                samples.sort_unstable();
                crate::diagnostic!(
                    "{}",
                    json!({"benchmark":"log_encoding_parts", "workload":name, "part":part,
                    "samples":1000, "serde_write_calls":calls.writes, "encoded_bytes":calls.bytes,
                    "p50_ns":samples[499], "p99_ns":samples[989], "max_ns":samples[999]})
                );
            }
        }
    }

    #[test]
    fn change_wait_covers_rearm_races_restart_and_invalid_future_cursor() {
        let mut fixture = Fixture::new();
        let initial = fixture.engine.change_token();
        assert_eq!(
            fixture.request(Request::WaitChanges { after: None }),
            serde_json::to_value(initial).unwrap()
        );
        // A change between the state read and wait registration must reply
        // immediately. Multiple changes coalesce without retaining a log.
        fixture.engine.state_changed();
        fixture.engine.state_changed();
        let current = fixture.engine.change_token();
        assert_eq!(
            fixture.request(Request::WaitChanges { after: Some(initial) }),
            serde_json::to_value(current).unwrap()
        );
        let mut waiting = fixture.client();
        ipc::write(
            &mut waiting,
            &encode_request(&Request::WaitChanges { after: Some(current) }).unwrap(),
        )
        .unwrap();
        for _ in 0..8 {
            fixture.turn();
        }
        assert_eq!(fixture.queue.pending.len(), 1);
        let pending = &mut fixture.queue.pending[0];
        pending.deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(pending.timeout(Instant::now()), None);
        assert!(!pending.expired());
        assert!(std::mem::size_of::<Pending>() <= 1024);
        assert_eq!(std::mem::size_of::<crate::engine::ChangeToken>(), 24);
        waiting.set_nonblocking(true).unwrap();
        assert_eq!(waiting.read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);
        waiting.set_nonblocking(false).unwrap();
        fixture.engine.state_changed();
        for _ in 0..8 {
            fixture.turn();
        }
        let reply = ipc::read(&mut waiting).unwrap();
        assert!(reply.len() <= 256);
        assert_eq!(
            decode_reply::<crate::engine::ChangeToken>(&reply).unwrap(),
            fixture.engine.change_token()
        );
        let future = crate::engine::ChangeToken { sequence: u64::MAX, ..current };
        assert!(
            fixture.request(Request::WaitChanges { after: Some(future) })["error"]
                .as_str()
                .unwrap()
                .contains("ahead")
        );
        let mut restarted = Fixture::new();
        assert_ne!(restarted.engine.change_token().generation, initial.generation);
        assert_eq!(
            restarted.request(Request::WaitChanges { after: Some(current) }),
            serde_json::to_value(restarted.engine.change_token()).unwrap()
        );
    }

    #[test]
    fn change_waiters_are_bounded_cancelable_and_do_not_block_local_requests() {
        let mut fixture = Fixture::new();
        let current = fixture.engine.change_token();
        let request = encode_request(&Request::WaitChanges { after: Some(current) }).unwrap();
        let mut clients = Vec::new();
        for _ in 0..MAX_CHANGE_WAITERS {
            let mut client = fixture.client();
            ipc::write(&mut client, &request).unwrap();
            clients.push(client);
            for _ in 0..8 {
                fixture.turn();
            }
        }
        assert_eq!(fixture.queue.allocated(), MAX_CHANGE_WAITERS * 1024);
        assert!(
            fixture.request(Request::WaitChanges { after: Some(current) })["error"]
                .as_str()
                .unwrap()
                .contains("waiter limit")
        );
        for _ in 0..1000 {
            assert!(fixture.request(Request::Status)["account"]["id"].is_string());
        }
        assert_eq!(
            fixture.request(Request::LocalProtocol),
            json!({"protocol":ipc::LOCAL_PROTOCOL,"version":crate::VERSION})
        );
        // Reads do not advance the cursor or finish any wait.
        assert_eq!(fixture.engine.change_token(), current);
        assert_eq!(fixture.queue.pending.len(), MAX_CHANGE_WAITERS);
        drop(clients.pop());
        let deadline = Instant::now() + Duration::from_secs(2);
        while fixture.queue.pending.len() == MAX_CHANGE_WAITERS {
            assert!(Instant::now() < deadline, "disconnected waiter must release admission");
            fixture.queue.run_until(&mut fixture.engine, Some(deadline)).unwrap();
        }
        assert_eq!(fixture.queue.pending.len(), MAX_CHANGE_WAITERS - 1);
        let mut replacement = fixture.client();
        ipc::write(&mut replacement, &request).unwrap();
        clients.push(replacement);
        for _ in 0..8 {
            fixture.turn();
        }
        assert_eq!(fixture.queue.pending.len(), MAX_CHANGE_WAITERS);
        fixture.engine.state_changed();
        for _ in 0..8 {
            fixture.turn();
        }
        for mut client in clients {
            let reply = ipc::read(&mut client).unwrap();
            assert!(reply.len() <= 256);
            assert_eq!(
                decode_reply::<crate::engine::ChangeToken>(&reply).unwrap(),
                fixture.engine.change_token()
            );
        }
        assert_eq!(fixture.queue.allocated(), 0);
    }

    #[test]
    fn declarative_initialization_routes_once_and_reopens_without_seeding() {
        // Exercise serialized IPC admission and the actual engine dispatcher,
        // then repeat the identity to check the shared idempotent implementation.
        let mut fixture = Fixture::new();
        let volume = crate::store::Id::generate().unwrap();
        let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
            "../../../guests/todo/playbit-volumes.json"
        ))
        .unwrap()
        .for_role(crate::package::AutomaticVolumeRole::Session);
        for _ in 0..2 {
            let result = fixture.request(Request::InitializeRoot {
                volume,
                name: "Declared".into(),
                declaration: Box::new(declaration.clone()),
            });
            assert_eq!(result["id"], volume.to_string());
        }
        let Reply::ToolContext { context } = fixture.exchange(Request::ToolContextOpen {
            scope: crate::engine::service_call::ToolScope::Account,
        }) else {
            panic!("tool context")
        };
        let call = crate::runtime::host::DataCall {
            volume: *volume.as_bytes(),
            operation: crate::runtime::host::DataOperation::Query {
                entry_id: *declaration.databases[0].id.as_bytes(),
                type_id: [1; 16],
                after: None,
                limit: playbit_api::abi::syscall::MAX_PAGE_OBJECTS,
                projection: None,
            },
        };
        let Reply::Syscall(outcome) = fixture.exchange(Request::Syscall {
            context,
            operation: 1,
            mutation: 0,
            call: Box::new(call.clone()),
        }) else {
            panic!("query outcome")
        };
        let crate::runtime::host::DataResult::Page(page) =
            outcome.take(context, 1, 0, &call).unwrap()
        else {
            panic!("page")
        };
        assert_eq!(page.objects.len(), 1);
        let status = fixture.request(Request::Status);
        assert_eq!(status["volumes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn local_mutation_wakes_waiter_but_unchanged_snapshot_reads_do_not() {
        let mut fixture = Fixture::new();
        let current = fixture.engine.change_token();
        let mut waiting = fixture.client();
        ipc::write(
            &mut waiting,
            &encode_request(&Request::WaitChanges { after: Some(current) }).unwrap(),
        )
        .unwrap();
        for _ in 0..8 {
            fixture.turn();
        }
        let created = fixture.request(Request::CreateVolume {
            share_key_with_service: true,
            name: "Observed".into(),
        });
        assert!(created["id"].is_string());
        let token: crate::engine::ChangeToken =
            decode_reply(&ipc::read(&mut waiting).unwrap()).unwrap();
        assert_ne!(token, current);
        let status = fixture.request(Request::Status);
        assert_eq!(status["volumes"].as_array().unwrap().len(), 1);
        assert_eq!(fixture.engine.change_token(), token);
        let root = serde_json::from_value(created["id"].clone()).unwrap();
        let session =
            fixture.engine.create_session(crate::engine::tests::button_package(), root).unwrap();
        // Direct dispatch isolates snapshot notification from background build completion.
        let token = fixture.engine.change_token();
        for (request, count) in [
            (Request::AppVolumes { app: session.app.clone() }, 3),
            (Request::AutomaticVolumes { session: session.id }, 6),
        ] {
            let Reply::AutomaticVolumes { slots } = fixture.engine.dispatch_reply(request).unwrap()
            else {
                panic!("automatic volume snapshot");
            };
            assert_eq!(slots.len(), count);
            assert_eq!(
                fixture.engine.change_token(),
                token,
                "snapshot reads do not wake observers"
            );
        }
        assert!(
            fixture
                .engine
                .dispatch_reply(Request::AutomaticVolumes {
                    session: crate::identifier::encode(&[0; 32]),
                })
                .is_err()
        );
        assert_eq!(fixture.engine.change_token(), token);
    }

    #[test]
    fn size_completion_delivers_its_reply_without_invalidating_observers() {
        // Observe real native Size completion in a session-free owner. Collect
        // only preparation replies, excluding unrelated background notifications.
        let mut fixture = Fixture::new();
        let volume = fixture.engine.services.store.create_volume("Measured", false).unwrap();
        for entry in [None, Some(crate::store::EntryId::generate().unwrap())] {
            fixture.engine.completion.drain().unwrap();
            let (server, _client) = UnixStream::pair().unwrap();
            let index = fixture.inject(server);
            let request = Request::PayloadSize { session: None, volume: volume.id, entry };
            fixture.queue.pending[index].state = State::Ready {
                body: encode_request(&request).unwrap(),
                route: Route { op: Op::PayloadSize },
                owned_bytes: 0,
            };
            let token = fixture.engine.change_token();
            let mut budget = TURN_BYTES;
            fixture.queue.dispatch_ready(&mut fixture.engine, &mut budget).unwrap();
            assert!(fixture.queue.bundle.is_some());
            let mut events = Events::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while fixture.queue.bundle.is_some() {
                let remaining = deadline.checked_duration_since(Instant::now()).unwrap();
                events.clear();
                fixture
                    .queue
                    .poller
                    .modify(&fixture.queue.completion, Event::readable(COMPLETION))
                    .unwrap();
                fixture.queue.poller.wait(&mut events, Some(remaining)).unwrap();
                fixture.engine.completion.drain().unwrap();
                fixture.queue.finish_completions(&mut fixture.engine).unwrap();
            }
            let State::Writing { body, .. } = &fixture.queue.pending[index].state else {
                panic!("Size completion owns its serialized reply");
            };
            let reply = codec::access::<crate::engine::reply::ArchivedReply>(body).unwrap();
            if entry.is_none() {
                assert!(reply.matches_request(&request));
                assert_eq!(decode_reply::<Value>(body).unwrap()["bytes"], 0);
            } else {
                assert!(matches!(reply, crate::engine::reply::ArchivedReply::Error { .. }));
            }
            assert_eq!(fixture.engine.change_token(), token);
            fixture.queue.close(index, true).unwrap();
        }
    }

    #[test]
    fn volume_copy_uses_the_single_local_preparation_slot() {
        // Retain a completed-or-running task until collection: another copy
        // waits for that slot, while unrelated local creation stays admissible.
        let mut fixture = Fixture::new();
        let volume = fixture.engine.services.store.create_volume("Original", false).unwrap();
        fixture.queue.bundle = Some(
            bundle::Task::start(
                &fixture.engine,
                usize::MAX,
                Request::DuplicateVolume { volume: volume.id },
            )
            .unwrap(),
        );
        let (server, _client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        fixture.queue.pending[index].state = State::Ready {
            body: codec::Bytes::new(),
            route: Route { op: Op::DuplicateVolume },
            owned_bytes: 0,
        };
        assert_eq!(fixture.queue.dispatchable(&fixture.engine), None);
        for op in [Op::DuplicateEntry, Op::PayloadSize, Op::CreateVolume] {
            let State::Ready { route, .. } = &mut fixture.queue.pending[index].state else {
                panic!("queued request remains ready");
            };
            route.op = op;
            if matches!(op, Op::DuplicateEntry | Op::PayloadSize) {
                assert_eq!(fixture.queue.dispatchable(&fixture.engine), None);
            }
        }
        assert_eq!(fixture.queue.dispatchable(&fixture.engine), Some(index));
        // Keep the fixture directory alive until accepted copying is durable.
        drop(fixture.queue.bundle.take());
    }

    #[test]
    fn local_preparation_cancels_optional_size_before_claiming_its_slot() {
        // Hold a real size worker at volume admission, queue app preparation,
        // then release the gate and verify cancellation frees only that read slot.
        use std::sync::mpsc;
        let mut fixture = Fixture::new();
        let volume = fixture.engine.services.store.create_volume("Measured", true).unwrap();
        let store = fixture.engine.services.store.clone();
        let (started, waiting) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            store
                .with_replica(volume.id, crate::store::DatabaseId::Index, |_| {
                    started.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(())
                })
                .unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        fixture.queue.bundle = Some(
            bundle::Task::start(
                &fixture.engine,
                usize::MAX,
                Request::PayloadSize { session: None, volume: volume.id, entry: None },
            )
            .unwrap(),
        );
        let (server, _client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        fixture.queue.pending[index].state = State::Ready {
            body: codec::Bytes::new(),
            route: Route { op: Op::PayloadSize },
            owned_bytes: 0,
        };
        assert_eq!(fixture.queue.dispatchable(&fixture.engine), None);
        assert!(fixture.queue.bundle.as_ref().unwrap().take().is_none());
        let (server, _preparation_client) = UnixStream::pair().unwrap();
        let preparation = fixture.inject(server);
        fixture.queue.pending[preparation].state = State::Ready {
            body: codec::encode(
                &Request::PrepareApp { bundle: fixture._directory.path().join("app") },
                codec::MAX_ARCHIVE,
            )
            .unwrap(),
            route: Route { op: Op::PrepareApp },
            owned_bytes: 0,
        };
        assert_eq!(fixture.queue.dispatchable(&fixture.engine), None);
        release.send(()).unwrap();
        holder.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let response = loop {
            if let Some(response) = fixture.queue.bundle.as_ref().unwrap().take() {
                break response;
            }
            assert!(Instant::now() < deadline, "cancelled size exits its bounded work");
            std::thread::yield_now();
        };
        assert!(matches!(response, bundle::Preparation::Reply(reply)
            if matches!(*reply, Reply::Error { ref error } if error.contains("size calculation cancelled"))));
        drop(fixture.queue.bundle.take());
        assert_eq!(
            fixture.queue.dispatchable(&fixture.engine),
            Some(preparation),
            "queued app preparation precedes an earlier optional size"
        );
        fixture.queue.close(preparation, true).unwrap();
        assert_eq!(fixture.queue.dispatchable(&fixture.engine), Some(index));
    }

    #[test]
    fn waiting_sync_does_not_hold_input_status_or_cancellation() {
        // Hold the Remote owner until local copies, later creation, import and
        // guest input finish. Reopen afterward to check acknowledged mutations.
        use crate::engine::background::tests::{diagnostic_ticket, notify, transfer};
        let mut fixture = Fixture::new();
        let root = fixture.engine.services.store.create_volume("Root", true).unwrap();
        let access = crate::store::Access::owner(fixture.engine.services.store.account().id);
        let original = fixture
            .engine
            .services
            .store
            .create_database(
                &access,
                root.id,
                "Settings",
                crate::store::volume_index::EntryKind::KvDb {},
            )
            .unwrap();
        let address =
            crate::store::volume_index::EntryAddress { volume: root.id, entry: original.id };
        fixture.engine.services.store.kv_put(&access, address, b"a", b"anne").unwrap();
        let session =
            fixture.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let mut owners = crate::engine::local_owner_test_support::LocalApps::default();
        let instance = owners.open(&mut fixture.engine, &session.id).unwrap();
        owners.settle();
        let (send, remote) = transfer(&mut fixture.engine);
        let ticket = diagnostic_ticket(&mut fixture.engine);
        let (server, mut sync_client) = UnixStream::pair().unwrap();
        sync_client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let index = fixture.inject(server);
        fixture.queue.pending[index].state = State::NetworkRequest { ticket, owned_bytes: 0 };
        assert_eq!(fixture.queue.allocated(), 1024);
        assert!(fixture.request(Request::Status)["account"]["id"].is_string());
        let Reply::Diagnostic(crate::engine::reply::DiagnosticReply::PayloadSize {
            volume,
            entry,
            bytes,
        }) = fixture.exchange(Request::PayloadSize {
            session: None,
            volume: root.id,
            entry: Some(original.id),
        })
        else {
            panic!("local size completes while the remote transfer is held");
        };
        assert_eq!((volume, entry, bytes), (root.id, Some(original.id), 5));
        let Reply::Diagnostic(crate::engine::reply::DiagnosticReply::VolumeDuplicated {
            source,
            volume: copy,
        }) = fixture.exchange(Request::DuplicateVolume { volume: root.id })
        else {
            panic!("local duplication completes while the remote transfer is held");
        };
        assert_eq!(source, root.id);
        assert_eq!(copy.name, "Root Copy");
        assert_ne!(copy.id, root.id);
        assert_eq!(fixture.engine.services.store.volume(copy.id).unwrap(), copy);
        let Reply::Diagnostic(crate::engine::reply::DiagnosticReply::EntryDuplicated {
            source,
            entry: copied_entry,
        }) = fixture.exchange(Request::DuplicateEntry { address })
        else {
            panic!("local entry duplication completes while the remote transfer is held");
        };
        assert_eq!(source, address);
        assert_eq!(copied_entry.info.name, "Settings Copy");
        assert_ne!(copied_entry.id, address.entry);
        let Reply::Volume(created) = fixture.exchange(Request::CreateVolume {
            name: "Created after duplicate".into(),
            share_key_with_service: false,
        }) else {
            panic!("subsequent local creation completes while the remote transfer is held");
        };
        assert_eq!(fixture.engine.services.store.volume(created.id).unwrap(), created);
        assert!(fixture.engine.background.is_some());
        assert!(!fixture.engine.network_request_ready());
        let bundle = fixture._directory.path().join("local.pbapp");
        let package = crate::package::signature::testing::signed(
            &fixture.engine.services.store,
            crate::engine::tests::button_package(),
        )
        .unwrap();
        std::fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
        let Reply::Developer(crate::engine::reply::DeveloperReply::Session(local)) =
            fixture.exchange(Request::CreateSession { bundle, root: None, user: None })
        else {
            panic!("local import completes while the remote transfer is held");
        };
        assert_ne!(local.root_volume, root.id);
        assert!(fixture.queue.bundle.is_none());
        assert!(!fixture.engine.network_request_ready());
        assert!(matches!(
            fixture.exchange(Request::PrepareSession { session: local.id }),
            Reply::SessionPrepared(_)
        ));
        owners.input(instance, crate::engine::input_event(2, 1, vec![]).unwrap()).unwrap();
        assert_eq!(owners.apps.logs(instance).unwrap(), ["turn", "turn"]);
        assert!(fixture.engine.background.is_some());
        assert_eq!(fixture.request(Request::TransferStatus)["transfer"]["kind"], "running");
        fixture.request(Request::CancelTransfer);
        assert_eq!(
            fixture.engine.transfer_result.kind,
            crate::engine::reply::TransferKind::Canceling
        );
        assert!(fixture.engine.background.is_some(), "cancellation cannot release running work");
        send.send((
            remote,
            Ok(crate::engine::background::NetworkResult::Synchronized),
            Default::default(),
        ))
        .unwrap();
        notify(&fixture.engine);
        for _ in 0..4 {
            fixture.turn();
        }
        let response: Value = decode_reply(&ipc::read(&mut sync_client).unwrap()).unwrap();
        assert_eq!(response["error"], "Paused. Resume when you’re ready");
        assert!(fixture.engine.network_request_ready());
        assert_eq!(owners.apps.logs(instance).unwrap().len(), 2);
        owners.quit(&mut fixture.engine, instance).unwrap();
        let profile = fixture._directory.path().join("profile");
        drop(owners);
        drop(fixture.queue);
        drop(fixture.engine);
        let reopened = crate::store::LocalStore::open(profile).unwrap();
        assert_eq!(reopened.volume(copy.id).unwrap(), copy);
        assert_eq!(reopened.volume(created.id).unwrap(), created);
        let copied_address =
            crate::store::volume_index::EntryAddress { volume: root.id, entry: copied_entry.id };
        assert_eq!(
            reopened.kv_read(&access, copied_address, b"a", 0, 64, None).unwrap().bytes,
            b"anne"
        );
    }

    #[test]
    fn foreign_signed_import_waits_for_trust_and_disconnect_releases_its_reservation() {
        // A real signed foreign package stays queued while AP ownership is held;
        // disconnect must release its original credit without creating a session.
        use crate::engine::background::tests::{notify, transfer};
        let mut fixture = Fixture::new();
        let package = crate::package::signature::testing::authenticate(
            &crate::engine::tests::button_package(),
            crate::store::Id::from_bytes([41; 16]),
            crate::store::now_unix_secs(),
        )
        .unwrap();
        let bundle = fixture._directory.path().join("foreign.pbapp");
        std::fs::write(&bundle, package.to_bytes().unwrap()).unwrap();
        let (send, remote) = transfer(&mut fixture.engine);
        let mut client = fixture.client();
        ipc::write(
            &mut client,
            &encode_request(&Request::CreateSession {
                bundle: bundle.clone(),
                root: None,
                user: None,
            })
            .unwrap(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fixture
            .queue
            .pending
            .iter()
            .any(|pending| matches!(pending.state, State::PreparedNetwork { .. }))
        {
            assert!(Instant::now() < deadline, "foreign import reaches bounded trust handoff");
            fixture.turn();
            std::thread::yield_now();
        }
        let pending = fixture
            .queue
            .pending
            .iter()
            .find(|pending| matches!(pending.state, State::PreparedNetwork { .. }))
            .unwrap();
        let State::PreparedNetwork { request, owned_bytes } = &pending.state else {
            unreachable!()
        };
        assert!(
            matches!(request.as_ref(), Request::CreateSession { bundle: actual, root: None, user: None } if actual == &bundle)
        );
        assert!(*owned_bytes >= std::mem::size_of::<Request>());
        assert_eq!(pending.allocated(), 1024 + owned_bytes);
        assert!(fixture.engine.services.store.sessions().unwrap().is_empty());
        assert!(!fixture.engine.network_request_ready());
        drop(client);
        for _ in 0..4 {
            fixture.turn();
        }
        assert!(fixture.queue.pending.is_empty());
        assert_eq!(fixture.queue.allocated(), 0);
        send.send((
            remote,
            Ok(crate::engine::background::NetworkResult::Synchronized),
            Default::default(),
        ))
        .unwrap();
        notify(&fixture.engine);
        for _ in 0..4 {
            fixture.turn();
        }
        assert!(fixture.engine.services.store.sessions().unwrap().is_empty());
    }

    #[test]
    fn abandoned_sync_releases_its_result_but_not_the_running_operation() {
        use crate::engine::background::tests::{diagnostic_ticket, notify, transfer};
        let mut fixture = Fixture::new();
        let (send, remote) = transfer(&mut fixture.engine);
        let ticket = diagnostic_ticket(&mut fixture.engine);
        let (server, client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        fixture.queue.pending[index].state = State::NetworkRequest { ticket, owned_bytes: 0 };
        drop(client);
        for _ in 0..4 {
            fixture.turn();
        }
        assert!(fixture.engine.network_request_waiter.is_none());
        assert!(fixture.engine.background.is_some());
        assert!(!fixture.engine.network_request_ready());
        assert_eq!(fixture.queue.allocated(), 0);
        assert!(fixture.request(Request::Status)["account"]["id"].is_string());
        send.send((remote, Err("one completed operation".into()), Default::default())).unwrap();
        notify(&fixture.engine);
        fixture.turn();
        assert!(fixture.engine.network_request_ready());
        assert_eq!(
            fixture.engine.transfer_result.message.as_deref().unwrap_or(""),
            "one completed operation"
        );
    }

    #[test]
    fn sync_rpc_dispatches_and_returns_a_terminal_failure() {
        let mut fixture = Fixture::new();
        fixture.engine.remote.as_mut().unwrap().set_test_transport(
            crate::network::ap::ClientConfig::platform("127.0.0.1:9".parse().unwrap(), "localhost"),
        );
        let mut client = fixture.client();
        ipc::write(&mut client, &encode_request(&Request::Sync).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline);
            fixture.turn();
            if fixture.engine.transfer_result.kind == crate::engine::reply::TransferKind::Failed
                && fixture.engine.network_request_waiter.is_none()
            {
                break;
            }
            std::thread::yield_now();
        }
        for _ in 0..4 {
            fixture.turn();
        }
        let response: Value = decode_reply(&ipc::read(&mut client).unwrap()).unwrap();
        assert!(response["error"].is_string(), "{response}");
        assert!(response.get("availability").is_none());
        assert!(fixture.engine.network_request_ready());
    }

    #[test]
    fn channel_commands_require_a_registered_instance_and_never_tool_authority() {
        use crate::{
            engine::service_call::{Context, ToolScope},
            runtime::host::{DataCall, DataOperation},
        };
        let mut fixture = Fixture::new();
        let root = fixture.engine.services.store.create_volume("Messages", false).unwrap();
        let session =
            fixture.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let reply =
            fixture.request(Request::ToolContextOpen { scope: ToolScope::Session(session.id) });
        let context: Context = serde_json::from_value(reply["context"].clone()).unwrap();
        let reply = fixture.request(Request::Syscall {
            context,
            operation: 1,
            mutation: 1,
            call: Box::new(DataCall {
                volume: [0; 16],
                operation: DataOperation::ChannelCommand {
                    instance: [1; 16],
                    name: "test".into(),
                    action: 1,
                    data: vec![],
                },
            }),
        });
        assert_eq!(reply["status"], playbit_api::abi::syscall::Error::Unsupported as i32);
        assert!(fixture.request(Request::Status)["account"]["id"].is_string());
    }

    #[test]
    fn partial_frame_does_not_block_another_client() {
        let mut fixture = Fixture::new();
        let mut partial = fixture.client();
        partial.write_all(&[1]).unwrap();
        let response = fixture.request(Request::Status);
        assert!(response["account"]["id"].is_string());
        assert_eq!(fixture.queue.pending.len(), 1);
    }

    #[test]
    fn different_build_is_rejected_before_mutation() {
        let mut fixture = Fixture::new();
        let mutation = encode_request(&Request::CreateVolume {
            share_key_with_service: true,
            name: "Must not exist".into(),
        })
        .unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        fixture.inject(server);
        let mut frame = Vec::new();
        codec::write(&mut frame, &mutation).unwrap();
        frame[12] ^= 1; // Same framing revision and valid archive, different build.
        client.write_all(&frame).unwrap();
        for _ in 0..4 {
            fixture.turn();
        }
        assert!(fixture.queue.pending.is_empty());
        assert!(fixture.engine.services.store.volumes().unwrap().is_empty());
    }

    #[test]
    fn disconnected_expired_and_pipelined_requests_never_mutate() {
        let mut fixture = Fixture::new();
        let mutation = encode_request(&Request::CreateVolume {
            share_key_with_service: true,
            name: "Must not exist".into(),
        })
        .unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        fixture.inject(server);
        ipc::write(&mut client, &mutation).unwrap();
        // Force EOF even if a parallel process-spawning test briefly inherits
        // this descriptor before exec closes it.
        client.shutdown(std::net::Shutdown::Both).unwrap();
        drop(client);
        fixture.turn();
        assert!(fixture.engine.services.store.volumes().unwrap().is_empty());
        let (server, mut client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        ipc::write(&mut client, &mutation).unwrap();
        fixture.queue.pending[index].deadline = Instant::now();
        for _ in 0..4 {
            fixture.turn();
        }
        let response: Value = decode_reply(&ipc::read(&mut client).unwrap()).unwrap();
        assert!(response["error"].as_str().unwrap().contains("did not start"));
        let mut client = fixture.client();
        let mut bytes = vec![];
        for _ in 0..2 {
            codec::write(&mut bytes, &mutation).unwrap();
        }
        client.write_all(&bytes).unwrap();
        for _ in 0..8 {
            fixture.turn();
        }
        let response: Value = decode_reply(&ipc::read(&mut client).unwrap()).unwrap();
        assert!(response["error"].as_str().unwrap().contains("one request"));
        assert!(fixture.engine.services.store.volumes().unwrap().is_empty());
    }

    #[test]
    fn admission_and_allocated_bytes_are_bounded() {
        let mut fixture = Fixture::new();
        let mut held = Vec::new();
        for _ in 0..MAX_PENDING {
            let (server, client) = UnixStream::pair().unwrap();
            fixture.inject(server);
            held.push(client);
        }
        let mut rejected = fixture.client();
        fixture.turn();
        let response: Value = decode_reply(&ipc::read(&mut rejected).unwrap()).unwrap();
        assert!(response["error"].as_str().unwrap().contains("queue is full"));
        assert_eq!(fixture.queue.pending.len(), MAX_PENDING);
        while !fixture.queue.pending.is_empty() {
            fixture.queue.close(0, false).unwrap();
        }
        // Header reservation charges the whole allocated body, even before the
        // client has supplied it. A fourth large allocation is rejected.
        let mut large = Vec::new();
        for _ in 0..4 {
            let mut client = fixture.client();
            client.write_all(&((MAX_MESSAGE - 256) as u32).to_le_bytes()).unwrap();
            client.write_all(&codec::PREFIX).unwrap();
            large.push(client);
            for _ in 0..8 {
                fixture.turn();
            }
        }
        let response: Value = decode_reply(&ipc::read(&mut large[3]).unwrap()).unwrap();
        assert!(response["error"].as_str().unwrap().contains("byte limit"));
        assert!(fixture.queue.allocated() <= MAX_BUFFERED);
        for pending in &mut fixture.queue.pending {
            pending.deadline = Instant::now();
        }
        for _ in 0..8 {
            fixture.turn();
        }
        assert_eq!(fixture.queue.allocated(), 0);
    }

    #[test]
    fn slow_reply_and_partial_writes_do_not_repeat_dispatch_or_block_status() {
        let mut fixture = Fixture::new();
        let (server, _slow_client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        fixture.queue.pending[index]
            .reply(&Reply::Availability { availability: "x".repeat(MAX_MESSAGE / 2) }, MAX_MESSAGE)
            .unwrap();
        fixture.queue.rearm(index).unwrap();
        let response = fixture.request(Request::Status);
        assert!(response["account"]["id"].is_string());
        assert!(matches!(fixture.queue.pending[0].state, State::Writing { .. }));
        let response = fixture.request(Request::CreateVolume {
            share_key_with_service: true,
            name: "Only once".into(),
        });
        assert!(response["id"].is_string());
        for _ in 0..8 {
            fixture.turn();
        }
        assert_eq!(fixture.engine.services.store.volumes().unwrap().len(), 1);
        assert!(fixture.queue.allocated() <= MAX_BUFFERED);
    }

    #[test]
    #[allow(
        unsafe_code,
        reason = "Native API calls borrow initialized buffers or descriptors whose owners outlive the call and registration."
    )]
    fn byte_quantum_rotates_across_continuously_readable_clients() {
        let mut fixture = Fixture::new();
        let mut clients = Vec::new();
        for _ in 0..8 {
            use std::os::fd::AsRawFd;
            let (server, mut client) = UnixStream::pair().unwrap();
            let bytes: libc::c_int = 256 * 1024;
            for (socket, option) in [(&server, libc::SO_RCVBUF), (&client, libc::SO_SNDBUF)] {
                // Buffer enough fixture bytes before driving the owner thread.
                assert_eq!(
                    unsafe {
                        libc::setsockopt(
                            socket.as_raw_fd(),
                            libc::SOL_SOCKET,
                            option,
                            std::ptr::addr_of!(bytes).cast(),
                            std::mem::size_of_val(&bytes) as libc::socklen_t,
                        )
                    },
                    0
                );
            }
            fixture.inject(server);
            client.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            client.write_all(&(512_u32 * 1024).to_le_bytes()).unwrap();
            client.write_all(&codec::PREFIX).unwrap();
            client.write_all(&vec![b' '; 80 * 1024]).unwrap();
            clients.push(client);
        }
        fixture.turn();
        fixture.turn();
        let mut read = 0;
        for pending in &fixture.queue.pending {
            let State::Reading { read: count, .. } = &pending.state else {
                panic!("partial body must remain pending");
            };
            assert!(*count > 0, "every ready client gets a bounded turn");
            read += count;
        }
        assert!(read <= 2 * TURN_BYTES);
        assert!(fixture.queue.allocated() <= MAX_BUFFERED);
    }

    #[test]
    fn oversized_frames_and_response_growth_are_rejected_before_unbounded_allocation() {
        let mut fixture = Fixture::new();
        let mut client = fixture.client();
        client.write_all(&((MAX_MESSAGE + 1) as u32).to_le_bytes()).unwrap();
        for _ in 0..8 {
            fixture.turn();
        }
        let response: Value = decode_reply(&ipc::read(&mut client).unwrap()).unwrap();
        assert!(response["error"].as_str().unwrap().contains("length is invalid"));
        let large = Reply::Availability { availability: "x".repeat(1025) };
        assert!(codec::encode(&large, 1024).is_err());
        let small = Reply::Availability { availability: "x".repeat(512) };
        let archive = codec::encode(&small, 1024).unwrap();
        assert!(archive.capacity() <= 1024);
    }

    #[test]
    fn registration_tokens_are_not_reused_and_idle_wait_needs_an_event() {
        let mut fixture = Fixture::new();
        let (server, client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        let previous = fixture.queue.pending[index].token;
        fixture.queue.close(index, false).unwrap();
        drop(client);
        let (server, _client) = UnixStream::pair().unwrap();
        let index = fixture.inject(server);
        assert!(fixture.queue.pending[index].token > previous);
        fixture.queue.close(index, false).unwrap();
        let poller = std::sync::Arc::new(Poller::new().unwrap());
        let wake = poller.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            wake.wait(&mut Events::new(), None).unwrap();
            send.send(()).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(25)).is_err());
        poller.notify().unwrap();
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        thread.join().unwrap();
    }
}
