//! Same-user tools address one exact GUI-owned session instance. One exchange
//! has one result; losing a reply never replays input or an application Quit.
mod canvas_input;

#[cfg(feature = "process")]
use crate::engine::ipc;
use crate::{
    engine::{Result, session_instance::LaunchOwner},
    store::Id,
};
#[cfg(feature = "process")]
use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

pub const REQUEST_LIMIT: usize = playbit_api::abi::syscall::MAX_EVENT_BYTES + 1024;
// Tool snapshots retain their existing IPC budget independently of rendering.
pub const MAX_SCENE_BYTES: usize = 1024 * 1024;
pub const REPLY_LIMIT: usize = MAX_SCENE_BYTES + 4096;
pub const MAX_SCREENSHOT_BYTES: usize = 2 * 1024 * 1024;
pub const SCREENSHOT_REPLY_LIMIT: usize = MAX_SCREENSHOT_BYTES + 4096;

#[cfg(all(feature = "process", target_os = "macos"))]
pub use crate::engine::process_owner::ExitStatusWatch;

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct LogCursor {
    pub instance: Id,
    pub generation: u64,
    pub next: u64,
}
/// Metadata recorded by the native owner when it publishes guest output.
/// Diagnostic rows added by a reader have no source timestamp.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct LogEntry {
    pub timestamp_micros: Option<u64>,
    pub severity: LogSeverity,
    pub message: String,
}
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum LogSeverity {
    Info,
    Error,
}
impl LogEntry {
    pub fn recorded(message: String, severity: LogSeverity) -> Self {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch");
        Self {
            timestamp_micros: Some(
                u64::try_from(time.as_micros()).expect("wall clock fits u64 microseconds"),
            ),
            severity,
            message,
        }
    }
    pub fn notice(message: String) -> Self {
        Self { timestamp_micros: None, severity: LogSeverity::Info, message }
    }
}
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct LogBatch {
    pub cursor: LogCursor,
    pub lines: Vec<LogEntry>,
    pub dropped: u64,
    pub reset: bool,
}
impl LogBatch {
    #[cfg(feature = "process")]
    fn validate(&self, after: Option<LogCursor>) -> Result<()> {
        let same = after.is_some_and(|old| {
            old.instance == self.cursor.instance && old.generation == self.cursor.generation
        });
        let first = if same { after.expect("same cursor is present").next } else { 0 };
        if self.reset != (after.is_some() && !same)
            || self.cursor.next.checked_sub(first)
                != self.dropped.checked_add(self.lines.len() as u64)
            || (same && first == self.cursor.next)
        {
            return Err("Invalid app log cursor or dropped-line count".into());
        }
        Ok(())
    }
}

/// Return an exact slice offset and overwritten-line count before materializing
/// a batch. The owner keeps at most 256 lines ending at its monotonic cursor.
#[cfg(all(feature = "desktop-ui", feature = "process"))]
pub(crate) fn log_window(
    next: u64,
    retained: usize,
    after: Option<u64>,
) -> std::result::Result<Option<(usize, u64)>, &'static str> {
    if retained > 256 || retained as u64 > next {
        return Err("Invalid retained app log range");
    }
    let requested = after.unwrap_or(0);
    if requested > next {
        return Err("App log cursor is ahead of this owner");
    }
    if after == Some(next) {
        return Ok(None);
    }
    let first = next - retained as u64;
    Ok(Some((requested.saturating_sub(first) as usize, first.saturating_sub(requested))))
}

#[derive(serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Request {
    pub instance: Id,
    pub generation: u64,
    pub operation: Operation,
}

#[derive(serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Operation {
    Frame,
    Logs { after: Option<LogCursor> },
    Input { kind: u32, target: u32, data: Vec<u8> },
    Quit,
    Show { presentation: u64 },
    Screenshot,
}
#[derive(Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Reply {
    Frame { instance: Id, generation: u64, build: String, scene: Vec<u8> },
    Logs(LogBatch),
    Accepted { instance: Id, generation: u64 },
    Error { error: String },
    Screenshot { instance: Id, generation: u64, png: Vec<u8> },
}

#[allow(unsafe_code, reason = "geteuid returns the socket namespace's OS account.")]
#[cfg(feature = "process")]
pub fn socket_path(profile: &Path, instance: Id) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(profile.canonicalize()?.as_os_str().as_bytes());
    hash.update(instance.as_bytes());
    let digest = hash.finish();
    let name: String = digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    // Keep the absolute address inside macOS's sun_path bound even for long
    // test/user profile paths. This is only discovery: every peer and binding
    // is verified independently. Never unlink an existing endpoint on launch.
    // SAFETY: geteuid has no pointers or process-state mutation.
    let uid = unsafe { libc::geteuid() };
    Ok(PathBuf::from(format!("/tmp/pb-app-{uid}-{name}.sock")))
}

