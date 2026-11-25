//! Native host ownership (tier B): one cooperative owner, bounded input/output,
//! readiness wakes, and a shutdown barrier after all admitted input is consumed.
use super::Launch;
use crate::ui::desktop::alert::Failure;
use crate::{
    engine::{
        GuestRenderer, Services,
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
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
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

// Queue items and reserved terminal edges share one atomic admission bound.
const RESERVATION: usize = MAX_INPUTS + 1;

pub(super) struct PendingInput(Arc<AtomicUsize>, usize);
impl PendingInput {
    fn charge(count: &Arc<AtomicUsize>, amount: usize) -> Option<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |units| {
                let used = units % RESERVATION + units / RESERVATION;
                (used < MAX_INPUTS).then_some(units + amount)
            })
            .ok()
            .map(|_| Self(count.clone(), amount))
    }

    fn activate(mut self) -> Self {
        assert_eq!(self.1, RESERVATION);
        assert!(self.0.fetch_sub(RESERVATION - 1, Ordering::AcqRel) >= RESERVATION);
        self.1 = 1;
        self
    }

    #[cfg(test)]
    fn new(count: &Arc<AtomicUsize>) -> Self {
        Self::charge(count, 1).expect("test input fits the admission bound")
    }
}
impl Drop for PendingInput {
    fn drop(&mut self) {
        assert!(self.0.fetch_sub(self.1, Ordering::AcqRel) >= self.1);
    }
}
pub(super) struct Message {
    pending: PendingInput,
    pub(super) input: Input,
    epoch: u64,
    object_id: u32,
    source: InputSource,
}
enum InputSource {
    Window,
    Menu(blake3::Hash),
    #[cfg(feature = "process")]
    Tool {
        object_id: u32,
        offset: Option<[f32; 2]>,
        legacy_canvas: bool,
    },
}
impl Message {
    /// Discard an obsolete selection before checking its epoch. Dropping its
    /// admission credit here makes any independent pump output ordinary readiness.
    fn if_current<'a>(self, scene: impl FnOnce() -> Option<&'a ViewNode>) -> Option<Self> {
        if let InputSource::Menu(digest) = &self.source
            && scene().is_some_and(|view| {
                !super::super::popup::guest_menu_present(view, &|items| {
                    crate::runtime::syscall::menu_contains(items, self.input.target)
                        && super::super::popup::menu_digest(items) == *digest
                })
            })
        {
            return None;
        }
        Some(self)
    }
    /// A closed window has no display demand. Revalidate other accepted actions
    /// after older runtime events have been consumed and their scene adopted.
    fn closing_event<'a>(
        self,
        handle: &Handle,
        scene: impl Fn() -> Option<&'a ViewNode>,
    ) -> Result<Option<Event>, String> {
        if crate::runtime::work::discard_on_deactivate(self.input.kind) {
            return Ok(None);
        }
        let Some(message) = self.if_current(&scene) else { return Ok(None) };
        if message.epoch != handle.epoch() {
            return Err("An earlier input failed. Try the action again.".into());
        }
        Ok(Some(message.source.event(message.input, message.object_id, scene)))
    }
}
impl InputSource {
    fn event<'a>(
        self,
        input: Input,
        object_id: u32,
        scene: impl FnOnce() -> Option<&'a ViewNode>,
    ) -> Event {
        let event = Event { object_id, kind: input.kind, target: input.target, data: input.data };
        #[cfg(feature = "process")]
        if let Self::Tool { offset, legacy_canvas, .. } = self
            && matches!(event.kind, EventKind::Pointer | EventKind::Key | EventKind::WindowPointer)
        {
            return map_tool_input(event, offset, legacy_canvas, scene());
        }
        #[cfg(not(feature = "process"))]
        let _ = scene;
        event
    }
}

