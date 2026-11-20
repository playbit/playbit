#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::runtime::Runtime;
use wasmtime::{Instance, Store};

fn features() -> WasmFeatures {
    WasmFeatures::default()
}
fn runtime(pulley: bool) -> Runtime {
    Runtime::with_backend_and_fuel_limit(
        if pulley {
            crate::runtime::ExecutionBackend::Pulley
        } else {
            crate::runtime::ExecutionBackend::Native
        },
        std::num::NonZeroU64::new(crate::runtime::TEST_FUEL),
    )
    .unwrap()
}

fn execute(runtime: &Runtime, bytes: &[u8], fuel: u64) -> (Result<i64, ()>, Vec<u8>, u64) {
    let module = runtime.module(bytes).unwrap();
    let mut store = Store::new(&runtime.engine, ());
    store.set_fuel(fuel).unwrap();
    let imports = module
        .imports()
        .map(|import| {
            assert_eq!((import.module(), import.name()), ("host", "identity"));
            wasmtime::Func::wrap(&mut store, |value: i32| value).into()
        })
        .collect::<Vec<_>>();
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let function = instance.get_typed_func::<(i32, i64), i64>(&mut store, "run").unwrap();
    let result = function.call(&mut store, (16, 0x1122334455667788)).map_err(|_| ());
    let memory = instance
        .get_memory(&mut store, "memory")
        .map(|m| m.data(&store).to_vec())
        .unwrap_or_default();
    (result, memory, store.get_fuel().unwrap())
}
fn module(body: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module (memory (export "memory") 1)
        (func (export "run") (param $address i32) (param $seed i64) (result i64)
            (local $existing i32)
            local.get $address local.get $seed i64.store
            {body}))"#
    ))
    .unwrap()
}
fn compare(native: &Runtime, pulley: &Runtime, original: &[u8], must_trap: bool) {
    let bytes = prepare(original, features(), true).unwrap();
    assert!(matches!(bytes, Cow::Owned(_)));
    let a = execute(native, original, 100_000);
    let b = execute(pulley, &bytes, 100_000);
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, b.1, "memory changed differently");
    assert_eq!(a.0.is_err(), must_trap);
    assert!(b.2 < 100_000, "lowering must remain fuel-metered");
}

// Exact scalar opcode matrix: 7 widths × (load/store + 6 RMW + CAS) = 63.
#[test]
#[cfg(not(target_os = "ios"))]
fn every_scalar_atomic_matches_native_including_narrow_cas_and_wrapping() {
    let native = runtime(false);
    let pulley = runtime(true);
    let mut opcodes = 0;
    for (ty, width, full) in [
        ("i32", 8, 32),
        ("i32", 16, 32),
        ("i32", 32, 32),
        ("i64", 8, 64),
        ("i64", 16, 64),
        ("i64", 32, 64),
        ("i64", 64, 64),
    ] {
        let narrow = if width == full { String::new() } else { width.to_string() };
        let suffix = if narrow.is_empty() { "" } else { "_u" };
        let widen = if ty == "i32" { "i64.extend_i32_u" } else { "" };
        for operation in ["load", "store", "add", "sub", "and", "or", "xor", "xchg", "cmpxchg"] {
            let opcode = match operation {
                "load" => format!("{ty}.atomic.load{narrow}{suffix}"),
                "store" => format!("{ty}.atomic.store{narrow}"),
                other => format!("{ty}.atomic.rmw{narrow}.{other}{suffix}"),
            };
            opcodes += 1;
            // Values deliberately include high bits above every narrow width.
            for operand in ["0", "-1", "0x1122334455667788"] {
                let operand =
                    if ty == "i32" && operand.starts_with("0x") { "0x55667788" } else { operand };
                let argument = format!("{ty}.const {operand}");
                let body = match operation {
                    "load" => format!("local.get $address {opcode} {widen}"),
                    "store" => format!("local.get $address {argument} {opcode} i64.const 0"),
                    "cmpxchg" => {
                        format!("local.get $address {argument} {ty}.const -1 {opcode} {widen}")
                    }
                    _ => format!("local.get $address {argument} {opcode} {widen}"),
                };
                compare(&native, &pulley, &module(&body), false);
            }
        }
    }
    assert_eq!(opcodes, 63);
}

