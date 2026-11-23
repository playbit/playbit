//! Independent consumer: no Cargo dev feature unification or linked VM.
use playbit::runtime::{
    RuntimeError, ViewNode,
    native::NativeInstance,
    syscall::{
        AvailableVolume, Event, EventKind, GuestContext, Host, HostError, Identity, Opcode,
        VolumeRights,
    },
};
use playbit_api::{abi::operations as abi, ui::CheckState};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::c_void,
    mem::{align_of, offset_of, size_of},
};

#[repr(C)]
struct Probe {
    off: *const u8,
    on: *const u8,
    off_len: u32,
    on_len: u32,
    view_bytes_limit: u32,
    initialized: u32,
    turns: u32,
    destroyed: u32,
    submitted: u32,
    fail_init: i32,
    fail_turn: i32,
    context: *mut abi::Context,
    generation: u64,
}
impl Probe {
    fn new(off: &[u8], on: &[u8]) -> Self {
        Self {
            off: off.as_ptr(),
            on: on.as_ptr(),
            off_len: off.len() as u32,
            on_len: on.len() as u32,
            view_bytes_limit: LocalHost.view_bytes_limit(),
            initialized: 0,
            turns: 0,
            destroyed: 0,
            submitted: 0,
            fail_init: 0,
            fail_turn: 0,
            context: std::ptr::null_mut(),
            generation: 0,
        }
    }
}
unsafe extern "C" {
    fn probe_app(probe: *mut Probe) -> abi::App;
    fn probe_outside(probe: *mut Probe) -> i32;
    fn probe_layout(index: u32) -> u64;
}
struct LocalHost;
impl Host for LocalHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
}
fn context(seed: u8) -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [seed; 16],
            is_service: false,
            device_id: [seed + 1; 16],
            instance_id: [seed + 2; 16],
            session_id: Some([seed + 3; 32]),
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [seed + 4; 16],
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        }],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: BTreeSet::new(),
        participants: vec![],
    }
}
fn scene(checked: bool) -> Vec<u8> {
    let mut view_state = playbit_api::context::ContextState::default();
    let mut view = view_state.view();
    view.column_start();
    view.checkbox(
        1,
        "Native C app",
        &mut if checked { CheckState::On } else { CheckState::Off },
        false,
    );
    view.column_end();
    view.finish()
}
fn checked(frame: &playbit::runtime::Frame) -> bool {
    let ViewNode::Theme { child, .. } = frame.view().unwrap() else { panic!("expected theme") };
    let ViewNode::Column(children) = &**child else { panic!("expected column") };
    assert_eq!(children.len(), 1);
    match &children[0] {
        ViewNode::Checkbox { state, .. } => *state == CheckState::On,
        other => panic!("unexpected scene {other:?}"),
    }
}
unsafe fn instance(probe: &mut Probe, seed: u8) -> NativeInstance {
    // SAFETY: callers drop the instance before the probe and scene allocations.
    unsafe { NativeInstance::new(context(seed), Box::new(LocalHost), probe_app(probe)).unwrap() }
}
#[test]
fn c_and_rust_agree_on_every_exchanged_record_layout() {
    let layout = [
        size_of::<abi::Event>(),
        align_of::<abi::Event>(),
        offset_of!(abi::Event, kind),
        offset_of!(abi::Event, target),
        size_of::<abi::Identity>(),
        align_of::<abi::Identity>(),
        offset_of!(abi::Identity, flags),
        offset_of!(abi::Identity, reserved),
        size_of::<playbit_api::abi::session::SessionInfo>(),
        align_of::<playbit_api::abi::session::SessionInfo>(),
        offset_of!(playbit_api::abi::session::SessionInfo, internal_id),
        offset_of!(playbit_api::abi::session::SessionInfo, key_secret),
        size_of::<abi::App>(),
        align_of::<abi::App>(),
        offset_of!(abi::App, user_data),
        size_of::<Probe>(),
        align_of::<Probe>(),
    ];
    for (index, expected) in layout.into_iter().enumerate() {
        // SAFETY: the fixed layout query has no borrowed storage.
        assert_eq!(unsafe { probe_layout(index as u32) }, expected as u64, "layout item {index}");
    }
}
#[test]
fn two_c_apps_use_plain_calls_and_keep_lifecycle_and_generation_separate() {
    let off = scene(false);
    let on = scene(true);
    let mut a = Probe::new(&off, &on);
    let mut b = Probe::new(&off, &on);
    // SAFETY: both configurations and scene buffers outlive their instances.
    let (mut first, mut second) = unsafe { (instance(&mut a, 1), instance(&mut b, 10)) };
    assert_eq!((a.initialized, b.initialized), (0, 0));
    assert!(!checked(&first.pump().unwrap()));
    assert!(!checked(&second.pump().unwrap()));
    assert_ne!(a.generation, b.generation);
    assert_eq!(
        first.queue_event(b.generation, Event { kind: EventKind::Click, target: 1, data: vec![] }),
        Err(HostError::StaleHandle)
    );
    first
        .queue_event(a.generation, Event { kind: EventKind::Click, target: 1, data: vec![] })
        .unwrap();
    first
        .queue_event(
            a.generation,
            Event { kind: EventKind::Submit, target: 2, data: b"test".to_vec() },
        )
        .unwrap();
    assert!(checked(&first.pump().unwrap()));
    assert!(!checked(&second.pump().unwrap()));
    assert_eq!(a.submitted, 1, "the short probe did not consume the event");
    assert_eq!((a.initialized, b.initialized), (1, 1));
    // SAFETY: these are live contexts, but intentionally outside a callback.
    assert_eq!(unsafe { probe_outside(&mut a) }, HostError::Conflict as i32);
    drop(first);
    drop(second);
    assert_eq!((a.destroyed, b.destroyed), (1, 1));
}
#[test]
fn failed_init_or_turn_halts_without_retry_and_destroys_once() {
    let off = scene(false);
    let on = scene(true);
    for fail_init in [false, true] {
        let mut probe = Probe::new(&off, &on);
        if fail_init {
            probe.fail_init = -5;
        } else {
            probe.fail_turn = -5;
        }
        // SAFETY: the probe and scenes outlive this instance, including failure.
        let mut app = unsafe { instance(&mut probe, 1) };
        assert!(matches!(app.pump(), Err(RuntimeError::NativeApp(-5))));
        assert!(matches!(app.pump(), Err(RuntimeError::GuestHalted)));
        assert!(!app.has_ready_work(std::time::Instant::now()));
        assert_eq!(probe.initialized, 1);
        assert_eq!(probe.turns, u32::from(!fail_init));
        drop(app);
        assert_eq!(probe.destroyed, 1);
    }
}

