//! Installation preferences and isolated durable native profiles.
//! Explicit profiles have their own selection state and never change the user's
//! remembered channel. Process leases prevent another installation or helper
//! from selecting a conflicting environment while ordinary work is running.
use super::Result;
use crate::release_channel::ReleaseChannel;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const CHANNEL_BYTES_MAX: u64 = 7;
pub(crate) const PROFILE_MARKER: &str = "channel";
const CONFLICT: &str = "Another Playbit process is using a different channel. Close Playbit apps and commands before switching channels. Your profiles have been preserved.";
static CURRENT: OnceLock<Mutex<Option<Selection>>> = OnceLock::new();

fn default_path(home: &Path, channel: ReleaseChannel) -> PathBuf {
    home.join(match channel {
        ReleaseChannel::Public => ".playbit",
        ReleaseChannel::Testing => ".playbit-testing",
        #[cfg(test)]
        _ => unreachable!("personal Testing requires an explicit profile"),
    })
}

pub(crate) struct Selection {
    pub(crate) channel: ReleaseChannel,
    pub(crate) path: PathBuf,
    #[cfg(any(feature = "desktop", test))]
    switchable: bool,
    active: Option<Lease>,
}

// A concurrent fork must not extend the owner's lock lifetime until exec.
// Helpers acquire independent leases when they initialize their own profile.
struct Lease(File);

impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            crate::diagnostic!("Failed to release profile channel lease: {error}");
        }
    }
}

struct Locations {
    preference: Option<PathBuf>,
    active: PathBuf,
}

fn installation_path(executable: &Path) -> Result<PathBuf> {
    let executable = executable.canonicalize()?;
    let installation = executable
        .ancestors()
        .filter(|path| {
            path.extension().is_some_and(|ext| ext.as_encoded_bytes().eq_ignore_ascii_case(b"app"))
        })
        .last()
        .unwrap_or(&executable);
    if installation.as_os_str().len() > 4096 {
        return Err("Installation path exceeds 4096 bytes".into());
    }
    Ok(installation.to_path_buf())
}

fn installation_preference(home: &Path, executable: &Path) -> Result<PathBuf> {
    let installation = installation_path(executable)?;
    let id = crate::identifier::encode(
        blake3::hash(installation.as_os_str().as_encoded_bytes()).as_bytes(),
    );
    Ok(home.join(".playbit-installations").join(id))
}

