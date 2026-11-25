//! Bounded native diagnostics and feedback. Only failures requiring attention interrupt input.
#![allow(unsafe_code, reason = "NSAlert is created, run and released on AppKit's main thread.")]
use gpui::{App, Global};
#[cfg(not(test))]
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
use std::{collections::VecDeque, path::Path, sync::Arc};

mod origin;
pub(crate) use origin::Origin;

const MAX_PENDING: usize = 8;
const MAX_DETAILS: usize = 4096;
// Failure transport can retain a complete guest backtrace. Modal notices still
// use MAX_DETAILS independently; the app log ring supplies the report budget.
pub(super) const MAX_FAILURE_BYTES: usize = 64 * 1024;
const FAILURE_TRUNCATED: &str = "\n[failure report truncated at 64 KiB]";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Recoverable,
    InputRejected,
    LocalDraft,
    DraftRetained,
    GuestLaunch,
    AppContextUnavailable,
    FinderOpen,
    Critical,
    EngineStartup,
    RecoverySlow,
}
impl Kind {
    fn modal_button(self) -> Option<&'static str> {
        match self {
            Self::Critical => Some("Quit"),
            Self::EngineStartup => Some("OK"),
            // A local draft can be the only copy of an unsaved edit.
            Self::LocalDraft => Some("Dismiss"),
            Self::Recoverable
            | Self::InputRejected
            | Self::DraftRetained
            | Self::GuestLaunch
            | Self::AppContextUnavailable
            | Self::FinderOpen
            | Self::RecoverySlow => None,
        }
    }
}
/// A fatal failure retains its detecting owner across UI and completion queues.
#[derive(Clone, Debug)]
pub(crate) struct Failure {
    pub details: String,
    pub origin: Origin,
}
impl From<String> for Failure {
    fn from(details: String) -> Self {
        Self { details: bounded_failure_details(&details), origin: Origin::capture() }
    }
}
impl From<&str> for Failure {
    fn from(details: &str) -> Self {
        details.to_owned().into()
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.details)
    }
}

struct Notice {
    kind: Kind,
    details: Arc<str>,
    feedback_window: Option<gpui::AnyWindowHandle>,
    coalescible: bool,
    acknowledged: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    logged: bool,
    origin: Option<Origin>,
}
struct Feedback {
    window: gpui::AnyWindowHandle,
    kind: Kind,
    details: Arc<str>,
}
#[derive(Default)]
struct Alerts {
    pending: VecDeque<Notice>,
    feedback: VecDeque<Feedback>,
    active_recoverable: Option<(Option<gpui::AnyWindowHandle>, Arc<str>)>,
    running: bool,
    overflow: usize,
    #[cfg(test)]
    presented: Vec<(Kind, String)>,
}
impl Global for Alerts {}

#[cfg(test)]
pub(crate) fn take_presented(cx: &mut App) -> Vec<(Kind, String)> {
    if cx.has_global::<Alerts>() {
        std::mem::take(&mut cx.global_mut::<Alerts>().presented)
    } else {
        Vec::new()
    }
}

/// Known failures get recovery guidance; unclassified failures retain their cause.
fn profile_error(details: &str) -> bool {
    #[cfg(feature = "app-host")]
    {
        details == crate::engine::ipc::PROFILE_OPEN_ERROR
    }
    #[cfg(not(feature = "app-host"))]
    {
        let _ = details;
        false
    }
}
fn corrupt_error(details: &str) -> bool {
    #[cfg(feature = "app-host")]
    {
        details == crate::engine::ipc::CORRUPT_STORE_ERROR
    }
    #[cfg(not(feature = "app-host"))]
    {
        let _ = details;
        false
    }
}

fn unsupported_volume_format(details: &str) -> bool {
    #[cfg(feature = "app-host")]
    {
        details == crate::store::Error::UNSUPPORTED_VOLUME_FORMAT
    }
    #[cfg(not(feature = "app-host"))]
    {
        let _ = details;
        false
    }
}

fn incompatible_service(details: &str) -> bool {
    #[cfg(feature = "app-host")]
    {
        details == crate::network::ap::service::INCOMPATIBLE_PROTOCOL
    }
    #[cfg(not(feature = "app-host"))]
    {
        let _ = details;
        false
    }
}

fn shared_service_upgrade_required(details: &str) -> bool {
    #[cfg(feature = "app-host")]
    {
        details == crate::network::ap::service::SHARED_SERVICE_UPGRADE_REQUIRED
    }
    #[cfg(not(feature = "app-host"))]
    {
        let _ = details;
        false
    }
}

