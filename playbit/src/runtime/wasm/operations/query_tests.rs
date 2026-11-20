#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::tests::{backends, context};
use super::*;
use crate::runtime::{
    TEST_FUEL,
    host::{DataCall, DataResult},
    syscall::{FieldChange, Host, Id, ObjectPage, ObjectRecord, Opcode, Value},
    wasm::{ExecutionBackend, Runtime},
};
use std::sync::{Arc, Mutex};
use wasmtime::{Instance, Store};
struct Results {
    page: ObjectPage,
    calls: usize,
    deny: bool,
    owned: Vec<Arc<DataCall>>,
    completions: std::collections::BTreeMap<u64, HostResult<Arc<DataResult>>>,
}
fn page() -> ObjectPage {
    ObjectPage {
        objects: vec![[9; 16], [10; 16]],
        next: Some([10; 16]),
        records: Some(vec![
            ObjectRecord {
                id: [9; 16],
                type_id: [8; 16],
                fields: vec![FieldChange {
                    field_id: [7; 16],
                    values: Some(vec![Value::Text("abc".into())]),
                }],
            },
            ObjectRecord {
                id: [10; 16],
                type_id: [8; 16],
                fields: vec![FieldChange {
                    field_id: [7; 16],
                    values: Some(vec![Value::Boolean(true)]),
                }],
            },
        ]),
    }
}
struct QueryHost(Arc<Mutex<Results>>);
impl Host for QueryHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::ObjectQueryBegin, Opcode::ObjectQueryTake]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        records.owned.push(call);
        Ok(records.owned.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        self.0
            .lock()
            .unwrap()
            .completions
            .get(&token)
            .cloned()
            .unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        self.0.lock().unwrap().completions.remove(&token);
    }
    fn query_objects(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        type_id: Id,
        _: Option<Id>,
        _: u32,
        _: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        assert_eq!((volume.volume_id, volume.entry_id, type_id), ([6; 16], [5; 15], [8; 16]));
        let mut results = self.0.lock().unwrap();
        results.calls += 1;
        if results.deny {
            return Err(HostError::AccessDenied);
        }
        Ok(results.page.clone())
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (memory (export "memory") 64 256)
    (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6 call $call))"#;
const QUERY_HANDLE: u32 = 3000000;
fn scalar(
    store: &mut Store<WasmState>,
    instance: &Instance,
    opcode: Opcode,
    mut args: [u64; 6],
) -> i64 {
    let handle_output = if opcode.capability().ends_with(".begin") {
        let index = playbit_api::abi::dispatch::layout(opcode).count as usize;
        let output = args[index];
        args[index] = 0;
        Some(output)
    } else {
        None
    };
    let result = instance
        .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut *store, "invoke")
        .unwrap()
        .call(&mut *store, (opcode as u32, args[0], args[1], args[2], args[3], args[4], args[5]))
        .unwrap();
    if let Some(output) = handle_output {
        let memory = instance.get_memory(&mut *store, "memory").unwrap();
        super::tests::save_handle(result, store, memory, output)
    } else {
        result
    }
}
fn begin_query(store: &mut Store<WasmState>, instance: &Instance, output: u32) -> Result<u64, i64> {
    let volume = store.data().app.volume_handle([6; 16]).unwrap();
    let _ = volume;
    let volume = 60000;
    let status =
        scalar(store, instance, Opcode::ObjectQueryBegin, [volume, 32, u64::from(output), 0, 0, 0]);
    if status < 0 {
        return Err(status);
    }
    assert_eq!(status, 0);
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    Ok(playbit_api::abi::dispatch::pack_handle(
        number(memory.data(&*store), output as usize + 4),
        number(memory.data(&*store), output as usize),
    ))
}
fn complete(host: &Arc<Mutex<Results>>) {
    let (token, owned) = {
        let records = host.lock().unwrap();
        (records.owned.len() as u64, records.owned.last().unwrap().as_ref().clone())
    };
    // Execute once between guest exports, without retaining a guest borrow.
    let result = owned.execute(&mut QueryHost(host.clone())).map(Arc::new);
    assert!(host.lock().unwrap().completions.insert(token, result).is_none());
}
fn take_query(store: &mut Store<WasmState>, instance: &Instance, pending: u64) -> i64 {
    scalar(store, instance, Opcode::ObjectQueryTake, [pending, 256, 0, 0, 0, 0])
}
fn query(store: &mut Store<WasmState>, instance: &Instance, host: &Arc<Mutex<Results>>) -> i64 {
    let pending = begin_query(store, instance, QUERY_HANDLE).unwrap();
    complete(host);
    take_query(store, instance, pending)
}

