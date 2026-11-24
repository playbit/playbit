//! macOS guest launch routing. Identity follows publishing account and app_id; ownership
//! follows the engine registry's exact session instance and verified process.
use crate::{
    engine::{
        Request, Result, ipc,
        reply::Reply,
        session_instance::{SessionInstance, State},
    },
    package::Package,
    store::{Id, Session},
};
use std::{
    fs,
    io::Read,
    num::NonZeroU64,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[path = "guest_icon.rs"]
pub(crate) mod icon;
#[path = "guest_launch.rs"]
mod native_launch;

pub(super) fn main(focus_window: bool) -> Result<bool> {
    crate::trace::mark("guest.launch.main");
    let executable = std::env::current_exe()?.canonicalize()?;
    let descriptor = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("Missing app directory")?
        .join("Resources/guest-launch");
    let args = native_launch::arguments()?;
    if !descriptor.exists() && args.is_empty() {
        return Ok(false);
    }
    let native_launch::Intent { instance, focus_window, fuel_limit } =
        native_launch::intent(&args, focus_window)?;
    let installation = read_installation(&executable)?;
    let profile = PathBuf::from(&installation.profile);
    crate::engine::profile::initialize_at(&profile)?;
    super::process_log::initialize(&profile)?;
    let engine = PathBuf::from(&installation.engine);
    let _installation_lease = crate::engine::installation::client_lease(&engine)?;
    let _profile_client = clients_gate(&profile)?;
    _profile_client.lock_shared()?;
    if super::guest_runtime::loaded_path()? != Path::new(&installation.runtime)
        || super::guest_runtime::runtime_for_engine(&engine)? != Path::new(&installation.runtime)
        || super::guest_runtime::loaded_cdhash()? != installation.runtime_hash
    {
        return Err("The guest runtime and engine installation differ. Reopen this app from Playbit to refresh its wrapper".into());
    }
    let profile_lock = profile_gate(&profile)?;
    profile_lock.lock_shared()?;
    let binding = RuntimeBinding::read(&profile)?.ok_or("Missing profile runtime binding")?;
    if binding != RuntimeBinding::from(&installation) {
        return Err("This wrapper belongs to another runtime installation. Reopen it from Playbit to refresh it".into());
    }
    crate::ui::desktop::native::run_guest(
        profile.clone(),
        engine,
        focus_window,
        fuel_limit,
        move || {
            let _profile_lock = profile_lock;
            let result = (|| -> Result<Option<SessionInstance>> {
                // The native worker has started/admitted the matching engine before
                // resolving app-level recency. Finder opens also work with no engine.
                let (selected, binding) = select_session(&profile, &installation.app, instance)?;
                crate::platform::failure::install_gui(&profile, selected.root_volume);
                if let Some(binding) = binding {
                    return Ok(Some(binding));
                }
                match reserve(&profile, &selected.id, false, focus_window, vec![], true, None)? {
                    Routing::Reserved(reservation) => Ok(Some(reservation.instance)),
                    Routing::Existing(_) => Ok(None),
                }
            })();
            result.map_err(|error| error.to_string())
        },
    )?;
    Ok(true)
}

fn select_session(
    profile: &Path,
    app: &crate::package::AppIdentity,
    instance: Option<Id>,
) -> Result<(Session, Option<SessionInstance>)> {
    let Reply::Status(status) = reply(profile, &Request::Status)? else {
        return Err("Unexpected application session catalog".into());
    };
    let mut candidates: Vec<_> =
        status.sessions.into_iter().filter(|session| &session.app == app).collect();
    candidates.sort_by_key(|session| {
        status.recent_sessions.iter().position(|id| id == &session.id).unwrap_or(usize::MAX)
    });
    let fallback = candidates.first().cloned();
    for session in candidates {
        let Reply::InstanceOwners(owners) =
            reply(profile, &Request::InstanceList { session: session.id.clone() })?
        else {
            return Err("Unexpected application instance catalog".into());
        };
        if let Some(id) = instance {
            if let Some(owner) = owners.into_iter().find(|owner| owner.instance.id == id) {
                if owner.instance.root != session.root_volume {
                    return Err("The instance belongs to another application root".into());
                }
                return Ok((session, Some(owner.instance)));
            }
        } else if owners.iter().any(|owner| {
            matches!(owner.instance.state, State::Owned | State::Unknown) && owner.process.is_some()
        }) {
            return Ok((session, None));
        }
    }
    if instance.is_some() {
        return Err("The requested instance does not belong to this application".into());
    }
    fallback.map(|session| (session, None)).ok_or_else(|| {
        "This app has no retained sessions. Open or join a session in Playbit first".into()
    })
}

const MAX_DESCRIPTOR: usize = 16 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Installation {
    pub profile: String,
    pub engine: String,
    pub runtime: String,
    pub runtime_hash: [u8; 20],
    pub app: crate::package::AppIdentity,
    pub build: String,
    pub icon: String,
}

fn reply(profile: &Path, request: &Request) -> Result<Reply> {
    ipc::call_archive(profile, request)?.into_owned()
}

fn session(profile: &Path, id: &str) -> Result<Session> {
    let Reply::Status(status) = reply(profile, &Request::Status)? else {
        return Err("Unexpected retained-session reply".into());
    };
    status
        .sessions
        .into_iter()
        .find(|session| session.id == id)
        .ok_or_else(|| "The session is not retained on this device".into())
}

/// Ordinary opens select a verified live owner. Reservations and unknown
/// dispatches never expire and cannot be converted into automatic new launches.
struct Reservation {
    instance: SessionInstance,
    incarnation: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LaunchReceipt {
    pub incarnation: Id,
    pub instance: Id,
    pub owner_generation: u64,
    pub presentation: u64,
}
enum Routing {
    Reserved(Reservation),
    Existing(LaunchReceipt),
}

pub struct LaunchOptions {
    pub fresh: bool,
    pub focus_window: bool,
    pub host_mounts: Vec<crate::engine::host_mount::HostMount>,
    pub restore_host_mounts: bool,
    pub fuel_limit: Option<NonZeroU64>,
    pub extra_volumes: Option<Vec<crate::store::PersonalVolumeGrant>>,
}

fn reserve(
    profile: &Path,
    session: &str,
    fresh: bool,
    focus_window: bool,
    host_mounts: Vec<crate::engine::host_mount::HostMount>,
    restore_host_mounts: bool,
    selected_volumes: Option<Vec<crate::store::PersonalVolumeGrant>>,
) -> Result<Routing> {
    crate::engine::host_mount::validate(&host_mounts)?;
    if !fresh && !host_mounts.is_empty() {
        return Err("Host mounts require a fresh session instance".into());
    }
    if !fresh && selected_volumes.is_some() {
        return Err("Explicit volume mounts require a fresh session instance".into());
    }
    let Reply::InstanceStatus { incarnation, .. } =
        reply(profile, &Request::InstanceInspect { request: None })?
    else {
        return Err("Unexpected engine launch-incarnation reply".into());
    };
    if !fresh {
        let Reply::InstanceOwners(owners) =
            reply(profile, &Request::InstanceList { session: session.into() })?
        else {
            return Err("Unexpected session-instance routing reply".into());
        };
        if let Some(owner) = owners.iter().rev().find(|owner| {
            matches!(owner.instance.state, State::Owned | State::Unknown) && owner.process.is_some()
        }) {
            let presentation = if focus_window {
                activate(profile, incarnation, owner)?
            } else {
                owner.presentation
            };
            return Ok(Routing::Existing(LaunchReceipt {
                incarnation,
                instance: owner.instance.id,
                owner_generation: owner.instance.owner_generation,
                presentation,
            }));
        }
        if !owners.is_empty() {
            return Err("This app has an unconfirmed launch or owner. Inspect the recorded session instance before starting another one.".into());
        }
    }
    let request = Id::generate()?;
    let retained = self::session(profile, session)?;
    let grants = if let Some(grants) = selected_volumes {
        grants
    } else {
        let Reply::PersonalVolumes { grants, .. } =
            reply(profile, &Request::PersonalVolumes { session: retained.id })?
        else {
            return Err("Unexpected selected-volume reply".into());
        };
        grants.unwrap_or_default()
    };
    let extra_volumes: Box<[_]> = grants
        .into_iter()
        .filter(|grant| {
            grant.volume != retained.root_volume && Some(grant.volume) != retained.shared_volume
        })
        .collect();
    let saved = if !fresh && restore_host_mounts {
        let Reply::InstanceConfigurations(configurations) =
            reply(profile, &Request::InstanceConfigurations { session: session.into() })?
        else {
            return Err("Unexpected saved-instance reply".into());
        };
        configurations.last().copied()
    } else {
        None
    };
    let reserve_request = if let Some(instance) = saved {
        Request::InstanceRestore { request, incarnation, instance }
    } else {
        Request::InstanceReserve {
            restore_host_mounts,
            request,
            incarnation,
            session: session.into(),
            extra_volumes,
            host_mounts: host_mounts.into_boxed_slice(),
        }
    };
    let response = reply(profile, &reserve_request);
    // Only inspect a lost reserve reply. No process dispatch has happened yet.
    let instance = match response {
        Ok(Reply::Instance(instance)) => instance,
        Ok(_) => return Err("Unexpected launch-reservation reply".into()),
        Err(error) => {
            let inspected = reply(profile, &Request::InstanceInspect { request: Some(request) });
            match inspected {
                Ok(Reply::InstanceStatus { incarnation: current, instance: Some(instance) })
                    if current == incarnation =>
                {
                    instance
                }
                _ => {
                    return Err(
                        format!("Launch reservation {request} needs inspection: {error}").into()
                    );
                }
            }
        }
    };
    if instance.launch_request != request
        || instance.session != session
        || instance.state != State::Reserved
    {
        return Err("The launch reservation has another binding or has already dispatched".into());
    }
    Ok(Routing::Reserved(Reservation { instance, incarnation }))
}

/// Called on a management worker with the matched packaged GUI/engine artifacts.
fn prepare_launch(
    profile: &Path,
    session: &Session,
    options: LaunchOptions,
) -> Result<PendingLaunch> {
    let LaunchOptions {
        fresh,
        focus_window,
        host_mounts,
        restore_host_mounts,
        fuel_limit,
        extra_volumes,
    } = options;
    let _timing = crate::trace::Span::new("guest.launch");
    if fuel_limit.is_some() && !fresh {
        return Err("A positive fuel limit requires a fresh app instance".into());
    }
    let profile = profile.canonicalize()?;
    // Confirm engine admission before doing wrapper filesystem/signing work.
    let Reply::InstanceStatus { .. } =
        reply(&profile, &Request::InstanceInspect { request: None })?
    else {
        return Err("Unexpected engine launch-incarnation reply".into());
    };
    let (bundle, _profile_lock) = prepare_bundle(&profile, session)?;
    let Reservation { instance, incarnation } = match reserve(
        &profile,
        &session.id,
        fresh,
        focus_window,
        host_mounts,
        restore_host_mounts,
        extra_volumes,
    )? {
        Routing::Reserved(reservation) => reservation,
        Routing::Existing(instance) => {
            return Ok(PendingLaunch {
                receipt: instance,
                profile,
                session: session.id.clone(),
                completion: None,
            });
        }
    };
    if instance.root != session.root_volume || instance.build != session.resolved_build_id {
        let _ =
            reply(&profile, &Request::InstanceLaunchFailed { instance: instance.id, incarnation });
        return Err(
            "The selected session changed before launch. Open its committed version explicitly."
                .into(),
        );
    }
    match native_launch::submit(&bundle, instance.id, focus_window, fuel_limit) {
        Ok(completion) => Ok(PendingLaunch {
            receipt: LaunchReceipt {
                presentation: 0,
                incarnation,
                instance: instance.id,
                owner_generation: instance
                    .owner_generation
                    .checked_add(1)
                    .expect("reserved instance can be claimed"),
            },
            profile,
            session: session.id.clone(),
            completion: Some(completion),
        }),
        Err(error) => {
            let _ = reply(
                &profile,
                &Request::InstanceLaunchFailed { instance: instance.id, incarnation },
            );
            Err(error.into())
        }
    }
}
struct PendingLaunch {
    receipt: LaunchReceipt,
    profile: PathBuf,
    session: String,
    completion: Option<native_launch::Completion>,
}
impl PendingLaunch {
    fn delivered(&self) -> bool {
        if !matches!(reply(&self.profile, &Request::InstanceInspect { request: None }), Ok(Reply::InstanceStatus { incarnation, .. }) if incarnation == self.receipt.incarnation)
        {
            return false;
        }
        matches!(reply(&self.profile, &Request::InstanceList { session: self.session.clone() }), Ok(Reply::InstanceOwners(owners)) if owners.iter().any(|owner| owner.instance.id == self.receipt.instance && owner.instance.owner_generation == self.receipt.owner_generation && owner.instance.state == State::Owned && owner.process.is_some()))
    }

    fn wait(mut self) -> Result<LaunchReceipt> {
        let Some(completion) = self.completion.take() else {
            return Ok(self.receipt);
        };
        let outcome = completion.wait();
        if !matches!(outcome, native_launch::Outcome::Delivered) && self.delivered() {
            return Ok(self.receipt);
        }
        match outcome {
            native_launch::Outcome::Delivered => Ok(self.receipt),
            native_launch::Outcome::Unconfirmed => Err(format!(
                "Launch Services did not confirm instance {}; inspect it before another launch",
                self.receipt.instance
            )
            .into()),
            native_launch::Outcome::Rejected(error) => {
                let _ = reply(
                    &self.profile,
                    &Request::InstanceLaunchFailed {
                        instance: self.receipt.instance,
                        incarnation: self.receipt.incarnation,
                    },
                );
                Err(error.into())
            }
        }
    }
}

/// CLI callers retain confirmed handoff and error reporting.
pub fn launch(
    profile: &Path,
    session: &Session,
    fresh: bool,
    focus_window: bool,
    host_mounts: Vec<crate::engine::host_mount::HostMount>,
    restore_host_mounts: bool,
    fuel_limit: Option<NonZeroU64>,
) -> Result<Id> {
    launch_with_options(
        profile,
        session,
        LaunchOptions {
            fresh,
            focus_window,
            host_mounts,
            restore_host_mounts,
            fuel_limit,
            extra_volumes: None,
        },
    )
}

pub fn launch_with_options(
    profile: &Path,
    session: &Session,
    options: LaunchOptions,
) -> Result<Id> {
    crate::diagnostic!("Starting app…");
    prepare_launch(profile, session, options)?.wait().map(|receipt| receipt.instance)
}

pub(super) fn launch_confirmed(
    profile: &Path,
    session: &Session,
    fresh: bool,
    focus_window: bool,
    host_mounts: Vec<crate::engine::host_mount::HostMount>,
    restore_host_mounts: bool,
) -> Result<LaunchReceipt> {
    prepare_launch(
        profile,
        session,
        LaunchOptions {
            fresh,
            focus_window,
            host_mounts,
            restore_host_mounts,
            fuel_limit: None,
            extra_volumes: None,
        },
    )?
    .wait()
}

pub(super) type LaunchCompletion =
    Box<dyn FnOnce(LaunchReceipt, std::result::Result<(), String>) + Send>;
static PENDING_LAUNCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct PendingCredit;
impl Drop for PendingCredit {
    fn drop(&mut self) {
        PENDING_LAUNCHES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
/// One bounded callback wait per submitted desktop launch. Neither AppKit nor
/// the management worker waits; the permit is retained until the deadline.
pub(super) fn launch_async(
    profile: &Path,
    session: &Session,
    fresh: bool,
    focus_window: bool,
    host_mounts: Vec<crate::engine::host_mount::HostMount>,
    restore_host_mounts: bool,
    completion: LaunchCompletion,
) -> Result<LaunchReceipt> {
    use std::sync::atomic::Ordering;
    PENDING_LAUNCHES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < crate::engine::session_instance::MAX_PROCESSES).then_some(count + 1)
        })
        .map_err(|_| "Too many apps are waiting to launch")?;
    let credit = PendingCredit;
    let pending = prepare_launch(
        profile,
        session,
        LaunchOptions {
            fresh,
            focus_window,
            host_mounts,
            restore_host_mounts,
            fuel_limit: None,
            extra_volumes: None,
        },
    )?;
    let receipt = pending.receipt;
    std::thread::Builder::new().name("guest-launch-completion".into()).stack_size(256 * 1024).spawn(move || {
        let _credit = credit;
        completion(receipt, pending.wait().map(|_| ()).map_err(|error| error.to_string()));
    }).map_err(|error| format!("Launch was submitted but its completion could not be observed: {error}; inspect instance {} before another launch", receipt.instance))?;
    Ok(receipt)
}

fn prepare_bundle(profile: &Path, session: &Session) -> Result<(PathBuf, fs::File)> {
    let _timing = crate::trace::Span::new("guest.launch.prepare_bundle");
    session.app.validate()?;
    if !crate::identifier::valid(&session.id, 32)
        || !crate::identifier::valid(&session.resolved_build_id, 32)
    {
        return Err("Invalid guest launch identity".into());
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    let contents = executable
        .parent()
        .and_then(Path::parent)
        .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        .ok_or("Guest launch requires a packaged Playbit installation")?;
    let template = contents.join("Resources/guest-launcher");
    let engine = crate::engine::launch::executable_for(&executable)?.canonicalize()?;
    let runtime = super::guest_runtime::runtime_for_engine(&engine)?;
    let descriptor = Installation {
        profile: profile.to_str().ok_or("Profile path is not UTF-8")?.into(),
        engine: engine.to_str().ok_or("Engine path is not UTF-8")?.into(),
        runtime: runtime.to_str().ok_or("Runtime path is not UTF-8")?.into(),
        runtime_hash: super::guest_runtime::cdhash(&runtime)?,
        app: session.app.clone(),
        build: session.resolved_build_id.clone(),
        icon: guest_icon_version(&contents.join("Resources/guest-icon"))?,
    };
    let profile_lock = bind_profile(profile, &descriptor)?;
    let parent = Path::new(&descriptor.profile)
        .join(crate::paths::NATIVE_APPS)
        .join(descriptor.app.short_bundle_id());
    crate::paths::private_directory(
        profile,
        parent.strip_prefix(profile)?.to_str().ok_or("Native app path is not UTF-8")?,
    )?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent.join(".lock"))?;
    lock.lock()?;
    let mut package = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join(crate::paths::BUILDS).join(&descriptor.build))?;
    if Package::read_app_id(&mut package)? != session.app.app_id {
        return Err("The selected build belongs to another application".into());
    }
    let name = Package::read_display_name(&mut package)?;
    if name.chars().any(|character| character < ' ' && !matches!(character, '\t' | '\n' | '\r')) {
        return Err("App name contains unsupported XML control characters".into());
    }
    let bundle = parent.join(format!("{}.app", filename(&name)));
    let previous = existing_bundle(&parent)?;
    if let Some(previous) = &previous {
        let retained = read_installation(&previous.join("Contents/MacOS/Playbit")).map_err(|error| {
            format!("{error}; preserve and move the generated app bundle aside, then reopen this app from Playbit. Your saved data is retained: {}", previous.display())
        })?;
        validate_binding(&retained, &descriptor)?;
        if retained == descriptor && previous == &bundle {
            return Ok((bundle, profile_lock));
        }
        if retained.engine != descriptor.engine || retained.runtime_hash != descriptor.runtime_hash
        {
            let Reply::InstanceOwners(owners) =
                reply(profile, &Request::InstanceList { session: session.id.clone() })?
            else {
                return Err("Cannot inspect running app instances".into());
            };
            if owners.iter().any(|owner| owner.process.is_some()) {
                return Err("Quit this app before changing its runtime installation".into());
            }
        }
    }
    let icon = Package::read_display_icon(&mut package)?;
    let temporary = parent.join(format!(".{}.app", Id::generate()?));
    let built = (|| -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(temporary.join("Contents/MacOS"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(temporary.join("Contents/Resources"))?;
        let mut launcher = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(temporary.join("Contents/MacOS/Playbit"))?;
        use std::io::Write;
        launcher.write_all(&super::guest_runtime::launcher(&template, &runtime)?)?;
        launcher.sync_all()?;
        let icon_info = write_bundle_icon(
            icon.as_deref(),
            &contents.join("Resources/guest-icon"),
            &temporary.join("Contents/Resources"),
        )?;
        let name = xml(&name);
        fs::write(
            temporary.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>playbit.app.{}</string>
    <key>CFBundleName</key><string>{name}</string>
    <key>CFBundleDisplayName</key><string>{name}</string>
    <key>CFBundleExecutable</key><string>Playbit</string>
{icon_info}
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#,
                descriptor.app.short_bundle_id()
            ),
        )?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temporary.join("Contents/Resources/guest-launch"))?;
        let bytes = serde_json::to_vec(&descriptor)?;
        if bytes.len() > MAX_DESCRIPTOR {
            return Err("Guest installation descriptor exceeds size limit".into());
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        let signing = crate::trace::Span::new("guest.launch.sign");
        super::guest_runtime::sign(
            &temporary,
            &contents.join("Resources/guest-entitlements.plist"),
            &descriptor.runtime_hash,
        )?;
        drop(signing);
        if let Some(previous) = &previous {
            exchange_contents(&temporary, previous)?;
            if previous != &bundle {
                fs::rename(previous, &bundle)?;
            }
        } else {
            fs::rename(&temporary, &bundle)?;
        }
        fs::File::open(&parent)?.sync_all()?;
        super::guest_runtime::register(&bundle)?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_dir_all(&temporary);
    }
    built?;
    Ok((bundle, profile_lock))
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RuntimeBinding {
    engine: String,
    runtime_hash: [u8; 20],
}

impl RuntimeBinding {
    fn read(profile: &Path) -> Result<Option<Self>> {
        match crate::engine::installation::read(
            &profile.join(crate::paths::NATIVE_APPS).join(".installation"),
            8192,
        ) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

impl From<&Installation> for RuntimeBinding {
    fn from(installation: &Installation) -> Self {
        Self { engine: installation.engine.clone(), runtime_hash: installation.runtime_hash }
    }
}

fn profile_gate(profile: &Path) -> Result<fs::File> {
    crate::paths::private_directory(profile, crate::paths::NATIVE_APPS)?;
    let directory = profile.join(crate::paths::NATIVE_APPS);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(".installation.lock"))?;
    Ok(lock)
}

fn clients_gate(profile: &Path) -> Result<fs::File> {
    crate::paths::private_directory(profile, crate::paths::NATIVE_APPS)?;
    Ok(fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(profile.join(crate::paths::NATIVE_APPS).join(".clients.lock"))?)
}

fn drained_clients(profile: &Path) -> Result<fs::File> {
    let file = clients_gate(profile)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => {
            Err("Quit this profile's guest apps before refreshing its runtime installation".into())
        }
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

// Hold the gate through wrapper publication and process reservation/dispatch.
// A different installation cannot rebind between the owner check and admission.
fn bind_profile(profile: &Path, installation: &Installation) -> Result<fs::File> {
    use std::io::Write;
    let lock = profile_gate(profile)?;
    lock.lock_shared()?;
    let binding = RuntimeBinding::from(installation);
    if RuntimeBinding::read(profile)?.as_ref() == Some(&binding) {
        return Ok(lock);
    }
    lock.unlock()?;
    lock.lock()?;
    // Recheck after upgrading the gate: another preparer may have bound it.
    let previous = RuntimeBinding::read(profile)?;
    if previous.as_ref() != Some(&binding) {
        let _clients = drained_clients(profile)?;
        if previous.is_some() {
            let Reply::Status(status) = reply(profile, &Request::Status)? else {
                return Err("Cannot inspect the profile's runtime clients".into());
            };
            for session in status.sessions {
                let Reply::InstanceOwners(owners) =
                    reply(profile, &Request::InstanceList { session: session.id })?
                else {
                    return Err("Cannot inspect runtime owners".into());
                };
                if owners.iter().any(|owner| {
                    matches!(
                        owner.instance.state,
                        State::Reserved | State::Owned | State::Unknown | State::Draining
                    )
                }) {
                    return Err("Quit this profile's guest apps and resolve pending launches before changing runtime installation".into());
                }
            }
        }
        let directory = profile.join(crate::paths::NATIVE_APPS);
        let temporary = directory.join(".installation.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&binding)?)?;
        file.sync_all()?;
        fs::rename(temporary, directory.join(".installation"))?;
        fs::File::open(directory)?.sync_all()?;
    }
    // Preparers for different apps may proceed together. The per-app gate
    // serializes their contents; rebinding requires exclusive profile ownership.
    lock.lock_shared()?;
    if RuntimeBinding::read(profile)?.as_ref() != Some(&binding) {
        return Err(
            "Runtime installation changed during app preparation; reopen the app from Playbit"
                .into(),
        );
    }
    Ok(lock)
}

/// Offline update cutover: retain app metadata and artwork, replacing only the
/// launcher, runtime binding and signature. The caller owns installation leases;
/// taking this profile gate also excludes concurrent wrapper publication.
pub(super) fn refresh_installation(profile: &Path) -> Result<()> {
    use std::io::Write;
    if !profile.is_absolute() {
        return Err("Guest app refresh requires an absolute profile path".into());
    }
    let profile = profile.canonicalize()?;
    let directory = profile.join(crate::paths::NATIVE_APPS);
    if !directory.exists() {
        return Ok(());
    }
    let _gate = profile_gate(&profile)?;
    _gate.lock()?;
    let binding_path = directory.join(".installation");
    let binding = RuntimeBinding::read(&profile)?.ok_or("Missing profile runtime binding")?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let engine = crate::engine::launch::executable_for(&executable)?.canonicalize()?;
    if Path::new(&binding.engine) != engine {
        // A recorded profile may since have been rebound to another installation.
        return Ok(());
    }
    let _clients = drained_clients(&profile)?;
    // This operation deliberately does not speak a possibly different engine
    // protocol. A live endpoint means cutover has not drained the profile.
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&socket2::SockAddr::unix(profile.join(crate::paths::ENGINE_SOCKET))?) {
        Ok(()) => return Err("Stop this profile's engine before refreshing app wrappers".into()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
    }
    let contents = executable.parent().and_then(Path::parent).ok_or("Missing app contents")?;
    let runtime = super::guest_runtime::runtime_for_engine(&engine)?;
    let runtime_hash = super::guest_runtime::cdhash(&runtime)?;
    for (index, entry) in fs::read_dir(&directory)?.enumerate() {
        if index >= 512 {
            return Err("Too many generated app directories".into());
        }
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        if !entry.file_type()?.is_dir() {
            return Err("Unsafe generated app directory".into());
        }
        let Some(bundle) = existing_bundle(&entry.path())? else { continue };
        let mut descriptor = read_installation(&bundle.join("Contents/MacOS/Playbit"))?;
        if Path::new(&descriptor.profile) != profile
            || Path::new(&descriptor.engine) != engine
            || entry.file_name() != std::ffi::OsStr::new(&descriptor.app.short_bundle_id())
        {
            return Err("Generated app has a conflicting installation or full app identity".into());
        }
        if descriptor.runtime_hash == runtime_hash && Path::new(&descriptor.runtime) == runtime {
            continue;
        }
        // Validate the old complete signed bundle before retaining its resources.
        super::guest_runtime::cdhash(&bundle)?;
        descriptor.runtime = runtime.to_str().ok_or("Runtime path is not UTF-8")?.into();
        descriptor.runtime_hash = runtime_hash;
        let temporary = entry.path().join(format!(".{}.app", Id::generate()?));
        let result = (|| -> Result<()> {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(temporary.join("Contents/MacOS"))?;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(temporary.join("Contents/Resources"))?;
            fs::write(
                temporary.join("Contents/Info.plist"),
                crate::engine::installation::read(&bundle.join("Contents/Info.plist"), 64 * 1024)?,
            )?;
            for name in
                ["Assets.car", "icon-guest.icns", "App.icns", "AppDark.icns", icon::PNG_ICON]
            {
                let source = bundle.join("Contents/Resources").join(name);
                match crate::engine::installation::read(&source, 16 * 1024 * 1024) {
                    Ok(bytes) => fs::write(temporary.join("Contents/Resources").join(name), bytes)?,
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(temporary.join("Contents/MacOS/Playbit"))?;
            file.write_all(&super::guest_runtime::launcher(
                &contents.join("Resources/guest-launcher"),
                &runtime,
            )?)?;
            file.sync_all()?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(temporary.join("Contents/Resources/guest-launch"))?;
            let bytes = serde_json::to_vec(&descriptor)?;
            if bytes.len() > MAX_DESCRIPTOR {
                return Err("Guest installation descriptor exceeds size limit".into());
            }
            file.write_all(&bytes)?;
            file.sync_all()?;
            super::guest_runtime::sign(
                &temporary,
                &contents.join("Resources/guest-entitlements.plist"),
                &runtime_hash,
            )?;
            exchange_contents(&temporary, &bundle)?;
            fs::File::open(entry.path())?.sync_all()?;
            super::guest_runtime::register(&bundle)?;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result?;
    }
    // Publish the profile binding only after every wrapper is ready. A partial
    // refresh stays closed to admission and is safe to retry after interruption.
    let temporary = directory.join(".installation.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(&RuntimeBinding { engine: binding.engine, runtime_hash })?)?;
    file.sync_all()?;
    fs::rename(temporary, binding_path)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// Called once after the management engine starts. Rebuild only existing app
/// wrappers; importing an account does not implicitly install all its apps.
pub(super) fn refresh_wrappers(profile: &Path) -> Result<()> {
    let executable = std::env::current_exe()?;
    let _installation = crate::engine::installation::client_lease(&executable)?;
    let Reply::Status(status) = reply(profile, &Request::Status)? else {
        return Err("Cannot refresh app wrappers".into());
    };
    let mut sessions = status.sessions;
    sessions.sort_by_key(|session| {
        status.recent_sessions.iter().position(|id| id == &session.id).unwrap_or(usize::MAX)
    });
    let mut seen = std::collections::BTreeSet::new();
    for session in sessions {
        let short = session.app.short_bundle_id();
        if !seen.insert(short.clone()) {
            continue;
        }
        let directory = profile.join(crate::paths::NATIVE_APPS).join(short);
        if directory.is_dir()
            && profile.join(crate::paths::BUILDS).join(&session.resolved_build_id).is_file()
        {
            prepare_bundle(profile, &session)?;
        }
    }
    Ok(())
}

fn filename(name: &str) -> String {
    let mut value = String::new();
    for character in name.chars().filter(|character| !character.is_control()) {
        let character = if matches!(character, '/' | ':') { '-' } else { character };
        if value.len() + character.len_utf8() > 200 {
            break;
        }
        value.push(character);
    }
    let value = value.trim_matches([' ', '.']);
    if value.is_empty() { "App".into() } else { value.into() }
}

fn existing_bundle(parent: &Path) -> Result<Option<PathBuf>> {
    let mut found = None;
    for (index, entry) in fs::read_dir(parent)?.enumerate() {
        if index >= 16 {
            return Err("Too many entries in app directory".into());
        }
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        if entry.path().extension().is_some_and(|ext| ext == "app") {
            if !entry.file_type()?.is_dir() || found.is_some() {
                return Err("Ambiguous or unsafe app wrapper directory".into());
            }
            found = Some(entry.path());
        }
    }
    Ok(found)
}

fn validate_binding(retained: &Installation, expected: &Installation) -> Result<()> {
    if retained.app != expected.app
        || (retained.profile != expected.profile && Path::new(&retained.profile).exists())
    {
        return Err("The wrapper belongs to another account or application; preserve it and resolve the app namespace collision".into());
    }
    Ok(())
}

#[allow(
    unsafe_code,
    reason = "Atomically exchange owned Contents directories without replacing the wrapper inode."
)]
fn exchange_contents(staged: &Path, installed: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let staged = std::ffi::CString::new(staged.join("Contents").as_os_str().as_bytes())?;
    let installed = std::ffi::CString::new(installed.join("Contents").as_os_str().as_bytes())?;
    // SAFETY: NUL-terminated paths remain live; both directories were checked and are locked.
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            staged.as_ptr(),
            libc::AT_FDCWD,
            installed.as_ptr(),
            libc::RENAME_SWAP,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub(super) fn read_installation(executable: &Path) -> Result<Installation> {
    let contents =
        executable.parent().and_then(Path::parent).ok_or("Missing guest bundle directory")?;
    // Persistent wrapper metadata must survive a different runtime/IPC build.
    let bytes = crate::engine::installation::read(
        &contents.join("Resources/guest-launch"),
        MAX_DESCRIPTOR as u64,
    )?;
    let value: Installation = serde_json::from_slice(&bytes)?;
    if value.profile.len() > 4096
        || value.engine.len() > 4096
        || value.runtime.len() >= super::guest_runtime::RUNTIME_PATH_BYTES
        || value.app.app_id.len() > crate::package::identity::MAX_APP_ID_BYTES
        || value.build.len() > 44
        || value.icon.len() > 44
    {
        return Err("Invalid guest descriptor bounds".into());
    }
    value.app.validate()?;
    if !Path::new(&value.profile).is_absolute()
        || !Path::new(&value.engine).is_absolute()
        || !Path::new(&value.runtime).is_absolute()
        || !crate::identifier::valid(&value.build, 32)
        || !crate::identifier::valid(&value.icon, 32)
        || value.runtime_hash == [0; 20]
    {
        return Err("Invalid guest installation descriptor binding".into());
    }
    Ok(value)
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn guest_icon_version(compiled: &Path) -> Result<String> {
    // Icon-only package updates must not reuse bundles with the previous artwork.
    let mut version = String::new();
    fs::File::open(compiled.join("version"))?.take(66).read_to_string(&mut version)?;
    let version = version.trim_end_matches('\n');
    if !crate::identifier::valid(version, 32) {
        return Err("Invalid packaged guest icon version".into());
    }
    let mut hash = blake3::Hasher::new();
    // Rebuild cached wrappers that predate the PNG-specific icon filename.
    hash.update(icon::PNG_ICON.as_bytes());
    hash.update(version.as_bytes());
    hash.update(crate::package::artwork::macos_template_digest().as_bytes());
    Ok(crate::identifier::encode(hash.finalize().as_bytes()))
}

fn write_bundle_icon(
    source: Option<&[u8]>,
    compiled: &Path,
    output: &Path,
) -> Result<&'static str> {
    if let Some(source) = source {
        let format = image::guess_format(source);
        if matches!(format, Ok(image::ImageFormat::Png)) {
            write_icon(source, &output.join(icon::PNG_ICON), false)?;
            return Ok("    <key>CFBundleIconFile</key><string>AppPNG.icns</string>");
        }
        write_icon(source, &output.join("App.icns"), false)?;
        if format.is_err() {
            write_icon(source, &output.join("AppDark.icns"), true)?;
        }
        return Ok("    <key>CFBundleIconFile</key><string>App.icns</string>");
    }
    for name in ["Assets.car", "icon-guest.icns"] {
        fs::copy(compiled.join(name), output.join(name))?;
    }
    Ok(r#"    <key>CFBundleIconFile</key><string>icon-guest</string>
    <key>CFBundleIconName</key><string>icon-guest</string>"#)
}

fn write_icon(source: &[u8], path: &Path, dark: bool) -> Result<()> {
    let mut bytes = b"icns\0\0\0\0".to_vec();
    for (side, kind) in [(128, b"ic07"), (256, b"ic08"), (512, b"ic09")] {
        let image = crate::package::artwork::macos_icon(source, side, dark)?;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png)?;
        let png = png.into_inner();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&u32::try_from(png.len() + 8)?.to_be_bytes());
        bytes.extend_from_slice(&png);
    }
    let length = u32::try_from(bytes.len())?;
    bytes[4..8].copy_from_slice(&length.to_be_bytes());
    fs::write(path, bytes)?;
    Ok(())
}

#[allow(unsafe_code, reason = "Activation uses only the kernel-verified owner's exact PID.")]
pub(super) fn activate(
    profile: &Path,
    incarnation: Id,
    owner: &crate::engine::session_instance::LaunchOwner,
) -> Result<u64> {
    use crate::ui::app_control::{Exchange, Operation};
    // Restore only this verified owner's local interface. Never replay Show or
    // create a replacement guest after an uncertain response.
    let Reply::InstanceShowing { presentation, .. } = reply(
        profile,
        &Request::InstanceShow {
            incarnation,
            instance: owner.instance.id,
            owner_generation: owner.instance.owner_generation,
        },
    )?
    else {
        return Err("Unexpected app presentation reply".into());
    };
    let shown = Exchange::connect(profile, owner, Operation::Show { presentation })
        .and_then(Exchange::receive);
    let identity = owner.process.as_ref().expect("selected owner has a verified process");
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    let pid = i32::try_from(identity.pid())
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or("Invalid verified GUI process")?;
    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
        let app: *mut Object =
            msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid];
        // NSRunningApplication retains its application identity. Recheck the
        // recorded kernel generation after obtaining that object, before acting.
        let owner = crate::engine::process_owner::Owner::recover(identity);
        let active = owner.as_ref().is_ok_and(Option::is_some) && !app.is_null() && {
            let result: bool = msg_send![app, activateWithOptions: 2usize];
            result
        };
        let _: () = msg_send![pool, drain];
        owner?.ok_or("The recorded app owner exited before activation; it was not relaunched")?;
        // Even a failed owner can have a recovery/quit dialog. Bring its verified
        // process forward before reporting a missing control endpoint.
        if let Err(error) = shown {
            return Err(activation_error(error).into());
        }
        if !active {
            return Err(
                "The verified app owner could not be activated; its instance was not relaunched"
                    .into(),
            );
        }
        Ok(presentation)
    }
}

fn activation_error(error: Box<dyn std::error::Error + Send + Sync>) -> String {
    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
    }) {
        "This session’s app is still open, but it is no longer responding to Playbit. Quit that app, then start the session again. Your saved data is retained.".into()
    } else {
        format!("Couldn’t contact this session’s existing app: {error}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded launch descriptor admission.")]
mod tests {
    use super::*;

    #[test]
    fn ordinary_opens_prefer_live_then_retained_recency_and_explicit_opens_check_identity() {
        use crate::engine::{Engine, session_instance::LaunchOwner};
        use std::os::unix::net::{UnixListener, UnixStream};
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let root = engine.services.store.create_volume("Routing", false).unwrap();
        let older = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let recent =
            engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
        let (client, peer) = UnixStream::pair().unwrap();
        let process = crate::engine::process_owner::Owner::capture(&peer).unwrap().identity;
        let instance = SessionInstance {
            id: Id::from_bytes([2; 16]),
            launch_request: Id::from_bytes([3; 16]),
            account: engine.services.store.account().id,
            session: older.id.clone(),
            build: older.resolved_build_id.clone(),
            root: older.root_volume,
            extra_volumes: vec![],
            host_mounts: Box::default(),
            restore_host_mounts: true,
            owner_generation: 1,
            state: State::Owned,
        };
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        for case in 0..5 {
            let Reply::Status(mut status) = engine.dispatch_reply(Request::Status).unwrap() else {
                panic!("status")
            };
            // Another publisher's app_id never enters this wrapper's candidate set.
            let mut foreign = recent.clone();
            foreign.id = crate::identifier::encode(&[9; 32]);
            foreign.app.publisher = Id::from_bytes([9; 16]);
            status.recent_sessions = vec![foreign.id.clone(), recent.id.clone(), older.id.clone()];
            status.sessions.push(foreign);
            if case == 4 {
                status.sessions.clear();
            }
            let mut responses = vec![(Request::Status, Reply::Status(status))];
            if case != 4 {
                responses.push((
                    Request::InstanceList { session: recent.id.clone() },
                    Reply::InstanceOwners(vec![]),
                ));
                let owners = if case == 0 {
                    vec![]
                } else {
                    vec![LaunchOwner {
                        presentation: 0,
                        instance: instance.clone(),
                        process: Some(process.clone()),
                    }]
                };
                responses.push((
                    Request::InstanceList { session: older.id.clone() },
                    Reply::InstanceOwners(owners),
                ));
            }
            let listener = listener.try_clone().unwrap();
            let server = std::thread::spawn(move || {
                for (expected, response) in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                    let bytes = ipc::read(&mut stream).unwrap();
                    let request =
                        rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).unwrap();
                    assert_eq!(
                        serde_json::to_value(request).unwrap(),
                        serde_json::to_value(expected).unwrap()
                    );
                    ipc::write(&mut stream, &ipc::codec::encode(&response, 1024 * 1024).unwrap())
                        .unwrap();
                }
            });
            let requested = match case {
                2 => Some(instance.id),
                3 => Some(Id::from_bytes([8; 16])),
                _ => None,
            };
            let selected = select_session(directory.path(), &older.app, requested);
            if case >= 3 {
                assert!(selected.is_err());
            } else {
                let (session, binding) = selected.unwrap();
                assert_eq!(session.id, if case == 0 { &recent.id } else { &older.id }.as_str());
                assert_eq!(binding.map(|value| value.id), requested);
            }
            server.join().unwrap();
        }
        drop(client);
    }

    #[test]
    fn claimed_instance_wins_over_rejection_and_missing_callback_preserves_reservation() {
        // Exercise the real authenticated IPC observations used after a callback;
        // a stale incarnation must not authorize another launch or mark failure.
        use crate::engine::{ipc::codec, session_instance::LaunchOwner};
        use std::os::unix::net::{UnixListener, UnixStream};
        for claimed in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join("control")).unwrap();
            let listener =
                UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
            let incarnation = Id::from_bytes([1; 16]);
            let instance = Id::from_bytes([2; 16]);
            let receipt =
                LaunchReceipt { presentation: 0, incarnation, instance, owner_generation: 1 };
            let session = crate::identifier::encode(&[3; 32]);
            let (client, peer) = UnixStream::pair().unwrap();
            let owner = crate::engine::process_owner::Owner::capture(&peer).unwrap();
            let binding = SessionInstance {
                id: instance,
                launch_request: Id::from_bytes([4; 16]),
                account: Id::from_bytes([5; 16]),
                session: session.clone(),
                build: crate::identifier::encode(&[6; 32]),
                root: Id::from_bytes([7; 16]),
                extra_volumes: vec![],
                host_mounts: Box::default(),
                restore_host_mounts: true,
                owner_generation: 1,
                state: State::Owned,
            };
            let server = std::thread::spawn(move || {
                let mut replies = vec![Reply::InstanceStatus {
                    incarnation: if claimed { incarnation } else { Id::from_bytes([8; 16]) },
                    instance: None,
                }];
                if claimed {
                    replies.push(Reply::InstanceOwners(vec![LaunchOwner {
                        presentation: 0,
                        instance: binding,
                        process: Some(owner.identity),
                    }]));
                }
                for (index, response) in replies.into_iter().enumerate() {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                    let bytes = ipc::read(&mut stream).unwrap();
                    let request =
                        rkyv::api::low::from_bytes::<Request, rkyv::rancor::Error>(&bytes).unwrap();
                    assert!(matches!(
                        (index, request),
                        (0, Request::InstanceInspect { request: None })
                            | (1, Request::InstanceList { .. })
                    ));
                    ipc::write(&mut stream, &codec::encode(&response, 4096).unwrap()).unwrap();
                }
            });
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            if claimed {
                send.send(native_launch::Outcome::Rejected("late rejection".into()))
                    .unwrap_or_else(|_| panic!("callback receiver"));
            }
            drop(send);
            let pending = PendingLaunch {
                receipt,
                profile: directory.path().to_owned(),
                session,
                completion: Some(native_launch::Completion(receive)),
            };
            if claimed {
                assert_eq!(pending.wait().unwrap(), receipt);
            } else {
                assert!(
                    pending
                        .wait()
                        .unwrap_err()
                        .to_string()
                        .contains("inspect it before another launch")
                );
            }
            server.join().unwrap();
            drop(client);
        }
    }

    #[test]
    fn missing_owner_endpoint_explains_recovery_without_claiming_missing_app_data() {
        for kind in [std::io::ErrorKind::NotFound, std::io::ErrorKind::ConnectionRefused] {
            let message = activation_error(std::io::Error::from(kind).into());
            assert!(message.contains("Quit that app"));
            assert!(message.contains("saved data is retained"));
        }
        let message =
            activation_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
        assert!(message.contains("permission denied"));
    }
    #[test]
    fn guest_icons_preserve_custom_artwork_and_require_packaged_default() {
        let directory = tempfile::tempdir().unwrap();
        let compiled = directory.path().join("compiled");
        let output = directory.path().join("output");
        fs::create_dir(&compiled).unwrap();
        fs::create_dir(&output).unwrap();
        assert!(guest_icon_version(&compiled).is_err());
        fs::write(
            compiled.join("version"),
            format!("{}\n", crate::identifier::encode(&[0xab; 32])),
        )
        .unwrap();
        let first = guest_icon_version(&compiled).unwrap();
        assert!(crate::identifier::valid(&first, 32));
        fs::write(compiled.join("version"), crate::identifier::encode(&[0xcd; 32])).unwrap();
        assert_ne!(guest_icon_version(&compiled).unwrap(), first);
        fs::write(compiled.join("version"), "../invalid").unwrap();
        assert!(guest_icon_version(&compiled).is_err());
        assert!(write_bundle_icon(None, &compiled, &output).is_err());
        fs::write(compiled.join("Assets.car"), b"compiled layered icon").unwrap();
        fs::write(compiled.join("icon-guest.icns"), b"compiled fallback icon").unwrap();
        let info = write_bundle_icon(None, &compiled, &output).unwrap();
        assert!(info.contains("<key>CFBundleIconName</key><string>icon-guest</string>"));
        for name in ["Assets.car", "icon-guest.icns"] {
            assert_eq!(
                fs::read(output.join(name)).unwrap(),
                fs::read(compiled.join(name)).unwrap()
            );
            fs::remove_file(output.join(name)).unwrap();
        }
        // Raster sources select their Dock treatment from their original format.
        for (format, filename) in
            [(image::ImageFormat::Png, icon::PNG_ICON), (image::ImageFormat::Jpeg, "App.icns")]
        {
            let mut source = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2).write_to(&mut source, format).unwrap();
            let raster = tempfile::tempdir().unwrap();
            let info = write_bundle_icon(Some(source.get_ref()), &compiled, raster.path()).unwrap();
            assert!(
                info.contains(&format!("<key>CFBundleIconFile</key><string>{filename}</string>"))
            );
            let files: Vec<_> = fs::read_dir(raster.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(files, [std::ffi::OsStr::new(filename)]);
        }
        let source =
            br#"<svg width="24" height="24"><rect width="24" height="24" fill="red"/></svg>"#;
        let info = write_bundle_icon(Some(source), &compiled, &output).unwrap();
        assert!(!info.contains("CFBundleIconName"));
        let bytes = fs::read(output.join("App.icns")).unwrap();
        assert_eq!(&bytes[..4], b"icns");
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len());
        // Both wrapper variants contain the same exact pixels supplied to Quick Look.
        for (name, dark) in [("App.icns", false), ("AppDark.icns", true)] {
            let bytes = fs::read(output.join(name)).unwrap();
            let mut offset = 8;
            for side in [128, 256, 512] {
                let length =
                    u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                let png = &bytes[offset + 8..offset + length];
                let image = image::load_from_memory(png).unwrap().to_rgba8();
                assert_eq!(image, crate::package::artwork::macos_icon(source, side, dark).unwrap());
                offset += length;
            }
            assert_eq!(offset, bytes.len());
        }
        assert!(!output.join("Assets.car").exists());
        assert!(write_bundle_icon(Some(b"invalid artwork"), &compiled, &output).is_err());
    }

    fn descriptor() -> Installation {
        Installation {
            profile: "/private/tmp/profile".into(),
            engine: "/private/tmp/Playbit.app/Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine".into(),
            runtime: "/private/tmp/Playbit.app/Contents/Frameworks/libplaybit_runtime.dylib".into(),
            runtime_hash: [7; 20],
            app: crate::package::AppIdentity { publisher: Id::from_bytes([3; 16]), app_id: "pinball".into() },
            build: crate::identifier::encode(&[4; 32]),
            icon: crate::identifier::encode(&[5; 32]),
        }
    }

    #[test]
    fn persisted_wrapper_metadata_is_bounded_json_independent_of_the_ipc_build() {
        let root = tempfile::tempdir().unwrap();
        let resources = root.path().join("Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        let path = resources.join("guest-launch");
        let executable = root.path().join("Contents/MacOS/Playbit");
        let expected = descriptor();
        let mut value = serde_json::to_value(&expected).unwrap();
        assert_eq!(value["app"]["publisher"], expected.app.publisher.to_string());
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(read_installation(&executable).unwrap(), expected);
        value["unexpected"] = true.into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_installation(&executable).is_err());
        fs::write(&path, vec![b' '; MAX_DESCRIPTOR + 1]).unwrap();
        assert!(read_installation(&executable).is_err());
    }

    #[test]
    fn app_preparers_share_the_profile_gate_and_exclude_installation_refresh() {
        let root = tempfile::tempdir().unwrap();
        let installation = descriptor();
        let first = bind_profile(root.path(), &installation).unwrap();
        let profile = root.path().to_owned();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let preparer = std::thread::spawn(move || {
            send.send(bind_profile(&profile, &installation)).unwrap();
        });
        let second = receive.recv_timeout(std::time::Duration::from_secs(2));
        drop(first);
        let second =
            second.expect("another app must prepare while the first holds its gate").unwrap();
        preparer.join().unwrap();
        let refresh = profile_gate(root.path()).unwrap();
        assert!(matches!(refresh.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(second);
        refresh.try_lock().unwrap();
    }

    #[test]
    fn wrapper_names_are_bounded_and_cannot_escape_the_app_directory() {
        assert_eq!(filename("../Pinball: Testing"), "-Pinball- Testing");
        assert_eq!(filename("..."), "App");
        assert!(filename(&"🎮".repeat(200)).len() <= 200);
    }

    #[test]
    fn a_truncated_account_collision_never_reuses_another_publishers_wrapper() {
        let expected = descriptor();
        let mut other = expected.clone();
        let mut account = *other.app.publisher.as_bytes();
        account[15] ^= 1;
        other.app.publisher = Id::from_bytes(account);
        assert_eq!(other.app.short_bundle_id(), expected.app.short_bundle_id());
        assert!(validate_binding(&other, &expected).is_err());
        assert!(validate_binding(&expected, &expected).is_ok());
    }

    #[test]
    fn refreshing_contents_preserves_the_wrapper_directory_identity() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("Pinball.app");
        let new = root.path().join("staged.app");
        fs::create_dir_all(old.join("Contents")).unwrap();
        fs::create_dir_all(new.join("Contents")).unwrap();
        fs::write(old.join("Contents/value"), b"old").unwrap();
        fs::write(new.join("Contents/value"), b"new").unwrap();
        let inode = fs::metadata(&old).unwrap().ino();
        exchange_contents(&new, &old).unwrap();
        assert_eq!(fs::metadata(&old).unwrap().ino(), inode);
        assert_eq!(fs::read(old.join("Contents/value")).unwrap(), b"new");
        assert_eq!(fs::read(new.join("Contents/value")).unwrap(), b"old");
    }
}
