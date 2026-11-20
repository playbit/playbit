//! Worker-only descriptor-relative traversal. Guest admission and lifecycle
//! enforcement belong to the engine resource owner, not these I/O primitives.
#![allow(
    unsafe_code,
    reason = "POSIX opens retain directory objects and reject symlink traversal."
)]
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::Path,
    sync::{Arc, RwLock},
};

use super::authority::Authority;

/// Shared across every host grant, including mounts of the filesystem root.
/// Retained descriptors keep protected identities from being reused.
#[derive(Debug, Default)]
pub struct PrivateObjects {
    objects: RwLock<BTreeMap<(u64, u64), Protected>>,
    names: RwLock<BTreeMap<PrivateName, ReservedName>>,
    pub(super) journal: std::sync::OnceLock<Arc<super::private_storage::Journal>>,
    pub(super) snapshot_budget: super::snapshot::SnapshotBudget,
    pub(super) staging_budget: super::staging::Budget,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PrivateName {
    directory: (u64, u64),
    name: Vec<u8>,
}
#[derive(Debug)]
struct ReservedName {
    _directory: Arc<File>,
    snapshot: Option<Arc<super::snapshot::Charge>>,
    staging: Option<Arc<super::staging::Charge>>,
}
/// Created before any named private object. Failed cleanup retains the name
/// reservation, bounded by the same admission ceiling, until engine restart.
/// All construction and drop happen on a filesystem worker.
pub(super) struct NameReservation {
    registry: Arc<PrivateObjects>,
    directory: Arc<File>,
    key: PrivateName,
}
impl NameReservation {
    pub(super) fn charge_staging(&self, charge: Arc<super::staging::Charge>) -> io::Result<()> {
        let mut names = self
            .registry
            .names
            .write()
            .map_err(|_| io::Error::other("private-name lock poisoned"))?;
        let reserved = names.get_mut(&self.key).expect("live reservation retains its name");
        assert!(reserved.staging.is_none());
        reserved.staging = Some(charge);
        Ok(())
    }