#[test]
#[cfg(not(target_os = "ios"))]
fn addresses_alignment_offsets_and_no_write_on_trap_match_native() {
    let native = runtime(false);
    let pulley = runtime(true);
    for (address, offset, trap) in [
        (16u32, 0u32, false),
        (12, 4, false),
        (17, 0, true),
        (16, 1, true),
        (65532, 0, false),
        (65536, 0, true),
        (0xfffffffcu32, 4, true),
        (4, 0xfffffffcu32, true),
    ] {
        let body = format!(
            "i32.const {address} i32.const 123 i32.atomic.rmw.add offset={offset} i64.extend_i32_u"
        );
        compare(&native, &pulley, &module(&body), trap);
    }
}

#[test]
#[cfg(not(target_os = "ios"))]
fn notify_wait_and_fence_have_unshared_memory_semantics() {
    let native = runtime(false);
    let pulley = runtime(true);
    for (body, trap) in [
        ("i32.const 16 i32.const -1 memory.atomic.notify i64.extend_i32_u", false),
        ("i32.const 17 i32.const 0 memory.atomic.notify i64.extend_i32_u", true),
        ("i32.const 65536 i32.const 0 memory.atomic.notify i64.extend_i32_u", true),
        ("i32.const 16 i32.const 0 i64.const 0 memory.atomic.wait32 i64.extend_i32_u", true),
        ("i32.const 16 i64.const 0 i64.const -1 memory.atomic.wait64 i64.extend_i32_u", true),
        ("atomic.fence i64.const 7", false),
    ] {
        compare(&native, &pulley, &module(body), trap);
    }
    let no_memory = wat::parse_str(
        "(module (func (export \"run\") (param i32 i64) (result i64) atomic.fence i64.const 1))",
    )
    .unwrap();
    compare(&native, &pulley, &no_memory, false);
}

#[test]
fn original_validation_rejects_bad_atomic_alignment_and_all_shared_memories() {
    let bad = module("i32.const 16 i32.atomic.load align=1 i64.extend_i32_u");
    assert!(prepare(&bad, features(), true).is_err());
    for wat in [
        "(module (memory 1 2 shared))",
        "(module (import \"host\" \"m\" (memory 1 2 shared)))",
        "(module (memory 1) (memory 1 2 shared))",
    ] {
        let wasm = wat::parse_str(wat).unwrap();
        for lower in [false, true] {
            assert!(prepare(&wasm, features(), lower).unwrap_err().contains("Shared"));
        }
    }
    for wat in ["(module (memory i64 1))", "(module (memory 1) (memory 1))"] {
        let wasm = wat::parse_str(wat).unwrap();
        for lower in [false, true] {
            assert!(prepare(&wasm, features(), lower).is_err());
        }
    }
    let bytes = wat::parse_str("(module (func nop))").unwrap();
    assert!(matches!(prepare(&bytes, features(), true).unwrap(), Cow::Borrowed(_)));
    let bytes = module("atomic.fence i64.const 0");
    assert!(matches!(prepare(&bytes, features(), false).unwrap(), Cow::Borrowed(_)));
    let mut restricted = features();
    restricted.set(WasmFeatures::SIMD, false);
    let bytes = module("v128.const i32x4 0 0 0 0 drop atomic.fence i64.const 0");
    assert!(
        prepare(&bytes, restricted, true).is_err(),
        "other engine features must remain enforced"
    );
}

#[test]
fn expansion_and_local_limits_fail_explicitly() {
    assert!(check_size(MAX_BYTES - 1, 1).is_ok());
    assert!(check_size(MAX_BYTES, 1).is_err());
    assert!(check_size(usize::MAX, 1).is_err());
    let mut m = Module::new();
    let mut types = wasm_encoder::TypeSection::new();
    types.ty().function([], []);
    m.section(&types);
    let mut functions = wasm_encoder::FunctionSection::new();
    functions.function(0);
    m.section(&functions);
    let mut code = CodeSection::new();
    let mut function = Function::new([(MAX_LOCALS, ValType::I32)]);
    function.instruction(&I::AtomicFence);
    function.instruction(&I::End);
    code.function(&function);
    m.section(&code);
    assert!(prepare(&m.finish(), features(), true).unwrap_err().contains("local limit"));
}