fn fixture(
    backend: ExecutionBackend,
    bytes: &[u8],
) -> (Store<WasmState>, Instance, Arc<Mutex<Results>>) {
    let runtime =
        Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
            .unwrap();
    let module = runtime.compile_app(bytes).unwrap();
    let results = Arc::new(Mutex::new(Results {
        page: page(),
        calls: 0,
        deny: false,
        owned: vec![],
        completions: Default::default(),
    }));
    let state = WasmState::new(context(), Box::new(QueryHost(results.clone())), runtime.fuel_limit)
        .unwrap();
    let mut store = Store::new(&runtime.engine, state);
    store.set_fuel(TEST_FUEL).unwrap();
    let mut linker = Linker::new(&runtime.engine);
    define(&mut linker).unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let handle = store.data().app.volume_handle([6; 16]).unwrap();
    super::tests::entry_pointers(&mut store, &instance, handle);
    (store, instance, results)
}
fn word(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn number(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn assert_same_bytes(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), expected.len());
    assert!(
        actual == expected,
        "first changed byte: {:?}",
        actual.iter().zip(expected).position(|(a, b)| a != b)
    );
}
fn buffer(data: &mut [u8], offset: usize, pointer: u32, capacity: u32) {
    word(data, offset, pointer);
    word(data, offset + 4, capacity);
    word(data, offset + 8, 0xcccccccc);
    let size = match offset {
        288 => 48,
        304 => 40,
        320 => 80,
        336 => return,
        _ => panic!("page buffer offset"),
    };
    word(data, offset + 12, size);
    for index in 0..capacity as usize {
        let start = pointer as usize + index * size as usize;
        word(data, start, size);
        word(data, start + 4, 0);
    }
}
fn inputs(data: &mut [u8]) {
    data[32..92].fill(0);
    word(data, 32, 60);
    data[40..56].fill(8);
    data[128..144].fill(7);
    word(data, 72, 2);
    word(data, 76, 2);
    word(data, 80, 128);
    word(data, 84, 1);
    data[256..348].fill(0xcc);
    word(data, 256, 92);
    word(data, 260, 0);
    buffer(data, 288, 512, 2);
    buffer(data, 304, 1024, 2);
    buffer(data, 320, 2048, 2);
    buffer(data, 336, 4096, 3);
}
#[test]
fn projected_page_short_takes_retain_one_snapshot_and_preserve_global_ranges() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        let pending = begin_query(&mut store, &instance, QUERY_HANDLE).unwrap();
        let before = memory.data(&store).to_vec();
        assert_eq!(take_query(&mut store, &instance, pending), -6);
        assert_same_bytes(memory.data(&store), &before);
        assert_eq!(host.lock().unwrap().calls, 0);
        complete(&host);
        for capacity in [292, 308, 324, 340] {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), capacity, 0);
            let mut expected = memory.data(&store).to_vec();
            for (offset, count) in [(296, 2), (312, 2), (328, 2), (344, 3)] {
                word(&mut expected, offset, count);
            }
            assert_eq!(take_query(&mut store, &instance, pending), -11);
            assert_same_bytes(memory.data(&store), &expected);
        }
        host.lock().unwrap().page.next = None;
        inputs(memory.data_mut(&mut store));
        assert_eq!(take_query(&mut store, &instance, pending), 0);
        let data = memory.data(&store);
        assert_eq!(&data[264..280], &[10; 16]);
        assert_eq!(number(data, 280), 1);
        assert_eq!(number(data, 284), 1);
        assert_eq!((number(data, 552), number(data, 600)), (0, 1));
        assert_eq!((number(data, 1048), number(data, 1088)), (0, 1));
        assert_eq!((number(data, 2056), number(data, 2136)), (1, 2));
        assert_eq!(&data[4096..4099], b"abc");
        assert_eq!(host.lock().unwrap().calls, 1);
        assert_eq!(take_query(&mut store, &instance, pending), -8);
        assert_eq!(query(&mut store, &instance, &host), 0);
        assert_eq!(number(memory.data(&store), 280), 0);
        host.lock().unwrap().page.records.as_mut().unwrap()[0].fields[0].values = None;
        host.lock().unwrap().page.records.as_mut().unwrap()[1].fields[0].values = Some(vec![]);
        assert_eq!(query(&mut store, &instance, &host), 0);
        assert_eq!((number(memory.data(&store), 1056), number(memory.data(&store), 1096)), (0, 1));
        assert_eq!((number(memory.data(&store), 328), number(memory.data(&store), 344)), (0, 0));
        assert!(host.lock().unwrap().completions.is_empty());
    }
}

