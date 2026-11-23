//! Runtime configuration, admission and execution regression tests.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::runtime::{Frame, syscall};
use playbit_api::abi::syscall::{Error as HostError, Opcode};
use syscall::{AvailableVolume, GuestContext, Host, Identity, VolumeRights};

const LOG_IMPORT: &str = r#"(import "playbit" "pb_syscall" (func $dispatch_log (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
"#;
const LOG_WRAPPER: &str = r#"(func $log (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_log (i32.const 2)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))"#;

struct NoData;
impl Host for NoData {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [1; 16],
            is_service: false,
            device_id: [2; 16],
            instance_id: [5; 16],
            session_id: None,
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [6; 16],
            name: "Root".into(),
            tags: Vec::new(),
            rights: VolumeRights::READ,
            is_root: true,
            role: 0,
        }],
        startup_arguments: Default::default(),
        trace_catalog: None,
        required_capabilities: Default::default(),
        participants: Vec::new(),
    }
}

fn test_backends() -> &'static [ExecutionBackend] {
    if cfg!(target_os = "ios") {
        &[ExecutionBackend::Pulley]
    } else {
        &[ExecutionBackend::Native, ExecutionBackend::Pulley]
    }
}

fn module(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module {LOG_IMPORT} {body} {LOG_WRAPPER})")).unwrap()
}

fn run(runtime: &Runtime, wasm: &[u8]) -> Result<Frame, RuntimeError> {
    runtime.create_guest(wasm, context(), Box::new(NoData))?.pump()
}

#[test]
fn guest_can_log() {
    let wasm = module(
        r#"
        (memory (export "memory") 1)
        (data (i32.const 8) "hello from wasm")
        (func (export "pb_main")
            i32.const 8 i32.const 15
            call $log drop)"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert_eq!(runtime.engine.is_pulley(), backend == ExecutionBackend::Pulley);
        assert_eq!(run(&runtime, &wasm).unwrap().logs(), &["hello from wasm"]);
    }
}

#[test]
fn invalid_utf8_in_a_large_log_is_rejected() {
    // A readable large message still requires valid UTF-8 throughout its span.
    let wasm = module(
        r#"
        (memory (export "memory") 2)
        (data (i32.const 65536) "\ff")
        (func (export "pb_main")
            (if (i32.ne (call $log (i32.const 8) (i32.const 65537)) (i32.const -3))
                (then unreachable)))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(run(&runtime, &wasm).unwrap().logs().is_empty());
    }
}

#[test]
fn large_pixel_scene_reaches_both_wasm_backends() {
    // Submit a native-resolution 1024x768 frame through the real scalar import,
    // exercising SDK encoding, memory admission, feature checks and ownership.
    use playbit_api::ui::{CanvasFrames, CanvasItem, CanvasPixels, CanvasRect, Color};
    struct PixelsHost;
    impl Host for PixelsHost {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
        fn supported_features(&self) -> &[&str] {
            &["ui.canvas.pixels.v1"]
        }
    }
    let rgba = vec![17; 1024 * 768 * 4];
    let mut state = playbit_api::context::ContextState::default();
    let mut encoder = state.view();
    encoder.column_start();
    encoder.canvas(
        1,
        1024,
        768,
        CanvasFrames::Static,
        &[CanvasItem::Pixels(CanvasPixels {
            bounds: CanvasRect { x: 0, y: 0, width: 1024, height: 768, color: Color(0) },
            source_width: 1024,
            source_height: 768,
            rgba: &rgba,
        })],
    );
    encoder.column_end();
    let scene = encoder.finish();
    let wasm = module(&format!(
        r#"
        (memory (export "memory") {})
        (func (export "pb_main")
            (if (i64.ne (call $dispatch_log (i32.const 3)
                (i64.const 8) (i64.const {}) (i64.const 0)
                (i64.const 0) (i64.const 0) (i64.const 0)) (i64.const 0))
                (then unreachable)))"#,
        (8 + scene.len()).div_ceil(65536),
        scene.len()
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let mut guest = runtime.create_guest(&wasm, context(), Box::new(PixelsHost)).unwrap();
        let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
        memory.write(&mut guest.store, 8, &scene).unwrap();
        let frame = guest.pump().unwrap();
        assert_eq!(frame.view_bytes(), Some(scene.as_slice()));
        assert!(frame.view().is_some());
    }
}

#[test]
fn wasm_atomics_execute_through_runtime_on_each_backend() {
    let wasm = module(
        r#"
        (memory (export "memory") 1)
        (func (export "pb_main")
            i32.const 0 i32.const 7 i32.atomic.store
            i32.const 0 i32.const 5 i32.atomic.rmw.add drop
            i32.const 0 i32.const 12 i32.const 21 i32.atomic.rmw.cmpxchg
            i32.const 12 i32.ne if unreachable end
            atomic.fence
            i32.const 0 i32.atomic.load i32.const 21 i32.ne if unreachable end
            i32.const 0 i32.const 1 memory.atomic.notify
            if unreachable end)"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        runtime.validate_guest(&wasm).unwrap();
        assert!(run(&runtime, &wasm).unwrap().logs().is_empty());
    }
}

#[test]
fn wasm_atomic_traps_and_shared_rejection_use_runtime_path() {
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        for code in [
            "i32.const 1 i32.atomic.load drop",
            "i32.const -4 i32.atomic.load offset=4 drop",
            "i32.const 65536 i32.const 1 i32.atomic.rmw.add drop",
            "i32.const 0 i32.const 0 i64.const 0 memory.atomic.wait32 drop",
        ] {
            let wasm = module(&format!(
                "(memory (export \"memory\") 1) (func (export \"pb_main\") {code})"
            ));
            assert!(matches!(run(&runtime, &wasm), Err(RuntimeError::GuestTrap(_))));
        }
        for memory in [
            "(memory (export \"memory\") 1 2 shared)",
            "(memory (export \"memory\") 1) (memory 1 2 shared)",
            "(import \"host\" \"memory\" (memory 1 2 shared))",
        ] {
            let wasm = module(&format!("{memory} (func (export \"pb_main\"))"));
            let error = runtime.validate_guest(&wasm).unwrap_err();
            assert!(
                matches!(&error, RuntimeError::InvalidModule(message) if message.contains("Shared"))
            );
        }
    }
}

