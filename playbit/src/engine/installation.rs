//! macOS installation ownership (tier B). Clients hold shared leases; the
//! updater takes exclusive ownership only after the last client has finished.
use super::Result;
use core_foundation::{
    base::{CFType, TCFType},
    data::CFData,
    dictionary::CFDictionary,
    propertylist,
    string::CFString,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAX_PROFILES: usize = 64;
const MAX_REGISTRY: u64 = 320 * 1024;
const RUN_INTERRUPT: &str = "run-interrupt.lock";
pub const MAX_PLIST: u64 = 64 * 1024;
pub const LOCATION_ERROR: &str =
    "Automatic updates require a writable, user-owned app in /Applications or ~/Applications.";

#[derive(Clone, Debug)]
pub struct Installation {
    pub app: PathBuf,
    pub state: PathBuf,
}

pub struct Lease {
    _file: File,
}

pub struct RunInterrupt {
    file: File,
}

impl std::os::fd::AsFd for RunInterrupt {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.file)
    }
}

impl RunInterrupt {
    pub fn requested(&self) -> Result<bool> {
        // An installer owns the exclusive lock only while it is trying to cut
        // over. A delayed file notification or installer crash is harmless.
        match self.file.try_lock_shared() {
            Ok(()) => {
                self.file.unlock()?;
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

#[derive(Clone, Copy)]
enum LockMode {
    Shared,
    Exclusive,
    TryExclusive,
}

pub const MAX_RELEASE_SEQUENCE: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIdentity {
    pub app_version: String,
    pub sequence: u64,
}

impl ReleaseIdentity {
    pub fn running() -> Self {
        Self { app_version: crate::RELEASE_VERSION.into(), sequence: crate::RELEASE_SEQUENCE }
    }

    pub fn validate(&self) -> Result<()> {
        if self.app_version.len() > 128 || self.sequence > MAX_RELEASE_SEQUENCE {
            return Err("Invalid release identity".into());
        }
        semver::Version::parse(&self.app_version)?;
        Ok(())
    }

    pub fn from_info(info: &CFDictionary<CFString, CFType>) -> Result<Self> {
        let text = string(info, "PlaybitReleaseSequence")?;
        let sequence = text.parse::<u64>()?;
        if sequence.to_string() != text {
            return Err("Invalid signed release sequence".into());
        }
        let identity = Self { app_version: string(info, "PlaybitVersion")?, sequence };
        identity.validate()?;
        Ok(identity)
    }
}

impl std::fmt::Display for ReleaseIdentity {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "{} (build {})", self.app_version, self.sequence)
    }
}

impl Installation {
    /// A generated guest resolves its original installation through the engine
    /// path in its authenticated descriptor, not through its copied executable.
    pub fn for_executable(executable: &Path) -> Result<Option<Self>> {
        let home = super::profile::credential_home()?.canonicalize()?;
        Self::for_executable_at(executable, &home)
    }

    fn for_executable_at(executable: &Path, home: &Path) -> Result<Option<Self>> {
        // Resolve executable aliases before selecting the outermost app bundle.
        let executable = executable.canonicalize()?;
        let Some(app) = executable
            .ancestors()
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.as_encoded_bytes().eq_ignore_ascii_case(b"app"))
            })
            .last()
        else {
            return Ok(None);
        };
        if app.as_os_str().len() > 4096 {
            return Err("Installation path exceeds 4096 bytes".into());
        }
        let info = match plist(app) {
            Ok(info) => info,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    && app.parent() != Some(Path::new("/Applications"))
                    && app.parent()
                        != Some(
                            super::profile::canonical_profile(&home.join("Applications"))?
                                .as_path(),
                        ) =>
            {
                // Packaging fixtures invoke the copied CLI before their app
                // metadata exists. An installed bundle must never bypass leases.
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if string(&info, "CFBundleIdentifier")? != "playbit.engine" {
            return Ok(None);
        }
        let profile = super::profile::canonical_profile(&home.join(".playbit"))?;
        let id =
            crate::identifier::encode(blake3::hash(app.as_os_str().as_encoded_bytes()).as_bytes());
        Ok(Some(Self { app: app.to_path_buf(), state: profile.join("updates").join(id) }))
    }

    /// Discovery and release checks also work for read-only installations.
    /// Recheck replacement eligibility immediately before changing the bundle.
    pub fn ensure_installable(&self) -> Result<()> {
        let home = super::profile::credential_home()?.canonicalize()?;
        self.ensure_installable_at(&home)
    }

    fn ensure_installable_at(&self, home: &Path) -> Result<()> {
        let parent = self.app.parent().ok_or(LOCATION_ERROR)?;
        let personal = super::profile::canonical_profile(&home.join("Applications"))?;
        if (parent != Path::new("/Applications") && parent != personal)
            || fs::symlink_metadata(&self.app)?.uid() != current_uid()
            || !writable(&self.app)
            || !writable(parent)
        {
            return Err(LOCATION_ERROR.into());
        }
        Ok(())
    }

    pub fn open(&self) -> Result<()> {
        // The default profile may have been deliberately relocated. Its resolved
        // directory and both updater ancestors must remain private and owned.
        if let Some(updates) = self.state.parent().filter(|path| path.ends_with("updates")) {
            private_directory(updates.parent().ok_or("Missing default profile directory")?)?;
            private_directory(updates)?;
        }
        private_directory(&self.state)
    }

    fn lock(&self, name: &str, mode: LockMode) -> Result<Option<Lease>> {
        self.open()?;
        let file = self.file(name)?;
        if !lock_file(&file, mode)? {
            return Ok(None);
        }
        Ok(Some(Lease { _file: file }))
    }

    pub fn lease(&self) -> Result<Lease> {
        let lease = self.lock("clients.lock", LockMode::Shared)?.ok_or("Missing client lease")?;
        // A process may have been loaded just before another updater acquired
        // the gate. Do not let that old process start against the new bundle.
        if self.release()? != ReleaseIdentity::running() {
            return Err("Playbit was updated. Reopen this app or run the command again.".into());
        }
        Ok(lease)
    }

    /// Held through engine teardown, after its IPC listener has closed.
    /// Acquire only while holding a client startup lease.
    pub fn engine_lease(&self) -> Result<Lease> {
        self.lock("engines.lock", LockMode::Shared)?.ok_or("Missing engine lease".into())
    }

    /// Authenticated stop has already observed process exit. A remaining lease
    /// means another engine is still draining or could not be reached safely.
    pub fn drained_engines(&self) -> Result<Lease> {
        self.lock("engines.lock", LockMode::TryExclusive)?.ok_or_else(|| {
            "A Playbit engine is still finishing work. Reopen Playbit and try the update again."
                .into()
        })
    }

    pub fn release(&self) -> Result<ReleaseIdentity> {
        ReleaseIdentity::from_info(&plist(&self.app)?)
    }

    pub fn engine(&self) -> PathBuf {
        self.app.join("Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine")
    }

    pub fn exclusive(&self) -> Result<Lease> {
        // Blocking flock sleeps in the kernel. Existing clients keep working;
        // their final close wakes this one installer without a polling timer.
        self.lock("clients.lock", LockMode::Exclusive)?.ok_or("Missing installation lease".into())
    }

    pub fn operation(&self) -> Result<Lease> {
        self.lock("operation.lock", LockMode::Exclusive)?.ok_or("Missing operation lease".into())
    }

    pub fn try_operation(&self) -> Result<Option<Lease>> {
        self.lock("operation.lock", LockMode::TryExclusive)
    }

    pub fn try_installer(&self) -> Result<Option<Lease>> {
        self.lock("installer.lock", LockMode::TryExclusive)
    }

    pub fn run_interrupt(&self) -> Result<RunInterrupt> {
        self.open()?;
        Ok(RunInterrupt { file: self.file(RUN_INTERRUPT)? })
    }

    pub fn interrupt_runs(&self) -> Result<Lease> {
        self.open()?;
        let file = self.file(RUN_INTERRUPT)?;
        file.lock()?;
        // The metadata change wakes the vnode observer while this descriptor
        // stays open. Only the lock carries state.
        file.set_modified(std::time::SystemTime::now())?;
        Ok(Lease { _file: file })
    }

    pub fn file(&self, name: &str) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(self.state.join(name))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != current_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("Unsafe update state file".into());
        }
        Ok(file)
    }

    pub fn register_profile(&self, profile: &Path) -> Result<()> {
        let _registry = self.lock("profiles.lock", LockMode::Exclusive)?;
        let profile = std::path::absolute(profile)?;
        if profile.as_os_str().len() > 4096 {
            return Err("Profile path exceeds update registry limit".into());
        }
        let mut profiles = self.profiles()?;
        if profiles.contains(&profile) {
            return Ok(());
        }
        // Inactive profiles with installed wrappers still need runtime re-signing
        // at cutover. The client lease excludes cutover during registration.
        profiles.retain(|path| {
            path.join(crate::paths::ENGINE_SOCKET).exists()
                || path.join(crate::paths::NATIVE_APPS).join(".installation").is_file()
        });
        if profiles.len() == MAX_PROFILES {
            return Err("Too many active profiles for this Playbit installation".into());
        }
        profiles.push(profile);
        self.write("profiles.json", &serde_json::to_vec(&profiles)?)
    }

    pub fn profiles(&self) -> Result<Vec<PathBuf>> {
        let bytes = match read(&self.state.join("profiles.json"), MAX_REGISTRY) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let paths: Vec<PathBuf> = serde_json::from_slice(&bytes)?;
        if paths.len() > MAX_PROFILES
            || paths.iter().any(|p| !p.is_absolute() || p.as_os_str().len() > 4096)
        {
            return Err("Invalid installation profile registry".into());
        }
        Ok(paths)
    }

    /// Atomic durable replacement. Callers hold either operation or registry lock.
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_REGISTRY as usize {
            return Err("Update record exceeds limit".into());
        }
        let temporary = self.state.join(format!("{name}.tmp"));
        let mut file = self.file(&format!("{name}.tmp"))?;
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(temporary, self.state.join(name))?;
        File::open(&self.state)?.sync_all()?;
        Ok(())
    }
}