#[cfg(feature = "process")]
fn map_tool_input(
    mut event: Event,
    offset: Option<[f32; 2]>,
    legacy_canvas: bool,
    scene: Option<&ViewNode>,
) -> Event {
    use crate::ui::app_control::canvas_input::{CANVAS_KEYS, keyboard};
    use playbit_api::abi::{
        input::{Input, Source},
        syscall::{KeyInput, PointerInput},
    };
    let size = scene.and_then(|view| super::super::canvas::canvas_size(view, event.target));
    let tool = || Source { timestamp_ns: playbit_trace::clock_ns(), ..Source::default() };
    let (source, mut pointer) = match event.kind {
        EventKind::Key => {
            if !legacy_canvas
                && size.is_some()
                && let Ok(key) = KeyInput::decode(&event.data)
            {
                let input = keyboard(CANVAS_KEYS[key.key as usize - 1], key.pressed);
                event.kind = input.kind();
                event.target = 0;
                event.data = input.encode(tool());
            }
            return event;
        }
        EventKind::Pointer => {
            let Ok(mut packet) = PointerInput::decode(&event.data) else { return event };
            if legacy_canvas {
                if let Some((width, height)) = size {
                    packet.x = packet.x.min(width);
                    packet.y = packet.y.min(height);
                    event.data.copy_from_slice(&packet.encode());
                }
                return event;
            }
            (tool(), window_pointer(packet))
        }
        EventKind::WindowPointer => {
            let Ok((source, Input::Pointer(pointer))) = Input::decode(event.kind, &event.data)
            else {
                return event;
            };
            (source, pointer)
        }
        _ => unreachable!("only canvas keys and pointers require tool mapping"),
    };
    // The scene can resize after admission. Only the original window offset was captured there.
    if let Some([x, y]) = offset {
        if let Some((width, height)) = size {
            pointer.canvas_x = pointer.canvas_x.clamp(0., width as f32);
            pointer.canvas_y = pointer.canvas_y.clamp(0., height as f32);
        }
        pointer.x = x + pointer.canvas_x;
        pointer.y = y + pointer.canvas_y;
        let input = Input::Pointer(pointer);
        event.kind = input.kind();
        event.data = input.encode(source);
    }
    event
}

#[cfg(feature = "process")]
fn window_pointer(
    pointer: playbit_api::abi::syscall::PointerInput,
) -> playbit_api::abi::input::Pointer {
    use playbit_api::abi::input::{self, Pointer};
    let phase = pointer.phase as u8;
    Pointer {
        id: u32::MAX,
        kind: input::MOUSE,
        phase,
        button: if matches!(phase, input::POINTER_DOWN | input::POINTER_UP) {
            0
        } else {
            input::NO_BUTTON
        },
        click_count: 0,
        flags: input::PRIMARY,
        buttons: pointer.buttons as u16,
        x: 0.,
        y: 0.,
        dx: 0.,
        dy: 0.,
        canvas_x: pointer.x as f32,
        canvas_y: pointer.y as f32,
        pen: None,
    }
}

#[cfg(feature = "process")]
struct ToolPointer {
    layouts: crossbeam_queue::ArrayQueue<ToolCanvasLayout>,
    state: std::sync::Mutex<ToolPointerState>,
}

#[cfg(feature = "process")]
impl Default for ToolPointer {
    fn default() -> Self {
        Self { layouts: crossbeam_queue::ArrayQueue::new(1), state: Default::default() }
    }
}

#[cfg(feature = "process")]
#[derive(Default)]
struct ToolPointerState {
    layout: ToolCanvasLayout,
    held: Option<ToolPointerOrigin>,
}

#[cfg(feature = "process")]
#[derive(Default)]
struct ToolCanvasLayout {
    object_id: u32,
    offsets: Vec<(u32, [f32; 2])>,
}

#[cfg(feature = "process")]
#[derive(Clone, Copy)]
struct ToolPointerOrigin {
    object_id: u32,
    target: u32,
    offset: [f32; 2],
}

