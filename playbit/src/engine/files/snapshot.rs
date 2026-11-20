//! Copy-on-write snapshots in an already admitted runtime-private directory.
//! A failed clone never falls back unless its precise error means unsupported.
#![allow(
    unsafe_code,
    reason = "Descriptor-based clone/open/unlink calls retain source and private placement."
)]
use super::host::PrivateObjects;
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const MAX_VIEWS: usize = 32;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Debug, Default)]
pub(super) struct Usage {
    views: usize,
    bytes: u128,
}
#[derive(Debug, Default)]
pub(super) struct SnapshotBudget(pub(super) Arc<Mutex<Usage>>);
#[derive(Debug)]
pub(super) struct Charge {
    usage: Arc<Mutex<Usage>>,
    bytes: AtomicU64,
}
impl Charge {
    fn resize(&self, bytes: u64) -> io::Result<()> {
        let mut usage =
            self.usage.lock().map_err(|_| io::Error::other("snapshot budget poisoned"))?;
        // Record even an oversized clone before rejecting it. A concurrently
        // growing source may exceed admission's estimate; failed cleanup must
        // retain that entire observed extent and block further admissions.
        let previous = self.bytes.load(Ordering::Relaxed);
        let total = usage.bytes - u128::from(previous) + u128::from(bytes);
        usage.bytes = total;
        self.bytes.store(bytes, Ordering::Relaxed);
        if bytes > MAX_FILE_BYTES || total > u128::from(MAX_BYTES) {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        Ok(())
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        let mut usage = self.usage.lock().expect("snapshot budget is not poisoned");
        let bytes = u128::from(self.bytes.load(Ordering::Relaxed));
        assert!(usage.views > 0 && usage.bytes >= bytes);
        usage.views -= 1;
        usage.bytes -= bytes;
    }
}
#[derive(Debug)]
pub struct SnapshotArea {
    directory: Arc<File>,
    private: Arc<PrivateObjects>,
    usage: Arc<Mutex<Usage>>,
}
#[derive(Debug)]
pub(super) struct Snapshot {
    pub file: Arc<File>,
    pub metadata: Metadata,
    private: Arc<PrivateObjects>,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        // Unexpected external aliases remain protected and charged. Never
        // expose private data or reuse its budget just because the guest closed.
        if let Err(error) = self.private.release_unlinked(&self.file) {
            eprintln!("Could not release private snapshot: {error}");
        }
    }
}
impl SnapshotArea {
    /// Caller owns this private placement and performs crash cleanup before
    /// admitting grants. This never creates a directory inside a read-only mount.
    pub fn new(directory: Arc<File>, private: Arc<PrivateObjects>) -> io::Result<Self> {
        super::privacy::directory(&directory)?;
        private.protect(directory.clone())?;
        let usage = private.snapshot_budget.0.clone();
        Ok(Self { directory, private, usage })
    }
    fn reserve(&self, bytes: u64) -> io::Result<Charge> {
        let mut usage =
            self.usage.lock().map_err(|_| io::Error::other("snapshot budget poisoned"))?;
        if usage.views == MAX_VIEWS
            || bytes > MAX_FILE_BYTES
            || usage.bytes + u128::from(bytes) > u128::from(MAX_BYTES)
        {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        usage.views += 1;
        usage.bytes += u128::from(bytes);
        Ok(Charge { usage: self.usage.clone(), bytes: AtomicU64::new(bytes) })
    }
    pub(super) fn capture(&self, source: &File) -> io::Result<Option<Snapshot>> {
        if source.metadata()?.len() > MAX_FILE_BYTES {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        // Reserve the maximum supported capture before invoking the filesystem.
        // The named reservation retains this charge even if cloning succeeds but
        // opening or cleanup fails before an inode can be protected.
        let charge = Arc::new(self.reserve(MAX_FILE_BYTES)?);
        let container = self
            .private
            .journal
            .get()
            .map(|journal| journal.container(self.directory.clone(), self.private.clone()))
            .transpose()?;
        let directory =
            container.as_ref().map_or(&self.directory, |container| &container.directory);
        let name = if container.is_some() {
            c"data".to_owned()
        } else {
            CString::new(format!(
                "snapshot-{}",
                crate::store::Id::generate().map_err(io::Error::other)?
            ))?
        };
        let reservation = self.private.reserve_name(directory.clone(), name.as_bytes())?;
        reservation.charge_snapshot(charge.clone())?;
        let Some(file) = clone(source, directory, &name)? else {
            return Ok(None);
        };
        let file = Arc::new(file);
        let mut named = NamedClone { directory, name: &name, file: &file, linked: true };
        let result = (|| {
            let metadata = file.metadata()?;
            self.private.protect_snapshot(file.clone(), charge.clone())?;
            charge.resize(metadata.len())?;
            named.unlink()?;
            Ok(Snapshot { file: file.clone(), metadata, private: self.private.clone() })
        })();
        drop(named);
        if result.is_err() {
            self.private.release_unlinked(&file)?;
        }
        result.map(Some)
    }
}
struct NamedClone<'a> {
    directory: &'a File,
    name: &'a CStr,
    file: &'a File,
    linked: bool,
}
impl NamedClone<'_> {
    fn unlink(&mut self) -> io::Result<()> {
        if !self.linked {
            return Ok(());
        }
        let expected = self.file.metadata()?;
        let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: retained parent, terminated name and exclusive output.
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                current.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized current.
        let current = unsafe { current.assume_init() };
        if super::device_id(current.st_dev) != expected.dev() || current.st_ino != expected.ino() {
            return Err(io::Error::other("private clone name changed identity"));
        }
        // SAFETY: this name still identifies the retained private clone.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.linked = false;
        Ok(())
    }
}
impl Drop for NamedClone<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.unlink() {
            eprintln!("Private clone cleanup failed: {error}");
        }
    }
}
fn unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EXDEV | libc::ENOTSUP | libc::ENOSYS | libc::ENOTTY))
}
fn open_at(parent: &File, name: &CStr, flags: i32) -> io::Result<File> {
    // SAFETY: retained parent, terminated name and explicit creation mode.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat transferred this new descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[cfg(target_vendor = "apple")]
fn clone(source: &File, directory: &File, name: &CStr) -> io::Result<Option<File>> {
    // sys/clonefile.h: do not transfer source ownership, even for a privileged runtime.
    const CLONE_NOOWNERCOPY: u32 = 0x0002;
    // SAFETY: both descriptors are retained and source was classified regular.
    if unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            directory.as_raw_fd(),
            name.as_ptr(),
            CLONE_NOOWNERCOPY,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        return if unsupported(&error) { Ok(None) } else { Err(error) };
    }
    let result = open_at(directory, name, libc::O_RDONLY);
    if result.is_err() {
        // SAFETY: remove only the name created by this successful clone call.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            // The name reservation retains storage credit until cleanup is
            // established, including this failure before descriptor adoption.
            return Err(io::Error::last_os_error());
        }
    }
    result.map(Some)
}
#[cfg(target_os = "linux")]
fn clone(source: &File, directory: &File, name: &CStr) -> io::Result<Option<File>> {
    let file = open_at(directory, name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
    // SAFETY: FICLONE consumes integer descriptors, both live regular files.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: this call exclusively created the private name.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        return if unsupported(&error) { Ok(None) } else { Err(error) };
    }
    Ok(Some(file))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert snapshot isolation and resource accounting.")]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, FileExt};
    #[test]
    fn only_unsupported_clone_errors_allow_a_mutable_view() {
        for code in [libc::EXDEV, libc::ENOTSUP, libc::ENOSYS, libc::ENOTTY] {
            assert!(unsupported(&io::Error::from_raw_os_error(code)));
        }
        for code in [
            libc::ENOSPC,
            libc::EDQUOT,
            libc::EACCES,
            libc::EPERM,
            libc::EIO,
            libc::EINVAL,
            libc::EEXIST,
        ] {
            assert!(!unsupported(&io::Error::from_raw_os_error(code)));
        }
    }
    #[test]
    fn snapshot_capture_releases_storage_on_close_or_unsupported() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        std::fs::write(&source_path, b"original").unwrap();
        let private_path = fixture.path().join("private");
        std::fs::DirBuilder::new().mode(0o700).create(&private_path).unwrap();
        let private = Arc::new(PrivateObjects::default());
        let area = SnapshotArea::new(Arc::new(File::open(&private_path).unwrap()), private.clone())
            .unwrap();
        let source = File::open(&source_path).unwrap();
        if let Some(snapshot) = area.capture(&source).unwrap() {
            assert!(private.contains(snapshot.metadata.dev(), snapshot.metadata.ino()).unwrap());
            std::fs::write(&source_path, b"changed!").unwrap();
            let mut bytes = [0; 8];
            snapshot.file.read_exact_at(&mut bytes, 0).unwrap();
            assert_eq!(&bytes, b"original");
            assert_eq!(area.usage.lock().unwrap().bytes, 8);
            drop(snapshot);
        } else {
            // Unsupported cloning is explicit and must not leave a private
            // file or retain reserved capacity.
            assert_eq!(std::fs::read(&source_path).unwrap(), b"original");
        }
        assert_eq!(std::fs::read_dir(&private_path).unwrap().count(), 0);
        assert_eq!(area.usage.lock().unwrap().bytes, 0);
        assert_eq!(area.usage.lock().unwrap().views, 0);
        let leases = (0..MAX_VIEWS).map(|_| area.reserve(1).unwrap()).collect::<Vec<_>>();
        assert!(area.reserve(0).is_err());
        let other_area =
            SnapshotArea::new(Arc::new(File::open(&private_path).unwrap()), private).unwrap();
        assert!(
            other_area.reserve(0).is_err(),
            "all private placements share the engine snapshot budget"
        );
        drop(leases);
        let leases = (0..4).map(|_| area.reserve(MAX_FILE_BYTES).unwrap()).collect::<Vec<_>>();
        assert!(area.reserve(1).is_err());
        drop(leases);
        assert!(area.reserve(1).is_ok());
    }
}

