//! The Playbit local engine: one Engine serving local requests until a stop
//! signal or endpoint loss, then teardown so its AP connection closes cleanly.
use crate::engine::{Engine, Result, Shutdown, admission::AdmissionQueue};
use std::{
    fs,
    io::Write,
    os::{
        fd::RawFd,
        unix::{fs::PermissionsExt, net::UnixListener},
    },
    path::Path,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
    time::{Duration, Instant},
};

mod endpoint;

static STOP: AtomicBool = AtomicBool::new(false);
/// A private duplicate of the Engine's owner-wake descriptor. It is never
/// closed, so the handler can never write into a descriptor number that
/// teardown has since closed and the process has reused.
static WAKE: AtomicI32 = AtomicI32::new(-1);
#[cfg(target_os = "linux")]
use libc::__errno_location as errno_location;
#[cfg(target_os = "macos")]
use libc::__error as errno_location;

#[allow(unsafe_code, reason = "The signal handler calls only async-signal-safe libc functions.")]
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
    // SAFETY: async-signal-safe calls only. The byte returns the owner loop
    // from a wait that may have no timeout. This can run between another
    // thread's failed syscall and its errno read, so errno is put back.
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        libc::write(WAKE.load(Ordering::Relaxed), [0u8].as_ptr().cast(), 1);
        *errno = saved;
    }
}
/// A stop signal ends the owner loop instead of the process: the replacing
/// engine instance sends SIGTERM, a terminal SIGINT. Neither may end us
/// before `Remote` has flushed its AP close.
#[allow(
    unsafe_code,
    reason = "Native signal installation borrows initialized structures and an owned descriptor."
)]
fn install_stop_signals(wake: RawFd) -> Result<()> {
    // SAFETY: plain libc calls on a live descriptor and an initialized action.
    unsafe {
        let wake = libc::dup(wake);
        if wake < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        WAKE.store(wake, Ordering::Relaxed);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = stop as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        for signal in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        // A launcher running on a dispatch worker can pass a blocked signal
        // mask to its child. Installing handlers does not unblock delivery.
        let mut signals = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGTERM);
        libc::sigaddset(&mut signals, libc::SIGINT);
        let status = libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
        if status != 0 {
            return Err(std::io::Error::from_raw_os_error(status).into());
        }
    }
    Ok(())
}
/// Serve a preconfigured isolated engine (used by embedding fixtures). The
/// executable uses `serve_with_ready` to reserve ownership before opening data.
pub fn serve(profile: &Path, engine: Engine) -> Result<()> {
    serve_with_ready(profile, || Ok(engine), || Ok(()))
}