#[derive(Clone)]
pub(super) struct Handle {
    pub generation: u64,
    pub(super) object_id: Arc<AtomicU32>,
    #[cfg(feature = "process")]
    tool_pointer: Arc<ToolPointer>,
    #[cfg(all(feature = "native-audio", target_os = "macos"))]
    audio_instance: Option<[u8; 16]>,
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
        !self.pending.load(Ordering::Acquire).is_multiple_of(RESERVATION)
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
                session: None,
                inputs,
                wake,
                object_id: Arc::new(AtomicU32::new(0)),
                #[cfg(feature = "process")]
                tool_pointer: Default::default(),
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
        self.enqueue(input, epoch, InputSource::Window, None)
    }
    pub(super) fn send_menu(&self, input: Input, epoch: u64, menu: blake3::Hash) -> bool {
        self.enqueue(input, Some(epoch), InputSource::Menu(menu), None)
    }
    #[cfg(feature = "process")]
    pub(super) fn send_tool_input(
        &self,
        input: Input,
        object_id: u32,
        legacy_canvas: bool,
    ) -> bool {
        use playbit_api::abi::input::{self as abi, Input as RawInput};
        let source = |offset| InputSource::Tool { object_id, offset, legacy_canvas };
        if legacy_canvas || !matches!(input.kind, EventKind::WindowPointer | EventKind::Pointer) {
            return self.enqueue(input, None, source(None), None);
        }
        let pointer = if input.kind == EventKind::Pointer {
            let Ok(packet) = playbit_api::abi::syscall::PointerInput::decode(&input.data) else {
                return false;
            };
            window_pointer(packet)
        } else {
            let Ok((_, RawInput::Pointer(pointer))) = RawInput::decode(input.kind, &input.data)
            else {
                return false;
            };
            pointer
        };
        let terminal = RawInput::Pointer(pointer).ends_sequence();
        let target = input.target;
        let mut state = self.tool_pointer.state.lock().expect("tool pointer state lock");
        if let Some(layout) = self.tool_pointer.layouts.pop() {
            state.layout = layout;
        }
        let held =
            state.held.filter(|origin| origin.object_id == object_id && origin.target == target);
        let offset = if state.layout.object_id == object_id {
            state.layout.offsets.iter().find(|(id, _)| *id == target).map(|(_, offset)| *offset)
        } else {
            None
        };
        let offset =
            offset.or_else(|| if terminal { held.map(|origin| origin.offset) } else { None });
        let Some(offset) = offset else { return false };
        if !self.enqueue(input, None, source(Some(offset)), None) {
            return false;
        }
        // CLI and MCP share one synthetic pointer. Keep its last origin until
        // the last button is released, even if the canvas disappears mid-chord.
        if pointer.phase == abi::POINTER_DOWN || held.is_some() {
            let released = terminal && (pointer.phase != abi::POINTER_UP || pointer.buttons == 0);
            state.held = (!released).then_some(ToolPointerOrigin { object_id, target, offset });
        }
        true
    }

    #[cfg(feature = "process")]
    pub(super) fn update_tool_canvas_offsets(
        &self,
        offsets: impl IntoIterator<Item = (u32, [f32; 2])>,
    ) {
        // The renderer publishes the latest layout without waiting on tool I/O.
        self.tool_pointer.layouts.force_push(ToolCanvasLayout {
            object_id: self.object_id.load(Ordering::Acquire),
            offsets: offsets.into_iter().collect(),
        });
    }
    #[cfg(target_os = "macos")]
    pub(super) fn reserve_release(&self) -> Option<PendingInput> {
        PendingInput::charge(&self.pending, RESERVATION)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn send_release(&self, input: Input, release: PendingInput) -> bool {
        self.enqueue(input, None, InputSource::Window, Some(release))
    }

    fn enqueue(
        &self,
        input: Input,
        epoch: Option<u64>,
        source: InputSource,
        release: Option<PendingInput>,
    ) -> bool {
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
        let input_kind = input.kind;
        let pending = (input.data.len() <= max_bytes)
            .then(|| {
                if let Some(release) = release {
                    Some(release.activate())
                } else {
                    PendingInput::charge(&self.pending, 1)
                }
            })
            .flatten();
        let object_id = match &source {
            #[cfg(feature = "process")]
            InputSource::Tool { object_id, .. } => *object_id,
            _ => self.object_id.load(Ordering::Acquire),
        };
        if let Some(pending) = pending
            && self.inputs.try_send(Message { input, epoch, pending, source, object_id }).is_ok()
        {
            let _ = self.wake.try_send(());
            true
        } else {
            crate::trace::mark("ui.input.rejected");
            // A display callback can race the old queue closing during hot reload.
            // Its frame is disposable, and no user action needs to be retried.
            if input_kind != EventKind::Frame
                && playbit_api::abi::input::Input::family(input_kind) == 0
            {
                let _ = self.errors.try_send(ADMISSION_ERROR);
            }
            false
        }
    }
    fn admit_closing_event(
        &self,
        event: Result<Option<Event>, String>,
        rejected: &mut Option<String>,
        admit: impl FnOnce(Event) -> Result<(), playbit_api::abi::syscall::Error>,
    ) {
        let result = event.and_then(|event| match event {
            Some(event) => {
                admit(event).map_err(|error| format!("Accepted input failed: {error:?}"))
            }
            None => Ok(()),
        });
        if let Err(error) = result {
            self.failed();
            rejected.get_or_insert(error);
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
    #[cfg(all(target_os = "macos", feature = "process"))]
    Screenshot {
        instance: crate::store::Id,
        owner_generation: u64,
        capture: Arc<super::screenshot::Capture>,
    },
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
    closing: Option<crate::runtime::work::ClosingInputs>,
    rejected_input: Option<String>,
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
            .and_then(|started| {
                let GuestEngine::Ready(child) = started else {
                    let _ = send.send_blocking(Output::Closed(0));
                    return Ok(());
                };
                let prepared = resolve().map_err(Failure::from).and_then(|instance| {
                    let Some(instance) = instance else { return Ok(None) };
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
                    Ok(Some(instance))
                });
                match prepared {
                    Ok(Some(instance)) => own_guest(
                        profile,
                        (engine, child),
                        GuestLaunch { instance, fuel_limit },
                        worker_wake,
                        receive,
                        &send,
                        worker_stop,
                    ),
                    Ok(None) => {
                        let _ = send.send_blocking(Output::Closed(0));
                        Ok(())
                    }
                    Err(error) if engine_endpoint_absent(&profile) => {
                        close_disconnected_guest(&profile, None, &error.details, 0, &send);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
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
fn engine_endpoint_absent(profile: &std::path::Path) -> bool {
    crate::engine::launch::connect_endpoint(profile).is_ok_and(|socket| socket.is_none())
}

#[cfg(feature = "process")]
fn disconnected_engine(
    profile: &std::path::Path,
    event_end: Option<&crate::engine::guest_host::EventEnd>,
    exchange_failed: bool,
) -> Option<String> {
    use crate::engine::guest_host::EventEnd;
    match event_end {
        Some(EventEnd::Disconnected(kind)) => Some(kind.to_string()),
        Some(EventEnd::Failed(_) | EventEnd::Stopped(_)) => None,
        None if exchange_failed && engine_endpoint_absent(profile) => {
            Some("engine endpoint absent".to_owned())
        }
        None => None,
    }
}

#[cfg(feature = "process")]
fn close_disconnected_guest(
    profile: &std::path::Path,
    instance: Option<crate::store::Id>,
    reason: &str,
    generation: u64,
    output: &async_channel::Sender<Output>,
) {
    let owner =
        instance.map_or_else(|| "during launch".to_owned(), |id| format!("for instance {id}"));
    crate::platform::failure::log(
        profile,
        "guest quitting",
        &format!(
            "local engine connection closed {owner} ({reason}). Accepted operations may have completed and were not replayed."
        ),
    );
    let _ = output.send_blocking(Output::Closed(generation));
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
    use crate::engine::guest_host::EventEnd;
    use crate::engine::gui_guest::Guest;
    let GuestLaunch { instance, fuel_limit } = launch;
    let (engine, child) = engine;
    // Bind before claim becomes visible, so a tool observing the verified
    // owner can wait for initial logs without racing endpoint installation.
    let controls = super::control::Server::start(&profile, instance.id, output.clone())
        .map_err(|error| error.to_string())?;
    let publisher = controls.publisher();
    let host_wake = wake.clone();
    let mut guest = match Guest::open(
        &profile,
        instance.id,
        &instance.session,
        instance.root,
        Arc::new(move || {
            let _ = host_wake.try_send(());
        }),
        crate::engine::gui_guest::GuestOptions { fuel_limit, renderer: GuestRenderer::AppKit },
    ) {
        Ok(guest) => guest,
        Err(error) => {
            if let Some(reason) = disconnected_engine(&profile, error.downcast_ref(), true) {
                close_disconnected_guest(&profile, Some(instance.id), &reason, 0, output);
                return Ok(());
            }
            return Err(error.to_string().into());
        }
    };
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
        let file_drain = guest.file_drain();
        publisher.stopped(result.as_ref().err().map(|error| error.details.as_str()));
        // Retire the VM and its input queue before showing recovery. Its data
        // host still resolves already admitted work; nothing is copied to a new VM.
        drop(guest);
        let event_end = file_drain.event_end();
        let report_failed = result.is_err()
            && !matches!(event_end, Some(EventEnd::Disconnected(_)))
            && crate::engine::gui_guest::startup(
                &profile,
                prior.context,
                None,
                crate::engine::session_instance::StartupPhase::Failed,
            )
            .is_err();
        let disconnect = disconnected_engine(&profile, event_end.as_ref(), report_failed);
        // File begin is nonblocking and can precede IPC admission. Wait on the
        // existing completion wake before retiring that context. This waits for
        // execution, not for retained results or failed cleanup that retirement
        // itself must release. Always drain finite Data exchanges as well: a
        // late stop can make this owner complete normally, and a later uncertain
        // result must still outrank that stop. Long-lived channel receives are
        // excluded. No guest callback or input is replayed.
        drain_guest_work(&file_drain, &receive)?;
        // A slow first launch displays generation 0. A replacement keeps its
        // old window generation until the new Opened output is delivered.
        let presented = presentation.presented_generation(generation);
        let window_generation = presented.unwrap_or(0);
        if let Some(reason) = disconnect {
            close_disconnected_guest(
                &profile,
                Some(prior.context.instance),
                &reason,
                window_generation,
                output,
            );
            return Ok(());
        }
        let Err(mut error) = result else {
            let _ = output.send_blocking(Output::Closed(window_generation));
            return Ok(());
        };
        // An accepted exchange may become uncertain while draining. Its final
        // authoritative failure must outrank the earlier expected context stop.
        if let Some(details) = file_drain.failure() {
            error.details = details.into();
        }
        let stop_reason = presented.and_then(|_| file_drain.stop_reason());
        let Some(recovered) = recovery.recover(&prior, error, window_generation, stop_reason)?
        else {
            return Ok(());
        };
        guest = recovered;
        replaces = presented;
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
enum GuestEngine {
    Ready(Option<std::process::Child>),
    Canceled,
}
#[cfg(feature = "process")]
fn initialize_guest_engine(
    profile: &std::path::Path,
    engine: &std::path::Path,
    wake: &mpsc::SyncSender<()>,
    receive: &mpsc::Receiver<()>,
    output: &async_channel::Sender<Output>,
    stop: &AtomicBool,
) -> Result<GuestEngine, Failure> {
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
    fn initialize(&mut self) -> Result<GuestEngine, Failure> {
        let active = Arc::new(AtomicBool::new(false));
        let mut first = true;
        let started = Instant::now();
        let result = loop {
            // No guest has been admitted. An explicit Quit can end startup
            // without reporting the preceding transient error as a failed drain.
            if self.stop.load(Ordering::Acquire) {
                break Ok(GuestEngine::Canceled);
            }
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
                Ok(()) => break Ok(GuestEngine::Ready(self.child.take())),
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
                    if !self.wait_retry(delay) && !self.stop.load(Ordering::Acquire) {
                        break Err(details.into());
                    }
                }
            }
        };
        active.store(false, Ordering::Release);
        // The engine probe may finish after Quit. Check again at admission,
        // before the caller can reserve an instance or deliver a guest turn.
        if self.stop.load(Ordering::Acquire) { Ok(GuestEngine::Canceled) } else { result }
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
        let result = self.reconstruct(prior, error, generation, stop_reason);
        active.store(false, Ordering::Release);
        result
    }
    fn reconstruct(
        &mut self,
        prior: &crate::engine::session_instance::Admission,
        mut error: Failure,
        generation: u64,
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
                        self.cancel_recovered_guest(guest, error, generation)?;
                        return Ok(None);
                    }
                    return Ok(Some(guest));
                }
                Err(failure) => {
                    if matches!(
                        failure.downcast_ref::<crate::engine::gui_guest::RecoveryError>(),
                        Some(crate::engine::gui_guest::RecoveryError::ContextUnavailable)
                    ) {
                        self.output
                            .send_blocking(Output::ContextUnavailable(generation))
                            .map_err(|_| "The app window stopped receiving owner completion")?;
                        return Ok(None);
                    }
                    if matches!(
                        failure.downcast_ref::<crate::engine::guest_host::EventEnd>(),
                        Some(crate::engine::guest_host::EventEnd::Disconnected(_))
                    ) {
                        close_disconnected_guest(
                            self.profile,
                            Some(prior.context.instance),
                            &failure.to_string(),
                            generation,
                            self.output,
                        );
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
    fn cancel_recovered_guest(
        &self,
        guest: crate::engine::gui_guest::Guest,
        error: Failure,
        generation: u64,
    ) -> Result<(), Failure> {
        let instance = guest.admission().context.instance;
        let drain = guest.file_drain();
        // Quit arrived during reconstruction, before the new VM presented a window.
        drop(guest);
        let Some(crate::engine::guest_host::EventEnd::Disconnected(reason)) = drain.event_end()
        else {
            return Err(error);
        };
        drain_guest_work(&drain, self.receive)?;
        close_disconnected_guest(
            self.profile,
            Some(instance),
            &reason.to_string(),
            generation,
            self.output,
        );
        Ok(())
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
            crate::engine::gui_guest::GuestOptions {
                fuel_limit: self.fuel_limit,
                renderer: GuestRenderer::AppKit,
            },
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
impl GuestPresentation<'_> {
    fn presented_generation(&self, guest_generation: u64) -> Option<u64> {
        if self.opened { Some(guest_generation) } else { self.replaces }
    }
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
        session: Some(guest.session_identity()),
        #[cfg(all(target_os = "macos", feature = "process"))]
        startup: Some(Arc::new((profile.to_owned(), guest.admission().context))),
        pending: Arc::new(AtomicUsize::new(0)),
        inputs,
        wake: wake.clone(),
        object_id: Arc::new(AtomicU32::new(0)),
        #[cfg(feature = "process")]
        tool_pointer: Default::default(),
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
    publisher.frame(first.encoded, first.view.as_ref(), &first.logs);
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
    let mut closing = None;
    let mut rejected_input = None;
    // One diagnostic token, never an execution or admission dependency. An
    // overlapping input is explicitly unqualified rather than retained forever.
    let mut ready_output = None;
    loop {
        let _ = receive.try_recv();
        if (stop.load(Ordering::Acquire) || receiver.is_closed()) && closing.is_none() {
            handle.inputs.close();
            closing = Some(
                guest
                    .deactivate_with_inputs(receiver.len())
                    .map_err(|error| format!("Cannot deactivate app: {error:?}"))?,
            );
        }
        if let Some(roots) = &mut closing {
            // A guest can consume only part of its event queue in a turn.
            if !guest.ready(Instant::now())
                && let Ok(message) = receiver.try_recv()
            {
                let event = message.closing_event(&handle, || guest.scene());
                handle.admit_closing_event(event, &mut rejected_input, |event| {
                    guest.admit_input(event, Some(roots))
                });
            }
        } else if let Ok(message) = receiver.try_recv() {
            advance_guest(guest, &handle, Some(message), output, publisher, &mut ready_output)?;
        }
        if guest.ready(Instant::now()) {
            advance_guest(guest, &handle, None, output, publisher, &mut ready_output)?;
        }
        if closing.is_some() {
            if guest.drain_failed() {
                return Err("Accepted app work could not finish. Reopen from committed data; \
                    uncertain edits were not replayed."
                    .into());
            }
            if receiver.is_empty() && !guest.accepted_work_pending() {
                if let Some(error) = rejected_input {
                    return Err(error.into());
                }
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
    let admitted =
        if let Some(message) = message.and_then(|input| input.if_current(|| guest.scene())) {
            ack = InputAck::new(message.input.target, message.input.revision.unwrap_or_default());

            frame_input = message.input.kind == EventKind::Frame;
            pending = Some(message.pending);
            admit_guest_input(
                guest,
                handle,
                message.input,
                message.epoch,
                message.object_id,
                message.source,
            )
        } else {
            Ok(())
        };
    let frame = guest.pump().map_err(|error| error.to_string())?;
    publisher.frame(frame.encoded, frame.view.as_ref(), &frame.logs);
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
    object_id: u32,
    source: InputSource,
) -> Result<(), String> {
    if epoch != handle.epoch() {
        return Err("An earlier input failed. Try the action again.".into());
    }
    let event = source.event(input, object_id, || guest.scene());
    guest.input(event).map_err(|error| format!("Input failed: {error:?}. Try the action again."))
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
    let mut apps = Apps::with_renderer(
        Arc::new(move || {
            let _ = data_wake.try_send(());
        }),
        GuestRenderer::AppKit,
    );
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
    let mut failure = None;
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
            let generation = slot.handle.generation;
            if slot.inputs.is_closed() && slot.closing.is_none() {
                slot.closing = Some(
                    apps.deactivate_with_inputs(generation, slot.inputs.len())
                        .map_err(|error| format!("Cannot deactivate app: {error:?}"))?,
                );
            }
            if let Some(roots) = &mut slot.closing {
                if apps.step(generation, Instant::now()).map_err(|e| format!("{e:?}"))?.ready {
                    advance(apps, slot, None, output);
                } else if let Ok(message) = slot.inputs.try_recv() {
                    let event = message
                        .closing_event(&slot.handle, || apps.scene(generation).ok().flatten());
                    slot.handle.admit_closing_event(event, &mut slot.rejected_input, |event| {
                        apps.admit_input(generation, event, Some(roots))
                    });
                }
            } else if let Ok(message) = slot.inputs.try_recv() {
                advance(apps, slot, Some(message), output);
            } else if apps.step(generation, Instant::now()).map_err(|e| format!("{e:?}"))?.ready {
                advance(apps, slot, None, output);
            }
            let step =
                apps.step(slot.handle.generation, Instant::now()).map_err(|e| format!("{e:?}"))?;
            ready |= !slot.inputs.is_empty() || step.ready;
            deadline = earliest(deadline, step.wake_after.map(|delay| Instant::now() + delay));
        }
        close_drained(apps, &mut slots, output, &mut failure)?;
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
    failure.map_or(Ok(()), Err)
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
        session: apps
            .session_identity(generation)
            .map_err(|error| format!("Cannot bind native app session: {error:?}"))?,
        #[cfg(all(target_os = "macos", feature = "process"))]
        startup: None,
        inputs,
        wake: wake.clone(),
        object_id: Arc::new(AtomicU32::new(0)),
        #[cfg(feature = "process")]
        tool_pointer: Default::default(),
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
    Ok(Slot {
        handle,
        inputs: receiver,
        ready_output: Default::default(),
        closing: None,
        rejected_input: None,
    })
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
    let mut apps = Apps::with_renderer(
        Arc::new(move || {
            let _ = data_wake.try_send(());
        }),
        GuestRenderer::AppKit,
    );
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
    failure: &mut Option<Failure>,
) -> Result<(), Failure> {
    for index in (0..slots.len()).rev() {
        if slots[index].closing.is_some()
            && !apps
                .drain_pending(slots[index].handle.generation)
                .map_err(|error| format!("Accepted app work failed to drain: {error:?}"))?
            && slots[index].inputs.is_empty()
        {
            let slot = slots.remove(index);
            let generation = slot.handle.generation;
            apps.detach(generation).map_err(|error| format!("Native close failed: {error:?}"))?;
            if let Some(error) = slot.rejected_input {
                if failure.is_none() {
                    *failure = Some(error.into());
                    // The shared worker will stop. Seal each sibling's mailbox
                    // now and let its accepted work reach the same close barrier.
                    for sibling in slots.iter() {
                        sibling.handle.close();
                    }
                }
            } else {
                let _ = output.send_blocking(Output::Closed(generation));
            }
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
    let scene = || apps.scene(generation).ok().flatten();
    if let Some(message) = message.and_then(|input| input.if_current(scene)) {
        pending = Some(message.pending);
        ack = InputAck::new(message.input.target, message.input.revision.unwrap_or_default());
        frame_input = message.input.kind == EventKind::Frame;
        admitted = if message.epoch == slot.handle.epoch() {
            let event = message.source.event(message.input, message.object_id, scene);
            apps.input(generation, event)
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
