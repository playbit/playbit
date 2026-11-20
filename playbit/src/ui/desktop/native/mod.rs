//! Native Mac hosts share one semantic renderer and serialized app callbacks.
//! Connected native embeddings add an independent network owner. Process guests
//! run their local Wasm VM here and use the shared engine's data services.
#![cfg_attr(
    all(target_os = "macos", feature = "macos-shared-runtime"),
    allow(
        dead_code,
        reason = "The shared GUI omits separate SDK/provider C exports; their unused Rust support is removed by the linker."
    )
)]
use crate::{engine::Services, store::Id};
use gpui::{App, Application};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};
#[cfg(feature = "process")]
mod control;
#[cfg(target_os = "macos")]
mod ffi;
#[cfg(feature = "connected-engine")]
mod network;
#[cfg(feature = "connected-engine")]
pub mod portable;
pub(crate) mod window;
mod worker;
const MAX_LOG_LINES: usize = 256;
const MAX_LOG_BYTES: usize = 64 * 1024;

pub struct Launch {
    pub title: String,
    pub root: Id,
    pub app: playbit_api::abi::operations::App,
}
/// Run a trusted native composition. Preparation and all app callbacks execute
/// on the worker, never on AppKit's main thread. Preparation may provision a
/// local fixture; portable app provisioning is supplied by the app launcher.
///
/// # Safety
/// Every returned descriptor obeys playbit/sys/abi.h. Callback code and borrowed
/// user data outlive this function, including worker destruction on application
/// quit. Native apps cooperate and cannot unwind through C callbacks.
#[allow(unsafe_code, reason = "The embedding host explicitly owns trusted app lifetimes.")]
pub unsafe fn run(
    profile: PathBuf,
    prepare: impl FnOnce(&Services) -> Result<Vec<Launch>, String> + Send + 'static,
) -> std::io::Result<()> {
    crate::engine::profile::initialize_at(&profile)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Err(error) = crate::trace::initialize(4, &[]) {
        crate::diagnostic!("Playbit tracing unavailable: {error}");
    }
    crate::trace::mark("app.main");
    // SAFETY: run's caller retains callback code/data until the worker is joined.
    let worker = Rc::new(RefCell::new(unsafe { worker::Worker::start(profile, prepare)? }));
    present(worker, true)
}

/// A compiled app bound to an exact retained session and distributable package.
#[cfg(feature = "connected-engine")]
pub struct SessionLaunch {
    pub session: String,
    pub package: crate::package::Package,
    pub app: playbit_api::abi::operations::App,
}
// SAFETY: run_connected requires callback code and user data to remain valid
// across preparation, app ownership and both worker joins.
#[cfg(feature = "connected-engine")]
#[allow(unsafe_code, reason = "The embedding host owns trusted callback lifetimes.")]
unsafe impl Send for SessionLaunch {}

/// Run native apps with in-process authentication and AP synchronization. The
/// preparation callback enrolls/selects retained sessions on the network worker.
/// Local app turns execute on a separate owner; reconnect cannot stall input.
///
/// # Safety
/// Descriptors obey run's lifetime contract across both workers. Preparation
/// must return only callbacks implementing each supplied exact package.
#[cfg(feature = "connected-engine")]
#[allow(unsafe_code, reason = "The embedding host owns trusted callback lifetimes.")]
pub unsafe fn run_connected(
    profile: PathBuf,
    prepare: impl FnOnce(&mut crate::engine::connected::Connected) -> Result<Vec<SessionLaunch>, String>
    + Send
    + 'static,
) -> std::io::Result<()> {
    crate::engine::profile::initialize_at(&profile)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Err(error) = crate::trace::initialize(4, &[]) {
        crate::diagnostic!("Playbit tracing unavailable: {error}");
    }
    crate::trace::mark("app.main");
    // SAFETY: the caller retains descriptors through both worker joins.
    let worker =
        Rc::new(RefCell::new(unsafe { worker::Worker::start_connected(profile, prepare)? }));
    present(worker, true)
}

