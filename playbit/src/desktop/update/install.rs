//! One detached installer. The journal describes bundle replacement, never data rollback.
use super::{copy, package};
use crate::engine::{
    Result,
    installation::{self, Installation, ReleaseIdentity},
    launch_agent::{ServiceAction, control},
};
use crate::release_channel::ReleaseChannel;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub channel: ReleaseChannel,
    pub from: ReleaseIdentity,
    pub to: ReleaseIdentity,
    pub phase: Phase,
    /// A recorded random name authorizes only this transaction's temporary
    /// sibling directory. Absence uses profile staging directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjacent: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Ready,
    Swapping,
    Installed,
}
impl Journal {
    pub fn read(installation: &Installation, channel: ReleaseChannel) -> Result<Option<Self>> {
        let path = installation.state.join("journal.json");
        let bytes = match installation::read(&path, 1024) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let journal: Self = serde_json::from_slice(&bytes)?;
        if let Some(nonce) = &journal.adjacent {
            let _: [u8; 16] = crate::identifier::decode(nonce)
                .map_err(|_| "Invalid temporary update directory identity")?;
        }
        journal.from.validate()?;
        journal.to.validate()?;
        if (journal.channel != channel && journal.phase != Phase::Installed)
            || !package::newer(&journal.to, &journal.from)?
        {
            return Err("Invalid update journal".into());
        }
        Ok(Some(journal))
    }
    pub fn write(&self, installation: &Installation) -> Result<()> {
        installation.write("journal.json", &serde_json::to_vec(self)?)
    }

    fn staging_directory(&self, installation: &Installation) -> Result<PathBuf> {
        let Some(nonce) = &self.adjacent else {
            return Ok(installation.state.clone());
        };
        let name = installation.app.file_name().ok_or("Missing installation name")?;
        let parent = installation.app.parent().ok_or("Missing installation parent")?;
        Ok(parent.join(format!(".{}.install-{nonce}", name.to_string_lossy())))
    }
}

pub fn arm(
    installation: &Installation,
    release: &ReleaseIdentity,
    channel: ReleaseChannel,
) -> Result<()> {
    let from = installation.release()?;
    if !package::newer(release, &from)? {
        return Err("The prepared update is not newer than this installation".into());
    }
    Journal { channel, from, to: release.clone(), phase: Phase::Ready, adjacent: None }
        .write(installation)
}

/// Parent keeps its client lease until this child has acknowledged ownership.
/// A duplicate child acknowledges the existing waiter and exits.
pub fn handoff(installation: &Installation, reopen: bool) -> Result<()> {
    let mut command = Command::new(installation.app.join("Contents/MacOS/Playbit"));
    command
        .arg("--apply-update")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    crate::engine::profile::configure_relaunch(&mut command)?;
    let mut child = command.spawn()?;
    let mut ready = child.stdout.take().ok_or("Updater readiness pipe is missing")?;
    wait_readable(&ready, Duration::from_secs(5))?;
    let mut message = [0; 5];
    ready.read_exact(&mut message)?;
    if message != *b"READY" {
        return Err("Updater could not accept the installation".into());
    }
    if reopen {
        let flag = installation.file("reopen")?;
        flag.sync_all()?;
        File::open(&installation.state)?.sync_all()?;
    }
    Ok(())
}

pub fn requested() -> bool {
    std::env::args_os().nth(1).is_some_and(|arg| arg == "--apply-update")
}

