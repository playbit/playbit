//! Shared in-process File scheduler. Two workers own filesystem I/O; one
//! readiness-driven scheduler drains the bounded executor without UI callbacks.
use super::{executor::Executor, host::PrivateObjects, private_storage::Managed};
use crate::{
    runtime::host::{HostResult, files::capacity::Retention},
    store::Id,
};
use playbit_api::abi::syscall::Error;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
};

#[derive(Default)]
struct Signal {
    sequence: Mutex<(u64, bool)>,
    changed: Condvar,
}
impl Signal {
    fn notify(&self) {
        let mut state = self.sequence.lock().expect("File scheduler signal lock is not poisoned");
        state.0 = state.0.checked_add(1).expect("File scheduler wake sequence exhausted");
        self.changed.notify_all();
    }
    fn sequence(&self) -> u64 {
        self.sequence.lock().expect("File scheduler signal lock is not poisoned").0
    }
    fn wait(&self, before: u64) -> bool {
        let state = self.sequence.lock().expect("File scheduler signal lock is not poisoned");
        let state = self
            .changed
            .wait_while(state, |state| state.0 == before && !state.1)
            .expect("File scheduler signal lock is not poisoned");
        !state.1
    }
}
struct State {
    executor: Executor,
    wakes: BTreeMap<Id, Arc<dyn Fn() + Send + Sync>>,
    retired: BTreeMap<Id, Vec<Retention>>,
    failed: bool,
}
/// Services clones share this owner and its process resource budget. Construction
/// and initial root preparation happen before the runtime enters guest code.
pub struct Owner {
    state: Arc<Mutex<State>>,
    signal: Arc<Signal>,
    thread: Option<JoinHandle<()>>,
    pub placement: Arc<Managed>,
    pub transport: super::super::blob::BlobTransport,
}
impl Owner {
    pub fn new(profile: PathBuf) -> std::io::Result<Self> {
        let signal = Arc::new(Signal::default());
        let notify = signal.clone();
        let executor = Executor::new(move || notify.notify())?;
        let private = Arc::new(PrivateObjects::default());
        #[cfg(all(feature = "wasm-runtime", feature = "local-store"))]
        super::super::compiled_cache::protect(&profile, &private)?;
        let state = Arc::new(Mutex::new(State {
            executor,
            wakes: BTreeMap::new(),
            retired: BTreeMap::new(),
            failed: false,
        }));
        let worker_state = state.clone();
        let worker_signal = signal.clone();
        let thread = std::thread::Builder::new().name("local-files".into()).spawn(move || {
            loop {
                let before = worker_signal.sequence();
                let wakes = {
                    let mut state =
                        worker_state.lock().expect("File scheduler lock is not poisoned");
                    if state.executor.maintain().is_err() {
                        state.failed = true;
                    }
                    let State { executor, retired, .. } = &mut *state;
                    retired.retain(|owner, _| executor.owner_retained(*owner));
                    state.wakes.values().cloned().collect::<Vec<_>>()
                };
                for wake in wakes {
                    wake();
                }
                if !worker_signal.wait(before) {
                    break;
                }
            }
        })?;
        Ok(Self {
            state,
            signal,
            thread: Some(thread),
            placement: Arc::new(Managed::new(profile, private)),
            transport: super::super::blob::BlobTransport::local(),
        })
    }
    pub fn register(&self, owner: Id, wake: Arc<dyn Fn() + Send + Sync>) -> HostResult<()> {
        let mut state = self.state.lock().map_err(|_| Error::Internal)?;
        if state.failed {
            return Err(Error::Unavailable);
        }
        if state.wakes.contains_key(&owner) {
            return Err(Error::Conflict);
        }
        // Executor leases cap live and retiring contexts at eight.
        let State { executor, retired, .. } = &mut *state;
        retired.retain(|owner, _| executor.owner_retained(*owner));
        state.executor.reserve_context(owner)?;
        state.wakes.insert(owner, wake);
        Ok(())
    }
    pub fn with<T>(&self, operation: impl FnOnce(&mut Executor) -> HostResult<T>) -> HostResult<T> {
        let result = {
            let mut state = self.state.lock().map_err(|_| Error::Internal)?;
            if state.failed {
                return Err(Error::Unavailable);
            }
            operation(&mut state.executor)
        };
        if result.is_ok() {
            self.signal.notify();
        }
        result
    }
    pub fn retire(&self, owner: Id, retention: Vec<Retention>) {
        let mut state = self.state.lock().expect("File scheduler lock is not poisoned");
        state.wakes.remove(&owner);
        state.executor.retire(owner);
        let State { executor, retired, .. } = &mut *state;
        retired.retain(|owner, _| executor.owner_retained(*owner));
        if executor.owner_retained(owner) {
            assert!(retired.insert(owner, retention).is_none(), "File owner retires once");
        }
        assert!(retired.len() <= 8, "retiring owners retain bounded context leases");
        drop(state);
        self.signal.notify();
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.signal.sequence.lock().expect("File scheduler signal lock is not poisoned").1 = true;
        self.signal.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Executor drop drains accepted writes/closes and worker-side cleanup.
    }
}