const PROBE_LOG_BYTES: usize = 64 * 1024 + 1;

unsafe extern "C" fn rust_init(
    raw: *mut abi::Context,
    data: *mut c_void,
    state: *mut *mut c_void,
) -> i32 {
    // SAFETY: the native executor supplies the callback borrow and output slot.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    // A wrong-thread call returns before acquiring app state, even while the
    // legitimate callback is active. Join keeps the context allocation live.
    let address = raw as usize;
    let rejected = std::thread::spawn(move || unsafe {
        abi::pb_log(address as *mut abi::Context, b"wrong thread".as_ptr(), 12)
    })
    .join()
    .unwrap();
    assert_eq!(rejected, HostError::Conflict as i64);
    context.log(&"x".repeat(PROBE_LOG_BYTES));
    context.log("Rust SDK initialized");
    assert_ne!(context.identity().unwrap().generation, 0);
    assert_eq!(context.wake_after(0), Err(HostError::InvalidArgument));
    assert_eq!(context.defer_update(123), Err(HostError::Conflict));
    unsafe {
        state.write(data);
    }
    0
}
unsafe extern "C" fn rust_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    // SAFETY: each callback borrows this context and the configured probe.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    let probe = unsafe { &mut *state.cast::<Probe>() };
    for event in context.raw_events() {
        assert_eq!(event.kind, EventKind::Started);
    }
    probe.turns += 1;
    // SAFETY: scenes remain owned by this test until after app destruction.
    context
        .submit_view(unsafe { std::slice::from_raw_parts(probe.off, probe.off_len as usize) })
        .unwrap();
    0
}
unsafe extern "C" fn rust_destroy(state: *mut c_void) {
    // SAFETY: Rust app state borrows the configured test probe; it does not own it.
    unsafe {
        (*state.cast::<Probe>()).destroyed += 1;
    }
}
#[test]
fn rust_sdk_calls_the_same_c_symbols_without_request_encoding() {
    let off = scene(false);
    let on = scene(true);
    let mut probe = Probe::new(&off, &on);
    let callbacks = abi::App {
        init: Some(rust_init),
        turn: Some(rust_turn),
        destroy: Some(rust_destroy),
        user_data: (&mut probe as *mut Probe).cast(),
        size: size_of::<abi::App>() as u32,
        written: 0,
    };
    // SAFETY: callbacks borrow the probe/scenes, which outlive app destruction.
    let mut app =
        unsafe { NativeInstance::new(context(30), Box::new(LocalHost), callbacks).unwrap() };
    let frame = app.pump().unwrap();
    assert!(!checked(&frame));
    assert_eq!(frame.logs(), ["x".repeat(PROBE_LOG_BYTES), "Rust SDK initialized".to_owned()]);
    assert_eq!(probe.turns, 1);
    drop(app);
    assert_eq!(probe.destroyed, 1);
}

