//! Readiness from the process that initialized the endpoint; never probe on a timer.
use crate::engine::{Result, ipc, process_identity::Peer};
use std::{
    fs::{self, OpenOptions, TryLockError},
    io::{self, Read},
    os::{
        fd::OwnedFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Locate the engine shipped with a client. Packaged Mac clients always use
/// the sole helper bundle; unbundled Cargo/Linux products use a sibling binary.
pub fn executable_for(client: &Path) -> Result<PathBuf> {
    let directory = client.parent().ok_or("Missing installation directory")?;
    #[cfg(target_os = "macos")]
    if directory.file_name().is_some_and(|name| name == "MacOS")
        && let Some(contents) = directory.parent()
        && contents.file_name().is_some_and(|name| name == "Contents")
    {
        return Ok(contents.join("Helpers/playbit-engine.app/Contents/MacOS/playbit-engine"));
    }
    Ok(directory.join("playbit-engine"))
}

enum Endpoint {
    Absent,
    Ready,
    Replace(Peer),
}

#[derive(Debug)]
struct FailedChild {
    child: Option<Child>,
    error: Box<dyn std::error::Error + Send + Sync>,
}
impl std::fmt::Display for FailedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for FailedChild {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Keep failed initialization children until the recovery owner can reap them.
/// EOF can arrive just before exit; dropping that handle leaks a zombie.
pub fn retain(result: Result<Option<Child>>, child: &mut Option<Child>) -> Result<()> {
    match result {
        Ok(process) => {
            *child = process;
            Ok(())
        }
        Err(mut error) => {
            if let Some(failure) = error.downcast_mut::<FailedChild>() {
                *child = failure.child.take();
            }
            Err(error)
        }
    }
}

pub(crate) fn connect_endpoint(profile: &Path) -> Result<Option<UnixStream>> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?) {
        Ok(()) => Ok(Some(socket.into())),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn endpoint(profile: &Path, executable: &Path, replace: bool) -> Result<Endpoint> {
    match connect_endpoint(profile)? {
        Some(mut stream) => probe(&mut stream, profile, executable, replace),
        None => Ok(Endpoint::Absent),
    }
}

fn probe(
    stream: &mut UnixStream,
    profile: &Path,
    executable: &Path,
    replace: bool,
) -> Result<Endpoint> {
    match identify(stream, profile, executable, replace) {
        // The accepted process exited between connect and identification,
        // e.g. another launcher is replacing it. That is no endpoint, not a
        // failed launch; the launch lock in `ensure` decides who continues.
        Err(error) if peer_vanished(&*error) => Ok(Endpoint::Absent),
        result => result,
    }
}

fn identify(
    stream: &mut UnixStream,
    profile: &Path,
    executable: &Path,
    replace: bool,
) -> Result<Endpoint> {
    #[cfg(not(target_os = "macos"))]
    let _ = profile;
    ipc::verify_peer(stream)?;
    // Standalone tools can use an already-compatible engine without a
    // bundled helper, but cannot replace one with a missing executable.
    if !executable.is_file() {
        ipc::check_protocol(stream)?;
        return Ok(Endpoint::Ready);
    }
    let peer = Peer::capture(stream)?;
    let protocol = ipc::check_protocol(stream);
    if peer.matches(executable)? {
        protocol?;
        return Ok(Endpoint::Ready);
    }
    peer.require_engine()?;
    if !replace {
        protocol?;
        return Ok(Endpoint::Ready);
    }
    #[cfg(target_os = "macos")]
    if super::profile::is_default_profile(profile, super::profile::for_profile(profile)?)?
        && borrowed_engine(
            executable,
            &peer,
            super::launch_agent::installed_owner(profile)?.as_deref(),
        )?
    {
        protocol?;
        return Ok(Endpoint::Ready);
    }
    Ok(Endpoint::Replace(peer))
}

/// A fresh installation can initially select another copy's environment.
/// Use the kernel's executable path even when both copies have the same UUID.
#[cfg(target_os = "macos")]
fn borrowed_engine(executable: &Path, peer: &Peer, owner: Option<&Path>) -> Result<bool> {
    let path = peer.engine_path()?;
    let app = super::launch_agent::app_for(path);
    if let Some(owner) = owner
        && app != Some(owner)
    {
        return Err(
            "The running Playbit engine does not match this environment’s registered owner.".into(),
        );
    }
    if path == executable.canonicalize()? {
        return Ok(false);
    }
    // Older installations have no owner record. Only recognize a foreign
    // packaged helper whose mapped build still matches its executable.
    let app = app.ok_or("The local engine does not belong to a Playbit app installation.")?;
    super::launch_agent::service_identifier(app)?;
    if !peer.matches(path)? {
        return Err("The running Playbit engine no longer matches its installation.".into());
    }
    if owner.is_none() {
        peer.verify_signing_identity(executable)?;
    }
    Ok(true)
}

// Peer credentials, identity options and the protocol reply fail this way once
// the connected process is gone (macOS reports EINVAL from getpeereid and
// ESRCH from proc_pidpath_audittoken when the captured process has exited).
pub(super) fn peer_vanished(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.raw_os_error(),
            Some(libc::ESRCH | libc::EINVAL | libc::ENOTCONN | libc::ECONNRESET | libc::EPIPE)
        ) || matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
        )
    })
}