fn package_upgrade_required(details: &str) -> bool {
    #[cfg(feature = "app-host")]
    {
        details == crate::package::codec::UPGRADE_REQUIRED
    }
    #[cfg(not(feature = "app-host"))]
    {
        let _ = details;
        false
    }
}

pub fn message(kind: Kind, details: &str, profile: &Path) -> (&'static str, String) {
    #[cfg(feature = "app-host")]
    if kind == Kind::Recoverable
        && let Some((scope, cause)) = crate::engine::remote::sync_failure_parts(details)
    {
        // Labels and diagnostic paths can themselves contain words like “credentials”.
        let reason = cause.lines().next().unwrap_or(cause);
        let (title, guidance) = unscoped_message(kind, reason, profile);
        return (title, format!("{scope}\n{guidance}"));
    }
    unscoped_message(kind, details, profile)
}

fn unscoped_message(kind: Kind, details: &str, profile: &Path) -> (&'static str, String) {
    match kind {
        Kind::DraftRetained => ("Local draft retained", bounded(details, MAX_DETAILS).into()),
        Kind::Recoverable if details.starts_with("Couldn’t save UI zoom.") => {
            ("Zoom setting couldn’t be saved", "Zoom wasn’t saved. Retry zoom or Quit.".into())
        }
        Kind::InputRejected => ("Input wasn’t delivered", bounded(details, MAX_DETAILS).into()),
        Kind::AppContextUnavailable => (
            "This app needs to be reopened",
            "This app can no longer use its data. \
             Close this window, then reopen the session from your library after access or \
             synchronization has recovered. Earlier accepted changes may have been saved; \
             no actions were repeated. (#1020)"
                .into(),
        ),
        Kind::RecoverySlow => (
            "Playbit is still recovering",
            "Playbit is reconnecting to its background process. Please wait a moment. (#1003)"
                .into(),
        ),
        #[cfg(feature = "process")]
        Kind::EngineStartup if details == crate::engine::launch_agent::START_TIMEOUT_ERROR => (
            "Playbit couldn’t start its background engine",
            "The background service did not respond before startup timed out. \
             Check that Playbit is allowed in System Settings > General > Login Items & Extensions. \
             Then quit and reopen Playbit. (#1021)"
                .into(),
        ),
        Kind::EngineStartup => (
            "Playbit couldn’t start its background engine",
            "Quit and reopen Playbit to try again. (#1021)".into(),
        ),
        #[cfg(feature = "app-host")]
        Kind::Critical if details == crate::engine::ipc::INCOMPATIBLE_ENGINE => (
            "Playbit can’t use the running background engine",
            "The background engine is from an incompatible Playbit build. \
             Reopen Playbit after that engine has stopped. (#1022)"
                .into(),
        ),
        Kind::Critical => (
            "Playbit can’t continue",
            if profile_error(details) {
                "Playbit couldn’t open your local profile. \
                 Quit Playbit and check the profile’s logs for details. \
                 Your local data has been kept. (#1002)"
                    .into()
            } else if corrupt_error(details) {
                "Playbit can’t open your saved data because the local store is damaged. \
                 Quit Playbit. Your data has been kept for recovery. (#1001)"
                    .into()
            } else if details == "The Playbit installation is incomplete. Reinstall Playbit." {
                "Playbit’s installation is incomplete. Quit Playbit and reinstall it. (#1011)"
                    .into()
            } else {
                "Playbit encountered an unrecoverable error. Quit and reopen Playbit. \
                 Your last action may already have been saved; check before repeating it. \
                 (#1000)"
                    .into()
            },
        ),
        Kind::Recoverable | Kind::GuestLaunch if package_upgrade_required(details) => {
            ("This app needs to be rebuilt", format!("{details} (#1015)"))
        }
        #[cfg(feature = "connected-engine")]
        Kind::Recoverable | Kind::GuestLaunch
            if details == crate::engine::remote::SESSION_BUILD_UNAVAILABLE =>
        {
            ("This session’s app build is unavailable", format!("{details} (#1016)"))
        }
        Kind::Recoverable if shared_service_upgrade_required(details) => {
            ("This session needs an updated sync service", format!("{details} (#1014)"))
        }
        Kind::Recoverable if incompatible_service(details) => {
            ("Sync is unavailable in this version of Playbit", format!("{details} (#1014)"))
        }
        Kind::Recoverable if unsupported_volume_format(details) => (
            "This session can’t be opened in this version of Playbit",
            format!("{details} (#1013)"),
        ),
        Kind::Recoverable if details == "invalid account record fields" => (
            "Playbit couldn’t read your account data",
            format!(
                "A saved account record has an invalid format. Account synchronization couldn’t finish.\n\n{} (#1012)",
                log_hint(profile)
            ),
        ),
        Kind::Recoverable
            if matches!(
                details,
                "waiting for initial database synchronization"
                    | "waiting for initial volume synchronization"
            ) =>
        {
            (
                "This data hasn’t finished synchronizing",
                "Wait for synchronization to finish, then try again. (#1017)".into(),
            )
        }
        Kind::Recoverable
            if matches!(
                details,
                "access denied: current authority requires authenticated catch-up"
                    | "access denied: entry authority requires authenticated catch-up"
                    | "access denied: dependency authority requires authenticated catch-up"
            ) =>
        {
            (
                "This data’s permissions need to be refreshed",
                "Wait for permission updates to finish synchronizing, then try again. (#1018)"
                    .into(),
            )
        }
        Kind::Recoverable
            if details
                .strip_prefix("Couldn’t copy invitation: ")
                .or_else(|| details.strip_prefix("Couldn’t copy session link: "))
                .unwrap_or(details)
                == "access denied: sharing new app sessions requires encrypted synchronization, which is not supported yet" =>
        {
            (
                "This session can’t be shared yet",
                "Sharing new app sessions isn’t supported in this version of Playbit. (#1019)"
                    .into(),
            )
        }
        Kind::Recoverable => {
            ("Playbit couldn’t complete this action", recoverable_message(details))
        }
        Kind::LocalDraft => ("Local changes need attention", bounded(details, MAX_DETAILS).into()),
        Kind::GuestLaunch => (
            "Playbit couldn’t open this app",
            format!(
                "{}\n\nAfter resolving this, select the session in Sessions to open it again.",
                bounded(details, MAX_DETAILS)
            ),
        ),
        Kind::FinderOpen => {
            ("Playbit couldn’t open this volume in Finder", bounded(details, MAX_DETAILS).into())
        }
    }
}