#[test]
fn wasm_bulk_memory_executes_on_each_backend() {
    let wasm = module(
        r#"
        (type $number (func (result i32)))
        (func $number (type $number) i32.const 17)
        (table 2 funcref)
        (elem $functions func $number)
        (memory (export "memory") 1)
        (data $bytes "ABCD")
        (func (export "pb_main")
            i32.const 0 i32.const 0 i32.const 4 memory.init $bytes
            data.drop $bytes
            i32.const 8 i32.const 0 i32.const 4 memory.copy
            i32.const 12 i32.const 69 i32.const 4 memory.fill
            i32.const 8 i32.load i32.const 1145258561 i32.ne
            if unreachable end
            i32.const 12 i32.load i32.const 1162167621 i32.ne
            if unreachable end
            i32.const 0 i32.const 0 i32.const 1 table.init $functions
            elem.drop $functions
            i32.const 1 i32.const 0 i32.const 1 table.copy
            i32.const 1 call_indirect (type $number)
            i32.const 17 i32.ne if unreachable end
            i32.const 8 i32.const 8
            call $log drop)"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert_eq!(run(&runtime, &wasm).unwrap().logs(), &["ABCDEEEE"]);
    }
}

#[test]
fn wasm_simd_executes_on_each_backend() {
    let wasm = module(
        r#"
        (memory (export "memory") 1)
        (data (i32.const 8) "SIMD")
        (func (export "pb_main")
            i32.const 33
            v128.const i32x4 1 2 3 4
            v128.const i32x4 10 20 30 40
            i32x4.add v128.store align=1
            i32.const 33 v128.load align=1
            v128.const i32x4 11 22 33 44
            i32x4.eq i32x4.all_true i32.eqz if unreachable end
            i32.const 33 v128.load align=1 i32x4.extract_lane 3
            i32.const 44 i32.ne if unreachable end
            i32.const 8 i32.const 4
            call $log drop)"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert_eq!(run(&runtime, &wasm).unwrap().logs(), &["SIMD"]);
    }
}

#[test]
fn wasm_extensions_preserve_memory_bounds_on_each_backend() {
    let bodies = [
        "i32.const 65535 i32.const 0 i32.const 2 memory.copy",
        "i32.const 0 i32.const 65535 i32.const 2 memory.copy",
        "i32.const 65535 i32.const 0 i32.const 2 memory.fill",
        "i32.const 65535 i32.const 0 i32.const 2 memory.init $bytes",
        "data.drop $bytes i32.const 0 i32.const 0 i32.const 1 memory.init $bytes",
        "i32.const 65521 v128.load drop",
        "i32.const 65521 v128.const i32x4 1 2 3 4 v128.store",
    ];
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        for body in bodies {
            let wasm = module(&format!(
                r#"(memory (export "memory") 1)
                (data $bytes "ABCD")
                (func (export "pb_main") {body})"#,
            ));
            assert!(
                matches!(run(&runtime, &wasm), Err(RuntimeError::GuestTrap(_))),
                "{backend:?}: {body}",
            );
        }
    }
}

#[test]
fn wasm_extension_loops_remain_fuel_bounded_on_each_backend() {
    // Explicit limits contain extension loops on both production backends.
    let bodies = [
        "i32.const 0 i32.const 65 i32.const 64 memory.fill",
        "i32.const 0 i32.const 0 v128.load v128.const i32x4 1 2 3 4 i32x4.add v128.store",
    ];
    for &backend in test_backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, NonZeroU64::new(10_000)).unwrap();
        for body in bodies {
            let wasm = module(&format!(
                r#"(memory (export "memory") 1)
                (func (export "pb_main") (loop $again {body} br $again))"#,
            ));
            assert!(
                matches!(run(&runtime, &wasm), Err(RuntimeError::FuelExhausted)),
                "{backend:?}: {body}",
            );
        }
    }
}

#[test]
fn invalid_log_pointer_returns_explicit_error_without_effects() {
    let wasm = module(&format!(
        r#"
        (memory (export "memory") 1)
        (func (export "pb_main")
            i32.const 65535 i32.const 2
            call $log i32.const {} i32.ne if unreachable end)"#,
        HostError::InvalidArgument as i32
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(run(&runtime, &wasm).unwrap().logs().is_empty());
    }
}

#[test]
fn missing_memory_is_an_explicit_error() {
    let wasm = module(r#"(func (export "pb_main"))"#);
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(matches!(runtime.validate_guest(&wasm), Err(RuntimeError::MissingMemory)));
        assert!(matches!(run(&runtime, &wasm), Err(RuntimeError::MissingMemory)));
    }
}

#[test]
fn missing_main_is_an_explicit_error() {
    let wasm = module(r#"(memory (export "memory") 1)"#);
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(matches!(runtime.validate_guest(&wasm), Err(RuntimeError::MissingMain)));
        assert!(matches!(run(&runtime, &wasm), Err(RuntimeError::MissingMain)));
    }
}

#[test]
fn app_validation_does_not_execute_guest_code() {
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") unreachable)"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        runtime.validate_guest(&wasm).unwrap();
        let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        assert!(matches!(guest.pump(), Err(RuntimeError::GuestTrap(_))));
    }
}

#[test]
fn guest_traps_resolve_source_locations_and_function_names() {
    // Generate DWARF for a known source line, then exercise the production
    // compile/serialize/load path on both execution backends.
    let source = "(module\n  (memory (export \"memory\") 1)\n  (func $_ZN5guest5crashE\n    unreachable)\n  (func (export \"pb_main\")\n    call $_ZN5guest5crashE))";
    let wasm = wat::Parser::new()
        .generate_dwarf(wat::GenerateDwarf::Full)
        .parse_str(Some(std::path::Path::new("guest-crash.wat")), source)
        .unwrap();
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let Err(RuntimeError::GuestTrap(report)) = run(&runtime, &wasm) else {
            panic!("guest must trap")
        };
        assert!(report.contains("guest::crash"), "{backend:?}: {report}");
        assert!(report.contains("guest-crash.wat:4"), "{backend:?}: {report}");
        assert!(report.contains("wasm `unreachable` instruction executed"), "{report}");
    }
}

