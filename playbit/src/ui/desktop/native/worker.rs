//! Native host ownership (tier B): one cooperative owner, bounded input/output,
//! readiness wakes, and a shutdown barrier after all admitted input is consumed.
use super::Launch;
use crate::ui::desktop::alert::Failure;
use crate::{
    engine::{
        Services,
        native::{Apps, MAX_APPS},
    },
    runtime::{
        ViewNode,
        syscall::{Event, EventKind},
    },
    trace::Span,
    ui::desktop::scene::{Input, InputAck},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Instant,
};
#[cfg(feature = "connected-engine")]
pub(crate) mod setup;
const MAX_INPUTS: usize = 8;
// Bound receipts through UI adoption, plus one coalesced readiness scene
// between receipts and lifecycle/control messages for each native app.
const OUTPUT_CAPACITY: usize = MAX_APPS * (2 * MAX_INPUTS + 4);
const MAX_TITLE_BYTES: usize = 256;
const ADMISSION_ERROR: &str = "Input could not be queued. Try the action again.";
mod ready_output;

pub(super) struct PendingInput(Arc<AtomicUsize>);
impl PendingInput {
    fn try_new(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < MAX_INPUTS { Some(n + 1) } else { None }
            })
            .ok()
            .map(|_| Self(count.clone()))
    }
    #[cfg(test)]
    fn new(count: &Arc<AtomicUsize>) -> Self {
        Self::try_new(count).expect("test input fits the admission bound")
    }
}
impl Drop for PendingInput {
    fn drop(&mut self) {
        assert!(self.0.fetch_sub(1, Ordering::AcqRel) > 0);
    }
}
pub(super) struct Message {
    pending: PendingInput,
    pub(super) input: Input,
    epoch: u64,
}
#[derive(Clone)]
pub(super) struct Handle {
    pub generation: u64,
    #[cfg(all(feature = "native-audio", target_os = "macos"))]
    audio_instance: Option<[u8; 16]>,
    #[cfg(feature = "desktop")]
    pub session: Option<[u8; 32]>,
    #[cfg(all(target_os = "macos", feature = "process"))]
    pub(super) startup: Option<Arc<(PathBuf, crate::engine::session_instance::Context)>>,
    inputs: async_channel::Sender<Message>,
    wake: mpsc::SyncSender<()>,
    failures: Arc<AtomicU64>,
    pending: Arc<AtomicUsize>,
    errors: async_channel::Sender<&'static str>,
}
impl Handle {
    #[cfg(all(feature = "native-audio", target_os = "macos"))]
    pub(super) fn audio_instance(&self) -> Option<[u8; 16]> {
        self.audio_instance
    }
    pub fn input_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
    #[cfg(test)]
    pub(super) fn fixture(generation: u64) -> (Self, async_channel::Receiver<Message>) {
        let (inputs, receive) = async_channel::bounded(MAX_INPUTS);
        let (errors, _) = async_channel::bounded(1);
        let (wake, _) = mpsc::sync_channel(1);
        (
            Self {
                generation,
                #[cfg(all(feature = "native-audio", target_os = "macos"))]
                audio_instance: None,
                #[cfg(feature = "desktop")]
                session: None,
                inputs,
                wake,
                failures: Arc::new(AtomicU64::new(0)),
                errors,
                #[cfg(all(target_os = "macos", feature = "process"))]
                startup: None,
                pending: Arc::new(AtomicUsize::new(0)),
            },
            receive,
        )
    }
    pub fn epoch(&self) -> u64 {
        self.failures.load(Ordering::Acquire)
    }
    pub fn close(&self) {
        self.inputs.close();
        self.errors.close();
        let _ = self.wake.try_send(());
    }
    pub fn send_scene_input(&self, input: Input) -> bool {
        // Layout retries the latest geometry after pending input is adopted.
        // Keep obsolete resize events out of the action queue. Window Close
        // uses send directly: its windowless surface is an ordered action.
        // Setup can retire its receiver before the admitted app replaces its
        // busy window. That obsolete geometry is not a failed user action.
        if input.kind == EventKind::SurfaceChanged
            && (self.input_pending() || self.inputs.is_closed())
        {
            return false;
        }
        self.send(input, None)
    }
    pub fn send(&self, input: Input, epoch: Option<u64>) -> bool {
        // UI admission is serial. Frames may not follow an unacknowledged input.
        if input.kind == EventKind::Frame && self.input_pending() {
            return false;
        }

        let _admission = Span::new("ui.input.admit");
        if input.kind == EventKind::Click {
            crate::trace::mark("ui.input.admit.click");
        } else if matches!(input.kind, EventKind::TextChanged | EventKind::Submit) {
            crate::trace::mark("ui.input.admit.edit");
        } else if input.kind == EventKind::SurfaceChanged {
            crate::trace::mark("ui.input.admit.surface");
        }
        let epoch = epoch.unwrap_or_else(|| self.epoch());
        let max_bytes = if input.kind == EventKind::Submit {
            playbit_api::abi::text::MAX_TEXT_BYTES
        } else {
            playbit_api::abi::syscall::MAX_EVENT_BYTES
        };
        let pending =
            (input.data.len() <= max_bytes).then(|| PendingInput::try_new(&self.pending)).flatten();
        if let Some(pending) = pending
            && self.inputs.try_send(Message { input, epoch, pending }).is_ok()
        {
            let _ = self.wake.try_send(());
            true
        } else {
            crate::trace::mark("ui.input.rejected");
            let _ = self.errors.try_send(ADMISSION_ERROR);
            false
        }
    }
    fn failed(&self) {
        self.failures
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .expect("native input failure epoch cannot wrap");
    }
}
pub(super) enum Output {
    UiZoom {
        scale: u32,
        save: super::super::zoom::Save,
    },
    Readiness(ready_output::Pending),
    #[cfg(feature = "connected-engine")]
    SignIn {
        enrolled: bool,
        profile: crate::engine::reply::AccountProfile,
        actions: async_channel::Sender<setup::SignInAction>,
        replies: async_channel::Receiver<setup::SignInReply>,
    },
    #[cfg(feature = "process")]
    QuitRequested,
    #[cfg(feature = "process")]
    ShowRequested {
        owner_generation: u64,
        presentation: u64,
    },
    Opened {
        replaces: Option<u64>,
        handle: Handle,
        errors: async_channel::Receiver<&'static str>,
        title: String,
        view: Option<ViewNode>,
        logs: Vec<String>,
        quit_on_last_window: bool,
    },
    Frame {
        pending: Option<PendingInput>,
        generation: u64,
        result: Result<Option<ViewNode>, String>,
        logs: Vec<String>,
        ack: InputAck,
        frame_input: bool,
        quit_on_last_window: Option<bool>,
    },
    #[cfg(feature = "connected-engine")]
    Connection(super::network::Status),
    Closed(u64),
    Failed(Failure),
    #[cfg(feature = "process")]
    ContextUnavailable(u64),
    #[cfg(feature = "process")]
    Recovering {
        generation: u64,
        active: Arc<AtomicBool>,
        started: Instant,
    },
}
impl Output {
    pub fn resolve(self) -> Self {
        match self {
            Self::Readiness(slot) => slot
                .lock()
                .expect("native readiness scene lock poisoned")
                .take()
                .expect("one queued notification owns this readiness scene"),
            output => output,
        }
    }
}
struct Slot {
    handle: Handle,
    inputs: async_channel::Receiver<Message>,
    ready_output: std::cell::RefCell<Option<ready_output::Pending>>,
    draining: bool,
}
pub(super) struct Worker {
    stop: Arc<AtomicBool>,
    wake: mpsc::SyncSender<()>,
    output: async_channel::Receiver<Output>,
    done: async_channel::Receiver<Result<(), Failure>>,
    thread: Option<JoinHandle<()>>,
    #[cfg(feature = "connected-engine")]
    network: Option<super::network::Network>,
    #[cfg(feature = "connected-engine")]
    setup_inputs: Option<setup::Cancel>,
}
impl Worker {
    #[cfg(feature = "process")]
    pub fn start_guest(
        profile: PathBuf,
        engine: PathBuf,
        fuel_limit: Option<std::num::NonZeroU64>,
        resolve: impl FnOnce()
            -> Result<Option<crate::engine::session_instance::SessionInstance>, String>
        + Send
        + 'static,
    ) -> std::io::Result<Self> {
        let (wake, receive) = mpsc::sync_channel(1);
        let (send, output) = async_channel::bounded(OUTPUT_CAPACITY);
        let (completed, done) = async_channel::bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_wake = wake.clone();
        let thread = std::thread::Builder::new().name("guest-owner".into()).spawn(move || {
            let result = initialize_guest_engine(
                &profile,
                &engine,
                &worker_wake,
                &receive,
                &send,
                &worker_stop,
            )
            .and_then(|child| {
                resolve().map_err(Failure::from).and_then(|instance| match instance {
                    Some(instance) => {
                        let root = Some(instance.root);
                        let crate::engine::reply::Reply::UiZoom { scale, .. } =
                            crate::engine::ipc::call_archive(
                                &profile,
                                &crate::engine::Request::UiZoom { root, scale: None },
                            )
                            .and_then(|reply| reply.into_owned())
                            .map_err(|e| e.to_string())?
                        else {
                            return Err("Unexpected UI zoom reply".into());
                        };
                        let location = profile.clone();
                        let save = Arc::new(move |scale| {
                            crate::engine::ipc::call_archive(
                                &location,
                                &crate::engine::Request::UiZoom { root, scale: Some(scale) },
                            )
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                        });
                        send.send_blocking(Output::UiZoom { scale, save })
                            .map_err(|_| "UI stopped receiving zoom preferences")?;
                        // The GUI guest bypasses the Sessions bootstrap. Register its
                        // initialized source asynchronously; collector ACK never gates app work.
                        if let Err(error) = crate::trace::attach(&profile) {
                            crate::diagnostic!("Playbit trace attachment unavailable: {error}");
                        }
                        #[cfg(feature = "desktop")]
                        crate::desktop::launch_timing::flush();
                        own_guest(
                            profile,
                            (engine, child),
                            GuestLaunch { instance, fuel_limit },
                            worker_wake,
                            receive,
                            &send,
                            worker_stop,
                        )
                    }
                    None => {
                        let _ = send.send_blocking(Output::Closed(0));
                        Ok(())
                    }
                })
            });
            if let Err(error) = &result {
                let _ = send.send_blocking(Output::Failed(error.clone()));
            }
            let _ = completed.try_send(result);
        })?;
        Ok(Self {
            stop,
            wake,
            output,
            done,
            thread: Some(thread),
            #[cfg(feature = "connected-engine")]
            network: None,
            #[cfg(feature = "connected-engine")]
            setup_inputs: None,
        })
    }
    /// # Safety
    /// The prepare callback's app descriptors obey native::run's lifetime contract.
    #[allow(unsafe_code, reason = "The native host retains callback code through worker join.")]
    pub unsafe fn start(
        profile: PathBuf,
        prepare: impl FnOnce(&Services) -> Result<Vec<Launch>, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (wake, receive) = mpsc::sync_channel(1);
        let (send, output) = async_channel::bounded(OUTPUT_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_wake = wake.clone();
        let (completed, done) = async_channel::bounded(1);
        let thread = std::thread::Builder::new().name("native-apps".into()).spawn(move || {
            // SAFETY: start's caller retains app code/data through shutdown/join.
            let result =
                unsafe { run(profile, prepare, worker_wake, receive, &send, &worker_stop) };
            if let Err(error) = &result {
                let _ = send.send_blocking(Output::Failed(error.clone()));
            }
            let _ = completed.try_send(result);
        })?;
        Ok(Self {
            stop,
            wake,
            output,
            done,
            thread: Some(thread),
            #[cfg(feature = "connected-engine")]
            network: None,
            #[cfg(feature = "connected-engine")]
            setup_inputs: None,
        })
    }
    /// # Safety
    /// The host retains prepared descriptors through shutdown and worker joins.
    #[cfg(feature = "connected-engine")]
    #[allow(unsafe_code, reason = "The host owns app callback lifetimes.")]
    pub unsafe fn start_connected(
        profile: PathBuf,
        prepare: impl FnOnce(
            &mut crate::engine::connected::Connected,
        ) -> Result<Vec<super::SessionLaunch>, String>
        + Send
        + 'static,
    ) -> std::io::Result<Self> {
        // SAFETY: the caller retains descriptors through both worker joins.
        unsafe {
            Self::start_with_setup(profile, move |connected, _, _| {
                prepare(connected.as_mut().ok_or("Account profile unavailable")?)
            })
        }
    }
    /// # Safety
    /// The prepared callbacks obey start_connected's ownership contract.
    #[cfg(feature = "connected-engine")]
    #[allow(unsafe_code, reason = "The host owns app callback lifetimes.")]
    pub(super) unsafe fn start_with_setup(
        profile: PathBuf,
        prepare: impl FnOnce(
            &mut Option<crate::engine::connected::Connected>,
            &mut super::network::ChangeProfile<'_>,
            &mut setup::Setup,
        ) -> Result<Vec<super::SessionLaunch>, String>
        + Send
        + 'static,
    ) -> std::io::Result<Self> {
        let (wake, receive) = mpsc::sync_channel(1);
        let (send, output) = async_channel::bounded(OUTPUT_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let mut setup = setup::Setup::new(send.clone(), wake.clone());
        let setup_inputs = Some(setup.cancel_handle());
        let canceled = Arc::new(AtomicBool::new(false));
        let setup_canceled = canceled.clone();
        let (network, prepared) =
            super::network::Network::start(profile, wake.clone(), move |connected, reset| {
                let result = prepare(connected, reset, &mut setup);
                if result.is_err() && setup.canceled() {
                    setup_canceled.store(true, Ordering::Release);
                    setup.close();
                }
                result
            })?;
        let status = network.status_handle();
        let worker_stop = stop.clone();
        let worker_wake = wake.clone();
        let (completed, done) = async_channel::bounded(1);
        let thread = std::thread::Builder::new().name("native-apps".into()).spawn(move || {
            // SAFETY: start_connected's caller retains descriptors through join.
            let result = unsafe {
                run_connected(prepared, status, worker_wake, receive, &send, &worker_stop)
            };
            let result = if result.is_err() && canceled.load(Ordering::Acquire) {
                Ok(()) // Explicit setup cancellation preceded any app admission.
            } else {
                result
            };
            if let Err(error) = &result
                && !worker_stop.load(Ordering::Acquire)
            {
                let _ = send.send_blocking(Output::Failed(error.clone()));
                let _ = send.send_blocking(Output::Closed(0));
            }
            let _ = completed.try_send(result);
        })?;
        Ok(Self {
            stop,
            wake,
            output,
            done,
            thread: Some(thread),
            network: Some(network),
            setup_inputs,
        })
    }
    pub fn output(&self) -> async_channel::Receiver<Output> {
        self.output.clone()
    }
    pub fn completion(&self) -> async_channel::Receiver<Result<(), Failure>> {
        self.done.clone()
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(feature = "connected-engine")]
        if let Some(inputs) = &self.setup_inputs {
            inputs.close();
        }
        let _ = self.wake.try_send(());
    }
    pub fn shutdown(&mut self) {
        // Unblock output before joining. Native code never waits for a UI callback;
        // admitted input still runs and commits, even when its window has gone.
        self.stop.store(true, Ordering::Release);
        self.output.close();
        #[cfg(feature = "connected-engine")]
        if let Some(inputs) = self.setup_inputs.take() {
            inputs.close();
        }
        #[cfg(feature = "connected-engine")]
        if let Some(network) = &self.network {
            network.request_stop();
        }
        let _ = self.wake.try_send(());
        if let Some(thread) = self.thread.take() {
            thread.join().expect("native app worker must not panic");
        }
        #[cfg(feature = "connected-engine")]
        if let Some(mut network) = self.network.take() {
            network.shutdown();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(feature = "process")]
struct GuestLaunch {
    instance: crate::engine::session_instance::SessionInstance,
    fuel_limit: Option<std::num::NonZeroU64>,
}

#[cfg(feature = "process")]
fn own_guest(
    profile: PathBuf,
    engine: (PathBuf, Option<std::process::Child>),
    launch: GuestLaunch,
    wake: mpsc::SyncSender<()>,
    receive: mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: Arc<AtomicBool>,
) -> Result<(), Failure> {
    use crate::engine::gui_guest::Guest;
    let GuestLaunch { instance, fuel_limit } = launch;
    let (engine, child) = engine;
    // Bind before claim becomes visible, so a tool observing the verified
    // owner can wait for initial logs without racing endpoint installation.
    let controls = super::control::Server::start(&profile, instance.id, output.clone())
        .map_err(|error| error.to_string())?;
    let publisher = controls.publisher();
    let host_wake = wake.clone();
    let mut guest = Guest::open(
        &profile,
        instance.id,
        &instance.session,
        instance.root,
        Arc::new(move || {
            let _ = host_wake.try_send(());
        }),
        fuel_limit,
    )
    .map_err(|error| error.to_string())?;
    let mut replaces = None;
    let mut recovery = GuestRecovery {
        profile: &profile,
        engine: &engine,
        child,
        wake: &wake,
        receive: &receive,
        output,
        stop: &stop,
        backoff: Default::default(),
        fuel_limit,
    };
    loop {
        let prior = guest.admission().clone();
        let generation = guest.generation();
        let mut presentation = GuestPresentation { profile: &profile, replaces, opened: false };
        let result =
            run_guest(&mut guest, &wake, &receive, output, &stop, &publisher, &mut presentation);
        if result.is_err() {
            let _ = crate::engine::gui_guest::startup(
                &profile,
                guest.admission().context,
                None,
                crate::engine::session_instance::StartupPhase::Failed,
            );
        }
        publisher.stopped(result.as_ref().err().map(|error| error.details.as_str()));
        // Nonfatal retirement belongs to an existing native window. Preserve
        // initial-launch failure handling before any window has been presented.
        let presented = presentation.opened || replaces.is_some();
        // Retire the VM and its input queue before showing recovery. Its data
        // host still resolves already admitted work; nothing is copied to a new VM.
        let file_drain = guest.file_drain();
        drop(guest);
        // File begin is nonblocking and can precede IPC admission. Wait on the
        // existing completion wake before retiring that context. This waits for
        // execution, not for retained results or failed cleanup that retirement
        // itself must release. Always drain finite Data exchanges as well: a
        // late stop can make this owner complete normally, and a later uncertain
        // result must still outrank that stop. Long-lived channel receives are
        // excluded. No guest callback or input is replayed.
        drain_guest_work(&file_drain, &receive)?;
        let Err(mut error) = result else { return Ok(()) };
        // An accepted exchange may become uncertain while draining. Its final
        // authoritative failure must outrank the earlier expected context stop.
        if let Some(details) = file_drain.failure() {
            error.details = details.into();
        }
        let stop_reason = presented.then(|| file_drain.stop_reason()).flatten();
        // A failed initial turn has not rebound the window yet. Keep its prior
        // routing identity rather than creating a second recovery window.
        let window_generation =
            if presentation.opened { generation } else { replaces.unwrap_or(generation) };
        let Some(recovered) = recovery.recover(&prior, error, window_generation, stop_reason)?
        else {
            return Ok(());
        };
        guest = recovered;
        replaces = Some(window_generation);
    }
}

#[cfg(feature = "process")]
fn drain_guest_work(
    drain: &crate::engine::guest_host::Inbox,
    receive: &mpsc::Receiver<()>,
) -> Result<(), Failure> {
    while drain.work_executing() {
        receive
            .recv()
            .map_err(|_| "Accepted-work completion channel closed during teardown".to_owned())?;
    }
    Ok(())
}

/// Startup has no window yet, but it uses the same bounded retry owner.
#[cfg(feature = "process")]
fn initialize_guest_engine(
    profile: &std::path::Path,
    engine: &std::path::Path,
    wake: &mpsc::SyncSender<()>,
    receive: &mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: &AtomicBool,
) -> Result<Option<std::process::Child>, Failure> {
    let mut recovery = GuestRecovery {
        profile,
        engine,
        child: None,
        wake,
        receive,
        output,
        stop,
        backoff: Default::default(),
        fuel_limit: None,
    };
    recovery.initialize()
}

#[cfg(feature = "process")]
struct GuestRecovery<'a> {
    profile: &'a std::path::Path,
    engine: &'a std::path::Path,
    child: Option<std::process::Child>,
    wake: &'a mpsc::SyncSender<()>,
    receive: &'a mpsc::Receiver<()>,
    output: &'a async_channel::Sender<Output>,
    stop: &'a AtomicBool,
    backoff: crate::engine::restart::Backoff,
    fuel_limit: Option<std::num::NonZeroU64>,
}
#[cfg(feature = "process")]
impl GuestRecovery<'_> {
    fn initialize(&mut self) -> Result<Option<std::process::Child>, Failure> {
        let active = Arc::new(AtomicBool::new(false));
        let mut first = true;
        let started = Instant::now();
        let result = loop {
            crate::engine::restart::reap(&mut self.child, self.profile);
            let result = if self.child.is_none() {
                let launch =
                    crate::engine::launch::start_for_desktop(self.profile, self.engine, !first);
                first = false;
                crate::engine::launch::retain(launch, &mut self.child)
            } else {
                crate::engine::ipc::call_archive(self.profile, &crate::engine::Request::Status)
                    .map(|_| ())
            };
            match result {
                Ok(()) => break Ok(self.child.take()),
                Err(error) => {
                    let details = error.to_string();
                    crate::platform::failure::log(
                        self.profile,
                        "app owner engine startup",
                        &format!("{error:?}"),
                    );
                    if crate::engine::restart::terminal(&details) {
                        break Err(details.into());
                    }
                    if !active.swap(true, Ordering::AcqRel)
                        && self
                            .output
                            .send_blocking(Output::Recovering {
                                generation: 0,
                                active: active.clone(),
                                started,
                            })
                            .is_err()
                    {
                        break Err(details.into());
                    }
                    let delay = self.backoff.failed(Instant::now());
                    if !self.wait_retry(delay) {
                        break Err(details.into());
                    }
                }
            }
        };
        active.store(false, Ordering::Release);
        result
    }
    fn recover(
        &mut self,
        prior: &crate::engine::session_instance::Admission,
        error: Failure,
        generation: u64,
        stop_reason: Option<crate::engine::service_events::StopReason>,
    ) -> Result<Option<crate::engine::gui_guest::Guest>, Failure> {
        crate::platform::failure::log(self.profile, "app owner stopped", &error.details);
        let active = Arc::new(AtomicBool::new(true));
        self.output
            .send_blocking(Output::Recovering {
                generation,
                active: active.clone(),
                started: Instant::now(),
            })
            .map_err(|_| error.clone())?;
        let result = self.reconstruct(prior, error, stop_reason);
        active.store(false, Ordering::Release);
        if matches!(&result, Ok(None)) {
            self.output
                .send_blocking(Output::ContextUnavailable(generation))
                .map_err(|_| "The app window stopped receiving owner completion")?;
        }
        result
    }
    fn reconstruct(
        &mut self,
        prior: &crate::engine::session_instance::Admission,
        mut error: Failure,
        stop_reason: Option<crate::engine::service_events::StopReason>,
    ) -> Result<Option<crate::engine::gui_guest::Guest>, Failure> {
        let mut attempted = (prior.context.incarnation, prior.instance.build.clone());
        loop {
            let delay = self.backoff.failed(Instant::now());
            if !self.wait_retry(delay) {
                return Err(error);
            }
            match self.attempt(prior, &mut attempted, stop_reason) {
                Ok(guest) => {
                    self.backoff.recovered(Instant::now());
                    if self.stop.load(Ordering::Acquire) {
                        return Err(error);
                    }
                    return Ok(Some(guest));
                }
                Err(failure) => {
                    if matches!(
                        failure.downcast_ref::<crate::engine::gui_guest::RecoveryError>(),
                        Some(crate::engine::gui_guest::RecoveryError::ContextUnavailable)
                    ) {
                        return Ok(None);
                    }
                    error = failure.to_string().into();
                    crate::platform::failure::log(
                        self.profile,
                        "automatic app reconstruction",
                        &format!("{failure:?}"),
                    );
                    if crate::engine::restart::terminal(&error.details)
                        || matches!(
                            failure.downcast_ref::<crate::engine::gui_guest::RecoveryError>(),
                            Some(crate::engine::gui_guest::RecoveryError::AlreadyAttempted)
                        )
                    {
                        return Err(error);
                    }
                }
            }
        }
    }
    fn wait_retry(&self, delay: std::time::Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            if self.stop.load(Ordering::Acquire) || self.output.is_closed() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            if self.receive.recv_timeout(remaining) == Err(mpsc::RecvTimeoutError::Disconnected) {
                return false;
            }
        }
    }
    fn attempt(
        &mut self,
        prior: &crate::engine::session_instance::Admission,
        attempted: &mut (crate::store::Id, String),
        stop_reason: Option<crate::engine::service_events::StopReason>,
    ) -> crate::engine::Result<crate::engine::gui_guest::Guest> {
        crate::engine::restart::reap(&mut self.child, self.profile);
        if self.child.is_none() {
            crate::engine::launch::retain(
                crate::engine::launch::start_for_desktop(self.profile, self.engine, true),
                &mut self.child,
            )?;
        }
        let wake = self.wake.clone();
        crate::engine::gui_guest::Guest::recover(
            self.profile,
            prior,
            attempted,
            Arc::new(move || {
                let _ = wake.try_send(());
            }),
            self.fuel_limit,
            stop_reason,
        )
    }
}

