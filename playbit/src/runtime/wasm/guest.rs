//! Wasmtime instance lifecycle and the guest-memory syscall adapter.
use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use playbit_api::abi::syscall as abi;
use wasmtime::{ExternType, Instance, InstancePre, Linker, Store, Trap};

use super::Runtime;
use crate::runtime::instance::State;
use crate::runtime::syscall::{
    AvailableVolume, Event, GuestContext, Host, HostResult, Id, Participant, PendingUpdate,
    VolumeHandle,
};
use crate::runtime::{Frame, RuntimeError};

/// Wasm-only resources surrounding the portable app state.
pub(in crate::runtime) struct WasmState {
    pub(in crate::runtime) app: State,
    pub(super) fuel_limit: Option<std::num::NonZeroU64>,
    pub(super) limits: super::limits::GuestLimits,
}

impl WasmState {
    pub(super) fn new(
        context: GuestContext,
        host: Box<dyn Host>,
        fuel_limit: Option<std::num::NonZeroU64>,
    ) -> Result<Self, RuntimeError> {
        let app = State::new(context, host)?;
        let limits = super::limits::GuestLimits::new();
        Ok(Self { app, fuel_limit, limits })
    }
}

/// The direct path stores the app inline. A suspendable store uses an owner-thread
/// key so Wasmtime can hold its Send future while the host remains thread-affine.
pub(super) trait StoreData: 'static {
    fn fuel_limit(&self) -> Option<std::num::NonZeroU64>;
    fn with_app<R>(&mut self, f: impl FnOnce(&mut State) -> R) -> R;
}

impl StoreData for WasmState {
    fn fuel_limit(&self) -> Option<std::num::NonZeroU64> {
        self.fuel_limit
    }
    #[inline]
    fn with_app<R>(&mut self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.app)
    }
}

pub(in crate::runtime) enum GuestStore {
    Direct(Store<WasmState>),
    Deadline(super::deadline::DeadlineGuest),
}

pub struct GuestInstance {
    pub(in crate::runtime) store: GuestStore,
    pub(in crate::runtime) instance: Instance,
}

/// Compiled code has no VM state or thread affinity. Instantiate it on the OS
/// thread that will own the VM; compilation may run on the background worker.
pub struct PreparedGuest {
    runtime: Runtime,
    instance: InstancePre<WasmState>,
}
impl std::fmt::Debug for PreparedGuest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedGuest")
    }
}

impl Runtime {
    /// Validates the v1 contract without instantiation or guest side effects.
    pub fn validate_guest(&self, wasm: &[u8]) -> Result<(), RuntimeError> {
        self.prepare_guest(wasm).map(drop)
    }

    /// Compile the guest and check the v1 contract, leaving it ready to
    /// instantiate. Validation and creation share this so a guest is compiled
    /// once per load, not once to check it and again to run it.
    pub fn prepare_guest(&self, wasm: &[u8]) -> Result<PreparedGuest, RuntimeError> {
        let module = self.compile_app(wasm)?;
        self.prepare_module(module)
    }

    #[cfg(all(
        feature = "local-store",
        any(test, all(not(target_os = "ios"), feature = "process"))
    ))]
    pub(crate) fn prepare_compiled(
        &self,
        wasm: &[u8],
        image: super::compiled::Image,
    ) -> Result<PreparedGuest, RuntimeError> {
        if blake3::hash(wasm).as_bytes() != &image.descriptor.wasm {
            return Err(RuntimeError::InvalidModule(
                "compiled image belongs to different Wasm".into(),
            ));
        }
        let module = self
            .load_image(&image)
            .map_err(|error| RuntimeError::InvalidModule(error.to_string()))?;
        self.prepare_module(module)
    }

    fn prepare_module(&self, module: wasmtime::Module) -> Result<PreparedGuest, RuntimeError> {
        let imports: Vec<_> = module.imports().collect();
        if imports.len() > abi::REGISTRY.len()
            || imports.iter().any(|import| {
                import.module() != abi::IMPORT_MODULE
                    || import.name() != "pb_syscall"
                    || !matches!(import.ty(), ExternType::Func(_))
            })
        {
            return Err(RuntimeError::InvalidModule(
                "guests may import only pb_syscall".to_owned(),
            ));
        }
        match module.get_export(abi::MEMORY_EXPORT) {
            Some(ExternType::Memory(_)) => {}
            Some(_) => return Err(RuntimeError::InvalidMemoryExport),
            None => return Err(RuntimeError::MissingMemory),
        }
        match module.get_export(abi::MAIN_EXPORT) {
            Some(ExternType::Func(function))
                if function.params().len() == 0 && function.results().len() == 0 => {}
            Some(_) => {
                return Err(RuntimeError::InvalidMainSignature("expected () -> ()".to_owned()));
            }
            None => return Err(RuntimeError::MissingMain),
        }
        let mut linker = Linker::<WasmState>::new(&self.engine);
        super::operations::define(&mut linker)
            .map_err(|error| RuntimeError::Linker(error.to_string()))?;
        let instance = linker
            .instantiate_pre(&module)
            .map_err(|error| RuntimeError::Instantiation(error.to_string()))?;
        Ok(PreparedGuest { runtime: self.clone(), instance })
    }

    /// Prepare on a runtime worker, never the native UI thread. The host is a
    /// principal-bound adapter; its implementation must reauthorize every call.
    pub fn create_guest(
        &self,
        wasm: &[u8],
        context: GuestContext,
        host: Box<dyn Host>,
    ) -> Result<GuestInstance, RuntimeError> {
        self.prepare_guest(wasm)?.instantiate(context, host)
    }
}

