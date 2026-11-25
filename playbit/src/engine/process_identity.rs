//! Kernel-authenticated local engine identity and bounded process-exit wait.
#![allow(
    unsafe_code,
    reason = "Peer identity and termination use native process and socket APIs with local safety checks."
)]
use crate::engine::{Result, readiness::wait_readable};
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
    time::{Duration, Instant},
};

fn stop_process(
    process: &OwnedFd,
    timeout: Duration,
    replace_data: bool,
    mut signal: impl FnMut(libc::c_int) -> Result<()>,
) -> Result<()> {
    signal(libc::SIGTERM)?;
    if wait_readable(process, Instant::now() + timeout)? {
        return Ok(());
    }
    if replace_data {
        signal(libc::SIGKILL)?;
        if wait_readable(process, Instant::now() + Duration::from_secs(5))? {
            return Ok(());
        }
    }
    Err("The old Playbit engine did not stop. Your local data is retained; retry after it exits."
        .into())
}

fn signal_result(result: libc::c_int) -> Result<()> {
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error.into()) }
}

impl Peer {
    pub fn stop(self, timeout: Duration) -> Result<()> {
        self.stop_inner(timeout, false)
    }

    /// Only explicit profile replacement authorizes discarding accepted work.
    pub fn stop_for_reset(self) -> Result<()> {
        self.stop_inner(Duration::from_secs(30), true)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{ffi::OsStr, fs::File, io::Read, os::unix::ffi::OsStrExt};

    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidpath_audittoken(
            token: *mut [u32; 8],
            buffer: *mut libc::c_void,
            size: u32,
        ) -> libc::c_int;
        fn proc_signal_with_audittoken(token: *mut [u32; 8], signal: libc::c_int) -> libc::c_int;
    }

    fn option<const N: usize>(stream: &UnixStream, option: libc::c_int) -> Result<[u8; N]> {
        let mut value = [0; N];
        let mut size = N as libc::socklen_t;
        // SAFETY: size exactly describes the writable fixed-size output.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                option,
                value.as_mut_ptr().cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if size as usize != N {
            return Err("Invalid local engine process identity".into());
        }
        Ok(value)
    }