    pub(super) fn charge_snapshot(&self, charge: Arc<super::snapshot::Charge>) -> io::Result<()> {
        let mut names = self
            .registry
            .names
            .write()
            .map_err(|_| io::Error::other("private-name lock poisoned"))?;
        let reserved = names.get_mut(&self.key).expect("live reservation retains its name");
        assert!(reserved.snapshot.is_none());
        reserved.snapshot = Some(charge);
        Ok(())
    }
}
impl Drop for NameReservation {
    fn drop(&mut self) {
        let name = CString::new(self.key.name.clone()).expect("reserved name excludes NUL");
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: retained directory, validated terminated component and output.
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                status.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            self.registry
                .names
                .write()
                .expect("private-name lock is not poisoned")
                .remove(&self.key);
        }
        // An existing object, or uncertain I/O result, remains inaccessible.
    }
}
#[derive(Debug)]
struct Protected {
    _file: Arc<File>,
    _snapshot: Option<Arc<super::snapshot::Charge>>,
    _staging: Option<Arc<super::staging::Charge>>,
}
impl PrivateObjects {
    pub(super) fn reserve_name(
        self: &Arc<Self>,
        directory: Arc<File>,
        name: &[u8],
    ) -> io::Result<NameReservation> {
        if components(name)?.len() != 1 || name.len() > 255 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let key = PrivateName { directory: (metadata.dev(), metadata.ino()), name: name.to_vec() };
        let mut names =
            self.names.write().map_err(|_| io::Error::other("private-name lock poisoned"))?;
        if names.len() == 128 {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        if names.contains_key(&key) {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        names.insert(
            key.clone(),
            ReservedName { _directory: directory.clone(), snapshot: None, staging: None },
        );
        Ok(NameReservation { registry: self.clone(), directory, key })
    }
    pub(super) fn reserved_name(&self, directory: &File, name: &[u8]) -> io::Result<bool> {
        let metadata = directory.metadata()?;
        let key = PrivateName { directory: (metadata.dev(), metadata.ino()), name: name.to_vec() };
        Ok(self
            .names
            .read()
            .map_err(|_| io::Error::other("private-name lock poisoned"))?
            .contains_key(&key))
    }
    pub fn protect(&self, file: Arc<File>) -> io::Result<()> {
        let metadata = file.metadata()?;
        let id = (metadata.dev(), metadata.ino());
        let mut objects =
            self.objects.write().map_err(|_| io::Error::other("private-object lock poisoned"))?;
        if objects.len() == 4096 && !objects.contains_key(&id) {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        objects.entry(id).or_insert(Protected { _file: file, _snapshot: None, _staging: None });
        Ok(())
    }
    pub(super) fn protect_snapshot(
        &self,
        file: Arc<File>,
        charge: Arc<super::snapshot::Charge>,
    ) -> io::Result<()> {
        let metadata = file.metadata()?;
        let id = (metadata.dev(), metadata.ino());
        let mut objects =
            self.objects.write().map_err(|_| io::Error::other("private-object lock poisoned"))?;
        if objects.len() == 4096 || objects.contains_key(&id) {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        objects.insert(id, Protected { _file: file, _snapshot: Some(charge), _staging: None });
        Ok(())
    }
    pub(super) fn protect_staging(
        &self,
        file: Arc<File>,
        charge: Arc<super::staging::Charge>,
    ) -> io::Result<()> {
        let metadata = file.metadata()?;
        let id = (metadata.dev(), metadata.ino());
        let mut objects =
            self.objects.write().map_err(|_| io::Error::other("private-object lock poisoned"))?;
        if objects.len() == 4096 || objects.contains_key(&id) {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        objects.insert(id, Protected { _file: file, _snapshot: None, _staging: Some(charge) });
        Ok(())
    }
    pub(super) fn published_staging(&self, device: u64, inode: u64) {
        let mut objects = self.objects.write().expect("private-object lock is not poisoned");
        let entry = objects.get(&(device, inode)).expect("published staging was protected");
        assert!(entry._staging.is_some());
        objects.remove(&(device, inode));
    }
    pub(super) fn release_unlinked(&self, file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if metadata.nlink() == 0 {
            self.objects
                .write()
                .map_err(|_| io::Error::other("private-object lock poisoned"))?
                .remove(&(metadata.dev(), metadata.ino()));
        }
        Ok(())
    }
    pub(super) fn contains(&self, device: u64, inode: u64) -> io::Result<bool> {
        Ok(self
            .objects
            .read()
            .map_err(|_| io::Error::other("private-object lock poisoned"))?
            .contains_key(&(device, inode)))
    }
    fn check(&self, file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if self.contains(metadata.dev(), metadata.ino())? {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct Directory {
    pub(super) file: Arc<File>,
    pub(super) private: Arc<PrivateObjects>,
    pub(super) authority: Arc<Authority>,
}
#[derive(Debug)]
pub struct Reader {
    pub(super) file: Arc<File>,
    private: Arc<PrivateObjects>,
    snapshot: Option<super::snapshot::Snapshot>,
    authority: Arc<Authority>,
}
impl Reader {
    fn check(&self) -> io::Result<()> {
        self.authority.check(false)?;
        if self.snapshot.is_none() {
            self.private.check(&self.file)?;
        }
        Ok(())
    }
    pub fn snapshot(mut self, area: &super::snapshot::SnapshotArea) -> io::Result<Self> {
        self.check()?;
        if let Some(snapshot) = area.capture(&self.file)? {
            self.check()?;
            self.file = snapshot.file.clone();
            self.snapshot = Some(snapshot);
        }
        self.check()?;
        Ok(self)
    }
    pub fn immutable(&self) -> io::Result<bool> {
        self.check()?;
        Ok(self.snapshot.is_some())
    }
    pub fn read(&self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        if length > 64 * 1024
            || offset.checked_add(length as u64).is_none_or(|end| end > 64 * 1024 * 1024)
        {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        self.check()?;
        let mut bytes = vec![0; length];
        let count = loop {
            match self.file.read_at(&mut bytes, offset) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        bytes.truncate(count);
        self.check()?;
        Ok(bytes)
    }
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.check()?;
        let metadata = match &self.snapshot {
            Some(snapshot) => snapshot.metadata.clone(),
            None => self.file.metadata()?,
        };
        self.check()?;
        Ok(metadata)
    }
}
impl Directory {
    pub(super) fn check(&self) -> io::Result<()> {
        self.authority.check(false)?;
        self.private.check(&self.file)?;
        if self.file.metadata()?.nlink() == 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        Ok(())
    }
    pub(super) fn enumerator(&self) -> io::Result<File> {
        self.check()?;
        // SAFETY: retained directory and static terminated dot component. A new
        // open description gives this capture a cursor independent of others.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat transferred this new descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn root(
        path: &Path,
        private: Arc<PrivateObjects>,
        authority: Arc<Authority>,
    ) -> io::Result<Self> {
        authority.check(false)?;
        let bytes = path.as_os_str().as_bytes();
        if bytes.first() != Some(&b'/') || bytes.contains(&0) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut directory = Self {
            private,
            authority,
            file: Arc::new(
                File::options()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open("/")?,
            ),
        };
        directory.private.check(&directory.file)?;
        if bytes.len() > 1 {
            for component in components(&bytes[1..])? {
                directory = directory.child(component)?;
            }
        }
        Ok(directory)
    }
    pub fn child(&self, component: &[u8]) -> io::Result<Self> {
        Ok(Self {
            file: Arc::new(self.open_component(component, libc::O_RDONLY | libc::O_DIRECTORY)?),
            private: self.private.clone(),
            authority: self.authority.clone(),
        })
    }
    pub fn directory(&self, path: &[u8]) -> io::Result<Self> {
        let mut directory = self.clone();
        for part in components(path)? {
            directory = directory.child(part)?;
        }
        Ok(directory)
    }
    pub(super) fn write_parent(&self, path: &[u8]) -> io::Result<(Self, CString, Vec<Arc<File>>)> {
        let parts = components(path)?;
        self.authority.check(true)?;
        let mut parent = self.clone();
        let mut created = Vec::new();
        for part in &parts[..parts.len() - 1] {
            let child = match parent.child(part) {
                Ok(child) => child,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    let name = CString::new(*part)?;
                    parent.check()?;
                    parent.authority.admit_publication()?;
                    // SAFETY: retained parent and one validated component. This
                    // visible creation uses ordinary umask/ACL policy, not staging.
                    let result =
                        unsafe { libc::mkdirat(parent.file.as_raw_fd(), name.as_ptr(), 0o777) };
                    if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                    {
                        return Err(io::Error::last_os_error());
                    }
                    let child = parent.child(part)?;
                    if result == 0 {
                        created.push(parent.file.clone());
                    }
                    child
                }
                Err(error) => return Err(error),
            };
            parent = child;
        }
        Ok((parent, CString::new(*parts.last().expect("validated path has a component"))?, created))
    }
    pub(super) fn entry_kind(&self, component: &[u8]) -> io::Result<Option<libc::mode_t>> {
        self.check()?;
        if components(component)?.len() != 1 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if self.private.reserved_name(&self.file, component)? {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        let name = CString::new(component)?;
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: retained parent, terminated component, exclusive output; no follow.
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                status.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) { Ok(None) } else { Err(error) };
        }
        // SAFETY: successful fstatat initialized status.
        let status = unsafe { status.assume_init() };
        if self.private.contains(super::device_id(status.st_dev), status.st_ino)? {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        self.authority.check(false)?;
        Ok(Some(status.st_mode & libc::S_IFMT))
    }
    pub fn reader(&self, path: &[u8]) -> io::Result<Reader> {
        let parts = components(path)?;
        let mut parent = self.clone();
        for part in &parts[..parts.len() - 1] {
            parent = parent.child(part)?;
        }
        let name = parts.last().expect("validated path has a final component");
        match parent.entry_kind(name)? {
            Some(libc::S_IFREG) => {}
            Some(libc::S_IFDIR) => return Err(io::Error::from_raw_os_error(libc::EISDIR)),
            Some(libc::S_IFLNK) => return Err(io::Error::from_raw_os_error(libc::ELOOP)),
            Some(_) => return Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
            None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
        let file = parent.open_component(name, libc::O_RDONLY)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::from_raw_os_error(if metadata.is_dir() {
                libc::EISDIR
            } else {
                libc::ENOTSUP
            }));
        }
        if metadata.len() > playbit_api::abi::files::MAX_FILE_EXTENT {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        self.authority.check(false)?;
        Ok(Reader {
            file: Arc::new(file),
            private: self.private.clone(),
            snapshot: None,
            authority: self.authority.clone(),
        })
    }
    fn open_component(&self, component: &[u8], flags: i32) -> io::Result<File> {
        self.check()?;
        if components(component)?.len() != 1 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if self.private.reserved_name(&self.file, component)? {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        let name = CString::new(component)?;
        // SAFETY: retained parent descriptor, NUL-terminated component, no O_CREAT.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat transfers one new descriptor to this File.
        let file = unsafe { File::from_raw_fd(fd) };
        self.private.check(&file)?;
        if self.private.reserved_name(&self.file, component)? {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        self.authority.check(false)?;
        Ok(file)
    }
}
pub(super) fn components(path: &[u8]) -> io::Result<Vec<&[u8]>> {
    if path.len() > 4096 {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    let mut parts = Vec::new();
    for part in path.split(|byte| *byte == b'/') {
        if matches!(part, b"" | b"." | b"..") || part.contains(&0) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if parts.len() == 64 {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        parts.push(part);
    }
    Ok(parts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert isolated filesystem fixture operations.")]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{ffi::OsStrExt, fs::symlink},
        path::PathBuf,
    };
    struct Fixture(PathBuf, #[allow(dead_code)] tempfile::TempDir);
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            Self(directory.path().canonicalize().unwrap(), directory)
        }
    }
    fn root(path: &Path) -> io::Result<Directory> {
        Directory::root(path, Arc::new(PrivateObjects::default()), Arc::new(Authority::new(false)))
    }
    fn bytes(file: &Reader) -> Vec<u8> {
        file.read(0, 16).unwrap()
    }
    #[test]
    fn symlinks_and_traversal_never_reach_outside_the_grant() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("mount")).unwrap();
        fs::write(f.0.join("secret"), b"outside").unwrap();
        let mount = root(&f.0.join("mount")).unwrap();
        symlink("../secret", f.0.join("mount/link")).unwrap();
        symlink("..", f.0.join("mount/up")).unwrap();
        for path in [
            b"link".as_slice(),
            b"up/secret",
            b"../secret",
            b"a/../secret",
            b"./link",
            b"/secret",
            b"link/",
            b"",
            b"a//b",
            b"secret\0junk",
        ] {
            assert!(mount.reader(path).is_err(), "{path:?}");
        }
        assert!(root(&f.0.join("mount/up")).is_err());
    }
    #[test]
    fn handles_follow_objects_after_parent_moves_and_path_replacement() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("mount")).unwrap();
        fs::create_dir(f.0.join("mount/dir")).unwrap();
        fs::write(f.0.join("mount/dir/read"), b"original").unwrap();
        let mount = root(&f.0.join("mount")).unwrap();
        let directory = mount.child(b"dir").unwrap();
        let reader = directory.reader(b"read").unwrap();
        fs::rename(f.0.join("mount/dir"), f.0.join("moved")).unwrap();
        fs::create_dir(f.0.join("mount/dir")).unwrap();
        fs::write(f.0.join("mount/dir/read"), b"decoy").unwrap();
        assert_eq!(bytes(&directory.reader(b"read").unwrap()), b"original");
        fs::rename(f.0.join("moved/read"), f.0.join("moved/old")).unwrap();
        fs::write(f.0.join("moved/read"), b"replacement").unwrap();
        assert_eq!(bytes(&reader), b"original");
        assert_eq!(bytes(&directory.reader(b"read").unwrap()), b"replacement");
        assert_eq!(bytes(&mount.reader(b"dir/read").unwrap()), b"decoy");
    }
    #[test]
    fn raw_names_and_nonregular_objects_remain_distinct() {
        let f = Fixture::new();
        let mount = root(&f.0).unwrap();
        #[cfg(target_os = "linux")]
        let raw = b"raw\xff".as_slice();
        #[cfg(target_vendor = "apple")]
        let raw = "raw雪".as_bytes();
        fs::write(f.0.join(std::ffi::OsStr::from_bytes(raw)), b"raw").unwrap();
        assert_eq!(bytes(&mount.reader(raw).unwrap()), b"raw");
        fs::create_dir(f.0.join("dir")).unwrap();
        assert_eq!(mount.reader(b"dir").unwrap_err().raw_os_error(), Some(libc::EISDIR));
        let fifo = CString::new(f.0.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: terminated fixture-local pathname, creation mode only.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(mount.reader(b"fifo").unwrap_err().raw_os_error(), Some(libc::ENOTSUP));
    }
    #[test]
    fn private_identities_and_grant_revocation_apply_to_all_retained_views() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("private")).unwrap();
        fs::write(f.0.join("private/secret"), b"private").unwrap();
        fs::write(f.0.join("public"), b"public").unwrap();
        let private = Arc::new(PrivateObjects::default());
        private.protect(Arc::new(File::open(f.0.join("private")).unwrap())).unwrap();
        private.protect(Arc::new(File::open(f.0.join("private/secret")).unwrap())).unwrap();
        fs::hard_link(f.0.join("private/secret"), f.0.join("alias")).unwrap();
        let authority = Arc::new(Authority::new(false));
        let mount = Directory::root(&f.0, private.clone(), authority.clone()).unwrap();
        assert!(mount.reader(b"private/secret").is_err());
        assert!(mount.reader(b"alias").is_err());
        assert!(Directory::root(&f.0.join("private"), private, authority.clone()).is_err());
        let reader = mount.reader(b"public").unwrap();
        drop(mount);
        assert_eq!(bytes(&reader), b"public");
        authority.revoke().unwrap();
        assert_eq!(reader.read(0, 0).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(reader.metadata().is_err());
    }
    #[test]
    fn private_name_reservations_cover_creation_and_retained_readers_recheck_objects() {
        let f = Fixture::new();
        let private = Arc::new(PrivateObjects::default());
        let mount =
            Directory::root(&f.0, private.clone(), Arc::new(Authority::new(false))).unwrap();
        let reservation = private.reserve_name(mount.file.clone(), b"staging").unwrap();
        assert_eq!(mount.reader(b"staging").unwrap_err().raw_os_error(), Some(libc::EACCES));
        fs::write(f.0.join("staging"), b"private").unwrap();
        assert!(mount.list().unwrap().is_empty());
        drop(reservation);
        assert!(mount.reader(b"staging").is_err(), "failed cleanup retains the reservation");
        assert!(private.reserve_name(mount.file.clone(), b"staging").is_err());

        fs::write(f.0.join("public"), b"public").unwrap();
        let retained = mount.reader(b"public").unwrap();
        assert_eq!(retained.read(0, 6).unwrap(), b"public");
        private.protect(Arc::new(File::open(f.0.join("public")).unwrap())).unwrap();
        assert_eq!(retained.read(0, 0).unwrap_err().raw_os_error(), Some(libc::EACCES));
        assert!(retained.metadata().is_err());
        assert!(retained.immutable().is_err());

        let removed = private.reserve_name(mount.file.clone(), b"removed").unwrap();
        fs::write(f.0.join("removed"), b"private").unwrap();
        fs::remove_file(f.0.join("removed")).unwrap();
        drop(removed);
        assert!(!private.reserved_name(&mount.file, b"removed").unwrap());
    }
}