#[test]
fn guest_traps_without_debug_information_retain_named_frames() {
    // Stripped guests still produce useful Wasm frames and the trap cause.
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func $_ZN5guest5crashE unreachable)
        (func (export "pb_main") call $_ZN5guest5crashE)"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let Err(RuntimeError::GuestTrap(report)) = run(&runtime, &wasm) else {
            panic!("guest must trap")
        };
        assert!(report.contains("guest::crash"), "{backend:?}: {report}");
        assert!(report.contains("wasm `unreachable` instruction executed"), "{report}");
    }
}

#[test]
fn a_guest_is_compiled_once_per_load() {
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main"))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let compilations = || runtime.compilations.load(std::sync::atomic::Ordering::Relaxed);
        runtime.validate_guest(&wasm).unwrap();
        assert_eq!(compilations(), 1, "validation compiles the guest");
        runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap().pump().unwrap();
        assert_eq!(
            compilations(),
            2,
            "creating a guest compiles it once, not once to check and again to run"
        );
    }
}

#[test]
fn prepared_guest_moves_to_its_vm_thread_without_recompiling() {
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main"))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let prepared = runtime.prepare_guest(&wasm).unwrap();
        assert_eq!(runtime.compilations.load(std::sync::atomic::Ordering::Relaxed), 1);
        std::thread::spawn(move || {
            prepared.instantiate(context(), Box::new(NoData)).unwrap().pump().unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(runtime.compilations.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}

#[test]
fn implicit_start_is_rejected_before_any_guest_execution() {
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func $initialize unreachable) (start $initialize)
        (func (export "pb_main"))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(matches!(
            runtime.validate_guest(&wasm),
            Err(RuntimeError::UnsupportedStartFunction)
        ));
        assert!(matches!(run(&runtime, &wasm), Err(RuntimeError::UnsupportedStartFunction)));
    }
}

#[test]
fn app_validation_checks_imports_and_export_types_without_instantiation() {
    let wrong_main = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") (param i32))"#,
    );
    let wrong_memory = module(r#"(func (export "memory")) (func (export "pb_main"))"#);
    let wrong_signature = wat::parse_str(
        r#"(module
        (import "playbit" "pb_syscall" (func))
        (memory (export "memory") 1) (func (export "pb_main")))"#,
    )
    .unwrap();
    let invalid_data_offset = module(
        r#"(memory (export "memory") 1)
        (data (i32.const 65536) "outside memory") (func (export "pb_main"))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(matches!(
            runtime.validate_guest(&wrong_main),
            Err(RuntimeError::InvalidMainSignature(_))
        ));
        assert!(matches!(run(&runtime, &wrong_main), Err(RuntimeError::InvalidMainSignature(_))));
        assert!(matches!(
            runtime.validate_guest(&wrong_memory),
            Err(RuntimeError::InvalidMemoryExport)
        ));
        assert!(matches!(run(&runtime, &wrong_memory), Err(RuntimeError::InvalidMemoryExport)));
        assert!(matches!(
            runtime.validate_guest(&wrong_signature),
            Err(RuntimeError::Instantiation(_))
        ));
        assert!(matches!(run(&runtime, &wrong_signature), Err(RuntimeError::Instantiation(_))));
        // Validation checks interfaces without applying data segments.
        runtime.validate_guest(&invalid_data_offset).unwrap();
        assert!(matches!(run(&runtime, &invalid_data_offset), Err(RuntimeError::Instantiation(_))));
    }
}

#[test]
fn finite_work_beyond_the_previous_budget_completes_on_each_backend() {
    // Run finite work exceeding the former 100-million-unit default, then
    // prove that the same work is rejected with that explicit finite limit.
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") (local $remaining i32)
            i32.const 30000000 local.set $remaining
            (loop $work
                local.get $remaining i32.const 1 i32.sub
                local.tee $remaining br_if $work))"#,
    );
    for &backend in test_backends() {
        let unlimited = Runtime::with_backend(backend).unwrap();
        let limited =
            Runtime::with_backend_and_fuel_limit(backend, NonZeroU64::new(100_000_000)).unwrap();
        assert!(run(&unlimited, &wasm).is_ok());
        assert!(matches!(run(&limited, &wasm), Err(RuntimeError::FuelExhausted)));
    }
}

#[test]
fn runaway_guest_exhausts_its_budget() {
    // Only execute this runaway with an explicit small budget. Exhaustion is
    // terminal even when the caller attempts another turn.
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") (loop $forever br $forever))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend_and_fuel_limit(backend, NonZeroU64::new(1000)).unwrap();
        let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        assert!(matches!(guest.pump(), Err(RuntimeError::FuelExhausted)));
        assert!(matches!(guest.pump(), Err(RuntimeError::GuestHalted)));
    }
}

#[test]
fn unlimited_engines_disable_metering_and_execute_host_calls() {
    // get_fuel and set_fuel must fail on the default engine: a large counter
    // masquerading as unlimited would pass ordinary finite-work tests.
    let wasm = module(
        r#"(memory (export "memory") 1)
        (data (i32.const 8) "unlimited")
        (func (export "pb_main")
            (if (i64.eqz (call $dispatch_log (i32.const 16)
                (i64.const 0) (i64.const 0) (i64.const 0)
                (i64.const 0) (i64.const 0) (i64.const 0))) (then unreachable))
            i32.const 8 i32.const 9 call $log
            if unreachable end)"#,
    );
    for &backend in test_backends() {
        for runtime in [
            Runtime::with_backend(backend).unwrap(),
            Runtime::with_backend_and_fuel_limit(backend, None).unwrap(),
        ] {
            let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
            assert!(guest.store.get_fuel().is_err());
            assert!(guest.store.set_fuel(u64::MAX).is_err());
            for _ in 0..2 {
                assert_eq!(guest.pump().unwrap().logs(), &["unlimited"]);
            }
        }
    }
    let runtime = Runtime::new().unwrap();
    let guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
    assert!(guest.store.get_fuel().is_err());
}

