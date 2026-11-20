#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::tests::{backends, context};
use super::*;
use crate::runtime::{
    TEST_FUEL,
    syscall::{
        AuthenticatedFieldRead, FieldProvenance, FieldRevision, Host, Id, ObjectRecord, Opcode,
        OperationHandle, Principal, Value,
    },
    wasm::{ExecutionBackend, Runtime},
};
use std::sync::{Arc, Mutex};
use wasmtime::{Instance, Store};
fn record() -> AuthenticatedFieldRead {
    AuthenticatedFieldRead {
        volume_id: [6; 16],
        entry_id: [5; 15],
        object_id: [9; 16],
        type_id: [8; 16],
        field_id: [7; 16],
        revision: FieldRevision([13; 32]),
        values: Some(vec![Value::Text("abc".into())]),
        provenance: FieldProvenance::ApplicationEdit {
            principal: Principal::Human([11; 16]),
            device_id: [12; 16],
        },
    }
}
struct Results {
    calls: usize,
    deny: bool,
    record: Option<AuthenticatedFieldRead>,
    next: u64,
    ready: bool,
    snapshots: std::collections::BTreeMap<u64, HostResult<Arc<crate::runtime::host::DataResult>>>,
    released: Vec<u64>,
}
struct FieldHost(Arc<Mutex<Results>>);
impl Host for FieldHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::ObjectFieldReadAuthenticatedBegin, Opcode::ObjectFieldReadAuthenticatedTake]
    }
    fn begin_data(&mut self, _: u64, call: Arc<crate::runtime::host::DataCall>) -> HostResult<u64> {
        use crate::runtime::host::{DataOperation, DataResult};
        assert_eq!(call.volume, [6; 16]);
        assert_eq!(
            call.operation,
            DataOperation::AuthenticatedField {
                entry_id: [5; 15],
                object_id: [9; 16],
                field_id: [7; 16]
            }
        );
        let mut r = self.0.lock().unwrap();
        r.calls += 1;
        r.next += 1;
        let token = r.next;
        let snapshot = if r.deny {
            Err(HostError::AccessDenied)
        } else {
            Ok(Arc::new(DataResult::AuthenticatedField(r.record.clone())))
        };
        r.snapshots.insert(token, snapshot);
        Ok(token)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::DataResult>> {
        let r = self.0.lock().unwrap();
        if !r.ready {
            return Err(HostError::WouldBlock);
        }
        r.snapshots.get(&token).unwrap().clone()
    }
    fn release_data(&mut self, token: u64) {
        let mut r = self.0.lock().unwrap();
        assert!(r.snapshots.remove(&token).is_some());
        r.released.push(token);
    }
    fn read_object(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        _: Id,
        _: &[Id],
    ) -> HostResult<ObjectRecord> {
        panic!("authenticated reads must use one snapshot")
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $dispatch (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (memory (export "memory") 64 256)

    ;; Test guests retain a handle returned in the scalar result for later exports.
    (func $save_handle (param $output i32) (param $result i64) (result i32)
        (if (result i32) (i64.lt_s (local.get $result) (i64.const 0))
            (then (i32.wrap_i64 (local.get $result)))
            (else (i64.store (local.get $output) (local.get $result)) (i32.const 0))))
    (func (export "begin") (param i32 i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 3))) (call $dispatch (i32.const 310)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.extend_i32_u (local.get 2)) (i64.const 0)
            (i64.const 0) (i64.const 0))))
    (func (export "take") (param i64 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch (i32.const 311)
            (i64.or (i64.shl (local.get 0) (i64.const 32)) (i64.extend_i32_u (local.get 1)))
            (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))"#;
fn fixture(
    backend: ExecutionBackend,
    bytes: &[u8],
) -> (Store<WasmState>, Instance, Arc<Mutex<Results>>) {
    let runtime =
        Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
            .unwrap();
    let module = runtime.compile_app(bytes).unwrap();
    let results = Arc::new(Mutex::new(Results {
        calls: 0,
        deny: false,
        record: Some(record()),
        next: 0,
        ready: true,
        snapshots: Default::default(),
        released: vec![],
    }));
    let state = WasmState::new(context(), Box::new(FieldHost(results.clone())), runtime.fuel_limit)
        .unwrap();
    let mut store = Store::new(&runtime.engine, state);
    store.set_fuel(TEST_FUEL).unwrap();
    let mut linker = Linker::new(&runtime.engine);
    define(&mut linker).unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let volume = store.data().app.volume_handle([6; 16]).unwrap();
    super::tests::entry_pointers(&mut store, &instance, volume);
    (store, instance, results)
}
fn word(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn number(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn buffer(data: &mut [u8], offset: usize, pointer: u32, capacity: u32) {
    word(data, offset, pointer);
    word(data, offset + 4, capacity);
    word(data, offset + 8, 0xcccccccc);
    if offset == 268 {
        word(data, offset + 12, 80);
        if capacity <= 4096
            && pointer != 0
            && (pointer as usize + capacity as usize * 80) <= data.len()
        {
            for index in 0..capacity as usize {
                word(data, pointer as usize + index * 80, 80);
                word(data, pointer as usize + index * 80 + 4, 0);
            }
        }
    }
}
fn inputs(data: &mut [u8]) {
    data[32..48].fill(9);
    data[64..80].fill(7);
    data[256..296].fill(0xcc);
    word(data, 256, 40);
    word(data, 260, 0);
    word(data, 264, 2048);
    data[2048..2216].fill(0xcc);
    word(data, 2048, 168);
    word(data, 2052, 0);
    buffer(data, 268, 512, 1);
    buffer(data, 284, 1024, 3);
}
fn failed_output(data: &[u8]) -> Vec<u8> {
    let mut expected = data.to_vec();
    word(&mut expected, 260, 0);
    word(&mut expected, 2052, 0);
    let pointer = number(data, 268) as usize;
    let capacity = number(data, 272) as usize;
    let stride = number(data, 280) as usize;
    for index in 0..capacity {
        word(&mut expected, pointer + index * stride + 4, 0);
    }
    expected
}
fn begin(store: &mut Store<WasmState>, instance: &Instance) -> OperationHandle {
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    let before: [u8; 8] = memory.data(&*store)[128..136].try_into().unwrap();
    assert_eq!(
        instance
            .get_typed_func::<(u32, u32, u32, u32), i32>(&mut *store, "begin")
            .unwrap()
            .call(&mut *store, (60000, 32, 64, 128))
            .unwrap(),
        0
    );
    let data = memory.data(&*store);
    let handle = OperationHandle { generation: number(data, 132), slot: number(data, 128) };
    // The test driver owns this scratch region; restore it before comparing Take's output.
    memory.data_mut(store)[128..136].copy_from_slice(&before);
    handle
}
fn take(
    store: &mut Store<WasmState>,
    instance: &Instance,
    handle: OperationHandle,
    output: u32,
) -> i32 {
    instance
        .get_typed_func::<(u64, u32, u32), i32>(&mut *store, "take")
        .unwrap()
        .call(store, (u64::from(handle.generation), handle.slot, output))
        .unwrap()
}
fn read(store: &mut Store<WasmState>, instance: &Instance) -> i32 {
    let handle = begin(store, instance);
    take(store, instance, handle, 256)
}
#[test]
fn distinct_begins_obtain_fresh_snapshots_and_short_takes_publish_counts_only() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for capacity in [272, 288] {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), capacity, 0);
            let mut expected = memory.data(&store).to_vec();
            word(&mut expected, 276, 1);
            word(&mut expected, 292, 3);
            assert_eq!(read(&mut store, &instance), -11);
            assert_eq!(memory.data(&store), expected);
        }
        {
            let mut r = host.lock().unwrap();
            let field = r.record.as_mut().unwrap();
            field.revision = FieldRevision([14; 32]);
            field.values = Some(vec![Value::Text("new".into())]);
            field.provenance = FieldProvenance::Rekey {
                principal: Principal::Service([15; 16]),
                device_id: [16; 16],
            };
        }
        inputs(memory.data_mut(&mut store));
        assert_eq!(read(&mut store, &instance), 0);
        let data = memory.data(&store);
        assert_eq!(&data[2120..2152], &[14; 32]);
        assert_eq!(&data[2152..2168], &[15; 16]);
        assert_eq!(&data[2168..2184], &[16; 16]);
        assert_eq!(number(data, 2184), 2);
        assert_eq!(number(data, 2188), 2);
        assert_eq!(number(data, 2192), 2);
        assert_eq!(number(data, 2196), 0);
        assert_eq!(&data[1024..1027], b"new");
        assert_eq!(host.lock().unwrap().calls, 3);
    }
}
#[test]
fn metadata_preserves_provenance_and_absent_tombstone_empty_distinctions() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for kind in 0..3 {
            for human in [false, true] {
                for present in [false, true] {
                    let principal = if human {
                        Principal::Human([11; 16])
                    } else {
                        Principal::Service([11; 16])
                    };
                    let mut field = record();
                    field.values = present.then(Vec::new);
                    field.provenance = match kind {
                        0 => FieldProvenance::Unproven,
                        1 => FieldProvenance::ApplicationEdit { principal, device_id: [12; 16] },
                        _ => FieldProvenance::Rekey { principal, device_id: [12; 16] },
                    };
                    host.lock().unwrap().record = Some(field);
                    inputs(memory.data_mut(&mut store));
                    assert_eq!(read(&mut store, &instance), 0);
                    let data = memory.data(&store);
                    assert_eq!(&data[2056..2072], &[6; 16]);
                    assert_eq!(&data[2200..2215], &[5; 15]);
                    assert_eq!(data[2215], 0);
                    assert_eq!(&data[2072..2088], &[9; 16]);
                    assert_eq!(&data[2088..2104], &[8; 16]);
                    assert_eq!(&data[2104..2120], &[7; 16]);
                    assert_eq!(&data[2120..2152], &[13; 32]);
                    assert_eq!(number(data, 2184), if present { 2 } else { 1 });
                    assert_eq!(number(data, 2188), kind);
                    assert_eq!(
                        number(data, 2192),
                        if kind == 0 {
                            0
                        } else if human {
                            1
                        } else {
                            2
                        }
                    );
                    assert_eq!(&data[2152..2168], if kind == 0 { &[0; 16] } else { &[11; 16] });
                    assert_eq!(&data[2168..2184], if kind == 0 { &[0; 16] } else { &[12; 16] });
                    assert_eq!(number(data, 276), 0);
                    assert_eq!(number(data, 292), 0);
                }
            }
        }
        host.lock().unwrap().record = None;
        inputs(memory.data_mut(&mut store));
        assert_eq!(read(&mut store, &instance), 0);
        assert_eq!(&memory.data(&store)[2056..2216], &[0; 160]);
    }
}
#[test]
fn authenticated_begin_inputs_and_take_destinations_are_admitted_before_effects() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_entry, _stale_entry) = super::tests::entry_pointers(&mut store, &instance, volume);
        let call =
            instance.get_typed_func::<(u32, u32, u32, u32), i32>(&mut store, "begin").unwrap();
        for (object, field, output) in [(u32::MAX, 64, 128), (32, u32::MAX, 128)] {
            let before = memory.data(&store).to_vec();
            assert_eq!(call.call(&mut store, (_entry, object, field, output)).unwrap(), -3);
            assert_eq!(memory.data(&store), before);
        }
        for id in [32, 64] {
            inputs(memory.data_mut(&mut store));
            memory.data_mut(&mut store)[id..id + 16].fill(0);
            let before = memory.data(&store).to_vec();
            assert_eq!(call.call(&mut store, (60000, 32, 64, 128)).unwrap(), -3);
            assert_eq!(memory.data(&store), before);
        }
        inputs(memory.data_mut(&mut store));
        assert_eq!(
            call.call(&mut store, (60032, 32, 64, 128)).unwrap(),
            HostError::StaleHandle as i32
        );
        assert_eq!(host.lock().unwrap().calls, 0);
        let operation = begin(&mut store, &instance);
        for (descriptor, own) in [(268, 512), (284, 1024)] {
            for other in [256, 512, 1024] {
                if own == other {
                    continue;
                }
                inputs(memory.data_mut(&mut store));
                word(memory.data_mut(&mut store), descriptor, other);
                let before = memory.data(&store).to_vec();
                assert!(matches!(take(&mut store, &instance, operation, 256), -3 | -11));
                assert_eq!(memory.data(&store), before);
            }
        }
        for (offset, value, status) in [(268, 513, -3), (272, 4097, -9), (288, 1048577, -9)] {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), offset, value);
            let before = memory.data(&store).to_vec();
            assert_eq!(take(&mut store, &instance, operation, 256), status);
            assert_eq!(memory.data(&store), before);
        }
        for output in [0, 257, u32::MAX] {
            let before = memory.data(&store).to_vec();
            assert_eq!(take(&mut store, &instance, operation, output), -3);
            assert_eq!(memory.data(&store), before);
        }
        assert_eq!(
            take(
                &mut store,
                &instance,
                OperationHandle { generation: operation.generation + 1, ..operation },
                256
            ),
            HostError::StaleHandle as i32
        );
        inputs(memory.data_mut(&mut store));
        assert_eq!(take(&mut store, &instance, operation, 256), 0);
        assert_eq!(host.lock().unwrap().calls, 1);
        assert_eq!(host.lock().unwrap().released, [1]);
    }
}
#[test]
fn delayed_completion_and_short_retry_retain_one_snapshot_and_recheck_delivery_grant() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        host.lock().unwrap().ready = false;
        let operation = begin(&mut store, &instance);
        let before = memory.data(&store).to_vec();
        assert_eq!(take(&mut store, &instance, operation, 256), -6);
        assert_eq!(memory.data(&store), before);
        {
            let mut r = host.lock().unwrap();
            r.ready = true;
            r.record.as_mut().unwrap().revision = FieldRevision([99; 32]);
            r.record.as_mut().unwrap().values = Some(vec![Value::Text("new".into())]);
        }
        for capacity in [272, 288] {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), capacity, 0);
            let mut expected = memory.data(&store).to_vec();
            word(&mut expected, 276, 1);
            word(&mut expected, 292, 3);
            assert_eq!(take(&mut store, &instance, operation, 256), -11);
            assert_eq!(memory.data(&store), expected);
        }
        inputs(memory.data_mut(&mut store));
        assert_eq!(take(&mut store, &instance, operation, 256), 0);
        assert_eq!(&memory.data(&store)[2120..2152], &[13; 32]);
        assert_eq!(&memory.data(&store)[1024..1027], b"abc");
        assert_eq!(take(&mut store, &instance, operation, 256), HostError::StaleHandle as i32);
        let next = begin(&mut store, &instance);
        let before = failed_output(memory.data(&store));
        super::tests::set_rights(&mut store, crate::runtime::syscall::VolumeRights(0));
        assert_eq!(take(&mut store, &instance, next, 256), -2);
        assert_eq!(memory.data(&store), before);
        assert_eq!(take(&mut store, &instance, next, 256), HostError::StaleHandle as i32);
        assert_eq!(host.lock().unwrap().calls, 2);
        assert_eq!(host.lock().unwrap().released, [1, 2]);
    }
}
#[test]
fn denied_and_malformed_snapshots_expose_neither_metadata_nor_values() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        let before = memory.data(&store).to_vec();
        for invalid in 0..7 {
            let mut field = record();
            match invalid {
                0 => field.volume_id = [99; 16],
                1 => field.object_id = [99; 16],
                2 => field.field_id = [99; 16],
                3 => field.type_id = [0; 16],
                4 => {
                    field.provenance = FieldProvenance::ApplicationEdit {
                        principal: Principal::Human([0; 16]),
                        device_id: [12; 16],
                    }
                }
                5 => {
                    field.provenance = FieldProvenance::Rekey {
                        principal: Principal::Service([11; 16]),
                        device_id: [0; 16],
                    }
                }
                _ => field.values = Some(vec![Value::Float(f64::NAN)]),
            };
            host.lock().unwrap().record = Some(field);
            assert_eq!(read(&mut store, &instance), HostError::Internal as i32);
            assert_eq!(memory.data(&store), before);
        }
        host.lock().unwrap().deny = true;
        assert_eq!(read(&mut store, &instance), -2);
        assert_eq!(memory.data(&store), before);
        word(memory.data_mut(&mut store), 272, 0);
        word(memory.data_mut(&mut store), 288, 0);
        let before = memory.data(&store).to_vec();
        assert_eq!(read(&mut store, &instance), -2);
        assert_eq!(memory.data(&store), before);
    }
}
#[test]
fn authenticated_value_and_payload_limits_preserve_atomic_publication() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        buffer(memory.data_mut(&mut store), 268, 65536, 4096);
        buffer(memory.data_mut(&mut store), 284, 1048576, 1048576);
        host.lock().unwrap().record.as_mut().unwrap().values =
            Some(vec![Value::Boolean(true); 4096]);
        assert_eq!(read(&mut store, &instance), 0);
        assert_eq!(number(memory.data(&store), 276), 4096);
        host.lock().unwrap().record.as_mut().unwrap().values =
            Some(vec![Value::Bytes(vec![0x55; 1048576])]);
        assert_eq!(read(&mut store, &instance), 0);
        assert_eq!(number(memory.data(&store), 292), 1048576);
        assert!(memory.data(&store)[1048576..2097152].iter().all(|b| *b == 0x55));
        let before = failed_output(memory.data(&store));
        for values in [vec![Value::Boolean(true); 4097], vec![Value::Bytes(vec![0x55; 1048577])]] {
            host.lock().unwrap().record.as_mut().unwrap().values = Some(values);
            assert_eq!(read(&mut store, &instance), -9);
            assert_eq!(memory.data(&store), before);
        }
    }
}
#[test]
#[ignore = "requires independently compiled C and Rust wasm32 consumers"]
fn compiler_authenticated_records_reach_named_imports() {
    for language in ["c", "rust"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/wasm-lowering/authenticated-{language}.wasm"));
        compiled_authenticated_guest(language, &std::fs::read(path).unwrap());
    }
}

