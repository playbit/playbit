//! macOS release updates. One background operation and one native dialog.
mod archive;
mod copy;
mod dialog;
mod events;
mod install;
mod package;
#[cfg(any(test, feature = "desktop-qualification"))]
mod preview;
#[cfg(any(test, feature = "desktop-qualification"))]
pub(super) use preview::setup as preview;
#[cfg(test)]
mod tests;

use crate::engine::{
    Result,
    installation::{self, Installation},
};
use crate::release_channel::ReleaseChannel;
use dialog::{Choice, Dialog};
use gpui::{App, Global};
use package::Release;
use std::time::Duration;

pub use dialog::launch_error;
pub use dialog::update_error;
pub use install::{requested as installer_requested, run as run_installer};

const INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
enum State {
    Starting,
    Disabled(DisabledReason),
    Idle,
    Checking,
    Available(Release),
    Preparing(String),
    Ready(String),
    Failed(String),
}

#[derive(Clone, Copy)]
enum DisabledReason {
    Development,
    PersonalTesting,
}

struct UpdateFailure {
    diagnostic: String,
    message: String,
    notify: bool,
}

impl UpdateFailure {
    fn new(state: &State, error: String) -> Self {
        let recovery = if error == installation::LOCATION_ERROR {
            "Move Playbit to your Applications folder, or download and install the update manually."
                .to_owned()
        } else {
            format!(
                "{error}\n\nTry Check for Updates again, or download and install the update manually."
            )
        };
        match state {
            State::Preparing(version) | State::Ready(version) => Self {
                diagnostic: format!(
                    "could not install Playbit {version}: {}",
                    error.replace(['\r', '\n'], " ")
                ),
                message: format!(
                    "Playbit {version} is available, but the update couldn't be installed automatically.\n\n{recovery}"
                ),
                notify: true,
            },
            _ => {
                Self { diagnostic: error.replace(['\r', '\n'], " "), message: error, notify: false }
            }
        }
    }
}
struct Updates {
    installation: Option<Installation>,
    state: State,
    busy: bool,
    refresh_pending: bool,
    _observer: Option<events::Observer>,
    prepared: Option<String>,
    explicit: bool,
    reopen: bool,
    quitting: bool,
    dialog: Option<Dialog>,
    choices: async_channel::Sender<Choice>,
}
impl Global for Updates {}

enum Operation {
    Initialize(Option<std::ffi::OsString>),
    Check,
    Prepare(Release),
    Refresh,
}
enum Response {
    Disabled,
    Initialized(Option<Installation>, Option<String>, Option<String>),
    Current,
    Available(Release),
    Prepared(String),
    Refreshed(Option<String>),
}

gpui::actions!(playbit_shell, [CheckForUpdates, Upgrade]);

pub(super) fn setup(host: gpui::WeakEntity<super::Shell>, cx: &mut App) {
    setup_for(host, crate::engine::profile::current().1, cx);
}

