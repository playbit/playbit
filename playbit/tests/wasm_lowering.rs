//! Independent C/Rust compiler conformance, run by build-wasm-probe.sh.
//! The observer checks imports only; it performs no Playbit operation or memory access.
#![cfg(feature = "wasm-runtime")]
use std::path::Path;
use wasmtime::{Caller, Config, Engine, ExternType, Instance, Linker, Module, Store, Val, ValType};

const IMPORTS: &str = include_str!("../platform/native/wasm-probe/imports.txt");
const LAYOUT: &str = include_str!("../platform/native/wasm-probe/layout-v2.tsv");

#[derive(Default)]
struct Observer {
    base: u32,
    calls: Vec<String>,
    sdk: bool,
}

fn arguments(specification: &str, base: u32) -> Vec<Val> {
    let (opcode, specification) = specification.split_once(' ').expect("opcode fixture");
    let opcode = u32::from_str_radix(&opcode[2..], 16).expect("opcode");
    let mut slots = Vec::new();
    if specification != "-" {
        for argument in specification.split(',') {
            let value = match argument {
                "G" => 0x7edc_ba98_89ab_cdef,
                "S" => continue,
                _ => {
                    let value = argument[1..].parse::<u32>().expect("fixture number");
                    match argument.as_bytes()[0] {
                        b'P' => u64::from(base + value),
                        b'I' => u64::from(0x8000_0000 + value),
                        b'L' => 0x8877_6655_4433_2200 + u64::from(value),
                        _ => panic!("unknown fixture argument"),
                    }
                }
            };
            slots.push(Val::I64(value as i64));
        }
    }
    slots.resize(6, Val::I64(0));
    slots.insert(0, Val::I32(opcode as i32));
    slots
}

fn assert_arguments(actual: &[Val], expected: &[Val], name: &str) {
    assert_eq!(actual.len(), expected.len(), "{name}");
    for (actual, expected) in actual.iter().zip(expected) {
        match (actual, expected) {
            (Val::I32(a), Val::I32(b)) => assert_eq!(a, b, "{name}"),
            (Val::I64(a), Val::I64(b)) => assert_eq!(a, b, "{name}"),
            _ => panic!("{name}: non-scalar or wrong-width argument"),
        }
    }
}

fn observe(module: &Module, engine: &Engine) -> (Store<Observer>, Instance) {
    let mut linker = Linker::<Observer>::new(engine);
    assert_eq!(module.imports().len(), 1);
    let mut names = Vec::new();
    for import in module.imports() {
        assert_eq!(import.module(), "playbit");
        let entry = import.name().to_owned();
        assert_eq!(entry, "pb_syscall");
        assert!(!names.contains(&entry), "duplicate import");
        names.push(entry.clone());
        let ExternType::Func(ty) = import.ty() else { panic!("non-function import") };
        assert_eq!(ty.params().len(), 7);
        assert!(matches!(ty.params().next(), Some(ValType::I32)));
        assert!(ty.params().skip(1).all(|ty| matches!(ty, ValType::I64)));
        assert!(matches!(ty.results().collect::<Vec<_>>().as_slice(), [ValType::I64]));
        linker
            .func_new(
                "playbit",
                import.name(),
                ty,
                move |mut caller: Caller<'_, Observer>, args, results| {
                    let opcode = u64::from(args[0].i32().expect("opcode") as u32);
                    let (name, specification) = IMPORTS
                        .lines()
                        .map(|line| line.split_once(' ').expect("fixture"))
                        .find(|(_, spec)| {
                            u64::from_str_radix(&spec.split_once(' ').expect("opcode").0[2..], 16)
                                .expect("opcode")
                                == opcode
                        })
                        .expect("known fixture operation");
                    if caller.data().sdk && name == "pb_event_poll" {
                        assert!(caller.data().calls.is_empty());
                        assert_ne!(args[1].i64(), Some(0));
                        assert_eq!(args[3].i64(), Some(0));
                        for argument in &args[4..] {
                            assert_eq!(argument.i64(), Some(0));
                        }
                        caller.data_mut().calls.push(name.to_owned());
                        results[0] = Val::I64(playbit_api::abi::syscall::Error::WouldBlock as i64);
                        return Ok(());
                    }
                    let expected = if caller.data().sdk {
                        assert_eq!(name, "pb_wake_after");
                        vec![
                            Val::I32(11),
                            Val::I64(1234),
                            Val::I64(0),
                            Val::I64(0),
                            Val::I64(0),
                            Val::I64(0),
                            Val::I64(0),
                        ]
                    } else {
                        arguments(specification, caller.data().base)
                    };
                    assert_eq!(entry, "pb_syscall");
                    assert_arguments(args, &expected, name);
                    assert!(caller.data().calls.len() < IMPORTS.lines().count());
                    caller.data_mut().calls.push(name.to_owned());
                    results[0] = Val::I64(if name == "pb_clock_read" {
                        0x8877_6655_4433_2201u64 as i64
                    } else if name.ends_with("_begin")
                        || matches!(
                            name,
                            "pb_file_open_take"
                                | "pb_file_list_open_take"
                                | "pb_channel_open"
                                | "pb_audio_open_take"
                                | "pb_sound_open_pcm"
                        )
                    {
                        0x7edc_ba98_89ab_cdef
                    } else {
                        -7
                    });
                    Ok(())
                },
            )
            .expect("link observer");
    }
    let mut store = Store::new(engine, Observer::default());
    let instance = linker.instantiate(&mut store, module).expect("instantiate compiler probe");
    (store, instance)
}