#[test]
fn positive_budgets_reset_per_turn_without_affecting_other_instances() {
    // Each turn costs more than half the limit. Interleave two limited guests
    // with an unlimited guest to expose missing resets or shared fuel state.
    // Write the log bytes during the turn, avoiding Wasmtime's separately
    // metered static-memory image initialization before pb_main.
    let wasm = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") (local $remaining i32)
            i32.const 8 i32.const 0x6e727574 i32.store
            i32.const 150 local.set $remaining
            (loop $work local.get $remaining i32.const 1 i32.sub
                local.tee $remaining br_if $work)
            i32.const 8 i32.const 4 call $log
            if unreachable end)"#,
    );
    for &backend in test_backends() {
        let limited = Runtime::with_backend_and_fuel_limit(backend, NonZeroU64::new(2500)).unwrap();
        let unlimited = Runtime::with_backend(backend).unwrap();
        let mut first = limited.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        let mut second = limited.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        let mut ordinary = unlimited.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        for _ in 0..3 {
            assert_eq!(first.pump().unwrap().logs(), &["turn"]);
            let remaining = first.store.get_fuel().unwrap();
            assert!(remaining < 1250, "each turn consumes more than half the budget");
            assert_eq!(ordinary.pump().unwrap().logs(), &["turn"]);
            assert!(ordinary.store.get_fuel().is_err());
            assert_eq!(second.pump().unwrap().logs(), &["turn"]);
            assert_eq!(second.store.get_fuel().unwrap(), remaining);
        }
    }
}

#[test]
fn same_guest_limits_apply_on_native_and_pulley() {
    let memory_growth = module(&format!(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") i32.const {} memory.grow drop)"#,
        DEFAULT_MEMORY_LIMIT / 65536
    ));
    let memory_load = module(
        r#"(memory (export "memory") 1)
        (func (export "pb_main") i32.const 65535 i32.load drop)"#,
    );
    let ambient_import = module(
        r#"(import "wasi_snapshot_preview1" "fd_write"
        (func (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1) (func (export "pb_main"))"#,
    );
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        assert!(matches!(run(&runtime, &memory_growth), Err(RuntimeError::GuestTrap(_))));
        assert!(matches!(run(&runtime, &memory_load), Err(RuntimeError::GuestTrap(_))));
        assert!(matches!(
            runtime.validate_guest(&ambient_import),
            Err(RuntimeError::InvalidModule(_))
        ));
        assert!(matches!(run(&runtime, &ambient_import), Err(RuntimeError::InvalidModule(_))));
    }
}

#[test]
fn both_backends_reject_unknown_imports() {
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        for mixed in [false, true] {
            let current = if mixed { LOG_IMPORT } else { "" };
            let wasm = wat::parse_str(format!(
                r#"(module {current}
                (import "playbit" "unknown_test_import" (func))
                (memory (export "memory") 1) (func (export "pb_main")))"#
            ))
            .unwrap();
            assert!(
                matches!(runtime.validate_guest(&wasm), Err(RuntimeError::InvalidModule(_))),
                "{backend:?} mixed={mixed}"
            );
            assert!(
                matches!(
                    runtime.create_guest(&wasm, context(), Box::new(NoData)),
                    Err(RuntimeError::InvalidModule(_))
                ),
                "{backend:?} mixed={mixed}"
            );
        }
    }
}

#[test]
fn pure_apps_need_no_imports() {
    let wasm = wat::parse_str(r#"(module (memory (export "memory") 1) (func (export "pb_main")))"#)
        .unwrap();
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        runtime.validate_guest(&wasm).unwrap();
        assert!(run(&runtime, &wasm).unwrap().logs().is_empty());
    }
}