// Installation location supplies defaults, never authority to reinterpret data.
fn launch_default(home: &Path, installation: &Path) -> Result<(PathBuf, ReleaseChannel)> {
    #[cfg(target_os = "macos")]
    if installation != Path::new("/Applications/Playbit.app")
        && installation != home.join("Applications/Playbit.app")
    {
        let testing = default_path(home, ReleaseChannel::Testing);
        match fs::metadata(&testing) {
            Ok(metadata) if metadata.is_dir() => {
                if let Some(channel) = read(&testing.join(PROFILE_MARKER))? {
                    return Ok((testing, channel));
                }
                // Unmarked account/database state is Public. Only a fresh
                // directory can adopt Testing without reinterpreting its data.
                for name in
                    [crate::paths::PROFILE_DB, "instance.json", "account.kvdb", "volumes", "blobs"]
                {
                    match fs::symlink_metadata(testing.join(name)) {
                        Ok(_) => return Ok((testing, ReleaseChannel::Public)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                        Err(error) => return Err(error.into()),
                    }
                }
                return Ok((testing, ReleaseChannel::Testing));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = installation;
    Ok((default_path(home, ReleaseChannel::Public), ReleaseChannel::Public))
}

fn resolve_selection(
    home: &Path,
    executable: &Path,
    profile: Option<&Path>,
    explicit: Option<ReleaseChannel>,
    remembered: Option<ReleaseChannel>,
) -> Result<(PathBuf, ReleaseChannel)> {
    if profile.is_none() && explicit.is_some_and(ReleaseChannel::is_personal) {
        return Err("Personal Testing requires an explicit isolated PLAYBIT_PROFILE".into());
    }
    let (path, channel) = if let Some(profile) = profile {
        let recorded = profile_binding(profile, remembered)?;
        if let (Some(explicit), Some(recorded)) = (explicit, recorded)
            && explicit != recorded
            && (explicit.is_personal() || recorded.is_personal())
        {
            return Err("Profile belongs to another channel".into());
        }
        (profile.to_path_buf(), explicit.or(recorded).unwrap_or_default())
    } else if let Some(channel) = explicit {
        (default_path(home, channel), channel)
    } else {
        let (path, default) = launch_default(home, &installation_path(executable)?)?;
        let channel = remembered.unwrap_or(default);
        if channel.is_personal() {
            return Err("Personal Testing requires an explicit isolated PLAYBIT_PROFILE".into());
        }
        (if channel == default { path } else { default_path(home, channel) }, channel)
    };
    let path = canonical_profile(&path)?;
    if channel.is_personal() {
        require_personal_profile(home, &path)?;
    }
    Ok((path, channel))
}

// The state beside a custom profile survives removal of the profile itself.
fn profile_binding(
    profile: &Path,
    remembered: Option<ReleaseChannel>,
) -> Result<Option<ReleaseChannel>> {
    let recorded = read(&profile.join(PROFILE_MARKER))?;
    let personal = remembered.filter(|channel| channel.is_personal());
    if let (Some(recorded), Some(personal)) = (recorded, personal)
        && recorded != personal
    {
        return Err("Profile belongs to another channel".into());
    }
    Ok(recorded.or(personal))
}

fn require_personal_profile(home: &Path, profile: &Path) -> Result<()> {
    #[cfg(all(target_os = "macos", feature = "connected-engine"))]
    let os_home = credential_home()?;
    #[cfg(all(target_os = "macos", feature = "connected-engine"))]
    let homes = [home, os_home.as_path()];
    #[cfg(not(all(target_os = "macos", feature = "connected-engine")))]
    let homes = [home];
    for home in homes {
        for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
            if matches_default_profile(home, profile, channel)? {
                return Err("Personal Testing requires an explicit isolated PLAYBIT_PROFILE".into());
            }
        }
    }
    Ok(())
}

fn locations(home: &Path, profile: Option<&Path>) -> Result<Locations> {
    let active = home.join(".playbit-runtime");
    let Some(profile) = profile else {
        return Ok(Locations {
            preference: Some(installation_preference(home, &std::env::current_exe()?)?),
            active,
        });
    };
    if matches_default_profile(home, profile, ReleaseChannel::Public)?
        || matches_default_profile(home, profile, ReleaseChannel::Testing)?
    {
        // Helpers and prepared guests carry a validated profile rather than
        // changing the installation's remembered user preference.
        return Ok(Locations { preference: None, active });
    }
    let active = custom_state_path(profile)?;
    Ok(Locations { preference: Some(active.clone()), active })
}

fn custom_state_path(profile: &Path) -> Result<PathBuf> {
    let id =
        crate::identifier::encode(blake3::hash(profile.as_os_str().as_encoded_bytes()).as_bytes());
    Ok(profile
        .parent()
        .ok_or("Profile requires a parent directory")?
        .join(format!(".playbit-channel-{id}")))
}

fn matches_default_profile(home: &Path, profile: &Path, channel: ReleaseChannel) -> Result<bool> {
    let default = canonical_profile(&default_path(home, channel))?;
    if profile == default {
        return Ok(true);
    }
    #[cfg(target_os = "macos")]
    if let (Some(parent), Some(default_parent)) = (profile.parent(), default.parent()) {
        if let (Ok(profile), Ok(default)) = (fs::metadata(profile), fs::metadata(&default)) {
            return Ok(same_file(&profile, &default));
        }
        if let (Ok(parent), Ok(default_parent_metadata)) =
            (fs::metadata(parent), fs::metadata(default_parent))
            && same_file(&parent, &default_parent_metadata)
            && case_insensitive(&File::open(default_parent)?)?
        {
            return Ok(macos_names_equal(profile, &default));
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "Read-only comparison of two live Core Foundation strings.")]
fn macos_names_equal(a: &Path, b: &Path) -> bool {
    use core_foundation::{
        base::{CFComparisonResult, TCFType},
        string::{CFString, CFStringCompare, kCFCompareCaseInsensitive, kCFCompareNonliteral},
    };
    let (Some(a), Some(b)) =
        (a.file_name().and_then(|s| s.to_str()), b.file_name().and_then(|s| s.to_str()))
    else {
        return false;
    };
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    let (a, b) = (CFString::new(a), CFString::new(b));
    // SAFETY: Both strings remain live during this read-only comparison.
    unsafe {
        CFStringCompare(
            a.as_concrete_TypeRef(),
            b.as_concrete_TypeRef(),
            kCFCompareCaseInsensitive | kCFCompareNonliteral,
        ) == CFComparisonResult::EqualTo
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "Read a directory's case sensitivity through a live descriptor.")]
fn case_insensitive(directory: &File) -> Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: fpathconf takes a live descriptor and a platform-defined option.
    match unsafe { libc::fpathconf(directory.as_raw_fd(), libc::_PC_CASE_SENSITIVE) } {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(std::io::Error::last_os_error().into()),
    }
}

#[cfg(target_os = "macos")]
const NAME_NAMESPACE: &str = ".playbit-profile-names";

#[cfg(target_os = "macos")]
fn require_unreserved_profile_name(profile: &Path) -> Result<()> {
    let namespace = Path::new(NAME_NAMESPACE);
    for path in profile.ancestors().filter(|path| macos_names_equal(path, namespace)) {
        if path.file_name() == namespace.file_name()
            || case_insensitive(&File::open(
                path.ancestors()
                    .skip(1)
                    .find(|parent| parent.is_dir())
                    .ok_or("Profile requires an accessible parent directory")?,
            )?)?
        {
            return Err("Profile path is reserved for Playbit profile state".into());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn profile_name_slot(profile: &Path) -> Result<PathBuf> {
    // Keep the basename intact so the filesystem's actual name lookup rules
    // decide which paths identify the same profile, even after its removal.
    let parent = profile.parent().ok_or("Profile requires a parent directory")?;
    Ok(parent.join(NAME_NAMESPACE).join(profile.file_name().ok_or("Profile needs a name")?))
}

#[cfg(target_os = "macos")]
fn register_profile_name(home: &Path, profile: &Path) -> Result<PathBuf> {
    for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
        if matches_default_profile(home, profile, channel)? {
            return canonical_profile(&default_path(home, channel));
        }
    }
    // Preserve an existing lease key when first recording an older profile.
    // If no state matches the requested spelling, the existing directory's
    // physical name may still identify the legacy lease.
    let actual;
    let name = if profile.is_dir() && !custom_state_path(profile)?.exists() {
        actual = macos_path_for_directory(profile)?;
        &actual
    } else {
        profile
    };
    let slot = profile_name_slot(name)?;
    private_directory(slot.parent().expect("profile name namespace"))?;
    private_directory(&slot)?;
    canonical_profile(profile)
}

pub(crate) fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.uid() != current_uid()
    {
        return Err("Unsafe channel state file".into());
    }
    Ok(file)
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    validate_private_directory(&fs::symlink_metadata(path)?)
}

fn validate_private_directory(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_dir() || metadata.mode() & 0o077 != 0 || metadata.uid() != current_uid() {
        return Err("Unsafe channel state directory".into());
    }
    Ok(())
}

pub fn read(path: &Path) -> Result<Option<ReleaseChannel>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > CHANNEL_BYTES_MAX {
        return Err("Invalid channel selection record".into());
    }
    let mut text = String::new();
    file.take(CHANNEL_BYTES_MAX + 1).read_to_string(&mut text)?;
    Ok(Some(ReleaseChannel::parse(&text)?))
}

pub fn for_profile(profile: &Path) -> Result<ReleaseChannel> {
    Ok(read(&profile.join(PROFILE_MARKER))?.unwrap_or_default())
}

#[cfg(all(target_os = "macos", feature = "connected-engine"))]
pub(crate) fn is_default_profile(profile: &Path, channel: ReleaseChannel) -> Result<bool> {
    if channel.is_personal() {
        return Ok(false);
    }
    // HOME overrides isolate launches but never grant access to the OS user's
    // distribution recovery namespace. Resolve that authority from the OS.
    let home = credential_home()?.canonicalize()?;
    matches_default_profile(&home, &canonical_profile(profile)?, channel)
}

#[cfg(all(target_os = "macos", feature = "connected-engine"))]
#[allow(unsafe_code, reason = "The OS user record is copied from one bounded getpwuid_r buffer.")]
pub(crate) fn credential_home() -> Result<PathBuf> {
    use std::{ffi::CStr, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let mut record = MaybeUninit::<libc::passwd>::uninit();
    let mut storage = [0u8; 16 * 1024];
    let mut found = std::ptr::null_mut();
    // SAFETY: record, storage and output pointer are writable and remain live
    // through the call. Successful pointers borrow this same storage.
    let status = unsafe {
        libc::getpwuid_r(
            current_uid(),
            record.as_mut_ptr(),
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut found,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status).into());
    }
    if found.is_null() {
        return Err("OS account home is unavailable".into());
    }
    // SAFETY: successful getpwuid_r initialized record and its terminated fields.
    let record = unsafe { record.assume_init() };
    if record.pw_dir.is_null() || record.pw_uid != current_uid() {
        return Err("OS account home is unavailable".into());
    }
    // SAFETY: pw_dir is a terminated field in the successful bounded lookup.
    let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes();
    if bytes.is_empty() || bytes.len() > 4096 {
        return Err("Invalid OS account home".into());
    }
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
    if !home.is_absolute() {
        return Err("OS account home must be absolute".into());
    }
    Ok(home)
}

fn write(state: &Path, channel: ReleaseChannel) -> Result<()> {
    let mut file = private_file(&state.join("selected.tmp"))?;
    file.set_len(0)?;
    file.write_all(channel.name().as_bytes())?;
    file.sync_all()?;
    fs::rename(state.join("selected.tmp"), state.join("selected"))?;
    File::open(state)?.sync_all()?;
    Ok(())
}

fn lock_admission(state: &Path) -> Result<Lease> {
    let admission = private_file(&state.join("admission.lock"))?;
    match admission.try_lock() {
        Ok(()) => Ok(Lease(admission)),
        Err(TryLockError::WouldBlock) => {
            Err("Another Playbit channel selection is in progress".into())
        }
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

impl Selection {
    pub fn open(
        home: &Path,
        profile: Option<&Path>,
        explicit: Option<ReleaseChannel>,
    ) -> Result<Self> {
        let home = home.canonicalize()?;
        let profile = profile
            .map(|path| -> Result<PathBuf> {
                let path = canonical_profile(path)?;
                #[cfg(target_os = "macos")]
                require_unreserved_profile_name(&path)?;
                if explicit.is_some_and(ReleaseChannel::is_personal) {
                    require_personal_profile(&home, &path)?;
                }
                fs::create_dir_all(path.parent().ok_or("Profile requires a parent directory")?)?;
                let path = canonical_profile(&path)?;
                #[cfg(target_os = "macos")]
                let path = register_profile_name(&home, &path)?;
                Ok(path)
            })
            .transpose()?;
        let switchable = profile.is_none();
        let locations = locations(&home, profile.as_deref())?;
        private_directory(&locations.active)?;
        if let Some(preference) = &locations.preference {
            if switchable {
                private_directory(&home.join(".playbit-installations"))?;
            }
            private_directory(preference)?;
        }
        let state = locations.preference.as_ref().unwrap_or(&locations.active);
        let _admission = lock_admission(&locations.active)?;
        let remembered = read(&state.join("selected"))?;
        let (path, channel) = resolve_selection(
            &home,
            &std::env::current_exe()?,
            profile.as_deref(),
            explicit,
            remembered,
        )?;
        if channel == ReleaseChannel::Testing
            && matches_default_profile(&home, &path, ReleaseChannel::Public)?
        {
            return Err(
                "Testing requires its own profile; ~/.playbit is reserved for Public".into()
            );
        }
        let active_channel = read(&locations.active.join("selected"))?.unwrap_or_default();
        let active = admit(&locations.active, &path, channel, active_channel)?;
        if locations.preference.is_some() && remembered != Some(channel) {
            write(state, channel)?;
        }
        Ok(Self {
            channel,
            path,
            #[cfg(any(feature = "desktop", test))]
            switchable,
            active: Some(active),
        })
    }
}

fn admit(
    state: &Path,
    path: &Path,
    channel: ReleaseChannel,
    remembered: ReleaseChannel,
) -> Result<Lease> {
    let active = Lease(private_file(&state.join("active.lock"))?);
    let exclusive = match active.0.try_lock() {
        Ok(()) => true,
        Err(TryLockError::WouldBlock) if channel != remembered => return Err(CONFLICT.into()),
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(error)) => return Err(error.into()),
    };
    // The caller retains admission during conversion and the profile recheck.
    match active.0.try_lock_shared() {
        Ok(()) => (),
        Err(TryLockError::WouldBlock) => {
            return Err("Another Playbit profile admission is in progress".into());
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    // Admission serializes profile preparation even when same-channel clients
    // retain leases. A deleted profile must be recreated, not interpreted as
    // Public solely because its channel marker disappeared.
    prepare(path, channel)?;
    if exclusive && (channel != remembered || !state.join("selected").exists()) {
        write(state, channel)?;
    }
    Ok(active)
}

/// Recover an orphan for the selected profile before acquiring the GUI lease.
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub fn initialize_gui() -> Result<()> {
    let (path, channel) = resolve_environment()?;
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    recover_gui_profile(&home.canonicalize()?, &path, channel)?;
    initialize()
}

#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(super) fn recover_gui_profile(home: &Path, path: &Path, channel: ReleaseChannel) -> Result<()> {
    let path = canonical_profile(path)?;
    let Some(state) = locations(home, Some(&path))?.preference else {
        return super::recovery_endpoint::retire_if_stale(&path, channel);
    };
    // Keep the retained owner stable while retiring its peer when the sibling
    // state exists. A missing sibling directory is never created by recovery.
    let _admission = match fs::symlink_metadata(&state) {
        Ok(_) => {
            private_directory(&state)?;
            Some(lock_admission(&state)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let remembered = read(&state.join("selected"))?;
    let binding = profile_binding(&path, remembered)?;
    if let Some(binding) = binding
        && binding != channel
        && (binding.is_personal() || channel.is_personal())
    {
        return Err("Profile belongs to another channel".into());
    }
    if channel.is_personal() && binding != Some(channel) {
        return Ok(());
    }
    super::recovery_endpoint::retire_if_stale(&path, channel)
}

pub(crate) fn canonical_profile(path: &Path) -> Result<PathBuf> {
    let profile = std::path::absolute(path)?;
    if profile.as_os_str().len() > 4096 {
        return Err("Profile path exceeds 4096 bytes".into());
    }
    let parent = profile.parent().ok_or("Profile requires a parent directory")?;
    let profile = if profile.exists() {
        profile.canonicalize()?
    } else if !parent.exists() {
        profile
    } else {
        parent.canonicalize()?.join(profile.file_name().ok_or("Profile needs a name")?)
    };
    #[cfg(target_os = "macos")]
    if let Some(name) = registered_profile_name(&profile)? {
        return Ok(name);
    }
    Ok(profile)
}

#[cfg(target_os = "macos")]
fn registered_profile_name(profile: &Path) -> Result<Option<PathBuf>> {
    let slot = profile_name_slot(profile)?;
    let namespace = slot.parent().expect("profile name namespace");
    for directory in [namespace, &slot] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) => validate_private_directory(&metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    let slot = macos_path_for_directory(&slot)?;
    let parent = slot.ancestors().nth(2).ok_or("Profile name has no parent directory")?;
    let name = slot.file_name().ok_or("Profile name is missing")?;
    Ok(Some(parent.join(name)))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "F_GETPATH writes a bounded path for a live directory descriptor.")]
fn macos_path_for_directory(path: &Path) -> Result<PathBuf> {
    use std::{
        ffi::{CStr, OsStr},
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
    };
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)?;
    let mut bytes = [0; libc::PATH_MAX as usize];
    // SAFETY: the descriptor is live and F_GETPATH writes at most PATH_MAX bytes.
    if unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let path = CStr::from_bytes_until_nul(&bytes)?.to_bytes();
    Ok(PathBuf::from(OsStr::from_bytes(path)))
}

pub fn initialize() -> Result<()> {
    let mut current =
        CURRENT.get_or_init(|| Mutex::new(None)).lock().expect("profile selection lock");
    if let Some(selected) = current.as_ref() {
        return if selected.active.is_some() {
            Ok(())
        } else {
            Err("Profile ownership is changing; reopen Playbit if the switch fails".into())
        };
    }
    let explicit = channel_from_environment()?;
    let profile = profile_from_environment()?;
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    *current = Some(Selection::open(&home, profile.as_deref(), explicit)?);
    Ok(())
}

/// Join the profile the client already admitted. Launchd uses the retained
/// default selection; isolated Launch Services requests carry their own binding.
#[cfg(all(target_os = "macos", feature = "process"))]
pub(super) fn initialize_agent(isolated: Option<(&Path, ReleaseChannel)>) -> Result<()> {
    if isolated.is_none() && std::env::var_os("PLAYBIT_PROFILE").is_some() {
        return initialize();
    }
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    let (path, channel) = if let Some((path, channel)) = isolated {
        if !path.is_absolute()
            || std::env::var_os(super::testing_profile::CREDENTIALS_ENV).is_some()
        {
            return Err("Invalid isolated background engine startup request".into());
        }
        let path = path.canonicalize()?;
        let os_home = credential_home()?.canonicalize()?;
        if matches_default_profile(&os_home, &path, ReleaseChannel::Public)?
            || matches_default_profile(&os_home, &path, ReleaseChannel::Testing)?
            || for_profile(&path)? != channel
        {
            return Err(
                "Background engine profile or channel does not match the isolated request".into()
            );
        }
        (path, channel)
    } else {
        let channel = read(&home.join(".playbit-runtime/selected"))?
            .ok_or("Open Playbit before starting its background service")?;
        if channel.is_personal() {
            return Err("Personal Testing requires an isolated background engine request".into());
        }
        (canonical_profile(&default_path(&home, channel))?, channel)
    };
    let mut current =
        CURRENT.get_or_init(|| Mutex::new(None)).lock().expect("profile selection lock");
    if let Some(selected) = current.as_ref() {
        return if selected.active.is_some() && selected.path == path && selected.channel == channel
        {
            Ok(())
        } else {
            Err("This process is already using another profile or channel".into())
        };
    }
    // Join like a bound guest, without overwriting the installation's remembered
    // GUI channel.
    *current = Some(Selection::open(&home, Some(&path), Some(channel))?);
    Ok(())
}

fn profile_from_environment() -> Result<Option<PathBuf>> {
    let profile = std::env::var_os("PLAYBIT_PROFILE").map(PathBuf::from);
    if profile.as_ref().is_some_and(|path| path.as_os_str().is_empty()) {
        return Err("PLAYBIT_PROFILE must not be empty".into());
    }
    profile.as_deref().map(canonical_profile).transpose()
}

/// Resolve once before admitting native work. Discovery/source-only CLI
/// commands may inspect this choice without creating a profile or state files.
pub fn resolve_environment() -> Result<(PathBuf, ReleaseChannel)> {
    let explicit = channel_from_environment()?;
    let profile = profile_from_environment()?;
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?.canonicalize()?;
    let executable = std::env::current_exe()?;
    let state = match profile.as_deref() {
        Some(path) => locations(&home, Some(path))?.preference,
        None => Some(installation_preference(&home, &executable)?),
    };
    let remembered = state.map(|state| read(&state.join("selected"))).transpose()?.flatten();
    resolve_selection(&home, &executable, profile.as_deref(), explicit, remembered)
}

pub fn channel_from_environment() -> Result<Option<ReleaseChannel>> {
    Ok(match std::env::var("PLAYBIT_CHANNEL") {
        Ok(value) => Some(ReleaseChannel::parse(&value)?),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err("PLAYBIT_CHANNEL must be UTF-8".into()),
    })
}

pub fn initialize_selected(path: &Path, channel: ReleaseChannel) -> Result<()> {
    let mut selected =
        CURRENT.get_or_init(|| Mutex::new(None)).lock().expect("profile selection lock");
    if let Some(current) = selected.as_ref() {
        return if current.active.is_some()
            && current.path == canonical_profile(path)?
            && current.channel == channel
        {
            Ok(())
        } else {
            Err("This process is already using another profile or channel".into())
        };
    }
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    let normal = !channel.is_personal()
        && std::env::var_os("PLAYBIT_PROFILE").is_none()
        && matches_default_profile(&home.canonicalize()?, &canonical_profile(path)?, channel)?;
    *selected =
        Some(Selection::open(&home, if normal { None } else { Some(path) }, Some(channel))?);
    Ok(())
}

/// The GUI has retired its worker and stopped its authenticated engine before
/// this call. Other clients/guests retain leases and refuse a conflicting switch.
/// Disk work occurs on a background lane, outside the short current-state lock.
#[cfg(feature = "desktop")]
pub(crate) fn switch_to(channel: ReleaseChannel) -> Result<()> {
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?.canonicalize()?;
    let (previous, active) = {
        let mut current = CURRENT
            .get()
            .ok_or("Profile selection is not initialized")?
            .lock()
            .expect("profile selection lock");
        let current = current.as_mut().ok_or("Profile selection is not initialized")?;
        if current.channel == channel {
            return Ok(());
        }
        if !current.switchable {
            return Err("An explicit PLAYBIT_PROFILE cannot change environment. Launch the other channel with its own isolated profile.".into());
        }
        (current.channel, current.active.take())
    };
    drop(active);
    let result = Selection::open(&home, None, Some(channel));
    let incoming = match result {
        Ok(incoming) => incoming,
        Err(error) => {
            let restored = Selection::open(&home, None, Some(previous));
            let restored = restored.map_err(|restore| format!("{error}; could not restore profile ownership: {restore}. Reopen Playbit to recover."))?;
            *CURRENT.get().expect("initialized profile").lock().expect("profile selection lock") =
                Some(restored);
            return Err(error);
        }
    };
    *CURRENT.get().expect("initialized profile").lock().expect("profile selection lock") =
        Some(incoming);
    Ok(())
}

pub fn current() -> (PathBuf, ReleaseChannel) {
    #[cfg(test)]
    if CURRENT.get().is_none() {
        return (
            std::env::var_os("PLAYBIT_PROFILE").map(PathBuf::from).unwrap_or_else(|| {
                default_path(&dirs::home_dir().expect("test home"), ReleaseChannel::Public)
            }),
            ReleaseChannel::Public,
        );
    }
    let selected = CURRENT
        .get()
        .expect("profile selection initialized at process entry")
        .lock()
        .expect("profile selection lock");
    let selected = selected.as_ref().expect("profile selection initialized at process entry");
    (selected.path.clone(), selected.channel)
}

/// Preserve the selected launch mode as well as its channel. An ordinary GUI
/// must remain switchable after reopening; an explicit profile remains bound.
#[cfg(feature = "desktop")]
pub(crate) fn configure_relaunch(command: &mut std::process::Command) -> Result<()> {
    let selected = CURRENT
        .get()
        .ok_or("Profile selection is not initialized")?
        .lock()
        .expect("profile selection lock");
    let selected = selected.as_ref().ok_or("Profile selection is not initialized")?;
    selected.configure_relaunch(command)
}

impl Selection {
    #[cfg(any(feature = "desktop", test))]
    #[allow(
        unsafe_code,
        reason = "The child restores ordinary process signal delivery before exec."
    )]
    fn configure_relaunch(&self, command: &mut std::process::Command) -> Result<()> {
        use std::os::unix::process::CommandExt;
        if self.active.is_none() {
            return Err("Cannot reopen while profile ownership is changing".into());
        }
        command.env("PLAYBIT_CHANNEL", self.channel.name());
        if self.switchable {
            command.env_remove("PLAYBIT_PROFILE");
        } else {
            command.env("PLAYBIT_PROFILE", &self.path);
        }
        // Dispatch workers block signals, including child-exit notifications.
        // A new GUI and its helpers must not inherit a worker's private mask.
        // SAFETY: only async-signal-safe functions run between fork and exec;
        // the closure neither allocates nor borrows state from another thread.
        unsafe {
            command.pre_exec(|| {
                let mut signals = std::mem::zeroed();
                libc::sigemptyset(&mut signals);
                if libc::sigprocmask(libc::SIG_SETMASK, &signals, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
}

/// A prepared guest's descriptor is already bound to a profile. A launch
/// override cannot redirect that capability into a different environment.
fn bound_channel(path: &Path, explicit: Option<ReleaseChannel>) -> Result<ReleaseChannel> {
    let channel = for_profile(path)?;
    if explicit.is_some_and(|explicit| explicit != channel) {
        return Err("PLAYBIT_CHANNEL conflicts with this app's bound profile".into());
    }
    Ok(channel)
}

/// Admit an owner whose descriptor already names its profile and channel.
pub fn initialize_at(path: &Path) -> Result<()> {
    #[cfg(test)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(test))]
    {
        let path = canonical_profile(path)?;
        let channel = bound_channel(&path, channel_from_environment()?)?;
        let mut selected =
            CURRENT.get_or_init(|| Mutex::new(None)).lock().expect("profile selection lock");
        if let Some(current) = selected.as_ref() {
            return if current.path == path && current.channel == channel && current.active.is_some()
            {
                Ok(())
            } else {
                Err("This process is already using another profile".into())
            };
        }
        let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
        *selected = Some(Selection::open(&home, Some(&path), Some(channel))?);
        Ok(())
    }
}

/// Called under exclusive selection or store admission. Never
/// reinterpret an existing ordinary profile as testing, even when signed out.
pub(crate) fn prepare(profile: &Path, channel: ReleaseChannel) -> Result<()> {
    if let Some(existing) = read(&profile.join(PROFILE_MARKER))? {
        if existing != channel {
            return Err("Profile belongs to another channel".into());
        }
        return Ok(());
    }
    if channel == ReleaseChannel::Public {
        match fs::DirBuilder::new().mode(0o700).create(profile) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && profile.is_dir() => {
            }
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    }
    for name in [crate::paths::PROFILE_DB, "instance.json", "account.kvdb", "volumes", "blobs"] {
        match fs::symlink_metadata(profile.join(name)) {
            Ok(_) => {
                return Err(
                    "Testing requires its own profile; public data has been preserved".into()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    private_directory(profile)?;
    let mut file = private_file(&profile.join(PROFILE_MARKER))?;
    file.write_all(channel.name().as_bytes())?;
    file.sync_all()?;
    File::open(profile)?.sync_all()?;
    Ok(())
}

#[allow(unsafe_code, reason = "getuid takes no arguments and returns this process's real UID.")]
pub(crate) fn current_uid() -> u32 {
    // SAFETY: getuid has no pointer or memory preconditions.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_testing_binds_only_an_isolated_profile_to_one_owner() {
        let home = tempfile::tempdir().expect("temporary home");
        let home = home.path();
        let profile = home.join("personal");
        let executable = std::env::current_exe().expect("test executable");
        for reserved in [".playbit", ".playbit-testing"] {
            assert!(
                resolve_selection(
                    home,
                    &executable,
                    Some(&home.join(reserved)),
                    Some(ReleaseChannel::FixtureA),
                    None
                )
                .is_err()
            );
        }
        assert!(
            resolve_selection(home, &executable, None, Some(ReleaseChannel::FixtureA), None)
                .is_err()
        );
        let selection = Selection::open(home, Some(&profile), Some(ReleaseChannel::FixtureA))
            .expect("personal profile");
        assert_eq!(for_profile(&selection.path).expect("bound channel"), ReleaseChannel::FixtureA);
        assert_eq!(selection.channel.ap_hostname(), "ap.fixture-a.example.invalid");
        for other in [ReleaseChannel::Public, ReleaseChannel::Testing, ReleaseChannel::FixtureB] {
            assert!(
                resolve_selection(home, &executable, Some(&profile), Some(other), None).is_err()
            );
            assert!(Selection::open(home, Some(&profile), Some(other)).is_err());
        }
        let reopened = Selection::open(home, Some(&profile), None).expect("retained owner");
        assert_eq!(reopened.channel, ReleaseChannel::FixtureA);
        fs::write(profile.join("instance.json"), "retained identity").expect("profile data");
        fs::remove_file(profile.join(PROFILE_MARKER)).expect("remove inner owner marker");
        assert!(Selection::open(home, Some(&profile), None).is_err());
        assert_eq!(
            fs::read_to_string(profile.join("instance.json")).expect("preserved identity"),
            "retained identity"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn personal_profile_names_follow_macos_volume_case_rules() {
        let home = tempfile::tempdir().expect("temporary home");
        let home = home.path().canonicalize().expect("canonical home");
        let insensitive = case_insensitive(&File::open(&home).expect("temporary home"))
            .expect("volume case sensitivity");
        let namespace = home.join(NAME_NAMESPACE);
        let inside_state = namespace.join("uncreated/nested/profile");
        assert!(
            Selection::open(&home, Some(&inside_state), Some(ReleaseChannel::FixtureA)).is_err()
        );
        if insensitive {
            let inside_alias = home.join(".PLAYBIT-PROFILE-NAMES/uncreated/nested/profile");
            assert!(
                Selection::open(&home, Some(&inside_alias), Some(ReleaseChannel::FixtureA))
                    .is_err()
            );
        }
        assert!(!namespace.exists(), "rejected path created the name namespace");
        for name in [".PLAYBIT", ".PLAYBIT-TESTING", ".playbit-teſting", ".playbit-teﬆing"] {
            let path = home.join(name);
            let selection = Selection::open(&home, Some(&path), Some(ReleaseChannel::FixtureA));
            if insensitive {
                assert!(selection.is_err(), "reserved alias accepted: {name}");
                assert!(!path.exists());
            } else {
                assert_eq!(
                    selection.expect("distinct custom name").channel,
                    ReleaseChannel::FixtureA
                );
            }
        }
        let similar = home.join(".playbit-tésting");
        Selection::open(&home, Some(&similar), Some(ReleaseChannel::FixtureA))
            .expect("similar non-alias remains a valid personal profile");
        let public = Selection::open(&home, None, Some(ReleaseChannel::Public))
            .expect("ordinary Public profile can still start");
        assert_eq!(public.path, home.join(".playbit"));
        assert_eq!(for_profile(&public.path).expect("Public channel"), ReleaseChannel::Public);

        let path = home.join("PeRsOnAl");
        let original = Selection::open(&home, Some(&path), Some(ReleaseChannel::FixtureA))
            .expect("original personal profile");
        let alias = home.join("PERSONAL");
        fs::remove_dir_all(&original.path).expect("delete personal profile");
        let replacement = Selection::open(&home, Some(&alias), Some(ReleaseChannel::FixtureB));
        if insensitive {
            assert!(replacement.is_err());
            assert!(!path.exists());
            let reopened =
                Selection::open(&home, Some(&alias), None).expect("retained owner via alias");
            assert_eq!(reopened.channel, ReleaseChannel::FixtureA);
            assert_eq!(reopened.path, original.path, "one durable profile identity");
        } else {
            assert_eq!(
                replacement.expect("distinct custom name").channel,
                ReleaseChannel::FixtureB
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn case_aliases_cannot_take_over_an_active_ordinary_profile() {
        let home = tempfile::tempdir().expect("temporary home");
        let home = home.path().canonicalize().expect("canonical home");
        let profile = home.join("OrDiNaRy");
        let alias = home.join("ORDINARY");
        let state = custom_state_path(&profile).expect("legacy state path");
        private_directory(&state).expect("legacy state");
        // An older process holds this legacy lease. The first new selection
        // uses that identity even when the existing directory is opened by alias.
        let old = admit(&state, &profile, ReleaseChannel::Public, ReleaseChannel::Public)
            .expect("legacy Public selection");
        let same_volume_name = alias.is_dir();
        let requested = if same_volume_name { &alias } else { &profile };
        let public = Selection::open(&home, Some(requested), Some(ReleaseChannel::Public))
            .expect("join existing legacy lease");
        assert_eq!(public.path, profile);
        drop(old);
        fs::remove_dir_all(&profile).expect("delete active ordinary profile");
        assert!(Selection::open(&home, Some(&profile), Some(ReleaseChannel::FixtureA)).is_err());
        if same_volume_name {
            assert!(Selection::open(&home, Some(&alias), Some(ReleaseChannel::FixtureA)).is_err());
        }
        assert!(!profile.exists());
    }

    #[test]
    fn deleted_profile_is_recreated_while_same_channel_clients_retain_leases() {
        // Keep an existing client admitted, delete its profile, and admit a new
        // client without losing the explicit channel or accepting Public data.
        let home = tempfile::tempdir().expect("home");
        let first = Selection::open(home.path(), None, Some(ReleaseChannel::Testing))
            .expect("first selection");
        fs::remove_dir_all(&first.path).expect("delete profile");
        let second = Selection::open(home.path(), None, Some(ReleaseChannel::Testing))
            .expect("recreate selected profile");
        assert_eq!(first.path, second.path);
        assert_eq!(for_profile(&second.path).expect("channel"), ReleaseChannel::Testing);
        fs::remove_file(second.path.join(PROFILE_MARKER)).expect("remove marker");
        fs::write(second.path.join(crate::paths::PROFILE_DB), b"unmarked data")
            .expect("existing data");
        assert!(Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).is_err());
    }

    #[test]
    fn public_selection_is_ready_for_gui_logs_before_engine_startup() {
        use std::os::unix::fs::PermissionsExt;
        // Open GUI diagnostics before the engine, with fresh and existing default/explicit profiles.
        for (explicit, existing) in [(false, false), (true, false), (false, true), (true, true)] {
            let home = tempfile::tempdir().expect("home");
            let profile = home.path().join(if explicit { "custom" } else { ".playbit" });
            let mode = if existing { 0o755 } else { 0o700 };
            if existing {
                fs::create_dir(&profile).expect("existing profile");
                fs::set_permissions(&profile, fs::Permissions::from_mode(mode))
                    .expect("existing profile permissions");
            }
            let selected = Selection::open(
                home.path(),
                explicit.then_some(profile.as_path()),
                Some(ReleaseChannel::Public),
            )
            .expect("Public selection");
            let logs = crate::paths::private_directory(&selected.path, "logs")
                .expect("GUI log directory before engine startup");
            fs::write(logs.join("playbit-gui.log"), b"startup").expect("GUI startup log");
            assert_eq!(selected.channel, ReleaseChannel::Public);
            assert_eq!(fs::metadata(&selected.path).expect("profile").mode() & 0o777, mode);
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_launch_defaults_preserve_existing_profile_authority() {
        // Inspect disposable locations and retained markers/data; defaults must never reinterpret them.
        let home = tempfile::tempdir().expect("home");
        let home = home.path().canonicalize().expect("canonical home");
        let installed = home.join("Applications/Playbit.app");
        let downloaded = home.join("Downloads/Playbit.app");
        let public = (home.join(".playbit"), ReleaseChannel::Public);
        assert_eq!(launch_default(&home, &downloaded).expect("no testing profile"), public);
        let testing = home.join(".playbit-testing");
        private_directory(&testing).expect("fresh testing directory");
        assert_eq!(
            launch_default(&home, &downloaded).expect("fresh directory"),
            (testing.clone(), ReleaseChannel::Testing)
        );
        for installation in [&installed, Path::new("/Applications/Playbit.app")] {
            assert_eq!(launch_default(&home, installation).expect("installed default"), public);
        }
        let retained = testing.join(crate::paths::PROFILE_DB);
        fs::write(&retained, b"unmarked Public state").expect("existing state");
        assert_eq!(
            launch_default(&home, &downloaded).expect("existing Public state"),
            (testing.clone(), ReleaseChannel::Public)
        );
        assert!(Selection::open(&home, None, Some(ReleaseChannel::Testing)).is_err());
        assert_eq!(fs::read(&retained).expect("preserved state"), b"unmarked Public state");
        let selected = Selection::open(&home, None, None).expect("existing Public selection");
        assert_eq!(
            (selected.path.clone(), selected.channel),
            (testing.clone(), ReleaseChannel::Public)
        );
        drop(selected);
        for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
            fs::write(testing.join(PROFILE_MARKER), channel.name()).expect("retained channel");
            assert_eq!(
                launch_default(&home, &downloaded).expect("remembered profile"),
                (testing.clone(), channel)
            );
        }
        fs::write(testing.join(PROFILE_MARKER), b"invalid").expect("corrupt marker");
        assert!(launch_default(&home, &downloaded).is_err());
        assert_eq!(launch_default(&home, &installed).expect("unrelated installed profile"), public);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_default_selection_keeps_environment_overrides_and_switches_isolated() {
        // Resolve each override and remembered switch, then prove conflicting admission preserves state.
        let home = tempfile::tempdir().expect("home");
        let home = home.path().canonicalize().expect("canonical home");
        let executable = home.join("Downloads/Playbit.app/Contents/MacOS/Playbit");
        fs::create_dir_all(executable.parent().expect("parent")).expect("bundle");
        fs::write(&executable, b"fixture executable").expect("executable");
        let testing = home.join(".playbit-testing");
        private_directory(&testing).expect("testing profile");
        prepare(&testing, ReleaseChannel::Testing).expect("bind Testing");
        assert_eq!(
            resolve_selection(&home, &executable, None, None, None).expect("default"),
            (testing.clone(), ReleaseChannel::Testing)
        );
        for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
            let expected = (default_path(&home, channel), channel);
            assert_eq!(
                resolve_selection(&home, &executable, None, Some(channel), None)
                    .expect("channel override"),
                expected
            );
            assert_eq!(
                resolve_selection(&home, &executable, None, None, Some(channel))
                    .expect("remembered switch"),
                expected
            );
        }
        let custom = home.join("custom");
        assert_eq!(
            resolve_selection(
                &home,
                &executable,
                Some(&custom),
                None,
                Some(ReleaseChannel::Testing)
            )
            .expect("profile override"),
            (custom.clone(), ReleaseChannel::Public)
        );
        assert_eq!(
            resolve_selection(
                &home,
                &executable,
                Some(&custom),
                Some(ReleaseChannel::Testing),
                None
            )
            .expect("both overrides"),
            (custom, ReleaseChannel::Testing)
        );
        assert!(
            Selection::open(&home, Some(&home.join(".playbit")), Some(ReleaseChannel::Testing))
                .is_err()
        );
        assert!(Selection::open(&home, Some(&testing), Some(ReleaseChannel::Public)).is_err());
        assert_eq!(
            read(&testing.join(PROFILE_MARKER)).expect("preserved authority"),
            Some(ReleaseChannel::Testing)
        );
    }

    #[test]
    #[allow(
        unsafe_code,
        reason = "Only the child mask is changed; the child verifies its inherited mask."
    )]
    fn configured_relaunch_restores_signal_delivery_from_a_blocked_worker() {
        use std::os::unix::process::CommandExt;
        const CHILD: &str = "PLAYBIT_CHANNEL_SIGNAL_MASK_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut signals = unsafe { std::mem::zeroed() };
            // SAFETY: the query writes one owned mask and changes no mask.
            assert_eq!(unsafe { libc::pthread_sigmask(0, std::ptr::null(), &mut signals) }, 0);
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGCHLD, libc::SIGUSR1] {
                // SAFETY: signals is the initialized mask returned above.
                assert_eq!(unsafe { libc::sigismember(&signals, signal) }, 0);
            }
            return;
        }
        let home = tempfile::tempdir().expect("isolated home");
        let selected =
            Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).expect("selection");
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        child.args(["--exact", "engine::profile::tests::configured_relaunch_restores_signal_delivery_from_a_blocked_worker"])
            .env(CHILD, "1");
        // SAFETY: only async-signal-safe calls execute in the child before exec.
        unsafe {
            child.pre_exec(|| {
                let mut signals = std::mem::zeroed();
                libc::sigfillset(&mut signals);
                if libc::sigprocmask(libc::SIG_SETMASK, &signals, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        selected.configure_relaunch(&mut child).expect("relaunch configuration");
        assert!(child.status().expect("relaunch child").success());
    }

    #[test]
    fn short_admission_releases_while_an_inherited_descriptor_remains_open() {
        // try_clone shares the same file description as a fork-inherited descriptor.
        for fail in [false, true] {
            let home = tempfile::tempdir().expect("home");
            let path = home.path().join("admission.lock");
            let file = private_file(&path).expect("admission file");
            file.try_lock().expect("exclusive admission");
            let inherited = file.try_clone().expect("inherited descriptor");
            let peer = private_file(&path).expect("independent peer");
            assert!(matches!(peer.try_lock(), Err(TryLockError::WouldBlock)));
            let result: Result<()> = (|| {
                let _admission = Lease(file);
                if fail {
                    return Err("fixture selection failed".into());
                }
                Ok(())
            })();
            assert_eq!(result.is_err(), fail);
            peer.try_lock().expect("admission ends before the inherited descriptor closes");
            drop(peer);
            drop(inherited);
        }
    }

    #[test]
    fn selection_is_remembered_and_conflicting_processes_cannot_switch_it() {
        // Leases include helpers: selecting another channel requires retirement.
        let home = tempfile::tempdir().expect("home");
        let public = Selection::open(home.path(), None, None).expect("public");
        assert_eq!(public.channel, ReleaseChannel::Public);
        assert!(Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).is_err());
        let helper = Selection::open(home.path(), Some(&public.path), Some(ReleaseChannel::Public))
            .expect("same-channel helper");
        drop(public);
        assert!(Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).is_err());
        drop(helper);
        let testing =
            Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).expect("testing");
        assert_eq!(for_profile(&testing.path).expect("profile channel"), ReleaseChannel::Testing);
        assert_eq!(
            testing.path,
            home.path().join(".playbit-testing").canonicalize().expect("path")
        );
        drop(testing);
        let remembered = Selection::open(home.path(), None, None).expect("Finder launch");
        assert_eq!(remembered.channel, ReleaseChannel::Testing);
        let mut reopen = std::process::Command::new("fixture");
        remembered.configure_relaunch(&mut reopen).expect("normal relaunch");
        let environment: Vec<_> = reopen.get_envs().collect();
        assert!(environment.contains(&(std::ffi::OsStr::new("PLAYBIT_PROFILE"), None)));
        assert!(environment.contains(&(
            std::ffi::OsStr::new("PLAYBIT_CHANNEL"),
            Some(std::ffi::OsStr::new("testing"))
        )));
    }

    #[test]
    #[cfg(all(target_os = "macos", feature = "process"))]
    fn agent_joins_bound_guest_without_changing_gui_preference() {
        const CHILD: &str = "PLAYBIT_AGENT_PROFILE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            initialize_agent(None).expect("join admitted Testing guest");
            let home = dirs::home_dir().expect("fixture home");
            assert_eq!(current(), (home.join(".playbit-testing"), ReleaseChannel::Testing));
            return;
        }
        let home = tempfile::tempdir().expect("isolated home");
        let home = home.path().canonicalize().expect("canonical home");
        drop(Selection::open(&home, None, Some(ReleaseChannel::Public)).expect("Public GUI"));
        let testing = home.join(".playbit-testing");
        let guest = Selection::open(&home, Some(&testing), Some(ReleaseChannel::Testing))
            .expect("bound Testing guest");
        let executable = std::env::current_exe().expect("test executable");
        let child = std::process::Command::new(&executable)
            .args([
                "--exact",
                "engine::profile::tests::agent_joins_bound_guest_without_changing_gui_preference",
            ])
            .env(CHILD, "1")
            .env("HOME", &home)
            .env_remove("PLAYBIT_PROFILE")
            .env_remove("PLAYBIT_CHANNEL")
            .status()
            .expect("agent child");
        assert!(child.success());
        let preference = installation_preference(&home, &executable).expect("GUI preference");
        assert_eq!(
            read(&preference.join("selected")).expect("remembered channel"),
            Some(ReleaseChannel::Public)
        );
        drop(guest);
        let gui = Selection::open(&home, None, None).expect("next GUI launch");
        assert_eq!(gui.channel, ReleaseChannel::Public);
    }

    #[test]
    fn explicit_profiles_do_not_change_user_selection_or_reinterpret_existing_data() {
        // A fixture gets isolated state, and its data never gains new authority.
        let home = tempfile::tempdir().expect("home");
        let _public = Selection::open(home.path(), None, None).expect("public");
        let root = home.path().join("fixture");
        let isolated = Selection::open(home.path(), Some(&root), Some(ReleaseChannel::Testing))
            .expect("isolated testing");
        let mut reopen = std::process::Command::new("fixture");
        isolated.configure_relaunch(&mut reopen).expect("isolated relaunch");
        assert!(reopen.get_envs().any(|(key, value)| {
            key == "PLAYBIT_PROFILE" && value == Some(isolated.path.as_os_str())
        }));
        let preference = installation_preference(
            home.path(),
            &std::env::current_exe().expect("fixture executable"),
        )
        .expect("installation");
        assert_eq!(
            read(&preference.join("selected")).expect("selection"),
            Some(ReleaseChannel::Public)
        );
        fs::write(isolated.path.join("retained"), b"unsynchronized").expect("data");
        drop(isolated);
        assert!(Selection::open(home.path(), Some(&root), Some(ReleaseChannel::Public)).is_err());
        assert_eq!(fs::read(root.join("retained")).expect("retained data"), b"unsynchronized");
        let root = home.path().join("old-public");
        private_directory(&root).expect("old profile");
        fs::write(root.join(crate::paths::PROFILE_DB), b"retained database").expect("data");
        assert!(Selection::open(home.path(), Some(&root), Some(ReleaseChannel::Testing)).is_err());
        assert_eq!(for_profile(&root).expect("ordinary profile"), ReleaseChannel::Public);
    }

    #[test]
    fn symlinks_share_ownership_and_malformed_selection_fails_explicitly() {
        // Aliases must not admit a second environment, and corrupt state is visible.
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join("fixture");
        let first = Selection::open(home.path(), Some(&root), Some(ReleaseChannel::Testing))
            .expect("testing");
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).expect("alias");
        assert!(Selection::open(home.path(), Some(&alias), Some(ReleaseChannel::Public)).is_err());
        let second = Selection::open(home.path(), Some(&alias), None).expect("same profile");
        assert_eq!(first.path, second.path);
        assert_eq!(
            bound_channel(&alias, None).expect("descriptor authority"),
            ReleaseChannel::Testing
        );
        assert!(bound_channel(&alias, Some(ReleaseChannel::Public)).is_err());
        assert_eq!(
            bound_channel(&alias, Some(ReleaseChannel::Testing)).expect("matching override"),
            ReleaseChannel::Testing
        );
        drop(first);
        drop(second);
        fs::write(root.join(PROFILE_MARKER), b"invalid channel").expect("corrupt marker");
        assert!(Selection::open(home.path(), Some(&root), None).is_err());
    }

    #[test]
    fn installation_preferences_use_the_outer_bundle_and_survive_helper_launches() {
        // Two downloaded copies remember their own choice; bundled helpers use
        // the same identity without discovering a machine address or file name.
        let home = tempfile::tempdir().expect("home");
        let app = home.path().join("first/Playbit.app");
        let gui = app.join("Contents/MacOS/Playbit");
        let helper = app.join("Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine");
        let other = home.path().join("second/Playbit.app/Contents/MacOS/Playbit");
        for path in [&gui, &helper, &other] {
            fs::create_dir_all(path.parent().expect("parent")).expect("bundle");
            fs::write(path, b"fixture executable").expect("executable");
        }
        assert_eq!(
            installation_preference(home.path(), &gui).expect("GUI"),
            installation_preference(home.path(), &helper).expect("helper")
        );
        assert_ne!(
            installation_preference(home.path(), &gui).expect("GUI"),
            installation_preference(home.path(), &other).expect("other installation")
        );
    }

    #[test]
    fn relocated_default_profile_retains_shared_process_ownership() {
        // A helper uses the same global admission even when ~/.playbit is a link.
        let home = tempfile::tempdir().expect("home");
        let actual = home.path().join("actual");
        private_directory(&actual).expect("profile");
        std::os::unix::fs::symlink(&actual, home.path().join(".playbit")).expect("link");
        let public = Selection::open(home.path(), None, None).expect("GUI");
        let helper = Selection::open(home.path(), Some(&public.path), Some(ReleaseChannel::Public))
            .expect("helper");
        // A concurrent fork retains this file description until exec.
        let inherited =
            helper.active.as_ref().expect("helper lease").0.try_clone().expect("inherited lease");
        drop(public);
        assert!(Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).is_err());
        drop(helper);
        assert!(Selection::open(home.path(), None, Some(ReleaseChannel::Testing)).is_ok());
        drop(inherited);
    }

    #[test]
    #[cfg(feature = "app-host")]
    fn testing_profiles_report_their_authorities_before_any_connection() {
        // Ordinary testing accounts do not need an integration key.
        let home = tempfile::tempdir().expect("home");
        for channel in [ReleaseChannel::Testing, ReleaseChannel::FixtureA] {
            let profile = home.path().join(channel.name());
            let selected = Selection::open(home.path(), Some(&profile), Some(channel))
                .expect("testing profile");
            let mut engine = crate::engine::Engine::open(&selected.path).expect("ordinary engine");
            let status = engine.status_snapshot().expect("local status");
            assert_eq!(status.ap_connection.hostname, channel.ap_hostname());
            assert_eq!(
                status.ap_connection.state,
                crate::engine::reply::ApConnectionState::Offline
            );
            assert!(!status.enrolled);
        }
    }

    #[test]
    #[ignore = "reference hardware: 1000 warm channel admissions per environment"]
    fn benchmark_runtime_channel_admission() -> Result<()> {
        // Includes installation/profile resolution, bounded records and kernel
        // leases. This is startup software work, not GUI input acceptance.
        if cfg!(debug_assertions) {
            return Err("Measure an optimized build".into());
        }
        let home = tempfile::tempdir().expect("isolated home");
        for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
            let active = Selection::open(home.path(), None, Some(channel)).expect("active");
            let mut samples = Vec::with_capacity(1000);
            for index in 0..1100 {
                let start = std::time::Instant::now();
                let selected =
                    Selection::open(home.path(), None, None).expect("remembered default launch");
                assert_eq!(selected.channel, channel);
                drop(selected);
                if index >= 100 {
                    samples.push(start.elapsed().as_nanos() as u64);
                }
            }
            let raw = samples.clone();
            samples.sort_unstable();
            println!(
                "{}",
                serde_json::json!({"benchmark":"runtime_channel_admission",
                    "channel":channel.name(), "optimized":true, "count":samples.len(),
                    "p50_us":samples[499] as f64/1000.,
                    "p95_us":samples[949] as f64/1000.,
                    "p99_us":samples[989] as f64/1000.,
                    "max_us":samples[999] as f64/1000., "samples_ns":raw,
                    "boundary":"warm remembered/location-default selection including profile validation and process lease; excludes cold creation, GUI, OS dispatch and network"})
            );
            assert!(samples[989] < 1_000_000, "channel bookkeeping exceeds 1 ms p99");
            drop(active);
        }
        Ok(())
    }
}