pub fn ensure(profile: &Path, executable: &Path) -> Result<Option<Child>> {
    start(profile, executable, true, false)
}

/// A surviving client must not replace a healthy engine launched by a newer
/// installation. Reuse a compatible endpoint or report the version conflict.
pub fn recover(profile: &Path, executable: &Path) -> Result<Option<Child>> {
    start(profile, executable, false, false)
}

/// The desktop can ask Launch Services to start an isolated background helper.
/// Cold starts from command-line tools retain direct child startup.
#[cfg(all(target_os = "macos", feature = "native-runtime", feature = "desktop-ui"))]
pub(crate) fn start_for_desktop(
    profile: &Path,
    executable: &Path,
    recovering: bool,
) -> Result<Option<Child>> {
    start(profile, executable, !recovering, true)
}

fn start(profile: &Path, executable: &Path, replace: bool, desktop: bool) -> Result<Option<Child>> {
    #[cfg(not(target_os = "macos"))]
    let _ = desktop;
    // Standalone tools may reuse a compatible engine without a local helper.
    // Discover an installation only when there is an executable to resolve.
    #[cfg(target_os = "macos")]
    if executable.is_file()
        && let Some(installation) = super::installation::Installation::for_executable(executable)?
    {
        installation.register_profile(profile)?;
    }
    let testing = super::testing_profile::from_environment()?;
    super::testing_profile::validate(profile, testing.as_ref())?;
    super::sign_out::resume(profile)?;
    if matches!(endpoint(profile, executable, replace)?, Endpoint::Ready) {
        return Ok(None);
    }
    if !executable.is_file() {
        crate::platform::failure::log(
            profile,
            "engine helper missing",
            &format!("executable={}", executable.display()),
        );
        return Err("The Playbit installation is incomplete. Reinstall Playbit.".into());
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(profile)?;
    // Control resources may precede durable profile initialization. Only
    // launchers hold this lock, never the running engine.
    let paths = crate::paths::Paths::for_profile(profile)?;
    let _launch = match try_launch_lock(profile, &paths)? {
        Some(launch) => launch,
        None if matches!(endpoint(profile, executable, replace)?, Endpoint::Ready) => {
            return Ok(None);
        }
        None => return Err(START_IN_PROGRESS.into()),
    };
    // Recheck after acquiring the launch lock: another launcher may have
    // already replaced the process seen by the initial probe.
    match endpoint(profile, executable, replace)? {
        Endpoint::Ready => return Ok(None),
        Endpoint::Replace(peer) => {
            let _timing = crate::trace::Span::new("engine.replace_exit");
            peer.stop(Duration::from_secs(3))?;
        }
        Endpoint::Absent => (),
    }
    #[cfg(target_os = "macos")]
    if super::launch_agent::start(profile, executable, desktop && testing.is_none())? {
        return match endpoint(profile, executable, replace)? {
            Endpoint::Ready => Ok(None),
            _ => Err("Playbit's background service exited before accepting requests".into()),
        };
    }
    start_child(profile, executable, replace, &paths)
}

const START_IN_PROGRESS: &str =
    "Playbit is already starting for this profile. Reopen the app when that launch finishes.";

fn try_launch_lock(profile: &Path, paths: &crate::paths::Paths) -> Result<Option<fs::File>> {
    crate::paths::private_directory(profile, "control")?;
    let launch = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&paths.launch_lock)?;
    launch.set_permissions(fs::Permissions::from_mode(0o600))?;
    match launch.try_lock() {
        Ok(()) => Ok(Some(launch)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// A different installation's extension can serve Finder. Authenticate the
/// domain owner's running engine or wake its already registered channel job.
/// The stored engine path is used for identity comparison only.
#[cfg(target_os = "macos")]
pub fn recover_provider_owner(profile: &Path, owner_engine: &Path) -> Result<()> {
    let channel = super::file_provider::require_system_profile(profile)?;
    if provider_owner_running(profile, owner_engine)? {
        return Ok(());
    }
    let paths = crate::paths::Paths::for_profile(profile)?;
    let launch = try_launch_lock(profile, &paths)?;
    if provider_owner_running(profile, owner_engine)? {
        return Ok(());
    }
    let _launch = launch.ok_or(START_IN_PROGRESS)?;
    super::launch_agent::start_registered(
        profile,
        owner_engine,
        channel,
        &std::env::current_exe()?,
    )?;
    if !provider_owner_running(profile, owner_engine)? {
        return Err("Playbit's background service exited before accepting requests".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn provider_owner_running(profile: &Path, owner_engine: &Path) -> Result<bool> {
    let Some(stream) = connect_endpoint(profile)? else { return Ok(false) };
    Peer::capture_at(&stream, Some(owner_engine))?;
    Ok(true)
}

/// Check the destination before an explicit switch stops the source engine.
#[cfg(all(target_os = "macos", feature = "desktop"))]
pub(crate) fn check_switch_destination(
    channel: crate::release_channel::ReleaseChannel,
    executable: &Path,
) -> Result<()> {
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    let destination = super::profile::default_path(&home, channel);
    let state = super::profile::default_runtime_path(&home, channel);
    check_switch_owner(&state, executable)?;
    if let Some(stream) = connect_endpoint(&destination)? {
        Peer::capture_at(&stream, Some(&executable.canonicalize()?))?;
    }
    Ok(())
}

#[cfg(all(target_os = "macos", any(feature = "desktop", test)))]
fn check_switch_owner(state: &Path, executable: &Path) -> Result<()> {
    if let Some(owner) = super::launch_agent::read_owner(state)?
        && owner.try_exists()?
        && executable_for(&owner.join("Contents/MacOS/Playbit"))? != executable.canonicalize()?
    {
        return Err(
            "Another installation manages this environment. Open that Playbit app to use it."
                .into(),
        );
    }
    Ok(())
}

/// Hold startup admission while ServiceManagement retires a drained job. A
/// foreign extension can request a cold launch without the updater's client
/// lease, so the per-profile launch lock also covers this cutover.
#[cfg(target_os = "macos")]
pub(super) fn drained_service(profile: &Path) -> Result<Option<fs::File>> {
    if !profile.try_exists()? {
        return Ok(None);
    }
    let paths = crate::paths::Paths::for_profile(profile)?;
    let lock = try_launch_lock(profile, &paths)?.ok_or(START_IN_PROGRESS)?;
    if connect_endpoint(profile)?.is_some() {
        return Err(
            "Close the running Playbit engine before retiring its background service.".into()
        );
    }
    Ok(Some(lock))
}

/// Drain only this installation's engines. Return whether the caller can
/// retire a managed source job, subject to the job's own locked owner check.
#[cfg(all(target_os = "macos", feature = "desktop"))]
pub(crate) fn retire_profile_engine(
    profile: &Path,
    executable: &Path,
    channel: crate::release_channel::ReleaseChannel,
) -> Result<bool> {
    let default = super::profile::is_default_profile(profile, channel)?;
    let Some(stream) = connect_endpoint(profile)? else { return Ok(default) };
    if default {
        return retire_owned_engine(
            stream,
            executable,
            super::launch_agent::installed_owner(profile)?.as_deref(),
        );
    }
    let peer = Peer::capture_at(&stream, Some(&executable.canonicalize()?))?;
    stop_reconfiguration_peer(stream, peer, executable)?;
    Ok(false)
}

#[cfg(all(target_os = "macos", any(feature = "desktop", test)))]
fn retire_owned_engine(
    stream: UnixStream,
    executable: &Path,
    owner: Option<&Path>,
) -> Result<bool> {
    let peer = Peer::capture(&stream)?;
    if borrowed_engine(executable, &peer, owner)? {
        return Ok(false);
    }
    stop_reconfiguration_peer(stream, peer, executable)?;
    Ok(true)
}

#[cfg(all(target_os = "macos", any(feature = "desktop", test)))]
fn stop_reconfiguration_peer(mut stream: UnixStream, peer: Peer, executable: &Path) -> Result<()> {
    if !peer.matches(executable)? {
        return Err("Another Playbit build owns this profile. \
            Close its apps and commands before reconfiguring Playbit."
            .into());
    }
    ipc::check_protocol(&mut stream)?;
    // This probe is not admitted work. Do not keep its connection alive
    // while the service drains clients during graceful shutdown.
    drop(stream);
    peer.stop(Duration::from_secs(3))?;
    Ok(())
}

fn start_child(
    profile: &Path,
    executable: &Path,
    replace: bool,
    paths: &crate::paths::Paths,
) -> Result<Option<Child>> {
    crate::paths::private_directory(profile, "logs")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&paths.engine_log)?;
    log.set_permissions(fs::Permissions::from_mode(0o600))?;
    let (mut ready, child_output) = UnixStream::pair()?;
    let output: OwnedFd = child_output.into();
    let mut child = Command::new(executable)
        .env("PLAYBIT_PROFILE", profile)
        .env("PLAYBIT_CHANNEL", super::profile::for_profile(profile)?.name())
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(log)
        .spawn()?;
    if let Err(error) = read_ready(&mut ready, Instant::now() + Duration::from_secs(10)) {
        // Reap an already-failed child, but never block waiting for a process
        // that is still initializing or cancel an in-flight durable write.
        let status = child.try_wait();
        crate::platform::failure::log(
            profile,
            "engine readiness failure",
            &format!(
                "executable={} child_pid={} status={status:?} error={error:?}",
                executable.display(),
                child.id()
            ),
        );
        return Err(Box::new(FailedChild { child: Some(child), error }));
    }
    match endpoint(profile, executable, replace) {
        Ok(Endpoint::Ready) => (),
        result => {
            let error = result.err().unwrap_or_else(|| {
                "Playbit exited before its local endpoint became available.".into()
            });
            return Err(Box::new(FailedChild { child: Some(child), error }));
        }
    }
    crate::trace::mark("ui.engine_ready_received");
    Ok(Some(child))
}

pub(super) fn read_ready(stream: &mut UnixStream, deadline: Instant) -> Result<()> {
    // A failed helper may have exited while its diagnostic is still buffered.
    // macOS can reject SO_RCVTIMEO after peer exit; use descriptor readiness.
    stream.set_nonblocking(true)?;
    let mut bytes = [0; ipc::ENGINE_READY.len()];
    read_startup_bytes(stream, &mut bytes, deadline)?;
    if bytes == ipc::ENGINE_READY {
        return Ok(());
    }
    if bytes != ipc::ENGINE_FAILED {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local engine readiness result",
        )
        .into());
    }
    let mut length = [0; 4];
    read_startup_bytes(stream, &mut length, deadline)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > super::failure::MAX_STARTUP_FAILURE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid engine startup diagnostic length",
        )
        .into());
    }
    let mut bytes = vec![0; length];
    read_startup_bytes(stream, &mut bytes, deadline)?;
    let failure: super::failure::StartupFailure = serde_json::from_slice(&bytes)?;
    Err(Box::new(failure))
}