impl PreparedGuest {
    pub fn instantiate(
        self,
        context: GuestContext,
        host: Box<dyn Host>,
    ) -> Result<GuestInstance, RuntimeError> {
        let _timing = crate::trace::Span::new("runtime.instantiate");
        let state = WasmState::new(context, host, self.runtime.fuel_limit)?;
        if state.app.supported.contains(&abi::Opcode::EventPollDeadline) {
            let (guest, instance) =
                super::deadline::DeadlineGuest::instantiate(self.instance.module(), state)?;
            return Ok(GuestInstance { store: GuestStore::Deadline(guest), instance });
        }
        let mut store = Store::new(&self.runtime.engine, state);
        store.limiter(|state| &mut state.limits);
        if let Some(limit) = self.runtime.fuel_limit {
            store.set_fuel(limit.get()).map_err(|error| RuntimeError::Engine(error.to_string()))?;
        }
        let instance = self
            .instance
            .instantiate(&mut store)
            .map_err(|error| RuntimeError::Instantiation(error.to_string()))?;
        Ok(GuestInstance { store: GuestStore::Direct(store), instance })
    }
}

impl GuestInstance {
    fn read_state<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        match &self.store {
            GuestStore::Direct(store) => f(&store.data().app),
            GuestStore::Deadline(guest) => f(&guest.state.borrow()),
        }
    }
    fn update_state<T>(&mut self, f: impl FnOnce(&mut State) -> T) -> T {
        match &mut self.store {
            GuestStore::Direct(store) => f(&mut store.data_mut().app),
            GuestStore::Deadline(guest) => f(&mut guest.state.borrow_mut()),
        }
    }
    pub fn generation(&self) -> u64 {
        self.read_state(|state| state.generation())
    }

    pub fn volume_handle(&self, id: Id) -> Option<VolumeHandle> {
        self.read_state(|state| state.volume_handle(id))
    }

    /// Change only the available-volume ceiling on the VM's owning thread.
    /// The host still authorizes each operation against current policy. Removed
    /// slots are never reassigned, even when that same volume is granted again.
    /// The caller schedules DataChanged after successful adoption.
    pub fn replace_volumes(&mut self, volumes: Vec<AvailableVolume>) -> HostResult<bool> {
        self.update_state(|state| state.replace_volumes(volumes))
    }

    /// Update the local account label on the guest's serialized owner.
    pub fn set_display_name(&mut self, name: &str) -> HostResult<()> {
        self.update_state(|state| state.set_display_name(name))
    }

    pub(crate) fn deactivate_with_inputs(
        &mut self,
        count: usize,
    ) -> HostResult<crate::runtime::work::ClosingInputs> {
        self.update_state(|state| state.deactivate_with_inputs(count))
    }
    pub(crate) fn queue_input(
        &mut self,
        generation: u64,
        event: Event,
        closing: Option<&mut crate::runtime::work::ClosingInputs>,
    ) -> HostResult<()> {
        self.update_state(|state| state.queue_event(generation, event, closing))
    }
    pub fn queue_event(&mut self, generation: u64, event: Event) -> HostResult<()> {
        self.queue_input(generation, event, None)
    }

    /// Signal local result readiness on the serialized runtime worker. Only a
    /// current pending token can produce an event; neither token nor result is
    /// exposed. Repeated notifications are coalesced while the event is queued.
    pub fn notify_data(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.update_state(|state| state.notify_data(generation, token))
    }

    pub fn notify_creation(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.update_state(|state| state.notify_creation(generation, token))
    }

    pub fn notify_file_copy(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.update_state(|state| state.notify_file_copy(generation, token))
    }

    pub fn notify_file_selection(&mut self, generation: u64, token: u64) -> HostResult<()> {
        self.update_state(|state| state.notify_file_selection(generation, token))
    }

    pub fn notify_update(&mut self, generation: u64, update: PendingUpdate) -> HostResult<()> {
        self.update_state(|state| state.notify_update(generation, update))
    }

    pub fn set_participants(
        &mut self,
        generation: u64,
        participants: Vec<Participant>,
    ) -> HostResult<()> {
        self.update_state(|state| state.set_participants(generation, participants))
    }

    pub fn pending_update(&self) -> Option<PendingUpdate> {
        self.read_state(|state| state.pending_update())
    }
    pub fn deferred_update(&self) -> Option<u64> {
        self.read_state(|state| state.deferred_update())
    }

    /// Runs one cooperative turn. Guests drain EventPoll and return when it
    /// reports WouldBlock. An explicit fuel limit traps and halts on exhaustion.
    pub fn pump(&mut self) -> Result<Frame, RuntimeError> {
        let _timing = crate::trace::Span::new("runtime.pump");
        self.update_state(|state| state.begin_turn(Instant::now()))?;
        let result = match &mut self.store {
            GuestStore::Direct(store) => {
                if let Some(limit) = store.data().fuel_limit {
                    store
                        .set_fuel(limit.get())
                        .map_err(|error| RuntimeError::Engine(error.to_string()))?;
                }
                let main = self
                    .instance
                    .get_typed_func::<(), ()>(&mut *store, abi::MAIN_EXPORT)
                    .map_err(|error| RuntimeError::InvalidMainSignature(error.to_string()))?;
                main.call(store, ())
            }
            GuestStore::Deadline(guest) => guest.advance(),
        };
        if let Err(error) = result {
            self.update_state(State::halt);
            // A trap is terminal for this generation. No host can accidentally
            // resume partially executed Wasm by pumping it or queuing new input.
            crate::diagnostic!(
                "Guest generation {} trapped and is halted; reload required",
                self.generation()
            );
            if error.downcast_ref::<Trap>() == Some(&Trap::OutOfFuel) {
                return Err(RuntimeError::FuelExhausted);
            }
            // Keep the final diagnostic, including Rust panic details, without a completed frame.
            let mut report = self.update_state(|state| state.logs.pop()).unwrap_or_default();
            if !report.is_empty() {
                report.push('\n');
            }
            write!(report, "{error:#}").expect("writing to a String cannot fail");
            return Err(RuntimeError::GuestTrap(report));
        }
        Ok(self.update_state(State::finish_frame))
    }

    /// A turn is needed only for queued events or an expired wake deadline.
    /// Querying readiness does not consume events or advance the deadline.
    pub fn has_ready_work(&self, now: Instant) -> bool {
        self.read_state(|state| state.has_ready_work(now))
    }
    pub fn deactivate(&mut self) {
        self.deactivate_with_inputs(0).expect("an empty closing mailbox always fits");
    }
    pub fn drain_pending(&self) -> bool {
        self.read_state(|state| state.drain_pending())
    }
    pub fn drain_failed(&self) -> bool {
        self.read_state(|state| state.drain_failed())
    }

    /// Hosts check this on their serialized worker without running idle Wasm.
    /// This deadline belongs only to this guest generation and is not durable.
    pub fn wake_due(&self, now: Instant) -> bool {
        self.read_state(|state| state.wake_due(now))
    }

    pub fn wake_after(&self, now: Instant) -> Option<Duration> {
        self.read_state(|state| state.wake_after(now))
    }

    /// Called by ManagedGuest after checking the accepted scene's reference.
    pub(crate) fn file_descriptor(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
    ) -> HostResult<(u64, [u8; 32])> {
        self.update_state(|state| state.file_descriptor(generation, file))
    }

    /// Called by ManagedGuest after checking the accepted scene's reference.
    pub(crate) fn read_file(
        &mut self,
        generation: u64,
        file: playbit_api::ui::FileReference,
        max_bytes: usize,
    ) -> HostResult<([u8; 32], Vec<u8>)> {
        self.update_state(|state| state.read_file(generation, file, max_bytes))
    }
}

// Low-level ABI fixtures call arbitrary exports of a direct store to inspect
// individual memory effects. Production callers enter exclusively through pump.
#[cfg(any(test, feature = "test-support"))]
impl std::ops::Deref for GuestStore {
    type Target = Store<WasmState>;
    fn deref(&self) -> &Self::Target {
        let Self::Direct(store) = self else { panic!("ABI fixture requires a direct store") };
        store
    }
}
#[cfg(any(test, feature = "test-support"))]
impl std::ops::DerefMut for GuestStore {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let Self::Direct(store) = self else { panic!("ABI fixture requires a direct store") };
        store
    }
}
#[cfg(any(test, feature = "test-support"))]
impl wasmtime::AsContext for GuestStore {
    type Data = WasmState;
    fn as_context(&self) -> wasmtime::StoreContext<'_, Self::Data> {
        (**self).as_context()
    }
}
#[cfg(any(test, feature = "test-support"))]
impl wasmtime::AsContextMut for GuestStore {
    fn as_context_mut(&mut self) -> wasmtime::StoreContextMut<'_, Self::Data> {
        (**self).as_context_mut()
    }
}
