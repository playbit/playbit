//! Process-wide engine panic policy. Guest traps are ordinary runtime errors;
//! invariant failures cannot be contained by unwinding an arbitrary engine task.
#[cfg(all(not(target_arch = "wasm32"), any(test, feature = "local-store")))]
use crate::trace::diagnostic::REPORT_BYTES;
#[cfg(not(target_arch = "wasm32"))]
use crate::trace::diagnostic::Report;
#[cfg(target_arch = "wasm32")]
use std::fmt;
use std::{fmt::Write as _, panic::PanicHookInfo, sync::Once};

#[cfg(target_arch = "wasm32")]
const REPORT_BYTES: usize = 8192;
pub const MESSAGE: &str = "Playbit stopped unexpectedly. Restart Playbit to continue. The last action may have completed and will not be repeated automatically.";
static INSTALL: Once = Once::new();
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub(crate) mod reports;
#[cfg(target_arch = "wasm32")]
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A JS catch/finally may run before the page receives the worker's fatal notice.
/// It must never re-enter poisoned Wasm state, even during attempted cleanup.
#[cfg(target_arch = "wasm32")]
pub(crate) fn guard() {
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::abort();
    }
}

#[cfg(target_arch = "wasm32")]
struct Report {
    bytes: [u8; REPORT_BYTES],
    length: usize,
}
#[cfg(target_arch = "wasm32")]
impl Report {
    fn new() -> Self {
        Self { bytes: [0; REPORT_BYTES], length: 0 }
    }
    fn text(&mut self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length])
            .expect("report copies complete UTF-8 prefixes")
    }
}
#[cfg(target_arch = "wasm32")]
impl fmt::Write for Report {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut length = text.len().min(REPORT_BYTES - self.length);
        while !text.is_char_boundary(length) {
            length -= 1;
        }
        self.bytes[self.length..self.length + length].copy_from_slice(&text.as_bytes()[..length]);
        self.length += length;
        if length < text.len() { Err(fmt::Error) } else { Ok(()) }
    }
}
fn report(info: &PanicHookInfo<'_>) -> Report {
    let mut report = Report::new();
    let _ = writeln!(report, "panic: {MESSAGE}");
    // Bound the payload separately, leaving room for the backtrace.
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    let mut end = message.len().min(2048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let _ = writeln!(report, "{} at {:?}", &message[..end], info.location());
    let _ = writeln!(report, "{}", std::backtrace::Backtrace::force_capture());
    report
}

/// Local diagnostics only. Call on a background owner, never during rendering.
/// Keep payloads bounded; callers pass operation names, not request contents or keys.
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub fn log(profile: &std::path::Path, operation: &str, error: &str) {
    diagnostic(profile, operation, error, None);
}

/// The owner is terminating the process. Preserve a backtrace before it exits.
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub fn critical(profile: &std::path::Path, operation: &str, error: &str) {
    diagnostic(profile, operation, error, Some(&std::backtrace::Backtrace::force_capture()));
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub(crate) fn diagnostic(
    profile: &std::path::Path,
    operation: &str,
    error: &str,
    origin: Option<&dyn std::fmt::Display>,
) {
    let mut report = Report::new();
    let mut end = error.len().min(REPORT_BYTES);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    let _ = writeln!(report, "{operation}: {}", &error[..end]);
    if let Some(origin) = origin {
        let _ = writeln!(report, "{origin}");
    }
    append_log(profile, report.text());
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
/// Record the release once when this process enters its runtime, before diagnostics.
pub fn startup(profile: &std::path::Path) {
    static STARTUP: Once = Once::new();
    STARTUP.call_once(|| log(profile, "startup", &format!("Playbit {}", crate::VERSION)));
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
fn append_log(profile: &std::path::Path, report: &str) {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt};
    #[cfg(feature = "desktop")]
    if crate::desktop::process_log::installed() {
        let _ = std::io::stderr().write_all(report.as_bytes());
        return;
    }
    let result = (|| -> std::io::Result<()> {
        // Diagnostics may precede durable profile initialization.
        use std::os::unix::fs::DirBuilderExt;
        let path = crate::paths::Paths::diagnostic_log(profile)?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(profile)?;
        crate::paths::private_directory(profile, "logs")?;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        log.write_all(report.as_bytes())
    })();
    if let Err(error) = result {
        crate::diagnostic!("Cannot write Playbit diagnostic log: {error}");
        let _ = std::io::stderr().write_all(report.as_bytes());
    }
}

/// Install once, before starting engine workers or entering engine state. The
/// sink records only this bounded report and must never call into the engine.
/// Returning from it always aborts, even when a caller wraps work in catch_unwind.
pub fn install(sink: impl Fn(&str) + Send + Sync + 'static) {
    INSTALL.call_once(|| {
        // Release text stays in the startup/panic log. Native definitions carry
        // the exact executable identity; names remain a fixed bounded catalog.
        crate::trace::mark("process.start");
        std::panic::set_hook(Box::new(move |info| {
            #[cfg(target_arch = "wasm32")]
            FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut report = report(info);
            sink(report.text());
            std::process::abort();
        }))
    });
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
static NATIVE_PROFILE: std::sync::Mutex<
    Option<(std::path::PathBuf, Option<crate::release_channel::ReleaseChannel>)>,
> = std::sync::Mutex::new(None);

/// Embedded hosts may rebind only after retiring every old runtime owner.
/// The panic hook never waits on this separate, short-lived context lock.
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub fn install_native(profile: &std::path::Path) {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt};
    let channel = report_channel(profile);
    *NATIVE_PROFILE.lock().expect("native panic context lock") =
        Some((profile.to_owned(), channel));
    install(|report| {
        let context = NATIVE_PROFILE.try_lock().ok().and_then(|context| context.clone());
        let Some((profile, channel)) = context else {
            let _ = std::io::stderr().write_all(report.as_bytes());
            return;
        };
        let _ = crate::paths::private_directory(&profile, "logs");
        let _ = crate::paths::private_directory(&profile, "control");
        // Desktop owners use their process log; engine and embedded hosts keep
        // the profile engine log. No lock on failed engine state is acquired.
        append_log(&profile, report);
        retain_report(&profile, channel, report);
        // Fixed-size next-launch notice; no replay payload and no stack in UI.
        if let Ok(mut marker) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(profile.join("control/engine-crash"))
        {
            let _ = marker.write_all(b"panic\n");
            let _ = marker.sync_all();
        }
    });
}

/// GUI invariant failures abort only their owner. Never write the engine's
/// shared crash marker from an independent guest process.
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub fn install_gui(profile: &std::path::Path, root: crate::store::Id) {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt};
    startup(profile);
    let path = profile.join("logs").join(format!("gui-crash-{root}-{}", std::process::id()));
    let channel = report_channel(profile);
    let profile = profile.to_owned();
    install(move |report| {
        append_log(&profile, report);
        retain_report(&profile, channel, report);
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            let _ = log.write_all(report.as_bytes());
            let _ = log.sync_all();
        }
    });
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
fn report_channel(profile: &std::path::Path) -> Option<crate::release_channel::ReleaseChannel> {
    match crate::engine::profile::for_profile(profile) {
        Ok(channel) => Some(channel),
        Err(error) => {
            crate::diagnostic!("Cannot bind panic reports to this profile: {error}");
            None
        }
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
fn retain_report(
    profile: &std::path::Path,
    channel: Option<crate::release_channel::ReleaseChannel>,
    report: &str,
) {
    if let Some(channel) = channel
        && let Err(error) = reports::write(profile, channel, report)
    {
        crate::diagnostic!("Cannot retain a panic report: {error}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    #[cfg(feature = "local-store")]
    #[test]
    fn startup_diagnostics_do_not_initialize_profile_identity() {
        // Logging a startup error writes its report without initializing data.
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("absent-profile");
        log(&profile, "startup fixture", "missing engine helper");
        assert!(!profile.join(crate::paths::PROFILE_DB).exists());
        let path = crate::paths::Paths::diagnostic_log(&profile).unwrap();
        let report = std::fs::read_to_string(path).unwrap();
        assert!(report.contains("startup fixture"));
        assert!(report.contains("missing engine helper"));
    }

    #[cfg(feature = "local-store")]
    #[test]
    fn ordinary_errors_share_the_engine_log_with_bounded_diagnostics() {
        // Routine errors retain bounded UTF-8 details without capturing a fatal backtrace.
        let profile = tempfile::tempdir().unwrap();
        let details = format!("{}last guest frame", "é".repeat(16 * 1024));
        log(profile.path(), "app owner stopped", &details);
        let path = crate::paths::Paths::for_profile(profile.path()).unwrap().engine_log;
        let report = std::fs::read_to_string(&path).unwrap();
        assert!(report.contains(&details));
        assert!(report.len() <= 64 * 1024);
        std::fs::remove_file(&path).unwrap();
        log(profile.path(), "fixture operation", &"é".repeat(REPORT_BYTES));
        let report = std::fs::read_to_string(&path).unwrap();
        assert!(report.len() > 63 * 1024 && report.len() <= 64 * 1024);
        assert!(report.contains("fixture operation"));
        assert!(report.starts_with('['));
        assert!(report.contains(&format!(" {}] fixture operation: ", std::process::id())));
        assert!(!report.contains("backtrace"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn report_stops_at_its_utf8_byte_bound() {
        let mut report = Report::new();
        assert!(write!(report, "{}", "é".repeat(REPORT_BYTES)).is_err());
        assert!(report.text().len() <= REPORT_BYTES);
        assert!(write!(report, "more").is_err());
    }
    #[test]
    fn panic_aborts_before_catch_can_resume_partial_state() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "PLAYBIT_W7_PANIC_CHILD";
        if let Some(directory) = std::env::var_os(CHILD) {
            let path = std::path::PathBuf::from(directory);
            startup(&path);
            startup(&path);
            install_native(&path);
            let state = std::sync::Mutex::new(vec![1, 2]);
            let _ = std::panic::catch_unwind(|| {
                state.lock().unwrap().pop();
                panic!("w7 partial shared state");
            });
            std::fs::write(path.join("continued"), b"unsafe").unwrap();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::failure::tests::panic_aborts_before_catch_can_resume_partial_state",
                "--nocapture",
            ])
            .env(CHILD, directory.path())
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGABRT));
        assert!(!directory.path().join("continued").exists());
        assert_eq!(
            std::fs::read(directory.path().join("control/engine-crash")).unwrap(),
            b"panic\n"
        );
        let log_path = crate::paths::Paths::for_profile(directory.path()).unwrap().engine_log;
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.len() <= 2 * REPORT_BYTES, "one startup record and one bounded panic record");
        assert!(log.contains("w7 partial shared state"));
        assert_eq!(log.matches(crate::VERSION).count(), 1, "version is logged once at startup");
        assert!(log.lines().all(|line| line.starts_with('[')));
        assert!(log.contains("failure::"), "backtrace must be captured without RUST_BACKTRACE");
        let queue =
            reports::Queue::open(directory.path(), crate::release_channel::ReleaseChannel::Public)
                .unwrap();
        assert_eq!(queue.names().len(), 1);
        let report = queue.read(queue.names()[0]).unwrap();
        assert!(report.report.contains("w7 partial shared state"));
        assert!(report.account_id.is_none());
        assert_eq!(report.build, crate::VERSION);
        std::fs::remove_file(log_path).unwrap();
    }

    #[test]
    fn retired_native_profile_cannot_receive_the_next_channels_panic() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "PLAYBIT_PANIC_REBIND_CHILD";
        if let Some(directory) = std::env::var_os(CHILD) {
            let directory = std::path::PathBuf::from(directory);
            install_native(&directory.join("public"));
            install_native(&directory.join("testing"));
            panic!("panic after retired profile");
        }
        let directory = tempfile::tempdir().unwrap();
        let testing = directory.path().join("testing");
        crate::engine::profile::prepare(&testing, crate::release_channel::ReleaseChannel::Testing)
            .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::failure::tests::retired_native_profile_cannot_receive_the_next_\
                 channels_panic",
            ])
            .env(CHILD, directory.path())
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGABRT));
        assert!(!directory.path().join("public").exists());
        let queue = reports::Queue::open(&testing, crate::release_channel::ReleaseChannel::Testing)
            .unwrap();
        assert_eq!(queue.names().len(), 1);
        assert!(
            queue.read(queue.names()[0]).unwrap().report.contains("panic after retired profile")
        );
        assert!(testing.join("control/engine-crash").exists());
    }

    #[test]
    fn gui_panic_does_not_mark_the_shared_engine_as_failed() {
        use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
        const CHILD: &str = "PLAYBIT_GUI_PANIC_CHILD";
        if let Some(directory) = std::env::var_os(CHILD) {
            #[cfg(feature = "desktop")]
            crate::desktop::process_log::initialize(std::path::Path::new(&directory))
                .expect("GUI process log");
            install_gui(std::path::Path::new(&directory), crate::store::Id::from_bytes([9; 16]));
            panic!("isolated GUI invariant failure");
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::engine::profile::prepare(
            directory.path(),
            crate::release_channel::ReleaseChannel::Testing,
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::failure::tests::gui_panic_does_not_mark_the_shared_engine_as_failed",
                "--nocapture",
            ])
            .env(CHILD, directory.path())
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGABRT));
        assert!(!directory.path().join("control/engine-crash").exists());
        let reports: Vec<_> = std::fs::read_dir(directory.path().join("logs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("gui-crash-"))
            .collect();
        assert_eq!(reports.len(), 1);
        let report = std::fs::read_to_string(&reports[0]).unwrap();
        assert!(report.len() <= REPORT_BYTES);
        assert!(report.contains("isolated GUI invariant failure"));
        let queue = self::reports::Queue::open(
            directory.path(),
            crate::release_channel::ReleaseChannel::Testing,
        )
        .unwrap();
        let report = queue.read(queue.names()[0]).unwrap();
        assert_eq!(report.channel, crate::release_channel::ReleaseChannel::Testing);
        assert!(report.account_id.is_none());
    }
}