fn recoverable_message(details: &str) -> String {
    let guidance = if details.contains("diagnostics") {
        "Playbit couldn’t open diagnostics for this app. You can keep using the app. (#1009)"
    } else if details.contains("authenticated")
        || details.contains("credentials")
        || details.contains("sign-in")
    {
        "Playbit couldn’t connect to your account. Sign in again to retry. (#1006)"
    } else if details.contains("AccessDenied")
        || details.contains("permission")
        || details.contains("denied")
    {
        "Playbit couldn’t access the data for this action. Check the app’s volume access, then try again. (#1007)"
    } else if details.contains("busy") || details.contains("restarting") {
        "Playbit is busy. Wait a moment, then try again. This action did not start. (#1005)"
    } else if details.contains("invitation") || details.contains("session link") {
        "Playbit couldn’t open this session link. Check the link and try again. (#1008)"
    } else {
        return "Playbit couldn’t finish this action. \
                Your change may already have been saved; check before trying again. (#1004)"
            .into();
    };
    guidance.into()
}

fn log_hint(profile: &Path) -> String {
    let path = match crate::paths::Paths::diagnostic_log(profile) {
        Ok(path) => path,
        Err(error) => return format!("The diagnostic log path is unavailable: {error}."),
    };
    let shortened = dirs::home_dir().and_then(|home| {
        path.strip_prefix(home).ok().map(|relative| Path::new("~").join(relative))
    });
    format!("See {} for details.", shortened.as_deref().unwrap_or(&path).display())
}

