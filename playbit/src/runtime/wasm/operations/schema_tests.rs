#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::tests::{backends, context};
use super::*;
use crate::runtime::linear_memory::layout::{
    operations::event,
    schema::{field, field_input, schema, type_input, type_record},
};
use crate::runtime::{
    TEST_FUEL,
    host::{DataCall, DataResult},
    syscall::{EventKind, FieldDeclaration, Host, Opcode, Schema, TypeDeclaration, ValueType},
    wasm::{ExecutionBackend, Runtime},
};
use std::sync::{Arc, Mutex};
use wasmtime::{Instance, Store};
struct Records {
    schema: Schema,
    reads: usize,
    writes: usize,
    deny: bool,
    calls: Vec<Arc<DataCall>>,
    results: std::collections::BTreeMap<u64, HostResult<Arc<DataResult>>>,
    released: Vec<u64>,
}
impl Default for Records {
    fn default() -> Self {
        Self {
            schema: Schema { types: vec![] },
            reads: 0,
            writes: 0,
            deny: false,
            calls: vec![],
            results: Default::default(),
            released: vec![],
        }
    }
}
struct SchemaHost(Arc<Mutex<Records>>);
impl Host for SchemaHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::SchemaReadBegin,
            Opcode::SchemaReadTake,
            Opcode::SchemaEstablishBegin,
            Opcode::SchemaEstablishTake,
        ]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        records.calls.push(call);
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        let records = self.0.lock().unwrap();
        records.results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        let mut records = self.0.lock().unwrap();
        records.results.remove(&token);
        records.released.push(token);
    }
    fn schema(&mut self, volume: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
        assert_eq!(volume.volume_id, [6; 16]);
        let mut records = self.0.lock().unwrap();
        records.reads += 1;
        if records.deny {
            return Err(HostError::AccessDenied);
        }
        Ok(records.schema.clone())
    }
    fn establish_schema(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        schema: &Schema,
    ) -> HostResult<()> {
        assert_eq!(volume.volume_id, [6; 16]);
        let mut records = self.0.lock().unwrap();
        if records.deny {
            return Err(HostError::AccessDenied);
        }
        records.writes += 1;
        records.schema = schema.clone();
        Ok(())
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (memory (export "memory") 64 256)
    (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6 call $call))"#;
