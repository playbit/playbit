//! macOS engine lifecycle (tier B). Default profiles use a LaunchAgent for
//! certificate-signed packages and Launch Services for ad-hoc packages. The
//! packaged desktop also uses Launch Services for ordinary isolated profiles.
//! All service control runs on the existing startup lane.
use super::{Result, installation, ipc, process_identity::Peer, readiness::wait_readable};
use crate::release_channel::ReleaseChannel;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const READY_SOCKET: &str = "control/agent-ready.sock";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const AUTHENTICATED: &[u8; 1] = b"A";
// SMAppServiceStatus, as declared by ServiceManagement/SMAppService.h.
const ENABLED: isize = 1;
const REQUIRES_APPROVAL: isize = 2;
const MAX_ERROR_BYTES: usize = 4096;
pub(super) const START_ERROR: &str = "Could not start Playbit's background service. Check Playbit in System Settings > General > Login Items & Extensions, then reopen Playbit.";

fn app_for(executable: &Path) -> Option<&Path> {
    let app = executable.ancestors().nth(6)?;
    (app.extension().is_some_and(|extension| extension == "app")
        && app.join("Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine")
            == executable)
        .then_some(app)
}

fn startup_command(
    profile: &Path,
    executable: &Path,
    allow_isolated: bool,
) -> Result<Option<(Command, bool)>> {
    let Some(app) = app_for(executable) else { return Ok(None) };
    let channel = super::profile::for_profile(profile)?;
    let default_profile = super::profile::is_default_profile(profile, channel)?;
    let mut command = if default_profile {
        let mut command = Command::new(app.join("Contents/MacOS/pb"));
        command.args(["engine-service", "start"]);
        command
    } else if allow_isolated {
        let mut command = Command::new("/usr/bin/open");
        // Each isolated profile needs its own process even when the same
        // helper bundle is already running for another profile.
        command
            .args(["-n", "-g", "-j", "-a"])
            .arg(app.join("Contents/Helpers/playbit-engine.app"))
            .args(["--args", "--launch-isolated-profile"])
            .arg(super::profile::canonical_profile(profile)?)
            .arg(channel.name())
            .env_remove("PLAYBIT_PROFILE")
            .env_remove("PLAYBIT_CHANNEL")
            .env_remove(super::testing_profile::CREDENTIALS_ENV);
        command
    } else {
        return Ok(None);
    };
    command.stdin(Stdio::null()).stdout(Stdio::null());
    Ok(Some((command, default_profile)))
}

/// Readiness comes from the exact initialized helper through a private socket,
/// preserving the existing startup protocol without probing the engine on a timer.
pub(super) fn start(profile: &Path, executable: &Path, allow_isolated: bool) -> Result<bool> {
    let Some((mut command, default_profile)) =
        startup_command(profile, executable, allow_isolated)?
    else {
        return Ok(false);
    };
    let path = profile.join(READY_SOCKET);
    match fs::remove_file(&path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&path)?;
    let _socket = ReadySocket(path);
    fs::set_permissions(&_socket.0, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + START_TIMEOUT;
    if !command.status()?.success() {
        return Err(if default_profile {
            START_ERROR
        } else {
            "macOS could not start the background engine for this Playbit profile."
        }
        .into());
    }
    if !wait_readable(&listener, deadline)? {
        return Err(if default_profile {
            "Playbit's background service did not become ready. Check Playbit in System Settings > General > Login Items & Extensions, then reopen Playbit."
        } else {
            "The background engine for this Playbit profile did not become ready."
        }
        .into());
    }
    let (mut ready, _) = listener.accept()?;
    authenticate(&mut ready, executable, deadline)?;
    super::launch::read_ready(&mut ready, deadline)?;
    Ok(true)
}

fn authenticate(ready: &mut UnixStream, executable: &Path, deadline: Instant) -> Result<()> {
    if !Peer::capture(ready)?.matches(executable)? {
        return Err("Unexpected process connected to Playbit's background startup socket".into());
    }
    let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "engine authentication deadline expired")
    })?;
    ready.set_nonblocking(false)?;
    ready.set_write_timeout(Some(remaining))?;
    ready.write_all(AUTHENTICATED)?;
    Ok(())
}

fn wait_for_authentication(ready: &mut UnixStream, timeout: Duration) -> Result<()> {
    ready.set_read_timeout(Some(timeout))?;
    let mut acknowledgment = [0];
    ready.read_exact(&mut acknowledgment)?;
    if acknowledgment != *AUTHENTICATED {
        return Err("Invalid background engine authentication acknowledgment".into());
    }
    Ok(())
}

struct ReadySocket(PathBuf);
impl Drop for ReadySocket {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            crate::diagnostic!("Cannot remove engine startup socket: {error}");
        }
    }
}