#[cfg(feature = "process")]
struct GuestPresentation<'a> {
    profile: &'a std::path::Path,
    replaces: Option<u64>,
    opened: bool,
}

#[cfg(feature = "process")]
fn run_guest(
    guest: &mut crate::engine::gui_guest::Guest,
    wake: &mpsc::SyncSender<()>,
    receive: &mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: &AtomicBool,
    publisher: &super::control::Publisher,
    presentation: &mut GuestPresentation<'_>,
) -> Result<(), Failure> {
    let profile = presentation.profile;
    let (inputs, receiver) = async_channel::bounded(MAX_INPUTS);
    let (errors, error_receive) = async_channel::bounded(1);
    let handle = Handle {
        generation: guest.generation(),
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        audio_instance: Some(*guest.admission().context.instance.as_bytes()),
        #[cfg(feature = "desktop")]
        session: Some(guest.session_identity()),
        #[cfg(all(target_os = "macos", feature = "process"))]
        startup: Some(Arc::new((profile.to_owned(), guest.admission().context))),
        pending: Arc::new(AtomicUsize::new(0)),
        inputs,
        wake: wake.clone(),
        failures: Arc::new(AtomicU64::new(0)),
        errors,
    };
    // A panic in the first turn must already have a readable log generation.
    publisher.opened(
        guest.admission().context.owner_generation,
        guest.admission().instance.build.clone(),
        handle.clone(),
    );
    let first = guest.pump().map_err(|error| error.to_string())?;
    crate::engine::gui_guest::startup(
        profile,
        guest.admission().context,
        None,
        crate::engine::session_instance::StartupPhase::OpeningWindow,
    )
    .map_err(|error| error.to_string())?;
    publisher.frame(first.encoded, &first.logs);
    let title = guest.title().to_owned();
    if title.is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err("App title must contain 1–256 UTF-8 bytes".into());
    }
    output
        .send_blocking(Output::Opened {
            replaces: presentation.replaces,
            handle: handle.clone(),
            errors: error_receive,
            title,
            view: first.view,
            logs: first.logs,
            quit_on_last_window: first.quit_on_last_window,
        })
        .map_err(|_| "The app window stopped receiving guest output")?;
    presentation.opened = true;
    let mut draining = false;
    let mut rejected_input = None;
    // One diagnostic token, never an execution or admission dependency. An
    // overlapping input is explicitly unqualified rather than retained forever.
    let mut ready_output = None;
    loop {
        let _ = receive.try_recv();
        if (stop.load(Ordering::Acquire) || receiver.is_closed()) && !draining {
            // Closing admission is the boundary. Every input admitted by the UI
            // before it is transferred to runtime work before deactivation.
            handle.inputs.close();
            while let Ok(message) = receiver.try_recv() {
                if let Err(error) = admit_guest_input(guest, &handle, message.input, message.epoch)
                {
                    handle.failed();
                    rejected_input = Some(error);
                }
            }
            guest.deactivate();
            draining = true;
        }
        if !draining && let Ok(message) = receiver.try_recv() {
            advance_guest(guest, &handle, Some(message), output, publisher, &mut ready_output)?;
        }
        if guest.ready(Instant::now()) {
            advance_guest(guest, &handle, None, output, publisher, &mut ready_output)?;
        }
        if draining {
            if guest.drain_failed() {
                return Err("Accepted app work could not finish. Reopen from committed data; uncertain edits were not replayed.".into());
            }
            if !guest.accepted_work_pending() {
                if let Some(error) = rejected_input {
                    return Err(error.into());
                }
                let _ = output.send_blocking(Output::Closed(handle.generation));
                return Ok(());
            }
        }
        if !receiver.is_empty() || guest.ready(Instant::now()) {
            continue;
        }
        match guest.wake_after(Instant::now()) {
            Some(delay) => {
                let _ = receive.recv_timeout(delay);
            }
            None => {
                receive.recv().map_err(|_| "The app owner lost its wake channel")?;
            }
        }
    }
}

