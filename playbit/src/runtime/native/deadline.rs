//! A deadline-enabled guest yields its C stack at an empty event poll. The host
//! retains its ordinary event loop and can deliver input, render or retire it.
use super::{Callbacks, Context, RuntimeError, abi, calls};
use crate::runtime::syscall::HostError;
use corosensei::{Coroutine, CoroutineResult, Yielder, stack::DefaultStack};
use std::{rc::Rc, sync::atomic::Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Resume {
    Run,
    Retire,
}
pub(super) enum Boundary {
    Waiting,
    Returned(i32),
}
pub(super) type PollYielder = Yielder<Resume, Boundary>;
pub(super) struct Turn(Coroutine<Resume, Boundary, ()>);

impl Turn {
    pub(super) fn new(
        context: Rc<Context>,
        callbacks: Rc<Callbacks>,
    ) -> Result<Self, RuntimeError> {
        // Only explicitly opted-in guests allocate this guard-paged stack.
        let stack = DefaultStack::new(1024 * 1024).map_err(|error| {
            RuntimeError::Instantiation(format!("Guest callback stack: {error}"))
        })?;
        Ok(Self(Coroutine::with_stack(stack, move |yielder, mut resume| {
            while resume == Resume::Run {
                context.poll_yielder.set(yielder);
                let status = callbacks.turn(&context);
                context.poll_yielder.set(std::ptr::null());
                // Retiring a suspended callback lets guest cleanup run on its
                // original stack. Never force-unwind a native C activation.
                if !context.active.load(Ordering::Acquire) {
                    return;
                }
                resume = yielder.suspend(Boundary::Returned(status));
            }
        })))
    }
    pub(super) fn advance(&mut self) -> i32 {
        match self.0.resume(Resume::Run) {
            CoroutineResult::Yield(Boundary::Waiting) => 0,
            CoroutineResult::Yield(Boundary::Returned(status)) => status,
            CoroutineResult::Return(()) => unreachable!("a retired native guest cannot be pumped"),
        }
    }
    pub(super) fn retire(&mut self) {
        assert!(matches!(self.0.resume(Resume::Retire), CoroutineResult::Return(())));
    }
}

/// # Safety
/// The native caller keeps both output allocations valid until this syscall
/// returns. The program, its stack and its instance context remain on one thread.
pub(super) unsafe fn poll(
    raw: *mut abi::Context,
    event: *mut abi::Event,
    data: *mut u8,
    capacity: u32,
    deadline: u64,
) -> i64 {
    if raw.is_null() || !(raw as usize).is_multiple_of(std::mem::align_of::<Context>()) {
        return HostError::InvalidArgument as i64;
    }
    // SAFETY: native syscall ingress requires a live, host-issued context.
    let context = unsafe { &*raw.cast::<Context>() };
    if context.owner != std::thread::current().id() || !context.active.load(Ordering::Acquire) {
        return HostError::Conflict as i64;
    }
    let yielder = context.poll_yielder.get();
    if yielder.is_null() {
        return HostError::Unsupported as i64;
    }
    loop {
        // Validate both disjoint output regions before allowing suspension.
        // This path also preserves the original size-probe/consume behavior.
        let result = unsafe { calls::pb_event_poll(raw, event, data, capacity) };
        if result != HostError::WouldBlock as i64 {
            return result;
        }
        let wait = context.state.lock().expect("native guest state").arm_poll_deadline(deadline);
        match wait {
            Err(error) => return error as i64,
            Ok(false) => continue,
            Ok(true) => {
                // No host state lock or Rust slice into guest memory crosses
                // this boundary. The scheduler presents the frame before wait.
                if unsafe { &*yielder }.suspend(Boundary::Waiting) == Resume::Retire {
                    return HostError::Closed as i64;
                }
                context.state.lock().expect("native guest state").poll_deadline = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        native::NativeInstance,
        syscall::{Event, EventKind, Host, Opcode},
    };
    use std::{
        cell::Cell,
        ffi::c_void,
        time::{Duration, Instant},
    };

    struct EmptyHost;
    impl Host for EmptyHost {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
    }
    struct Probe {
        deadline: u64,
        entered: Cell<usize>,
        outcome: Cell<Option<(i64, u32)>>,
        destroyed: Cell<bool>,
    }
    unsafe extern "C" fn init(
        _: *mut abi::Context,
        user: *mut c_void,
        state: *mut *mut c_void,
    ) -> i32 {
        unsafe {
            state.write(user);
        }
        0
    }
    unsafe extern "C" fn parked_init(
        ctx: *mut abi::Context,
        user: *mut c_void,
        state: *mut *mut c_void,
    ) -> i32 {
        unsafe {
            state.write(user);
        }
        let probe = unsafe { &*user.cast::<Probe>() };
        let (mut event, mut buffer) = (abi::Event::default(), [0; 256]);
        while unsafe { calls::pb_event_poll(ctx, &mut event, buffer.as_mut_ptr(), 256) } >= 0 {}
        let result = unsafe { poll(ctx, &mut event, buffer.as_mut_ptr(), 256, u64::MAX) };
        probe.outcome.set(Some((result, event.kind)));
        0
    }
    unsafe extern "C" fn turn(ctx: *mut abi::Context, raw: *mut c_void) -> i32 {
        let probe = unsafe { &*raw.cast::<Probe>() };
        probe.entered.set(probe.entered.get() + 1);
        let mut event = abi::Event::default();
        let mut buffer = [0_u8; 256];
        // Drain bootstrap/trace events to reach the actual waiting boundary.
        while unsafe { calls::pb_event_poll(ctx, &mut event, buffer.as_mut_ptr(), 256) } >= 0 {}
        let result = unsafe { poll(ctx, &mut event, buffer.as_mut_ptr(), 256, probe.deadline) };
        probe.outcome.set(Some((result, event.kind)));
        0
    }
    unsafe extern "C" fn destroy(raw: *mut c_void) {
        unsafe { &*raw.cast::<Probe>() }.destroyed.set(true);
    }

    #[test]
    fn native_deadline_poll_preserves_callback_on_input_timeout_and_retirement() {
        for mode in ["input", "timeout", "retire", "retire-init"] {
            let mut context =
                crate::runtime::syscall::pending_data::tests::state().0.context.clone();
            context.required_capabilities = ["event.poll.deadline".to_owned()].into();
            let mut probe = Probe {
                deadline: if mode == "timeout" {
                    crate::trace::clock_ns() + 20_000_000
                } else {
                    u64::MAX
                },
                entered: Cell::new(0),
                outcome: Cell::new(None),
                destroyed: Cell::new(false),
            };
            let app = abi::App {
                size: size_of::<abi::App>() as u32,
                written: 0,
                init: Some(if mode == "retire-init" { parked_init } else { init }),
                turn: Some(turn),
                destroy: Some(destroy),
                user_data: (&raw mut probe).cast(),
            };
            // SAFETY: all probe storage outlives the instance and every callback.
            let mut guest = unsafe { NativeInstance::new(context, Box::new(EmptyHost), app) }
                .expect("native guest");
            guest.pump().expect("park callback");
            if mode == "timeout" {
                if probe.outcome.get().is_none() {
                    let remaining = guest.wake_after(Instant::now()).expect("finite wait");
                    assert!(remaining <= Duration::from_millis(20));
                    std::thread::sleep(remaining);
                    guest.pump().expect("resume expired poll");
                }
                assert_eq!(probe.outcome.get().map(|r| r.0), Some(HostError::WouldBlock as i64));
            } else {
                assert_eq!(probe.outcome.get(), None);
                assert!(guest.wake_after(Instant::now()).is_none());
                assert!(!guest.has_ready_work(Instant::now()));
                if mode == "input" {
                    guest
                        .queue_event(
                            guest.generation(),
                            Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
                        )
                        .expect("admit event");
                    guest.pump().expect("resume waiting callback");
                    assert_eq!(probe.outcome.get(), Some((0, EventKind::DataChanged as u32)));
                }
            }
            if mode.starts_with("retire") {
                guest.deactivate();
                guest.pump().expect("spurious idle notification while draining");
                assert_eq!(probe.outcome.get(), None);
            }
            drop(guest);
            assert_eq!(probe.entered.get(), usize::from(mode != "retire-init"));
            assert!(probe.destroyed.get());
            if mode.starts_with("retire") {
                assert_eq!(probe.outcome.get().map(|r| r.0), Some(HostError::Closed as i64));
            }
        }
    }
}