/// Select the helper's startup mode and wire background readiness before
/// opening the store. A bundled helper without a launch intent must not fall
/// back to opening an ordinary profile if Launch Services stripped arguments.
#[allow(
    unsafe_code,
    reason = "dup2 borrows live owned descriptors until both standard streams are installed."
)]
pub fn prepare_engine() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let isolated = match arguments.next().as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--launch-agent") => {
            if arguments.next().is_some() {
                return Err("Invalid background service startup request".into());
            }
            None
        }
        Some("--launch-isolated-profile") => {
            let (Some(profile), Some(channel), None) =
                (arguments.next(), arguments.next(), arguments.next())
            else {
                return Err("Invalid isolated background engine startup request".into());
            };
            if app_for(&std::env::current_exe()?).is_none() {
                return Err("Isolated background startup requires a packaged engine".into());
            }
            let channel = channel.to_str().ok_or("Invalid background engine channel")?;
            Some((PathBuf::from(profile), ReleaseChannel::parse(channel)?))
        }
        _ => {
            if std::env::var_os("PLAYBIT_PROFILE").is_none()
                && app_for(&std::env::current_exe()?).is_some()
            {
                return Err(
                    "The packaged engine requires an explicit Playbit startup request".into()
                );
            }
            return Ok(());
        }
    };
    super::profile::initialize_agent(
        isolated.as_ref().map(|(path, channel)| (path.as_path(), *channel)),
    )?;
    let profile = super::profile_path();
    let mut ready = UnixStream::connect(profile.join(READY_SOCKET))?;
    ipc::verify_peer(&ready)?;
    // A failing initializer may exit immediately after writing its diagnostic.
    // Keep this process alive until the launcher has captured its kernel identity.
    wait_for_authentication(&mut ready, START_TIMEOUT)?;
    crate::paths::private_directory(&profile, "logs")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join("logs/engine.log"))?;
    // SAFETY: the connected stream and log remain live through dup2. These
    // replace only this new engine process's stdout/stderr, before any worker.
    unsafe {
        if libc::dup2(ready.as_raw_fd(), libc::STDOUT_FILENO) < 0
            || libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Executed by the original installation's pb, including when a prepared guest
/// requests startup. SMAppService resolves plists relative to the calling bundle.
/// Unregistration is reserved for callers that have already drained the engines.
pub fn control(start: bool) -> Result<()> {
    let executable = std::env::current_exe()?;
    let _installation = if start { installation::client_lease(&executable)? } else { None };
    let contents = executable.parent().and_then(Path::parent).ok_or("Missing app bundle")?;
    let app = contents.parent().ok_or("Missing app bundle")?;
    let info = installation::plist(app)?;
    let identifier = installation::string(&info, "CFBundleIdentifier")?;
    if !["playbit.engine", "playbit.engine.debug"].contains(&identifier.as_str()) {
        return Err("Background service control requires the original Playbit app bundle".into());
    }
    if ad_hoc_signature()? {
        // Ad-hoc signatures change with every rebuild. ServiceManagement can
        // retain the old hash even after unregistering. Launch Services starts
        // the background-only helper independently without a persistent job.
        // An isolated profile can already be using this bundle. The profile
        // lease still protects an initializing or draining default engine.
        if start {
            let status = Command::new("/usr/bin/open")
                .args(["-n", "-g", "-j", "-a"])
                .arg(contents.join("Helpers/playbit-engine.app"))
                .args(["--args", "--launch-agent"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()?;
            if !status.success() {
                return Err("macOS could not launch Playbit's development engine".into());
            }
        }
        // This launch path has no persistent registration to remove.
        return Ok(());
    }
    configure(start)?;
    if start {
        let target = format!(
            "gui/{}/{}",
            super::profile::current_uid(),
            service_label(contents, &identifier)?
        );
        let status = Command::new("/bin/launchctl")
            .args(["kickstart", &target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            return Err("macOS could not launch Playbit's background service".into());
        }
    }
    Ok(())
}

#[allow(
    unsafe_code,
    reason = "Security returns an owned signing-information dictionary; its flags are type-checked before use."
)]
fn ad_hoc_signature() -> Result<bool> {
    use core_foundation::{
        base::{CFType, TCFType},
        dictionary::{CFDictionary, CFDictionaryRef},
        number::CFNumber,
        string::{CFString, CFStringRef},
    };
    use security_framework::os::macos::code_signing::{Flags, SecCode};
    use security_framework_sys::code_signing::SecStaticCodeRef;
    unsafe extern "C" {
        static kSecCodeInfoFlags: CFStringRef;
        fn SecCodeCopySigningInformation(
            code: SecStaticCodeRef,
            flags: u32,
            information: *mut CFDictionaryRef,
        ) -> i32;
    }
    const AD_HOC: i64 = 0x0002; // kSecCodeSignatureAdhoc, CSCommon.h.
    let code = SecCode::for_self(Flags::empty())?;
    let mut raw = std::ptr::null();
    // SAFETY: code is live, raw is writable, and default flags request only
    // generic information. This API accepts either a Code or StaticCode object.
    let status =
        unsafe { SecCodeCopySigningInformation(code.as_concrete_TypeRef().cast(), 0, &mut raw) };
    if status != 0 {
        return Err(security_framework::base::Error::from_code(status).into());
    }
    if raw.is_null() {
        return Err("Missing Playbit signing information".into());
    }
    // SAFETY: successful copy returns an owned dictionary and the exported key
    // is a process-lifetime CFString. Values are checked as numbers below.
    let info: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let key = unsafe { CFString::wrap_under_get_rule(kSecCodeInfoFlags) };
    let flags = info
        .find(key)
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|value| value.to_i64())
        .ok_or("Missing Playbit signature flags")?;
    Ok(flags & AD_HOC != 0)
}

#[allow(
    unsafe_code,
    reason = "Core Foundation returns an owned, type-checked dictionary from the bounded signed service plist."
)]
fn service_label(contents: &Path, identifier: &str) -> Result<String> {
    use core_foundation::{
        base::{CFType, TCFType},
        data::CFData,
        dictionary::CFDictionary,
        propertylist,
        string::CFString,
    };
    let bytes = installation::read(
        &contents.join("Library/LaunchAgents/playbit-engine.plist"),
        installation::MAX_PLIST,
    )?;
    let (raw, _) = propertylist::create_with_data(
        CFData::from_buffer(&bytes),
        propertylist::kCFPropertyListImmutable,
    )
    .map_err(|error| error.to_string())?;
    // SAFETY: create returned one owned non-null object. Only a dictionary is
    // accepted, and the retained string is copied before the dictionary drops.
    let value = unsafe { CFType::wrap_under_create_rule(raw) };
    let dictionary = value
        .downcast::<CFDictionary>()
        .ok_or("Playbit's LaunchAgent plist is not a dictionary")?;
    let info: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_get_rule(dictionary.as_concrete_TypeRef()) };
    let label = installation::string(&info, "Label")?;
    if label.len() > 255
        || !label.starts_with(&format!("{identifier}."))
        || !label.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err("Invalid Playbit background service label".into());
    }
    Ok(label)
}