#[cfg(feature = "process")]
fn advance_guest(
    guest: &mut crate::engine::gui_guest::Guest,
    handle: &Handle,
    message: Option<Message>,
    output: &async_channel::Sender<Output>,
    publisher: &super::control::Publisher,
    ready_output: &mut Option<ready_output::Pending>,
) -> Result<(), Failure> {
    let mut pending = None;
    let _job = Span::new(if message.is_some() { "ui.worker.input" } else { "ui.guest.ready" });
    let mut ack = InputAck::default();
    let mut frame_input = false;
    let admitted = if let Some(message) = message {
        ack = InputAck::new(message.input.target, message.input.revision.unwrap_or_default());

        frame_input = message.input.kind == EventKind::Frame;
        pending = Some(message.pending);
        admit_guest_input(guest, handle, message.input, message.epoch)
    } else {
        Ok(())
    };
    let frame = guest.pump().map_err(|error| error.to_string())?;
    publisher.frame(frame.encoded, &frame.logs);
    let result = admitted.map(|()| frame.view);
    if result.is_err() {
        handle.failed();
    }

    let _ = ready_output::send(
        output,
        ready_output,
        Output::Frame {
            pending,
            generation: handle.generation,
            result,
            logs: frame.logs,
            quit_on_last_window: Some(frame.quit_on_last_window),
            ack,
            frame_input,
        },
    );
    Ok(())
}

