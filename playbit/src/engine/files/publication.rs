//! Host publication from completed private contents. Only filesystem workers
//! enter this module; the engine retains accepted work and close result credit.
#![allow(
    unsafe_code,
    reason = "Descriptor-relative publication and creation-policy probes retain authority bindings."
)]
use super::{
    host::Directory,
    metadata::Prepared,
    staging::{Area, Recovery, Sealed, Writer},
};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    sync::{Arc, Mutex, Weak},
};

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Key {
    directory: (u64, u64),
    name: Vec<u8>,
}
struct Object {
    file: Arc<File>,
    identity: (u64, u64),
}
impl Object {
    fn new(file: Arc<File>) -> io::Result<Self> {
        let status = file.metadata()?;
        if !status.is_file() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(Self { file, identity: (status.dev(), status.ino()) })
    }
}
type Binding = Arc<Mutex<Option<Object>>>;
/// Shared across mounts. Keys are destination entries, never just file inodes;
/// writers for different hard links cannot redirect each other's destinations.
#[derive(Default)]
pub struct Registry(Mutex<BTreeMap<Key, Weak<Mutex<Option<Object>>>>>);
impl Registry {
    fn bind(
        &self,
        parent: &Directory,
        name: &CString,
        mut object: Option<Object>,
    ) -> io::Result<Binding> {
        let status = parent.file.metadata()?;
        let key = Key { directory: (status.dev(), status.ino()), name: name.as_bytes().to_vec() };
        let mut entries =
            self.0.lock().map_err(|_| io::Error::other("publication registry poisoned"))?;
        entries.retain(|_, value| value.strong_count() != 0);
        if object.is_some()
            && let Some(binding) = entries.get(&key).and_then(Weak::upgrade)
        {
            drop(entries);
            let current =
                binding.lock().map_err(|_| io::Error::other("destination lock poisoned"))?;
            // A local publication may finish while this open waits. Resolve
            // once more under its binding lock before returning the writer.
            let actual = Object::new(parent.reader(name.as_bytes())?.file)?;
            let matches = current.as_ref().is_some_and(|value| value.identity == actual.identity);
            drop(current);
            entries =
                self.0.lock().map_err(|_| io::Error::other("publication registry poisoned"))?;
            // Another opener can install a binding while this worker checks
            // the destination. Never replace that newer binding: doing so
            // would split writers to the same runtime replacement.
            if entries.get(&key).is_none_or(|value| !Weak::ptr_eq(value, &Arc::downgrade(&binding)))
            {
                return Err(io::Error::from_raw_os_error(libc::EAGAIN));
            }
            if matches {
                return Ok(binding);
            }
            object = Some(actual);
        }
        if entries.len() == 16 && !entries.contains_key(&key) {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let binding = Arc::new(Mutex::new(object));
        entries.insert(key, Arc::downgrade(&binding));
        Ok(binding)
    }
}
pub struct HostWriter {
    writer: Writer,
    parent: Directory,
    name: CString,
    binding: Binding,
    ancestors: Vec<Arc<File>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Publication {
    NotPublished,
    PublishedDurabilityUnknown,
    Unknown,
}
pub struct Failure {
    pub reason: io::Error,
    pub publication: Publication,
    pub recovery: Option<Box<Recovery>>,
}
impl std::fmt::Debug for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Failure")
            .field("reason", &self.reason)
            .field("publication", &self.publication)
            .field("recovery", &self.recovery.is_some())
            .finish()
    }
}
impl HostWriter {
    /// Caller selects an admitted private area compatible with this destination.
    /// This never truncates or changes metadata on the published destination.
    pub fn open(
        parent: &Directory,
        path: &[u8],
        exclusive: bool,
        preferred: Option<&Area>,
        registry: &Registry,
    ) -> io::Result<Self> {
        let (parent, name, ancestors) = parent.write_parent(path)?;
        let compatible = match preferred {
            Some(area) => match area.compatible(&parent.file) {
                Ok(()) => Some(area),
                Err(error) if error.raw_os_error() == Some(libc::EXDEV) => None,
                Err(error) => return Err(error),
            },
            None => None,
        };
        let sibling = if compatible.is_none() { Some(Area::sibling(&parent)?) } else { None };
        let area = compatible.or(sibling.as_ref()).expect("private or sibling placement selected");
        if exclusive && parent.entry_kind(name.as_bytes())?.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        let object = match parent.reader(name.as_bytes()) {
            Ok(reader) => {
                if exclusive {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
                Some(Object::new(reader.file)?)
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => None,
            Err(error) => return Err(error),
        };
        let binding = registry.bind(&parent, &name, object)?;
        let writer = area.create(parent.authority.clone())?;
        Ok(Self { writer, parent, name, binding, ancestors })
    }
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.writer.write(offset, bytes)
    }
    pub fn length(&self) -> io::Result<u64> {
        self.writer.length()
    }
    /// Resource admission already validates flags and seals the guest handle.
    /// DISCARD simply drops this worker-owned object after prior work retires.
    pub fn close(self, sync: bool) -> Result<(), Failure> {
        let Self { writer, parent, name, binding, ancestors } = self;
        let mut sealed = writer.seal().map_err(|reason| Failure {
            reason,
            publication: Publication::NotPublished,
            recovery: None,
        })?;
        let mut current = match binding.lock() {
            Ok(current) => current,
            Err(_) => return Err(recover(sealed, io::Error::other("destination lock poisoned"))),
        };
        let prepared = prepare_publication(&parent, &name, current.as_ref(), &sealed, sync);
        let published = match prepared {
            Ok(value) => value,
            Err(error) => return Err(recover(sealed, error)),
        };
        let result = rename(&sealed, &parent, &name, current.is_none());
        if let Err(reason) = result {
            if definitely_not_published(&reason) {
                return Err(recover(sealed, reason));
            }
            // An uncertain rename may already expose this inode to external
            // writes. A retained descriptor is not immutable recovery.
            return Err(Failure { reason, publication: Publication::Unknown, recovery: None });
        }
        sealed.storage.published = true;
        *current = Some(published);
        let published = current.as_ref().expect("successful publication installs its identity");
        let completion = (|| {
            sealed.storage.private.published_staging(published.identity.0, published.identity.1);
            if sync {
                parent.file.sync_all()?;
                for ancestor in ancestors.iter().rev() {
                    ancestor.sync_all()?;
                }
                // Namespace fsync precedes the final device-cache flush on Darwin.
                flush_file(&sealed.storage.file)?;
            }
            Ok(())
        })();
        completion.map_err(|reason| Failure {
            reason,
            publication: Publication::PublishedDurabilityUnknown,
            recovery: None,
        })
    }
}
fn prepare_publication(
    parent: &Directory,
    name: &CString,
    current: Option<&Object>,
    sealed: &Sealed,
    sync: bool,
) -> io::Result<Object> {
    parent.check()?;
    let metadata = match current {
        Some(expected) => {
            let actual = Object::new(parent.reader(name.as_bytes())?.file)?;
            if actual.identity != expected.identity {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            Prepared::capture(&actual.file)?
        }
        None => creation_policy(parent)?,
    };
    sealed.prepare_metadata(&metadata)?;
    if sync {
        flush_file(&sealed.storage.file)?;
    }
    // Preparation may involve slow I/O. Recheck identity and authority
    // at publication. The external check/rename race remains documented.
    parent.check()?;
    if let Some(expected) = current {
        let actual = Object::new(parent.reader(name.as_bytes())?.file)?;
        if actual.identity != expected.identity {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
    }
    sealed.storage.check_name()?;
    let published = Object::new(sealed.storage.file.clone())?;
    parent.authority.admit_publication()?;
    Ok(published)
}

fn recover(sealed: Sealed, reason: io::Error) -> Failure {
    Failure {
        reason,
        publication: Publication::NotPublished,
        recovery: sealed.recovery().ok().map(Box::new),
    }
}
fn creation_policy(parent: &Directory) -> io::Result<Prepared> {
    let name = CString::new(format!(
        "policy-{}",
        crate::store::Id::generate().map_err(io::Error::other)?
    ))?;
    let _reservation = parent.private.reserve_name(parent.file.clone(), name.as_bytes())?;
    parent.authority.check(true)?;
    // SAFETY: fresh empty probe, normal destination creation permissions. No
    // private contents are ever written into this broadly accessible object.
    let fd = unsafe {
        libc::openat(
            parent.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o666,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat transferred this descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let result = Prepared::creation_policy(&file);
    let expected = file.metadata()?;
    let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: retained parent and live output; inspect the probe without following links.
    if unsafe {
        libc::fstatat(
            parent.file.as_raw_fd(),
            name.as_ptr(),
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
        return Err(io::Error::other("creation probe name changed identity"));
    }
    // SAFETY: the name still refers to our exclusively created empty probe.
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    result
}
fn flush_file(file: &File) -> io::Result<()> {
    file.sync_all()?;
    #[cfg(target_vendor = "apple")]
    // SAFETY: retained regular staging/published file, no pointer argument.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn rename(sealed: &Sealed, parent: &Directory, name: &CString, exclusive: bool) -> io::Result<()> {
    // SAFETY: retained private source/destination parents, terminated names.
    #[cfg(target_vendor = "apple")]
    let result = unsafe {
        libc::renameatx_np(
            sealed.storage.directory.as_raw_fd(),
            sealed.storage.name.as_ptr(),
            parent.file.as_raw_fd(),
            name.as_ptr(),
            if exclusive { libc::RENAME_EXCL } else { 0 },
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            sealed.storage.directory.as_raw_fd(),
            sealed.storage.name.as_ptr(),
            parent.file.as_raw_fd(),
            name.as_ptr(),
            if exclusive { libc::RENAME_NOREPLACE } else { 0 },
        )
    };
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
fn definitely_not_published(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EEXIST
                | libc::ENOENT
                | libc::EACCES
                | libc::EPERM
                | libc::EINVAL
                | libc::ENOTDIR
                | libc::EISDIR
                | libc::EXDEV
                | libc::EROFS
                | libc::ENOSPC
                | libc::EDQUOT
                | libc::ENAMETOOLONG
                | libc::ENOSYS
                | libc::ENOTSUP
        )
    )
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests publish only in isolated temporary host roots.")]
mod tests {
    use super::super::{authority::Authority, host::PrivateObjects};
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    #[test]
    fn atomic_creation_discard_late_conflict_and_moved_parent_keep_their_bindings() {
        let fixture = tempfile::tempdir().unwrap();
        let private = Arc::new(PrivateObjects::default());
        let authority = Arc::new(Authority::new(true));
        let base = fixture.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("root")).unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(base.join("private")).unwrap();
        let area = Area::new(Arc::new(File::open(base.join("private")).unwrap()), private.clone())
            .unwrap();
        let root = Directory::root(&base.join("root"), private, authority).unwrap();
        let registry = Registry::default();
        std::fs::create_dir(base.join("root/occupied")).unwrap();
        std::os::unix::fs::symlink("occupied", base.join("root/link")).unwrap();
        for name in [b"occupied".as_slice(), b"link"] {
            assert_eq!(
                HostWriter::open(&root, name, true, Some(&area), &registry)
                    .err()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::EEXIST)
            );
        }
        let mut a = HostWriter::open(&root, b"dir/file", false, Some(&area), &registry).unwrap();
        let mut b = HostWriter::open(&root, b"dir/file", false, Some(&area), &registry).unwrap();
        a.write(0, b"first").unwrap();
        b.write(0, b"second").unwrap();
        assert!(!base.join("root/dir/file").exists());
        a.close(true).unwrap();
        assert_eq!(std::fs::read(base.join("root/dir/file")).unwrap(), b"first");
        assert_eq!(root.reader(b"dir/file").unwrap().read(0, 16).unwrap(), b"first");
        let conflict = b.close(false).unwrap_err();
        assert_eq!(conflict.publication, Publication::NotPublished);
        assert_eq!(conflict.recovery.unwrap().read(0, 16).unwrap(), b"second");
        assert!(HostWriter::open(&root, b"dir/file", true, Some(&area), &registry).is_err());
        let discard =
            HostWriter::open(&root, b"other/empty", false, Some(&area), &registry).unwrap();
        drop(discard);
        assert!(base.join("root/other").is_dir());
        assert!(!base.join("root/other/empty").exists());
        let mut moved = HostWriter::open(&root, b"dir/new", true, Some(&area), &registry).unwrap();
        moved.write(0, b"moved").unwrap();
        std::fs::rename(base.join("root/dir"), base.join("moved")).unwrap();
        std::fs::create_dir(base.join("root/dir")).unwrap();
        moved.close(false).unwrap();
        assert_eq!(std::fs::read(base.join("moved/new")).unwrap(), b"moved");
        assert!(!base.join("root/dir/new").exists());
        std::fs::write(base.join("moved/ordinary"), []).unwrap();
        let ordinary_mode =
            std::fs::metadata(base.join("moved/ordinary")).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            std::fs::metadata(base.join("moved/new")).unwrap().permissions().mode() & 0o777,
            ordinary_mode
        );
        let mut tampered =
            HostWriter::open(&root, b"tampered", false, Some(&area), &registry).unwrap();
        tampered.write(0, b"complete private contents").unwrap();
        let staging =
            std::fs::read_dir(base.join("private")).unwrap().next().unwrap().unwrap().path();
        std::fs::rename(&staging, base.join("retained-private")).unwrap();
        std::fs::write(&staging, b"unrelated replacement").unwrap();
        let failed = tampered.close(false).unwrap_err();
        assert_eq!(failed.reason.raw_os_error(), Some(libc::ESTALE));
        assert_eq!(failed.publication, Publication::NotPublished);
        assert_eq!(failed.recovery.unwrap().read(0, 64).unwrap(), b"complete private contents");
        assert!(!base.join("root/tampered").exists());
        assert_eq!(std::fs::read(&staging).unwrap(), b"unrelated replacement");
        assert!(root.reader(b"../retained-private").is_err());
        let mut revoked =
            HostWriter::open(&root, b"withdrawn", false, Some(&area), &registry).unwrap();
        revoked.write(0, b"private").unwrap();
        root.authority.revoke().unwrap();
        let failed = revoked.close(false).unwrap_err();
        assert_eq!(failed.publication, Publication::NotPublished);
        assert!(failed.recovery.is_none());
        assert!(!base.join("root/withdrawn").exists());
    }
}
