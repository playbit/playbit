//! Cooperative native app execution. The host owns one instance and its thread.
//! Native app code shares the process and must not unwind across C callbacks.
//! There is no VM, fuel preemption, implicit current context or syscall scratch.
#![allow(
    unsafe_code,
    reason = "This adapter invokes the explicit C app lifecycle; unsafe constructors establish callback and user-data lifetimes."
)]
use super::{Frame, RuntimeError, instance::State, syscall::*};
use playbit_api::abi::operations as abi;
use std::{
    cell::Cell,
    ffi::c_void,
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

mod async_data;
mod audio;
mod authenticated;
mod bloblog;
#[cfg(test)]
mod bloblog_tests;
mod calls;
mod data;
mod deadline;
mod dispatch;
mod environment;
mod files;
mod kv;
#[cfg(test)]
mod kv_tests;
mod memory;
mod objects;
mod pending;
mod record;
mod schema;

struct Context {
    state: Mutex<State>,
    owner: ThreadId,
    active: AtomicBool,
    poll_yielder: Cell<*const deadline::PollYielder>,
}
struct ActiveTurn<'a>(&'a AtomicBool);
impl Drop for ActiveTurn<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Callbacks {
    app: abi::App,
    state: Cell<*mut c_void>,
    initialized: Cell<bool>,
}
impl Callbacks {
    fn turn(&self, context: &Context) -> i32 {
        let raw = std::ptr::from_ref(context).cast_mut().cast();
        if !self.initialized.replace(true) {
            // The native guest owns its state between init and destroy. Preserve even
            // the partial state transferred by an init that reports an error.
            let mut state = std::ptr::null_mut();
            let status = unsafe {
                self.app.init.expect("validated native init")(raw, self.app.user_data, &mut state)
            };
            self.state.set(state);
            if status != 0 || !context.active.load(Ordering::Acquire) {
                return status;
            }
        }
        // SAFETY: all callbacks run serially on their constructing OS thread.
        unsafe { self.app.turn.expect("validated native turn")(raw, self.state.get()) }
    }
}
pub struct NativeInstance {
    context: Rc<Context>,
    callbacks: Rc<Callbacks>,
    deadline_turn: Option<deadline::Turn>,
}
impl NativeInstance {
    /// Bind a native app to the same checked authority as a Wasm instance.
    /// No app callback runs until the first `pump`.
    ///
    /// # Safety
    /// Callbacks must obey `playbit/sys/abi.h`, including valid user_data through
    /// destruction, cooperative return, state ownership and no unwinding.
    pub unsafe fn new(
        context: GuestContext,
        host: Box<dyn Host>,
        app: abi::App,
    ) -> Result<Self, RuntimeError> {
        if app.init.is_none() || app.turn.is_none() || app.destroy.is_none() {
            return Err(RuntimeError::Instantiation(
                "native app requires init, turn and destroy callbacks".into(),
            ));
        }
        let deadline_enabled = context.required_capabilities.contains("event.poll.deadline");
        let context = Rc::new(Context {
            state: Mutex::new(State::new(context, host)?),
            owner: std::thread::current().id(),
            active: AtomicBool::new(false),
            poll_yielder: Cell::new(std::ptr::null()),
        });
        let callbacks = Rc::new(Callbacks {
            app,
            state: Cell::new(std::ptr::null_mut()),
            initialized: Cell::new(false),
        });
        let deadline_turn = if deadline_enabled {
            Some(deadline::Turn::new(context.clone(), callbacks.clone())?)
        } else {
            None
        };
        Ok(Self { context, callbacks, deadline_turn })
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.context.state.lock().expect("native instance state is not poisoned")
    }
    /// Execute init once and one cooperative turn, without holding syscall locks
    /// across callbacks. Failure halts this generation; it never retries input.
    pub fn pump(&mut self) -> Result<Frame, RuntimeError> {
        let _timing = crate::trace::Span::new("runtime.native_turn");
        let now = Instant::now();
        {
            let mut state = self.state();
            state.begin_turn(now)?;
            if !self.context.poll_yielder.get().is_null()
                && state.work.draining
                && !state.has_ready_work(now)
            {
                return Ok(state.finish_frame());
            }
        }
        assert!(!self.context.active.swap(true, Ordering::AcqRel), "native turn reentry");
        let _active = ActiveTurn(&self.context.active);
        let status = match &mut self.deadline_turn {
            Some(turn) => turn.advance(),
            None => self.callbacks.turn(&self.context),
        };
        if status != 0 {
            self.state().halt();
            return Err(RuntimeError::NativeApp(status));
        }
        Ok(self.state().finish_frame())
    }
    pub fn generation(&self) -> u64 {
        self.state().generation()
    }
    #[cfg(feature = "local-store")]
    pub(crate) fn instance_id(&self) -> [u8; 16] {
        self.state().context.identity.instance_id
    }
    pub fn volume_handle(&self, id: Id) -> Option<VolumeHandle> {
        self.state().volume_handle(id)
    }
    pub fn replace_volumes(&mut self, volumes: Vec<AvailableVolume>) -> HostResult<bool> {
        self.state().replace_volumes(volumes)
    }
    pub fn queue_event(&mut self, generation: u64, event: Event) -> HostResult<()> {
        self.state().queue_event(generation, event)
    }
    pub fn notify_data(&mut self, generation: u64, token: u64) -> Result<(), HostError> {
        self.state().notify_data(generation, token)
    }