#[cfg(feature = "process")]
fn admit_guest_input(
    guest: &mut crate::engine::gui_guest::Guest,
    handle: &Handle,
    input: Input,
    epoch: u64,
) -> Result<(), String> {
    if epoch != handle.epoch() {
        return Err("An earlier input failed. Try the action again.".into());
    }
    guest
        .input(Event { kind: input.kind, target: input.target, data: input.data })
        .map_err(|error| format!("Input failed: {error:?}. Try the action again."))
}

#[allow(unsafe_code, reason = "The host binds trusted C descriptors on their owner thread.")]
// Preserve the origin-retention test's stack boundary in optimized test builds.
#[cfg_attr(test, inline(never))]
unsafe fn run(
    profile: PathBuf,
    prepare: impl FnOnce(&Services) -> Result<Vec<Launch>, String>,
    wake: mpsc::SyncSender<()>,
    receive: mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: &AtomicBool,
) -> Result<(), Failure> {
    let store_wake = wake.clone();
    let services = Services::open(profile, move || {
        let _ = store_wake.try_send(());
    })
    .map_err(|error| error.to_string())?;
    let mut changes = services.store.take_change_receiver().expect("new services own the feed");
    let _recorder = crate::trace::Recorder::for_profile(
        services.profile_path(),
        &(services.profile_path()).join(crate::paths::TRACE),
    );
    let launches = prepare(&services)?;
    if launches.is_empty() || launches.len() > MAX_APPS {
        return Err("A native host must launch between one and four apps".into());
    }
    publish_zoom(&services, Some(launches[0].root), output)?;
    // Drop order is intentional: apps release their store references first.
    let data_wake = wake.clone();
    let mut apps = Apps::new(Arc::new(move || {
        let _ = data_wake.try_send(());
    }));
    // SAFETY: descriptors remain valid until this owner returns.
    let slots = unsafe { attach(&mut apps, &services, launches, &wake, output)? };
    drive(&mut apps, slots, receive, output, stop, |apps| {
        apps.committed(&changes.drain());
        Ok(services.store.pending_change_count() != 0)
    })
}
fn publish_zoom(
    services: &Services,
    root: Option<crate::store::Id>,
    output: &async_channel::Sender<Output>,
) -> Result<(), Failure> {
    let scale = services.store.ui_zoom(root).map_err(|error| error.to_string())?;
    let store = services.store.clone();
    let save =
        Arc::new(move |scale| store.set_ui_zoom(root, scale).map_err(|error| error.to_string()));
    output
        .send_blocking(Output::UiZoom { scale, save })
        .map_err(|_| "UI stopped receiving zoom preferences".into())
}