fn bounded(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn bounded_failure_details(details: &str) -> String {
    if details.len() <= MAX_FAILURE_BYTES {
        return details.to_owned();
    }
    let mut report = bounded(details, MAX_FAILURE_BYTES - FAILURE_TRUNCATED.len()).to_owned();
    report.push_str(FAILURE_TRUNCATED);
    report
}
pub fn show(kind: Kind, details: &str, cx: &mut App) {
    let origin = (kind == Kind::Critical).then(Origin::capture);
    show_with_origin(kind, details, origin, cx);
}

pub(crate) fn show_with_origin(kind: Kind, details: &str, origin: Option<Origin>, cx: &mut App) {
    enqueue(kind, details, origin, None, cx.active_window(), cx);
}

/// Report an error discovered while rendering a particular window, even if
/// another window currently has keyboard focus.
pub(crate) fn show_in_window(
    kind: Kind,
    details: &str,
    window: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    enqueue(kind, details, None, None, Some(window), cx);
}

/// The originating guest window owns its inline feedback. Retain the same
/// bounded process diagnostic without delivering it to a second window.
pub(super) fn record_in_window(kind: Kind, details: &str, cx: &mut App) {
    assert!(kind.modal_button().is_none());
    enqueue(kind, details, None, None, None, cx);
}

pub(crate) fn fatal(
    failure: &Failure,
    acknowledged: std::rc::Rc<std::cell::Cell<bool>>,
    cx: &mut App,
) {
    enqueue(
        Kind::Critical,
        &failure.details,
        Some(failure.origin.clone()),
        Some(acknowledged),
        None,
        cx,
    );
}

fn enqueue(
    kind: Kind,
    details: &str,
    origin: Option<Origin>,
    acknowledged: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    feedback_window: Option<gpui::AnyWindowHandle>,
    cx: &mut App,
) {
    if !cx.has_global::<Alerts>() {
        cx.set_global(Alerts::default());
    }
    let coalescible =
        matches!(kind, Kind::Recoverable | Kind::InputRejected) && details.len() <= MAX_DETAILS;
    let details: Arc<str> = bounded(details, MAX_DETAILS).into();
    if let Some(window) = feedback_window.filter(|_| kind.modal_button().is_none()) {
        let feedback = &mut cx.global_mut::<Alerts>().feedback;
        let schedule = feedback.is_empty();
        feedback.retain(|bell| bell.window != window);
        if feedback.len() == MAX_PENDING {
            feedback.pop_front();
        }
        feedback.push_back(Feedback { window, kind, details: details.clone() });
        if schedule {
            // Exit the originating render borrow before updating any window.
            cx.defer(deliver_feedback);
        }
    }
    let alerts = cx.global_mut::<Alerts>();
    if coalescible
        && (alerts
            .active_recoverable
            .as_ref()
            .is_some_and(|(target, text)| *target == feedback_window && text == &details)
            || alerts.pending.iter().any(|notice| {
                notice.coalescible
                    && notice.feedback_window == feedback_window
                    && notice.details == details
            }))
    {
        return;
    }
    let notice =
        Notice { kind, feedback_window, details, coalescible, acknowledged, logged: false, origin };
    if alerts.pending.len() == MAX_PENDING {
        alerts.overflow = alerts.overflow.saturating_add(1);
        // A fatal error must never disappear behind recoverable notices.
        if kind == Kind::Critical {
            alerts.pending.pop_back();
        } else {
            return;
        }
    }
    if kind == Kind::Critical {
        alerts.pending.push_front(notice);
    } else {
        alerts.pending.push_back(notice);
    }
    let start = !alerts.running;
    alerts.running = true;
    if start {
        run_alerts(cx);
    }
}

fn deliver_feedback(cx: &mut App) {
    let feedback = std::mem::take(&mut cx.global_mut::<Alerts>().feedback);
    let profile = crate::engine::profile_path();
    for Feedback { window, kind, details } in feedback {
        let (_, body) = message(kind, &details, &profile);
        #[cfg(feature = "desktop")]
        crate::desktop::show_error_feedback(window, &body, cx);
        #[cfg(feature = "connected-engine")]
        if let Some(sign_in) = window.downcast::<super::sign_in::SignIn>() {
            let _ = sign_in.update(cx, |sign_in, _, cx| sign_in.show_feedback(&body, cx));
        }
        #[cfg(feature = "native-runtime")]
        if let Some(guest) = window.downcast::<super::native::window::AppWindow>() {
            let _ = guest.update(cx, |guest, _, cx| guest.show_feedback(&body, &details, cx));
        }
    }
}

fn run_alerts(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let next = cx.update(|cx| {
                let alerts = cx.global_mut::<Alerts>();
                let notice = alerts.pending.pop_front();
                // More failures can arrive while this notice is logged or presented.
                alerts.active_recoverable = notice
                    .as_ref()
                    .filter(|notice| notice.coalescible)
                    .map(|notice| (notice.feedback_window, notice.details.clone()));
                if notice.is_none() {
                    alerts.running = false;
                }
                let mut diagnostics = Vec::with_capacity(MAX_PENDING + 1);
                if let Some(notice) = &notice
                    && !notice.logged
                {
                    diagnostics.push((notice.kind, notice.details.clone(), notice.origin.clone()));
                }
                // Log the whole admitted batch before a critical dialog can quit.
                for notice in &mut alerts.pending {
                    if !notice.logged {
                        diagnostics.push((
                            notice.kind,
                            notice.details.clone(),
                            notice.origin.clone(),
                        ));
                        notice.logged = true;
                    }
                }
                (notice, diagnostics, std::mem::take(&mut alerts.overflow))
            });
            let Ok((Some(notice), diagnostics, overflow)) = next else { break };
            let profile = crate::engine::profile_path();
            let log_profile = profile.clone();
            cx.background_executor()
                .spawn(async move {
                    for (kind, details, origin) in diagnostics {
                        log_notice(&log_profile, kind, &details, overflow, origin.as_ref());
                    }
                })
                .await;
            if let Some(_button) = notice.kind.modal_button() {
                #[cfg(not(test))]
                {
                    let (title, body) = message(notice.kind, &notice.details, &profile);
                    run_modal(title, &body, _button);
                }
            }
            #[cfg(test)]
            let _ = cx.update(|cx| {
                cx.global_mut::<Alerts>().presented.push((notice.kind, notice.details.to_string()));
            });
            if notice.kind == Kind::Critical {
                if let Some(acknowledged) = notice.acknowledged {
                    acknowledged.set(true);
                }
                let _ = cx.update(|cx| {
                    cx.global_mut::<Alerts>().running = false;
                    #[cfg(not(test))]
                    cx.quit();
                });
                break;
            }
        }
    })
    .detach();
}

