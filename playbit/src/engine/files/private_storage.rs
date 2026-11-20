//! Worker-owned private placement and crash reclamation. A durable record binds
//! each data container to its directory identity before it can contain bytes.
//! Cleanup never scans a host directory for filename prefixes.
#![allow(unsafe_code, reason = "Descriptor-relative private directory creation and reclamation.")]
use super::{
    host::PrivateObjects, privacy, resource::Placement, snapshot::SnapshotArea, staging::Area,
};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsString},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStringExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

const MAX_CONTAINERS: usize = 64;
const MAX_MANIFEST: u64 = 512 * 1024;
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    parent: Vec<u8>,
    parent_device: u64,
    parent_inode: u64,
    device: u64,
    inode: u64,
}
#[derive(Debug)]
pub struct Journal {
    root: Arc<File>,
    records: Mutex<BTreeMap<String, Record>>,
}
pub struct Managed {
    profile: PathBuf,
    private: Arc<PrivateObjects>,
    ready: Mutex<Option<Arc<Placement>>>,
    pub(super) preparation_started: AtomicBool,
}
impl Managed {
    /// No I/O: constructing launch configuration remains on the engine owner.
    pub fn new(profile: PathBuf, private: Arc<PrivateObjects>) -> Self {
        Self {
            profile,
            private,
            ready: Mutex::new(None),
            preparation_started: AtomicBool::new(false),
        }
    }
    /// Runs only on an existing File worker. Failure leaves ordinary reads
    /// available and writer-open retries the same checked preparation path.
    pub(super) fn prepare_write(&self) -> io::Result<()> {
        let placement = self.get()?;
        placement
            .staging
            .as_ref()
            .ok_or_else(|| io::Error::other("staging unavailable"))?
            .prepare_unlinked_container()
    }
    pub fn get(&self) -> io::Result<Arc<Placement>> {
        let mut ready =
            self.ready.lock().map_err(|_| io::Error::other("private placement lock poisoned"))?;
        if let Some(placement) = &*ready {
            return Ok(placement.clone());
        }
        let profile = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(self.profile.canonicalize()?)?;
        let created = mkdir(&profile, c"file-io")?;
        let root = Arc::new(open(&profile, c"file-io", true)?);
        if created {
            privacy::restrict_new(&root)?;
            root.sync_all()?;
            profile.sync_all()?;
        }
        privacy::directory(&root)?;
        // A second engine must never reclaim live snapshots or staged writes.
        // This retained descriptor owns the lock through all dependent handles.
        // SAFETY: advisory nonblocking exclusive lock on the retained private root.
        if unsafe { libc::flock(root.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let records = read_manifest(&root)?;
        match open(&root, c"manifest.next", false) {
            Ok(file) => {
                privacy::control_file(&file)?;
                // SAFETY: fixed protocol control name in locked private root;
                // never a data container or a match against host file prefixes.
                if unsafe { libc::unlinkat(root.as_raw_fd(), c"manifest.next".as_ptr(), 0) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                root.sync_all()?;
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            Err(error) => return Err(error),
        }
        let journal = Arc::new(Journal { root: root.clone(), records: Mutex::new(records) });
        journal.reclaim()?;
        self.private.protect(root.clone())?;
        self.private
            .journal
            .set(journal)
            .map_err(|_| io::Error::other("private placement already initialized"))?;
        let placement = Arc::new(Placement {
            snapshots: Some(SnapshotArea::new(root.clone(), self.private.clone())?),
            staging: Some(Area::new(root, self.private.clone())?),
        });
        *ready = Some(placement.clone());
        Ok(placement)
    }
}
fn valid_name(name: &str) -> bool {
    name.strip_prefix(".playbit-file-").is_some_and(|id| {
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}
fn open(parent: &File, name: &CStr, directory: bool) -> io::Result<File> {
    // SAFETY: retained parent, terminated component; transferred descriptor.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_NONBLOCK
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
}
fn mkdir(parent: &File, name: &CStr) -> io::Result<bool> {
    // SAFETY: retained parent and terminated component, restrictive initial mode.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EEXIST) { Ok(false) } else { Err(error) }
}
fn flush(file: &File) -> io::Result<()> {
    file.sync_all()?;
    #[cfg(target_vendor = "apple")]
    // SAFETY: retained file descriptor, no pointer argument.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn path(file: &File) -> io::Result<Vec<u8>> {
    #[cfg(target_vendor = "apple")]
    {
        let mut bytes = [0u8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes to the exclusive output.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| io::Error::other("unterminated directory path"))?;
        Ok(bytes[..length].to_vec())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?
            .as_os_str()
            .as_bytes()
            .to_vec())
    }
}
impl Journal {
    fn persist(&self, records: &BTreeMap<String, Record>) -> io::Result<()> {
        let bytes = serde_json::to_vec(records).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let name = c"manifest.next";
        // SAFETY: retained runtime-private parent and fresh terminated component.
        let fd = unsafe {
            libc::openat(
                self.root.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat transferred this new descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            privacy::restrict_new(&file)?;
            file.write_all(&bytes)?;
            flush(&file)?;
            // SAFETY: both names belong to retained private root; atomic replacement.
            if unsafe {
                libc::renameat(
                    self.root.as_raw_fd(),
                    name.as_ptr(),
                    self.root.as_raw_fd(),
                    c"manifest".as_ptr(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            self.root.sync_all()?;
            flush(&file)
        })();
        // SAFETY: remove only this exclusively created manifest temporary name.
        unsafe {
            libc::unlinkat(self.root.as_raw_fd(), name.as_ptr(), 0);
        }
        result
    }
    fn reclaim(&self) -> io::Result<()> {
        let mut records =
            self.records.lock().map_err(|_| io::Error::other("private journal lock poisoned"))?;
        // An absent or empty validated manifest owns no containers. There is
        // no recovery transition to persist; registration still flushes its
        // ownership record before the first container accepts private bytes.
        if records.is_empty() {
            return Ok(());
        }
        for (name, record) in records.iter() {
            let parent = self.recovery_parent(record)?;
            let name = CString::new(name.as_bytes())?;
            match open(&parent, &name, true) {
                Ok(directory) => {
                    let status = directory.metadata()?;
                    if (status.dev(), status.ino()) != (record.device, record.inode) {
                        return Err(io::Error::other("private storage container changed identity"));
                    }
                    privacy::directory(&directory)?;
                    // The durably registered private container has exactly one
                    // runtime-owned contents name. Never follow a replacement.
                    match open(&directory, c"data", false) {
                        Ok(file) => {
                            let status = file.metadata()?;
                            if !status.is_file() || status.nlink() != 1 {
                                return Err(io::Error::other(
                                    "private contents have unexpected type or aliases",
                                ));
                            }
                            // SAFETY: verified owned container and regular private data.
                            if unsafe { libc::unlinkat(directory.as_raw_fd(), c"data".as_ptr(), 0) }
                                < 0
                            {
                                return Err(io::Error::last_os_error());
                            }
                            directory.sync_all()?;
                        }
                        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                        Err(error) => return Err(error),
                    }
                    remove_directory(&parent, &name, &directory)?;
                }
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                Err(error) => return Err(error),
            }
            parent.sync_all()?;
        }
        self.persist(&BTreeMap::new())?;
        records.clear();
        Ok(())
    }
    fn recovery_parent(&self, record: &Record) -> io::Result<Arc<File>> {
        let root = self.root.metadata()?;
        let expected = (record.parent_device, record.parent_inode);
        // iOS permits access to the app container, but can deny opening its
        // ancestors. Reuse the validated private root when it owns this record.
        let parent = if (root.dev(), root.ino()) == expected {
            self.root.clone()
        } else {
            let path = PathBuf::from(OsString::from_vec(record.parent.clone()));
            super::host::Directory::root(
                &path,
                Arc::new(PrivateObjects::default()),
                Arc::new(super::authority::Authority::new(false)),
            )?
            .file
        };
        let status = parent.metadata()?;
        if (status.dev(), status.ino()) != expected {
            return Err(io::Error::other("private storage parent changed identity"));
        }
        Ok(parent)
    }
    pub(super) fn container(
        self: &Arc<Self>,
        parent: Arc<File>,
        private: Arc<PrivateObjects>,
    ) -> io::Result<Container> {
        let mut records =
            self.records.lock().map_err(|_| io::Error::other("private journal lock poisoned"))?;
        if records.len() == MAX_CONTAINERS {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let parent_path = path(&parent)?;
        if parent_path.len() > 4096 {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        let parent_status = parent.metadata()?;
        let name = CString::new(format!(
            ".playbit-file-{}",
            crate::store::Id::generate().map_err(io::Error::other)?
        ))?;
        let reservation = private.reserve_name(parent.clone(), name.as_bytes())?;
        if !mkdir(&parent, &name)? {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        let directory = Arc::new(open(&parent, &name, true)?);
        privacy::restrict_new(&directory)?;
        private.protect(directory.clone())?;
        let status = directory.metadata()?;
        let mut container = Container {
            directory,
            parent,
            name,
            private,
            journal: self.clone(),
            _reservation: reservation,
            registered: false,
        };
        directory_sync(&container.directory, &container.parent)?;
        let key = container.name.to_str().map_err(io::Error::other)?.to_owned();
        records.insert(
            key.clone(),
            Record {
                parent: parent_path,
                parent_device: parent_status.dev(),
                parent_inode: parent_status.ino(),
                device: status.dev(),
                inode: status.ino(),
            },
        );
        let result = self.persist(&records);
        if result.is_err() {
            records.remove(&key);
        }
        drop(records);
        result?;
        container.registered = true;
        Ok(container)
    }
}
fn directory_sync(directory: &File, parent: &File) -> io::Result<()> {
    directory.sync_all()?;
    parent.sync_all()
}
fn remove_directory(parent: &File, name: &CStr, expected: &File) -> io::Result<()> {
    let current = match open(parent, name, true) {
        Ok(current) => current,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
        Err(error) => return Err(error),
    };
    let current = current.metadata()?;
    let expected = expected.metadata()?;
    if (current.dev(), current.ino()) != (expected.dev(), expected.ino()) {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    // SAFETY: exact retained empty container, never recursive removal.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
        return Err(io::Error::last_os_error());
    }
    parent.sync_all()
}
pub(super) struct Container {
    pub directory: Arc<File>,
    parent: Arc<File>,
    name: CString,
    private: Arc<PrivateObjects>,
    journal: Arc<Journal>,
    _reservation: super::host::NameReservation,
    registered: bool,
}
impl Drop for Container {
    fn drop(&mut self) {
        let result = (|| {
            remove_directory(&self.parent, &self.name, &self.directory)?;
            self.private.release_unlinked(&self.directory)?;
            if self.registered {
                let mut records = self
                    .journal
                    .records
                    .lock()
                    .map_err(|_| io::Error::other("private journal lock poisoned"))?;
                let key = self.name.to_str().map_err(io::Error::other)?;
                let mut remaining = records.clone();
                remaining.remove(key);
                self.journal.persist(&remaining)?;
                *records = remaining;
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = result {
            eprintln!("Private file container cleanup retained its ownership record: {error}");
        }
    }
}

fn read_manifest(root: &File) -> io::Result<BTreeMap<String, Record>> {
    let records = match open(root, c"manifest", false) {
        Ok(file) => {
            privacy::control_file(&file)?;
            if file.metadata()?.len() > MAX_MANIFEST {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            let mut bytes = Vec::new();
            file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_MANIFEST {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            serde_json::from_slice::<BTreeMap<String, Record>>(&bytes).map_err(io::Error::other)?
        }
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => BTreeMap::new(),
        Err(error) => return Err(error),
    };
    if records.len() > MAX_CONTAINERS
        || records.iter().any(|(name, record)| {
            !valid_name(name)
                || record.parent.len() > 4096
                || record.parent.first() != Some(&b'/')
                || record.parent.contains(&0)
                || record.device == 0
                || record.inode == 0
        })
    {
        return Err(io::Error::other("invalid private storage manifest"));
    }
    Ok(records)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests exercise durable ownership records in isolated process fixtures."
)]
mod tests {
    use super::super::{
        authority::Authority,
        host::Directory,
        publication::{HostWriter, Registry},
    };
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};
    const CHILD_PROFILE: &str = "PLAYBIT_FILE_CRASH_TEST_PROFILE";
    const CHILD: &str = "engine::files::private_storage::tests::crash_writer_child";

    #[test]
    fn crash_writer_child() {
        let Some(profile) = std::env::var_os(CHILD_PROFILE) else { return };
        let profile = PathBuf::from(profile);
        let private = Arc::new(PrivateObjects::default());
        let managed = Managed::new(profile.clone(), private.clone());
        let placement = managed.get().unwrap();
        if std::env::var("PLAYBIT_FILE_CRASH_TEST_UNLINKED").as_deref() == Ok("1") {
            let mut writer = placement
                .staging
                .as_ref()
                .unwrap()
                .create_unlinked(Arc::new(Authority::new(true)))
                .unwrap();
            writer.write(0, b"unpublished private contents").unwrap();
            std::process::exit(0);
        }
        let root = Directory::root(
            &profile.join("mount").canonicalize().unwrap(),
            private,
            Arc::new(Authority::new(true)),
        )
        .unwrap();
        let mut writer = HostWriter::open(
            &root,
            b"new",
            false,
            placement.staging.as_ref(),
            &Registry::default(),
        )
        .unwrap();
        writer.write(0, b"unpublished private contents").unwrap();
        // Process exit skips Writer, Container, and Journal destruction. Restart
        // must use the durable record, without a prior handle or an in-memory ID.
        std::process::exit(0);
    }
    fn crash(profile: &Path, unlinked: bool) {
        fs::create_dir(profile.join("mount")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--nocapture"])
            .env(CHILD_PROFILE, profile)
            .env("PLAYBIT_FILE_CRASH_TEST_UNLINKED", if unlinked { "1" } else { "0" })
            .status()
            .unwrap();
        assert!(status.success());
    }
    fn records(profile: &Path) -> BTreeMap<String, Record> {
        serde_json::from_slice(&fs::read(profile.join("file-io/manifest")).unwrap()).unwrap()
    }
    #[test]
    fn process_exit_reclaims_unlinked_volume_staging_with_one_container_record() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        crash(&profile, true);
        let entries = records(&profile);
        assert_eq!(entries.len(), 1);
        let name = entries.keys().next().unwrap();
        let container = profile.join("file-io").join(name);
        assert_eq!(fs::read_dir(&container).unwrap().count(), 0);
        // Model death after exclusive creation but before unlink: no private
        // bytes have been accepted, and the retained container owns this name.
        File::options()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(container.join("data"))
            .unwrap();
        let managed = Managed::new(profile.clone(), Arc::new(PrivateObjects::default()));
        managed.get().unwrap();
        assert!(records(&profile).is_empty());
        assert_eq!(fs::read_dir(profile.join("file-io")).unwrap().count(), 1);
    }
    #[test]
    fn restart_reclaims_registered_contents_and_never_prefix_matches_host_files() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        crash(&profile, false);
        assert_eq!(records(&profile).len(), 1);
        let unrelated = profile.join("mount/.playbit-file-unrelated");
        fs::write(&unrelated, b"keep").unwrap();
        let private = Arc::new(PrivateObjects::default());
        let managed = Managed::new(profile.clone(), private.clone());
        managed.get().unwrap();
        assert!(records(&profile).is_empty());
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
        assert!(!profile.join("mount/new").exists());
        assert!(!fs::read_dir(profile.join("file-io")).unwrap().any(|entry| {
            entry.unwrap().file_name().to_string_lossy().starts_with(".playbit-file-")
        }));
        let broad = Directory::root(&profile, private, Arc::new(Authority::new(true))).unwrap();
        assert!(broad.directory(b"file-io").is_err());
        assert!(!broad.list().unwrap().iter().any(|entry| entry.name == b"file-io"));
        let second = Managed::new(profile, Arc::new(PrivateObjects::default()));
        assert!(second.get().is_err(), "another owner cannot reclaim live storage");
    }
    #[test]
    fn restart_rejects_replaced_container_and_retains_unrelated_contents() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        crash(&profile, false);
        let entries = records(&profile);
        let (name, record) = entries.iter().next().unwrap();
        let parent = PathBuf::from(OsString::from_vec(record.parent.clone()));
        let original = parent.join(name);
        let saved = parent.join("original-private");
        fs::rename(&original, &saved).unwrap();
        fs::create_dir(&original).unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(original.join("data"), b"unrelated").unwrap();
        let managed = Managed::new(profile.clone(), Arc::new(PrivateObjects::default()));
        assert!(managed.get().is_err());
        assert_eq!(fs::read(original.join("data")).unwrap(), b"unrelated");
        assert_eq!(fs::read(saved.join("data")).unwrap(), b"unpublished private contents");
        assert_eq!(records(&profile).len(), 1);
        // A failed admission released its filesystem lock. Restoring the exact
        // directory identity makes a later request usable without restarting.
        fs::rename(&original, parent.join("unrelated-preserved")).unwrap();
        fs::rename(&saved, &original).unwrap();
        managed.get().unwrap();
        assert!(records(&profile).is_empty());
        assert_eq!(fs::read(parent.join("unrelated-preserved/data")).unwrap(), b"unrelated");
    }
    #[test]
    fn preparation_registers_one_empty_container_and_writer_reuses_it() {
        // Preparing twice and opening the first writer must share the same
        // durable registration; preparation never creates plaintext contents.
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        let managed = Managed::new(profile.clone(), Arc::new(PrivateObjects::default()));
        managed.prepare_write().unwrap();
        let entries = records(&profile);
        assert_eq!(entries.len(), 1);
        let name = entries.keys().next().unwrap();
        assert_eq!(fs::read_dir(profile.join("file-io").join(name)).unwrap().count(), 0);
        let manifest = profile.join("file-io/manifest");
        let before = fs::metadata(&manifest).unwrap();
        managed.prepare_write().unwrap();
        let placement = managed.get().unwrap();
        let writer = placement
            .staging
            .as_ref()
            .unwrap()
            .create_unlinked(Arc::new(Authority::new(true)))
            .unwrap();
        assert_eq!(records(&profile).len(), 1);
        assert_eq!(before.ino(), fs::metadata(&manifest).unwrap().ino());
        drop(writer);
        drop(placement);
        drop(managed);
        assert!(records(&profile).is_empty());
    }
    #[test]
    fn empty_recovery_preserves_manifest_and_registration_remains_durable() {
        // Empty recovery does not replace its validated control file. The first
        // writer still records ownership, and clean retirement removes it.
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        let managed = Managed::new(profile.clone(), Arc::new(PrivateObjects::default()));
        let placement = managed.get().unwrap();
        let mut writer = placement
            .staging
            .as_ref()
            .unwrap()
            .create_unlinked(Arc::new(Authority::new(true)))
            .unwrap();
        writer.write(0, b"private contents").unwrap();
        assert_eq!(records(&profile).len(), 1);
        drop(writer);
        drop(placement);
        drop(managed);
        assert!(records(&profile).is_empty());
        let manifest = profile.join("file-io/manifest");
        let before = fs::metadata(&manifest).unwrap();
        let managed = Managed::new(profile.clone(), Arc::new(PrivateObjects::default()));
        managed.get().unwrap();
        let after = fs::metadata(&manifest).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(records(&profile).is_empty());
    }
    #[test]
    fn interrupted_manifest_is_bounded_and_aliases_are_not_deleted() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        let private = Arc::new(PrivateObjects::default());
        let managed = Managed::new(profile.clone(), private.clone());
        managed.get().unwrap();
        drop(managed);
        drop(private);
        let pending = profile.join("file-io/manifest.next");
        let mut file =
            File::options().create_new(true).write(true).mode(0o600).open(&pending).unwrap();
        privacy::restrict_new(&file).unwrap();
        file.write_all(b"incomplete control record").unwrap();
        let alias = profile.join("alias");
        fs::hard_link(&pending, &alias).unwrap();
        let managed = Managed::new(profile, Arc::new(PrivateObjects::default()));
        assert!(managed.get().is_err());
        assert_eq!(fs::read(&alias).unwrap(), b"incomplete control record");
        fs::remove_file(alias).unwrap();
        managed.get().unwrap();
        assert!(!pending.exists());
    }
    #[test]
    fn restart_cannot_release_private_data_with_an_external_alias() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().canonicalize().unwrap();
        crash(&profile, false);
        let entries = records(&profile);
        let (name, record) = entries.iter().next().unwrap();
        let parent = PathBuf::from(OsString::from_vec(record.parent.clone()));
        let data = parent.join(name).join("data");
        fs::hard_link(&data, profile.join("mount/alias")).unwrap();
        let managed = Managed::new(profile.clone(), Arc::new(PrivateObjects::default()));
        assert!(managed.get().is_err());
        assert!(data.exists());
        assert_eq!(records(&profile).len(), 1);
    }
}
