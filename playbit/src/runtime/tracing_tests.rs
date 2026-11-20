//! Native, Wasm and interpreter ingestion use one structural event contract.
#![allow(unsafe_code, reason = "Fixtures own callback storage and syscall outputs.")]
use super::{ExecutionBackend, Runtime, native::NativeInstance, syscall::*};
use crate::trace;
use playbit_api::abi::{operations as abi, trace as contract};
use std::{collections::BTreeMap, ffi::c_void, io::Read};
fn context() -> GuestContext {
    GuestContext {
        app_name: "Private fixture".into(),
        app_build: None,
        identity: Identity { account_id: [1; 16], device_id: [2; 16], instance_id: [3; 16], session_id: None, is_service: false },
        volumes: vec![AvailableVolume { backing: playbit_api::abi::data::VOLUME_PLAYBIT, source_epoch: [0; 16], id: [4; 16], name: "Root".into(), tags: vec![], rights: VolumeRights::READ, is_root: true, role: 0 }],
        startup_arguments: BTreeMap::new(),
        trace_catalog: Some(std::sync::Arc::from(br#"{"version":1,"names":[{"name":"app.turn","kind":"span","description":"Fixture stage"},{"name":"app.inner","kind":"span","description":"Fixture stage"},{"name":"app.rows","kind":"mark","description":"Fixture value"}]}"#.as_slice())),
        required_capabilities: Default::default(), participants: vec![],
    }
}

struct NoData;
impl Host for NoData {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
}
fn events(step: u64) -> Vec<u8> {
    let metadata = br#"{"position":{"x":12.5},"ok":true}"#;
    let mut bytes = vec![0; contract::event_bytes(metadata.len()).expect("size")];
    let header = contract::TraceEvent {
        timestamp: if step == 0 { 20 } else { 10 },
        reference_eid: if step == 0 { 10 } else { 0 },
        kind: if step == 0 { contract::END } else { contract::START },
        metadata_len: metadata.len() as u32,
        ..Default::default()
    };
    contract::encode_header(header, &mut bytes).expect("header");
    bytes[contract::HEADER_BYTES..][..metadata.len()].copy_from_slice(metadata);
    bytes
}
struct Callback {
    step: u64,
    generation: u64,
}
unsafe extern "C" fn init(_: *mut abi::Context, user: *mut c_void, state: *mut *mut c_void) -> i32 {
    // SAFETY: borrowed fixture state outlives both serialized callbacks.
    unsafe { state.write(user) };
    0
}
unsafe extern "C" fn destroy(_: *mut c_void) {}
unsafe extern "C" fn turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    // SAFETY: fixture init supplied this unique callback state.
    let state = unsafe { &mut *state.cast::<Callback>() };
    loop {
        let mut event = abi::Event::default();
        let mut data = [0; 16];
        // SAFETY: live callback and disjoint initialized outputs.
        let result = unsafe { abi::pb_event_poll(raw, &mut event, data.as_mut_ptr(), 16) };
        if result == playbit_api::abi::syscall::Error::WouldBlock as i64 {
            break;
        }
        assert!(result >= 0);
        if event.kind == EventKind::TraceConfig as u32 {
            state.generation = u64::from_le_bytes(data[..8].try_into().expect("generation"));
        }
    }
    // SAFETY: complete local inputs live until each call returns.
    unsafe {
        let now = abi::pb_clock_read(raw);
        assert!(now > 0);
        let bytes = events(state.step);
        assert_eq!(
            abi::pb_trace_record(raw, bytes.as_ptr(), bytes.len() as u32, state.generation, 1),
            0
        );
        assert_eq!(
            abi::pb_trace_record(raw, bytes.as_ptr(), bytes.len() as u32, state.generation, 0),
            playbit_api::abi::syscall::Error::InvalidArgument as i64
        );
    }
    state.step += 1;
    0
}
fn wasm() -> Vec<u8> {
    let escaped =
        |bytes: Vec<u8>| bytes.iter().map(|byte| format!("\\{byte:02x}")).collect::<String>();
    wat::parse_str(format!(r#"(module
      (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (global $step (mut i32) (i32.const 0))
      (global $generation (mut i64) (i64.const 0))
      (data (i32.const 512) "{}") (data (i32.const 1024) "{}")
      (func (export "pb_main") (local $pointer i32) (local $result i64)
        (i32.store (i32.const 64) (i32.const 24))
        (block $done (loop $poll
          (local.set $result (call $call (i32.const 4) (i64.const 64) (i64.const 128) (i64.const 16) (i64.const 0) (i64.const 0) (i64.const 0)))
          (br_if $done (i64.eq (local.get $result) (i64.const -6)))
          (if (i64.lt_s (local.get $result) (i64.const 0)) (then unreachable))
          (if (i32.eq (i32.load (i32.const 80)) (i32.const 20)) (then (global.set $generation (i64.load (i32.const 128)))))
          (br $poll)))
        (local.set $pointer (select (i32.const 512) (i32.const 1024) (i32.eqz (global.get $step))))
        (if (i64.eq (call $call (i32.const 16) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)) (i64.const 0)) (then unreachable))
        (if (i64.ne (call $call (i32.const 15) (i64.extend_i32_u (local.get $pointer)) (i64.const {}) (global.get $generation) (i64.const 1) (i64.const 0) (i64.const 0)) (i64.const 0)) (then unreachable))
        (global.set $step (i32.add (global.get $step) (i32.const 1)))))"#,
        escaped(events(0)),escaped(events(1)),events(0).len())).expect("fixture wasm")
}
#[test]
fn adapters_keep_out_of_order_endpoints_and_host_attribution() {
    // Isolate native/JIT/Pulley sources, submit an end first, then inspect raw tail.
    const TEST: &str =
        "runtime::tracing_tests::adapters_keep_out_of_order_endpoints_and_host_attribution";
    let Ok(mode) = std::env::var("PLAYBIT_EVENT_ADAPTER_TEST") else {
        for mode in ["native", "wasm", "pulley"] {
            assert!(
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--exact", TEST, "--nocapture"])
                    .env("PLAYBIT_EVENT_ADAPTER_TEST", mode)
                    .status()
                    .expect("adapter child")
                    .success(),
                "{mode}"
            );
        }
        return;
    };
    trace::initialize(8, &[]).expect("source");
    let directory = tempfile::tempdir().expect("profile");
    let _recorder =
        trace::Recorder::start(directory.path(), &directory.path().join("trace"), false)
            .expect("recorder");
    let mut tail = trace::tail::connect(directory.path(), None, None).expect("tail");
    tail.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("deadline");
    if mode == "native" {
        let mut callback = Callback { step: 0, generation: 0 };
        let app = abi::App {
            size: core::mem::size_of::<abi::App>() as u32,
            written: 0,
            init: Some(init),
            turn: Some(turn),
            destroy: Some(destroy),
            user_data: (&mut callback as *mut Callback).cast(),
        };
        // SAFETY: descriptor callbacks and borrowed state outlive this execution.
        let mut instance = unsafe { NativeInstance::new(context(), Box::new(NoData), app) }
            .expect("native instance");
        instance.pump().expect("end callback");
        instance.pump().expect("start callback");
    } else {
        let runtime = Runtime::with_backend(if mode == "pulley" {
            ExecutionBackend::Pulley
        } else {
            ExecutionBackend::Native
        })
        .expect("runtime");
        let mut instance =
            runtime.create_guest(&wasm(), context(), Box::new(NoData)).expect("wasm instance");
        instance.pump().expect("end callback");
        instance.pump().expect("start callback");
    }
    let mut seen = Vec::new();
    for _ in 0..100 {
        let mut length = [0; 4];
        tail.read_exact(&mut length).expect("frame length");
        let length = u32::from_le_bytes(length) as usize;
        assert!(length <= trace::tail::MAX_FRAME_BYTES);
        let mut bytes = vec![0; length];
        tail.read_exact(&mut bytes).expect("frame");
        if !matches!(bytes[0], 1 | 7) {
            continue;
        }
        let (envelope, payload) = trace::format::Envelope::decode(&bytes[1..]).expect("block");
        if envelope.kind != trace::format::BlockKind::Events {
            continue;
        }
        let event = trace::format::Events::decode(payload).expect("event");
        if event.scope == 0 {
            continue;
        }
        assert_eq!(event.tid, 1);
        assert_ne!(envelope.source, [0; 2]);
        assert!(
            serde_json::from_slice::<serde_json::Value>(event.event.metadata)
                .expect("metadata")
                .is_object()
        );
        seen.push(event.event.header);
        if seen.len() == 2 {
            break;
        }
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].kind, contract::END);
    assert_eq!(seen[0].reference_eid, 10);
    assert_eq!(seen[0].reference_tid, 1);
    assert_eq!(seen[1].kind, contract::START);
    assert_eq!(seen[1].timestamp, 10);
}

