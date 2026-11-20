//! Small bounded event recording. Native guards are local producer conveniences;
//! the recorder retains independent starts, ends and instants without a tree.
mod app;
mod clock;
mod gauge;
#[cfg(any(target_os = "ios", test))]
mod tail_admission;
pub use app::{AppScope, AppTraceError};
pub use gauge::Gauge;
pub mod format;
pub mod identifier;
pub mod input;
pub use format::{AppIdentity, AppNameKind};
#[cfg(unix)]
pub mod diagnostic;
#[macro_export]
macro_rules! diagnostic{($($argument:tt)*)=>{{#[cfg(unix)]$crate::diagnostic::stderr(format_args!($($argument)*));#[cfg(not(unix))]eprintln!($($argument)*);}};}
#[cfg(all(unix, target_has_atomic = "64"))]
mod collector;
#[cfg(all(unix, target_has_atomic = "64"))]
pub mod control;
#[cfg(all(unix, target_has_atomic = "64"))]
mod descriptors;
#[cfg(all(unix, target_has_atomic = "64"))]
mod lease;
#[cfg(all(unix, target_has_atomic = "64"))]
mod log;
#[cfg(all(unix, target_has_atomic = "64"))]
mod notify;
#[cfg(all(unix, target_has_atomic = "64"))]
mod process;
#[cfg(all(unix, target_has_atomic = "64"))]
mod queue;
#[cfg(all(unix, target_has_atomic = "64"))]
mod region;
#[cfg(all(unix, target_has_atomic = "64"))]
mod source;
#[cfg(all(unix, target_has_atomic = "64"))]
pub mod tail;
#[cfg(all(unix, target_has_atomic = "64"))]
pub use collector::{Recorder, attach};
#[cfg(all(unix, target_has_atomic = "64"))]
pub use log::Cursor;
pub fn initialize(kind: u64, names: &[&'static str]) -> std::io::Result<()> {
    #[cfg(all(unix, target_has_atomic = "64"))]
    {
        source::initialize(kind, names)
    }
    #[cfg(not(all(unix, target_has_atomic = "64")))]
    {
        let _ = (kind, names);
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
pub fn clock_ns() -> u64 {
    clock::now_ns()
}
#[inline]
pub fn is_recording() -> bool {
    #[cfg(all(unix, target_has_atomic = "64"))]
    {
        source::SOURCE.get().is_some_and(|s| s.generation() != 0)
    }
    #[cfg(not(all(unix, target_has_atomic = "64")))]
    {
        false
    }
}
pub fn detail_enabled() -> bool {
    is_recording()
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpanId {
    generation: u64,
    tid: u32,
    timestamp: u64,
}
struct Active {
    generation: u64,
    id: SpanId,
}
pub struct Span {
    active: Option<Active>,
}
impl Span {
    #[inline]
    pub fn new(name: &'static str) -> Self {
        Self::related(name, SpanId::default())
    }
    pub fn related(name: &'static str, parent: SpanId) -> Self {
        Self { active: emit(name, 1, parent, 0, None) }
    }
    pub fn detail(name: &'static str) -> Option<Self> {
        detail_enabled().then(|| Self::new(name))
    }
    pub fn id(&self) -> SpanId {
        self.active.as_ref().map_or(SpanId::default(), |a| a.id)
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = emit("", 2, active.id, 0, Some(active.generation));
        }
    }
}
pub struct Interval(Span);
impl Interval {
    pub fn new(name: &'static str) -> Self {
        Self(Span::new(name))
    }
    pub fn related(name: &'static str, parent: SpanId) -> Self {
        Self(Span::related(name, parent))
    }
    pub fn detail(name: &'static str) -> Option<Self> {
        Span::detail(name).map(Self)
    }
    pub fn id(&self) -> SpanId {
        self.0.id()
    }
}
pub fn mark(name: &'static str) {
    mark_value(name, 0);
}
pub fn mark_value(name: &'static str, value: u64) {
    let _ = emit(name, 3, SpanId::default(), value, None);
}
pub fn mark_related(name: &'static str, related: SpanId) {
    let _ = emit(name, 3, related, 0, None);
}
pub fn mark_related_at(name: &'static str, related: SpanId, source_ns: u64) -> bool {
    if !is_recording() || source_ns == 0 || source_ns > clock_ns() {
        return false;
    }
    let _ = emit(name, 3, related, source_ns, None);
    true
}
fn emit(
    name: &'static str,
    kind: u8,
    parent: SpanId,
    value: u64,
    expected: Option<u64>,
) -> Option<Active> {
    #[cfg(all(unix, target_has_atomic = "64"))]
    {
        let source = source::SOURCE.get()?;
        let generation = source.generation();
        if generation == 0 || expected.is_some_and(|g| g != generation) {
            return None;
        }
        if parent.timestamp != 0 && parent.generation != generation {
            source.region.header().lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            source.wake();
            return None;
        }
        let name = if kind == 2 {
            0
        } else {
            match source.name(name) {
                Some(name) => name,
                None => {
                    source.region.header().lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    source.wake();
                    return None;
                }
            }
        };
        let Some((tid, timestamp)) = source.timestamp() else {
            source.region.header().lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            source.wake();
            return None;
        };
        let mut header = playbit_api::abi::trace::TraceEvent {
            timestamp,
            reference_eid: parent.timestamp,
            reference_tid: if parent.tid == tid { 0 } else { parent.tid },
            value,
            name,
            kind,
            ..Default::default()
        };
        // Native cross-thread references are explicit and interpreted by readers.
        if parent.timestamp == 0 {
            header.reference_tid = 0;
        }
        let mut bytes = [0; 40];
        playbit_api::abi::trace::encode_header(header, &mut bytes).expect("native event header");
        source.emit(0, tid, generation, &bytes);
        Some(Active { generation, id: SpanId { generation, tid, timestamp } })
    }
    #[cfg(not(all(unix, target_has_atomic = "64")))]
    {
        let _ = (name, kind, parent, value, expected);
        None
    }
}
/// Profile-relative trace transport endpoints. The host supplies the log destination.
#[cfg(all(unix, target_has_atomic = "64"))]
pub mod profile_paths {
    pub(crate) fn prepare_control(profile: &std::path::Path) -> std::io::Result<()> {
        private_directory(&profile.join("control"))
    }
    pub(crate) fn private_directory(path: &std::path::Path) -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other("trace directory must be private (0700)"));
        }
        Ok(())
    }

    pub const LOCK: &str = "control/trace.lock";
    pub const REGISTER: &str = "control/trace-register.sock";
    pub const CONTROL: &str = "control/trace-control.sock";
    pub const TAIL: &str = "control/trace-tail.sock";
}

#[cfg(all(unix, target_has_atomic = "64"))]
mod atomic {
    pub use std::sync::atomic::*;
}
#[cfg(all(test, unix, target_has_atomic = "64"))]
mod tests;