/// After readiness the installer owns user feedback; earlier failures belong to
/// the waiting GUI, which must not race a second error dialog from this child.
pub fn run(admitted: &mut bool) -> Result<()> {
    if std::env::args_os().count() != 2 {
        return Err("Invalid updater invocation".into());
    }
    let installation = Installation::for_executable(&std::env::current_exe()?)?
        .ok_or(installation::LOCATION_ERROR)?;
    installation.open()?;
    let installer = installation.try_installer()?;
    std::io::stdout().write_all(b"READY")?;
    std::io::stdout().flush()?;
    if installer.is_none() {
        return Ok(());
    }
    *admitted = true;
    let result = replace(&installation);
    if let Err(error) = &result {
        let message = format!("Playbit could not install the update: {error}");
        let reopen = installation.state.join("reopen");
        if reopen.exists() {
            fs::remove_file(reopen)?;
        }
        installation
            .write("error.txt", message.chars().take(2048).collect::<String>().as_bytes())?;
        return result;
    }
    let reopen = installation.state.join("reopen");
    if reopen.exists() {
        fs::remove_file(&reopen)?;
        let status = Command::new("/usr/bin/open").arg(&installation.app).status()?;
        if !status.success() {
            installation.write(
                "error.txt",
                b"The update was installed, but macOS could not reopen Playbit.",
            )?;
            return Err("macOS could not reopen Playbit".into());
        }
    }
    Ok(())
}

/// Recovery always completes a successful exchange; it never launches older
/// code against data that the new engine might already have opened.
pub fn recover(installation: &Installation, journal: &mut Journal) -> Result<()> {
    let current = installation.release()?;
    if current == journal.from && journal.phase != Phase::Installed {
        return Ok(());
    }
    if current != journal.to || journal.phase == Phase::Ready {
        return Err("Installation does not match the update journal".into());
    }
    if journal.phase == Phase::Swapping {
        package::verify(&installation.app, &journal.to)?;
        let staged = journal.staging_directory(installation)?.join("staged.app");
        let previous = installation.state.join("previous.app");
        if staged.exists() {
            package::verify(&staged, &journal.from)?;
            if journal.adjacent.is_some() {
                retain_previous(&staged, &previous, &journal.from)?;
            } else {
                if previous.exists() {
                    return Err("Ambiguous previous update bundle".into());
                }
                fs::rename(staged, &previous)?;
            }
        } else if !previous.exists() {
            return Err("The previous update bundle is missing".into());
        }
        package::verify(&previous, &journal.from)?;
        File::open(&installation.state)?.sync_all()?;
        journal.phase = Phase::Installed;
        journal.write(installation)?;
    }
    if journal.phase == Phase::Installed && journal.adjacent.is_some() {
        let temporary = journal.staging_directory(installation)?;
        if temporary.try_exists()? {
            installation::private_directory(&temporary)?;
            fs::remove_dir_all(&temporary)?;
            File::open(temporary.parent().ok_or("Missing temporary update parent")?)?.sync_all()?;
        }
        let staged = installation.state.join("staged.app");
        if staged.try_exists()? {
            package::verify(&staged, &journal.to)?;
            fs::remove_dir_all(staged)?;
        }
        journal.adjacent = None;
        journal.write(installation)?;
    }
    Ok(())
}

fn retain_previous(source: &Path, previous: &Path, identity: &ReleaseIdentity) -> Result<()> {
    if previous.try_exists()? {
        return package::verify(previous, identity);
    }
    let incoming = previous.with_extension("incoming");
    if incoming.try_exists()? {
        installation::private_directory(&incoming)?;
        fs::remove_dir_all(&incoming)?;
    }
    copy::bundle(source, &incoming)?;
    package::verify(&incoming, identity)?;
    fs::rename(&incoming, previous)?;
    File::open(previous.parent().ok_or("Missing previous update parent")?)?.sync_all()?;
    Ok(())
}

#[allow(
    unsafe_code,
    reason = "renameatx_np exchanges two owned directories atomically on one filesystem."
)]
pub(super) fn swap(first: &Path, second: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let first = std::ffi::CString::new(first.as_os_str().as_bytes())?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())?;
    // SAFETY: terminated paths remain live throughout this synchronous syscall.
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[allow(unsafe_code, reason = "poll borrows one live readiness descriptor for a bounded wait.")]
fn wait_readable(fd: &impl std::os::fd::AsRawFd, timeout: Duration) -> Result<()> {
    let mut event = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: initialized event and one borrowed descriptor; no ownership transfer.
    if unsafe { libc::poll(&mut event, 1, timeout.as_millis() as i32) } <= 0 {
        return Err("Updater did not acknowledge the quit request".into());
    }
    Ok(())
}