    pub fn notify_creation(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.state().notify_creation(generation, token)
    }
    pub fn notify_file_copy(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.state().notify_file_copy(generation, token)
    }
    pub fn notify_file_selection(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.state().notify_file_selection(generation, token)
    }
    pub fn notify_update(&mut self, generation: u64, update: PendingUpdate) -> HostResult<()> {
        self.state().notify_update(generation, update)
    }
    pub fn set_participants(
        &mut self,
        generation: u64,
        participants: Vec<Participant>,
    ) -> HostResult<()> {
        self.state().set_participants(generation, participants)
    }
    pub fn pending_update(&self) -> Option<PendingUpdate> {
        self.state().pending_update()
    }
    pub fn deferred_update(&self) -> Option<u64> {
        self.state().deferred_update()
    }
    pub fn is_halted(&self) -> bool {
        self.state().halted
    }
    pub fn has_ready_work(&self, now: Instant) -> bool {
        self.state().has_ready_work(now)
    }
    pub fn deactivate(&mut self) {
        self.state().deactivate();
    }
    pub fn drain_pending(&self) -> bool {
        self.state().drain_pending()
    }
    pub fn drain_failed(&self) -> bool {
        self.state().drain_failed()
    }
    pub fn wake_due(&self, now: Instant) -> bool {
        self.state().wake_due(now)
    }
    pub fn wake_after(&self, now: Instant) -> Option<Duration> {
        self.state().wake_after(now)
    }
    /// The renderer must first verify the file occurs in its accepted scene.
    pub fn file_descriptor(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
    ) -> HostResult<(u64, [u8; 32])> {
        self.state().file_descriptor(generation, file)
    }
    /// The renderer must first verify the file occurs in its accepted scene.
    pub fn read_file(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
        max_bytes: usize,
    ) -> HostResult<([u8; 32], Vec<u8>)> {
        self.state().read_file(generation, file, max_bytes)
    }
}
impl Drop for NativeInstance {
    fn drop(&mut self) {
        self.context.active.store(false, Ordering::Release);
        self.state().halt();
        if let Some(turn) = &mut self.deadline_turn {
            turn.retire();
        }
        if self.callbacks.initialized.get() {
            // SAFETY: the callback contract transfers even failed init state;
            // this is the only destruction call and the context is inactive.
            unsafe {
                self.callbacks.app.destroy.expect("validated native destroy")(
                    self.callbacks.state.get(),
                );
            }
        }
    }
}

#[cfg(test)]
fn test_handle(result: i64, output: &mut playbit_api::abi::pending::OperationHandle) -> i64 {
    if result > 0 {
        *output = playbit_api::abi::pending::OperationHandle(result as u64);
        0
    } else {
        result
    }
}
