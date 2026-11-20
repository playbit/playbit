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
    ffi::c_void,
    marker::PhantomData,
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
}
struct ActiveTurn<'a>(&'a AtomicBool);
impl Drop for ActiveTurn<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct NativeInstance {
    context: Box<Context>,
    app: abi::App,
    app_state: *mut c_void,
    initialized: bool,
    // Construction, execution and destruction belong to the same host thread.
    _owner: PhantomData<Rc<()>>,
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
        Ok(Self {
            context: Box::new(Context {
                state: Mutex::new(State::new(context, host)?),
                owner: std::thread::current().id(),
                active: AtomicBool::new(false),
            }),
            app,
            app_state: std::ptr::null_mut(),
            initialized: false,
            _owner: PhantomData,
        })
    }
    fn context_ptr(&self) -> *mut abi::Context {
        (&*self.context as *const Context).cast_mut().cast()
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.context.state.lock().expect("native instance state is not poisoned")
    }
    /// Execute init once and one cooperative turn, without holding syscall locks
    /// across callbacks. Failure halts this generation; it never retries input.
    pub fn pump(&mut self) -> Result<Frame, RuntimeError> {
        let _timing = crate::trace::Span::new("runtime.native_turn");
        self.state().begin_turn(Instant::now())?;
        assert!(!self.context.active.swap(true, Ordering::AcqRel), "native turn reentry");
        let _active = ActiveTurn(&self.context.active);
        let context = self.context_ptr();
        if !self.initialized {
            self.initialized = true;
            // SAFETY: new validated the table and the caller owns its lifetime.
            let status = unsafe {
                self.app.init.expect("validated native init")(
                    context,
                    self.app.user_data,
                    &mut self.app_state,
                )
            };
            if status != 0 {
                self.state().halt();
                return Err(RuntimeError::NativeApp(status));
            }
        }
        // SAFETY: init transferred app_state ownership, valid until destroy.
        let status =
            unsafe { self.app.turn.expect("validated native turn")(context, self.app_state) };
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
        if self.initialized {
            // SAFETY: the callback contract transfers even failed init state;
            // this is the only destruction call and the context is inactive.
            unsafe {
                self.app.destroy.expect("validated native destroy")(self.app_state);
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
