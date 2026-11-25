//! Memory-only ownership for the browser's accepted input and I/O queues.
//! JavaScript retains payloads and schedules OPFS; common Rust Work owns roots.
use crate::capability::{Budget, Handle, Rights, Table};
use crate::runtime::{
    capabilities::{self, Capabilities},
    host::DataKind,
    work::{Work, browser_event_fits, browser_operation_fits},
};
use playbit_api::abi::syscall::Error;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

type Result<T> = std::result::Result<T, Error>;
struct Event {
    root: u64,
    kind: u32,
    bytes: usize,
    operation: Option<u64>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Pending,
    Complete,
    AbandonedPending,
    AbandonedComplete,
}
#[derive(Clone)]
pub(super) struct Operation {
    kind: DataKind,
    bytes: usize,
    phase: Rc<Cell<Phase>>,
}
struct Owner {
    work: Work,
    inputs: BTreeMap<u64, usize>,
    events: Table<Event>,
    capabilities: Capabilities<Operation>,
    operations: BTreeMap<u64, Operation>,
    epoch: u64,
    event_bytes: usize,
    data_bytes: usize,
    in_turn: bool,
}
impl Owner {
    fn new() -> Self {
        let mut work = Work::new();
        // Started is queued explicitly by the browser after guest installation.
        work.reap([]);
        Self {
            work,
            inputs: BTreeMap::new(),
            events: Table::new(Budget::backend()),
            capabilities: Capabilities::new(),
            operations: BTreeMap::new(),
            epoch: 1,
            event_bytes: 0,
            data_bytes: 0,
            in_turn: false,
        }
    }
    fn reap(&mut self) {
        self.work
            .reap(self.inputs.keys().copied().chain(self.events.values().map(|event| event.root)));
    }
    fn event_fits(&self, bytes: usize) -> Result<()> {
        if !browser_event_fits(self.events.len() + self.inputs.len(), self.event_bytes, bytes) {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }
    fn input(&mut self, bytes: usize) -> Result<u64> {
        self.reap();
        self.work.admit_root()?;
        self.event_fits(bytes)?;
        let root = self.work.new_root()?;
        assert!(self.inputs.insert(root, bytes).is_none());
        self.event_bytes += bytes + 20;
        Ok(root)
    }
    fn queue(&mut self, root: u64, kind: u32, bytes: usize) -> Result<u64> {
        let supported = matches!(kind, 1..=6 | 10..=15 | 17..=19)
            || (kind == playbit_api::abi::syscall::EventKind::IdentityChanged as u32 && bytes == 0);
        if !supported || self.inputs.get(&root) != Some(&bytes) {
            return Err(Error::InvalidArgument);
        }
        let key =
            self.events.allocate(1, Rights::NONE, Event { root, kind, bytes, operation: None })?;
        self.inputs.remove(&root);
        Ok(key.raw())
    }
    fn reject(&mut self, root: u64) -> Result<()> {
        let bytes = self.inputs.remove(&root).ok_or(Error::StaleHandle)?;
        self.event_bytes -= bytes + 20;
        self.work.failed = true;
        self.reap();
        Ok(())
    }
    fn begin(&mut self, kind: u32, bytes: u64) -> Result<u64> {
        if !self.in_turn {
            return Err(Error::Conflict);
        }
        let kind = DataKind::from_raw(kind)?;
        if !matches!(
            kind,
            DataKind::SchemaRead
                | DataKind::SchemaEstablish
                | DataKind::ResolveEntry
                | DataKind::Create
                | DataKind::Read
                | DataKind::Edit
                | DataKind::Query
                | DataKind::FileRead
                | DataKind::FileWrite
                | DataKind::FileOpen
                | DataKind::FileOpenWriter
                | DataKind::FileStatus
                | DataKind::FileClose
                | DataKind::FileListOpen
                | DataKind::FileListNext
                | DataKind::KvKeys
                | DataKind::KvRead
                | DataKind::KvPut
                | DataKind::KvDelete
                | DataKind::BloblogAppend
                | DataKind::BloblogRead
                | DataKind::ChannelCommand
        ) {
            return Err(Error::Unsupported);
        }
        self.reap();
        self.work.admit_step()?;
        if !browser_operation_fits(self.operations.len(), self.data_bytes, bytes) {
            return Err(Error::LimitExceeded);
        }
        let bytes = usize::try_from(bytes).map_err(|_| Error::LimitExceeded)?;
        let handle = self.capabilities.reserve(capabilities::OPERATION, Rights::NONE)?;
        let operation = Operation { kind, bytes, phase: Rc::new(Cell::new(Phase::Pending)) };
        self.capabilities.insert_operation(handle, operation.clone())?;
        let slot = handle.raw();
        assert!(self.operations.insert(slot, operation).is_none());
        self.data_bytes += bytes;
        self.work.register(slot);
        Ok(slot)
    }
    fn ready(&mut self, slot: u64) -> Result<u64> {
        let operation = self.capabilities.operation(Handle(slot))?;
        if operation.phase.get() != Phase::Complete {
            return Err(Error::WouldBlock);
        }
        let kind = if operation.kind == DataKind::Create { 7 } else { 16 };
        let root = *self.work.pending.get(&slot).expect("accepted operation retains its root");
        if self.events.values().any(|event| event.operation == Some(slot)) {
            return Err(Error::Conflict);
        }
        self.event_fits(8)?;
        let key = self.events.allocate(
            1,
            Rights::NONE,
            Event { root, kind, bytes: 8, operation: Some(slot) },
        )?;
        self.event_bytes += 28;
        Ok(key.raw())
    }
    fn poll(&mut self, key: u64) -> Result<()> {
        if !self.in_turn {
            return Err(Error::Conflict);
        }
        let event = self.events.take(&key).ok_or(Error::StaleHandle)?;
        self.event_bytes -= event.bytes + 20;
        self.work.active = Some(event.root);
        self.work.progress = true;
        self.reap();
        Ok(())
    }
    fn take(&mut self, slot: u64, kind: u32) -> Result<()> {
        if !self.in_turn {
            return Err(Error::Conflict);
        }
        self.check(slot, kind)?;
        self.work.active =
            Some(*self.work.pending.get(&slot).expect("accepted slot retains its root"));
        Ok(())
    }
    fn release(&mut self, slot: u64, status: i32, delivered: bool) -> Result<()> {
        if matches!(status, -6 | -11) {
            return Err(Error::InvalidArgument);
        }
        if delivered && !self.in_turn {
            return Err(Error::Conflict);
        }
        let operation = self.operations.get(&slot).ok_or(Error::StaleHandle)?;
        if matches!(operation.phase.get(), Phase::AbandonedPending) {
            return Err(Error::Busy);
        }
        if delivered && operation.phase.get() != Phase::Complete {
            return Err(Error::WouldBlock);
        }
        if !delivered && operation.phase.get() == Phase::Pending && status != 0 {
            return Err(Error::Busy);
        }
        let operation = self.operations.remove(&slot).expect("checked retained operation");
        if self.capabilities.table.contains(Handle(slot)) {
            self.capabilities.remove_operation(Handle(slot))?;
        }
        let root = self.work.pending.remove(&slot).expect("accepted slot retains its root");
        if delivered {
            self.work.active = Some(root);
        }
        self.work.progress = true;
        self.work.failed |= status < 0;
        self.data_bytes -= operation.bytes;
        self.events.retain(|_, event| {
            if event.operation == Some(slot) {
                self.event_bytes -= event.bytes + 20;
                false
            } else {
                true
            }
        });
        self.reap();
        Ok(())
    }
    fn check(&self, handle: u64, kind: u32) -> Result<()> {
        let operation = self.capabilities.operation(Handle(handle))?;
        if kind != 0 && operation.kind as u32 != kind {
            return Err(Error::StaleHandle);
        }
        Ok(())
    }
    fn complete(&mut self, handle: u64, status: i32) -> Result<()> {
        if status != 0 && (Error::try_from(status).is_err() || matches!(status, -6 | -11)) {
            return Err(Error::InvalidArgument);
        }
        let operation = self.operations.get(&handle).ok_or(Error::StaleHandle)?;
        operation.phase.set(match operation.phase.get() {
            Phase::Pending => Phase::Complete,
            Phase::AbandonedPending => Phase::AbandonedComplete,
            _ => return Err(Error::Conflict),
        });
        Ok(())
    }
    fn abandon(&mut self, handle: u64) -> Result<()> {
        let operation = self.capabilities.remove_operation(Handle(handle))?;
        operation.phase.set(match operation.phase.get() {
            Phase::Pending => Phase::AbandonedPending,
            Phase::Complete => Phase::AbandonedComplete,
            _ => unreachable!("live operation has not been abandoned"),
        });
        self.events.retain(|_, event| {
            if event.operation == Some(handle) {
                self.event_bytes -= event.bytes + 20;
                false
            } else {
                true
            }
        });
        Ok(())
    }
    fn cancel_notification(&mut self, kind: u32) -> Result<()> {
        self.events.retain(|_, event| {
            if event.kind == kind {
                self.event_bytes -= event.bytes + 20;
                false
            } else {
                true
            }
        });
        self.reap();
        Ok(())
    }
    fn deactivate(&mut self) -> Result<()> {
        if self.in_turn || self.work.active.is_some() {
            return Err(Error::Conflict);
        }
        self.work.deactivate();
        self.events.retain(|_, event| {
            if matches!(event.kind, 10 | 13 | 17) {
                self.event_bytes -= event.bytes + 20;
                false
            } else {
                true
            }
        });
        self.reap();
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        self.reap();
        if self.in_turn || !self.work.remaining.is_empty() || !self.operations.is_empty() {
            return Err(Error::Conflict);
        }
        assert!(self.events.is_empty() && self.inputs.is_empty());
        assert_eq!((self.event_bytes, self.data_bytes), (0, 0));
        let epoch = self.epoch.checked_add(1).ok_or(Error::LimitExceeded)?;
        let host = super::HOST.with(|host| host.borrow().clone());
        if let Some(host) = &host {
            host.files.borrow().admit_reset()?;
        }
        if let Some(host) = host {
            host.files
                .borrow_mut()
                .reset(&mut self.capabilities)
                .expect("file reset passed admission without an intervening callback");
        }
        self.epoch = epoch;
        self.capabilities = Capabilities::new();
        self.work = Work::new();
        self.work.reap([]);
        Ok(())
    }
}
thread_local! {
    static OWNER: RefCell<Owner> = RefCell::new(Owner::new());
}
pub(super) fn capabilities<T>(
    operation: impl FnOnce(&mut Capabilities<Operation>) -> Result<T>,
) -> Result<T> {
    OWNER.with(|owner| operation(&mut owner.borrow_mut().capabilities))
}
fn handle(operation: impl FnOnce(&mut Owner) -> Result<u64>) -> i64 {
    crate::platform::failure::guard();
    OWNER
        .with(|owner| operation(&mut owner.borrow_mut()))
        .map_or_else(|error| error as i64, |handle| handle as i64)
}
fn status(operation: impl FnOnce(&mut Owner) -> Result<()>) -> i32 {
    crate::platform::failure::guard();
    OWNER.with(|owner| operation(&mut owner.borrow_mut())).map_or_else(|error| error as i32, |()| 0)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_input(bytes: u32) -> i64 {
    crate::platform::failure::guard();
    OWNER
        .with(|owner| owner.borrow_mut().input(bytes as usize))
        .map_or_else(|error| error as i64, |root| root as i64)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_queue(root: u64, kind: u32, bytes: u32) -> i64 {
    handle(|owner| owner.queue(root, kind, bytes as usize))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_reject(root: u64) -> i32 {
    status(|owner| owner.reject(root))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_begin(kind: u32, bytes: u64) -> i64 {
    handle(|owner| {
        let result = owner.begin(kind, bytes);
        owner.work.failed |= owner.work.draining && result.is_err();
        result
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_ready(slot: u64) -> i64 {
    handle(|owner| owner.ready(slot))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_poll(key: u64) -> i32 {
    status(|owner| owner.poll(key))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_take(slot: u64, kind: u32) -> i32 {
    status(|owner| owner.take(slot, kind))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_release(slot: u64, result: i32, delivered: u32) -> i32 {
    status(|owner| {
        if delivered > 1 {
            return Err(Error::InvalidArgument);
        }
        owner.release(slot, result, delivered == 1)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_check(handle: u64, kind: u32) -> i32 {
    status(|owner| owner.check(handle, kind))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_complete(handle: u64, result: i32) -> i32 {
    status(|owner| owner.complete(handle, result))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_abandon(handle: u64) -> i32 {
    status(|owner| owner.abandon(handle))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_epoch() -> u64 {
    crate::platform::failure::guard();
    OWNER.with(|owner| owner.borrow().epoch)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_turn(begin: u32) -> i32 {
    status(|owner| match begin {
        1 if !owner.in_turn => {
            owner.in_turn = true;
            owner.work.progress = false;
            Ok(())
        }
        0 if owner.in_turn => {
            owner.in_turn = false;
            owner.work.finish_turn(!owner.events.is_empty());
            owner.reap();
            Ok(())
        }
        _ => Err(Error::Conflict),
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_cancel_volume_changes() -> i32 {
    status(|owner| owner.cancel_notification(17))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_cancel_wake() -> i32 {
    status(|owner| owner.cancel_notification(10))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_deactivate() -> i32 {
    status(Owner::deactivate)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_reset() -> i32 {
    status(Owner::reset)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_state() -> u32 {
    crate::platform::failure::guard();
    OWNER.with(|owner| {
        let owner = owner.borrow();
        u32::from(!owner.work.remaining.is_empty())
            | (u32::from(owner.work.failed) << 1)
            | (u32::from(owner.work.draining) << 2)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_guest_mutation(kind: u32) -> i32 {
    crate::platform::failure::guard();
    DataKind::from_raw(kind).map_or_else(|error| error as i32, |kind| i32::from(kind.is_mutation()))
}