#[test]
fn query_take_outputs_are_disjoint_and_owned_inputs_can_be_reused() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));

        let pending = begin_query(&mut store, &instance, QUERY_HANDLE).unwrap();
        complete(&host);
        for (descriptor, own) in [(288, 512), (304, 1024), (320, 2048), (336, 4096)] {
            for other in [256, 512, 1024, 2048, 4096] {
                if own == other {
                    continue;
                }
                inputs(memory.data_mut(&mut store));
                let alias = if other == 4096 { other } else { other + 8 };
                word(memory.data_mut(&mut store), descriptor, alias);
                if descriptor != 336 {
                    let size = match descriptor {
                        288 => 48,
                        304 => 40,
                        320 => 80,
                        _ => unreachable!(),
                    };
                    word(memory.data_mut(&mut store), descriptor + 4, 1);
                    word(memory.data_mut(&mut store), alias as usize, size);
                    word(memory.data_mut(&mut store), alias as usize + 4, 0);
                }
                let before = memory.data(&store).to_vec();
                assert_eq!(take_query(&mut store, &instance, pending), -3);
                assert_same_bytes(memory.data(&store), &before);
                assert_eq!(host.lock().unwrap().calls, 1);
                assert_eq!(host.lock().unwrap().completions.len(), 1);
            }
        }
        inputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 336, 32);
        assert_eq!(take_query(&mut store, &instance, pending), 0);
        assert_eq!(&memory.data(&store)[32..35], b"abc");
        assert!(host.lock().unwrap().completions.is_empty());
    }
}

#[test]
fn invalid_query_begin_admits_nothing_and_invalid_take_retains_completion() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for (offset, value, status) in [
            (56, 1, -3),
            (72, 4, -3),
            (72, 0, -3),
            (76, 0, -3),
            (76, 65, -9),
            (80, u32::MAX, -3),
            (84, 0, -3),
            (84, 257, -9),
            (88, 1, -3),
        ] {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), offset, value);
            let before = memory.data(&store).to_vec();
            assert_eq!(
                begin_query(&mut store, &instance, QUERY_HANDLE),
                Err(status),
                "offset {offset}"
            );
            assert_same_bytes(memory.data(&store), &before);
        }
        inputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 84, 2);
        memory.data_mut(&mut store)[144..160].fill(7);
        assert_eq!(begin_query(&mut store, &instance, QUERY_HANDLE), Err(-3));
        assert_eq!(host.lock().unwrap().calls, 0);
        assert!(host.lock().unwrap().owned.is_empty());
        inputs(memory.data_mut(&mut store));
        let pending = begin_query(&mut store, &instance, QUERY_HANDLE).unwrap();
        complete(&host);
        for (offset, value, status) in
            [(288, 513, -3), (292, 65, -9), (308, 16385, -9), (324, 262145, -9), (340, 1048577, -9)]
        {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), offset, value);
            let before = memory.data(&store).to_vec();
            assert_eq!(take_query(&mut store, &instance, pending), status);
            assert_same_bytes(memory.data(&store), &before);
            assert_eq!(host.lock().unwrap().calls, 1);
        }
        inputs(memory.data_mut(&mut store));
        assert_eq!(take_query(&mut store, &instance, pending), 0);
    }
}

