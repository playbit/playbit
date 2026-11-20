//! Private working contents. Calls and destruction run on filesystem workers.
//! Sealing transfers ownership; failed writes cannot yield complete recovery.
#![allow(
    unsafe_code,
    reason = "Descriptor-relative creation and cleanup bind private staging names."
)]
use super::{
    authority::Authority,
    host::{NameReservation, PrivateObjects},
};
use std::{
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt},
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_EXTENT: u64 = 64 * 1024 * 1024;
pub const MAX_WRITE: usize = 1024 * 1024;
const MAX_VIEWS: usize = 16;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
#[derive(Debug, Default)]
struct Usage {
    views: usize,
    bytes: u64,
}
#[derive(Debug, Default)]
pub(super) struct Budget(Arc<Mutex<Usage>>);
#[derive(Debug)]
pub(super) struct Charge {
    usage: Arc<Mutex<Usage>>,
    bytes: AtomicU64,
}
impl Charge {
    fn grow(&self, bytes: u64) -> io::Result<()> {
        let mut usage =
            self.usage.lock().map_err(|_| io::Error::other("staging budget poisoned"))?;
        let old = self.bytes.load(Ordering::Relaxed);
        if bytes <= old {
            return Ok(());
        }
        if bytes > MAX_EXTENT || usage.bytes + bytes - old > MAX_BYTES {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        usage.bytes += bytes - old;
        self.bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        let mut usage = self.usage.lock().expect("staging budget is not poisoned");
        let bytes = self.bytes.load(Ordering::Relaxed);
        assert!(usage.views > 0 && usage.bytes >= bytes);
        usage.views -= 1;
        usage.bytes -= bytes;
    }
}
pub struct Area {
    directory: Arc<File>,
    private: Arc<PrivateObjects>,
    // One durably registered empty container covers the create-before-unlink
    // crash window. Serialize only creation, never retained writer operations.
    unlinked: Mutex<Option<super::private_storage::Container>>,
}
impl Area {
    /// Placement and crash cleanup are established before admitting this area.
    /// Host publication also requires an area on the destination filesystem.
    pub fn new(directory: Arc<File>, private: Arc<PrivateObjects>) -> io::Result<Self> {
        super::privacy::directory(&directory)?;
        private.protect(directory.clone())?;
        Ok(Self { directory, private, unlinked: Mutex::new(None) })
    }
    /// Writable destination fallback. Only the new file is private: protecting
    /// this directory would incorrectly hide the user's destination itself.
    /// Name reservation precedes exclusive creation, and ACLs are restricted
    /// before private bytes are accepted.
    pub(super) fn sibling(parent: &super::host::Directory) -> io::Result<Self> {
        parent.authority.check(true)?;
        parent.check()?;
        Ok(Self {
            directory: parent.file.clone(),
            private: parent.private.clone(),
            unlinked: Mutex::new(None),
        })
    }
    pub(super) fn compatible(&self, destination: &File) -> io::Result<()> {
        if self.directory.metadata()?.dev() != destination.metadata()?.dev() {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        Ok(())
    }
    pub fn create(&self, authority: Arc<Authority>) -> io::Result<Writer> {
        self.create_file(authority, false)
    }
    /// Volume publication consumes the descriptor, never a staging pathname.
    /// Unlink before accepting bytes so process exit reclaims private contents
    /// without a per-save journal flush. One retained container record covers
    /// a crash between creating the empty file and unlinking it.
    pub(super) fn create_unlinked(&self, authority: Arc<Authority>) -> io::Result<Writer> {
        self.create_file(authority, true)
    }
    pub(super) fn prepare_unlinked_container(&self) -> io::Result<()> {
        let mut slot =
            self.unlinked.lock().map_err(|_| io::Error::other("unlinked staging lock poisoned"))?;
        self.prepare_unlinked(&mut slot)
    }
    fn prepare_unlinked(
        &self,
        slot: &mut Option<super::private_storage::Container>,
    ) -> io::Result<()> {
        if slot.is_none()
            && let Some(journal) = self.private.journal.get()
        {
            *slot = Some(journal.container(self.directory.clone(), self.private.clone())?);
        }
        Ok(())
    }
    fn reserve_charge(&self) -> io::Result<Arc<Charge>> {
        let charge = {
            let mut usage = self
                .private
                .staging_budget
                .0
                .lock()
                .map_err(|_| io::Error::other("staging budget poisoned"))?;
            if usage.views == MAX_VIEWS {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            usage.views += 1;
            Arc::new(Charge {
                usage: self.private.staging_budget.0.clone(),
                bytes: AtomicU64::new(0),
            })
        };
        Ok(charge)
    }
    fn create_file(&self, authority: Arc<Authority>, unlinked: bool) -> io::Result<Writer> {
        authority.check(true)?;
        let charge = self.reserve_charge()?;
        let mut unlinked_container = if unlinked {
            Some(
                self.unlinked
                    .lock()
                    .map_err(|_| io::Error::other("unlinked staging lock poisoned"))?,
            )
        } else {
            None
        };
        if let Some(slot) = &mut unlinked_container {
            self.prepare_unlinked(slot)?;
        }
        let container = self
            .private
            .journal
            .get()
            .filter(|_| !unlinked)
            .map(|journal| journal.container(self.directory.clone(), self.private.clone()))
            .transpose()?;
        let placement = container
            .as_ref()
            .or_else(|| unlinked_container.as_ref().and_then(|slot| slot.as_ref()));
        let directory = placement.map_or(&self.directory, |container| &container.directory);
        let name = if placement.is_some() {
            c"data".to_owned()
        } else {
            CString::new(format!(
                "write-{}",
                crate::store::Id::generate().map_err(io::Error::other)?
            ))?
        };
        let reservation = self.private.reserve_name(directory.clone(), name.as_bytes())?;
        reservation.charge_staging(charge.clone())?;
        let file = create_private_file(directory, &name)?;
        let mut storage = Storage {
            file,
            directory: directory.clone(),
            name,
            private: self.private.clone(),
            charge,
            reservation: Some(reservation),
            named: true,
            published: false,
            _container: container,
        };
        super::privacy::restrict_new(&storage.file)?;
        storage.private.protect_staging(storage.file.clone(), storage.charge.clone())?;
        if unlinked {
            storage.unlink()?;
            storage.named = false;
            storage.reservation.take();
            if storage.file.metadata()?.nlink() != 0 {
                return Err(io::Error::from_raw_os_error(libc::EMLINK));
            }
        }
        authority.check(true)?;
        Ok(Writer { storage, authority, failed: false, length: 0 })
    }
}
pub(super) struct Storage {
    pub(super) file: Arc<File>,
    pub(super) directory: Arc<File>,
    pub(super) name: CString,
    pub(super) private: Arc<PrivateObjects>,
    charge: Arc<Charge>,
    reservation: Option<NameReservation>,
    named: bool,
    pub(super) published: bool,
    _container: Option<super::private_storage::Container>,
}
impl Storage {
    pub(super) fn check_name(&self) -> io::Result<()> {
        if !self.named {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        let expected = self.file.metadata()?;
        let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: retained parent, terminated component and exclusive output.
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
        // SAFETY: successful fstatat initialized the output.
        let current = unsafe { current.assume_init() };
        if super::device_id(current.st_dev) != expected.dev() || current.st_ino != expected.ino() {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(())
    }
    fn unlink(&self) -> io::Result<()> {
        if !self.named {
            return Ok(());
        }
        match self.check_name() {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
            Err(error) => return Err(error),
        }
        // SAFETY: unlink only the name still referring to our retained object.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        if let Err(error) = self.unlink() {
            eprintln!("Private staging cleanup failed: {error}");
        }
        // Failed cleanup and unexpected aliases retain both protection and charge.
        if let Err(error) = self.private.release_unlinked(&self.file) {
            eprintln!("Private staging release failed: {error}");
        }
    }
}
pub struct Writer {
    storage: Storage,
    authority: Arc<Authority>,
    failed: bool,
    length: u64,
}
impl Writer {
    pub fn fail(&mut self) {
        self.failed = true;
    }
    /// Admission rejects invalid ranges before enqueueing. Repeat validation here
    /// so misuse cannot write a prefix or poison a writer on an invalid request.
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|end| *end <= MAX_EXTENT)
            .filter(|_| bytes.len() <= MAX_WRITE)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))?;
        self.check()?;
        if bytes.is_empty() {
            return Ok(());
        }
        let length = self.length.max(end);
        let result = self
            .storage
            .charge
            .grow(length)
            .and_then(|()| self.storage.file.write_all_at(bytes, offset));
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.length = length;
        Ok(())
    }
    fn check(&self) -> io::Result<()> {
        self.authority.check(true)?;
        if self.failed {
            return Err(io::Error::from_raw_os_error(libc::EPIPE));
        }
        Ok(())
    }
    pub fn length(&self) -> io::Result<u64> {
        self.check()?;
        Ok(self.length)
    }
    /// The resource owner seals admission first and finishes earlier accepted work.
    /// Consuming this object then makes further byte writes impossible.
    pub fn seal(self) -> io::Result<Sealed> {
        self.check()?;
        Ok(Sealed { storage: self.storage, authority: self.authority, length: self.length })
    }
}
pub struct Sealed {
    pub(super) storage: Storage,
    pub(super) authority: Arc<Authority>,
    length: u64,
}
impl Sealed {
    pub fn prepare_metadata(&self, metadata: &super::metadata::Prepared) -> io::Result<()> {
        self.authority.check(true)?;
        metadata.apply(&self.storage.file)
    }
    pub fn recovery(self) -> io::Result<Recovery> {
        self.authority.check(false)?;
        Ok(Recovery { storage: self.storage, authority: self.authority, length: self.length })
    }
}
pub struct Recovery {
    storage: Storage,
    authority: Arc<Authority>,
    length: u64,
}
impl Recovery {
    pub fn length(&self) -> io::Result<u64> {
        self.authority.check(false)?;
        Ok(self.length)
    }
    pub fn read(&self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        self.authority.check(false)?;
        if length > 64 * 1024
            || offset.checked_add(length as u64).is_none_or(|end| end > MAX_EXTENT)
        {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        let count = (self.length.saturating_sub(offset)).min(length as u64) as usize;
        let mut bytes = vec![0; count];
        if count != 0 {
            self.storage.file.read_exact_at(&mut bytes, offset)?;
        }
        self.authority.check(false)?;
        Ok(bytes)
    }
}
fn create_private_file(directory: &File, name: &std::ffi::CStr) -> io::Result<Arc<File>> {
    // SAFETY: retained private parent and fresh validated component; no symlink follow.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat transferred this fresh descriptor.
    let file = Arc::new(unsafe { File::from_raw_fd(fd) });
    Ok(file)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests exercise private working views and exact storage quotas."
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn area() -> (tempfile::TempDir, Area, Arc<Authority>) {
        let fixture = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let area = Area::new(
            Arc::new(File::open(fixture.path()).unwrap()),
            Arc::new(PrivateObjects::default()),
        )
        .unwrap();
        (fixture, area, Arc::new(Authority::new(true)))
    }
    #[test]
    fn writes_preserve_tail_fill_holes_and_transfer_only_complete_recovery() {
        let (fixture, area, authority) = area();
        let mut writer = area.create(authority.clone()).unwrap();
        writer.write(0, b"abc").unwrap();
        writer.write(6, b"XY").unwrap();
        writer.write(1, b"Z").unwrap();
        writer.write(MAX_EXTENT, b"").unwrap();
        assert_eq!(writer.length().unwrap(), 8);
        assert!(writer.write(u64::MAX, b"X").is_err());
        assert_eq!(writer.length().unwrap(), 8, "invalid ranges do not poison staging");
        let recovery = writer.seal().unwrap().recovery().unwrap();
        assert_eq!(recovery.read(0, 16).unwrap(), b"aZc\0\0\0XY");
        assert!(recovery.read(8, 1).unwrap().is_empty());
        assert_eq!(area.private.staging_budget.0.lock().unwrap().bytes, 8);
        authority.revoke().unwrap();
        assert!(recovery.read(0, 0).is_err());
        assert!(recovery.length().is_err());
        drop(recovery);
        assert_eq!(area.private.staging_budget.0.lock().unwrap().bytes, 0);
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }
    #[test]
    fn unlinked_volume_staging_keeps_recovery_and_credit_without_named_contents() {
        let (fixture, area, authority) = area();
        let mut writer = area.create_unlinked(authority).unwrap();
        assert_eq!(writer.storage.file.metadata().unwrap().nlink(), 0);
        assert!(writer.storage.check_name().is_err());
        writer.write(0, b"private volume contents").unwrap();
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
        let recovery = writer.seal().unwrap().recovery().unwrap();
        assert_eq!(recovery.read(0, 64).unwrap(), b"private volume contents");
        assert_eq!(area.private.staging_budget.0.lock().unwrap().bytes, 23);
        drop(recovery);
        assert_eq!(area.private.staging_budget.0.lock().unwrap().views, 0);
        assert_eq!(area.private.staging_budget.0.lock().unwrap().bytes, 0);
    }
    #[test]
    fn quota_failure_poisoning_and_discard_reclaim_all_retained_credit() {
        let (fixture, area, authority) = area();
        let mut writers =
            (0..MAX_VIEWS).map(|_| area.create(authority.clone()).unwrap()).collect::<Vec<_>>();
        assert!(area.create(authority.clone()).is_err());
        for writer in &mut writers[..8] {
            writer.write(MAX_EXTENT - 1, b"X").unwrap();
        }
        assert!(writers[8].write(0, b"X").is_err());
        assert!(writers[8].length().is_err());
        assert!(writers[8].write(0, b"").is_err());
        assert!(
            writers.remove(8).seal().is_err(),
            "partial failed writes never yield complete recovery"
        );
        drop(writers);
        assert_eq!(area.private.staging_budget.0.lock().unwrap().views, 0);
        assert_eq!(area.private.staging_budget.0.lock().unwrap().bytes, 0);
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
        assert!(area.create(authority).is_ok());
    }
}