#[test]
fn unknown_opcode_is_unsupported() {
    let wasm = wat::parse_str(r#"(module
        (import "playbit" "pb_syscall" (func $syscall (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (memory (export "memory") 1)
        (func (export "pb_main")
            (if (i64.ne
                (call $syscall (i32.const -1) (i64.const -1) (i64.const -1)
                    (i64.const -1) (i64.const -1) (i64.const -1) (i64.const -1))
                (i64.const -7)) (then unreachable))))"#).unwrap();
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        runtime.validate_guest(&wasm).unwrap();
        run(&runtime, &wasm).unwrap();
    }
}

#[test]
fn last_window_intent_reaches_both_wasm_backends_without_memory_or_io() {
    for &backend in test_backends() {
        for intent in [0, 1] {
            let wasm = module(&format!(
                r#"
                (memory (export "memory") 1)
                (func (export "pb_main")
                    (if (i64.ne (call $dispatch_log (i32.const 14)
                        (i64.const {intent}) (i64.const 0) (i64.const 0)
                        (i64.const 0) (i64.const 0) (i64.const 0)) (i64.const 0))
                        (then unreachable)))"#
            ));
            let runtime = Runtime::with_backend(backend).unwrap();
            let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
            assert_eq!(guest.pump().unwrap().quit_on_last_window(), intent == 1);
            guest.deactivate();
            assert_eq!(guest.pump().unwrap().quit_on_last_window(), intent == 1);
        }
    }
}

#[cfg(feature = "native-runtime")]
#[test]
#[allow(unsafe_code, reason = "Fixture owns callback lifetime and opaque scalar state.")]
fn last_window_intent_reaches_the_native_scalar_boundary() {
    use playbit_api::abi::operations as abi;
    use std::ffi::c_void;
    unsafe extern "C" fn init(
        _: *mut abi::Context,
        intent: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        // SAFETY: NativeInstance lends the live exclusive output; intent is an
        // opaque scalar token, with no allocation or borrowed data to retain.
        unsafe { out.write(intent) };
        0
    }
    unsafe extern "C" fn turn(context: *mut abi::Context, intent: *mut c_void) -> i32 {
        // SAFETY: the host lends the live context on its owning callback thread.
        i32::try_from(unsafe { abi::pb_quit_on_last_window(context, intent as usize as u32) })
            .expect("status-only syscall returns a defined i32 error")
    }
    unsafe extern "C" fn destroy(_: *mut c_void) {}
    for intent in [0, 1] {
        let app = abi::App {
            size: core::mem::size_of::<abi::App>() as u32,
            written: 0,
            init: Some(init),
            turn: Some(turn),
            destroy: Some(destroy),
            user_data: intent as usize as *mut c_void,
        };
        // SAFETY: static callbacks retain only an opaque scalar, never pointers.
        let mut guest = unsafe {
            crate::runtime::native::NativeInstance::new(context(), Box::new(NoData), app)
        }
        .unwrap();
        assert_eq!(guest.pump().unwrap().quit_on_last_window(), intent == 1);
        guest.deactivate();
        assert_eq!(guest.pump().unwrap().quit_on_last_window(), intent == 1);
    }
}

#[test]
fn gc_cycles_are_collected_and_state_survives_turns() {
    // Allocate more than the heap budget in discarded self-cycles while retaining
    // one root across calls. Atomics also exercise Pulley's GC-aware lowering.
    let wasm = module(&format!(
        r#"
        (type $bytes (array (mut i8)))
        (type $node (struct (field (mut (ref null $node))) (field (ref $bytes))))
        (global $turn (mut i32) (i32.const 0))
        (global $root (mut (ref null $node)) (ref.null $node))
        (memory (export "memory") {pages})
        (func (export "pb_main") (local $i i32) (local $node (ref $node))
            (if (i32.eqz (global.get $turn)) (then
                (global.set $root (struct.new $node (ref.null $node)
                    (array.new $bytes (i32.const 42) (i32.const 16))))))
            (loop $allocate
                (local.set $node (struct.new $node (ref.null $node)
                    (array.new $bytes (i32.const 7) (i32.const 1024))))
                (struct.set $node 0 (local.get $node) (local.get $node))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br_if $allocate (i32.lt_u (local.get $i) (i32.const 25000))))
            (if (i32.ne (array.get_u $bytes
                (struct.get $node 1 (ref.as_non_null (global.get $root))) (i32.const 0))
                (i32.const 42)) (then unreachable))
            (global.set $turn (i32.add (global.get $turn) (i32.const 1)))
            (i32.atomic.store (i32.const 0) (global.get $turn)))
    "#,
        pages = (DEFAULT_MEMORY_LIMIT - 16 * 1024 * 1024) / 65536
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        for turn in 1_i32..=3 {
            guest.pump().unwrap();
            let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
            assert_eq!(&memory.data(&guest.store)[..4], &turn.to_le_bytes());
        }
    }
}

#[test]
fn gc_heap_and_linear_memory_share_a_budget() {
    // Either memory fits individually, but their combined capacities exceed the budget.
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        for (pages, heap_bytes) in
            [(DEFAULT_MEMORY_LIMIT / 65536 - 16, 2 * 1024 * 1024), (1, DEFAULT_MEMORY_LIMIT)]
        {
            let wasm = module(&format!(
                r#"
                (type $bytes (array (mut i8)))
                (memory (export "memory") {pages})
                (func (export "pb_main")
                    (drop (array.new $bytes (i32.const 0) (i32.const {heap_bytes}))))"#
            ));
            let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
            assert!(matches!(guest.pump(), Err(RuntimeError::GuestTrap(_))));
            assert!(guest.pump().is_err(), "a trapped generation must stay halted");
        }
    }
}

#[test]
#[ignore = "requires gmake -C playbit/sdk/ocaml wasm; exercises the external compiler fork"]
fn ocaml_counter_matches_rust_scene_and_retains_state() {
    // Run actual generated OCaml through both production backends. Compare the
    // whole scene with the Rust encoder and force the event capacity retry.
    use playbit_api::{
        context::ContextState,
        ui::{ButtonStyle, TextStyle},
    };
    use std::time::Instant;
    use syscall::{Event, EventKind};
    let wasm = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sdk/ocaml/_build/wasm/debug/counter.wasm"
    ))
    .expect("build the OCaml fixture first");
    for &backend in test_backends() {
        let started = Instant::now();
        let runtime = Runtime::with_backend(backend).unwrap();
        let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
        let frame = guest.pump().unwrap();
        assert_eq!(frame.logs(), &["OCaml initialized"]);
        assert!(frame.quit_on_last_window());
        let mut state = ContextState::default();
        let mut view = state.view();
        view.column_start();
        view.text(TextStyle::Title, "OCaml on Playbit");
        view.text(TextStyle::Body, "Count: 0");
        view.text(TextStyle::Muted, "0 events · Wasm GC");
        view.button(1, "Increment", ButtonStyle::Primary, false);
        view.button(2, "Reset", ButtonStyle::Default, false);
        view.column_end();
        assert_eq!(frame.view_bytes(), Some(view.finish().as_slice()));
        let startup = started.elapsed();
        let mut startups = vec![startup.as_secs_f64() * 1000.0];
        for _ in 1..100 {
            let started = Instant::now();
            let runtime = Runtime::with_backend(backend).unwrap();
            let mut sample = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
            assert_eq!(sample.pump().unwrap().logs(), &["OCaml initialized"]);
            startups.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        startups.sort_by(f64::total_cmp);
        eprintln!(
            "OCaml {backend:?}: 100 engine/compile/first-turn starts p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms (no GUI)",
            startups[49], startups[94], startups[98], startups[99]
        );
        let mut samples = Vec::new();
        for count in 1..=1000 {
            let started = Instant::now();
            guest
                .queue_event(
                    guest.generation(),
                    Event {
                        kind: EventKind::Click,
                        target: 1,
                        data: if count == 1 { vec![42; 512] } else { Vec::new() },
                    },
                )
                .unwrap();
            let frame = guest.pump().unwrap();
            assert_eq!(frame.logs(), &[format!("Count: {count} ({count} events)")]);
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        guest
            .queue_event(
                guest.generation(),
                Event { kind: EventKind::Click, target: 2, data: Vec::new() },
            )
            .unwrap();
        assert_eq!(guest.pump().unwrap().logs(), &["Count: 0 (1001 events)"]);
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "OCaml {backend:?}: compile/start={:.3}ms, 1000 turns p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms (runtime boundary, not UI)",
            startup.as_secs_f64() * 1000.0,
            samples[500],
            samples[950],
            samples[990],
            samples[999]
        );
    }
}