fn run_probe(engine: &Engine, language: &str) -> Vec<u32> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../target/wasm-lowering/{language}.wasm"));
    let module = Module::from_file(engine, path).expect("run build-wasm-probe.sh first");
    let (mut store, instance) = observe(&module, engine);
    let base = instance
        .get_typed_func::<(), u32>(&mut store, "probe_base")
        .expect("base")
        .call(&mut store, ())
        .expect("base result");
    assert_ne!(base, 0);
    assert_eq!(base % 8, 0);
    store.data_mut().base = base;
    let imports = instance.get_typed_func::<(), u32>(&mut store, "probe_imports").expect("imports");
    assert_eq!(imports.call(&mut store, ()).expect("imports result"), 0, "{language}");
    let expected: Vec<_> =
        IMPORTS.lines().map(|line| line.split_once(' ').expect("fixture").0).collect();
    assert_eq!(store.data().calls, expected, "{language}");
    let invalid = instance.get_typed_func::<(), u32>(&mut store, "probe_invalid").expect("invalid");
    assert_eq!(invalid.call(&mut store, ()).expect("invalid result"), 0);
    assert_eq!(store.data().calls, expected, "invalid calls must stay local");
    if language == "rust" {
        store.data_mut().sdk = true;
        store.data_mut().calls.clear();
        let sdk = instance.get_typed_func::<(), i32>(&mut store, "probe_sdk").expect("SDK");
        assert_eq!(sdk.call(&mut store, ()).expect("SDK result"), -7);
        assert_eq!(store.data().calls, ["pb_event_poll", "pb_wake_after"]);
    }
    let layout = instance.get_typed_func::<u32, u32>(&mut store, "probe_layout").expect("layout");
    let values = LAYOUT
        .lines()
        .enumerate()
        .map(|(index, _)| layout.call(&mut store, index as u32).expect("layout result"))
        .collect();
    assert_eq!(layout.call(&mut store, LAYOUT.lines().count() as u32).expect("end"), u32::MAX);
    values
}

#[test]
#[ignore = "requires independent wasm32 C/Rust compilers; use build-wasm-probe.sh"]
fn c_and_rust_wasm32_scalar_imports_and_records_agree() {
    let mut config = Config::new();
    config.target("pulley64").expect("portable test backend");
    config.signals_based_traps(false);
    let engine = Engine::new(&config).expect("engine");
    let c = run_probe(&engine, "c");
    let rust = run_probe(&engine, "rust");
    for ((line, c), rust) in LAYOUT.lines().zip(c).zip(rust) {
        let (name, expected) = line.split_once('\t').expect("layout fixture");
        let expected = expected.parse::<u32>().expect("layout number");
        assert_eq!(c, expected, "C wasm32 {name}");
        assert_eq!(rust, expected, "Rust wasm32 {name}");
    }
    println!(
        "{} operations through one scalar import and {} layout measurements agree in C and Rust",
        IMPORTS.lines().count(),
        LAYOUT.lines().count()
    );
}