#[link(name = "ServiceManagement", kind = "framework")]
#[allow(
    unsafe_code,
    reason = "Link the native frameworks used by the synchronous Objective-C adapter below."
)]
unsafe extern "C" {}

#[link(name = "Foundation", kind = "framework")]
#[allow(unsafe_code, reason = "Link the Foundation classes used by the Objective-C adapter.")]
unsafe extern "C" {}

#[allow(
    unsafe_code,
    reason = "SMAppService objects and NSError messages are used synchronously inside one autorelease pool."
)]
fn configure(start: bool) -> Result<()> {
    use objc::{
        class, msg_send,
        runtime::{BOOL, NO, Object},
        sel, sel_impl,
    };
    objc::rc::autoreleasepool(|| unsafe {
        let name: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: c"playbit-engine.plist".as_ptr()];
        let service: *mut Object = msg_send![class!(SMAppService), agentServiceWithPlistName: name];
        if service.is_null() {
            return Err("macOS could not configure Playbit's background service".into());
        }
        let state: isize = msg_send![service, status];
        // Never unregister/re-register a service the user disabled. Respect the
        // system approval state and surface an actionable startup failure.
        if start && state == REQUIRES_APPROVAL {
            return Err("Allow Playbit in System Settings > General > Login Items & Extensions, then reopen Playbit.".into());
        }
        // An absent endpoint can mean slow initialization or a closed admission
        // listener while accepted work drains. Unregistering kills that process.
        // Updates retire their stopped job explicitly before replacing the bundle.
        if start && state == ENABLED {
            return Ok(());
        }
        let mut error: *mut Object = std::ptr::null_mut();
        if state == ENABLED || state == REQUIRES_APPROVAL {
            let ok: BOOL = msg_send![service, unregisterAndReturnError: &mut error];
            if ok == NO {
                return Err(service_error(error).into());
            }
        }
        if start {
            let ok: BOOL = msg_send![service, registerAndReturnError: &mut error];
            if ok == NO {
                return Err(service_error(error).into());
            }
            let state: isize = msg_send![service, status];
            if state != ENABLED {
                return Err("Allow Playbit's background service in System Settings > General > Login Items & Extensions, then reopen Playbit.".into());
            }
        }
        Ok(())
    })
}

