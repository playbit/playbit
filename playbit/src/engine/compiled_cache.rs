//! Disposable compiler output in a trusted, guest-inaccessible local directory.
//! Checksums detect corruption, not hostile writes by the local OS account. GUI
//! processes receive descriptors over authenticated IPC and hash owned bytes.
use super::files::{host::PrivateObjects, privacy};
use crate::runtime::compiled::{self, Descriptor, Image, MAX_IMAGE_BYTES};
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, Mutex, RwLock},
};

const DIRECTORY: &str = "cache/wasm-vm";
const MAX_ENTRIES: usize = 128;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const HEADER_BYTES: usize = 144;
const RECORD_BYTES: usize = HEADER_BYTES + 32;
const MAGIC: &[u8; 8] = b"PBCODE01";
const _: () = assert!(MAX_IMAGE_BYTES as u64 <= MAX_BYTES);

struct Entry {
    descriptor: Descriptor,
    used: u64,
}
#[derive(Default)]
struct Index {
    entries: BTreeMap<[u8; 32], Entry>,
    clock: u64,
}
pub(crate) struct Cache {
    directory: File,
    index: RwLock<Index>,
    publisher: Mutex<()>,
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}
fn open(directory: &File, name: &str) -> io::Result<File> {
    Ok(rustix::fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into())
}
// Resolve profile aliases, then reject symlinks in both cache directory components.
fn directory(profile: &Path, create: bool) -> io::Result<File> {
    let mut parent = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(profile.canonicalize()?)?;
    for name in DIRECTORY.split('/') {
        let created = if create {
            match rustix::fs::mkdirat(&parent, name, Mode::RWXU) {
                Ok(()) => true,
                Err(rustix::io::Errno::EXIST) => false,
                Err(error) => return Err(error.into()),
            }
        } else {
            false
        };
        let child = open(&parent, name)?;
        if created {
            privacy::restrict_new(&child)?;
            parent.sync_all()?;
        }
        privacy::directory(&child)?;
        parent = child;
    }
    Ok(parent)
}
fn header(descriptor: &Descriptor) -> [u8; HEADER_BYTES] {
    let mut bytes = [0; HEADER_BYTES];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..40].copy_from_slice(&descriptor.wasm);
    bytes[40..72].copy_from_slice(&descriptor.compatibility);
    bytes[72..104].copy_from_slice(&descriptor.image);
    bytes[104..112].copy_from_slice(&descriptor.bytes.to_le_bytes());
    bytes
}
impl Cache {
    /// Called on engine startup, before guests can obtain filesystem grants.
    /// Indexing reads at most 128 small checksummed headers, never code bodies.
    pub(crate) fn open(profile: &Path) -> io::Result<Self> {
        let cache = Self {
            directory: directory(profile, true)?,
            index: RwLock::new(Index::default()),
            publisher: Mutex::new(()),
        };
        let mut total = 0;
        for (count, entry) in fs::read_dir(profile.join(DIRECTORY))?.enumerate() {
            if count > MAX_ENTRIES {
                return Err(invalid("compiled cache directory exceeds capacity"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(invalid("invalid compiled cache filename"));
            };
            if name == "next" {
                let file = open(&cache.directory, name)?;
                privacy::control_file(&file)?;
                rustix::fs::unlinkat(&cache.directory, name, AtFlags::empty())?;
                continue;
            }
            let Ok(key) = crate::identifier::decode::<32>(name) else {
                return Err(invalid("invalid compiled cache filename"));
            };
            match cache.read_descriptor(&key) {
                Ok(descriptor) => {
                    total += descriptor.bytes + RECORD_BYTES as u64;
                    if total > MAX_BYTES {
                        return Err(invalid("compiled cache exceeds byte capacity"));
                    }
                    let mut index = cache.index.write().expect("compiled cache index lock");
                    if index.entries.len() == MAX_ENTRIES {
                        return Err(invalid("compiled cache entry capacity exceeded"));
                    }
                    index.entries.insert(key, Entry { descriptor, used: 0 });
                }
                Err(error) => {
                    eprintln!("Discarding invalid compiled cache entry: {error}");
                    rustix::fs::unlinkat(&cache.directory, name, AtFlags::empty())?;
                }
            }
        }
        Ok(cache)
    }
    fn read_descriptor(&self, key: &[u8; 32]) -> io::Result<Descriptor> {
        let mut file = open(&self.directory, &crate::identifier::encode(key))?;
        privacy::control_file(&file)?;
        let mut bytes = [0; RECORD_BYTES];
        file.read_exact(&mut bytes)?;
        let (body, checksum) = bytes.split_at(HEADER_BYTES);
        if &body[..8] != MAGIC || blake3::hash(body).as_bytes() != checksum {
            return Err(invalid("compiled cache header checksum mismatch"));
        }
        let descriptor = Descriptor {
            wasm: body[8..40].try_into().expect("header width"),
            compatibility: body[40..72].try_into().expect("header width"),
            image: body[72..104].try_into().expect("header width"),
            bytes: u64::from_le_bytes(body[104..112].try_into().expect("header width")),
        };
        if descriptor.bytes > MAX_IMAGE_BYTES as u64
            || descriptor.key() != *key
            || file.metadata()?.len() != RECORD_BYTES as u64 + descriptor.bytes
            || body[112..].iter().any(|byte| *byte != 0)
        {
            return Err(invalid("invalid compiled cache header"));
        }
        Ok(descriptor)
    }
    fn publish_inner(&self, image: &Image) -> io::Result<()> {
        let _writer =
            self.publisher.lock().map_err(|_| invalid("compiled cache publisher lock"))?;
        let key = image.descriptor.key();
        // Keep accounting until unlink succeeds. Readers can race deletion and
        // safely compile their verified Wasm on a file miss.
        loop {
            let evicted = {
                let index = self.index.read().map_err(|_| invalid("compiled cache index lock"))?;
                let total: u64 = index
                    .entries
                    .values()
                    .map(|entry| entry.descriptor.bytes + RECORD_BYTES as u64)
                    .sum();
                // Include the temporary publication in the disk bound. A
                // replacement still occupies its old file until atomic rename.
                if (index.entries.len() < MAX_ENTRIES || index.entries.contains_key(&key))
                    && total + image.descriptor.bytes + RECORD_BYTES as u64 <= MAX_BYTES
                {
                    None
                } else {
                    let oldest = index
                        .entries
                        .iter()
                        .min_by_key(|(_, entry)| entry.used)
                        .map(|(key, _)| *key)
                        .ok_or_else(|| invalid("compiled cache capacity accounting"))?;
                    Some(oldest)
                }
            };
            let Some(evicted) = evicted else {
                break;
            };
            match rustix::fs::unlinkat(
                &self.directory,
                crate::identifier::encode(&evicted),
                AtFlags::empty(),
            ) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(error.into()),
            }
            self.index
                .write()
                .map_err(|_| invalid("compiled cache index lock"))?
                .entries
                .remove(&evicted);
        }
        let fd = rustix::fs::openat(
            &self.directory,
            "next",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = File::from(fd);
        privacy::restrict_new(&file)?;
        let body = header(&image.descriptor);
        let result = (|| {
            file.write_all(&body)?;
            file.write_all(blake3::hash(&body).as_bytes())?;
            file.write_all(&image.bytes)?;
            // Disposable code can be regenerated after a crash. Atomic rename
            // and checksums suffice; no durable app data depends on it.
            rustix::fs::renameat(
                &self.directory,
                "next",
                &self.directory,
                crate::identifier::encode(&key),
            )?;
            Ok::<_, io::Error>(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.directory, "next", AtFlags::empty());
        }
        result?;
        let mut index = self.index.write().map_err(|_| invalid("compiled cache index lock"))?;
        index.clock = index.clock.checked_add(1).expect("cache clock exhausted");
        let used = index.clock;
        index.entries.insert(key, Entry { descriptor: image.descriptor.clone(), used });
        Ok(())
    }
}
impl compiled::Cache for Cache {
    fn load(&self, wasm: [u8; 32], compatibility: [u8; 32]) -> Option<Image> {
        let descriptor = self.descriptor(wasm, compatibility)?;
        match read_image(&self.directory, descriptor) {
            Ok(image) => Some(image),
            Err(error) => {
                eprintln!("Compiled cache miss after read failure: {error}");
                None
            }
        }
    }
    fn publish(&self, image: &Image) {
        if image.bytes.len() > MAX_IMAGE_BYTES {
            return;
        }
        if let Err(error) = self.publish_inner(image) {
            eprintln!("Compiled cache publication failed: {error}");
        }
    }
    fn descriptor(&self, wasm: [u8; 32], compatibility: [u8; 32]) -> Option<Descriptor> {
        let mut index = self.index.write().ok()?;
        index.clock = index.clock.checked_add(1).expect("cache clock exhausted");
        let used = index.clock;
        let entry = index.entries.get_mut(&compiled::key(wasm, compatibility))?;
        entry.used = used;
        Some(entry.descriptor.clone())
    }
}
fn read_image(directory: &File, descriptor: Descriptor) -> io::Result<Image> {
    if descriptor.bytes > MAX_IMAGE_BYTES as u64 {
        return Err(invalid("compiled image exceeds capacity"));
    }
    let mut file = open(directory, &crate::identifier::encode(&descriptor.key()))?;
    privacy::control_file(&file)?;
    if file.metadata()?.len() != RECORD_BYTES as u64 + descriptor.bytes {
        return Err(invalid("compiled image length mismatch"));
    }
    let mut header = [0; RECORD_BYTES];
    file.read_exact(&mut header)?;
    let mut bytes = vec![0; descriptor.bytes as usize];
    file.read_exact(&mut bytes)?;
    Image::verified(descriptor, bytes).map_err(io::Error::other)
}
#[cfg(any(test, all(not(target_os = "ios"), feature = "process")))]
pub(crate) fn read(profile: &Path, descriptor: Descriptor) -> io::Result<Image> {
    let _timing = crate::trace::Span::new("guest.compiled.read");
    read_image(&directory(profile, false)?, descriptor)
}

/// Every host-file owner protects the cache before executing any guest.
pub(crate) fn protect(profile: &Path, private: &Arc<PrivateObjects>) -> io::Result<()> {
    // Establish the private namespace even when caching is unavailable.
    private.protect(Arc::new(directory(profile, true)?))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Tests assert private cache operations in disposable profiles."
)]
mod tests {
    use super::*;
    use crate::runtime::compiled::Cache as _;
    fn image(seed: u8) -> Image {
        let bytes = vec![seed; 64];
        Image {
            descriptor: Descriptor {
                wasm: [seed; 32],
                compatibility: [7; 32],
                image: *blake3::hash(&bytes).as_bytes(),
                bytes: bytes.len() as u64,
            },
            bytes,
        }
    }
    #[test]
    fn restart_corruption_and_atomic_repair() {
        // Reopen cached code, reject header/body corruption and truncation, and repair publication.
        let profile = tempfile::tempdir().unwrap();
        let image = image(1);
        let cache = Cache::open(profile.path()).unwrap();
        cache.publish(&image);
        let descriptor = cache.descriptor([1; 32], [7; 32]).unwrap();
        assert!(cache.descriptor([1; 32], [8; 32]).is_none());
        assert!(cache.descriptor([2; 32], [7; 32]).is_none());
        drop(cache);
        let cache = Cache::open(profile.path()).unwrap();
        assert_eq!(cache.load([1; 32], [7; 32]).unwrap().bytes, image.bytes);
        let path =
            profile.path().join(DIRECTORY).join(crate::identifier::encode(&descriptor.key()));
        let mut corrupted = fs::read(&path).unwrap();
        *corrupted.last_mut().unwrap() ^= 1;
        fs::write(&path, &corrupted).unwrap();
        assert!(read(profile.path(), descriptor.clone()).is_err());
        assert!(cache.load([1; 32], [7; 32]).is_none());
        cache.publish(&image);
        assert!(read(profile.path(), descriptor.clone()).is_ok());
        File::options().write(true).open(&path).unwrap().set_len(RECORD_BYTES as u64 + 1).unwrap();
        assert!(read(profile.path(), descriptor).is_err());
        drop(cache);
        let cache = Cache::open(profile.path()).unwrap();
        assert!(cache.load([1; 32], [7; 32]).is_none());
        cache.publish(&image);
        let mut corrupted_header = fs::read(&path).unwrap();
        corrupted_header[72] ^= 1;
        fs::write(&path, corrupted_header).unwrap();
        drop(cache);
        let cache = Cache::open(profile.path()).unwrap();
        assert!(cache.descriptor([1; 32], [7; 32]).is_none());
        assert!(!path.exists());
        let file = rustix::fs::openat(
            &cache.directory,
            "next",
            OFlags::CREATE | OFlags::WRONLY | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        File::from(file).write_all(b"interrupted").unwrap();
        drop(cache);
        Cache::open(profile.path()).unwrap();
        assert!(!profile.path().join(DIRECTORY).join("next").exists());
    }
    #[test]
    fn entry_eviction_and_concurrent_publication_remain_bounded() {
        // Publish beyond capacity from four workers, then verify the persisted bound.
        let profile = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(profile.path()).unwrap());
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let cache = cache.clone();
                scope.spawn(move || {
                    for index in 0..40 {
                        cache.publish(&image(worker * 40 + index));
                    }
                });
            }
        });
        assert_eq!(cache.index.read().unwrap().entries.len(), MAX_ENTRIES);
        assert_eq!(fs::read_dir(profile.path().join(DIRECTORY)).unwrap().count(), MAX_ENTRIES);
        drop(cache);
        let cache = Cache::open(profile.path()).unwrap();
        assert_eq!(cache.index.read().unwrap().entries.len(), MAX_ENTRIES);
    }
    #[test]
    fn image_and_disk_byte_limits_include_atomic_publication() {
        // Exercise the real byte capacities, including publication headers and
        // replacement space. Oversized compiler output is not saved.
        let profile = tempfile::tempdir().unwrap();
        let cache = Cache::open(profile.path()).unwrap();
        let mut image = image(1);
        image.bytes.resize(MAX_IMAGE_BYTES, 1);
        image.descriptor.bytes = image.bytes.len() as u64;
        image.descriptor.image = *blake3::hash(&image.bytes).as_bytes();
        for seed in 1..=4 {
            image.descriptor.wasm = [seed; 32];
            cache.publish(&image);
        }
        assert_eq!(cache.index.read().unwrap().entries.len(), 3);
        assert!(cache.descriptor([1; 32], [7; 32]).is_none());
        cache.publish(&image);
        assert!(cache.load([4; 32], [7; 32]).is_some());
        let bytes: u64 = fs::read_dir(profile.path().join(DIRECTORY))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(bytes <= MAX_BYTES);
        image.bytes.push(1);
        image.descriptor.wasm = [5; 32];
        image.descriptor.bytes += 1;
        image.descriptor.image = *blake3::hash(&image.bytes).as_bytes();
        cache.publish(&image);
        assert!(cache.descriptor([5; 32], [7; 32]).is_none());
        assert!(!profile.path().join(DIRECTORY).join("next").exists());
    }
    #[test]
    fn failed_eviction_retains_accounting_and_missing_files_are_reclaimed() {
        // A failed deletion must not make room for additional files on disk.
        let profile = tempfile::tempdir().unwrap();
        let cache = Cache::open(profile.path()).unwrap();
        for seed in 0..MAX_ENTRIES as u8 {
            cache.publish(&image(seed));
        }
        let oldest = profile
            .path()
            .join(DIRECTORY)
            .join(crate::identifier::encode(&image(0).descriptor.key()));
        fs::remove_file(&oldest).unwrap();
        fs::create_dir(&oldest).unwrap();
        assert!(cache.publish_inner(&image(128)).is_err());
        assert_eq!(cache.index.read().unwrap().entries.len(), MAX_ENTRIES);
        assert!(!profile.path().join(DIRECTORY).join("next").exists());
        fs::remove_dir(&oldest).unwrap();
        cache.publish_inner(&image(128)).unwrap();
        assert_eq!(cache.index.read().unwrap().entries.len(), MAX_ENTRIES);
        assert!(cache.load([128; 32], [7; 32]).is_some());
    }
    #[test]
    fn cache_is_private_through_ancestor_and_direct_grants() {
        // Protect before initialization; neither parent nor direct grants expose compiled code.
        use super::super::files::{authority::Authority, host::Directory};
        let profile = tempfile::tempdir().unwrap();
        let private = Arc::new(PrivateObjects::default());
        protect(profile.path(), &private).unwrap();
        let cache = Cache::open(profile.path()).unwrap();
        let image = image(1);
        cache.publish(&image);
        let name = crate::identifier::encode(&image.descriptor.key());
        let canonical = profile.path().canonicalize().unwrap();
        for parent in [&canonical, &canonical.join("cache")] {
            let root =
                Directory::root(parent, private.clone(), Arc::new(Authority::new(true))).unwrap();
            let relative = canonical.join(DIRECTORY).join(&name);
            let relative = relative.strip_prefix(parent).unwrap().to_str().unwrap().as_bytes();
            assert!(root.reader(relative).is_err());
        }
        assert!(
            Directory::root(&canonical.join(DIRECTORY), private, Arc::new(Authority::new(true)))
                .is_err()
        );
    }
    #[test]
    fn cache_rejects_symlinked_directory_components() {
        // A trusted cache must not follow a redirected parent or leaf directory.
        use std::os::unix::fs::symlink;
        for component in ["cache", DIRECTORY] {
            let profile = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            let path = profile.path().join(component);
            if component == DIRECTORY {
                crate::paths::private_directory(profile.path(), "cache").unwrap();
            }
            symlink(target.path(), &path).unwrap();
            assert!(Cache::open(profile.path()).is_err());
            assert!(protect(profile.path(), &Arc::new(PrivateObjects::default())).is_err());
            assert!(read(profile.path(), image(1).descriptor).is_err());
            assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
        }
    }
}