/// Run a portable native app with account and session setup in the Mac UI.
///
/// # Safety
/// The descriptor implements the exact supplied package and obeys run's
/// callback lifetime contract until application termination or early return.
#[cfg(feature = "connected-engine")]
#[allow(unsafe_code, reason = "The host owns trusted callback lifetimes.")]
pub unsafe fn run_portable(profile: PathBuf, app: portable::Portable) -> std::io::Result<()> {
    crate::engine::profile::initialize_at(&profile)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Err(error) = crate::trace::initialize(4, &[]) {
        crate::diagnostic!("Playbit tracing unavailable: {error}");
    }
    crate::trace::mark("app.main");
    // SAFETY: the caller retains the descriptor through both worker joins.
    let worker = Rc::new(RefCell::new(unsafe {
        worker::Worker::start_with_setup(profile, move |connected, reset, ui| {
            app.prepare(connected, reset, ui)
        })?
    }));
    present(worker, true)
}

fn present(worker: Rc<RefCell<worker::Worker>>, focus_window: bool) -> std::io::Result<()> {
    let outputs = worker.borrow().output();
    let application_worker = worker.clone();
    let windows = Rc::new(RefCell::new(window::Windows::default()));
    let reopened = windows.clone();
    #[cfg(feature = "desktop")]
    crate::desktop::launch_timing::appkit_start();
    let application = Application::new().with_assets(super::controls::Assets);
    application.on_reopen(move |cx| window::reopen(&mut reopened.borrow_mut(), cx));
    application.run(move |cx: &mut App| {
        #[cfg(feature = "desktop")]
        crate::desktop::launch_timing::appkit_ready();
        #[cfg(target_os = "macos")]
        super::appearance::set_light_appearance();
        #[cfg(all(target_os = "macos", feature = "desktop"))]
        let mut icon = crate::desktop::guest_app::icon::Observer::new();
        #[cfg(all(target_os = "macos", feature = "desktop"))]
        cx.on_app_quit(move |_| {
            drop(icon.take());
            async {}
        })
        .detach();
        let owner = application_worker.clone();
        let acknowledged = Rc::new(Cell::new(false));
        let quit_acknowledged = acknowledged.clone();
        let terminal = Rc::new(RefCell::new(None::<Result<(), super::alert::Failure>>));
        let quitting_windows = windows.clone();
        cx.on_quit_requested(move |cx| {
            if super::zoom::defer_quit(cx) {
                return;
            }
            if quit_acknowledged.get() {
                cx.reply_to_quit(true).expect("AppKit owns this pending termination request");
                return;
            }
            if let Some(Err(error)) = &*terminal.borrow() {
                window::show_failure(error.clone(), quit_acknowledged.clone(), focus_window, cx);
                cx.reply_to_quit(false).expect("AppKit owns this pending termination request");
                return;
            }
            window::deactivate(&quitting_windows.borrow(), cx);
            cx.deactivate().expect("macOS can yield activation without hiding drain status");
            owner.borrow().request_stop();
            let completion = owner.borrow().completion();
            let terminal = terminal.clone();
            let acknowledged = quit_acknowledged.clone();
            cx.spawn(async move |cx| {
                let result = completion.recv().await.unwrap_or_else(|_| {
                    Err("The app owner stopped without confirming its accepted work.".into())
                });
                let _ = cx.update(|cx| {
                    let drained = result.is_ok();
                    if let Err(error) = &result {
                        window::show_failure(error.clone(), acknowledged, focus_window, cx);
                    }
                    *terminal.borrow_mut() = Some(result);
                    cx.reply_to_quit(drained)
                        .expect("AppKit owns this pending termination request");
                });
            })
            .detach();
        })
        .expect("macOS provides deferred application termination");
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(super::UI_FONT_DATA)])
            .expect("bundled control font must load");
        super::text_editor::install_keybindings(cx);
        window::install_menus(cx);
        window::receive(outputs, acknowledged, windows, focus_window, cx);
        if focus_window {
            cx.activate(true);
        }
    });
    worker.borrow_mut().shutdown();
    Ok(())
}

#[cfg(feature = "process")]
pub fn run_guest(
    profile: PathBuf,
    engine: PathBuf,
    focus_window: bool,
    fuel_limit: Option<std::num::NonZeroU64>,
    resolve: impl FnOnce() -> Result<Option<crate::engine::session_instance::SessionInstance>, String>
    + Send
    + 'static,
) -> std::io::Result<()> {
    crate::trace::mark("app.main");
    let worker =
        Rc::new(RefCell::new(worker::Worker::start_guest(profile, engine, fuel_limit, resolve)?));
    present(worker, focus_window)
}