#[test]
fn dropping_before_first_pump_runs_no_app_callbacks() {
    let off = scene(false);
    let on = scene(true);
    let mut probe = Probe::new(&off, &on);
    // SAFETY: the probe and scenes remain alive until after destruction.
    drop(unsafe { instance(&mut probe, 1) });
    assert_eq!((probe.initialized, probe.turns, probe.destroyed), (0, 0, 0));
}

#[test]
fn missing_lifecycle_callback_is_rejected_before_init() {
    let off = scene(false);
    let on = scene(true);
    let mut probe = Probe::new(&off, &on);
    // SAFETY: probe_app only returns the C function table and user_data.
    let mut callbacks = unsafe { probe_app(&mut probe) };
    callbacks.destroy = None;
    // SAFETY: all supplied pointers remain valid; rejection must precede init.
    assert!(matches!(
        unsafe { NativeInstance::new(context(1), Box::new(LocalHost), callbacks) },
        Err(RuntimeError::Instantiation(_))
    ));
    assert_eq!((probe.initialized, probe.turns, probe.destroyed), (0, 0, 0));
}

#[test]
#[ignore = "Operator-only optimized diagnostic; not physical presentation or persistent editing."]
fn native_input_to_validated_scene_timing() {
    use std::time::Instant;
    let off = scene(false);
    let on = scene(true);
    let mut probe = Probe::new(&off, &on);
    let startup = Instant::now();
    // SAFETY: the probe and scene allocations outlive the app.
    let mut app = unsafe { instance(&mut probe, 1) };
    assert!(!checked(&app.pump().unwrap()));
    let startup = startup.elapsed();
    let mut timings = Vec::with_capacity(1000);
    for index in 0..1000 {
        let start = Instant::now();
        app.queue_event(
            app.generation(),
            Event { kind: EventKind::Click, target: 1, data: vec![] },
        )
        .unwrap();
        let frame = app.pump().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(checked(&frame), index % 2 == 0);
        timings.push(elapsed);
    }
    timings.sort_unstable();
    eprintln!(
        "native C checkbox: construction+first scene={startup:?}; 1000 queue+turn+validated scene samples: p50={:?} p95={:?} p99={:?} max={:?}",
        timings[499], timings[949], timings[989], timings[999]
    );
    drop(app);
    assert_eq!(probe.destroyed, 1);
}

mod data_host;

mod environment;

mod files;

mod pending;

mod authenticated;