#[test]
fn terminal_query_errors_leave_outputs_unchanged_and_consume_exactly_once() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        for invalid in 0..10 {
            let mut bad = page();
            match invalid {
                0 => bad.objects.swap(0, 1),
                1 => bad.objects[1] = bad.objects[0],
                2 => bad.next = Some([11; 16]),
                3 => bad.records = None,
                4 => {
                    bad.records.as_mut().unwrap().pop();
                }
                5 => bad.records.as_mut().unwrap()[0].id = [11; 16],
                6 => bad.records.as_mut().unwrap()[0].type_id = [11; 16],
                7 => bad.records.as_mut().unwrap()[0].fields[0].field_id = [11; 16],
                8 => {
                    bad.records.as_mut().unwrap()[0].fields[0].values =
                        Some(vec![Value::Float(f64::NAN)])
                }
                _ => host.lock().unwrap().deny = true,
            }
            host.lock().unwrap().page = bad;
            let pending = begin_query(&mut store, &instance, QUERY_HANDLE).unwrap();
            complete(&host);
            let before = memory.data(&store).to_vec();
            assert_eq!(
                take_query(&mut store, &instance, pending),
                if invalid == 9 { -2 } else { -4 }
            );
            assert_same_bytes(memory.data(&store), &before);
            assert_eq!(take_query(&mut store, &instance, pending), -8);
            assert!(host.lock().unwrap().completions.is_empty());
        }
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_, stale) = super::tests::entry_pointers(&mut store, &instance, volume);
        let stale = u64::from(stale);
        let before = memory.data(&store).to_vec();
        assert_eq!(
            scalar(
                &mut store,
                &instance,
                Opcode::ObjectQueryBegin,
                [stale, 32, u64::from(QUERY_HANDLE), 0, 0, 0]
            ),
            -8
        );
        assert_same_bytes(memory.data(&store), &before);
    }
}

#[test]
fn async_unprojected_empty_pages_and_cursor_checks_preserve_semantics() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 72, 0);
        word(memory.data_mut(&mut store), 84, 0);
        host.lock().unwrap().page.records = None;
        assert_eq!(query(&mut store, &instance, &host), 0);
        assert_eq!((number(memory.data(&store), 284), number(memory.data(&store), 312)), (0, 0));
        word(memory.data_mut(&mut store), 72, 1);
        memory.data_mut(&mut store)[56..72].fill(9);
        let pending = begin_query(&mut store, &instance, QUERY_HANDLE).unwrap();
        complete(&host);
        let mut before = memory.data(&store).to_vec();
        for offset in [260, 516, 564] {
            word(&mut before, offset, 0);
        }
        assert_eq!(take_query(&mut store, &instance, pending), -4);
        assert_same_bytes(memory.data(&store), &before);
        host.lock().unwrap().page = ObjectPage { objects: vec![], records: None, next: None };
        assert_eq!(query(&mut store, &instance, &host), 0);
        assert_eq!((number(memory.data(&store), 296), number(memory.data(&store), 280)), (0, 0));
    }
}

#[test]
#[ignore = "requires independently compiled C and Rust wasm32 consumers"]
fn compiler_query_records_use_separate_begin_and_take_exports() {
    for language in ["c", "rust"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/wasm-lowering/query-{language}.wasm"));
        compiled_query_guest(language, &std::fs::read(path).unwrap());
    }
}

#[test]
fn frozen_v1_rc2_query_guest_runs_on_current_runtime() {
    // Execute retained compiler output against today's runtime, without rebuilding it.
    for (language, bytes) in super::frozen_guest_tests::QUERY {
        compiled_query_guest(language, bytes);
    }
}

fn compiled_query_guest(language: &str, bytes: &[u8]) {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, bytes);
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let generation = u64::from(volume.generation);
        let begin =
            instance.get_typed_func::<(u64, u32), u64>(&mut store, "probe_query_begin").unwrap();
        let take =
            instance.get_typed_func::<(u64, u32), i32>(&mut store, "probe_query_take").unwrap();
        let pending = begin.call(&mut store, (generation, volume.slot)).unwrap();
        assert_ne!(pending, 0, "{language}/{backend:?}");
        assert_eq!(take.call(&mut store, (pending >> 32, pending as u32)).unwrap(), -6);
        assert_eq!(host.lock().unwrap().calls, 0);
        complete(&host);
        assert_eq!(take.call(&mut store, (pending >> 32, pending as u32)).unwrap(), 0);
        assert_eq!(take.call(&mut store, (pending >> 32, pending as u32)).unwrap(), -8);
        assert_eq!(host.lock().unwrap().calls, 1);
        assert!(host.lock().unwrap().completions.is_empty());
    }
}