fn setup_for(host: gpui::WeakEntity<super::Shell>, channel: ReleaseChannel, cx: &mut App) {
    let (choices, receive) = async_channel::bounded(1);
    let personal = channel.is_personal();
    cx.set_global(Updates {
        _observer: None,
        refresh_pending: false,
        installation: None,
        state: if personal {
            State::Disabled(DisabledReason::PersonalTesting)
        } else {
            State::Starting
        },
        busy: false,
        prepared: None,
        explicit: false,
        reopen: false,
        quitting: false,
        dialog: None,
        choices,
    });
    cx.on_action(|_: &CheckForUpdates, cx| check(true, cx));
    cx.spawn(async move |cx| {
        while let Ok(choice) = receive.recv().await {
            if cx.update(|cx| choose(choice, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    if personal {
        return;
    }
    cx.on_action(|_: &Upgrade, cx| {
        if !ready(cx) {
            return;
        }
        cx.global_mut::<Updates>().reopen = true;
        cx.quit();
    });
    cx.on_quit_requested(quit_requested).expect("macOS deferred termination");
    cx.observe_global::<Updates>(move |cx| {
        let _ = host.update(cx, |_, cx| cx.notify());
    })
    .detach();
    let development_override = std::env::var_os("PLAYBIT_DEV_ENABLE_AUTOUPDATE");
    // Admission runs after shell creation; signature checks never gate the UI.
    cx.spawn(async move |cx| {
        gpui::Timer::after(Duration::ZERO).await;
        let _ = cx.update(|cx| start(Operation::Initialize(development_override), cx));
    })
    .detach();
}

fn watch(cx: &mut App) {
    let (changed, changes) = async_channel::bounded(1);
    cx.global_mut::<Updates>()._observer = Some(events::Observer::new(changed));
    cx.spawn(async move |cx| {
        while changes.recv().await.is_ok() {
            if cx
                .update(|cx| {
                    let updates = cx.global_mut::<Updates>();
                    if updates.installation.is_none() {
                        return;
                    }
                    if updates.busy {
                        updates.refresh_pending = true;
                    } else {
                        start(Operation::Refresh, cx);
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        loop {
            events::after(INTERVAL).await;
            if cx.update(|cx| check(false, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

pub(super) fn ready(cx: &App) -> bool {
    cx.try_global::<Updates>().is_some_and(|updates| updates.prepared.is_some())
}

fn check(explicit: bool, cx: &mut App) {
    let updates = cx.global_mut::<Updates>();
    if explicit {
        updates.explicit = true;
        if let Some(dialog) = &updates.dialog {
            dialog.focus();
            return;
        }
    }
    if updates.busy || matches!(updates.state, State::Starting | State::Disabled(_)) {
        present(cx);
        return;
    }
    updates.state = State::Checking;
    present(cx);
    start(Operation::Check, cx);
}

pub(super) fn channel_switch_ready(cx: &App) -> std::result::Result<(), String> {
    if cx
        .try_global::<Updates>()
        .is_some_and(|updates| updates.busy || updates.quitting || updates.prepared.is_some())
    {
        return Err("Finish the current update before switching channels".into());
    }
    Ok(())
}

fn choose(choice: Choice, cx: &mut App) {
    let updates = cx.global_mut::<Updates>();
    updates.dialog.take();
    if choice == Choice::Dismiss {
        updates.explicit = false;
        return;
    }
    let state = updates.state.clone();
    updates.reopen = choice == Choice::Install;
    updates.explicit = choice == Choice::Install;
    match state {
        State::Available(release) => {
            updates.state = State::Preparing(release.identity().to_string());
            present(cx);
            start(Operation::Prepare(release), cx);
        }
        State::Ready(_) if choice == Choice::Install => cx.quit(),
        _ => (),
    }
}

fn present(cx: &mut App) {
    let updates = cx.global_mut::<Updates>();
    if !updates.explicit {
        return;
    }
    let (title, message, progress, available) = match &updates.state {
        State::Starting => (
            "Checking for updates",
            "Checking whether this build supports updates…".into(),
            true,
            false,
        ),
        State::Disabled(DisabledReason::Development) => (
            "Updates disabled",
            "Updates are disabled for this development build.\n\nRelaunch Playbit with PLAYBIT_DEV_ENABLE_AUTOUPDATE=1 to enable updates.".into(),
            false,
            false,
        ),
        State::Disabled(DisabledReason::PersonalTesting) => (
            "Updates unavailable",
            "Playbit updates are unavailable in personal Testing.".into(),
            false,
            false,
        ),
        State::Checking => {
            ("Checking for updates", "Contacting the release server…".into(), true, false)
        }
        State::Preparing(_) => {
            ("Preparing update", "Downloading and verifying Playbit…".into(), true, false)
        }
        State::Idle => (
            "You're up to date",
            format!("Playbit {} is currently the newest version available", crate::RELEASE_VERSION),
            false,
            false,
        ),
        State::Available(release) => (
            "Update available",
            format!("Playbit {} is available", release.identity()),
            false,
            true,
        ),
        State::Ready(version) => {
            ("Update available", format!("Playbit {version} is available"), false, true)
        }
        State::Failed(message) => ("Couldn't update Playbit", message.clone(), false, false),
    };
    let buttons: &[(&str, Choice)] = if available {
        &[("Install", Choice::Install), ("Install on Quit", Choice::Later)]
    } else {
        &[("Dismiss", Choice::Dismiss)]
    };
    // Keep the environment variable unbroken in the development hint.
    let minimum_width = if matches!(updates.state, State::Disabled(DisabledReason::Development)) {
        300.
    } else {
        0.
    };
    updates.dialog.take();
    updates.dialog = Some(Dialog::new(
        title,
        &message,
        buttons,
        progress,
        minimum_width,
        updates.choices.clone(),
    ));
}

fn start(operation: Operation, cx: &mut App) {
    let updates = cx.global_mut::<Updates>();
    if updates.busy
        || matches!(updates.state, State::Disabled(_))
        || (matches!(updates.state, State::Starting)
            && !matches!(operation, Operation::Initialize(_)))
    {
        return;
    }
    updates.busy = true;
    let installation = updates.installation.clone();
    let work = cx
        .background_executor()
        .spawn(async move { execute(installation, operation).map_err(|error| error.to_string()) });
    cx.spawn(async move |cx| {
        let result = work.await;
        let _ = cx.update(|cx| complete(result, cx));
    })
    .detach();
}

fn execute(installation: Option<Installation>, operation: Operation) -> Result<Response> {
    if let Operation::Initialize(development_override) = operation {
        let eligibility = std::env::current_exe()
            .map_err(Into::into)
            .and_then(|executable| package::enabled(&executable, development_override.as_deref()));
        match eligibility {
            Ok(true) => return initialize(),
            Ok(false) => return Ok(Response::Disabled),
            Err(error) => {
                crate::platform::failure::log(
                    &crate::engine::profile_path(),
                    "macOS update",
                    &format!("could not determine update eligibility: {error}"),
                );
                return Ok(Response::Disabled);
            }
        }
    }
    // Copies that cannot install still discover releases and explain the failure
    // only when there is an update to apply.
    if matches!(operation, Operation::Check) && installation.is_none() {
        return Ok(match package::check(crate::engine::profile::current().1)? {
            Some(release) => Response::Available(release),
            None => Response::Current,
        });
    }
    let installation = installation.ok_or(installation::LOCATION_ERROR)?;
    let requested = std::time::SystemTime::now();
    let _operation = installation.operation()?;
    let ready = || -> Result<Option<String>> {
        Ok(install::Journal::read(&installation, crate::engine::profile::current().1)?
            .filter(|journal| journal.phase != install::Phase::Installed)
            .map(|journal| journal.to.to_string()))
    };
    match operation {
        Operation::Check => Ok(
            match package::coalesced_check(
                &installation,
                requested,
                crate::engine::profile::current().1,
            )? {
                Some(release) => Response::Available(release),
                None => Response::Current,
            },
        ),
        Operation::Prepare(release) => {
            installation.ensure_installable()?;
            if let Some(version) = ready()? {
                return Ok(Response::Prepared(version));
            }
            package::prepare(&installation, &release)?;
            install::arm(&installation, &release.identity(), crate::engine::profile::current().1)?;
            Ok(Response::Prepared(release.identity().to_string()))
        }
        Operation::Refresh => Ok(Response::Refreshed(ready()?)),
        Operation::Initialize(_) => unreachable!("initialization is handled above"),
    }
}

fn complete(result: std::result::Result<Response, String>, cx: &mut App) {
    let updates = cx.global_mut::<Updates>();
    updates.busy = false;
    match result {
        Ok(Response::Disabled) => updates.state = State::Disabled(DisabledReason::Development),
        Ok(Response::Initialized(installation, ready, error)) => {
            initialized(installation, ready, error, cx);
            return;
        }
        Ok(Response::Current) => {
            updates.state = updates.prepared.clone().map(State::Ready).unwrap_or(State::Idle)
        }
        Ok(Response::Available(release)) => {
            if let Some(version) = &updates.prepared {
                updates.state = State::Ready(version.clone());
                present(cx);
                return;
            }
            if !updates.explicit {
                updates.state = State::Preparing(release.identity().to_string());
                start(Operation::Prepare(release), cx);
                return;
            }
            updates.state = State::Available(release);
        }
        Ok(Response::Prepared(version)) => {
            events::prepared();
            updates.prepared = Some(version.clone());
            updates.state = State::Ready(version);
            if updates.reopen {
                updates.dialog.take();
                cx.quit();
                return;
            }
        }
        Ok(Response::Refreshed(version)) => {
            if let Some(version) = version {
                updates.prepared = Some(version.clone());
                updates.state = State::Ready(version);
            }
        }
        Err(error) => {
            failed(error, cx);
        }
    }
    present(cx);
    let updates = cx.global_mut::<Updates>();
    if updates.refresh_pending {
        updates.refresh_pending = false;
        start(Operation::Refresh, cx);
    }
}

fn failed(error: String, cx: &mut App) {
    let updates = cx.global_mut::<Updates>();
    updates.reopen = false;
    let failure = UpdateFailure::new(&updates.state, error);
    updates.explicit |= failure.notify;
    updates.state = State::Failed(failure.message);
    cx.background_executor()
        .spawn(async move {
            crate::platform::failure::log(
                &crate::engine::profile_path(),
                "macOS update",
                &failure.diagnostic,
            );
        })
        .detach();
}

fn quit_requested(cx: &mut App) {
    if !crate::desktop::draft_lifecycle::retain_window_drafts(cx) {
        cx.reply_to_quit(false).expect("pending AppKit quit request");
        return;
    }
    if crate::desktop::drafts::defer_quit(cx) {
        return;
    }
    if crate::ui::desktop::zoom::defer_quit(cx) {
        return;
    }
    let updates = cx.global_mut::<Updates>();
    if updates.quitting {
        return;
    }
    if updates.prepared.is_none() {
        cx.reply_to_quit(true).expect("pending AppKit quit request");
        return;
    }
    updates.quitting = true;
    let installation = updates.installation.clone().expect("prepared update has an installation");
    let reopen = updates.reopen;
    let handoff = cx.background_executor().spawn(async move {
        install::handoff(&installation, reopen).map_err(|error| error.to_string())
    });
    cx.spawn(async move |cx| {
        let result = handoff.await;
        let _ = cx.update(|cx| {
            let allowed = result.is_ok();
            if let Err(error) = result {
                let updates = cx.global_mut::<Updates>();
                updates.quitting = false;
                updates.state = State::Ready(
                    updates.prepared.clone().expect("handoff requires a prepared update"),
                );
                failed(error, cx);
                present(cx);
            }
            cx.reply_to_quit(allowed).expect("pending AppKit quit request");
        });
    })
    .detach();
}

fn initialize() -> Result<Response> {
    let Some(installation) = Installation::for_executable(&std::env::current_exe()?)? else {
        return Ok(Response::Initialized(None, None, None));
    };
    installation.open()?;
    package::verify(&installation.app, &installation::ReleaseIdentity::running())?;
    let operation = installation.try_operation()?;
    let mut journal = install::Journal::read(&installation, crate::engine::profile::current().1)?;
    if operation.is_some()
        && let Some(journal) = &mut journal
    {
        install::recover(&installation, journal)?;
    }
    let ready = journal
        .filter(|journal| {
            journal.phase == install::Phase::Ready || journal.phase == install::Phase::Swapping
        })
        .map(|journal| journal.to.to_string());
    let error_file = installation.state.join("error.txt");
    let error = if operation.is_some() && error_file.exists() {
        let error = String::from_utf8(installation::read(&error_file, 8192)?)?;
        std::fs::remove_file(&error_file)?;
        Some(error)
    } else {
        None
    };
    Ok(Response::Initialized(Some(installation), ready, error))
}

fn initialized(
    installation: Option<Installation>,
    ready: Option<String>,
    error: Option<String>,
    cx: &mut App,
) {
    watch(cx);
    let updates = cx.global_mut::<Updates>();
    updates.installation = installation;
    updates.prepared = ready.clone();
    updates.state = ready.clone().map(State::Ready).unwrap_or(State::Idle);
    if let Some(error) = error {
        updates.explicit = true;
        updates.state = State::Failed(error);
        present(cx);
    } else {
        check(updates.explicit, cx);
    }
}