/// No session-level ambiguity and no automatic substitute when the selected
/// process exits. Unknown owners may still serve logs/Quit during recovery.
pub fn select(owners: Vec<LaunchOwner>, selected: Option<Id>) -> Result<LaunchOwner> {
    let mut owners = owners.into_iter().filter(|owner| owner.process.is_some());
    if let Some(id) = selected {
        return owners
            .find(|owner| owner.instance.id == id)
            .ok_or_else(|| "The selected session instance has no verified live owner".into());
    }
    let owner = owners.next().ok_or("This session has no verified live app owner")?;
    if owners.next().is_some() {
        return Err("Several session instances are running; select one with --instance ID".into());
    }
    Ok(owner)
}

#[cfg(feature = "process")]
pub struct Exchange {
    stream: UnixStream,
    instance: Id,
    generation: u64,
    after: Option<LogCursor>,
    operation: u8,
    #[cfg(target_os = "macos")]
    process: crate::engine::session_instance::ProcessIdentity,
}
#[cfg(feature = "process")]
impl Exchange {
    pub fn connect(profile: &Path, owner: &LaunchOwner, operation: Operation) -> Result<Self> {
        let request = Request {
            instance: owner.instance.id,
            generation: owner.instance.owner_generation,
            operation,
        };
        let mut stream = UnixStream::connect(socket_path(profile, request.instance)?).map_err(
            |error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "The selected app's control endpoint is unavailable; no request was sent: {error}"
                    ),
                )
            },
        )?;
        ipc::verify_peer(&stream)?;
        let actual = crate::engine::process_owner::Owner::capture(&stream)?;
        if owner.process.as_ref() != Some(&actual.identity) {
            return Err("The app endpoint has another process identity; no action was sent".into());
        }
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let after = match &request.operation {
            Operation::Logs { after } => *after,
            _ => None,
        };
        let operation = match &request.operation {
            Operation::Frame => 0,
            Operation::Logs { .. } => 1,
            Operation::Input { .. } => 2,
            Operation::Quit => 3,
            Operation::Show { .. } => 4,
            Operation::Screenshot => 5,
        };
        ipc::codec::write(&mut stream, &ipc::codec::encode(&request, REQUEST_LIMIT)?)?;
        // No further requests can be sent on this exchange. EOF is normal and
        // affects only this tool request, never the GUI's engine context.
        Ok(Self {
            stream,
            instance: request.instance,
            generation: request.generation,
            after,
            operation,
            #[cfg(target_os = "macos")]
            process: actual.identity,
        })
    }
    #[cfg(target_os = "macos")]
    pub fn exit_watch(&self) -> Result<ExitStatusWatch> {
        ExitStatusWatch::new(&self.process)
    }
    pub fn cancellation_handle(&self) -> Result<UnixStream> {
        Ok(self.stream.try_clone()?)
    }
    #[allow(
        unsafe_code,
        reason = "A readiness wait borrows this exchange's initialized socket descriptor."
    )]
    pub fn receive(mut self) -> Result<Reply> {
        if self.operation == 1 {
            use std::os::fd::AsRawFd;
            let mut event =
                libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            loop {
                // SAFETY: the one descriptor remains owned across this wait.
                if unsafe { libc::poll(&mut event, 1, -1) } > 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
        let limit = if self.operation == 5 { SCREENSHOT_REPLY_LIMIT } else { REPLY_LIMIT };
        let bytes = ipc::codec::read(&mut self.stream, limit)?;
        let archived = ipc::codec::access::<ArchivedReply>(&bytes)?;
        // Validate collection bounds before allocating an owned reply.
        match archived {
            ArchivedReply::Frame { build, scene, .. }
                if crate::identifier::valid(build.as_str(), 32)
                    && scene.len() <= MAX_SCENE_BYTES => {}
            ArchivedReply::Logs(batch)
                if batch.lines.len() <= 256
                    && batch
                        .lines
                        .iter()
                        .try_fold(0usize, |n, line| n.checked_add(line.message.len()))
                        .is_some_and(|n| n <= 64 * 1024) => {}
            ArchivedReply::Screenshot { png, .. }
                if self.operation == 5
                    && png.len() <= MAX_SCREENSHOT_BYTES
                    && png.as_slice().starts_with(b"\x89PNG\r\n\x1a\n") => {}
            ArchivedReply::Accepted { .. } if (2..=4).contains(&self.operation) => {}
            ArchivedReply::Error { error } if error.len() <= 4096 => {
                return Err(error.as_str().into());
            }
            _ => return Err("Invalid app control reply bounds".into()),
        }
        let reply = rkyv::api::low::deserialize::<Reply, rkyv::rancor::Error>(archived)?;
        let (instance, generation, operation) = match &reply {
            Reply::Frame { instance, generation, .. } => (*instance, *generation, 0),
            Reply::Screenshot { instance, generation, .. } => (*instance, *generation, 5),
            Reply::Logs(batch) => {
                batch.validate(self.after)?;
                (batch.cursor.instance, batch.cursor.generation, 1)
            }
            Reply::Accepted { instance, generation } => (*instance, *generation, self.operation),
            Reply::Error { .. } => unreachable!("checked above"),
        };
        let log_reset = matches!(&reply, Reply::Logs(batch) if self.after.is_some() && batch.reset && generation > self.generation);
        if instance != self.instance
            || (generation != self.generation && !log_reset)
            || operation != self.operation
        {
            return Err("The reply has another session-instance binding or operation".into());
        }
        Ok(reply)
    }
}
