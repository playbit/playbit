//! One profile owner drains bounded producer queues. It records raw events and
//! loss without reconstructing spans, parentage or causal completeness.
use crate::{
    control::{self, Command, Status},
    descriptors,
    format::{self, BlockKind, Envelope},
    log::{Append, Log, Ownership, Preparation},
    notify, process, queue,
    region::{self, Region},
    source,
};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
const REGISTER_MAGIC: &[u8; 8] = b"PBTREG02";
const ACK: &[u8; 8] = b"PBTACK02";
const PENDING: usize = 8;
const EVENT_QUANTUM: usize = 256;
// Varint widths follow the bounded catalogs: u32 scope/pid, two-byte counts
// and label lengths, one-byte name lengths/kinds, and arbitrary u64 source kind.
const NATIVE_CATALOG_BYTES: usize =
    format::BLOCK_HEADER_BYTES + 10 + 5 + 2 + region::MAX_NAMES * (1 + region::MAX_NAME_BYTES);
const APP_CATALOG_BYTES: usize = format::BLOCK_HEADER_BYTES
    + 5
    + 1
    + 64
    + 16
    + 2
    + format::APP_LABEL_BYTES
    + 2
    + region::APP_NAMES * (1 + 1 + region::MAX_NAME_BYTES);
const TURN_METADATA_BYTES: usize = queue::MAX_SOURCES
    * (NATIVE_CATALOG_BYTES
        + region::APPS * APP_CATALOG_BYTES
        + 2 * (region::APPS + 1) * (format::BLOCK_HEADER_BYTES + 30));
const _: () = assert!(
    region::MAX_NAME_BYTES < 128
        && region::MAX_NAMES < 16384
        && region::APP_NAMES < 16384
        && format::APP_LABEL_BYTES < 16384
);
const _: () = assert!(
    TURN_METADATA_BYTES + EVENT_QUANTUM * (format::BLOCK_HEADER_BYTES + queue::EVENT_MAX_BYTES)
        <= crate::log::TURN_BYTES as usize
);
// The old eight-MiB aggregate allocation ceiling now includes each admitted
// guest's SDK ring. The smaller recorder has no model, compression scratch or
// causal coverage state. Its 1,280-KiB allowance includes both 128-KiB workers,
// four maximum tail frames, one tail copy, codecs, catalogs and control state.
const COLLECTOR_BYTES: usize = 1280 * 1024;
const SOURCE_BYTES: usize = region::MAX_REGION_BYTES
    + source::PRIVATE_BYTES
    + region::APPS
        * (playbit_api::abi::trace::BUFFER_BYTES + std::mem::size_of::<crate::AppScope>());
const _: () = assert!(queue::MAX_SOURCES * SOURCE_BYTES + COLLECTOR_BYTES <= 8 * 1024 * 1024);
static ATTACHING: AtomicBool = AtomicBool::new(false);
pub struct Recorder {
    stop: UnixStream,
    thread: Option<JoinHandle<io::Result<()>>>,
    join_on_drop: bool,
}
struct Attached {
    region: Arc<Region>,
    notification: UnixStream,
    watch: Option<process::Watch>,
    disconnected: bool,
    dead: bool,
    defined: bool,
    activity: u64,
    floor: u64,
    apps: [u32; region::APPS],
    loss: u64,
    app_loss: [u64; region::APPS],
    app_floor: [u64; region::APPS],
    app_cleared: [bool; region::APPS],
}
impl Attached {
    fn new(region: Arc<Region>, notification: UnixStream, watch: Option<process::Watch>) -> Self {
        Self {
            region,
            notification,
            watch,
            disconnected: false,
            dead: false,
            defined: false,
            activity: 0,
            floor: 1,
            apps: [0; region::APPS],
            loss: 0,
            app_loss: [0; region::APPS],
            app_floor: [0; region::APPS],
            app_cleared: [false; region::APPS],
        }
    }
}
struct Collector {
    profile: PathBuf,
    directory: PathBuf,
    small: bool,
    default: bool,
    closing: bool,
    failed: bool,
    cursor: usize,
    log: Option<Log>,
    tail: Option<crate::tail::Service>,
    control: control::Server,
    sources: Vec<Option<Attached>>,
    pending: [Option<UnixStream>; PENDING],
    listener: Option<UnixListener>,
    stop: UnixStream,
    sequence: u64,
    encoded: Vec<u8>,
    payload: Vec<u8>,
    event: [u8; queue::EVENT_MAX_BYTES],
    revision: u64,
    refused: u64,
}
fn format_error(_: format::DecodeError) -> io::Error {
    io::Error::other("trace encoding or decoding failed")
}
impl Recorder {
    pub fn for_profile(profile: &Path, directory: &Path) -> Option<Self> {
        match Self::spawn(profile, directory, cfg!(target_os = "ios"), false) {
            Ok((recorder, _)) => Some(recorder),
            Err(e) => {
                crate::diagnostic!("Playbit tracing unavailable: {e}");
                None
            }
        }
    }
    pub fn start(profile: &Path, directory: &Path, small: bool) -> io::Result<Self> {
        let (mut recorder, ready) = Self::spawn(profile, directory, small, true)?;
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        recorder.join_on_drop = true;
        Ok(recorder)
    }
    fn spawn(
        profile: &Path,
        directory: &Path,
        small: bool,
        requested: bool,
    ) -> io::Result<(Self, std::sync::mpsc::Receiver<Result<(), String>>)> {
        let local =
            source::SOURCE.get().ok_or_else(|| io::Error::other("trace bootstrap required"))?;
        local.bind_profile(profile)?;
        let (stop, receiver) = notify::pair()?;
        let (sender, ready) = std::sync::mpsc::sync_channel(1);
        let (profile, directory) = (profile.to_owned(), directory.to_owned());
        let thread = thread::Builder::new()
            .name("playbit-trace".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let result = record(profile, directory, small, requested, local, receiver, &sender);
                if let Err(e) = &result {
                    let _ = sender.try_send(Err(e.to_string()));
                    crate::diagnostic!("Playbit trace recording incomplete: {e}");
                }
                result
            })?;
        Ok((Self { stop, thread: Some(thread), join_on_drop: false }, ready))
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = notify::send(self.stop.as_raw_fd());
        if self.join_on_drop
            && let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            crate::diagnostic!("Playbit trace collector failed");
        }
    }
}
fn prepare_destination(profile: &Path, directory: &Path) -> io::Result<Ownership> {
    crate::profile_paths::prepare_control(profile)?;
    let relative = directory
        .strip_prefix(profile)
        .map_err(|_| io::Error::other("trace directory must belong to profile"))?;
    let mut parent = profile.to_owned();
    if let Some(path) = relative.parent() {
        for component in path.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(io::ErrorKind::InvalidInput.into());
            };
            parent.push(name);
            crate::profile_paths::private_directory(&parent)?;
        }
    }
    process::collector_priority()?;
    let ownership = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join(crate::profile_paths::LOCK))?;
    crate::log::private_file(&ownership)?;
    ownership.try_lock().map_err(io::Error::other)?;
    Ok(Ownership(ownership))
}
fn bind_registration(profile: &Path) -> io::Result<Option<UnixListener>> {
    let endpoint = profile.join(crate::profile_paths::REGISTER);
    if cfg!(target_os = "ios") {
        Ok(None)
    } else {
        match fs::remove_file(&endpoint) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&endpoint)?;
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Some(listener))
    }
}

