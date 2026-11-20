//! Kernel endpoint notifications share the engine's existing admission poller.
#![allow(unsafe_code, reason = "Filesystem notification APIs borrow owned native descriptors.")]
use crate::engine::Result;
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

pub(crate) struct Endpoint {
    path: PathBuf,
    device: u64,
    inode: u64,
    pub(crate) changes: OwnedFd,
    #[cfg(target_os = "macos")]
    _directories: [OwnedFd; 2],
}
impl Endpoint {
    pub(crate) fn new(path: &Path, profile: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err("Local engine endpoint is not a socket".into());
        }
        let parent = path.parent().ok_or("Local engine endpoint has no parent")?;
        #[cfg(target_os = "macos")]
        let (changes, directories) = watch(parent, profile)?;
        #[cfg(target_os = "linux")]
        let changes = watch(parent, profile)?;
        let endpoint = Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            changes,
            #[cfg(target_os = "macos")]
            _directories: directories,
        };
        // Validate again after registration to close the bind-to-watch race.
        if !endpoint.is_current()? {
            return Err("Local engine endpoint disappeared during startup".into());
        }
        Ok(endpoint)
    }
    pub(crate) fn lost(&self) -> Result<bool> {
        drain(&self.changes)?;
        Ok(!self.is_current()?)
    }
    fn is_current(&self) -> io::Result<bool> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => Ok(metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        // The engine lease excludes another cooperating writer until teardown.
        // Never remove a replacement installed after our endpoint disappeared.
        if self.is_current().unwrap_or(false) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(target_os = "macos")]
fn watch(parent: &Path, profile: &Path) -> Result<(OwnedFd, [OwnedFd; 2])> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: kqueue returns a new descriptor, owned immediately on success.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let changes = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut directories = Vec::with_capacity(2);
    // macOS cannot open socket vnodes, even with O_EVTONLY. Control-directory
    // writes cover unlink/replacement. Watch the profile itself too: renaming
    // it does not change the nested control directory's vnode.
    for directory in [parent, profile] {
        let path = std::ffi::CString::new(directory.as_os_str().as_bytes())?;
        // SAFETY: NUL-terminated path; successful open is immediately owned.
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let directory = unsafe { OwnedFd::from_raw_fd(raw) };
        let event = libc::kevent {
            ident: directory.as_raw_fd() as _,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: initialized event and live descriptors retained with queue.
        if unsafe {
            libc::kevent(changes.as_raw_fd(), &event, 1, std::ptr::null_mut(), 0, std::ptr::null())
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        directories.push(directory);
    }
    Ok((changes, directories.try_into().expect("two endpoint directories")))
}

#[cfg(target_os = "macos")]
fn drain(changes: &OwnedFd) -> Result<()> {
    let mut events = [libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    }; 2];
    let immediate = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: fixed initialized output and owned queue; never blocks the owner.
    let count = unsafe {
        libc::kevent(changes.as_raw_fd(), std::ptr::null(), 0, events.as_mut_ptr(), 2, &immediate)
    };
    if count < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if events[..count as usize].iter().any(|event| event.flags & libc::EV_ERROR != 0) {
        return Err("Local engine endpoint observation failed".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn watch(parent: &Path, profile: &Path) -> Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: successful descriptor is immediately owned; valid C path below.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let changes = unsafe { OwnedFd::from_raw_fd(raw) };
    for directory in [parent, profile] {
        let path = std::ffi::CString::new(directory.as_os_str().as_bytes())?;
        // SAFETY: owned descriptor and NUL-terminated path.
        if unsafe {
            libc::inotify_add_watch(
                changes.as_raw_fd(),
                path.as_ptr(),
                libc::IN_DELETE
                    | libc::IN_CREATE
                    | libc::IN_MOVED_FROM
                    | libc::IN_MOVED_TO
                    | libc::IN_DELETE_SELF
                    | libc::IN_MOVE_SELF,
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(changes)
}

#[cfg(target_os = "linux")]
fn drain(changes: &OwnedFd) -> Result<()> {
    // At most one fixed batch per owner turn. Remaining events keep it readable;
    // overflow is harmless because every event revalidates current path identity.
    let mut buffer = [0u8; 4096];
    // SAFETY: live descriptor and bounded initialized output.
    if unsafe { libc::read(changes.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert socket notification and ownership outcomes.")]
mod tests {
    use super::*;
    use std::{
        os::unix::net::UnixListener,
        time::{Duration, Instant},
    };

    #[test]
    fn socket_loss_wakes_owner_and_preserves_replacement() {
        // Real kernel readiness covers unlink, profile deletion/rename and a
        // replaced socket; old-owner cleanup must preserve the replacement.
        for action in 0..5 {
            let directory = tempfile::tempdir().unwrap();
            let profile = directory.path().join("profile");
            fs::create_dir_all(profile.join("control")).unwrap();
            let path = profile.join(crate::paths::ENGINE_SOCKET);
            let _listener = UnixListener::bind(&path).unwrap();
            let endpoint = Endpoint::new(&path, &profile).unwrap();
            assert!(!endpoint.lost().unwrap());
            let replacement = match action {
                0 => {
                    fs::remove_file(&path).unwrap();
                    None
                }
                1 => {
                    fs::remove_dir_all(&profile).unwrap();
                    None
                }
                2 => {
                    fs::rename(&profile, directory.path().join("retired")).unwrap();
                    None
                }
                3 => {
                    fs::rename(&path, profile.join("retired.sock")).unwrap();
                    Some(UnixListener::bind(&path).unwrap())
                }
                _ => {
                    // Only MOVED_TO reaches control when another directory's
                    // socket atomically replaces the still-present endpoint.
                    let incoming = directory.path().join("incoming.sock");
                    let replacement = UnixListener::bind(&incoming).unwrap();
                    fs::rename(incoming, &path).unwrap();
                    Some(replacement)
                }
            };
            assert!(
                crate::engine::readiness::wait_readable(
                    &endpoint.changes,
                    Instant::now() + Duration::from_secs(5)
                )
                .unwrap()
            );
            assert!(endpoint.lost().unwrap());
            drop(endpoint);
            assert_eq!(path.exists(), replacement.is_some());
        }
    }

    #[test]
    fn unrelated_profile_changes_keep_endpoint_alive() {
        // Revalidation ignores ordinary profile changes and owns final unlink.
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("control")).unwrap();
        let path = directory.path().join(crate::paths::ENGINE_SOCKET);
        let _listener = UnixListener::bind(&path).unwrap();
        let endpoint = Endpoint::new(&path, directory.path()).unwrap();
        fs::write(directory.path().join("other"), b"data").unwrap();
        fs::remove_file(directory.path().join("other")).unwrap();
        assert!(!endpoint.lost().unwrap());
        drop(endpoint);
        assert!(!path.exists());
    }
}