#[cfg(feature = "app-host")]
#[test]
fn trusted_compiled_cache_reuses_import_across_guests_and_engine_restart() {
    // One compilation serves concurrent validation, guest loads and a later engine.
    use super::compiled::Cache as _;
    let directory = tempfile::tempdir().unwrap();
    let wasm = crate::engine::tests::button_package().wasm().unwrap();
    let cache =
        std::sync::Arc::new(crate::engine::compiled_cache::Cache::open(directory.path()).unwrap());
    let runtime = Runtime::new().unwrap().with_cache(cache.clone());
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let runtime = runtime.clone();
            let wasm = &wasm;
            scope.spawn(move || runtime.validate_guest(wasm).unwrap());
        }
    });
    assert_eq!(runtime.compilations.load(std::sync::atomic::Ordering::Relaxed), 1);
    let descriptor =
        cache.descriptor(*blake3::hash(&wasm).as_bytes(), runtime.compatibility()).unwrap();
    let pulley = Runtime::with_backend(ExecutionBackend::Pulley).unwrap();
    assert_ne!(runtime.compatibility(), pulley.compatibility());
    let image = crate::engine::compiled_cache::read(directory.path(), descriptor.clone()).unwrap();
    assert!(pulley.prepare_compiled(&wasm, image).is_err());
    let guest_runtime = Runtime::new().unwrap();
    let image = crate::engine::compiled_cache::read(directory.path(), descriptor.clone()).unwrap();
    guest_runtime
        .prepare_compiled(&wasm, image)
        .unwrap()
        .instantiate(context(), Box::new(NoData))
        .unwrap();
    assert_eq!(guest_runtime.compilations.load(std::sync::atomic::Ordering::Relaxed), 0);
    drop(runtime);
    drop(cache);
    let cache =
        std::sync::Arc::new(crate::engine::compiled_cache::Cache::open(directory.path()).unwrap());
    let runtime = Runtime::new().unwrap().with_cache(cache);
    runtime.validate_guest(&wasm).unwrap();
    assert_eq!(runtime.compilations.load(std::sync::atomic::Ordering::Relaxed), 0);
    let mut different = wasm.clone();
    different.extend_from_slice(&[0, 1, 0]);
    let image = crate::engine::compiled_cache::read(directory.path(), descriptor).unwrap();
    assert!(runtime.prepare_compiled(&different, image).is_err());
}