fn record(
    profile: PathBuf,
    directory: PathBuf,
    small: bool,
    requested: bool,
    local: &source::Source,
    stop: UnixStream,
    ready: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> io::Result<()> {
    let _ownership = prepare_destination(&profile, &directory)?;
    let endpoint = profile.join(crate::profile_paths::REGISTER);
    let listener = bind_registration(&profile)?;
    let control = control::Server::bind(&profile)?;
    let (producer, notification) = notify::pair()?;
    local.attach(producer)?;
    let mut sources: Vec<_> = (0..queue::MAX_SOURCES).map(|_| None).collect();
    sources[0] = Some(Attached::new(local.region.clone(), notification, None));
    let mut collector = Collector {
        profile,
        directory,
        small,
        default: false,
        closing: false,
        failed: false,
        cursor: 0,
        log: None,
        tail: None,
        control,
        sources,
        pending: std::array::from_fn(|_| None),
        listener,
        stop,
        sequence: 0,
        encoded: Vec::with_capacity(format::MAX_BLOCK_BYTES),
        payload: Vec::with_capacity(format::MAX_PAYLOAD_BYTES),
        event: [0; queue::EVENT_MAX_BYTES],
        revision: 1,
        refused: 0,
    };
    if requested {
        collector.open_capture()?;
        collector.set_default(true)?;
    } else {
        collector.log = Log::retained(&collector.directory, small, crate::clock_ns())?;
        collector.start_tail()?;
    }
    let _ = ready.try_send(Ok(()));
    let result = collector.run();
    drop(collector);
    if !cfg!(target_os = "ios") {
        let _ = fs::remove_file(endpoint);
    }
    result
}
impl Drop for Collector {
    fn drop(&mut self) {
        for source in self.sources.iter().flatten() {
            let h = source.region.header();
            h.recording.store(0, Ordering::Release);
            for app in &h.apps {
                app.generation.store(0, Ordering::Release);
            }
        }
        if let Some(log) = &mut self.log {
            log.close();
        }
    }
}
impl Collector {
    fn start_tail(&mut self) -> io::Result<()> {
        if self.tail.is_none()
            && let Some(log) = &mut self.log
        {
            self.tail = Some(crate::tail::Service::start(
                &self.profile,
                log.reader(),
                log.take_notification()?,
            )?);
        }
        Ok(())
    }
    fn open_capture(&mut self) -> io::Result<()> {
        self.closing = false;
        self.failed = false;
        if self.log.is_none() {
            self.log = Some(Log::create(&self.directory, self.small, crate::clock_ns())?);
        }
        let log = self.log.as_mut().expect("capture created");
        if log.is_stopped() {
            log.begin_epoch(crate::clock_ns())?;
            self.sequence = 0;
            for source in self.sources.iter_mut().flatten() {
                source.defined = false;
                source.apps = [0; region::APPS];
                source.floor = source.region.header().generation.load(Ordering::Acquire);
            }
            self.payload.clear();
            format::integer(1, &mut self.payload).map_err(format_error)?;
            self.append(BlockKind::Epoch, [0, 0])?;
        }
        self.start_tail()
    }
    fn next_generation(region: &Region) -> io::Result<u64> {
        let old = region
            .header()
            .generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
            .map_err(|_| io::Error::other("trace generation exhausted"))?;
        Ok(old)
    }
    fn set_default(&mut self, enabled: bool) -> io::Result<()> {
        self.default = enabled;
        for source in self.sources.iter_mut().flatten() {
            if enabled {
                source.app_cleared = [false; region::APPS];
            }
            let h = source.region.header();
            let generation = if enabled { Self::next_generation(&source.region)? } else { 0 };
            h.recording.store(generation, Ordering::Release);
            for app in &h.apps {
                if app.state.load(Ordering::Acquire) == 2 {
                    app.selection.store(0, Ordering::Release);
                    app.generation.store(generation, Ordering::Release);
                }
            }
        }
        Ok(())
    }
    fn target(&self, id: [u8; 16]) -> Option<(usize, usize)> {
        self.sources
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
            .filter(|(_, s)| {
                !s.disconnected && s.region.header().disabled.load(Ordering::Acquire) == 0
            })
            .find_map(|(i, s)| {
                s.region.header().apps.iter().enumerate().find_map(|(j, a)| {
                    if a.state.load(Ordering::Acquire) != 2 {
                        return None;
                    }
                    let mut bytes = [0; 16];
                    for (word, out) in
                        a.instance.iter().zip(bytes.as_chunks_mut::<8>().0.iter_mut())
                    {
                        out.copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
                    }
                    (bytes == id).then_some((i, j))
                })
            })
    }
    fn unavailable_status(&self) -> Status {
        Status { unavailable: true, request: 1, revision: self.revision, ..Default::default() }
    }
    fn command(&mut self, request: control::Request) -> io::Result<Status> {
        let mut target = if let Some(id) = request.selector {
            match self.target(id) {
                Some(target) => Some(target),
                None => {
                    return Ok(self.unavailable_status());
                }
            }
        } else {
            None
        };
        if request.clean {
            let identity = target.map(|(i, j)| {
                let source = self.sources[i].as_ref().expect("live target");
                (
                    source.region.source(),
                    source.region.header().apps[j].scope.load(Ordering::Acquire),
                )
            });
            self.clean_capture(target, request.selector)?;
            if let Some(id) = request.selector {
                target = self.target(id).filter(|(i, j)| {
                    let source = self.sources[*i].as_ref().expect("live target");
                    identity
                        == Some((
                            source.region.source(),
                            source.region.header().apps[*j].scope.load(Ordering::Acquire),
                        ))
                });
                if target.is_none() {
                    return Ok(self.unavailable_status());
                }
            }
        }
        if request.command == Command::Start {
            self.open_capture()?;
        }
        if request.command != Command::Observe {
            // Controls are a separate metadata phase; app ownership can change
            // after collection and before these requests are processed.
            self.prepare_log()?;
            let enabled = request.command == Command::Start;
            if let Some((source_index, app_index)) = target {
                let source = self.sources[source_index].as_mut().expect("live target");
                if enabled {
                    source.app_cleared[app_index] = false;
                }
                let app = &source.region.header().apps[app_index];
                if app.state.load(Ordering::Acquire) != 2 {
                    return Ok(self.unavailable_status());
                }
                let generation = if enabled { Self::next_generation(&source.region)? } else { 0 };
                app.selection.store(if enabled { 1 } else { 2 }, Ordering::Release);
                app.generation.store(generation, Ordering::Release);
            } else {
                self.set_default(enabled)?;
            }
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("trace control revision exhausted"))?;
            for index in 0..self.sources.len() {
                if self.sources[index].is_some() {
                    self.catalogs(index)?;
                }
            }
            self.boundaries(target)?;
            if !self.any_recording() {
                self.closing = true;
            }
        }
        Ok(self.status(target))
    }
    fn clean_capture(
        &mut self,
        target: Option<(usize, usize)>,
        selector: Option<[u8; 16]>,
    ) -> io::Result<()> {
        if let Some((i, j)) = target {
            let source = self.sources[i].as_mut().expect("live target");
            let header = source.region.header();
            header.apps[j].selection.store(2, Ordering::Release);
            header.apps[j].generation.store(0, Ordering::Release);
            // A paused producer can publish after the head cut. Only this
            // app's old generations are excluded; sibling apps remain admitted.
            source.app_floor[j] = header.generation.load(Ordering::Acquire);
            source.app_cleared[j] = true;
        } else {
            self.set_default(false)?;
            for source in self.sources.iter_mut().flatten() {
                source.floor = source.region.header().generation.load(Ordering::Acquire);
                source.app_cleared = [true; region::APPS];
            }
        }
        let mut cuts = [[0; queue::MAX_PRODUCERS]; queue::MAX_SOURCES];
        for (i, source) in self.sources.iter().enumerate() {
            if let Some(source) = source {
                for (cut, queue) in cuts[i].iter_mut().zip(&source.region.header().queues) {
                    *cut = queue.head.load(Ordering::Acquire);
                }
            }
        }
        // Each turn services 256 events and each queue holds at most 16 KiB.
        // The extra full queue-count covers round-robin granularity. New work
        // from sibling apps is never included in this fixed drain boundary.
        const DRAIN_TURNS: usize = queue::MAX_SOURCES
            * queue::MAX_PRODUCERS
            * (queue::QUEUE_BYTES / 56 / EVENT_QUANTUM + 2);
        for _ in 0..DRAIN_TURNS {
            let drained = self.sources.iter().enumerate().all(|(i, source)| {
                source.as_ref().is_none_or(|source| {
                    source
                        .region
                        .header()
                        .queues
                        .iter()
                        .zip(cuts[i])
                        .all(|(queue, cut)| queue.tail.load(Ordering::Acquire) >= cut)
                })
            });
            if drained {
                break;
            }
            self.collect()?;
        }
        if let Some(log) = &mut self.log {
            log.clean(selector)?;
        }
        if let Some(tail) = &mut self.tail {
            tail.invalidate()?;
        }
        // A clear invalidates catalogs for the removed stream. Repeating a
        // sibling definition is harmless and uses the existing bounded phase.
        for source in self.sources.iter_mut().flatten() {
            source.defined = false;
            source.apps = [0; region::APPS];
        }
        self.closing = !self.any_recording();
        Ok(())
    }
    fn status(&self, target: Option<(usize, usize)>) -> Status {
        let recording = if let Some((i, j)) = target {
            let header = self.sources[i].as_ref().expect("target").region.header();
            header.apps[j].effective_generation(header.recording.load(Ordering::Acquire)) != 0
        } else {
            self.default
        };
        Status {
            request: 1,
            revision: self.revision,
            epoch: self.log.as_ref().map_or([0, 0], |l| l.epoch),
            recording,
            incomplete: !recording,
            sources: self.sources.iter().flatten().count() as u8,
            pending_sources: self
                .sources
                .iter()
                .flatten()
                .filter(|s| {
                    s.disconnected || s.region.header().disabled.load(Ordering::Acquire) != 0
                })
                .count() as u8,
            refused_sources: self.refused,
            unavailable: self.failed,
        }
    }
    fn any_recording(&self) -> bool {
        self.sources.iter().flatten().any(|s| {
            s.region.header().recording.load(Ordering::Acquire) != 0
                || s.region.header().apps.iter().any(|a| {
                    a.state.load(Ordering::Acquire) == 2
                        && a.effective_generation(
                            s.region.header().recording.load(Ordering::Acquire),
                        ) != 0
                })
        })
    }
    fn boundaries(&mut self, target: Option<(usize, usize)>) -> io::Result<()> {
        for i in 0..self.sources.len() {
            let Some(source) = &self.sources[i] else {
                continue;
            };
            let region = source.region.clone();
            let source = region.source();
            if target.is_none() {
                self.payload.clear();
                for value in
                    [0, region.header().recording.load(Ordering::Acquire), u64::from(self.default)]
                {
                    format::integer(value, &mut self.payload).map_err(format_error)?;
                }
                self.append(BlockKind::Recording, source)?;
            }
            for j in 0..region::APPS {
                if target.is_some_and(|t| t != (i, j)) {
                    continue;
                }
                let app = &region.header().apps[j];
                if app.state.load(Ordering::Acquire) != 2 {
                    continue;
                }
                self.payload.clear();
                let generation =
                    app.effective_generation(region.header().recording.load(Ordering::Acquire));
                for value in
                    [app.scope.load(Ordering::Relaxed), generation, u64::from(generation != 0)]
                {
                    format::integer(value, &mut self.payload).map_err(format_error)?;
                }
                self.append(BlockKind::Recording, source)?;
            }
        }
        Ok(())
    }
    fn append(&mut self, kind: BlockKind, source: [u64; 2]) -> io::Result<bool> {
        let Some(log) = &mut self.log else {
            return Ok(false);
        };
        if log.is_stopped() {
            return Ok(false);
        }
        // A cleared, stopped app must leave no new metadata files behind.
        // Its definition is emitted again only after recording is enabled.
        let scope = self.block_scope(kind)?;
        if let Some(scope) = scope.filter(|scope| *scope != 0) {
            let cleared = self
                .sources
                .iter()
                .flatten()
                .filter(|attached| attached.region.source() == source)
                .any(|attached| {
                    let header = attached.region.header();
                    header.apps.iter().enumerate().any(|(slot, app)| {
                        app.scope.load(Ordering::Acquire) == scope
                            && attached.app_cleared[slot]
                            && app.effective_generation(header.recording.load(Ordering::Acquire))
                                == 0
                    })
                });
            if cleared {
                return Ok(kind == BlockKind::AppState);
            }
        }
        let log = self.log.as_ref().expect("capture checked");
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("trace sequence exhausted"))?;
        let envelope = Envelope {
            kind,
            epoch: log.epoch,
            first: self.sequence,
            count: 1,
            source,
            time_base: crate::clock_ns(),
        };
        envelope.encode(&self.payload, &mut self.encoded).map_err(format_error)?;
        let instance = self.block_instance(kind, source)?;
        let log = self.log.as_mut().expect("capture checked");
        match log.append_to(&self.encoded, crate::clock_ns(), instance)? {
            Append::Written => Ok(true),
            Append::Deferred => Ok(false),
            Append::Stopped => Err(io::Error::other("trace recording stopped")),
            _ if kind == BlockKind::Events => Ok(false),
            _ => Err(io::Error::other(
                "trace stopped because required catalog/control metadata could not be retained",
            )),
        }
    }
    fn block_scope(&self, kind: BlockKind) -> io::Result<Option<u64>> {
        Ok(match kind {
            BlockKind::Epoch | BlockKind::Source => None,
            BlockKind::Events => Some(u64::from(u32::from_le_bytes(
                self.payload[..4].try_into().expect("event scope"),
            ))),
            _ => {
                let mut payload = self.payload.as_slice();
                Some(format::read_integer(&mut payload).map_err(format_error)?)
            }
        })
    }
    fn block_instance(&self, kind: BlockKind, source: [u64; 2]) -> io::Result<Option<[u8; 16]>> {
        let Some(scope) = self.block_scope(kind)? else {
            return Ok(None);
        };
        if scope == 0 {
            return Ok(None);
        }
        let app = self
            .sources
            .iter()
            .flatten()
            .find(|attached| attached.region.source() == source)
            .and_then(|attached| {
                attached
                    .region
                    .header()
                    .apps
                    .iter()
                    .find(|app| app.scope.load(Ordering::Acquire) == scope)
            })
            .ok_or_else(|| io::Error::other("trace app scope has no trusted instance"))?;
        Ok(Some(Self::instance(app)))
    }
    fn instance(app: &region::App) -> [u8; 16] {
        let mut bytes = [0; 16];
        for (word, out) in app.instance.iter().zip(bytes.as_chunks_mut::<8>().0.iter_mut()) {
            out.copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        bytes
    }
    fn capture_failed(&mut self, error: &io::Error) {
        crate::diagnostic!("Trace capture is incomplete: {error}");
        self.failed = true;
        self.closing = false;
        self.default = false;
        for source in self.sources.iter().flatten() {
            let h = source.region.header();
            h.recording.store(0, Ordering::Release);
            for app in &h.apps {
                app.selection.store(0, Ordering::Release);
                app.generation.store(0, Ordering::Release);
            }
        }
        if let Some(log) = &mut self.log {
            log.close();
        }
    }

    fn catalogs(&mut self, index: usize) -> io::Result<()> {
        let region = self.sources[index].as_ref().expect("source").region.clone();
        let h = region.header();
        let id = region.source();
        if !self.sources[index].as_ref().expect("source").defined {
            self.payload.clear();
            let count = h.name_count.load(Ordering::Acquire) as usize;
            if count > region::MAX_NAMES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            for value in
                [h.kind.load(Ordering::Relaxed), h.pid.load(Ordering::Relaxed), count as u64]
            {
                format::integer(value, &mut self.payload).map_err(format_error)?;
            }
            for name in &h.names[..count] {
                let mut bytes = [0; 48];
                let text = name.get(&mut bytes)?;
                format::integer(text.len() as u64, &mut self.payload).map_err(format_error)?;
                self.payload.extend_from_slice(text.as_bytes());
            }
            let written = self.append(BlockKind::Source, id)?;
            self.sources[index].as_mut().expect("source").defined = written;
        }
        for slot in 0..region::APPS {
            let app = &h.apps[slot];
            let state = app.state.load(Ordering::Acquire);
            if !matches!(state, 2 | 3) {
                continue;
            }
            let scope = u32::try_from(app.scope.load(Ordering::Relaxed))
                .map_err(|_| io::ErrorKind::InvalidData)?;
            if self.sources[index].as_ref().expect("source").apps[slot] == scope {
                continue;
            }
            self.app_definition(scope, app)?;
            if self.append(BlockKind::AppDefinition, id)? {
                self.sources[index].as_mut().expect("source").apps[slot] = scope;
            }
        }
        Ok(())
    }
    fn app_definition(&mut self, scope: u32, app: &region::App) -> io::Result<()> {
        self.payload.clear();
        format::integer(u64::from(scope), &mut self.payload).map_err(format_error)?;
        let flags = app.identity_flags.load(Ordering::Relaxed);
        if flags > 3 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        format::integer(flags, &mut self.payload).map_err(format_error)?;
        for i in 0..2 {
            if flags & (1 << i) != 0 {
                for word in &app.identity[i * 4..i * 4 + 4] {
                    self.payload.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
                }
            }
        }
        for word in &app.instance {
            self.payload.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        let length = app.label_length.load(Ordering::Relaxed) as usize;
        if length == 0 || length > 256 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        format::integer(length as u64, &mut self.payload).map_err(format_error)?;
        let mut bytes = [0; 256];
        for (word, out) in app.label.iter().zip(bytes.as_chunks_mut::<8>().0.iter_mut()) {
            out.copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        self.payload.extend_from_slice(&bytes[..length]);
        let count = app.count.load(Ordering::Relaxed) as usize;
        if count > region::APP_NAMES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        format::integer(count as u64, &mut self.payload).map_err(format_error)?;
        for i in 0..count {
            format::integer(app.kinds[i].load(Ordering::Relaxed), &mut self.payload)
                .map_err(format_error)?;
            let mut bytes = [0; 48];
            let name = app.names[i].get(&mut bytes)?;
            format::integer(name.len() as u64, &mut self.payload).map_err(format_error)?;
            self.payload.extend_from_slice(name.as_bytes());
        }
        Ok(())
    }
    fn prepare_log(&mut self) -> io::Result<()> {
        let Some(log) = self.log.as_mut().filter(|log| !log.is_stopped()) else {
            return Ok(());
        };
        let mut instances = [[0; 16]; queue::MAX_SOURCES * region::APPS];
        let mut count = 0;
        for source in self.sources.iter().flatten() {
            for app in &source.region.header().apps {
                if matches!(app.state.load(Ordering::Acquire), 2 | 3) {
                    let id = Self::instance(app);
                    if !instances[..count].contains(&id) {
                        instances[count] = id;
                        count += 1;
                    }
                }
            }
        }
        match log.prepare_streams(&instances[..count], crate::clock_ns())? {
            Preparation::Ready => return Ok(()),
            Preparation::Deferred => {
                return Err(io::Error::other("trace rotation write credit unavailable"));
            }
            Preparation::Rotated => {}
        }
        for source in self.sources.iter_mut().flatten() {
            source.defined = false;
            source.apps = [0; region::APPS];
        }
        Ok(())
    }
    fn collect(&mut self) -> io::Result<bool> {
        // Capture complete-event heads before the app roster. An app published
        // after preparation cannot contribute a consumed event in this turn;
        // its catalog and events can wait for the next bounded preparation.
        let mut cuts = [[0; queue::MAX_PRODUCERS]; queue::MAX_SOURCES];
        for (i, source) in self.sources.iter().enumerate() {
            if let Some(source) = source {
                for (cut, queue) in cuts[i].iter_mut().zip(&source.region.header().queues) {
                    *cut = queue.head.load(Ordering::Acquire);
                }
            }
        }
        let active = self.log.as_ref().is_some_and(|log| !log.is_stopped());
        self.prepare_log()?;
        for index in 0..self.sources.len() {
            if let Some(source) = &mut self.sources[index] {
                source.activity = source.region.header().activity.load(Ordering::Acquire);
                if active {
                    self.catalogs(index)?;
                }
            }
        }
        let mut remaining = EVENT_QUANTUM;
        let queues = queue::MAX_SOURCES * queue::MAX_PRODUCERS;
        let first = self.cursor;
        // Eight events per queue visit prevents a hot source from monopolizing a turn.
        for offset in 0..queues {
            let flat = (first + offset) % queues;
            let index = flat / queue::MAX_PRODUCERS;
            if self.sources[index].is_none() {
                continue;
            }
            let queue = flat % queue::MAX_PRODUCERS;
            self.collect_queue(index, queue, cuts[index][queue], &mut remaining, active)?;
            self.cursor = (flat + 1) % queues;
            if remaining == 0 {
                break;
            }
        }
        for index in 0..self.sources.len() {
            if self.sources[index].is_some() {
                self.losses(index)?;
                self.retire_apps(index)?;
                self.retire_source(index)?;
            }
        }
        if self.closing && !self.queued() {
            if let Some(log) = &mut self.log {
                log.close();
            }
            self.closing = false;
        }
        Ok(remaining == 0)
    }
    fn collect_queue(
        &mut self,
        index: usize,
        queue_index: usize,
        cut: u64,
        remaining: &mut usize,
        active: bool,
    ) -> io::Result<()> {
        let source = self.sources[index].as_ref().expect("source");
        let region = source.region.clone();
        let floor = source.floor;
        let q = &region.header().queues[queue_index];
        for _ in 0..8 {
            if *remaining == 0 || q.tail.load(Ordering::Acquire) >= cut {
                break;
            }
            let length = match q.pop(&mut self.event) {
                Ok(n) => n,
                Err(error) => {
                    crate::diagnostic!("Malformed trace queue: {error}");
                    q.tail.store(q.head.load(Ordering::Acquire), Ordering::Release);
                    region.header().lost.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            };
            if length == 0 {
                break;
            }
            *remaining -= 1;
            let event = format::Events::decode(&self.event[..length]).map_err(format_error)?;
            let known = event.scope == 0
                || self.sources[index].as_ref().expect("source").apps.contains(&event.scope);
            let app_floor = self.sources[index]
                .as_ref()
                .expect("source")
                .apps
                .iter()
                .position(|scope| *scope == event.scope)
                .map_or(0, |slot| self.sources[index].as_ref().expect("source").app_floor[slot]);
            let admitted = event.generation >= floor.max(app_floor)
                && event.generation < region.header().generation.load(Ordering::Acquire);
            if active && admitted && known && self.sources[index].as_ref().expect("source").defined
            {
                self.payload.clear();
                self.payload.extend_from_slice(&self.event[..length]);
                if !self.append(BlockKind::Events, region.source())? {
                    region.header().lost.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                region.header().lost.fetch_add(1, Ordering::Relaxed);
            }
        }
        if q.state.load(Ordering::Acquire) == 2
            && q.head.load(Ordering::Acquire) == q.tail.load(Ordering::Acquire)
        {
            q.state.store(0, Ordering::Release);
        }
        Ok(())
    }
    fn queued(&self) -> bool {
        self.sources.iter().flatten().any(|s| {
            s.region
                .header()
                .queues
                .iter()
                .any(|q| q.head.load(Ordering::Acquire) != q.tail.load(Ordering::Acquire))
        })
    }
    fn losses(&mut self, index: usize) -> io::Result<()> {
        let region = self.sources[index].as_ref().expect("source").region.clone();
        let loss = region.header().lost.load(Ordering::Acquire);
        if loss != self.sources[index].as_ref().expect("source").loss {
            self.payload.clear();
            for v in [0, loss] {
                format::integer(v, &mut self.payload).map_err(format_error)?;
            }
            if self.append(BlockKind::Loss, region.source())? {
                self.sources[index].as_mut().expect("source").loss = loss;
            }
        }
        for slot in 0..region::APPS {
            let app = &region.header().apps[slot];
            let lost = app.lost.load(Ordering::Acquire);
            if lost != self.sources[index].as_ref().expect("source").app_loss[slot] {
                self.payload.clear();
                for value in [app.scope.load(Ordering::Acquire), lost] {
                    format::integer(value, &mut self.payload).map_err(format_error)?;
                }
                if self.append(BlockKind::Loss, region.source())? {
                    self.sources[index].as_mut().expect("source").app_loss[slot] = lost;
                }
            }
        }
        Ok(())
    }
    fn retire_apps(&mut self, index: usize) -> io::Result<()> {
        let region = self.sources[index].as_ref().expect("source").region.clone();
        for (slot, app) in region.header().apps.iter().enumerate() {
            if app.state.load(Ordering::Acquire) != 3 {
                continue;
            }
            if app.retire_heads.iter().zip(&region.header().queues).any(|(cut, queue)| {
                queue.tail.load(Ordering::Acquire) < cut.load(Ordering::Relaxed)
            }) {
                continue;
            }
            let scope = app.scope.load(Ordering::Relaxed);
            self.payload.clear();
            for value in [scope, app.lost.load(Ordering::Relaxed), 1] {
                format::integer(value, &mut self.payload).map_err(format_error)?;
            }
            if self.log.as_ref().is_none_or(|l| l.is_stopped())
                || self.append(BlockKind::AppState, region.source())?
            {
                app.state.store(0, Ordering::Release);
                self.sources[index].as_mut().expect("source").apps[slot] = 0;
                self.sources[index].as_mut().expect("source").app_loss[slot] = 0;
                self.sources[index].as_mut().expect("source").app_floor[slot] = 0;
                self.sources[index].as_mut().expect("source").app_cleared[slot] = false;
            }
        }
        Ok(())
    }
    fn retire_source(&mut self, index: usize) -> io::Result<()> {
        let Some(source) = &self.sources[index] else {
            return Ok(());
        };
        if !source.dead {
            return Ok(());
        }
        let region = source.region.clone();
        let h = region.header();
        if h.queues.iter().any(|q| q.head.load(Ordering::Acquire) != q.tail.load(Ordering::Acquire))
        {
            return Ok(());
        }
        if self.log.as_ref().is_some_and(|l| !l.is_stopped()) {
            for app in &h.apps {
                if !matches!(app.state.load(Ordering::Acquire), 2 | 3) {
                    continue;
                }
                self.payload.clear();
                for value in
                    [app.scope.load(Ordering::Acquire), app.lost.load(Ordering::Acquire), 2]
                {
                    format::integer(value, &mut self.payload).map_err(format_error)?;
                }
                if !self.append(BlockKind::AppState, region.source())? {
                    return Ok(());
                }
            }
            self.payload.clear();
            for value in [0, 0, 0] {
                format::integer(value, &mut self.payload).map_err(format_error)?;
            }
            if !self.append(BlockKind::Recording, region.source())? {
                return Ok(());
            }
        }
        self.sources[index] = None;
        Ok(())
    }
    fn ready(&self) -> bool {
        self.sources.iter().flatten().any(|s| {
            s.region.header().activity.load(Ordering::Acquire) != s.activity
                || s.region
                    .header()
                    .queues
                    .iter()
                    .any(|q| q.head.load(Ordering::Acquire) != q.tail.load(Ordering::Relaxed))
        })
    }
    fn run(&mut self) -> io::Result<()> {
        loop {
            self.registration()?;
            let more = match self.collect() {
                Ok(more) => more,
                Err(error) => {
                    self.capture_failed(&error);
                    false
                }
            };
            let requests = self.control.advance()?;
            for (index, request) in requests.into_iter().enumerate() {
                if let Some(request) = request {
                    let status = match self.command(request) {
                        Ok(status) => status,
                        Err(e) => {
                            self.capture_failed(&e);
                            self.unavailable_status()
                        }
                    };
                    self.control.reply(index, status);
                }
            }
            if self.poll_sources(more)? {
                let _ = self.collect();
                return Ok(());
            }
        }
    }
    fn poll_sources(&mut self, more: bool) -> io::Result<bool> {
        let mut fds = Vec::with_capacity(2 + 5 + PENDING + queue::MAX_SOURCES * 2);
        fds.push(libc::pollfd { fd: self.stop.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd {
            fd: self.listener.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN,
            revents: 0,
        });
        let start = fds.len();
        fds.resize(start + 5, libc::pollfd { fd: -1, events: 0, revents: 0 });
        self.control.descriptors(&mut fds[start..]);
        for pending in &self.pending {
            fds.push(libc::pollfd {
                fd: pending.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for source in self.sources.iter().flatten() {
            fds.push(libc::pollfd {
                fd: if source.disconnected { -1 } else { source.notification.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            });
            fds.push(libc::pollfd {
                fd: if source.dead {
                    -1
                } else {
                    source.watch.as_ref().map_or(-1, process::Watch::descriptor)
                },
                events: libc::POLLIN,
                revents: 0,
            });
            source.region.header().sleeping.store(1, Ordering::SeqCst);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        let blocking = !more && !self.ready();
        process::wait(&mut fds, blocking)?;
        for source in self.sources.iter().flatten() {
            source.region.header().sleeping.store(0, Ordering::SeqCst);
        }
        if fds[0].revents != 0 {
            return Ok(true);
        }
        self.observe_sources(&fds);
        Ok(false)
    }
    fn observe_sources(&mut self, fds: &[libc::pollfd]) {
        let mut i = 2 + 5 + PENDING;
        for index in 0..self.sources.len() {
            let Some(source) = &mut self.sources[index] else {
                continue;
            };
            let events = fds[i].revents;
            i += 2;
            if events & libc::POLLIN != 0 {
                let _ = source.notification.read(&mut [0; 4096]);
            }
            if events & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                source.disconnected = true;
            }
            if !source.dead
                && let Some(watch) = &source.watch
            {
                match watch.exited() {
                    Ok(true) => {
                        source.dead = true;
                        source.disconnected = true;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        crate::diagnostic!("Trace process observation failed: {error}");
                        source.dead = true;
                        source.disconnected = true;
                    }
                }
            }
        }
    }
    fn registration(&mut self) -> io::Result<()> {
        if let Some(listener) = &self.listener {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    if notify::configure(&stream, 1024).is_ok()
                        && descriptors::peer(&stream).is_ok()
                    {
                        if let Some(slot) = self.pending.iter_mut().find(|s| s.is_none()) {
                            *slot = Some(stream);
                        } else {
                            self.refused = self.refused.saturating_add(1);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        for i in 0..PENDING {
            let Some(stream) = &self.pending[i] else {
                continue;
            };
            let received = descriptors::receive(stream);
            if received.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock) {
                continue;
            }
            let mut stream = self.pending[i].take().expect("pending");
            let result =
                received.and_then(|(bytes, rights)| self.register(&stream, &bytes, rights));
            if result.is_ok() {
                let _ = stream.write(ACK);
            } else {
                self.refused = self.refused.saturating_add(1);
            }
        }
        Ok(())
    }
    fn register(
        &mut self,
        stream: &UnixStream,
        bytes: &[u8; descriptors::REGISTRATION_BYTES],
        rights: [OwnedFd; 2],
    ) -> io::Result<()> {
        if &bytes[..8] != REGISTER_MAGIC || bytes[24..] != [0; 40] {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let id = [
            u64::from_le_bytes(bytes[8..16].try_into().expect("source")),
            u64::from_le_bytes(bytes[16..24].try_into().expect("source")),
        ];
        let [region, notification] = rights;
        let region = Arc::new(Region::attach(region, id)?);
        let pid = descriptors::peer(stream)?;
        if region.header().pid.load(Ordering::Acquire) != u64::from(pid) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let existing =
            self.sources.iter().position(|s| s.as_ref().is_some_and(|s| s.region.source() == id));
        let index = existing
            .or_else(|| self.sources.iter().position(Option::is_none))
            .ok_or(io::ErrorKind::WouldBlock)?;
        let floor = existing.map_or_else(
            || region.header().generation.load(Ordering::Acquire),
            |i| self.sources[i].as_ref().expect("source").floor,
        );
        if existing.is_none() {
            let generation = if self.default { Self::next_generation(&region)? } else { 0 };
            region.header().recording.store(generation, Ordering::Release);
            for app in &region.header().apps {
                if app.state.load(Ordering::Acquire) == 2 {
                    app.selection.store(0, Ordering::Release);
                    app.generation.store(generation, Ordering::Release);
                }
            }
        }
        let notification = UnixStream::from(notification);
        notify::configure(&notification, 1024)?;
        notification.set_nonblocking(true)?;
        let mut attached = Attached::new(region, notification, Some(process::Watch::new(pid)?));
        attached.floor = floor;
        self.sources[index] = Some(attached);
        Ok(())
    }
}
pub fn attach(profile: &Path) -> io::Result<()> {
    let source =
        source::SOURCE.get().ok_or_else(|| io::Error::other("trace bootstrap required"))?;
    source.bind_profile(profile)?;
    if ATTACHING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Ok(());
    }
    let profile = profile.to_owned();
    let result = thread::Builder::new()
        .name("playbit-trace-register".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            if let Err(e) = register_source(source, &profile) {
                crate::diagnostic!("Playbit trace attachment unavailable: {e}");
            }
            ATTACHING.store(false, Ordering::Release);
        });
    if let Err(e) = result {
        ATTACHING.store(false, Ordering::Release);
        return Err(e);
    }
    Ok(())
}
fn register_source(source: &source::Source, profile: &Path) -> io::Result<()> {
    process::background()?;
    let mut stream = descriptors::connect(&profile.join(crate::profile_paths::REGISTER))?;
    descriptors::peer(&stream)?;
    notify::configure(&stream, 1024)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
    let (producer, receiver) = notify::pair()?;
    let receiver: OwnedFd = receiver.into();
    let mut bytes = [0; descriptors::REGISTRATION_BYTES];
    bytes[..8].copy_from_slice(REGISTER_MAGIC);
    for (out, value) in bytes[8..24].as_chunks_mut::<8>().0.iter_mut().zip(source.region.source()) {
        out.copy_from_slice(&value.to_le_bytes());
    }
    descriptors::send(&stream, &bytes, [source.region.descriptor()?, &receiver])?;
    let mut ack = [0; 8];
    stream.read_exact(&mut ack)?;
    if &ack != ACK {
        return Err(io::Error::other("invalid trace registration acknowledgment"));
    }
    source.attach(producer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use playbit_api::abi::trace::{self as wire, TraceEvent};
    fn fixture() -> (Collector, PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let profile = std::env::temp_dir().join(format!(
            "pt-rec-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&profile).expect("test profile");
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("private profile");
        let directory = profile.join("trace");
        let log = Log::open(&directory, false, crate::clock_ns()).expect("bounded test log");
        let (_, stop) = notify::pair().expect("stop pair");
        let control = control::Server::bind(&profile).expect("real control endpoint");
        let collector = Collector {
            profile: profile.clone(),
            directory,
            small: false,
            default: true,
            closing: false,
            failed: false,
            cursor: 0,
            log: Some(log),
            tail: None,
            control,
            sources: (0..queue::MAX_SOURCES).map(|_| None).collect(),
            pending: std::array::from_fn(|_| None),
            listener: None,
            stop,
            sequence: 0,
            encoded: Vec::with_capacity(format::MAX_BLOCK_BYTES),
            payload: Vec::with_capacity(format::MAX_PAYLOAD_BYTES),
            event: [0; queue::EVENT_MAX_BYTES],
            revision: 1,
            refused: 0,
        };
        (collector, profile)
    }
    fn source() -> Attached {
        let region = Arc::new(Region::create(8, &["test.work"]).expect("bounded mapping"));
        region.header().recording.store(1, Ordering::Release);
        region.header().generation.store(2, Ordering::Release);
        let (_, notification) = notify::pair().expect("notification pair");
        Attached::new(region, notification, None)
    }
    fn publish(region: &Region, queue: usize, count: usize) {
        let mut prefix = [0; 16];
        prefix[4] = 1;
        prefix[8] = 1;
        for timestamp in 1..=count {
            let mut bytes = [0; 40];
            wire::encode_header(
                TraceEvent {
                    timestamp: timestamp as u64,
                    kind: wire::INSTANT,
                    ..Default::default()
                },
                &mut bytes,
            )
            .expect("header");
            assert!(region.header().queues[queue].push(&prefix, &bytes));
        }
    }
    #[test]
    fn stop_drains_admitted_backlog_without_waiting_for_producers() {
        // Stop closes admission immediately but retains more than one turn of
        // already-published complete events before declaring the log stopped.
        let (mut recorder, path) = fixture();
        recorder.sources[0] = Some(source());
        let region = recorder.sources[0].as_ref().expect("source").region.clone();
        publish(&region, 0, 290);
        publish(&region, 1, 100);
        recorder.collect().expect("first bounded turn");
        let status = recorder
            .command(control::Request { command: Command::Stop, selector: None, clean: false })
            .expect("stop admission");
        assert!(!status.recording);
        assert_eq!(region.header().recording.load(Ordering::Acquire), 0);
        assert!(!recorder.log.as_ref().expect("log").is_stopped());
        for _ in 0..64 {
            recorder.collect().expect("bounded backlog turn");
            if recorder.log.as_ref().expect("log").is_stopped() {
                break;
            }
        }
        assert!(recorder.log.as_ref().expect("log").is_stopped());
        let reader = recorder.log.as_ref().expect("log").reader();
        let mut position = crate::log::Position::new(None);
        let mut output = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        let mut count = 0;
        for _ in 0..1024 {
            match reader.next(&mut position, &mut output).expect("retained backlog") {
                crate::log::ReadResult::Block(envelope) if envelope.kind == BlockKind::Events => {
                    count += 1
                }
                crate::log::ReadResult::Stopped => break,
                _ => {}
            }
        }
        assert_eq!(count, 390);
        drop(reader);
        drop(recorder);
        fs::remove_dir_all(path).expect("test cleanup");
    }
    #[test]
    fn rotating_cursor_services_a_later_source_under_saturation() {
        // Saturating every queue in source zero spends the whole first turn.
        // The next turn must still service the last source before wrapping.
        let (mut recorder, path) = fixture();
        recorder.sources[0] = Some(source());
        recorder.sources[7] = Some(source());
        for queue in 0..queue::MAX_PRODUCERS {
            publish(&recorder.sources[0].as_ref().expect("source").region, queue, 16);
        }
        let last = recorder.sources[7].as_ref().expect("last source").region.clone();
        publish(&last, 0, 1);
        recorder.collect().expect("full first turn");
        assert_eq!(last.header().queues[0].tail.load(Ordering::Acquire), 0);
        recorder.collect().expect("fair next turn");
        assert_ne!(last.header().queues[0].tail.load(Ordering::Acquire), 0);
        drop(recorder);
        fs::remove_dir_all(path).expect("test cleanup");
    }
    #[test]
    fn mandatory_metadata_refusal_stops_without_loss_feedback() {
        // Exhaust the same bounded byte allowance with valid maximum catalogs.
        // Refusing mandatory metadata must not create another loss publication.
        let (mut recorder, path) = fixture();
        recorder.sources[0] = Some(source());
        let region = recorder.sources[0].as_ref().expect("source").region.clone();
        recorder.payload.clear();
        for value in [8, 1, 1024] {
            format::integer(value, &mut recorder.payload).expect("source prefix");
        }
        for _ in 0..1024 {
            format::integer(48, &mut recorder.payload).expect("name extent");
            recorder.payload.extend_from_slice(b"test.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        }
        let mut refused = None;
        for _ in 0..256 {
            if let Err(error) = recorder.append(BlockKind::Source, region.source()) {
                refused = Some(error);
                break;
            }
        }
        let error = refused.expect("metadata credit must exhaust");
        assert_eq!(region.header().lost.load(Ordering::Acquire), 0);
        recorder.capture_failed(&error);
        assert!(recorder.failed);
        assert_eq!(region.header().recording.load(Ordering::Acquire), 0);
        assert!(recorder.log.as_ref().expect("retained log").is_stopped());
        drop(recorder);
        fs::remove_dir_all(path).expect("test cleanup");
    }
    #[test]
    fn retired_catalog_reclaims_while_unrelated_work_remains() {
        // Only the finite prefix published before ownership retirement must be
        // consumed. Future native work cannot prevent catalog slot reuse.
        let (mut recorder, path) = fixture();
        recorder.sources[0] = Some(source());
        let region = recorder.sources[0].as_ref().expect("source").region.clone();
        let app = &region.header().apps[0];
        let queue = &region.header().queues[0];
        app.scope.store(1, Ordering::Relaxed);
        publish(&region, 0, 1);
        app.retire_heads[0].store(queue.head.load(Ordering::Acquire), Ordering::Relaxed);
        app.state.store(3, Ordering::Release);
        publish(&region, 0, 1);
        recorder.retire_apps(0).expect("retain published prefix");
        assert_eq!(app.state.load(Ordering::Acquire), 3);
        let mut copy = [0; queue::EVENT_MAX_BYTES];
        queue.pop(&mut copy).expect("consume retired prefix");
        recorder.retire_apps(0).expect("reclaim retired catalog");
        assert_eq!(app.state.load(Ordering::Acquire), 0);
        assert!(queue.tail.load(Ordering::Acquire) < queue.head.load(Ordering::Acquire));
        drop(recorder);
        fs::remove_dir_all(path).expect("test cleanup");
    }
    #[test]
    fn a_reused_cleaned_slot_records_under_the_unchanged_profile_default() {
        let (mut recorder, path) = fixture();
        recorder.sources[0] = Some(source());
        let region = recorder.sources[0].as_ref().expect("source").region.clone();
        let configure = |scope: u64, id: [u8; 16]| {
            let app = &region.header().apps[0];
            app.scope.store(scope, Ordering::Relaxed);
            for (word, bytes) in app.instance.iter().zip(id.as_chunks::<8>().0) {
                word.store(u64::from_le_bytes(*bytes), Ordering::Relaxed);
            }
            app.label_length.store(1, Ordering::Relaxed);
            app.label[0].store(u64::from(b'A'), Ordering::Relaxed);
            app.count.store(0, Ordering::Relaxed);
            app.selection.store(0, Ordering::Relaxed);
            app.state.store(2, Ordering::Release);
        };
        configure(1, [1; 16]);
        recorder.collect().expect("first app catalog");
        recorder
            .command(control::Request {
                command: Command::Stop,
                selector: Some([1; 16]),
                clean: true,
            })
            .expect("clean first app");
        region.header().apps[0].state.store(3, Ordering::Release);
        recorder.collect().expect("retire cleared app");
        assert_eq!(region.header().apps[0].state.load(Ordering::Acquire), 0);
        configure(2, [2; 16]);
        let mut prefix = [0; 16];
        prefix[..4].copy_from_slice(&2u32.to_le_bytes());
        prefix[4..8].copy_from_slice(&1u32.to_le_bytes());
        prefix[8..].copy_from_slice(&1u64.to_le_bytes());
        let mut event = [0; 40];
        wire::encode_header(
            TraceEvent { timestamp: 1, kind: wire::INSTANT, ..Default::default() },
            &mut event,
        )
        .expect("second app event");
        assert!(region.header().queues[0].push(&prefix, &event));
        recorder.collect().expect("replacement app recording");
        let reader = recorder.log.as_ref().expect("log").reader();
        let mut position = crate::log::Position::new(None);
        let mut output = Vec::with_capacity(format::MAX_BLOCK_BYTES);
        let mut observed = false;
        for _ in 0..64 {
            match reader.next(&mut position, &mut output).expect("retained event") {
                crate::log::ReadResult::Block(envelope) if envelope.kind == BlockKind::Events => {
                    let (_, payload) = Envelope::decode(&output).expect("retained block");
                    assert_eq!(format::Events::decode(payload).expect("app event").scope, 2);
                    observed = true;
                }
                crate::log::ReadResult::Pending => break,
                _ => (),
            }
        }
        assert!(observed, "slot reuse must not inherit the cleaned app's generation floor");
        assert!(!recorder.directory.join(crate::identifier::encode(&[1; 16])).exists());
        drop(reader);
        drop(recorder);
        fs::remove_dir_all(path).expect("cleanup");
    }
}
