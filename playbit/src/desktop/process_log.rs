//! Desktop process diagnostics, separate from guest output and the engine log.
//! Open once after profile selection, before workers. Stderr remains an append-only
//! file descriptor, including during panic/exit; there is no buffered log queue.
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static LOGGER: Logger = Logger;
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            crate::diagnostic!("{} {}: {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

pub(crate) fn installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

fn open(profile: &Path) -> io::Result<File> {
    let directory = crate::paths::private_directory(profile, "logs")?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("playbit-gui.log"))?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("GUI log is not an ordinary file"));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Call once after selecting the desktop profile, before starting any workers.
/// Failure is a launch error: a GUI must not silently run without diagnostics.
#[allow(unsafe_code, reason = "Install an owned log descriptor as inheritable process stderr.")]
pub fn initialize(profile: &Path) -> io::Result<()> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let file = open(profile)?;
    // SAFETY: file owns a live descriptor for this call. dup2 retains its own
    // reference, so stderr remains valid after the local File is dropped.
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if file.as_raw_fd() == libc::STDERR_FILENO {
        // Opening the file may have reused a closed stderr descriptor.
        // SAFETY: file still owns descriptor 2. Match dup2's inheritable result.
        if unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_SETFD, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = file.into_raw_fd();
    }
    INSTALLED.store(true, Ordering::Release);
    log::set_logger(&LOGGER).map_err(io::Error::other)?;
    log::set_max_level(log::LevelFilter::Info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn process_diagnostics_append_and_survive_panic() {
        // Subprocesses exercise the actual stderr descriptor and global logger,
        // including a second launch and the native aborting panic hook.
        const CHILD: &str = "PLAYBIT_GUI_LOG_TEST_PROFILE";
        let guest_frames = format!("{}last guest frame", "guest::frame é\n".repeat(512));
        if let Some(profile) = std::env::var_os(CHILD) {
            let profile = Path::new(&profile);
            initialize(profile).expect("GUI logger");
            crate::platform::failure::startup(profile);
            crate::platform::failure::install_native(profile);
            crate::diagnostic!("diagnostic fixture");
            log::error!("framework error fixture");
            std::io::stderr().write_all(b"raw stderr fixture\n").expect("stderr");
            crate::platform::failure::log(profile, "operation fixture", "error fixture");
            crate::platform::failure::log(profile, "app owner stopped", &guest_frames);
            crate::diagnostic!("CLI-style guest backtrace: {guest_frames}");
            if std::env::var_os("PLAYBIT_GUI_LOG_TEST_PANIC").is_some() {
                panic!("GUI panic fixture");
            }
            return;
        }
        let profile = tempfile::tempdir().expect("profile");
        std::fs::set_permissions(profile.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private profile");
        crate::engine::profile::prepare(
            profile.path(),
            crate::release_channel::ReleaseChannel::Testing,
        )
        .expect("profile channel");
        for panic in [false, true] {
            let mut command =
                std::process::Command::new(std::env::current_exe().expect("test exe"));
            command
                .args([
                    "--exact",
                    "desktop::process_log::tests::process_diagnostics_append_and_survive_panic",
                    "--nocapture",
                ])
                .env(CHILD, profile.path());
            if panic {
                command.env("PLAYBIT_GUI_LOG_TEST_PANIC", "1");
            }
            let output = command.output().expect("child");
            if panic {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(output.status.signal(), Some(libc::SIGABRT));
            } else {
                assert!(output.status.success(), "{output:?}");
            }
            assert!(output.stderr.is_empty(), "all stderr belongs in the profile log");
        }
        let text =
            std::fs::read_to_string(profile.path().join("logs/playbit-gui.log")).expect("GUI log");
        for message in [
            "diagnostic fixture",
            "framework error fixture",
            "raw stderr fixture",
            "operation fixture: error fixture",
        ] {
            assert_eq!(text.matches(message).count(), 2, "{message}: {text}");
        }
        // Both diagnostic paths preserve a multiline report beyond the old 8 KiB ceiling.
        assert_eq!(text.matches("guest::frame é").count(), 4 * 512);
        assert_eq!(text.matches("last guest frame").count(), 4);
        assert_eq!(text.matches("GUI panic fixture").count(), 1);
        assert!(text.contains("startup: Playbit"));
        assert!(!profile.path().join("logs/engine.log").exists());
    }

    #[test]
    fn profile_logs_are_private_append_only_and_reject_symlinks() {
        // Existing content survives reopen; separate profiles never share a sink.
        // Reject an attacker-controlled file or directory alias before writing.
        let first = tempfile::tempdir().expect("first profile");
        let second = tempfile::tempdir().expect("second profile");
        open(first.path()).expect("first open").write_all(b"first\n").expect("first record");
        open(first.path()).expect("reopen").write_all(b"second\n").expect("second record");
        open(second.path()).expect("second profile").write_all(b"other\n").expect("other record");
        let path = first.path().join("logs/playbit-gui.log");
        assert_eq!(std::fs::read(&path).expect("read"), b"first\nsecond\n");
        assert_eq!(std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(&path).expect("remove fixture");
        let target = second.path().join("logs/playbit-gui.log");
        std::os::unix::fs::symlink(&target, &path).expect("file alias");
        assert!(open(first.path()).is_err());
        assert_eq!(std::fs::read(&target).expect("other log"), b"other\n");
        std::fs::remove_file(&path).expect("remove alias");
        std::fs::remove_dir(first.path().join("logs")).expect("remove logs");
        std::os::unix::fs::symlink(second.path().join("logs"), first.path().join("logs"))
            .expect("directory alias");
        assert!(open(first.path()).is_err());
    }
}
