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
pub(crate) const START_TIMEOUT_ERROR: &str = "Playbit's background service did not become ready. Check Playbit in System Settings > General > Login Items & Extensions, then reopen Playbit.";

/// Finder can be served by either installed extension. Only the default
/// profile's previously authenticated helper publishes its containing app.
pub fn owner(profile: &Path) -> Result<Option<PathBuf>> {
    let channel = super::file_provider::require_system_profile(profile)?;
    let state = super::profile::default_runtime_path(&super::profile::credential_home()?, channel);
    read_owner(&state)
}

pub(super) fn installed_owner(profile: &Path) -> Result<Option<PathBuf>> {
    match owner(profile)? {
        Some(app) if app.try_exists()? => Ok(Some(app)),
        _ => Ok(None),
    }
}

pub(super) fn read_owner(state: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = match installation::read(&state.join("agent-owner"), 4096) {
        Ok(bytes) => bytes,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    if !path.is_absolute() || path.extension().is_none_or(|ext| ext != "app") {
        return Err("Invalid Playbit background service owner".into());
    }
    Ok(Some(path))
}

fn service_lock(state: &Path) -> Result<fs::File> {
    let lock = super::profile::private_file(&state.join("agent.lock"))?;
    lock.lock()?;
    Ok(lock)
}

// The service lock serializes registration, owner replacement and retirement.
// Publish by rename so a concurrent Finder read always sees a complete path.
fn record_owner(state: &Path, app: &Path) -> Result<()> {
    if read_owner(state)?.as_deref() == Some(app) {
        return Ok(());
    }
    super::profile::write_state(state, "agent-owner", app.as_os_str().as_encoded_bytes())
}

pub(super) fn app_for(executable: &Path) -> Option<&Path> {
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
        command
            .args(["engine-service", "start"])
            .env("PLAYBIT_CHANNEL", channel.name())
            .env_remove("PLAYBIT_PROFILE");
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
    if default_profile
        && let Some(owner) = installed_owner(profile)?
        && Some(owner.as_path()) != app_for(&executable.canonicalize()?)
    {
        let engine = super::launch::executable_for(&owner.join("Contents/MacOS/Playbit"))?;
        // A prepared guest's executable is its generated wrapper. The command
        // was resolved from the originating engine in its signed descriptor.
        start_registered(
            profile,
            &engine,
            super::profile::for_profile(profile)?,
            Path::new(command.get_program()),
        )?;
        return Ok(true);
    }
    launch_with_readiness(
        profile,
        ExpectedEngine::Build(executable),
        &mut command,
        default_profile,
    )?;
    Ok(true)
}

/// Wake the previously registered channel job from another installation.
/// The recorded engine path authenticates the peer;
/// launchctl uses the channel label from the caller's own signed bundle.
pub(super) fn start_registered(
    profile: &Path,
    executable: &Path,
    channel: ReleaseChannel,
    controller: &Path,
) -> Result<()> {
    let mut command = registered_command(controller, channel)?;
    launch_with_readiness(profile, ExpectedEngine::Owner(executable), &mut command, true)
}

fn registered_command(controller: &Path, channel: ReleaseChannel) -> Result<Command> {
    if ad_hoc_signature(controller)? {
        return Err(
            "Open the Playbit app that owns this environment to start its background engine."
                .into(),
        );
    }
    let (contents, identifier) = service_bundle(controller)?;
    let label = service_label(contents, &identifier, service_plist(channel)?.to_str()?)?;
    let mut command = Command::new("/bin/launchctl");
    command
        .arg("kickstart")
        .arg(format!("gui/{}/{label}", super::profile::current_uid()))
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    Ok(command)
}

enum ExpectedEngine<'a> {
    Build(&'a Path),
    Owner(&'a Path),
}

fn launch_with_readiness(
    profile: &Path,
    engine: ExpectedEngine<'_>,
    command: &mut Command,
    default_profile: bool,
) -> Result<()> {
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
            START_TIMEOUT_ERROR
        } else {
            "The background engine for this Playbit profile did not become ready."
        }
        .into());
    }
    let (mut ready, _) = listener.accept()?;
    authenticate(&mut ready, engine, deadline)?;
    super::launch::read_ready(&mut ready, deadline)
}

