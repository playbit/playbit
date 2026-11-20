//! Captured host directory names. Classification never follows links or opens
//! devices, sockets, or FIFOs. Cursor ownership is handled by the engine.
#![allow(
    unsafe_code,
    reason = "Bounded fdopendir/readdir/fstatat capture retains one directory descriptor."
)]
use super::host::Directory;
use std::{
    ffi::CStr,
    io,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
};

pub const MAX_ENTRIES: usize = 16 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    pub name: Vec<u8>,
}

struct Stream(*mut libc::DIR);
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: this object exclusively owns the successful fdopendir result.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
impl Directory {
    pub fn list(&self) -> io::Result<Vec<Entry>> {
        self.check()?;
        let file = self.enumerator()?;
        let fd = file.into_raw_fd();
        // SAFETY: fd is owned; fdopendir consumes it only on success.
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: a failed fdopendir did not consume fd.
            drop(unsafe { std::fs::File::from_raw_fd(fd) });
            return Err(error);
        }
        let stream = Stream(raw);
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        loop {
            clear_errno();
            // SAFETY: stream is live and exclusively used by this capture.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(error);
                }
                break;
            }
            // SAFETY: readdir supplies a terminated name valid until next readdir.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..")
                || self.private.reserved_name(&self.file, name.to_bytes())?
            {
                continue;
            }
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: retained directory, terminated name, exclusive stat output.
            if unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful fstatat initialized the complete output.
            let metadata = unsafe { metadata.assume_init() };
            if self.private.contains(super::device_id(metadata.st_dev), metadata.st_ino)?
                || self.private.reserved_name(&self.file, name.to_bytes())?
            {
                continue;
            }
            bytes = bytes
                .checked_add(name.to_bytes().len() + std::mem::size_of::<Entry>())
                .ok_or_else(|| io::Error::from_raw_os_error(libc::E2BIG))?;
            if entries.len() == MAX_ENTRIES || bytes > MAX_BYTES {
                return Err(io::Error::from_raw_os_error(libc::E2BIG));
            }
            let kind = match metadata.st_mode & libc::S_IFMT {
                libc::S_IFREG => Kind::File,
                libc::S_IFDIR => Kind::Directory,
                libc::S_IFLNK => Kind::Symlink,
                _ => Kind::Other,
            };
            entries.push(Entry { kind, name: name.to_bytes().to_vec() });
        }
        self.check()?;
        Ok(entries)
    }
}
fn clear_errno() {
    // SAFETY: errno points to storage local to the current thread.
    #[cfg(target_vendor = "apple")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded filesystem capture behavior.")]
mod tests {
    use super::*;
    use crate::engine::files::{authority::Authority, host::PrivateObjects};
    use std::{fs, os::unix::fs::symlink, sync::Arc};
    #[test]
    fn captures_preserve_kinds_and_names_independently_of_later_changes() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().canonicalize().unwrap();
        fs::write(path.join("file"), b"body").unwrap();
        fs::create_dir(path.join("directory")).unwrap();
        fs::create_dir(path.join("private")).unwrap();
        symlink("absent", path.join("broken")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(path.join("socket")).unwrap();
        let private = Arc::new(PrivateObjects::default());
        private.protect(Arc::new(fs::File::open(path.join("private")).unwrap())).unwrap();
        let authority = Arc::new(Authority::new(false));
        let directory = Directory::root(&path, private, authority.clone()).unwrap();
        let mut captured = directory.list().unwrap();
        captured.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            captured,
            vec![
                Entry { name: b"broken".to_vec(), kind: Kind::Symlink },
                Entry { name: b"directory".to_vec(), kind: Kind::Directory },
                Entry { name: b"file".to_vec(), kind: Kind::File },
                Entry { name: b"socket".to_vec(), kind: Kind::Other }
            ]
        );
        fs::remove_file(path.join("file")).unwrap();
        assert_eq!(captured.len(), 4);
        assert_eq!(directory.list().unwrap().len(), 3);
        authority.revoke().unwrap();
        assert!(directory.list().is_err());
    }
}