fn drive(
    apps: &mut Apps,
    mut slots: Vec<Slot>,
    receive: mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: &AtomicBool,
    mut changes: impl FnMut(&mut Apps) -> Result<bool, String>,
) -> Result<(), Failure> {
    while !slots.is_empty() {
        let _ = receive.try_recv(); // Clear before drain: store wake can rearm it.
        let pending_changes = changes(apps)?;
        if stop.load(Ordering::Acquire) {
            for slot in &slots {
                slot.inputs.close();
            }
        }
        let mut ready = false;
        let mut deadline = None;
        for slot in &mut slots {
            if slot.inputs.is_closed() && !slot.draining {
                // Transfer already accepted inputs before closing root admission.
                while let Ok(message) = slot.inputs.try_recv() {
                    if message.epoch != slot.handle.epoch() {
                        return Err("An accepted input failed before deactivation.".into());
                    }
                    let _job = Span::new("ui.worker.input");

                    apps.input(
                        slot.handle.generation,
                        Event {
                            kind: message.input.kind,
                            target: message.input.target,
                            data: message.input.data,
                        },
                    )
                    .map_err(|error| format!("Accepted input failed: {error:?}"))?;
                }
                apps.deactivate(slot.handle.generation)
                    .map_err(|error| format!("Cannot deactivate app: {error:?}"))?;
                slot.draining = true;
            }
            if let Ok(message) = slot.inputs.try_recv() {
                advance(apps, slot, Some(message), output);
            } else if apps
                .step(slot.handle.generation, Instant::now())
                .map_err(|e| format!("{e:?}"))?
                .ready
            {
                advance(apps, slot, None, output);
            }
            let step =
                apps.step(slot.handle.generation, Instant::now()).map_err(|e| format!("{e:?}"))?;
            ready |= !slot.inputs.is_empty() || step.ready;
            deadline = earliest(deadline, step.wake_after.map(|delay| Instant::now() + delay));
        }
        close_drained(apps, &mut slots, output)?;
        if slots.is_empty() {
            break;
        }
        if ready || pending_changes {
            continue;
        }
        match deadline {
            Some(deadline) => {
                let _ = receive.recv_timeout(deadline.saturating_duration_since(Instant::now()));
            }
            None => {
                let _ = receive.recv();
            }
        }
    }
    Ok(())
}
#[allow(unsafe_code, reason = "The owner retains trusted descriptors through detach.")]
unsafe fn attach(
    apps: &mut Apps,
    services: &Services,
    launches: Vec<Launch>,
    wake: &mpsc::SyncSender<()>,
    output: &async_channel::Sender<Output>,
) -> Result<Vec<Slot>, Failure> {
    let mut slots = Vec::with_capacity(MAX_APPS);
    for launch in launches {
        if launch.title.is_empty() || launch.title.len() > MAX_TITLE_BYTES {
            return Err("Native app title must contain 1–256 UTF-8 bytes".into());
        }
        // SAFETY: the caller retains descriptors through this function and join.
        let generation = unsafe { apps.attach(services, launch.root, launch.app) }
            .map_err(|error| format!("Cannot attach native app: {error:?}"))?;
        slots.push(open_slot(apps, generation, launch.title, wake, output)?);
    }
    Ok(slots)
}
fn open_slot(
    apps: &mut Apps,
    generation: u64,
    title: String,
    wake: &mpsc::SyncSender<()>,
    output: &async_channel::Sender<Output>,
) -> Result<Slot, Failure> {
    let (inputs, receiver) = async_channel::bounded(MAX_INPUTS);
    let (errors, error_receive) = async_channel::bounded(1);
    let handle = Handle {
        generation,
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        audio_instance: Some(
            apps.instance_id(generation)
                .map_err(|error| format!("Cannot bind native app audio: {error:?}"))?,
        ),
        #[cfg(feature = "desktop")]
        session: apps
            .session_identity(generation)
            .map_err(|error| format!("Cannot bind native app session: {error:?}"))?,
        #[cfg(all(target_os = "macos", feature = "process"))]
        startup: None,
        inputs,
        wake: wake.clone(),
        failures: Arc::new(AtomicU64::new(0)),
        pending: Arc::new(AtomicUsize::new(0)),
        errors,
    };
    let mut view = None;
    let mut logs = Vec::new();
    let mut quit_on_last_window = true;
    apps.advance(generation, |frame| {
        quit_on_last_window = frame.quit_on_last_window();
        view = frame.view().cloned();
        logs = frame.logs().to_vec();
    })
    .map_err(|error| format!("Cannot initialize native app: {error:?}"))?;
    let _ = output.send_blocking(Output::Opened {
        replaces: None,
        handle: handle.clone(),
        errors: error_receive,
        title,
        view,
        logs,
        quit_on_last_window,
    });
    Ok(Slot { handle, inputs: receiver, ready_output: Default::default(), draining: false })
}