#[test]
fn maximum_page_cardinality_and_payload_are_bounded() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.grow(&mut store, 192).unwrap();
        inputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 76, 64);
        word(memory.data_mut(&mut store), 80, 4096);
        word(memory.data_mut(&mut store), 84, 256);
        let ids: Vec<Id> = (0u32..256)
            .map(|index| {
                let mut id = [0; 16];
                id[..4].copy_from_slice(&index.to_be_bytes());
                id
            })
            .collect();
        for (index, id) in ids.iter().enumerate() {
            memory.write(&mut store, 4096 + index * 16, id).unwrap();
        }
        buffer(memory.data_mut(&mut store), 288, 16384, 64);
        buffer(memory.data_mut(&mut store), 304, 65536, 16384);
        buffer(
            memory.data_mut(&mut store),
            320,
            1048576,
            playbit_api::abi::syscall::MAX_PAGE_VALUES,
        );
        buffer(memory.data_mut(&mut store), 336, 0, 0);
        let objects: Vec<Id> = (1..=64).map(|index| [index; 16]).collect();
        let mut remaining = playbit_api::abi::syscall::MAX_PAGE_VALUES as usize;
        let records = objects
            .iter()
            .map(|id| ObjectRecord {
                id: *id,
                type_id: [8; 16],
                fields: ids
                    .iter()
                    .map(|id| {
                        let count = remaining.min(16);
                        remaining -= count;
                        FieldChange {
                            field_id: *id,
                            values: Some(vec![Value::Boolean(true); count]),
                        }
                    })
                    .collect(),
            })
            .collect();
        host.lock().unwrap().page = ObjectPage { objects, records: Some(records), next: None };
        assert_eq!(query(&mut store, &instance, &host), 0);
        assert_eq!(number(memory.data(&store), 296), 64);
        assert_eq!(number(memory.data(&store), 312), 16384);
        assert_eq!(remaining, 0);
        let count = playbit_api::abi::syscall::MAX_PAGE_VALUES as usize;
        assert_eq!(number(memory.data(&store), 328), count as u32);
        assert_eq!(number(memory.data(&store), 344), 0);
        assert_eq!(number(memory.data(&store), 1048576 + (count - 1) * 80 + 8), 2);
        assert_eq!(number(memory.data(&store), 65536 + 16383 * 40 + 24), count as u32);
        {
            let mut results = host.lock().unwrap();
            for record in results.page.records.as_mut().unwrap() {
                for field in &mut record.fields {
                    field.values = Some(vec![]);
                }
                record.fields[0].values = Some(vec![Value::Text("x".repeat(16384))]);
            }
        }
        buffer(memory.data_mut(&mut store), 320, 1048576, 64);
        buffer(memory.data_mut(&mut store), 336, 2097152, 1048576);
        assert_eq!(query(&mut store, &instance, &host), 0);
        assert_eq!(number(memory.data(&store), 328), 64);
        assert_eq!(number(memory.data(&store), 344), 1048576);
        assert!(memory.data(&store)[2097152..3145728].iter().all(|byte| *byte == b'x'));
        host.lock().unwrap().page.records.as_mut().unwrap()[63].fields[0].values =
            Some(vec![Value::Text("x".repeat(16385))]);
        let pending = begin_query(&mut store, &instance, QUERY_HANDLE).unwrap();
        complete(&host);
        let mut before = memory.data(&store).to_vec();
        word(&mut before, 260, 0);
        for (start, count, stride) in [(16384, 64, 48), (65536, 16384, 40), (1048576, 64, 80)] {
            for index in 0..count {
                word(&mut before, start + index * stride + 4, 0);
            }
        }
        assert_eq!(take_query(&mut store, &instance, pending), -9);
        assert_same_bytes(memory.data(&store), &before);
    }
}
