//! Only a guest that requires deadline polling runs on a Wasmtime-managed
//! fiber. Its VM call may be suspended, while its state and Host always belong
//! to the serialized pump thread. No guest memory borrow survives a yield.
use super::{
    guest::{StoreData, WasmState},
    limits::GuestLimits,
    operations,
};
use crate::runtime::{RuntimeError, instance::State};
use playbit_api::abi::{dispatch, syscall as abi};
use std::{
    cell::RefCell,
    future::Future,
    num::NonZeroU64,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};
use wasmtime::{Caller, Instance, Linker, Module, Store, TypedFunc};

// Wasmtime requires Send store data for public async execution. Host itself can
// be thread-affine. Install the exact state only while polling this owner's VM;
// the store retains just an identity and cannot dereference a pointer or access
// another thread's Host. Nested calls restore the previous owner on every exit.
thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

struct Active(Option<Rc<RefCell<State>>>);
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.replace(self.0.take());
    }
}
fn enter<R>(state: &Rc<RefCell<State>>, f: impl FnOnce() -> R) -> R {
    let _active = Active(ACTIVE.replace(Some(state.clone())));
    f()
}

pub(super) struct DeadlineData {
    owner: usize,
    fuel_limit: Option<NonZeroU64>,
    limits: GuestLimits,
}

impl StoreData for DeadlineData {
    fn fuel_limit(&self) -> Option<NonZeroU64> {
        self.fuel_limit
    }

    fn with_app<R>(&mut self, f: impl FnOnce(&mut State) -> R) -> R {
        let state =
            ACTIVE.with_borrow(|slot| slot.as_ref().expect("guest pump owns its VM").clone());
        assert_eq!(self.owner, Rc::as_ptr(&state) as usize, "syscall on a different guest owner");
        f(&mut state.borrow_mut())
    }
}

type GuestCall = Pin<Box<dyn Future<Output = (Store<DeadlineData>, wasmtime::Result<()>)>>>;

pub(in crate::runtime) struct DeadlineGuest {
    pub(super) state: Rc<RefCell<State>>,
    main: TypedFunc<(), ()>,
    idle: Option<Store<DeadlineData>>,
    call: Option<GuestCall>,
}

impl DeadlineGuest {
    pub(super) fn instantiate(
        module: &Module,
        state: WasmState,
    ) -> Result<(Self, Instance), RuntimeError> {
        let WasmState { app, fuel_limit, limits } = state;
        let state = Rc::new(RefCell::new(app));
        let data = DeadlineData { owner: Rc::as_ptr(&state) as usize, fuel_limit, limits };
        let mut store = Store::new(module.engine(), data);
        store.limiter(|state| &mut state.limits);
        if let Some(fuel) = fuel_limit {
            store.set_fuel(fuel.get()).map_err(|error| RuntimeError::Engine(error.to_string()))?;
        }
        let mut linker = Linker::new(module.engine());
        linker
            .func_wrap_async(
                abi::IMPORT_MODULE,
                "pb_syscall",
                |caller: Caller<'_, DeadlineData>,
                 (op, a, b, c, d, e, f): (u32, u64, u64, u64, u64, u64, u64)| {
                    Box::new(async move {
                        let args = [a, b, c, d, e, f];
                        if op == abi::Opcode::EventPollDeadline as u32 {
                            if let Err(error) = dispatch::admit(op, &args, 32) {
                                return error as i64;
                            }
                            operations::control::deadline(caller, a as u32, b as u32, c as u32, d)
                                .await
                        } else {
                            operations::dispatch(caller, op, args)
                        }
                    })
                },
            )
            .map_err(|error| RuntimeError::Linker(error.to_string()))?;
        // Start functions are rejected during compilation. There is no guest
        // instruction or host wait while constructing the instance.
        let prepared = linker
            .instantiate_pre(module)
            .map_err(|e| RuntimeError::Instantiation(e.to_string()))?;
        let instance = {
            let mut build = std::pin::pin!(prepared.instantiate_async(&mut store));
            let Poll::Ready(result) =
                enter(&state, || build.as_mut().poll(&mut Context::from_waker(Waker::noop())))
            else {
                return Err(RuntimeError::Instantiation(
                    "Guest suspended during instantiation".into(),
                ));
            };
            result.map_err(|e| RuntimeError::Instantiation(e.to_string()))?
        };
        let main = instance
            .get_typed_func::<(), ()>(&mut store, abi::MAIN_EXPORT)
            .map_err(|error| RuntimeError::InvalidMainSignature(error.to_string()))?;
        Ok((Self { state, main, idle: Some(store), call: None }, instance))
    }

    pub(super) fn advance(&mut self) -> wasmtime::Result<()> {
        if self.call.is_some() {
            let state = self.state.borrow();
            // A poll deadline is retired before the worker drains accepted roots.
            // Empty host notifications must not resume unrelated timeout code.
            if state.work.draining && !state.has_ready_work(std::time::Instant::now()) {
                return Ok(());
            }
        }
        if let Some(mut store) = self.idle.take() {
            if let Some(fuel) = store.data().fuel_limit {
                store.set_fuel(fuel.get())?;
            }
            let main = self.main.clone();
            self.call = Some(Box::pin(async move {
                let result = main.call_async(&mut store, ()).await;
                (store, result)
            }));
        }
        let call = self.call.as_mut().expect("one idle store or one active guest call");
        match enter(&self.state, || call.as_mut().poll(&mut Context::from_waker(Waker::noop()))) {
            Poll::Pending => {
                assert!(
                    self.state.borrow().poll_deadline.is_some(),
                    "only event polling yields the VM"
                );
                Ok(())
            }
            Poll::Ready((store, result)) => {
                self.call = None;
                self.idle = Some(store);
                result
            }
        }
    }
}

impl Drop for DeadlineGuest {
    fn drop(&mut self) {
        // Wasmtime cancels the pending host future and unwinds its own stack.
        // Run its cancellation with the same owner binding used during execution.
        if let Some(call) = self.call.take() {
            enter(&self.state, || drop(call));
        }
    }
}