#[allow(
    unsafe_code,
    reason = "NSError is retained by the enclosing autorelease pool; copy its bounded UTF-8 description before returning."
)]
unsafe fn service_error(error: *mut objc::runtime::Object) -> String {
    use objc::{msg_send, runtime::Object, sel, sel_impl};
    if error.is_null() {
        return "macOS rejected Playbit's background service request".into();
    }
    // SAFETY: the failed ServiceManagement call supplied a live NSError.
    unsafe {
        let text: *mut Object = msg_send![error, localizedDescription];
        let bytes: *const libc::c_char = msg_send![text, UTF8String];
        if bytes.is_null() {
            return "macOS rejected Playbit's background service request".into();
        }
        let length = libc::strnlen(bytes, MAX_ERROR_BYTES);
        String::from_utf8_lossy(std::slice::from_raw_parts(bytes.cast(), length)).into_owned()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful isolated fixture setup.")]
mod tests {
    use super::*;

    #[test]
    fn startup_failure_survives_exit_after_peer_authentication() {
        const CHILD: &str = "PLAYBIT_AGENT_FAILURE_SOCKET";
        if let Some(path) = std::env::var_os(CHILD) {
            let mut ready = UnixStream::connect(&path).unwrap();
            ipc::verify_peer(&ready).unwrap();
            wait_for_authentication(&mut ready, START_TIMEOUT).unwrap();
            super::super::failure::write_startup_failure(
                Path::new(&path).parent().unwrap(),
                &crate::store::Error::corrupt("fixture account record is missing"),
                &mut ready,
            )
            .unwrap();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(&executable)
            .args(["--exact", "engine::launch_agent::tests::startup_failure_survives_exit_after_peer_authentication"])
            .env(CHILD, &path)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + START_TIMEOUT;
        assert!(wait_readable(&listener, deadline).unwrap());
        let (mut ready, _) = listener.accept().unwrap();
        // Simulate the service controller returning after the helper connects.
        std::thread::sleep(Duration::from_millis(20));
        assert!(child.try_wait().unwrap().is_none());
        authenticate(&mut ready, &executable, deadline).unwrap();
        assert!(child.wait().unwrap().success());
        // The helper is gone, but its authenticated failure frame is buffered.
        let error = super::super::launch::read_ready(&mut ready, deadline).unwrap_err();
        assert_eq!(error.to_string(), ipc::CORRUPT_STORE_ERROR);
        assert!(super::super::restart::terminal(&error.to_string()));
    }

    #[test]
    fn helper_authentication_is_bounded_and_requires_the_acknowledgment() {
        let (mut helper, mut launcher) = UnixStream::pair().unwrap();
        launcher.write_all(b"X").unwrap();
        assert!(wait_for_authentication(&mut helper, START_TIMEOUT).is_err());
        let (mut helper, _launcher) = UnixStream::pair().unwrap();
        let timeout = Duration::from_millis(20);
        let started = Instant::now();
        assert!(wait_for_authentication(&mut helper, timeout).is_err());
        assert!(started.elapsed() >= timeout);
    }

    #[test]
    fn packaged_engine_resolution_and_custom_profile_isolation() {
        // A cold CLI can still use its direct child, while the desktop starts
        // independent background instances of the same signed helper.
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("Playbit.app");
        let engine = app.join("Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine");
        assert_eq!(app_for(&engine), Some(app.as_path()));
        assert_eq!(app_for(&app.join("Contents/MacOS/Playbit")), None);
        assert_eq!(app_for(Path::new("/installation/playbit-engine")), None);
        for channel in ReleaseChannel::ALL {
            let profile = directory.path().join(channel.name());
            super::super::profile::prepare(&profile, channel).unwrap();
            assert!(startup_command(&profile, &engine, false).unwrap().is_none());
            let (command, default) = startup_command(&profile, &engine, true).unwrap().unwrap();
            assert!(!default);
            assert_eq!(command.get_program(), "/usr/bin/open");
            let args: Vec<_> = command.get_args().collect();
            assert_eq!(args.len(), 9);
            assert_eq!(args[0], "-n");
            assert_eq!(args[4], app.join("Contents/Helpers/playbit-engine.app").as_os_str());
            assert_eq!(args[6], "--launch-isolated-profile");
            assert_eq!(args[7], profile.canonicalize().unwrap().as_os_str());
            assert_eq!(args[8], channel.name());
            for key in [
                "PLAYBIT_PROFILE",
                "PLAYBIT_CHANNEL",
                super::super::testing_profile::CREDENTIALS_ENV,
            ] {
                assert!(command.get_envs().any(|(name, value)| name == key && value.is_none()));
            }
        }
    }
}