fn lock_file(file: &File, mode: LockMode) -> Result<bool> {
    match mode {
        LockMode::Shared => file.lock_shared().map(|()| true).map_err(Into::into),
        LockMode::Exclusive => file.lock().map(|()| true).map_err(Into::into),
        LockMode::TryExclusive => match file.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error.into()),
        },
    }
}

#[allow(unsafe_code, reason = "access borrows a NUL-terminated path for a permission check.")]
fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: the path remains live throughout the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

pub fn client_lease(executable: &Path) -> Result<Option<Lease>> {
    let home = super::profile::credential_home()?.canonicalize()?;
    client_lease_at(executable, &home)
}

fn client_lease_at(executable: &Path, home: &Path) -> Result<Option<Lease>> {
    let Some(installation) = Installation::for_executable_at(executable, home)? else {
        return Ok(None);
    };
    let profile = home.join(".playbit");
    if installation.state.parent() == Some(profile.join("updates").as_path()) {
        repair_legacy_profile(&profile)?;
    }
    installation.lease().map(Some)
}

/// Older apps created ~/.playbit with ordinary directory permissions. Tighten
/// read/search access only when the owner already has exclusive write access.
fn repair_legacy_profile(path: &Path) -> Result<()> {
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let metadata = directory.metadata()?;
    let mode = metadata.mode() & 0o7777;
    if metadata.uid() == current_uid()
        && mode & !0o055 == 0o700
        && mode != 0o700
        && super::files::privacy::acl::empty(&directory)?
    {
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn private_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        return Err("Unsafe update directory".into());
    }
    Ok(())
}