#[cfg(feature = "connected-engine")]
#[allow(unsafe_code, reason = "The host owns prepared callbacks through app worker join.")]
unsafe fn run_connected(
    prepared: mpsc::Receiver<Result<super::network::Prepared, Failure>>,
    status: Arc<std::sync::Mutex<Option<super::network::Status>>>,
    wake: mpsc::SyncSender<()>,
    receive: mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: &AtomicBool,
) -> Result<(), Failure> {
    let prepared = prepared.recv().map_err(|_| "Native network preparation stopped")??;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    if prepared.launches.is_empty() || prepared.launches.len() > MAX_APPS {
        return Err("A native host must launch between one and four apps".into());
    }
    let data_wake = wake.clone();
    let mut apps = Apps::new(Arc::new(move || {
        let _ = data_wake.try_send(());
    }));
    let root = prepared
        .services
        .store
        .session(&prepared.launches[0].session)
        .map_err(|error| error.to_string())?
        .root_volume;
    publish_zoom(&prepared.services, Some(root), output)?;
    let mut slots = Vec::with_capacity(MAX_APPS);
    for launch in prepared.launches {
        let title = launch.package.name().to_owned();
        // SAFETY: the caller guarantees exact package/callback identity and lifetime.
        let generation = unsafe {
            apps.attach_session(
                &prepared.services,
                &launch.session,
                launch.package,
                launch.app,
                prepared.blobs.clone(),
            )
        }
        .map_err(|error| format!("Cannot attach native session: {error:?}"))?;
        slots.push(open_slot(&mut apps, generation, title, &wake, output)?);
    }
    drive(&mut apps, slots, receive, output, stop, |apps| {
        apps.received(&prepared.notifications.drain())
            .map_err(|error| format!("Native replica delivery failed: {error:?}"))?;
        let status = status.lock().expect("network status gate is not poisoned").take();
        if let Some(status) = status {
            let _ = output.send_blocking(Output::Connection(status));
        }
        Ok(false)
    })
}

fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
fn close_drained(
    apps: &mut Apps,
    slots: &mut Vec<Slot>,
    output: &async_channel::Sender<Output>,
) -> Result<(), Failure> {
    for index in (0..slots.len()).rev() {
        if slots[index].draining
            && !apps
                .drain_pending(slots[index].handle.generation)
                .map_err(|error| format!("Accepted app work failed to drain: {error:?}"))?
        {
            let generation = slots.remove(index).handle.generation;
            apps.detach(generation).map_err(|error| format!("Native close failed: {error:?}"))?;
            let _ = output.send_blocking(Output::Closed(generation));
        }
    }
    Ok(())
}
fn advance(
    apps: &mut Apps,
    slot: &Slot,
    message: Option<Message>,
    output: &async_channel::Sender<Output>,
) {
    let _job = Span::new(if message.is_some() { "ui.worker.input" } else { "ui.native.ready" });
    let generation = slot.handle.generation;
    let mut ack = InputAck::default();
    let mut frame_input = false;
    let mut pending = None;
    let mut admitted = Ok(());
    if let Some(message) = message {
        pending = Some(message.pending);

        ack = InputAck::new(message.input.target, message.input.revision.unwrap_or_default());

        frame_input = message.input.kind == EventKind::Frame;
        admitted = if message.epoch == slot.handle.epoch() {
            apps.input(
                generation,
                Event {
                    kind: message.input.kind,
                    target: message.input.target,
                    data: message.input.data,
                },
            )
            .map_err(|error| format!("Input failed: {error:?}. Try the action again."))
        } else {
            Err("An earlier input failed. Try the action again.".into())
        };
    }
    let mut view = None;
    let mut logs = Vec::new();
    let mut quit_on_last_window = None;
    let result = admitted
        .and_then(|()| {
            apps.advance(generation, |frame| {
                quit_on_last_window = Some(frame.quit_on_last_window());
                view = frame.view().cloned(); // The UI and owner each need their accepted scene.
                logs = frame.logs().to_vec();
            })
            .map_err(|error| format!("Native app stopped: {error:?}. Quit and reopen the app."))
        })
        .map(|()| view);
    if result.is_err() {
        slot.handle.failed();
    }

    if let Err(output) = ready_output::send(
        output,
        &mut slot.ready_output.borrow_mut(),
        Output::Frame { pending, generation, result, logs, ack, frame_input, quit_on_last_window },
    ) && let Output::Frame { result: Err(error), .. } = *output
    {
        crate::diagnostic!("Native app input failed during shutdown: {error}");
    }
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