fn authenticate(
    ready: &mut UnixStream,
    engine: ExpectedEngine<'_>,
    deadline: Instant,
) -> Result<()> {
    match engine {
        ExpectedEngine::Owner(path) => {
            Peer::capture_at(ready, Some(path))?;
        }
        ExpectedEngine::Build(executable) => {
            let path = executable.canonicalize()?;
            if !Peer::capture_at(ready, Some(&path))?.matches(executable)? {
                return Err(
                    "Unexpected process connected to Playbit's background startup socket".into()
                );
            }
        }
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
    let (channel, isolated) = match arguments.next().as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--launch-agent") => {
            let channel = match arguments.next() {
                None => ReleaseChannel::Public,
                Some(channel) if channel == "testing" => ReleaseChannel::Testing,
                _ => return Err("Invalid background service channel".into()),
            };
            if arguments.next().is_some() {
                return Err("Invalid background service startup request".into());
            }
            (channel, None)
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
            (ReleaseChannel::parse(channel)?, Some(PathBuf::from(profile)))
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
    super::profile::initialize_agent(channel, isolated.as_deref())?;
    let profile = super::profile_path();
    let mut ready = UnixStream::connect(profile.join(READY_SOCKET))?;
    ipc::verify_peer(&ready)?;
    // A failing initializer may exit immediately after writing its diagnostic.
    // Keep this process alive until the launcher has captured its kernel identity.
    wait_for_authentication(&mut ready, START_TIMEOUT)?;
    if isolated.is_none() {
        let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
        let state = super::profile::default_runtime_path(&home, channel);
        let executable = std::env::current_exe()?.canonicalize()?;
        let app = app_for(&executable).ok_or("Background service requires the packaged engine")?;
        let _service = service_lock(&state)?;
        record_owner(&state, app)?;
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    RetireOwned,
    Unregister,
}

/// Executed by the original installation's pb, including when a prepared guest
/// requests startup. SMAppService resolves plists relative to the calling bundle.
pub fn control(action: ServiceAction, channel: ReleaseChannel) -> Result<()> {
    let start = action == ServiceAction::Start;
    let plist_name = service_plist(channel)?;
    let executable = std::env::current_exe()?;
    let _installation = if start { installation::client_lease(&executable)? } else { None };
    let (contents, identifier) = service_bundle(&executable)?;
    let app = contents.parent().ok_or("Missing app bundle")?;
    let ad_hoc = ad_hoc_signature(&executable)?;
    if start && ad_hoc {
        // Ad-hoc signatures change with every rebuild. ServiceManagement can
        // retain the old hash even after unregistering. Launch Services starts
        // the background-only helper independently without a persistent job.
        // An isolated profile can already be using this bundle. The profile
        // lease still protects an initializing or draining default engine.
        let mut command = Command::new("/usr/bin/open");
        command
            .args(["-n", "-g", "-j", "-a"])
            .arg(contents.join("Helpers/playbit-engine.app"))
            .args(["--args", "--launch-agent"]);
        if channel == ReleaseChannel::Testing {
            command.arg(channel.name());
        }
        let status = command.stdin(Stdio::null()).stdout(Stdio::null()).status()?;
        if !status.success() {
            return Err("macOS could not launch Playbit's development engine".into());
        }
        return Ok(());
    }
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    let state = super::profile::default_runtime_path(&home, channel);
    if !start {
        let legacy = action == ServiceAction::Unregister
            && channel == ReleaseChannel::Public
            && !ad_hoc
            && identifier == "playbit.engine";
        if legacy && home.canonicalize()? != super::profile::credential_home()?.canonicalize()? {
            return Err("Use your login account's home directory to unregister \
                the Public background service."
                .into());
        }
        if !legacy && !state.try_exists()? {
            return Ok(());
        }
        super::profile::private_directory(&state)?;
        let profile = super::profile::default_path(&home, channel);
        return retire(&state, &profile, app, action, legacy, || {
            if !ad_hoc {
                configure(plist_name, None)?;
            }
            Ok(())
        });
    }
    super::profile::private_directory(&state)?;
    {
        let _service = service_lock(&state)?;
        configure(plist_name, Some((&state, app)))?;
    }
    let target = format!(
        "gui/{}/{}",
        super::profile::current_uid(),
        service_label(contents, &identifier, plist_name.to_str()?)?
    );
    let status = Command::new("/bin/launchctl")
        .args(["kickstart", &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("macOS could not launch Playbit's background service".into());
    }
    Ok(())
}

fn retire(
    state: &Path,
    profile: &Path,
    app: &Path,
    action: ServiceAction,
    legacy: bool,
    unregister: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let _service = service_lock(state)?;
    let owner = read_owner(state)?;
    let _legacy_users = match owner.as_deref() {
        Some(owner) if owner == app => None,
        Some(owner) if action == ServiceAction::Unregister => {
            return Err(format!(
                "{} manages this background service. Run its bundled pb to unregister it.",
                owner.display()
            )
            .into());
        }
        None if action == ServiceAction::Unregister && legacy => {
            // The single-job release used this lease for both ordinary channels.
            // Keep new and old users out until ServiceManagement has finished.
            let active = super::profile::private_file(&state.join("active.lock"))?;
            match active.try_lock() {
                Ok(()) => Some(active),
                Err(fs::TryLockError::WouldBlock) => {
                    return Err("Close the Public and Testing Playbit apps and commands and let \
                        their engines exit before unregistering the previous background service."
                        .into());
                }
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        _ => return Ok(()),
    };
    let _launch = super::launch::drained_service(profile)?;
    unregister()?;
    if owner.is_some() {
        fs::remove_file(state.join("agent-owner"))?;
        fs::File::open(state)?.sync_all()?;
    }
    Ok(())
}

fn service_plist(channel: ReleaseChannel) -> Result<&'static std::ffi::CStr> {
    match channel {
        ReleaseChannel::Public => Ok(c"playbit-engine.plist"),
        ReleaseChannel::Testing => Ok(c"playbit-engine-testing.plist"),
        _ => Err("Managed background services require a distribution channel".into()),
    }
}

fn service_bundle(executable: &Path) -> Result<(&Path, String)> {
    let contents = executable.parent().and_then(Path::parent).ok_or("Missing app bundle")?;
    let app = contents.parent().ok_or("Missing app bundle")?;
    Ok((contents, service_identifier(app)?))
}

pub(super) fn service_identifier(app: &Path) -> Result<String> {
    let identifier = installation::string(&installation::plist(app)?, "CFBundleIdentifier")?;
    if !["playbit.engine", "playbit.engine.debug"].contains(&identifier.as_str()) {
        return Err("Background service control requires the original Playbit app bundle".into());
    }
    Ok(identifier)
}

#[allow(
    unsafe_code,
    reason = "Security returns an owned signing-information dictionary; its flags are type-checked before use."
)]
fn ad_hoc_signature(executable: &Path) -> Result<bool> {
    use core_foundation::url::CFURL;
    use core_foundation::{
        base::{CFType, TCFType},
        dictionary::{CFDictionary, CFDictionaryRef},
        number::CFNumber,
        string::{CFString, CFStringRef},
    };
    use security_framework::os::macos::code_signing::{Flags, SecStaticCode};
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
    let url =
        CFURL::from_path(executable, false).ok_or("Invalid Playbit service controller path")?;
    let code = SecStaticCode::from_path(&url, Flags::empty())?;
    let mut raw = std::ptr::null();
    // SAFETY: code is live, raw is writable, and default flags request only
    // generic information from the selected packaged executable.
    let status = unsafe { SecCodeCopySigningInformation(code.as_concrete_TypeRef(), 0, &mut raw) };
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
fn service_label(contents: &Path, identifier: &str, plist_name: &str) -> Result<String> {
    use core_foundation::{
        base::{CFType, TCFType},
        data::CFData,
        dictionary::CFDictionary,
        propertylist,
        string::CFString,
    };
    let bytes = installation::read(
        &contents.join("Library/LaunchAgents").join(plist_name),
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
fn configure(plist_name: &std::ffi::CStr, registration: Option<(&Path, &Path)>) -> Result<()> {
    use objc::{
        class, msg_send,
        runtime::{BOOL, NO, Object},
        sel, sel_impl,
    };
    objc::rc::autoreleasepool(|| unsafe {
        let name: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: plist_name.as_ptr()];
        let service: *mut Object = msg_send![class!(SMAppService), agentServiceWithPlistName: name];
        if service.is_null() {
            return Err("macOS could not configure Playbit's background service".into());
        }
        let state: isize = msg_send![service, status];
        // Never unregister/re-register a service the user disabled. Respect the
        // system approval state and surface an actionable startup failure.
        if registration.is_some() && state == REQUIRES_APPROVAL {
            return Err("Allow Playbit in System Settings > General > Login Items & Extensions, \
                then reopen Playbit."
                .into());
        }
        // An absent endpoint can mean slow initialization or a closed admission
        // listener while accepted work drains. Unregistering kills that process.
        // Updates retire their stopped job explicitly before replacing the bundle.
        if registration.is_some() && state == ENABLED {
            return Ok(());
        }
        let mut error: *mut Object = std::ptr::null_mut();
        if state == ENABLED || state == REQUIRES_APPROVAL {
            let ok: BOOL = msg_send![service, unregisterAndReturnError: &mut error];
            if ok == NO {
                return Err(service_error(error).into());
            }
        }
        if let Some((owner_state, app)) = registration {
            let ok: BOOL = msg_send![service, registerAndReturnError: &mut error];
            if ok == NO {
                return Err(service_error(error).into());
            }
            let state: isize = msg_send![service, status];
            // Approval can delay launch after this app successfully registered
            // its job. Its updater must still be able to retire that job.
            if matches!(state, ENABLED | REQUIRES_APPROVAL) {
                record_owner(owner_state, app)?;
            }
            if state != ENABLED {
                return Err(
                    "Allow Playbit's background service in System Settings > General > Login \
                     Items & Extensions, then reopen Playbit."
                        .into(),
                );
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
            .args([
                "--exact",
                "engine::launch_agent::tests::startup_failure_survives_exit_after_peer_\
                 authentication",
            ])
            .env(CHILD, &path)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + START_TIMEOUT;
        assert!(wait_readable(&listener, deadline).unwrap());
        let (mut ready, _) = listener.accept().unwrap();
        // Simulate the service controller returning after the helper connects.
        std::thread::sleep(Duration::from_millis(20));
        assert!(child.try_wait().unwrap().is_none());
        authenticate(&mut ready, ExpectedEngine::Build(&executable), deadline).unwrap();
        assert!(child.wait().unwrap().success());
        // The helper is gone, but its authenticated failure frame is buffered.
        let error = super::super::launch::read_ready(&mut ready, deadline).unwrap_err();
        assert_eq!(error.to_string(), ipc::CORRUPT_STORE_ERROR);
        assert!(super::super::restart::terminal(&error.to_string()));
    }

    #[test]
    fn registered_app_persists_across_restart_and_updates_only_its_channel() {
        let home = tempfile::tempdir().unwrap();
        let public =
            super::super::profile::default_runtime_path(home.path(), ReleaseChannel::Public);
        let testing =
            super::super::profile::default_runtime_path(home.path(), ReleaseChannel::Testing);
        let app = home.path().join("Playbit.app");
        let testing_app = home.path().join("Playbit (testing).app");
        for (state, owner) in [(&public, &app), (&testing, &testing_app)] {
            super::super::profile::private_directory(state).unwrap();
            assert_eq!(read_owner(state).unwrap(), None);
            let _service = service_lock(state).unwrap();
            record_owner(state, owner).unwrap();
            record_owner(state, owner).unwrap();
        }
        {
            let _service = service_lock(&testing).unwrap();
            record_owner(&testing, &app).unwrap();
        }
        assert_eq!(read_owner(&public).unwrap(), Some(app.clone()));
        assert_eq!(read_owner(&testing).unwrap(), Some(app));
    }

    #[test]
    fn legacy_public_unregistration_requires_explicit_request_and_quiescence() {
        use super::super::profile::{private_directory, private_file};
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let app = home.path().join("Playbit.app");
        let state = home.path().join(".playbit-runtime");
        let profile = home.path().join(".playbit");
        private_directory(&state).unwrap();
        private_directory(&profile).unwrap();
        let unexpected = || Err("unexpected service retirement".into());
        retire(&state, &profile, &app, ServiceAction::RetireOwned, true, unexpected).unwrap();
        retire(&state, &profile, &app, ServiceAction::Unregister, false, unexpected).unwrap();

        let active = private_file(&state.join("active.lock")).unwrap();
        active.lock_shared().unwrap(); // The previous Testing app used this same lease.
        let explicit = ServiceAction::Unregister;
        let error = retire(&state, &profile, &app, explicit, true, unexpected).unwrap_err();
        assert!(error.to_string().contains("let their engines exit"));
        active.unlock().unwrap();
        let admission = super::super::launch::drained_service(&profile).unwrap().unwrap();
        let error = retire(&state, &profile, &app, explicit, true, unexpected).unwrap_err();
        assert!(error.to_string().contains("already starting"));
        drop(admission);
        let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let error = retire(&state, &profile, &app, explicit, true, unexpected).unwrap_err();
        assert!(error.to_string().contains("running Playbit engine"));
        drop(listener);

        let mut unregistered = false;
        retire(&state, &profile, &app, explicit, true, || {
            for path in [state.join("agent.lock"), profile.join("control/launch.lock")] {
                assert!(matches!(
                    private_file(&path)?.try_lock(),
                    Err(fs::TryLockError::WouldBlock)
                ));
            }
            assert!(matches!(active.try_lock_shared(), Err(fs::TryLockError::WouldBlock)));
            unregistered = true;
            Ok(())
        })
        .unwrap();
        assert!(unregistered);

        let foreign = home.path().join("Playbit (testing).app");
        record_owner(&state, &foreign).unwrap();
        let error = retire(&state, &profile, &app, explicit, true, unexpected).unwrap_err();
        assert!(error.to_string().contains(foreign.to_str().unwrap()));
        retire(&state, &profile, &app, ServiceAction::RetireOwned, false, unexpected).unwrap();
        assert_eq!(read_owner(&state).unwrap(), Some(foreign));
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
    fn borrowed_cold_start_uses_the_originating_packaged_controller() {
        // A generated guest has its own identifier and ad-hoc signature.
        // Its descriptor points back into the packaged installation.
        let directory = tempfile::tempdir().unwrap();
        let contents = directory.path().join("Origin.app/Contents");
        fs::create_dir_all(contents.join("MacOS")).unwrap();
        fs::create_dir_all(contents.join("Library/LaunchAgents")).unwrap();
        let controller = contents.join("MacOS/pb");
        fs::copy("/bin/echo", &controller).unwrap(); // a certificate-signed test executable
        fs::write(
            contents.join("Info.plist"),
            b"<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key>\
              <string>playbit.engine.debug</string></dict></plist>",
        )
        .unwrap();
        let label = "playbit.engine.debug.helper.testing.agent";
        fs::write(
            contents.join("Library/LaunchAgents/playbit-engine-testing.plist"),
            format!(
                "<plist version=\"1.0\"><dict><key>Label</key>\
                 <string>{label}</string></dict></plist>"
            ),
        )
        .unwrap();
        let command = registered_command(&controller, ReleaseChannel::Testing).unwrap();
        assert_eq!(command.get_program(), "/bin/launchctl");
        let target = format!("gui/{}/{label}", super::super::profile::current_uid());
        assert!(command.get_args().eq(["kickstart", target.as_str()]));
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