#[test]
fn lowered_atomic_loop_cannot_escape_fuel() {
    let bytes = module("(loop i32.const 16 i32.const 1 i32.atomic.rmw.add drop br 0) i64.const 0");
    let lowered = prepare(&bytes, features(), true).unwrap();
    let (result, _, fuel) = execute(&runtime(true), &lowered, 500);
    assert!(result.is_err());
    assert_eq!(fuel, 0);
}

#[test]
fn component_encoding_is_rejected_without_entering_core_types() {
    // Empty component binary, independent of wat's component feature selection.
    assert!(prepare(b"\0asm\x0d\0\x01\0", features(), true).unwrap_err().contains("core Wasm"));
}

#[test]
#[cfg(not(target_os = "ios"))]
fn imported_function_indices_and_existing_locals_are_preserved() {
    let bytes = wat::parse_str(
        r#"(module
        (import "host" "identity" (func $identity (param i32) (result i32)))
        (memory (export "memory") 1)
        (func $helper (param i32) (result i32) local.get 0 call $identity)
        (func (export "run") (param i32 i64) (result i64) (local i64)
            local.get 0 call $helper local.get 1 i64.atomic.store
            local.get 0 call $helper i64.atomic.load local.set 2 local.get 2))"#,
    )
    .unwrap();
    compare(&runtime(false), &runtime(true), &bytes, false);
}

#[test]
fn local_limit_includes_parameters_at_exact_boundary() {
    for (locals, accepted) in [(MAX_LOCALS - 2 - 7, true), (MAX_LOCALS - 2 - 6, false)] {
        let mut m = Module::new();
        let mut types = wasm_encoder::TypeSection::new();
        types.ty().function([ValType::I32, ValType::I64], []);
        m.section(&types);
        let mut functions = wasm_encoder::FunctionSection::new();
        functions.function(0);
        m.section(&functions);
        let mut code = CodeSection::new();
        let mut function = Function::new([(locals, ValType::I32)]);
        function.instruction(&I::AtomicFence);
        function.instruction(&I::End);
        code.function(&function);
        m.section(&code);
        let bytes = m.finish();
        for lower in [false, true] {
            let result = prepare(&bytes, features(), lower);
            assert_eq!(result.is_ok(), accepted);
            if !accepted {
                assert!(result.unwrap_err().contains("local limit"));
            }
        }
    }
}

#[test]
fn custom_pages_are_rejected_even_when_original_validation_enables_them() {
    let mut module = Module::new();
    let mut memories = wasm_encoder::MemorySection::new();
    memories.memory(wasm_encoder::MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: Some(0),
    });
    module.section(&memories);
    let bytes = module.finish();
    let mut enabled = features();
    enabled.set(WasmFeatures::CUSTOM_PAGE_SIZES, true);
    for lower in [false, true] {
        assert!(prepare(&bytes, enabled, lower).unwrap_err().contains("64 KiB"));
    }
}

#[test]
fn fences_keep_fuel_cost_even_without_memory_or_loops() {
    let body = "atomic.fence ".repeat(1000);
    let bytes = wat::parse_str(format!(
        "(module (func (export \"run\") (param i32 i64) (result i64) {body} i64.const 0))"
    ))
    .unwrap();
    let lowered = prepare(&bytes, features(), true).unwrap();
    let observe = |runtime: Runtime, wasm: &[u8], fuel| {
        let module = runtime.module(wasm).unwrap();
        let mut store = Store::new(&runtime.engine, ());
        store.set_fuel(fuel).unwrap();
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let run = instance.get_typed_func::<(i32, i64), i64>(&mut store, "run").unwrap();
        let first = run.call(&mut store, (0, 0)).is_ok();
        let remaining = store.get_fuel().unwrap();
        let second = run.call(&mut store, (0, 0)).is_ok();
        (first, remaining, second)
    };
    for fuel in [500u64, 2000] {
        let result = observe(runtime(true), &lowered, fuel);
        assert!(result.0);
        assert!(result.1 <= fuel.saturating_sub(1000), "fences must be charged");
        if fuel == 500 {
            assert_eq!(result.1, 0);
            assert!(!result.2, "exhausted fuel must trap on the next function entry");
        }
        #[cfg(not(target_os = "ios"))]
        assert_eq!(result, observe(runtime(false), &bytes, fuel));
    }
}