#[allow(unsafe_code, reason = "getuid takes no arguments and returns this process's real UID.")]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn read(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > max {
        return Err("Invalid or oversized update file".into());
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err("Update file exceeds limit".into());
    }
    Ok(bytes)
}

#[allow(unsafe_code, reason = "Core Foundation returns an owned, type-checked property list.")]
pub fn plist(app: &Path) -> Result<CFDictionary<CFString, CFType>> {
    let data = CFData::from_buffer(&read(&app.join("Contents/Info.plist"), MAX_PLIST)?);
    let (raw, _) = propertylist::create_with_data(data, propertylist::kCFPropertyListImmutable)
        .map_err(|e| e.to_string())?;
    // SAFETY: successful create returns one owned non-null CF object. Check its
    // dynamic dictionary type before treating values as retained CFTypes.
    let value = unsafe { CFType::wrap_under_create_rule(raw) };
    let dictionary =
        value.downcast::<CFDictionary>().ok_or("App Info.plist is not a dictionary")?;
    Ok(unsafe { CFDictionary::wrap_under_get_rule(dictionary.as_concrete_TypeRef()) })
}

pub fn string(info: &CFDictionary<CFString, CFType>, key: &str) -> Result<String> {
    info.find(CFString::new(key))
        .and_then(|value| value.downcast::<CFString>())
        .map(|value| value.to_string())
        .ok_or_else(|| format!("Missing app property: {key}").into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn incomplete_development_bundles_work_but_installed_metadata_is_required() {
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let executable = installation.app.join("Contents/MacOS/Playbit");
        fs::remove_file(installation.app.join("Contents/Info.plist")).unwrap();
        assert!(Installation::for_executable_at(&executable, root.path()).unwrap().is_none());
        let applications = root.path().join("Applications");
        fs::create_dir(&applications).unwrap();
        let installed = applications.join("Playbit.app");
        fs::rename(&installation.app, &installed).unwrap();
        let executable = installed.join("Contents/MacOS/Playbit");
        assert!(Installation::for_executable_at(&executable, root.path()).is_err());
    }

    fn fixture(root: &Path) -> Installation {
        let app = root.join("Playbit.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::write(app.join("Contents/MacOS/Playbit"), b"executable fixture").unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
                 CFBundleIdentifier</key><string>playbit.engine</string><key>\
                 PlaybitVersion</key><string>{}</string><key>PlaybitReleaseSequence</key>\
                 <string>{}</string></dict></plist>",
                crate::RELEASE_VERSION,
                crate::RELEASE_SEQUENCE
            ),
        )
        .unwrap();
        Installation { app, state: root.join("state") }
    }

    #[test]
    fn updater_state_is_per_canonical_installation_in_the_default_profile() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let executable = installation.app.join("Contents/MacOS/Playbit");
        let alias = root.path().join("Playbit alias");
        symlink(&executable, &alias).unwrap();
        let first = Installation::for_executable_at(&executable, &home).unwrap().unwrap();
        let second = Installation::for_executable_at(&alias, &home).unwrap().unwrap();
        assert_eq!(first.app, second.app);
        assert_eq!(first.state, second.state);
        assert_eq!(first.state.parent().unwrap(), home.join(".playbit/updates"));
        assert_eq!(
            crate::identifier::decode::<32>(first.state.file_name().unwrap().to_str().unwrap())
                .unwrap(),
            *blake3::hash(first.app.as_os_str().as_encoded_bytes()).as_bytes(),
        );
        first.open().unwrap();
        let other = root.path().join("other");
        fs::create_dir(&other).unwrap();
        let other = fixture(&other);
        let other =
            Installation::for_executable_at(&other.app.join("Contents/MacOS/Playbit"), &home)
                .unwrap()
                .unwrap();
        assert_ne!(first.state, other.state);
    }

    #[test]
    fn client_lease_repairs_legacy_profile_permissions() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let executable = fixture(&home).app.join("Contents/MacOS/Playbit");
        let profile = home.join(".playbit");
        fs::create_dir(&profile).unwrap();
        fs::write(profile.join("legacy-data"), b"preserved").unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();

        let _lease = client_lease_at(&executable, &home).unwrap().unwrap();
        assert_eq!(fs::metadata(&profile).unwrap().mode() & 0o7777, 0o700);
        assert_eq!(fs::read(profile.join("legacy-data")).unwrap(), b"preserved");

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(client_lease_at(&executable, &home).is_err());
        assert_eq!(fs::metadata(&profile).unwrap().mode() & 0o7777, 0o775);

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file"])
            .arg(&profile)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(client_lease_at(&executable, &home).is_err());
        assert_eq!(fs::metadata(&profile).unwrap().mode() & 0o7777, 0o755);
    }

    #[test]
    fn relocated_default_profile_keeps_one_updater_namespace() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let profile = root.path().join("relocated-profile");
        private_directory(&profile).unwrap();
        symlink(&profile, home.join(".playbit")).unwrap();
        let discovered = Installation::for_executable_at(
            &installation.app.join("Contents/MacOS/Playbit"),
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            discovered.state.parent().unwrap(),
            profile.canonicalize().unwrap().join("updates")
        );
        discovered.open().unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(client_lease_at(&installation.app.join("Contents/MacOS/Playbit"), &home).is_err());
        assert_eq!(fs::metadata(&profile).unwrap().mode() & 0o7777, 0o755);
    }

    #[test]
    fn updater_state_ignores_environment_home_and_profile_overrides() {
        const CHILD: &str = "PLAYBIT_INSTALLATION_TEST_EXECUTABLE";
        if let Some(executable) = std::env::var_os(CHILD) {
            let installation =
                Installation::for_executable(Path::new(&executable)).unwrap().unwrap();
            let home = super::super::profile::credential_home().unwrap().canonicalize().unwrap();
            let profile = super::super::profile::canonical_profile(&home.join(".playbit")).unwrap();
            assert_eq!(installation.state.parent().unwrap(), profile.join("updates"));
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::installation::tests::updater_state_ignores_environment_home_and_\
                 profile_overrides",
            ])
            .env(CHILD, installation.app.join("Contents/MacOS/Playbit"))
            .env("HOME", root.path().join("overridden-home"))
            .env("PLAYBIT_PROFILE", root.path().join("custom-profile"))
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn release_discovery_and_state_do_not_require_a_writable_installation_parent() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let applications = home.join("Applications");
        fs::create_dir(&applications).unwrap();
        let installation = fixture(&applications);
        let discovered = Installation::for_executable_at(
            &installation.app.join("Contents/MacOS/Playbit"),
            &home,
        )
        .unwrap()
        .unwrap();
        discovered.ensure_installable_at(&home).unwrap();
        fs::set_permissions(&applications, fs::Permissions::from_mode(0o555)).unwrap();
        discovered.open().unwrap();
        assert!(discovered.ensure_installable_at(&home).is_err());
        assert!(
            Installation::for_executable_at(
                &installation.app.join("Contents/MacOS/Playbit"),
                &home
            )
            .unwrap()
            .is_some()
        );
        fs::set_permissions(&applications, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn operation_and_installer_locks_exclude_duplicate_owners() {
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let operation = installation.try_operation().unwrap().unwrap();
        assert!(installation.try_operation().unwrap().is_none());
        drop(operation);
        installation.try_operation().unwrap().unwrap();
        let installer = installation.try_installer().unwrap().unwrap();
        assert!(installation.try_installer().unwrap().is_none());
        drop(installer);
        installation.try_installer().unwrap().unwrap();
    }

    #[test]
    fn exclusive_installation_waits_for_all_clients_and_allows_reopening() {
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let first = installation.lease().unwrap();
        let second = installation.lease().unwrap();
        let other = installation.clone();
        let (send, receive) = mpsc::channel();
        let (finish, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _exclusive = other.exclusive().unwrap();
            send.send(()).unwrap();
            finished.recv().unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(20)).is_err());
        let reopened = installation.file("clients.lock").unwrap();
        reopened.try_lock_shared().unwrap();
        drop(first);
        drop(second);
        assert!(receive.recv_timeout(Duration::from_millis(20)).is_err());
        drop(reopened);
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            installation.file("clients.lock").unwrap().try_lock_shared(),
            Err(TryLockError::WouldBlock)
        ));
        finish.send(()).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn old_clients_cannot_enter_after_an_update() {
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        installation.lease().unwrap();
        let plist = installation.app.join("Contents/Info.plist");
        let text = fs::read_to_string(&plist).unwrap().replace(crate::RELEASE_VERSION, "99.0.0");
        fs::write(plist, text).unwrap();
        assert!(installation.lease().is_err());
    }

    #[test]
    fn engine_teardown_must_finish_even_after_the_last_client_leaves() {
        // Socket readiness is not proof of idleness: retain the engine lease
        // past client exit, exactly as an engine draining accepted writes does.
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let client = installation.lease().unwrap();
        let engine = installation.engine_lease().unwrap();
        drop(client);
        let _exclusive = installation.exclusive().unwrap();
        assert!(installation.drained_engines().is_err());
        drop(engine);
        installation.drained_engines().unwrap();
    }

    #[test]
    fn profile_registration_retains_live_endpoints_and_inactive_installed_wrappers() {
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        let active = root.path().join("active");
        let stopped = root.path().join("stopped");
        let starting = root.path().join("starting");
        let wrapped = root.path().join("wrapped");
        fs::create_dir_all(wrapped.join(crate::paths::NATIVE_APPS)).unwrap();
        fs::write(wrapped.join(crate::paths::NATIVE_APPS).join(".installation"), b"bound").unwrap();
        fs::create_dir_all(active.join("control")).unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(active.join(crate::paths::ENGINE_SOCKET))
                .unwrap();
        installation.register_profile(&wrapped).unwrap();
        installation.register_profile(&stopped).unwrap();
        installation.register_profile(&active).unwrap();
        installation.register_profile(&starting).unwrap();
        assert_eq!(installation.profiles().unwrap(), vec![wrapped, active, starting]);
    }

    #[test]
    fn journal_write_rejects_hardlinks_before_truncating() {
        let root = tempfile::tempdir().unwrap();
        let installation = fixture(root.path());
        installation.open().unwrap();
        let unrelated = root.path().join("unrelated");
        fs::write(&unrelated, b"preserved").unwrap();
        fs::hard_link(&unrelated, installation.state.join("journal.json.tmp")).unwrap();
        assert!(installation.write("journal.json", b"replacement").is_err());
        assert_eq!(fs::read(unrelated).unwrap(), b"preserved");
    }

    #[test]
    fn private_state_rejects_symlinks_and_shared_directories() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        private_directory(&real).unwrap();
        symlink(&real, root.path().join("link")).unwrap();
        assert!(private_directory(&root.path().join("link")).is_err());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(private_directory(&real).is_err());
    }
}