fn read_startup_bytes(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if !super::readiness::wait_readable(stream, deadline).map_err(io::Error::other)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "engine readiness deadline expired",
            ));
        }
        match stream.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "engine exited before readiness",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::{io::Write, thread};

    #[test]
    fn failed_readiness_retains_a_child_that_has_not_exited_yet() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().join("profile");
        let engine = fixture.path().join("playbit-engine");
        fs::write(&engine, "#!/bin/sh\nexec 1>&-\nsleep 0.3\nexit 23\n").unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = None;
        let error = retain(ensure(&profile, &engine), &mut child).unwrap_err();
        assert!(error.to_string().contains("engine exited before readiness"));
        assert!(child.as_mut().unwrap().try_wait().unwrap().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.is_some() {
            assert!(Instant::now() < deadline);
            super::super::restart::reap(&mut child, &profile);
            thread::sleep(Duration::from_millis(1));
        }
        let path = crate::paths::Paths::for_profile(&profile).unwrap().engine_log;
        assert!(fs::read_to_string(&path).unwrap().contains("status=exit status: 23"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn client_layout_selects_one_engine_without_filesystem_fallback() {
        // A missing packaged helper must not redirect to a stray flat executable.
        for tool in ["Playbit", "pb", "playbit-mcp"] {
            let bare = Path::new("/installation").join(tool);
            assert_eq!(executable_for(&bare).unwrap(), Path::new("/installation/playbit-engine"));
            #[cfg(target_os = "macos")]
            {
                let contents = Path::new("/installation/Playbit 雪.app/Contents");
                assert_eq!(
                    executable_for(&contents.join("MacOS").join(tool)).unwrap(),
                    contents.join("Helpers/playbit-engine.app/Contents/MacOS/playbit-engine")
                );
            }
        }
    }

    #[test]
    fn corrupt_open_has_a_distinct_launch_result() {
        // The failure frame preserves detail while native UI retains its terminal category.
        assert_eq!(ipc::ENGINE_READY.len(), ipc::ENGINE_FAILED.len());
        let (mut receiver, mut sender) = UnixStream::pair().unwrap();
        let directory = tempfile::tempdir().unwrap();
        super::super::failure::write_startup_failure(
            directory.path(),
            &crate::store::Error::corrupt("account record is missing"),
            &mut sender,
        )
        .unwrap();
        let error = read_ready(&mut receiver, Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.to_string(), ipc::CORRUPT_STORE_ERROR);
        let wrapped = FailedChild { child: None, error };
        let text = super::super::failure::cli_message(&wrapped);
        assert!(text.contains("account record is missing"));
        assert!(text.contains("PLAYBIT_PROFILE="));
    }

    #[test]
    fn failed_profile_open_is_terminal_without_reset() {
        assert_eq!(ipc::ENGINE_READY.len(), ipc::ENGINE_FAILED.len());
        let (mut receiver, mut sender) = UnixStream::pair().unwrap();
        let directory = tempfile::tempdir().unwrap();
        super::super::failure::write_startup_failure(
            directory.path(),
            &crate::store::Error::Credentials("local account key is missing".into()),
            &mut sender,
        )
        .unwrap();
        let error = read_ready(&mut receiver, Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.to_string(), ipc::PROFILE_OPEN_ERROR);
        assert!(super::super::restart::terminal(&error.to_string()));
    }

    #[test]
    fn an_exited_process_is_absent_but_permission_and_identity_errors_fail() {
        // macOS proc_pidpath_audittoken can race with another launcher's
        // replacement after the socket's audit token has been obtained.
        assert!(peer_vanished(&io::Error::from_raw_os_error(libc::ESRCH)));
        for code in [libc::EPERM, libc::EACCES, libc::ENOENT, libc::EIO] {
            assert!(!peer_vanished(&io::Error::from_raw_os_error(code)));
        }
        assert!(!peer_vanished(&io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid local engine process identity"
        )));
    }

    #[test]
    fn fragmented_readiness_is_received_without_waiting_for_process_exit() {
        // A managed engine connects before initialization finishes. macOS
        // accepted sockets inherit the listener's nonblocking mode.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut sender = UnixStream::connect(&path).unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            for byte in ipc::ENGINE_READY {
                sender.write_all(&[*byte]).unwrap();
            }
            sender
        });
        read_ready(&mut receiver, Instant::now() + Duration::from_secs(1)).unwrap();
        // The producer is still open: readiness is not EOF or a polling probe.
        let _sender = writer.join().unwrap();
    }

    #[test]
    fn malformed_and_incomplete_readiness_fail_without_unbounded_buffering() {
        for bytes in [b"".as_slice(), b"{\"ready\":false}", b"diagnostic text before readiness"] {
            let (mut receiver, mut sender) = UnixStream::pair().unwrap();
            sender.write_all(bytes).unwrap();
            drop(sender);
            assert!(read_ready(&mut receiver, Instant::now() + Duration::from_secs(1)).is_err());
        }
    }

    #[test]
    fn failed_readiness_rejects_invalid_lengths_and_truncated_payloads() {
        // Reject lengths before allocation and require the complete diagnostic frame.
        for (length, payload) in [(0, b"".as_slice()), (8193, b""), (8, b"{}"), (2, b"{}")] {
            let (mut receiver, mut sender) = UnixStream::pair().unwrap();
            sender.write_all(ipc::ENGINE_FAILED).unwrap();
            sender.write_all(&u32::to_le_bytes(length)).unwrap();
            sender.write_all(payload).unwrap();
            drop(sender);
            assert!(read_ready(&mut receiver, Instant::now() + Duration::from_secs(1)).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn explicit_switch_preserves_an_idle_owner_and_allows_a_relocated_app() {
        let directory = tempfile::tempdir().unwrap();
        let owner = directory.path().join("Playbit.app");
        let executable = std::env::current_exe().unwrap();
        fs::create_dir(&owner).unwrap();
        super::super::profile::write_state(
            directory.path(),
            "agent-owner",
            owner.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        assert!(check_switch_owner(directory.path(), &executable).is_err());
        fs::rename(&owner, directory.path().join("Relocated.app")).unwrap();
        check_switch_owner(directory.path(), &executable).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn switching_a_borrowed_engine_leaves_the_recorded_and_legacy_owners_running() {
        const SOCKET: &str = "PLAYBIT_BORROWED_ENGINE_TEST_SOCKET";
        if let Some(socket) = std::env::var_os(SOCKET) {
            let mut stream = UnixStream::connect(socket).unwrap();
            // Even a legacy engine with an incompatible IPC protocol can
            // remain running when a borrowed profile's client goes away.
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            assert_eq!(std::io::stdin().read(&mut [0]).unwrap(), 0);
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let app = root.join("Playbit.app");
        let engine = executable_for(&app.join("Contents/MacOS/Playbit")).unwrap();
        fs::create_dir_all(engine.parent().unwrap()).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            b"<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key>\
              <string>playbit.engine</string></dict></plist>",
        )
        .unwrap();
        let client = std::env::current_exe().unwrap();
        fs::copy(&client, &engine).unwrap();
        let socket = root.join("peer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let other = root.join("Other.app");
        for (owner, originating_engine, allowed) in [
            (Some(app.as_path()), client.as_path(), true),
            (None, client.as_path(), true),
            (None, Path::new("/bin/echo"), false),
            (Some(other.as_path()), client.as_path(), false),
        ] {
            let mut command = Command::new(&engine);
            command
                .args([
                    "--exact",
                    "engine::launch::tests::switching_a_borrowed_engine_\
                     leaves_the_recorded_and_legacy_owners_running",
                ])
                .env(SOCKET, &socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::null());
            let mut child = command.spawn().unwrap();
            assert!(
                super::super::readiness::wait_readable(
                    &listener,
                    Instant::now() + Duration::from_secs(5)
                )
                .unwrap()
            );
            let (stream, _) = listener.accept().unwrap();
            let result = retire_owned_engine(stream, originating_engine, owner);
            if allowed {
                assert!(!result.unwrap());
            } else {
                assert!(result.is_err());
            }
            assert!(child.try_wait().unwrap().is_none());
            drop(child.stdin.take());
            assert!(child.wait().unwrap().success());
        }
        let (client_stream, _server) = UnixStream::pair().unwrap();
        assert!(retire_owned_engine(client_stream, &client, None).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn provider_recovery_recognizes_the_running_owner_without_a_wire_exchange() {
        // The extension registered by macOS may come from another build.
        // Compare kernel paths even when a second copy has the same build UUID.
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path();
        crate::paths::private_directory(profile, "control").unwrap();
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert!(!provider_owner_running(profile, &executable).unwrap());
        let _listener =
            std::os::unix::net::UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET))
                .unwrap();
        assert!(provider_owner_running(profile, &executable).unwrap());
        let other = profile.join("other-installation");
        std::os::unix::fs::symlink(&executable, &other).unwrap();
        assert!(provider_owner_running(profile, &other).is_err());
    }

    #[test]
    fn a_peer_that_exits_during_the_probe_is_no_endpoint() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        crate::paths::private_directory(directory.path(), "control").unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let mut stream =
            UnixStream::connect(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        // Accept and close before identification, as an engine being replaced
        // by another launcher does between this launcher's connect and probe.
        drop(listener.accept().unwrap());
        drop(listener);
        for executable in [Path::new("/nonexistent"), std::env::current_exe().unwrap().as_path()] {
            match probe(&mut stream, directory.path(), executable, true) {
                Ok(Endpoint::Absent) => (),
                Ok(_) => panic!("a closed peer is not an endpoint"),
                Err(error) => panic!("closed peer reported as failure: {error}"),
            }
        }
    }

    #[test]
    fn readiness_deadline_reports_failure() {
        let (mut receiver, _sender) = UnixStream::pair().unwrap();
        receiver.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let error =
            read_ready(&mut receiver, Instant::now() + Duration::from_millis(20)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
    }
}
