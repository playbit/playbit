//! Accepted event roots keep finite completion dependencies alive during Quit.
use super::host::HostResult;
use playbit_api::abi::syscall as abi;
use playbit_api::abi::syscall::Error as HostError;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) use abi::{MAX_QUEUED_EVENT_BYTES, MAX_QUEUED_EVENTS};

const MAX_ROOTS: usize = MAX_QUEUED_EVENTS + abi::MAX_PENDING_OPERATIONS + 1;
// A supported 64-MiB file needs 1,024 guest reads at 64 KiB per chunk.
// Allow another full readback and 128 metadata/pagination dependencies. This
// remains a finite per-root drain bound, not a guest instruction budget.
pub(crate) const DRAIN_STEPS: u16 = 2 * 1024 + 128;

/// Each input in an owner's sealed mailbox may still introduce its accepted
/// work. Discarded inputs never use a runtime queue slot or a root identity.
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) struct ClosingInputs {
    generation: u64,
    remaining: usize,
}
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) fn discard_on_deactivate(kind: abi::EventKind) -> bool {
    matches!(kind, abi::EventKind::Wake | abi::EventKind::Frame | abi::EventKind::VolumesChanged)
}

pub(crate) struct Work {
    pub draining: bool,
    pub failed: bool,
    pub progress: bool,
    pub active: Option<u64>,
    pub pending: BTreeMap<u64, u64>,
    pub remaining: BTreeMap<u64, u16>,
    next: u64,
}
impl Work {
    pub fn new() -> Self {
        Self {
            draining: false,
            failed: false,
            progress: false,
            active: None,
            pending: BTreeMap::new(),
            remaining: BTreeMap::from([(1, DRAIN_STEPS)]),
            next: 2,
        }
    }
    pub fn admit_root(&self) -> HostResult<()> {
        if self.draining {
            return Err(HostError::Conflict);
        }
        self.check_root_capacity()
    }
    fn check_root_capacity(&self) -> HostResult<()> {
        // The browser bridge returns positive i64 roots and negative ABI errors.
        if !root_fits(self.next) || self.remaining.len() >= MAX_ROOTS {
            return Err(HostError::LimitExceeded);
        }
        Ok(())
    }
    pub fn new_root(&mut self) -> HostResult<u64> {
        self.admit_root()?;
        Ok(self.allocate_root())
    }
    fn allocate_root(&mut self) -> u64 {
        let root = self.next;
        self.next = root.checked_add(1).unwrap_or(0);
        assert!(self.remaining.insert(root, DRAIN_STEPS).is_none(), "accepted roots never repeat");
        root
    }
    pub fn admit_step(&self) -> HostResult<()> {
        match self.active {
            Some(root) if step_allowed(self.draining, self.remaining.get(&root).copied()) => Ok(()),
            Some(_) => Err(HostError::LimitExceeded),
            None => self.admit_root(),
        }
    }
    /// Called only after admission and successful host begin, on this owner.
    pub fn register(&mut self, handle: u64) {
        self.progress = true;
        let root = match self.active {
            Some(root) => root,
            None => {
                let root = self.new_root().expect("operation root was admitted before host begin");
                self.active = Some(root);
                root
            }
        };
        let remaining = self.remaining.get_mut(&root).expect("active work root is retained");
        if self.draining {
            *remaining =
                remaining.checked_sub(1).expect("dependent step was admitted before host begin");
        }
        assert!(
            self.pending.insert(handle, root).is_none(),
            "live operation identities are unique"
        );
    }
    pub fn reap(&mut self, queued: impl IntoIterator<Item = u64>) {
        let live: BTreeSet<_> =
            self.active.into_iter().chain(self.pending.values().copied()).chain(queued).collect();
        self.remaining.retain(|root, _| live.contains(root));
    }
    pub fn finish_turn(&mut self, queued: bool) {
        if self.draining && !self.progress && queued {
            self.failed = true;
        }
        self.active = None;
    }
    pub fn deactivate(&mut self) {
        assert!(self.active.is_none(), "deactivation happens between guest turns");
        self.draining = true;
    }
}
#[cfg(any(feature = "native-runtime", feature = "wasm-runtime"))]
impl Work {
    pub fn seal_inputs(&self, generation: u64, count: usize) -> HostResult<ClosingInputs> {
        if self.draining && count != 0 {
            return Err(HostError::Conflict);
        }
        if count > MAX_QUEUED_EVENTS {
            return Err(HostError::LimitExceeded);
        }
        Ok(ClosingInputs { generation, remaining: count })
    }
    pub fn take_closing_input(
        &mut self,
        generation: u64,
        inputs: &mut ClosingInputs,
    ) -> HostResult<u64> {
        if !self.draining || inputs.generation != generation || inputs.remaining == 0 {
            return Err(HostError::Conflict);
        }
        inputs.remaining -= 1;
        self.check_root_capacity()?;
        Ok(self.allocate_root())
    }
}
fn step_allowed(draining: bool, remaining: Option<u16>) -> bool {
    remaining.is_some_and(|remaining| !draining || remaining > 0)
}
fn root_fits(root: u64) -> bool {
    root != 0 && root <= i64::MAX as u64
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn browser_operation_fits(count: usize, held: usize, bytes: u64) -> bool {
    use super::host::MAX_RETAINED_DATA_BYTES;
    count < abi::MAX_PENDING_OPERATIONS
        && held <= MAX_RETAINED_DATA_BYTES
        && bytes <= (MAX_RETAINED_DATA_BYTES - held) as u64
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn browser_event_fits(count: usize, held: usize, bytes: usize) -> bool {
    bytes <= abi::MAX_EVENT_BYTES
        && count < MAX_QUEUED_EVENTS
        && held <= MAX_QUEUED_EVENT_BYTES
        && bytes + abi::EVENT_QUEUE_OVERHEAD <= MAX_QUEUED_EVENT_BYTES - held
}