const HANDLE: u64 = 3500000; // Disjoint from the maximum nested schema input.
type Invoke = wasmtime::TypedFunc<(u32, u64, u64, u64, u64, u64, u64), i64>;
fn invoke(store: &mut Store<WasmState>, instance: &Instance) -> Invoke {
    instance.get_typed_func(store, "invoke").unwrap()
}
fn call(
    store: &mut Store<WasmState>,
    instance: &Instance,
    invoke: &Invoke,
    opcode: Opcode,
    args: [u64; 4],
) -> i64 {
    let [a, b, c, d] = args;
    let (d, e) = if opcode == Opcode::SchemaEstablishBegin {
        (u64::from(type_input::SIZEOF), d)
    } else {
        (d, 0)
    };
    let (b, e, output) = match opcode {
        Opcode::SchemaReadBegin => (0, 0, Some(b)),
        Opcode::SchemaEstablishBegin => (b, 0, Some(e)),
        _ => (b, e, None),
    };
    let result = invoke.call(&mut *store, (opcode as u32, a, b, c, d, e, 0)).unwrap();
    if let Some(output) = output {
        if result > 0 {
            // The helper's guest fixture exports its memory through WasmState.
            let memory = instance.get_memory(&mut *store, "memory").unwrap();
            super::tests::save_handle(result, store, memory, output)
        } else {
            result
        }
    } else {
        result
    }
}
fn volume(store: &Store<WasmState>) -> u64 {
    store.data().app.volume_handle([6; 16]).unwrap();
    2097184
}
fn operation(store: &mut Store<WasmState>, instance: &Instance) -> u64 {
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    playbit_api::abi::dispatch::pack_handle(
        number(memory.data(&*store), HANDLE as usize + 4),
        number(memory.data(&*store), HANDLE as usize),
    )
}
// Only the external driver executes a backend, between guest exports. Begin
// records owned input and Take only accesses the retained result.
fn complete(records: &Arc<Mutex<Records>>) {
    let (token, owned) = {
        let records = records.lock().unwrap();
        (records.calls.len() as u64, records.calls.last().unwrap().as_ref().clone())
    };
    let result = owned.execute(&mut SchemaHost(records.clone())).map(Arc::new);
    assert!(
        records.lock().unwrap().results.insert(token, result).is_none(),
        "never repeat an accepted execution"
    );
}
fn begin_read(store: &mut Store<WasmState>, instance: &Instance, invoke: &Invoke) -> u64 {
    let volume = volume(store);
    assert_eq!(call(store, instance, invoke, Opcode::SchemaReadBegin, [volume, HANDLE, 0, 0]), 0);
    operation(store, instance)
}
fn fixture(
    backend: ExecutionBackend,
    bytes: &[u8],
) -> (Store<WasmState>, Instance, Arc<Mutex<Records>>) {
    let runtime =
        Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
            .unwrap();
    let module = runtime.compile_app(bytes).unwrap();
    let records =
        Arc::new(Mutex::new(Records { schema: Schema { types: vec![] }, ..Default::default() }));
    let state =
        WasmState::new(context(), Box::new(SchemaHost(records.clone())), runtime.fuel_limit)
            .unwrap();
    let mut store = Store::new(&runtime.engine, state);
    store.set_fuel(TEST_FUEL).unwrap();
    let mut linker = Linker::new(&runtime.engine);
    define(&mut linker).unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let handle = store.data().app.volume_handle([6; 16]).unwrap();
    super::tests::entry_pointers_at(&mut store, &instance, handle, 2097184);
    (store, instance, records)
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
}
fn record_buffer(data: &mut [u8], offset: usize, pointer: u32, capacity: u32, size: u32) {
    buffer(data, offset, pointer, capacity);
    word(data, offset + 12, size);
    for index in 0..capacity as usize {
        word(data, pointer as usize + index * size as usize, size);
        word(data, pointer as usize + index * size as usize + 4, 0);
    }
}
fn inputs(data: &mut [u8]) {
    data[128..128 + type_input::SIZEOF as usize].fill(0);
    data[256..256 + field_input::SIZEOF as usize].fill(0);
    word(data, 128, type_input::SIZEOF);
    word(data, 256, field_input::SIZEOF);
    data[128 + type_input::ID..128 + type_input::ID + 16].fill(8);
    data[256 + field_input::ID..256 + field_input::ID + 16].fill(7);
    word(data, 128 + type_input::NAME_DATA, 512);
    word(data, 128 + type_input::NAME_LENGTH, 4);
    word(data, 128 + type_input::FIELDS, 256);
    word(data, 128 + type_input::COUNT, 1);
    word(data, 128 + type_input::STRIDE, field_input::SIZEOF);
    word(data, 128 + type_input::FLAGS, 1);
    word(data, 256 + field_input::NAME_DATA, 516);
    word(data, 256 + field_input::NAME_LENGTH, 4);
    word(data, 256 + field_input::KIND, 2);
    word(data, 256 + field_input::FLAGS, 3);
    data[512..520].copy_from_slice(b"Itemdone");
}
fn outputs(data: &mut [u8]) {
    word(data, 1024, schema::SIZEOF);
    word(data, 1028, 0);
    record_buffer(data, 1024 + schema::TYPES_DATA, 2048, 1, type_record::SIZEOF);
    record_buffer(data, 1024 + schema::FIELDS_DATA, 4096, 1, field::SIZEOF);
    buffer(data, 1024 + schema::BYTES_DATA, 8192, 8);
}