#[test]
fn frozen_v1_rc2_authenticated_guest_runs_on_current_runtime() {
    // Execute retained compiler output against today's runtime, without rebuilding it.
    for (language, bytes) in super::frozen_guest_tests::AUTHENTICATED {
        compiled_authenticated_guest(language, bytes);
    }
}

fn compiled_authenticated_guest(language: &str, bytes: &[u8]) {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, bytes);
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let begin = instance
            .get_typed_func::<(u64, u32), u64>(&mut store, "probe_authenticated_begin")
            .unwrap();
        let operation =
            begin.call(&mut store, (u64::from(volume.generation), volume.slot)).unwrap();
        assert_ne!(operation, 0);
        let call =
            instance.get_typed_func::<(u64, u32), u32>(&mut store, "probe_authenticated").unwrap();
        assert_eq!(
            call.call(&mut store, (operation >> 32, operation as u32)).unwrap(),
            0,
            "{backend:?} {language}"
        );
        assert_eq!(host.lock().unwrap().calls, 1);
    }
}

#[test]
fn missing_authenticated_capability_is_explicit_before_guest_memory_access() {
    struct Missing;
    impl Host for Missing {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
    }
    for &backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let mut required = context();
        required
            .required_capabilities
            .insert(Opcode::ObjectFieldReadAuthenticatedBegin.capability().into());
        assert!(WasmState::new(required, Box::new(Missing), runtime.fuel_limit).is_err());
        let module = runtime.compile_app(&wat::parse_str(MODULE).unwrap()).unwrap();
        let mut store = Store::new(
            &runtime.engine,
            WasmState::new(context(), Box::new(Missing), runtime.fuel_limit).unwrap(),
        );
        store.set_fuel(TEST_FUEL).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        super::tests::entry_pointers(&mut store, &instance, volume);
        let call =
            instance.get_typed_func::<(u32, u32, u32, u32), i32>(&mut store, "begin").unwrap();
        assert_eq!(call.call(&mut store, (60000, 0, 0, 0)).unwrap(), HostError::Unsupported as i32);
        assert_eq!(
            take(
                &mut store,
                &instance,
                OperationHandle { generation: volume.generation, slot: 1 },
                0
            ),
            HostError::Unsupported as i32
        );
    }
}