#[cfg(test)]
#[test]
#[allow(clippy::unwrap_used, reason = "Tests verify failed cleanup retains snapshot credit.")]
fn failed_cleanup_keeps_name_and_full_snapshot_charge() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let private = Arc::new(PrivateObjects::default());
    let directory = Arc::new(File::open(fixture.path()).unwrap());
    let area = SnapshotArea::new(directory.clone(), private.clone()).unwrap();
    let usage = area.usage.clone();
    let charge = Arc::new(area.reserve(MAX_FILE_BYTES).unwrap());
    let reserved = private.reserve_name(directory.clone(), b"incomplete").unwrap();
    reserved.charge_snapshot(charge.clone()).unwrap();
    std::fs::write(fixture.path().join("incomplete"), []).unwrap();
    // Model a clone which outgrew the pre-call source metadata. Its whole
    // observed extent remains charged even though admission fails.
    assert!(charge.resize(MAX_BYTES + 1).is_err());
    drop(charge);
    drop(reserved);
    assert!(private.reserved_name(&directory, b"incomplete").unwrap());
    assert_eq!(usage.lock().unwrap().bytes, u128::from(MAX_BYTES) + 1);
    assert!(area.reserve(0).is_err());
    drop(area);
    assert_eq!(usage.lock().unwrap().views, 1);
    drop(private);
    assert_eq!(usage.lock().unwrap().views, 0);
    assert_eq!(usage.lock().unwrap().bytes, 0);
}