#[test]
fn async_schema_owns_inputs_and_short_take_retains_a_snapshot() {
    const ASYNC: &str = r#"(module
        (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (memory (export "memory") 1)
        (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
            local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6 call $call))"#;
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(ASYNC).unwrap());
        let volume = 2097184;
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let invoke = instance
            .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut store, "invoke")
            .unwrap();
        inputs(memory.data_mut(&mut store));

        assert_eq!(
            super::tests::save_handle(
                invoke
                    .call(&mut store, (0x104, volume, 128, 1, u64::from(type_input::SIZEOF), 0, 0))
                    .unwrap(),
                &mut store,
                memory,
                640
            ),
            0
        );
        memory.data_mut(&mut store)[512..520].fill(b'x');
        let pending = playbit_api::abi::dispatch::pack_handle(
            number(memory.data(&store), 644),
            number(memory.data(&store), 640),
        );
        {
            let mut retained = records.lock().unwrap();
            let crate::runtime::host::DataOperation::SchemaEstablish { schema, .. } =
                &retained.calls[0].operation
            else {
                panic!("schema input")
            };
            assert_eq!(schema.types[0].name, "Item");
            retained.results.insert(1, Ok(Arc::new(DataResult::SchemaEstablished)));
        }
        assert_eq!(invoke.call(&mut store, (0x105, pending, 0, 0, 0, 0, 0)).unwrap(), 0);
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x102, volume, 0, 0, 0, 0, 0)).unwrap(),
                &mut store,
                memory,
                640
            ),
            0
        );
        let pending = playbit_api::abi::dispatch::pack_handle(
            number(memory.data(&store), 644),
            number(memory.data(&store), 640),
        );
        records.lock().unwrap().results.insert(
            2,
            Ok(Arc::new(DataResult::Schema(Schema {
                types: vec![TypeDeclaration {
                    id: [8; 16],
                    name: "Snapshot".into(),
                    deprecated: false,
                    fields: vec![],
                }],
            }))),
        );
        outputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 1024 + schema::BYTES_CAPACITY, 0);
        assert_eq!(
            invoke.call(&mut store, (0x103, pending, 1024, 0, 0, 0, 0)).unwrap(),
            HostError::BufferTooSmall as i64
        );
        assert_eq!(number(memory.data(&store), 1024 + schema::BYTES_COUNT), 8);
        assert_eq!(records.lock().unwrap().released, vec![1]);
        word(memory.data_mut(&mut store), 1024 + schema::BYTES_CAPACITY, 8);
        assert_eq!(invoke.call(&mut store, (0x103, pending, 1024, 0, 0, 0, 0)).unwrap(), 0);
        assert_eq!(&memory.data(&store)[8192..8200], b"Snapshot");
        assert_eq!(records.lock().unwrap().released, vec![1, 2]);
        assert_eq!(records.lock().unwrap().calls.len(), 2);
        assert_eq!(
            invoke.call(&mut store, (0x103, pending, 1024, 0, 0, 0, 0)).unwrap(),
            HostError::StaleHandle as i64
        );
    }
}
#[test]
fn async_schema_roundtrip_all_kinds_pending_and_retained_short_output() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let volume = volume(&store);
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let header = 1408;
        let payload = header + event::SIZEOF as usize;
        let poll = [header as u64, payload as u64, 8, 0];
        word(memory.data_mut(&mut store), header, event::SIZEOF);
        assert_eq!(call(&mut store, &instance, &invoke, Opcode::EventPoll, poll), 0);
        assert_eq!(number(memory.data(&store), header + event::KIND), EventKind::Started as u32);
        for kind in 1..=7 {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), 256 + field_input::KIND, kind);
            if kind == 6 {
                memory.data_mut(&mut store)
                    [256 + field_input::REFERENCE_TYPE..256 + field_input::REFERENCE_TYPE + 16]
                    .fill(8);
            }
            assert_eq!(
                call(
                    &mut store,
                    &instance,
                    &invoke,
                    Opcode::SchemaEstablishBegin,
                    [volume, 128, 1, HANDLE]
                ),
                0
            );
            let pending = operation(&mut store, &instance);
            assert_eq!(
                call(
                    &mut store,
                    &instance,
                    &invoke,
                    Opcode::SchemaEstablishTake,
                    [pending, 0, 0, 0]
                ),
                -6
            );
            assert_eq!(records.lock().unwrap().writes, kind as usize - 1);
            complete(&records);
            assert_eq!(
                call(
                    &mut store,
                    &instance,
                    &invoke,
                    Opcode::SchemaEstablishTake,
                    [pending, 0, 0, 0]
                ),
                0
            );
            let pending = begin_read(&mut store, &instance, &invoke);
            outputs(memory.data_mut(&mut store));
            let before = memory.data(&store).to_vec();
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                -6
            );
            assert_eq!(
                memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                None,
                "unexpected memory change"
            );
            assert_eq!(records.lock().unwrap().reads, kind as usize - 1);
            complete(&records);
            let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(pending);
            let token = records.lock().unwrap().calls.len() as u64;
            let epoch = store.data().app.generation();
            store.data_mut().app.notify_data(epoch, token).unwrap();
            assert_eq!(call(&mut store, &instance, &invoke, Opcode::EventPoll, poll), 8);
            assert_eq!(
                number(memory.data(&store), header + event::KIND),
                EventKind::DataReady as u32
            );
            assert_eq!(number(memory.data(&store), payload), slot);
            assert_eq!(number(memory.data(&store), payload + 4), generation);
            word(memory.data_mut(&mut store), 1024 + schema::BYTES_CAPACITY, 7);
            let mut expected = memory.data(&store).to_vec();
            for (offset, count) in [
                (1024 + schema::TYPES_COUNT, 1),
                (1024 + schema::FIELDS_COUNT, 1),
                (1024 + schema::BYTES_COUNT, 8),
            ] {
                word(&mut expected, offset, count);
            }
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                -11
            );
            assert_eq!(memory.data(&store), expected);
            word(memory.data_mut(&mut store), 1024 + schema::BYTES_CAPACITY, 8);
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                0
            );
            let data = memory.data(&store);
            assert_eq!(&data[8192..8200], b"Itemdone");
            assert_eq!(
                (
                    number(data, 2048 + type_record::FLAGS),
                    number(data, 2048 + type_record::RESERVED)
                ),
                (1, 0)
            );
            assert_eq!(
                (number(data, 4096 + field::KIND), number(data, 4096 + field::FLAGS)),
                (kind, 3)
            );
            assert_eq!(
                &data[4096 + field::REFERENCE_TYPE..4096 + field::REFERENCE_TYPE + 16],
                if kind == 6 { &[8; 16] } else { &[0; 16] }
            );
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                -8
            );
        }
        let records = records.lock().unwrap();
        assert_eq!(
            (records.writes, records.reads, records.calls.len(), records.released.len()),
            (7, 7, 14, 14)
        );
    }
}
#[test]
fn malformed_async_schema_inputs_fail_before_admission_or_host_effects() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let volume = volume(&store);
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for (offset, value, status) in [
            (128 + type_input::NAME_DATA, 0, -3),
            (128 + type_input::NAME_LENGTH, 256, -3),
            (128, type_input::SIZEOF - 1, -3),
            (128 + type_input::FIELDS, 257, -3),
            (128 + type_input::COUNT, 257, -9),
            (128 + type_input::FLAGS, 2, -3),
            (256 + field_input::NAME_DATA, u32::MAX, -3),
            (256 + field_input::NAME_LENGTH, 256, -3),
            (256, field_input::SIZEOF - 1, -3),
            (256 + field_input::KIND, 99, -3),
            (256 + field_input::FLAGS, 4, -3),
            (256 + field_input::REFERENCE_TYPE, 1, -3),
        ] {
            inputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), offset, value);
            let before = memory.data(&store).to_vec();
            assert_eq!(
                call(
                    &mut store,
                    &instance,
                    &invoke,
                    Opcode::SchemaEstablishBegin,
                    [volume, 128, 1, HANDLE]
                ),
                status,
                "offset {offset}"
            );
            assert_eq!(
                memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                None,
                "unexpected memory change"
            );
        }
        for invalid in [b' ', b'\n', 0xff] {
            inputs(memory.data_mut(&mut store));
            memory.data_mut(&mut store)[512..516].fill(invalid);
            assert_eq!(
                call(
                    &mut store,
                    &instance,
                    &invoke,
                    Opcode::SchemaEstablishBegin,
                    [volume, 128, 1, HANDLE]
                ),
                -3
            );
        }
        inputs(memory.data_mut(&mut store));
        memory.data_mut(&mut store)[256..256 + field_input::NAME_DATA].fill(8);
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 1, HANDLE]
            ),
            -3
        );
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 257, HANDLE]
            ),
            -9
        );
        assert!(records.lock().unwrap().calls.is_empty());
        assert_eq!(records.lock().unwrap().writes, 0);
    }
}
#[test]
fn async_schema_output_admission_retains_completion_and_terminal_errors_publish_nothing() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let pending = begin_read(&mut store, &instance, &invoke);
        complete(&records);
        for (offset, value, status) in [
            (1024 + schema::TYPES_DATA, 1024, -3),
            (1024 + schema::FIELDS_DATA, 2048, -11),
            (1024 + schema::BYTES_DATA, 4096, -3),
            (1024 + schema::TYPES_DATA, 2049, -3),
            (1024 + schema::TYPES_CAPACITY, 257, -9),
            (1024 + schema::FIELDS_CAPACITY, 4097, -9),
            (1024 + schema::BYTES_CAPACITY, 1048577, -9),
        ] {
            outputs(memory.data_mut(&mut store));
            word(memory.data_mut(&mut store), offset, value);
            let mut before = memory.data(&store).to_vec();
            word(&mut before, 1024 + schema::WRITTEN, 0);
            word(&mut before, 2048 + type_record::WRITTEN, 0);
            word(&mut before, 4096 + field::WRITTEN, 0);
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                status
            );
            assert_eq!(
                memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                None,
                "unexpected memory change"
            );
            assert!(records.lock().unwrap().released.is_empty());
        }
        outputs(memory.data_mut(&mut store));
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
            0
        );
        for malformed in [false, true] {
            records.lock().unwrap().deny = !malformed;
            if malformed {
                records.lock().unwrap().schema.types.push(TypeDeclaration {
                    id: [8; 16],
                    name: "".into(),
                    deprecated: false,
                    fields: vec![],
                });
            }
            let pending = begin_read(&mut store, &instance, &invoke);
            complete(&records);
            let mut before = memory.data(&store).to_vec();
            word(&mut before, 1024 + schema::WRITTEN, 0);
            word(&mut before, 2048 + type_record::WRITTEN, 0);
            word(&mut before, 4096 + field::WRITTEN, 0);
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                if malformed { -4 } else { -2 }
            );
            assert_eq!(
                memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                None,
                "unexpected memory change"
            );
            assert_eq!(
                call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
                -8
            );
        }
        let volume = volume(&store);
        let before = memory.data(&store).to_vec();
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaReadBegin,
                [volume + 32, HANDLE, 0, 0]
            ),
            -8
        );
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
    }
}
#[test]
fn async_schema_current_read_grants_and_known_establish_receipts_are_distinct() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let volume = volume(&store);
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        inputs(memory.data_mut(&mut store));
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 1, HANDLE]
            ),
            0
        );
        let pending = operation(&mut store, &instance);
        complete(&records);
        super::tests::set_rights(&mut store, crate::runtime::syscall::VolumeRights::READ);
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaEstablishTake, [pending, 0, 0, 0]),
            0
        );
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 1, HANDLE]
            ),
            -2
        );
        let pending = begin_read(&mut store, &instance, &invoke);
        complete(&records);
        outputs(memory.data_mut(&mut store));
        let before = memory.data(&store).to_vec();
        super::tests::set_rights(&mut store, crate::runtime::syscall::VolumeRights(0));
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
            -2
        );
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
        assert_eq!(records.lock().unwrap().writes, 1);
        assert_eq!(records.lock().unwrap().reads, 1);
    }
}
#[test]
fn schema_aggregate_limits_and_fuel_bound_begin_decoding() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let volume = volume(&store);
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for index in 0..17 {
            let base = 128 + index * type_input::SIZEOF as usize;
            word(memory.data_mut(&mut store), base, type_input::SIZEOF);
            word(memory.data_mut(&mut store), base + type_input::FIELDS, 16384);
            word(memory.data_mut(&mut store), base + type_input::COUNT, 256);
            word(memory.data_mut(&mut store), base + type_input::STRIDE, field_input::SIZEOF);
            for field in 0..256 {
                word(
                    memory.data_mut(&mut store),
                    16384 + field * field_input::SIZEOF as usize,
                    field_input::SIZEOF,
                );
            }
        }
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 17, HANDLE]
            ),
            -9
        );
        inputs(memory.data_mut(&mut store));
        store.set_fuel(1005).unwrap();
        let result = invoke.call(
            &mut store,
            (
                Opcode::SchemaEstablishBegin as u32,
                volume,
                128,
                1,
                u64::from(type_input::SIZEOF),
                0,
                0,
            ),
        );
        assert!(matches!(result, Ok(-9) | Err(_)));
        assert!(records.lock().unwrap().calls.is_empty());
        assert_eq!(records.lock().unwrap().writes, 0);
    }
}
#[test]
fn async_schema_maximum_fields_types_and_result_overflow() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        records.lock().unwrap().schema.types = (0u32..256)
            .map(|index| {
                let mut id = [0; 16];
                id[..4].copy_from_slice(&(index + 1).to_le_bytes());
                TypeDeclaration {
                    id,
                    name: format!("T{index}"),
                    deprecated: false,
                    fields: (0u32..16)
                        .map(|f| {
                            let mut id = [0; 16];
                            id[..4].copy_from_slice(&(257 + index * 16 + f).to_le_bytes());
                            FieldDeclaration {
                                id,
                                name: format!("f{f}"),
                                value_type: ValueType::Text,
                                deprecated: false,
                                repeated: false,
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let pending = begin_read(&mut store, &instance, &invoke);
        complete(&records);
        word(memory.data_mut(&mut store), 1024, schema::SIZEOF);
        record_buffer(
            memory.data_mut(&mut store),
            1024 + schema::TYPES_DATA,
            16384,
            256,
            type_record::SIZEOF,
        );
        record_buffer(
            memory.data_mut(&mut store),
            1024 + schema::FIELDS_DATA,
            65536,
            4096,
            field::SIZEOF,
        );
        buffer(memory.data_mut(&mut store), 1024 + schema::BYTES_DATA, 1048576, 1048576);
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
            0
        );
        assert_eq!(
            (
                number(memory.data(&store), 1024 + schema::TYPES_COUNT),
                number(memory.data(&store), 1024 + schema::FIELDS_COUNT)
            ),
            (256, 4096)
        );
        records.lock().unwrap().schema.types[0].fields.push(FieldDeclaration {
            id: [99; 16],
            name: "extra".into(),
            value_type: ValueType::Text,
            deprecated: false,
            repeated: false,
        });
        let pending = begin_read(&mut store, &instance, &invoke);
        complete(&records);
        let mut before = memory.data(&store).to_vec();
        word(&mut before, 1024 + schema::WRITTEN, 0);
        for index in 0..256 {
            word(
                &mut before,
                16384 + index * type_record::SIZEOF as usize + type_record::WRITTEN,
                0,
            );
        }
        for index in 0..4096 {
            word(&mut before, 65536 + index * field::SIZEOF as usize + field::WRITTEN, 0);
        }
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaReadTake, [pending, 1024, 0, 0]),
            -9
        );
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
    }
}
#[test]
#[ignore = "requires the independently compiled C and Rust wasm32 probes"]
fn compiler_schema_records_use_separate_begin_and_take_exports() {
    for language in ["c", "rust"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/wasm-lowering/schema-{language}.wasm"));
        compiled_schema_guest(language, &std::fs::read(path).unwrap());
    }
}

#[test]
fn frozen_v1_rc2_schema_guest_runs_on_current_runtime() {
    // Execute retained compiler output against today's runtime, without rebuilding it.
    for (language, bytes) in super::frozen_guest_tests::SCHEMA {
        compiled_schema_guest(language, bytes);
    }
}

fn compiled_schema_guest(language: &str, bytes: &[u8]) {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, bytes);
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let generation = u64::from(volume.generation);
        let establish = instance
            .get_typed_func::<(u64, u32), u64>(&mut store, "probe_schema_establish_begin")
            .unwrap();
        let take = instance
            .get_typed_func::<(u64, u32), i32>(&mut store, "probe_schema_establish_take")
            .unwrap();
        let read = instance
            .get_typed_func::<(u64, u32), u64>(&mut store, "probe_schema_read_begin")
            .unwrap();
        let snapshot =
            instance.get_typed_func::<(u64, u32), u32>(&mut store, "probe_schema").unwrap();
        let pending = establish.call(&mut store, (generation, volume.slot)).unwrap();
        assert_ne!(pending, 0);
        assert_eq!(records.lock().unwrap().writes, 0);
        assert_eq!(take.call(&mut store, (pending >> 32, pending as u32)).unwrap(), -6);
        complete(&records);
        assert_eq!(take.call(&mut store, (pending >> 32, pending as u32)).unwrap(), 0);
        let pending = read.call(&mut store, (generation, volume.slot)).unwrap();
        assert_ne!(pending, 0);
        assert_eq!(records.lock().unwrap().reads, 0);
        complete(&records);
        assert_eq!(
            snapshot.call(&mut store, (pending >> 32, pending as u32)).unwrap(),
            0,
            "{backend:?} {language}"
        );
        let records = records.lock().unwrap();
        assert_eq!((records.writes, records.reads, records.released.len()), (1, 1, 2));
        assert_eq!(records.schema.types[0].fields[0].value_type, ValueType::Reference([8; 16]));
    }
}