// A small opt-in cost sample, not device/UI qualification. Run in an isolated
// child so the process-wide recorder/source does not depend on test ordering.
#[test]
#[ignore = "event cost sample; run alone with --nocapture"]
fn event_emission_cost() {
    const TEST: &str = "runtime::tracing_tests::event_emission_cost";
    let Ok(mode) = std::env::var("PLAYBIT_EVENT_COST") else {
        for mode in ["off", "on"] {
            let mut command =
                std::process::Command::new(std::env::current_exe().expect("test binary"));
            command
                .args(["--exact", TEST, "--ignored", "--nocapture"])
                .env("PLAYBIT_EVENT_COST", mode);
            assert!(command.status().expect("cost child").success());
        }
        return;
    };
    trace::initialize(8, &[]).expect("source");
    let profile = tempfile::tempdir().expect("profile");
    let _recorder = trace::Recorder::for_profile(profile.path(), &profile.path().join("trace"))
        .expect("recorder");
    struct State {
        context: playbit_api::context::ContextState,
        enabled: bool,
    }
    unsafe extern "C" fn cost_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
        // SAFETY: test owns the borrowed state and this serialized callback.
        let state = unsafe { &mut *state.cast::<State>() };
        let (mut context, _) =
            unsafe { playbit_api::context::Context::from_raw(raw, &mut state.context) }
                .expect("SDK context");
        assert_eq!(context.trace_enabled(), state.enabled);
        let mut previous = 0;
        let mut repeats = 0;
        let mut last_t = 0;
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            // SAFETY: raw belongs to the live callback.
            let now = unsafe { abi::pb_clock_read(raw) };
            repeats += u64::from(now == previous);
            previous = now;
            std::hint::black_box(contract::make_timestamp(&mut last_t, now).expect("clock"));
        }
        eprintln!(
            "clock_and_generator mean_ns={} equal_samples={repeats} samples=10000",
            start.elapsed().as_nanos() / 10_000
        );
        for size in [0, 64, 4096] {
            let encoding = std::time::Instant::now();
            let mut metadata = Vec::new();
            for _ in 0..128 {
                metadata = if size == 0 {
                    Vec::new()
                } else {
                    serde_json::to_vec(&serde_json::json!({"data":"x".repeat(size-11)}))
                        .expect("metadata")
                };
                std::hint::black_box(&metadata);
            }
            eprintln!(
                "metadata encoding bytes={size} mean_ns={} samples=128",
                encoding.elapsed().as_nanos() / 128
            );
            assert_eq!(metadata.len(), size);
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                std::hint::black_box(context.trace_instant(2, 42, None, &metadata)).ok();
            }
            let _ = context.trace_flush();
            eprintln!(
                "SDK emission enabled={} metadata_bytes={size} mean_ns={} samples=1000 ring_bytes={}",
                state.enabled,
                start.elapsed().as_nanos() / 1000,
                contract::BUFFER_BYTES
            );
        }
        0
    }
    let mut state = State { context: Default::default(), enabled: mode == "on" };
    let app = abi::App {
        size: core::mem::size_of::<abi::App>() as u32,
        written: 0,
        init: Some(init),
        turn: Some(cost_turn),
        destroy: Some(destroy),
        user_data: (&mut state as *mut State).cast(),
    };
    // SAFETY: callback and its borrowed state outlive this execution.
    let mut instance =
        unsafe { NativeInstance::new(context(), Box::new(NoData), app) }.expect("instance");
    // for_profile is asynchronous. A control acknowledgment observes initialization
    // without waking or adding product timers to the guest.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(mut control) = trace::control::connect(
            profile.path(),
            if mode == "on" {
                trace::control::Command::Start
            } else {
                trace::control::Command::Observe
            },
            None,
            false,
        ) {
            control.set_nonblocking(false).expect("control");
            control.set_read_timeout(Some(std::time::Duration::from_secs(2))).expect("deadline");
            let mut status = [0; trace::control::STATUS_BYTES];
            control.read_exact(&mut status).expect("status");
            trace::control::Status::decode(&status).expect("initialized");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "recorder initialization");
        std::thread::yield_now();
    }
    instance.pump().expect("cost callback");
    // Measure the actual Wasm clock import separately from SDK encoding/copying.
    let clock_module=wat::parse_str(r#"(module
        (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (memory (export "memory") 1)
        (func (export "pb_main") (local $remaining i32)
            (local.set $remaining (i32.const 10000))
            (loop $sample
                (drop (call $call (i32.const 16) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
                (local.set $remaining (i32.sub (local.get $remaining) (i32.const 1)))
                (br_if $sample (local.get $remaining)))))"#).expect("clock loop");
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime = Runtime::with_backend(backend).expect("clock runtime");
        let mut instance =
            runtime.create_guest(&clock_module, context(), Box::new(NoData)).expect("clock guest");
        let start = std::time::Instant::now();
        instance.pump().expect("clock loop");
        eprintln!(
            "Wasm clock loop incl callback backend={backend:?} mean_ns={} samples=10000",
            start.elapsed().as_nanos() / 10000
        );
    }
}
