//! Logical GUI ownership is authenticated once and watched independently of IPC.
#![allow(unsafe_code, reason = "Kernel process identity and exit descriptors require native APIs.")]
use super::{Result, process_identity::Peer, session_instance::ProcessIdentity};
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
};

pub(crate) struct Owner {
    pub identity: ProcessIdentity,
    pub exit: OwnedFd,
}

/// A kernel exit notification for an already authenticated process identity.
#[cfg(target_os = "macos")]
pub struct ExitStatusWatch {
    descriptor: OwnedFd,
    pid: usize,
}
#[cfg(target_os = "macos")]
impl ExitStatusWatch {
    pub(crate) fn new(identity: &ProcessIdentity) -> Result<Self> {
        if platform::boot()? != identity.boot {
            return Err("The app process exited before exit observation started".into());
        }
        let owner =
            platform::recover_with_flags(identity, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS)?
                .ok_or("The app process exited before exit observation started")?;
        Ok(Self { descriptor: owner.exit, pid: identity.pid as usize })
    }

    /// EOF can precede process teardown. Wait at most five seconds, or until
    /// the log consumer is cancelled, without polling the process for liveness.
    pub fn wait(&self, cancelled: &UnixStream) -> Result<Option<std::process::ExitStatus>> {
        use std::{
            os::unix::process::ExitStatusExt,
            time::{Duration, Instant},
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut descriptors = [
            libc::pollfd { fd: self.descriptor.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: cancelled.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let millis = remaining.as_millis().min(5000) as i32;
            // SAFETY: both descriptors and the initialized two-entry array stay owned.
            let ready =
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, millis) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 || descriptors[1].revents != 0 {
                return Ok(None);
            }
            let mut event = libc::kevent {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let immediate = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: the owned kqueue and initialized single-event output are valid.
            let count = unsafe {
                libc::kevent(
                    self.descriptor.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &immediate,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if count != 1
                || event.ident != self.pid
                || event.filter != libc::EVFILT_PROC
                || event.flags & libc::EV_ERROR != 0
                || event.fflags & (libc::NOTE_EXIT | libc::NOTE_EXITSTATUS)
                    != (libc::NOTE_EXIT | libc::NOTE_EXITSTATUS)
            {
                return Err("App process exit status was not available".into());
            }
            let status = (event.data & libc::NOTE_PDATAMASK as isize) as i32;
            return Ok(Some(std::process::ExitStatus::from_raw(status)));
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Watch {
    SessionInstance(crate::store::Id),
    Tool(crate::store::Id),
}

impl Owner {
    pub fn capture(stream: &UnixStream) -> Result<Self> {
        let peer = Peer::capture(stream)?;
        platform::capture(&peer)
    }
    /// None means the exact recorded process is dead. Any other failure leaves
    /// ownership unknown; callers must not infer death from denied inspection.
    pub fn recover(identity: &ProcessIdentity) -> Result<Option<Self>> {
        if platform::boot()? != identity.boot {
            return Ok(None);
        }
        platform::recover(identity)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    unsafe extern "C" {
        fn csops_audittoken(
            pid: libc::pid_t,
            operation: libc::c_uint,
            buffer: *mut libc::c_void,
            size: usize,
            token: *const [u32; 8],
        ) -> libc::c_int;
    }
    pub fn boot() -> Result<[u8; 36]> {
        let mut bytes = [0u8; 37];
        let mut length = bytes.len();
        // SAFETY: bounded initialized output; sysctl name is NUL terminated.
        if unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if length != bytes.len() || bytes[36] != 0 {
            return Err("Invalid kernel boot identity".into());
        }
        Ok(bytes[..36].try_into().expect("checked boot UUID width"))
    }
    fn live(identity: &ProcessIdentity) -> Result<bool> {
        let mut status = 0u32;
        // CS_OPS_STATUS checks PID and generation against the audit token without
        // resolving the executable's vnode path. Atomic wrapper refresh can
        // unlink that path while the authenticated process remains alive.
        // SAFETY: initialized, bounded output and a live fixed-width audit token.
        if unsafe {
            csops_audittoken(
                identity.pid,
                0,
                (&mut status as *mut u32).cast(),
                std::mem::size_of_val(&status),
                &identity.token,
            )
        } == 0
        {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) { Ok(false) } else { Err(error.into()) }
    }
    pub fn capture(peer: &Peer) -> Result<Owner> {
        let token = peer.audit_token();
        let identity = ProcessIdentity { boot: boot()?, pid: token[5] as i32, token, started: 0 };
        recover(&identity)?
            .ok_or_else(|| "The guest process exited before claiming its instance".into())
    }
    pub fn recover(identity: &ProcessIdentity) -> Result<Option<Owner>> {
        recover_with_flags(identity, libc::NOTE_EXIT)
    }
    pub(super) fn recover_with_flags(
        identity: &ProcessIdentity,
        flags: u32,
    ) -> Result<Option<Owner>> {
        if identity.pid <= 1 || identity.token[5] != identity.pid as u32 {
            return Err("Invalid recorded process identity".into());
        }
        if !live(identity)? {
            return Ok(None);
        }
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let exit = unsafe { OwnedFd::from_raw_fd(raw) };
        let event = libc::kevent {
            ident: identity.pid as _,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: flags,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // The exact token is verified both sides of numeric-PID registration.
        // A PID replacement is never admitted as the recorded owner.
        if unsafe {
            libc::kevent(exit.as_raw_fd(), &event, 1, std::ptr::null_mut(), 0, std::ptr::null())
        } < 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error.into());
        }
        if !live(identity)? {
            return Ok(None);
        }
        Ok(Some(Owner { identity: identity.clone(), exit }))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::fs;
    pub fn boot() -> Result<[u8; 36]> {
        let text = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        text.trim().as_bytes().try_into().map_err(|_| "Invalid kernel boot identity".into())
    }
    fn started(pid: i32) -> Result<Option<u64>> {
        let text = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if text.len() > 64 * 1024 {
            return Err("Invalid process identity record".into());
        }
        // comm can contain spaces and ')'; all subsequent fields are numeric
        // except state. starttime is field 22, index 19 after the final ')'.
        let tail = text.rsplit_once(')').ok_or("Invalid process identity record")?.1;
        Ok(Some(tail.split_whitespace().nth(19).ok_or("Missing process start identity")?.parse()?))
    }
    pub fn capture(peer: &Peer) -> Result<Owner> {
        let exit = peer.process_handle()?;
        let pid = peer.pid();
        let start = started(pid)?.ok_or("The guest process exited before claiming its instance")?;
        // pidfd remains bound to the socket peer if numeric PID inspection races.
        let mut ready = libc::pollfd { fd: exit.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut ready, 1, 0) } != 0 {
            return Err("The guest process exited before claiming its instance".into());
        }
        Ok(Owner {
            identity: ProcessIdentity { boot: boot()?, pid, started: start, token: [0; 8] },
            exit,
        })
    }
    pub fn recover(identity: &ProcessIdentity) -> Result<Option<Owner>> {
        if identity.pid <= 1 {
            return Err("Invalid recorded process identity".into());
        }
        if started(identity.pid)? != Some(identity.started) {
            return Ok(None);
        }
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) } as i32;
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let exit = unsafe { OwnedFd::from_raw_fd(raw) };
        if started(identity.pid)? != Some(identity.started) {
            return Ok(None);
        }
        Ok(Some(Owner { identity: identity.clone(), exit }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful isolated process fixtures.")]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    struct Fixture(Child);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn authenticated_exit_status_distinguishes_success_failure_signal_and_cancellation() {
        use std::{net::Shutdown, os::unix::process::ExitStatusExt};
        const SCRIPT: &str = r#"import os, socket, sys
if sys.argv[3] == '1':
    child = os.fork()
    if child:
        _, status = os.waitpid(child, 0)
        sys.exit(os.waitstatus_to_exitcode(status))
peer = socket.socket(socket.AF_UNIX)
peer.connect(sys.argv[1])
peer.sendall(b'R')
peer.recv(1)
peer.close()
sys.stdin.buffer.read(1)
sys.exit(int(sys.argv[2]))
"#;
        // LaunchServices owners are not direct children of the CLI.
        for (code, indirect) in
            [(Some(0), false), (Some(23), false), (None, false), (Some(0), true), (Some(23), true)]
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("owner.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let child = Command::new("/usr/bin/python3")
                .args(["-c", SCRIPT])
                .arg(&path)
                .arg(code.unwrap_or(0).to_string())
                .arg(if indirect { "1" } else { "0" })
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let mut fixture = Fixture(child);
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.read_exact(&mut [0]).unwrap();
            let owner = Owner::capture(&stream).unwrap();
            let status = ExitStatusWatch::new(&owner.identity).unwrap();
            stream.write_all(b"C").unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            assert!(fixture.0.try_wait().unwrap().is_none());
            let (cancelled, wake) = UnixStream::pair().unwrap();
            wake.shutdown(Shutdown::Both).unwrap();
            assert!(status.wait(&cancelled).unwrap().is_none());
            if code.is_some() {
                fixture.0.stdin.take();
            } else {
                fixture.0.kill().unwrap();
            }
            let (waiting, _wake) = UnixStream::pair().unwrap();
            let observed = status.wait(&waiting).unwrap().unwrap();
            assert_eq!(observed.code(), code);
            if code.is_none() {
                assert_eq!(observed.signal(), Some(libc::SIGKILL));
            }
            assert_eq!(observed, fixture.0.wait().unwrap());
        }
    }
    #[test]
    fn kernel_exit_wakes_without_an_operation_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let child = Command::new("/usr/bin/python3")
            .args(["-c", "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(b'R'); s.recv(1); s.close(); sys.stdin.buffer.read(1)"])
            .arg(&path).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let mut fixture = Fixture(child);
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        let owner = Owner::capture(&stream).unwrap();
        stream.write_all(b"C").unwrap();
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
        drop(stream);
        assert!(
            !crate::engine::readiness::wait_readable(
                &owner.exit,
                Instant::now() + Duration::from_millis(10)
            )
            .unwrap()
        );
        fixture.0.stdin.take();
        assert!(
            crate::engine::readiness::wait_readable(
                &owner.exit,
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap()
        );
        fixture.0.wait().unwrap();
        assert!(Owner::recover(&owner.identity).unwrap().is_none());
    }
    #[test]
    fn operation_socket_closure_preserves_exact_owner() {
        let (client, server) = UnixStream::pair().unwrap();
        let owner = Owner::capture(&server).unwrap();
        let identity = owner.identity.clone();
        drop(client);
        drop(server);
        let recovered = Owner::recover(&identity).unwrap().unwrap();
        assert_eq!(recovered.identity, identity);
        let mut wrong_boot = identity;
        wrong_boot.boot[0] ^= 1;
        assert!(Owner::recover(&wrong_boot).unwrap().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unlinked_launcher_preserves_exact_owner_and_peer_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.sock");
        let executable = directory.path().join("owner");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let child = Command::new(&executable)
            .args(["--exact", "engine::process_owner::tests::owner_test_child", "--ignored"])
            .env("PLAYBIT_OWNER_TEST_SOCKET", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Fixture(child);
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        let owner = Owner::capture(&stream).unwrap();
        std::fs::remove_file(&executable).unwrap();
        assert!(Owner::recover(&owner.identity).unwrap().is_some());
        assert_eq!(Peer::capture(&stream).unwrap().audit_token(), owner.identity.token);
        let mut replaced = owner.identity.clone();
        replaced.token[7] ^= 1;
        assert!(Owner::recover(&replaced).unwrap().is_none());
        fixture.0.stdin.take();
        fixture.0.wait().unwrap();
        assert!(Owner::recover(&owner.identity).unwrap().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "subprocess fixture for exact owner recovery"]
    fn owner_test_child() {
        let Some(path) = std::env::var_os("PLAYBIT_OWNER_TEST_SOCKET") else { return };
        let mut stream = UnixStream::connect(path).unwrap();
        stream.write_all(b"R").unwrap();
        assert_eq!(std::io::stdin().read(&mut [0]).unwrap(), 0);
    }
}