fn replace(installation: &Installation) -> Result<()> {
    let interrupt = installation.interrupt_runs()?;
    let _exclusive = installation.exclusive()?;
    drop(interrupt);
    let _operation = installation.operation()?;
    for profile in installation.profiles()? {
        let channel = crate::engine::profile::for_profile(&profile)?;
        if crate::engine::profile::is_default_profile(&profile, channel)?
            && crate::engine::launch_agent::owner(&profile)?
                .is_some_and(|owner| owner != installation.app)
        {
            continue;
        }
        crate::engine::launch::retire_profile_engine(&profile, &installation.engine(), channel)?;
    }
    let _engines = installation.drained_engines()?;
    let Some(mut journal) = Journal::read(installation, crate::engine::profile::current().1)?
    else {
        return Ok(());
    };
    recover(installation, &mut journal)?;
    if journal.phase == Phase::Installed {
        return refresh_guest_apps(installation);
    }
    installation.ensure_installable()?;
    if installation.release()? != journal.from {
        return Err("The installed version changed while waiting to update".into());
    }
    // ServiceManagement retains the executable identity across launches. Retire
    // the stopped job before replacing its signed bundle; the next client
    // registers the new helper after cutover, while normal admission is held.
    for channel in [ReleaseChannel::Public, ReleaseChannel::Testing] {
        control(ServiceAction::RetireOwned, channel)?;
    }
    let previous = installation.state.join("previous.app");
    if previous.exists() {
        fs::remove_dir_all(&previous)?;
    }
    let staged = prepare_exchange(installation, &mut journal)?;
    journal.phase = Phase::Swapping;
    journal.write(installation)?;
    swap(&installation.app, &staged)?;
    File::open(installation.app.parent().ok_or("Missing installation parent")?)?.sync_all()?;
    File::open(&installation.state)?.sync_all()?;
    File::open(staged.parent().ok_or("Missing update staging parent")?)?.sync_all()?;
    recover(installation, &mut journal)?;
    refresh_guest_apps(installation)
}

fn prepare_exchange(installation: &Installation, journal: &mut Journal) -> Result<PathBuf> {
    let staged = installation.state.join("staged.app");
    package::verify(&staged, &journal.to)?;
    if journal.adjacent.is_none()
        && fs::metadata(&installation.state)?.dev()
            == fs::metadata(installation.app.parent().ok_or("Missing installation parent")?)?.dev()
    {
        return Ok(staged);
    }
    if journal.adjacent.is_none() {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| error.to_string())?;
        journal.adjacent = Some(crate::identifier::encode(&nonce));
        let temporary = journal.staging_directory(installation)?;
        if temporary.try_exists()? {
            return Err("Temporary update directory already exists".into());
        }
        // Persist the name before creating any adjacent artifact, so every
        // interruption has one bounded, explicitly owned recovery location.
        journal.write(installation)?;
    }
    let temporary = journal.staging_directory(installation)?;
    installation::private_directory(&temporary)?;
    let incoming = temporary.join("staged.app");
    if incoming.try_exists()? {
        // With the source identity still installed, this can only be an
        // interrupted copy of the replacement, never the exchanged old app.
        installation::private_directory(&incoming)?;
        fs::remove_dir_all(&incoming)?;
    }
    copy::bundle(&staged, &incoming)?;
    package::verify(&incoming, &journal.to)?;
    File::open(&temporary)?.sync_all()?;
    File::open(temporary.parent().ok_or("Missing temporary update parent")?)?.sync_all()?;
    Ok(incoming)
}

/// The new runtime signs wrappers for its own CDHash while cutover still owns
/// the exclusive leases. No engine or client may enter during this operation.
fn refresh_guest_apps(installation: &Installation) -> Result<()> {
    for profile in installation.profiles()? {
        if !profile.exists() {
            continue;
        }
        let status = Command::new(installation.app.join("Contents/MacOS/Playbit"))
            .arg("--refresh-guest-apps")
            .arg(&profile)
            .stdin(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(format!(
                "The update is installed, but its app wrappers could not be re-signed for {}. \
                 Reopen Playbit to refresh them",
                profile.display()
            )
            .into());
        }
    }
    Ok(())
}