#[cfg(feature = "app-host")]
#[test]
fn compiled_cache_separates_metering_modes_and_reuses_positive_limits() {
    // The same cache stores one image per metering mode. Positive budget
    // values remain store state and do not cause separate compilations.
    use super::compiled::Cache as _;
    let wasm = module(r#"(memory (export "memory") 1) (func (export "pb_main"))"#);
    for &backend in test_backends() {
        let directory = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(
            crate::engine::compiled_cache::Cache::open(directory.path()).unwrap(),
        );
        let unlimited = Runtime::with_backend(backend).unwrap().with_cache(cache.clone());
        let limited = unlimited.for_fuel_limit(NonZeroU64::new(2000)).unwrap();
        let another_limit = limited.for_fuel_limit(NonZeroU64::new(3000)).unwrap();
        assert_ne!(unlimited.compatibility(), limited.compatibility());
        assert_eq!(limited.compatibility(), another_limit.compatibility());
        std::thread::scope(|scope| {
            for runtime in [&unlimited, &limited, &another_limit] {
                let wasm = &wasm;
                scope.spawn(move || runtime.validate_guest(wasm).unwrap());
            }
        });
        for runtime in [&unlimited, &limited, &another_limit] {
            assert_eq!(
                runtime.compilations.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "one compilation per mode, shared by different positive budgets"
            );
        }
        let digest = *blake3::hash(&wasm).as_bytes();
        for (source, incompatible) in [(&unlimited, &limited), (&limited, &unlimited)] {
            let descriptor = cache.descriptor(digest, source.compatibility()).unwrap();
            let image = crate::engine::compiled_cache::read(directory.path(), descriptor).unwrap();
            assert!(incompatible.prepare_compiled(&wasm, image).is_err());
        }
        let descriptor = cache.descriptor(digest, limited.compatibility()).unwrap();
        let image = crate::engine::compiled_cache::read(directory.path(), descriptor).unwrap();
        let mut guest = another_limit
            .prepare_compiled(&wasm, image)
            .unwrap()
            .instantiate(context(), Box::new(NoData))
            .unwrap();
        guest.pump().unwrap();
        assert!(guest.store.get_fuel().unwrap() > 2000, "handoff retains the receiving budget");
    }
}

#[cfg(feature = "app-host")]
#[test]
fn oversized_compiled_module_remains_runnable_without_persistence() {
    // A large passive segment needs no large guest memory allocation, but its
    // native image exceeds the persistence limit. It must still instantiate.
    use wasm_encoder::{Encode, Section};
    let directory = tempfile::tempdir().unwrap();
    let cache =
        std::sync::Arc::new(crate::engine::compiled_cache::Cache::open(directory.path()).unwrap());
    let runtime = Runtime::new().unwrap().with_cache(cache);
    let mut wasm =
        wat::parse_str("(module (memory (export \"memory\") 1) (func (export \"pb_main\")))")
            .unwrap();
    {
        let mut data = wasm_encoder::DataSection::new();
        data.passive(std::iter::repeat_n(0, super::compiled::MAX_IMAGE_BYTES));
        wasm.push(data.id());
        data.encode(&mut wasm);
    }
    let mut guest = runtime.create_guest(&wasm, context(), Box::new(NoData)).unwrap();
    guest.pump().unwrap();
    assert!(
        runtime
            .compiled_descriptor(*blake3::hash(&wasm).as_bytes(), runtime.compatibility())
            .is_none()
    );
    assert_eq!(std::fs::read_dir(directory.path().join("cache/wasm-vm")).unwrap().count(), 0);
}

fn deadline_context() -> GuestContext {
    let mut context = context();
    context.required_capabilities.insert(Opcode::EventPollDeadline.capability().into());
    context
}

const POLL_FIXTURE: &str = r#"
    (memory (export "memory") 1)
    (func $poll (param $op i32) (param $n i64) (param $deadline i64) (result i64)
        (i32.store (i32.const 64) (i32.const 24))
        (call $dispatch_log (local.get $op)
            (i64.const 64) (i64.const 128) (local.get $n) (local.get $deadline)
            (i64.const 0) (i64.const 0)))
    (func $drain
        ;; Construct "waitingresumed" on entry so static-image initialization
        ;; cannot consume the deliberately small per-turn fuel test budget.
        (i64.store (i32.const 32) (i64.const 0x72676e6974696177))
        (i64.store (i32.const 40) (i64.const 0x64656d757365))
        (loop $more
            (br_if $more (i64.ge_s
                (call $poll (i32.const 4) (i64.const 128) (i64.const 0)) (i64.const 0)))))
    (func $clock (result i64)
        (call $dispatch_log (i32.const 16) (i64.const 0) (i64.const 0) (i64.const 0)
             (i64.const 0) (i64.const 0) (i64.const 0)))"#;

#[test]
fn deadline_poll_publishes_then_resumes_the_selected_guest_stack() {
    // Two overlapping waits exercise Wasmtime's per-fiber trap/TLS restoration.
    // A size probe on resume leaves the payload queued even with an expired deadline.
    let mut context_state = playbit_api::context::ContextState::default();
    let mut view = context_state.view();
    view.column_start();
    view.text(playbit_api::ui::TextStyle::Body, "Waiting for input");
    view.column_end();
    let scene = view.finish();
    let bytes: String = scene.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let wasm = module(&format!(
        r#"{POLL_FIXTURE}
        (data (i32.const 512) "{bytes}")
        (global $entries (mut i32) (i32.const 0))
        (func (export "pb_main") (local $retained i32)
            (if (global.get $entries) (then unreachable))
            (global.set $entries (i32.const 1))
            (call $drain)
            (local.set $retained (i32.const 42))
            (drop (call $log (i32.const 32) (i32.const 7)))
            (drop (call $dispatch_log (i32.const 3) (i64.const 512) (i64.const {})
                (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
            (if (i64.ne (call $poll (i32.const 21) (i64.const 0) (i64.const -1))
                        (i64.const 3)) (then unreachable))
            (if (i32.load (i32.const 68)) (then unreachable))
            (if (i64.ne (call $poll (i32.const 21) (i64.const 3) (i64.const 0))
                        (i64.const 3)) (then unreachable))
            (if (i32.ne (i32.load8_u (i32.const 128)) (local.get $retained)) (then unreachable))
            (if (i32.ne (i32.load (i32.const 84)) (i32.const 7)) (then unreachable))
            (drop (call $log (i32.const 39) (i32.const 7))))"#,
        scene.len()
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let create = || runtime.create_guest(&wasm, deadline_context(), Box::new(NoData)).unwrap();
        let mut first = create();
        let mut second = create();
        for guest in [&mut first, &mut second] {
            let frame = guest.pump().unwrap();
            assert_eq!(frame.logs(), &["waiting"]);
            assert_eq!(frame.view_bytes(), Some(scene.as_slice()));
            assert!(!guest.has_ready_work(std::time::Instant::now()));
            assert_eq!(guest.wake_after(std::time::Instant::now()), None);
        }
        first
            .queue_event(
                first.generation(),
                syscall::Event {
                    kind: syscall::EventKind::Submit,
                    target: 7,
                    data: vec![42, 2, 3],
                },
            )
            .unwrap();
        assert_eq!(first.pump().unwrap().logs(), &["resumed"]);
        drop(second); // Cancellation while suspended cannot execute its guest continuation.
    }
}

#[test]
fn session_info_stays_with_its_guest_across_deadline_waits() {
    struct SessionHost(u8);
    impl Host for SessionHost {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
        fn session_key_secret(&self) -> syscall::HostResult<Option<[u8; 16]>> {
            Ok(Some([self.0; 16]))
        }
    }

    // The event carries this fixture's expected byte. Each suspended guest must
    // get its own binding back when the host resumes them in reverse order.
    let wasm = module(&format!(
        r#"{POLL_FIXTURE}
        (func $session (result i32)
            (i32.store (i32.const 256) (i32.const 64))
            (if (i64.ne (call $dispatch_log (i32.const 0x163) (i64.const 256)
                    (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))
                    (i64.const 0)) (then unreachable))
            (if (i32.ne (i32.load (i32.const 264)) (i32.const 1)) (then unreachable))
            (if (i32.ne (i32.load8_u (i32.const 272)) (i32.load8_u (i32.const 304)))
                (then unreachable))
            (i32.load8_u (i32.const 304)))
        (func (export "pb_main") (local $before i32)
            (call $drain)
            (local.set $before (call $session))
            (if (i64.ne (call $poll (i32.const 21) (i64.const 1) (i64.const -1)) (i64.const 1))
                (then unreachable))
            (if (i32.ne (local.get $before) (i32.load8_u (i32.const 128))) (then unreachable))
            (if (i32.ne (call $session) (local.get $before)) (then unreachable)))"#
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        let mut guests = [3, 7].map(|tag| {
            let mut context = deadline_context();
            context.identity.session_id = Some([tag; 32]);
            let mut guest =
                runtime.create_guest(&wasm, context, Box::new(SessionHost(tag))).unwrap();
            guest.pump().unwrap();
            (tag, guest)
        });
        for (tag, guest) in guests.iter_mut().rev() {
            let event =
                syscall::Event { kind: syscall::EventKind::Submit, target: 7, data: vec![*tag] };
            guest.queue_event(guest.generation(), event).unwrap();
            guest.pump().unwrap();
        }
    }
}

#[test]
fn deadline_poll_uses_the_host_clock_and_only_opted_in_guests_can_wait() {
    let wasm = module(&format!(
        r#"{POLL_FIXTURE}
        (func (export "pb_main") (local $deadline i64)
            (call $drain)
            (local.set $deadline (i64.add (call $clock) (i64.const 20000000)))
            (if (i64.ne (call $poll (i32.const 21) (i64.const 0) (local.get $deadline))
                        (i64.const -6)) (then unreachable))
            (if (i64.lt_u (call $clock) (local.get $deadline)) (then unreachable))
            (drop (call $log (i32.const 39) (i32.const 7))))"#
    ));
    let legacy = module(&format!(
        r#"{POLL_FIXTURE}
        (func (export "pb_main")
            (if (i64.ne (call $poll (i32.const 21) (i64.const 0) (i64.const -1))
                        (i64.const -7)) (then unreachable)))"#
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        run(&runtime, &legacy).unwrap();
        let mut guest = runtime.create_guest(&wasm, deadline_context(), Box::new(NoData)).unwrap();
        assert!(guest.pump().unwrap().logs().is_empty());
        // A spurious pump can only rearm the same deadline; it cannot return
        // WouldBlock until the monotonic clock actually reaches that deadline.
        while !guest.wake_due(std::time::Instant::now()) {
            std::thread::sleep(guest.wake_after(std::time::Instant::now()).unwrap());
        }
        assert_eq!(guest.pump().unwrap().logs(), &["resumed"]);
        assert_eq!(guest.wake_after(std::time::Instant::now()), None);
    }
}

#[test]
fn deadline_poll_refills_fuel_on_suspension_only() {
    // This single invocation stays live across many callbacks, spending more
    // than its entire per-turn budget in aggregate. An immediate poll never
    // surrenders the VM, so a tight loop still traps when it spends its budget.
    let wait = module(&format!(
        r#"{POLL_FIXTURE}
        (func (export "pb_main") (local $i i32)
            (call $drain)
            (loop $wait
                (if (i64.ne (call $poll (i32.const 21) (i64.const 0) (i64.const -1))
                            (i64.const 0)) (then unreachable))
                (local.set $i (i32.const 12))
                (loop $burn
                    (drop (call $clock))
                    (br_if $burn (local.tee $i (i32.sub (local.get $i) (i32.const 1)))))
                (drop (call $log (i32.const 39) (i32.const 7)))
                (br $wait)))"#
    ));
    let spin = module(&format!(
        r#"{POLL_FIXTURE}
        (func (export "pb_main")
            (call $drain)
            (loop $spin
                (drop (call $poll (i32.const 21) (i64.const 0) (i64.const 0)))
                (br $spin)))"#
    ));
    for &backend in test_backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, NonZeroU64::new(24_000)).unwrap();
        let mut guest = runtime.create_guest(&wait, deadline_context(), Box::new(NoData)).unwrap();
        assert!(guest.pump().unwrap().logs().is_empty());
        for _ in 0..8 {
            guest
                .queue_event(
                    guest.generation(),
                    syscall::Event { kind: syscall::EventKind::Click, target: 7, data: Vec::new() },
                )
                .unwrap();
            assert_eq!(guest.pump().unwrap().logs(), &["resumed"]);
        }
        let mut spinner =
            runtime.create_guest(&spin, deadline_context(), Box::new(NoData)).unwrap();
        assert!(matches!(spinner.pump(), Err(RuntimeError::FuelExhausted)));
        // A trap in one store and cancellation of its peer must restore TLS.
        drop(guest);
        assert!(matches!(spinner.pump(), Err(RuntimeError::GuestHalted)));
    }
}

#[test]
fn deadline_poll_retirement_cancels_timeouts_and_drains_accepted_events() {
    let wasm = module(&format!(
        r#"{POLL_FIXTURE}
        (func (export "pb_main") (local $deadline i64)
            (call $drain)
            (local.set $deadline (i64.add (call $clock) (i64.const 20000000)))
            (if (i64.ne (call $poll (i32.const 21) (i64.const 0) (local.get $deadline))
                        (i64.const 0)) (then unreachable))
            (if (i32.ne (i32.load (i32.const 80)) (i32.const 2)) (then unreachable))
            (if (i64.ne (call $poll (i32.const 21) (i64.const 0) (i64.const -1))
                        (i64.const -10)) (then unreachable))
            (drop (call $log (i32.const 39) (i32.const 7))))"#
    ));
    for &backend in test_backends() {
        let runtime = Runtime::with_backend(backend).unwrap();
        for queued in [false, true] {
            let mut guest =
                runtime.create_guest(&wasm, deadline_context(), Box::new(NoData)).unwrap();
            assert!(guest.pump().unwrap().logs().is_empty());
            while !guest.wake_due(std::time::Instant::now()) {
                std::thread::sleep(guest.wake_after(std::time::Instant::now()).unwrap());
            }
            if queued {
                guest
                    .queue_event(
                        guest.generation(),
                        syscall::Event { kind: syscall::EventKind::Click, target: 7, data: vec![] },
                    )
                    .unwrap();
            }
            guest.deactivate();
            assert_eq!(guest.wake_after(std::time::Instant::now()), None);
            assert_eq!(guest.has_ready_work(std::time::Instant::now()), queued);
            let frame = guest.pump().unwrap();
            if queued {
                assert_eq!(frame.logs(), &["resumed"]);
            } else {
                assert!(frame.logs().is_empty());
            }
            assert!(!guest.drain_pending());
        }
    }
}