    pub struct Peer {
        token: [u32; 8],
        pid: libc::pid_t,
        uuid: [u8; 16],
        path: Option<std::path::PathBuf>,
    }
    impl Peer {
        pub fn capture(stream: &UnixStream) -> Result<Self> {
            Self::capture_at(stream, None)
        }
        pub(in crate::engine) fn capture_at(
            stream: &UnixStream,
            expected: Option<&Path>,
        ) -> Result<Self> {
            crate::engine::ipc::verify_peer(stream)?;
            let bytes = option::<32>(stream, libc::LOCAL_PEERTOKEN)?;
            let token = std::array::from_fn(|i| {
                u32::from_ne_bytes(
                    bytes[i * 4..i * 4 + 4]
                        .try_into()
                        .expect("checked fixed-width field has the destination width"),
                )
            });
            let pid = i32::from_ne_bytes(option(stream, libc::LOCAL_PEERPID)?);
            let uuid = option(stream, libc::LOCAL_PEERUUID)?;
            let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            let mut identity = token;
            // SAFETY: token is the socket's kernel audit token, including PID
            // generation. The output is bounded by the supplied buffer length.
            let length = unsafe {
                proc_pidpath_audittoken(&mut identity, path.as_mut_ptr().cast(), path.len() as u32)
            };
            let path_available = length > 0;
            if length <= 0 {
                let error = io::Error::last_os_error();
                // A refreshed guest may still map an unlinked launcher. Its
                // socket audit token/UUID remain authoritative; an unavailable
                // path must never qualify it as an engine we can terminate.
                if error.raw_os_error() != Some(libc::ENOENT) {
                    return Err(error.into());
                }
            }
            let length = path.iter().position(|b| *b == 0).unwrap_or(path.len());
            let path = Path::new(OsStr::from_bytes(&path[..length]));
            if let Some(expected) = expected
                && (!path_available || path != expected)
            {
                return Err("Another Playbit installation is using this profile. \
                    Open that installation to continue."
                    .into());
            }
            Ok(Self { token, pid, uuid, path: path_available.then(|| path.to_owned()) })
        }
        pub fn matches(&self, executable: &Path) -> Result<bool> {
            Ok(self.uuid == executable_uuid(executable)?)
        }
        /// A legacy helper can have a different build and wire protocol. Use
        /// the originating installation's designated code requirement to
        /// recognize another release of that helper by its socket audit token.
        pub(in crate::engine) fn verify_signing_identity(&self, executable: &Path) -> Result<()> {
            use core_foundation::{base::TCFType, data::CFData, url::CFURL};
            use security_framework::os::macos::code_signing::{
                Flags, GuestAttributes, SecCode, SecRequirement, SecStaticCode,
            };
            use security_framework_sys::code_signing::{SecRequirementRef, SecStaticCodeRef};
            unsafe extern "C" {
                fn SecCodeCopyDesignatedRequirement(
                    code: SecStaticCodeRef,
                    flags: u32,
                    requirement: *mut SecRequirementRef,
                ) -> i32;
            }
            let url = CFURL::from_path(executable, false).ok_or("Invalid Playbit helper path")?;
            let expected = SecStaticCode::from_path(&url, Flags::NONE)?;
            let mut requirement = std::ptr::null_mut();
            // SAFETY: expected is live and a successful copy returns an owned requirement.
            let status = unsafe {
                SecCodeCopyDesignatedRequirement(
                    expected.as_concrete_TypeRef(),
                    0,
                    &mut requirement,
                )
            };
            if status != 0 {
                return Err(security_framework::base::Error::from_code(status).into());
            }
            if requirement.is_null() {
                return Err("Missing Playbit helper signing identity".into());
            }
            // SAFETY: the successful copy above returned a non-null owned requirement.
            let requirement = unsafe { SecRequirement::wrap_under_create_rule(requirement) };
            let bytes = self.token.map(u32::to_ne_bytes);
            let token = CFData::from_buffer(bytes.as_flattened());
            let mut attributes = GuestAttributes::new();
            attributes.set_audit_token(token.as_concrete_TypeRef());
            let process = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE)?;
            process.check_validity(Flags::NONE, &requirement).map_err(|_| {
                "The local process does not have Playbit's engine signing identity.".into()
            })
        }
        pub(crate) fn audit_token(&self) -> [u32; 8] {
            self.token
        }
        pub fn require_engine(&self) -> Result<()> {
            self.engine_path().map(|_| ())
        }
        pub(in crate::engine) fn engine_path(&self) -> Result<&Path> {
            // SAFETY: getpid has no pointer arguments or side effects.
            if self.pid > 1
                && self.pid != unsafe { libc::getpid() }
                && let Some(path) = self.path.as_deref()
                && path.file_name() == Some(OsStr::new("playbit-engine"))
            {
                return Ok(path);
            }
            Err("The local endpoint is not a separate Playbit engine. \
                It was left running; your data is unchanged."
                .into())
        }
        pub(super) fn stop_inner(mut self, timeout: Duration, replace_data: bool) -> Result<()> {
            self.require_engine()?;
            // SAFETY: kqueue returns a new owned descriptor on success.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return Err(io::Error::last_os_error().into());
            }
            let queue = unsafe { OwnedFd::from_raw_fd(raw) };
            let event = libc::kevent {
                ident: self.pid as _,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // Register before signaling. Even immediate exit is retained by the
            // kernel; no timer probes or PID-name scans are involved.
            if unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    &event,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            } < 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                return Err(error.into());
            }
            // The audit token protects against PID reuse between verification,
            // registration and signaling. Never escalate to a name-based kill.
            stop_process(&queue, timeout, replace_data, |signal| {
                // SAFETY: both signals address the original socket peer's PID generation.
                signal_result(unsafe { proc_signal_with_audittoken(&mut self.token, signal) })
            })
        }
    }

    fn executable_uuid(executable: &Path) -> Result<[u8; 16]> {
        let _timing = crate::trace::Span::new("engine.executable_identity");
        let mut file = File::open(executable)?;
        let mut header = [0; 32];
        file.read_exact(&mut header)?;
        let word = |bytes: &[u8]| {
            u32::from_le_bytes(
                bytes.try_into().expect("checked fixed-width field has the destination width"),
            )
        };
        if word(&header[..4]) != 0xfeedfacf {
            return Err("Playbit engine is not a native 64-bit executable".into());
        }
        let count = word(&header[16..20]) as usize;
        let length = word(&header[20..24]) as usize;
        if length > 64 * 1024 || count > length / 8 {
            return Err("Invalid Playbit engine executable header".into());
        }
        let mut commands = vec![0; length];
        file.read_exact(&mut commands)?;
        uuid_from_commands(&commands, count)
    }

    fn uuid_from_commands(mut commands: &[u8], count: usize) -> Result<[u8; 16]> {
        for _ in 0..count {
            if commands.len() < 8 {
                break;
            }
            let kind = u32::from_le_bytes(
                commands[..4]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"),
            );
            let length = u32::from_le_bytes(
                commands[4..8]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"),
            ) as usize;
            if length < 8 || length > commands.len() {
                break;
            }
            if kind == 0x1b && length == 24 {
                return Ok(commands[8..24]
                    .try_into()
                    .expect("checked fixed-width field has the destination width"));
            }
            commands = &commands[length..];
        }
        Err("Playbit engine executable has no valid build identity".into())
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
    mod tests {
        use super::*;
        #[test]
        fn executable_uuid_is_bounded_and_rejects_malformed_commands() {
            let mut command = vec![0x1b, 0, 0, 0, 24, 0, 0, 0];
            command.extend([7; 16]);
            assert_eq!(uuid_from_commands(&command, 1).unwrap(), [7; 16]);
            for length in [0, 7, 23, 25, u32::MAX] {
                command[4..8].copy_from_slice(&length.to_le_bytes());
                assert!(uuid_from_commands(&command, 1).is_err());
            }
            assert!(uuid_from_commands(&[], 1).is_err());
            assert_ne!(executable_uuid(&std::env::current_exe().unwrap()).unwrap(), [0; 16]);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};
    pub struct Peer {
        stream: UnixStream,
        pid: libc::pid_t,
        executable: PathBuf,
    }
    impl Peer {
        pub fn capture(stream: &UnixStream) -> Result<Self> {
            crate::engine::ipc::verify_peer(stream)?;
            let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    std::ptr::addr_of_mut!(credentials).cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            if size as usize != std::mem::size_of_val(&credentials) {
                return Err("Invalid local peer credentials".into());
            }
            let executable = PathBuf::from(format!("/proc/{}/exe", credentials.pid));
            Ok(Self { stream: stream.try_clone()?, pid: credentials.pid, executable })
        }
        pub fn matches(&self, executable: &Path) -> Result<bool> {
            let running = fs::metadata(&self.executable)?;
            let desired = fs::metadata(executable)?;
            Ok((running.dev(), running.ino()) == (desired.dev(), desired.ino()))
        }
        pub(crate) fn process_handle(&self) -> Result<OwnedFd> {
            let mut raw = -1;
            let mut size = std::mem::size_of_val(&raw) as libc::socklen_t;
            // SAFETY: the kernel writes one owned descriptor into a sized output.
            if unsafe {
                libc::getsockopt(
                    self.stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERPIDFD,
                    std::ptr::addr_of_mut!(raw).cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            if raw < 0 {
                return Err("Invalid local process handle".into());
            }
            let process = unsafe { OwnedFd::from_raw_fd(raw) };
            if size as usize != std::mem::size_of_val(&raw) {
                return Err("Invalid local process handle size".into());
            }
            Ok(process)
        }
        pub(crate) fn pid(&self) -> libc::pid_t {
            self.pid
        }
        pub fn require_engine(&self) -> Result<()> {
            if self.pid <= 1 || self.pid == unsafe { libc::getpid() } {
                return Err("The local endpoint is not a separate Playbit engine. \
                    It was left running; your data is unchanged."
                    .into());
            }
            let name = match fs::read_link(&self.executable) {
                Ok(name) => name,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH).into());
                }
                Err(error) => return Err(error.into()),
            };
            if !matches!(
                name.file_name().and_then(|name| name.to_str()),
                Some("playbit-engine" | "playbit-engine (deleted)")
            ) {
                return Err("The local endpoint is not a Playbit engine. It was left running; \
                    your data is unchanged."
                    .into());
            }
            Ok(())
        }
        pub(super) fn stop_inner(self, timeout: Duration, replace_data: bool) -> Result<()> {
            self.require_engine()?;
            // Retain the socket's pidfd through both signals and exit confirmation.
            let process = self.process_handle()?;
            stop_process(&process, timeout, replace_data, |signal| {
                // SAFETY: pidfd remains bound to the original peer even after PID reuse.
                signal_result(unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        process.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                } as libc::c_int)
            })
        }
    }
}

pub(crate) use platform::Peer;

#[cfg(test)]
mod tests;