fn log_notice(profile: &Path, kind: Kind, details: &str, overflow: usize, origin: Option<&Origin>) {
    let (title, body) = message(kind, details, profile);
    // Preserve the cause before display text can exhaust the diagnostic byte limit.
    crate::platform::failure::diagnostic(
        profile,
        "native alert",
        &format!("kind={kind:?} additional_errors={overflow}\ndetails={details}\n{title}\n{body}"),
        origin.map(|origin| origin as &dyn std::fmt::Display),
    );
}

#[cfg(not(test))]
fn run_modal(title: &str, body: &str, button: &str) {
    // No App/Context/Entity borrow survives this reentrant native event loop.
    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
        let alert: *mut Object = msg_send![class!(NSAlert), new];
        let panel: *mut Object = msg_send![alert, window];
        let _: () = msg_send![panel, setAppearance: super::appearance::aqua()];
        for (text, role) in [(title, 0), (body, 1), (button, 2)] {
            let text = std::ffi::CString::new(text.replace('\0', "�")).expect("NUL removed");
            let string: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()];
            match role {
                0 => {
                    let _: () = msg_send![alert, setMessageText: string];
                }
                1 => {
                    let _: () = msg_send![alert, setInformativeText: string];
                }
                _ => {
                    let _: *mut Object = msg_send![alert, addButtonWithTitle: string];
                }
            }
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let _: isize = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        let _: () = msg_send![pool, drain];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn full_diagnostic_queue_keeps_the_latest_bell_for_each_window(cx: &mut gpui::TestAppContext) {
        let first = cx.add_window(|_, _| gpui::EmptyView).into();
        let second = cx.add_window(|_, _| gpui::EmptyView).into();
        cx.update(|cx| {
            for index in 1..=9 {
                let window = if index % 2 == 0 { second } else { first };
                enqueue(
                    Kind::InputRejected,
                    &format!("Input {index}"),
                    None,
                    None,
                    Some(window),
                    cx,
                );
            }
            let alerts = cx.global::<Alerts>();
            assert_eq!(alerts.pending.len(), 8);
            assert_eq!(alerts.overflow, 1);
            let bells: Vec<_> =
                alerts.feedback.iter().map(|bell| (bell.window, &*bell.details)).collect();
            assert!(bells == [(second, "Input 8"), (first, "Input 9")]);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(cx.global::<Alerts>().feedback.is_empty());
            assert_eq!(take_presented(cx).len(), 8);
        });
    }

    #[gpui::test]
    fn repeated_recoverable_notices_coalesce_while_being_reported(cx: &mut gpui::TestAppContext) {
        let details = "access denied: current authority requires authenticated catch-up";
        cx.update(|cx| {
            show(Kind::Recoverable, details, cx);
            show(Kind::Recoverable, details, cx);
        });
        while !cx.update(|cx| cx.global::<Alerts>().active_recoverable.is_some()) {
            assert!(cx.executor().tick(), "the admitted notice becomes active");
        }
        let prefix = "x".repeat(MAX_DETAILS);
        cx.update(|cx| {
            show(Kind::Recoverable, details, cx);
            for _ in 0..2 {
                show(Kind::Recoverable, "different failure", cx);
            }
            show(Kind::Recoverable, &format!("{prefix}a"), cx);
            show(Kind::Recoverable, &format!("{prefix}b"), cx);
            show(Kind::Recoverable, &prefix, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(take_presented),
            [details, "different failure", &prefix, &prefix, &prefix]
                .into_iter()
                .map(|details| (Kind::Recoverable, details.to_owned()))
                .collect::<Vec<_>>()
        );
        cx.update(|cx| show(Kind::Recoverable, details, cx));
        cx.run_until_parked();
        assert_eq!(cx.update(take_presented), vec![(Kind::Recoverable, details.into())]);

        // Critical acknowledgments belong to their individual detecting owners.
        let acknowledged = std::rc::Rc::new(std::cell::Cell::new(false));
        let other = std::rc::Rc::new(std::cell::Cell::new(false));
        cx.update(|cx| {
            enqueue(Kind::Critical, details, None, Some(acknowledged.clone()), None, cx);
            enqueue(Kind::Critical, details, None, Some(other.clone()), None, cx);
            assert_eq!(cx.global::<Alerts>().pending.len(), 2);
        });
        cx.run_until_parked();
        assert!(other.get(), "critical notices retain priority");
        assert!(!acknowledged.get(), "the other owner is not acknowledged by coalescing");
    }

    #[cfg(feature = "desktop")]
    #[inline(never)]
    fn originating_failure() -> Failure {
        format!("origin fixture: {}", "é".repeat(MAX_DETAILS)).into()
    }

    #[test]
    #[cfg(feature = "desktop")]
    fn critical_log_preserves_the_origin_after_its_thread_exits() {
        // The logging owner cannot reconstruct the failing thread's stack.
        // A child exercises the actual GUI stderr sink without replacing the
        // test runner's global logger. Delivery and cloning retain one origin.
        const CHILD: &str = "PLAYBIT_ALERT_ORIGIN_PROFILE";
        if let Some(profile) = std::env::var_os(CHILD) {
            let profile = Path::new(&profile);
            crate::desktop::process_log::initialize(profile).expect("GUI logger");
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || send.send(originating_failure()).expect("failure receiver"))
                .join()
                .expect("originating owner exited");
            let failure = receive.recv().expect("originating failure");
            let clone = failure.clone();
            assert_eq!(failure.origin.to_string(), clone.origin.to_string());
            assert_eq!(failure.details, format!("origin fixture: {}", "é".repeat(MAX_DETAILS)));
            log_notice(
                profile,
                Kind::Critical,
                bounded(&clone.details, MAX_DETAILS),
                2,
                Some(&clone.origin),
            );
            return;
        }
        let profile = tempfile::tempdir().expect("isolated diagnostic profile");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "ui::desktop::alert::tests::critical_log_preserves_the_origin_after_its_\
                 thread_exits",
                "--nocapture",
            ])
            .env(CHILD, profile.path())
            .env_remove("RUST_BACKTRACE")
            .env_remove("RUST_LIB_BACKTRACE")
            .output()
            .expect("origin log child");
        assert!(output.status.success(), "{output:?}");
        let path = profile.path().join("logs/playbit-gui.log");
        let report = std::fs::read_to_string(path).expect("critical diagnostic");
        assert!(report.len() <= crate::trace::diagnostic::REPORT_BYTES);
        assert!(report.lines().all(|line| line.starts_with('[')));
        assert!(report.contains("additional_errors=2"));
        assert!(report.contains("details=origin fixture: é"));
        assert!(report.contains("Error origin backtrace:"));
        assert!(report.contains("originating_failure"), "{report}");
    }

    #[test]
    fn guest_launch_preserves_concrete_recovery_instructions() {
        // Cache recovery needs its exact generated path, and uncertain dispatch
        // needs inspection guidance instead of the generic action error.
        for detail in [
            "Move the generated app bundle aside: /profile/apps/pinball.0123456789abcdef/Pinball.app",
            "Launch Services did not confirm instance 123; inspect it before another launch",
        ] {
            let (title, body) = message(Kind::GuestLaunch, detail, Path::new("/profile"));
            assert_eq!(title, "Playbit couldn’t open this app");
            assert!(body.contains(detail));
            assert!(body.contains("After resolving this"));
        }
        assert_eq!(Kind::GuestLaunch.modal_button(), None);
    }

    #[test]
    fn finder_open_preserves_recovery_instructions_with_bounded_details() {
        // Finder errors retain their actual recovery action and limit text at a UTF-8 boundary.
        let instructions = "This account’s Playbit drive is unavailable in Finder. Enable its File Provider location, then try again.";
        for details in
            [instructions, "Finder drives require the OS user's default Public or Testing profile"]
        {
            let (title, body) = message(Kind::FinderOpen, details, Path::new("/profile"));
            assert_eq!(title, "Playbit couldn’t open this volume in Finder");
            assert_eq!(body, details);
        }
        let details = format!("{instructions} {}", "🌱".repeat(MAX_DETAILS));
        let (_, body) = message(Kind::FinderOpen, &details, Path::new("/profile"));
        assert!(body.len() <= MAX_DETAILS);
        assert!(body.starts_with(instructions));
        assert!(body.ends_with("🌱"));
        assert_eq!(Kind::FinderOpen.modal_button(), None);
    }

    #[test]
    #[cfg(feature = "app-host")]
    fn unsupported_volume_explains_compatibility_and_preserves_data() {
        let details = crate::store::Error::UNSUPPORTED_VOLUME_FORMAT;
        let profile = tempfile::tempdir().expect("isolated diagnostic profile");
        let (title, body) = message(Kind::Recoverable, details, profile.path());
        assert_eq!(title, "This session can’t be opened in this version of Playbit");
        assert_eq!(body, format!("{details} (#1013)"));
        log_notice(profile.path(), Kind::Recoverable, details, 0, None);
        let path = crate::paths::Paths::diagnostic_log(profile.path()).expect("log path");
        assert!(std::fs::read_to_string(path).expect("diagnostic").contains(details));
    }

    #[test]
    #[cfg(feature = "app-host")]
    fn incompatible_service_explains_sync_without_suggesting_account_reset() {
        for (details, expected_title) in [
            (
                crate::network::ap::service::INCOMPATIBLE_PROTOCOL,
                "Sync is unavailable in this version of Playbit",
            ),
            (
                crate::network::ap::service::SHARED_SERVICE_UPGRADE_REQUIRED,
                "This session needs an updated sync service",
            ),
        ] {
            let (title, body) = message(Kind::Recoverable, details, Path::new("/profile"));
            assert_eq!(title, expected_title);
            assert_eq!(body, format!("{details} (#1014)"));
        }
    }

    #[test]
    #[cfg(feature = "app-host")]
    fn retained_package_failure_explains_rebuild_and_kept_data() {
        let details = crate::package::codec::UPGRADE_REQUIRED;
        for kind in [Kind::Recoverable, Kind::GuestLaunch] {
            let (title, body) = message(kind, details, Path::new("/profile"));
            assert_eq!(title, "This app needs to be rebuilt");
            assert_eq!(body, format!("{details} (#1015)"));
            assert_eq!(kind.modal_button(), None);
        }
    }

    #[test]
    #[cfg(feature = "app-host")]
    fn scoped_sync_failure_keeps_its_target_and_classifies_only_the_cause() {
        let scope = "Couldn’t sync database \"credentials\" in volume \"Shared\"";
        for (cause, code) in [
            ("waiting for initial database synchronization", "#1017"),
            ("access denied: volume permission\nProfile: /profiles/credentials/.playbit", "#1007"),
        ] {
            let (_, body) =
                message(Kind::Recoverable, &format!("{scope}\n{cause}"), Path::new("/profile"));
            assert!(body.starts_with(scope));
            assert!(body.contains(code));
        }
    }

    #[test]
    fn technical_errors_explain_the_next_user_action() {
        // Known errors offer recovery guidance; internal causes stay in the log.
        let (title, body) =
            message(Kind::AppContextUnavailable, "context stopped", Path::new("/profile"));
        assert_eq!(title, "This app needs to be reopened");
        assert_eq!(
            body,
            "This app can no longer use its data. Close this window, then reopen the session from your library after access or synchronization has recovered. Earlier accepted changes may have been saved; no actions were repeated. (#1020)"
        );
        assert_eq!(Kind::AppContextUnavailable.modal_button(), None);
        // Pending initial synchronization reports readiness without claiming an uncertain write.
        for details in [
            "waiting for initial database synchronization",
            "waiting for initial volume synchronization",
        ] {
            #[cfg(feature = "app-host")]
            let details = &crate::engine::failure::message(
                Path::new("/profile"),
                &crate::store::Error::System(details.into()),
            );
            let (title, body) = message(Kind::Recoverable, details, Path::new("/profile"));
            assert_eq!(title, "This data hasn’t finished synchronizing");
            assert_eq!(body, "Wait for synchronization to finish, then try again. (#1017)");
        }
        for details in [
            "access denied: current authority requires authenticated catch-up",
            "access denied: entry authority requires authenticated catch-up",
            "access denied: dependency authority requires authenticated catch-up",
        ] {
            let (title, body) = message(Kind::Recoverable, details, Path::new("/profile"));
            assert_eq!(title, "This data’s permissions need to be refreshed");
            assert_eq!(
                body,
                "Wait for permission updates to finish synchronizing, then try again. (#1018)"
            );
        }
        for prefix in ["", "Couldn’t copy invitation: ", "Couldn’t copy session link: "] {
            let details = format!(
                "{prefix}access denied: sharing new app sessions requires encrypted synchronization, which is not supported yet"
            );
            let (title, body) = message(Kind::Recoverable, &details, Path::new("/profile"));
            assert_eq!(title, "This session can’t be shared yet");
            assert_eq!(
                body,
                "Sharing new app sessions isn’t supported in this version of Playbit. (#1019)"
            );
        }
        let (_, message) = message(
            Kind::Recoverable,
            "AP requires a newly issued authenticated instance identity",
            Path::new("/profile"),
        );
        assert!(message.contains("Sign in again"));
        assert!(!message.contains("AP"));
        assert!(!message.contains("instance identity"));
        let (_, message) =
            super::message(Kind::Recoverable, "failed to fill whole buffer", Path::new("/profile"));
        assert!(message.contains("check before trying again"));
        assert!(!message.contains("failed to fill whole buffer"));
        assert!(!message.contains("/profile"));
    }
    #[test]
    fn account_record_failure_names_the_failed_operation_and_logs_its_cause() {
        // Exercise the screenshot's failure through the production text and log sink.
        let details = "invalid account record fields";
        let profile = tempfile::tempdir().expect("isolated diagnostic profile");
        let (title, body) = message(Kind::Recoverable, details, profile.path());
        assert_eq!(title, "Playbit couldn’t read your account data");
        assert!(body.contains("Account synchronization couldn’t finish"));
        assert!(body.contains("invalid format"));
        assert!(body.contains("logs/engine.log"));

        log_notice(profile.path(), Kind::Recoverable, details, 0, None);
        let path = crate::paths::Paths::diagnostic_log(profile.path()).expect("log path");
        let report = std::fs::read_to_string(path).expect("alert diagnostic");
        assert!(report.contains("native alert: "));
        assert!(report.contains(&format!("details={details}")));
        assert!(report.contains(title));
    }
    #[test]
    fn long_alert_causes_precede_display_text_in_the_bounded_log() {
        // A long fallback body must not displace the original cause in the log.
        let details = format!("fixture failure: {}", "é".repeat(MAX_DETAILS));
        let profile = tempfile::tempdir().expect("isolated diagnostic profile");
        log_notice(profile.path(), Kind::Recoverable, bounded(&details, MAX_DETAILS), 2, None);
        let path = crate::paths::Paths::diagnostic_log(profile.path()).expect("log path");
        let report = std::fs::read_to_string(path).expect("alert diagnostic");
        assert!(report.len() <= 8192);
        assert!(report.contains("additional_errors=2"));
        assert!(report.contains("] details=fixture failure: é"));
    }
    #[test]
    fn log_hints_use_the_selected_profile_and_shorten_only_the_home_directory() {
        // Default and custom profiles name their actual log, with component-aware shortening.
        let home = dirs::home_dir().expect("home directory");
        let sibling = home.with_file_name(format!(
            "{}-other",
            home.file_name().expect("home directory name").to_string_lossy()
        ));
        for (profile, expected) in [
            (home.join(".playbit"), "~/.playbit/logs/engine.log".to_owned()),
            (home.join("Playbit Profile"), "~/Playbit Profile/logs/engine.log".to_owned()),
            (
                Path::new("/Volumes/Playbit/profile").to_owned(),
                "/Volumes/Playbit/profile/logs/engine.log".to_owned(),
            ),
            (sibling.clone(), format!("{}/logs/engine.log", sibling.display())),
        ] {
            let (_, body) = message(Kind::Recoverable, "invalid account record fields", &profile);
            assert!(body.contains(&format!("See {expected} for details.")));
        }
    }
    #[test]
    fn diagnostic_text_is_bounded_in_bytes_without_breaking_utf8() {
        let details = "🌱".repeat(MAX_DETAILS);
        assert_eq!(bounded(&details, MAX_DETAILS).len(), MAX_DETAILS);
        assert_eq!(bounded("🌱x", 3), "");
    }
    #[test]
    fn errors_offer_only_the_action_appropriate_to_their_severity() {
        assert_eq!(Kind::Critical.modal_button(), Some("Quit"));
        assert_eq!(Kind::EngineStartup.modal_button(), Some("OK"));
        assert_eq!(Kind::LocalDraft.modal_button(), Some("Dismiss"));
        #[cfg(feature = "app-host")]
        {
            let (title, body) = message(
                Kind::Critical,
                crate::engine::ipc::INCOMPATIBLE_ENGINE,
                Path::new("/profile"),
            );
            assert_eq!(title, "Playbit can’t use the running background engine");
            assert!(body.contains("incompatible Playbit build"));
        }
        for kind in [
            Kind::Recoverable,
            Kind::InputRejected,
            Kind::RecoverySlow,
            Kind::FinderOpen,
            Kind::AppContextUnavailable,
        ] {
            assert_eq!(kind.modal_button(), None);
        }
        assert!(
            message(
                Kind::Critical,
                "assertion failed: secret implementation detail",
                Path::new("/profile")
            )
            .1
            .contains("Quit and reopen")
        );
        assert!(
            !message(
                Kind::Critical,
                "assertion failed: secret implementation detail",
                Path::new("/profile")
            )
            .1
            .contains("assertion")
        );
        assert!(
            message(Kind::RecoverySlow, "connection refused", Path::new("/profile"))
                .1
                .contains("reconnecting to its background process")
        );
    }
}