#[test]
fn schema_exact_name_budget_is_owned_once_and_overflow_admits_nothing() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let volume = volume(&store);
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let mut cursor = 262144;
        for index in 0u32..256 {
            let data = memory.data_mut(&mut store);
            let base = 128 + index as usize * type_input::SIZEOF as usize;
            word(data, base, type_input::SIZEOF);
            word(data, base + type_input::ID, index + 1);
            word(data, base + type_input::NAME_DATA, cursor as u32);
            word(data, base + type_input::NAME_LENGTH, 16);
            data[cursor..cursor + 16].copy_from_slice(format!("Type{index:012}").as_bytes());
            cursor += 16;
            word(data, base + type_input::FIELDS, 16384 + index * 16 * field_input::SIZEOF);
            word(data, base + type_input::COUNT, 16);
            word(data, base + type_input::STRIDE, field_input::SIZEOF);
            for field in 0u32..16 {
                let base = 16384 + (index * 16 + field) as usize * field_input::SIZEOF as usize;
                word(data, base, field_input::SIZEOF);
                word(data, base + field_input::ID, 257 + index * 16 + field);
                word(data, base + field_input::NAME_DATA, cursor as u32);
                word(data, base + field_input::NAME_LENGTH, 255);
                word(data, base + field_input::KIND, 1);
                data[cursor..cursor + 255]
                    .copy_from_slice(format!("f{field:02}{}", "x".repeat(252)).as_bytes());
                cursor += 255;
            }
        }
        assert_eq!(cursor - 262144, 1048576);
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 256, HANDLE]
            ),
            0
        );
        let pending = operation(&mut store, &instance);
        complete(&records);
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaEstablishTake, [pending, 0, 0, 0]),
            0
        );
        assert_eq!(records.lock().unwrap().schema.types.len(), 256);
        word(memory.data_mut(&mut store), 128 + type_input::NAME_LENGTH, 17);
        let before = memory.data(&store).to_vec();
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 256, HANDLE]
            ),
            -9
        );
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
        inputs(memory.data_mut(&mut store));
        records.lock().unwrap().deny = true;
        assert_eq!(
            call(
                &mut store,
                &instance,
                &invoke,
                Opcode::SchemaEstablishBegin,
                [volume, 128, 1, HANDLE]
            ),
            0
        );
        let pending = operation(&mut store, &instance);
        complete(&records);
        assert_eq!(
            call(&mut store, &instance, &invoke, Opcode::SchemaEstablishTake, [pending, 0, 0, 0]),
            -2
        );
        assert_eq!(records.lock().unwrap().writes, 1);
    }
}
#[test]
fn unknown_opcode_rejects_before_memory_or_admission() {
    for &backend in backends() {
        let (mut store, instance, records) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let invoke = invoke(&mut store, &instance);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.data_mut(&mut store).fill(0xa5);
        let before = memory.data(&store).to_vec();
        assert_eq!(
            invoke
                .call(
                    &mut store,
                    (u32::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX)
                )
                .unwrap(),
            -7
        );
        assert!(records.lock().unwrap().calls.is_empty());
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
    }
}