/// Release startup-only resources once this process has a registered endpoint.
pub fn serve_with_ready(
    profile: &Path,
    open: impl FnOnce() -> Result<Engine>,
    ready: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // Reserve outside the profile before opening any store. A deleted profile
    // must not let a second engine bypass the original owner's lifetime lease.
    // Declaration order retains that lease through Engine teardown on errors.
    let mut recovery = super::recovery_endpoint::Endpoint::reserve(profile)?;
    let mut engine = open()?;
    // Product teardown only signals the recorder; slow diagnostic I/O must not
    // delay Engine shutdown. The detached collector retains its own locks.
    // Completeness requires an explicit seal, never this guard's drop order.
    let _recorder =
        crate::trace::Recorder::for_profile(profile, &(profile).join(crate::paths::TRACE));
    install_stop_signals(engine.wake_fd().expect("process owns Unix completion"))?;
    let path = profile.join(crate::paths::ENGINE_SOCKET);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let endpoint = endpoint::Endpoint::new(&path, profile)?;
    crate::diagnostic!("Playbit local engine: {}", profile.display());
    let mut requests = AdmissionQueue::new(listener, &engine)?;
    requests.watch_endpoint(&endpoint.changes)?;
    recovery.listen()?;
    #[cfg(target_os = "macos")]
    let provider = {
        let wake = engine.completion.wake.clone();
        super::file_provider::Server::start(
            profile,
            &engine.services,
            engine.network.blobs.clone(),
            move || wake.notify(),
        )?
    };
    crate::trace::mark("engine.ready");
    ready()?;
    #[cfg(target_os = "macos")]
    let _drives = super::file_provider::Coordinator::start(profile)?;
    if let Err(error) = std::io::stdout()
        .write_all(crate::engine::ipc::ENGINE_READY)
        .and_then(|_| std::io::stdout().flush())
    {
        // A closing/timed-out launcher does not cancel this initialized engine.
        crate::diagnostic!("Launch readiness receiver unavailable: {error}");
    }
    let start = Instant::now();
    let mut idle = super::idle::Idle::default();
    let mut outgoing = super::idle::Outgoing::default();
    while !STOP.load(Ordering::Acquire) {
        if requests.endpoint_changed() {
            match endpoint.lost() {
                Ok(false) => {}
                Ok(true) => {
                    crate::diagnostic!(
                        "Stopping because the local engine socket disappeared or changed"
                    );
                    break;
                }
                Err(error) => {
                    crate::diagnostic!(
                        "Stopping because the local engine socket cannot be watched: {error}"
                    );
                    break;
                }
            }
        }
        let (connected, generation) = requests.connections();
        let provider_busy = {
            #[cfg(target_os = "macos")]
            {
                provider.busy()
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        };
        let local_work = connected
            || provider_busy
            || engine.session_instances.active()
            || engine.tool_contexts.active()
            || (engine.service_executor.pending() || engine.file_pending());
        let outgoing_idle = outgoing.observe(&engine, !local_work);
        let active = local_work || !outgoing_idle;
        let deadline = idle.observe(start.elapsed(), active, generation);
        // Process readable sockets/completions before deciding to leave, even
        // when the deadline has expired. A newly accepted client wins.
        // A finishing local turn may have no outgoing effects. Let its
        // completion wake us without extending the idle clock or spinning.
        let wait_deadline = deadline.filter(|at| {
            start.elapsed() < *at || !(engine.service_executor.pending() || engine.file_pending())
        });
        requests.run_until(&mut engine, wait_deadline.map(|at| start + at))?;
        let (connected, generation) = requests.connections();
        let provider_busy = {
            #[cfg(target_os = "macos")]
            {
                provider.busy()
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        };
        let local_work = connected
            || provider_busy
            || engine.session_instances.active()
            || engine.tool_contexts.active()
            || (engine.service_executor.pending() || engine.file_pending());
        let outgoing_idle = outgoing.observe(&engine, !local_work);
        let active = local_work || !outgoing_idle;
        if idle.observe(start.elapsed(), active, generation).is_some_and(|at| start.elapsed() >= at)
            && !(engine.service_executor.pending() || engine.file_pending())
        {
            crate::diagnostic!(
                "Stopping after 15 minutes without local IPC connections or outgoing sync work"
            );
            break;
        }
    }
    // No further admission: the listener closes here, so a launcher's probe
    // fails at once instead of queuing behind an engine that is leaving.
    drop(requests);
    #[cfg(target_os = "macos")]
    drop(provider);
    // The replacing instance allows three seconds. This leaves room for
    // `Remote`'s own bounded close flush when the Engine drops below.
    let mut deferred = false;
    loop {
        match engine.shutdown(Duration::from_secs(1)) {
            Ok(Shutdown::Done) => break,
            // An enrollment has no second attempt: the AP may already have
            // issued credentials this device has not written down, and
            // nothing reissues them. So exit waits for that exchange however
            // long it takes, instead of fitting the replacement deadline.
            // The replacing instance reports the wait through its own
            // timeout, and admission is closed meanwhile.
            Ok(Shutdown::EnrollmentPending) => {
                if !deferred {
                    deferred = true;
                    crate::diagnostic!(
                        "Stop deferred: finishing an enrollment exchange before exit"
                    );
                }
            }
            Ok(Shutdown::AcceptedWorkPending) => {
                if !deferred {
                    deferred = true;
                    crate::diagnostic!(
                        "Stop deferred: finishing accepted data operations before exit"
                    );
                }
            }
            Err(error) => {
                crate::diagnostic!("Stopping with background network work unfinished: {error}");
                break;
            }
        }
    }
    drop(engine);
    Ok(())
}
