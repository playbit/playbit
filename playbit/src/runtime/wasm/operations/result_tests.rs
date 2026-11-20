#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::tests::{backends, context};
use super::*;
use crate::runtime::linear_memory::layout::{
    data::{field, object, range, value, volume, volumes},
    environment::{argument, arguments, capabilities, participant},
};
use crate::runtime::{
    TEST_FUEL,
    host::{DataCall, DataResult},
    syscall::{FieldChange, FileChunk, Host, Id, ObjectRecord, Opcode, Participant, Value},
    wasm::{ExecutionBackend, Runtime},
};
use std::sync::{Arc, Mutex};
use wasmtime::{Instance, Store};

struct Results {
    root: Option<crate::runtime::host::files::Root>,
    file_capacity: Arc<crate::runtime::host::files::capacity::Capacity>,
    reads: usize,
    writes: usize,
    deny: bool,
    object: ObjectRecord,
    file: Vec<u8>,
    staged: Vec<u8>,
    file_calls: Vec<Arc<crate::runtime::host::files::Call>>,
    file_results:
        std::collections::BTreeMap<u64, HostResult<Arc<crate::runtime::host::files::Output>>>,
    calls: Vec<Arc<DataCall>>,
    completions: std::collections::BTreeMap<u64, HostResult<Arc<crate::runtime::host::DataResult>>>,
}
impl Default for Results {
    fn default() -> Self {
        Self {
            reads: 0,
            writes: 0,
            deny: false,
            object: ObjectRecord {
                id: [9; 16],
                type_id: [8; 16],
                fields: vec![FieldChange {
                    field_id: [7; 16],
                    values: Some(vec![Value::Text("abc".into())]),
                }],
            },
            file: b"hello".to_vec(),
            root: None,
            file_capacity: Default::default(),
            staged: vec![],
            file_calls: vec![],
            file_results: Default::default(),
            calls: vec![],
            completions: Default::default(),
        }
    }
}
struct DataHost(Arc<Mutex<Results>>);
impl Host for DataHost {
    fn file_capacity(&self) -> Arc<crate::runtime::host::files::capacity::Capacity> {
        self.0.lock().unwrap().file_capacity.clone()
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
        ]
    }
    fn file_roots(
        &mut self,
        _: &[crate::runtime::host::AvailableVolume],
    ) -> HostResult<Vec<crate::runtime::host::files::Root>> {
        let mut state = self.0.lock().unwrap();
        if state.root.is_none() {
            state.root = Some(crate::runtime::host::files::Root {
                replaces: None,
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                volume: [6; 16],
                grant: [6; 16],
                engine: 10,
                writable: true,
                credit: state
                    .file_capacity
                    .one(crate::runtime::host::files::capacity::Kind::File)?,
            });
        }
        Ok(vec![state.root.as_ref().unwrap().clone()])
    }
    fn begin_file(
        &mut self,
        _: u64,
        call: Arc<crate::runtime::host::files::Call>,
        _: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<u64> {
        let mut state = self.0.lock().unwrap();
        state.file_calls.push(call);
        Ok((1 << 63) | state.file_calls.len() as u64)
    }
    fn peek_file(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::files::Output>> {
        self.0
            .lock()
            .unwrap()
            .file_results
            .get(&token)
            .cloned()
            .unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_file(&mut self, token: u64, _: bool) {
        self.0.lock().unwrap().file_results.remove(&token);
    }
    fn discard_file(
        &mut self,
        _: u64,
        _: bool,
        _: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<()> {
        Ok(())
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(call);
        Ok(state.calls.len() as u64)
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
    fn read_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object: Id,
        _: &[Id],
    ) -> HostResult<ObjectRecord> {
        assert_eq!((volume.volume_id, object), ([6; 16], [9; 16]));
        let mut results = self.0.lock().unwrap();
        results.reads += 1;
        if results.deny {
            return Err(HostError::AccessDenied);
        }
        Ok(results.object.clone())
    }
    fn read_file(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        offset: u64,
        limit: u32,
    ) -> HostResult<FileChunk> {
        assert_eq!((volume.volume_id, volume.entry_id), ([6; 16], [5; 15]));
        let mut results = self.0.lock().unwrap();
        results.reads += 1;
        if results.deny {
            return Err(HostError::AccessDenied);
        }
        let offset = usize::try_from(offset).map_err(|_| HostError::InvalidArgument)?;
        let tail = results.file.get(offset..).ok_or(HostError::InvalidArgument)?;
        Ok(FileChunk {
            total_bytes: results.file.len() as u64,
            bytes: tail[..tail.len().min(limit as usize)].to_vec(),
        })
    }
    fn write_file(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        bytes: &[u8],
    ) -> HostResult<()> {
        assert_eq!((volume.volume_id, volume.entry_id), ([6; 16], [5; 15]));
        let mut results = self.0.lock().unwrap();
        if results.deny {
            return Err(HostError::AccessDenied);
        }
        results.writes += 1;
        results.file = bytes.to_vec();
        Ok(())
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $dispatch_caps (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_args (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_presence (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_volumes (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_read (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $caps (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_caps (i32.const 1)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $args (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_args (i32.const 6)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $presence (param i32 i32 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_presence (i32.const 10)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.extend_i32_u (local.get 2)) (i64.extend_i32_u (local.get 3)) (i64.const 0) (i64.const 0))))
    (func $volumes (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_volumes (i32.const 7)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))

    (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6 call $dispatch_read)
    (memory (export "memory") 64 256)
    (func (export "caps") (param i32) (result i32) local.get 0 call $caps)
    (func (export "args") (param i32) (result i32) local.get 0 call $args)
    (func (export "presence") (param i32 i32 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 local.get 3 call $presence)
    (func (export "volumes") (param i32) (result i32) local.get 0 call $volumes)

)"#;

fn fixture(backend: ExecutionBackend) -> (Store<WasmState>, Instance, Arc<Mutex<Results>>) {
    let runtime =
        Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
            .unwrap();
    let module = runtime.compile_app(&wat::parse_str(MODULE).unwrap()).unwrap();
    instantiate(&runtime, &module)
}
fn instantiate(
    runtime: &Runtime,
    module: &wasmtime::Module,
) -> (Store<WasmState>, Instance, Arc<Mutex<Results>>) {
    let mut context = context();
    context.volumes[0].tags = vec!["test".into()];
    context.startup_arguments.insert("mode".into(), "demo".into());
    context.participants.push(Participant {
        account_id: [1; 16],
        device_id: [2; 16],
        instance_id: [3; 16],
        build_id: [5; 32],
    });
    let results = Arc::new(Mutex::new(Results::default()));
    let state =
        WasmState::new(context, Box::new(DataHost(results.clone())), runtime.fuel_limit).unwrap();
    let mut store = Store::new(&runtime.engine, state);
    store.set_fuel(TEST_FUEL).unwrap();
    let mut linker = Linker::new(&runtime.engine);
    define(&mut linker).unwrap();
    let instance = linker.instantiate(&mut store, module).unwrap();
    (store, instance, results)
}
const READ_HANDLE: u32 = 480000;
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
fn begin_read(store: &mut Store<WasmState>, instance: &Instance, fields: u32, count: u32) -> u64 {
    let volume = store.data().app.volume_handle([6; 16]).unwrap();
    let (entry, _) = super::tests::entry_pointers_at(&mut *store, instance, volume, 2097184);
    let volume = u64::from(entry);
    assert_eq!(
        scalar(
            store,
            instance,
            Opcode::ObjectReadBegin,
            [volume, 32, u64::from(fields), u64::from(count), u64::from(READ_HANDLE), 0]
        ),
        0
    );
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    playbit_api::abi::dispatch::pack_handle(
        number(memory.data(&*store), READ_HANDLE as usize + 4),
        number(memory.data(&*store), READ_HANDLE as usize),
    )
}
fn complete(host: &Arc<Mutex<Results>>) {
    let (token, owned) = {
        let records = host.lock().unwrap();
        (records.calls.len() as u64, records.calls.last().unwrap().as_ref().clone())
    };
    // The external driver executes once between guest calls.
    let result = owned.execute(&mut DataHost(host.clone())).map(Arc::new);
    assert!(host.lock().unwrap().completions.insert(token, result).is_none());
}
fn take_read(store: &mut Store<WasmState>, instance: &Instance, pending: u64, output: u32) -> i64 {
    scalar(store, instance, Opcode::ObjectReadTake, [pending, u64::from(output), 0, 0, 0, 0])
}
fn read(
    store: &mut Store<WasmState>,
    instance: &Instance,
    host: &Arc<Mutex<Results>>,
    fields: u32,
    count: u32,
    output: u32,
) -> i64 {
    let pending = begin_read(store, instance, fields, count);
    complete(host);
    take_read(store, instance, pending, output)
}

fn word(memory: &mut [u8], offset: usize, value: u32) {
    memory[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn number(memory: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(memory[offset..offset + 4].try_into().unwrap())
}
fn buffer(memory: &mut [u8], descriptor: usize, data: u32, capacity: u32) {
    word(memory, descriptor, data);
    word(memory, descriptor + 4, capacity);
    word(memory, descriptor + 8, 0xcccccccc);
}
fn only_counts_changed(before: &[u8], after: &[u8], counts: &[(usize, u32)]) {
    let mut expected = before.to_vec();
    for &(offset, value) in counts {
        word(&mut expected, offset, value);
    }
    assert_eq!(
        expected.iter().zip(after).position(|(a, b)| a != b),
        None,
        "unexpected memory change"
    );
}
fn record_buffer(memory: &mut [u8], descriptor: usize, data: u32, capacity: u32, size: u32) {
    buffer(memory, descriptor, data, capacity);
    word(memory, descriptor + 12, size);
    for index in 0..capacity as usize {
        word(memory, data as usize + index * size as usize, size);
        word(memory, data as usize + index * size as usize + 4, 0);
    }
}
fn object_outputs(data: &mut [u8]) {
    data[32..48].fill(9);
    data[64..80].fill(7);
    word(data, 128, object::SIZEOF);
    word(data, 132, 0);
    data[128 + object::TYPE_ID..128 + object::TYPE_ID + 16].fill(0xcc);
    record_buffer(data, 128 + object::FIELDS_DATA, 256, 1, field::SIZEOF);
    record_buffer(data, 128 + object::VALUES_DATA, 512, 1, value::SIZEOF);
    buffer(data, 128 + object::BYTES_DATA, 2048, 3);
}
fn metadata_outputs(data: &mut [u8], name: &str, capacity: u32, bytes: u32) {
    let (size, offset, element) = match name {
        "caps" => (capabilities::SIZEOF, capabilities::NAMES_DATA, range::SIZEOF),
        "args" => (arguments::SIZEOF, arguments::ARGUMENTS_DATA, argument::SIZEOF),
        _ => (volumes::SIZEOF, volumes::VOLUMES_DATA, volume::SIZEOF),
    };
    data[128..128 + size as usize].fill(0);
    word(data, 128, size);
    record_buffer(data, 128 + offset, if capacity == 0 { 0 } else { 256 }, capacity, element);
    if name == "volumes" {
        record_buffer(
            data,
            128 + volumes::TAGS_DATA,
            if capacity == 0 { 0 } else { 4096 },
            capacity,
            range::SIZEOF,
        );
        buffer(data, 128 + volumes::BYTES_DATA, if bytes == 0 { 0 } else { 6144 }, bytes);
    } else {
        buffer(data, 128 + capabilities::BYTES_DATA, if bytes == 0 { 0 } else { 4096 }, bytes);
    }
}
#[test]
fn metadata_short_results_only_publish_counts_then_return_disjoint_arrays() {
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for name in ["caps", "args", "volumes"] {
            let call = instance.get_typed_func::<u32, i32>(&mut store, name).unwrap();
            metadata_outputs(memory.data_mut(&mut store), name, 0, 0);
            let before = memory.data(&store).to_vec();
            assert_eq!(call.call(&mut store, 128).unwrap(), -11, "{name}");
            let counts = match name {
                "caps" => vec![
                    (128 + capabilities::NAMES_COUNT, store.data().app.supported.len() as u32),
                    (
                        128 + capabilities::BYTES_COUNT,
                        store
                            .data()
                            .app
                            .supported
                            .iter()
                            .map(|op| op.capability().len() as u32)
                            .sum(),
                    ),
                ],
                "args" => {
                    vec![(128 + arguments::ARGUMENTS_COUNT, 1), (128 + arguments::BYTES_COUNT, 8)]
                }
                _ => vec![
                    (128 + volumes::VOLUMES_COUNT, 1),
                    (128 + volumes::TAGS_COUNT, 1),
                    (128 + volumes::BYTES_COUNT, 8),
                ],
            };
            only_counts_changed(&before, memory.data(&store), &counts);
            metadata_outputs(
                memory.data_mut(&mut store),
                name,
                counts[0].1,
                counts.last().unwrap().1,
            );
            assert_eq!(call.call(&mut store, 128).unwrap(), 0, "{name}");
            match name {
                "args" => assert_eq!(&memory.data(&store)[4096..4104], b"modedemo"),
                "volumes" => {
                    assert_eq!(
                        &memory.data(&store)[256 + volume::ID..256 + volume::ID + 16],
                        &[6; 16]
                    );
                    assert_eq!(
                        &memory.data(&store)[256 + volume::GRANT_ID..256 + volume::GRANT_ID + 16],
                        &[6; 16]
                    );
                    assert_eq!(
                        number(memory.data(&store), 256 + volume::FLAGS),
                        playbit_api::abi::data::VOLUME_ROOT
                            | playbit_api::abi::data::VOLUME_PLAYBIT
                    );
                    assert_eq!(&memory.data(&store)[6144..6152], b"Roottest");
                }
                _ => assert_eq!(
                    number(memory.data(&store), 128 + capabilities::NAMES_COUNT),
                    store.data().app.supported.len() as u32
                ),
            }
        }
    }
}
#[test]
fn metadata_rejects_descriptor_array_and_cross_array_aliases_without_writes() {
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for name in ["caps", "args", "volumes"] {
            let call = instance.get_typed_func::<u32, i32>(&mut store, name).unwrap();
            let first =
                if name == "volumes" { volumes::VOLUMES_DATA } else { capabilities::NAMES_DATA };
            for (pointer, capacity) in [(128, 1), (0, 1), (4194303, 1), (256, u32::MAX)] {
                metadata_outputs(memory.data_mut(&mut store), name, 1, 8);
                word(memory.data_mut(&mut store), 128 + first, pointer);
                word(memory.data_mut(&mut store), 128 + first + 4, capacity);
                let before = memory.data(&store).to_vec();
                assert!(call.call(&mut store, 128).unwrap() < 0, "{name}");
                assert_eq!(
                    memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                    None,
                    "unexpected memory change"
                );
            }
            metadata_outputs(memory.data_mut(&mut store), name, 1, 8);
            let bytes =
                if name == "volumes" { volumes::BYTES_DATA } else { capabilities::BYTES_DATA };
            word(memory.data_mut(&mut store), 128 + bytes, 256);
            let before = memory.data(&store).to_vec();
            assert_eq!(call.call(&mut store, 128).unwrap(), -3);
            assert_eq!(
                memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                None,
                "unexpected memory change"
            );
        }
    }
}

#[test]
fn object_short_take_retains_its_snapshot_and_a_distinct_begin_reads_fresh_data() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        object_outputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 128 + object::BYTES_CAPACITY, 2);
        let pending = begin_read(&mut store, &instance, 64, 1);
        let before = memory.data(&store).to_vec();
        assert_eq!(take_read(&mut store, &instance, pending, 128), -6);
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
        assert_eq!(host.lock().unwrap().reads, 0);
        complete(&host);
        assert_eq!(take_read(&mut store, &instance, pending, 128), -11);
        only_counts_changed(
            &before,
            memory.data(&store),
            &[
                (128 + object::FIELDS_COUNT, 1),
                (128 + object::VALUES_COUNT, 1),
                (128 + object::BYTES_COUNT, 3),
            ],
        );
        host.lock().unwrap().object.fields[0].values = Some(vec![Value::Text("new".into())]);
        word(memory.data_mut(&mut store), 128 + object::BYTES_CAPACITY, 3);
        assert_eq!(take_read(&mut store, &instance, pending, 128), 0);
        assert_eq!(&memory.data(&store)[2048..2051], b"abc");
        assert_eq!(host.lock().unwrap().reads, 1);
        assert_eq!(take_read(&mut store, &instance, pending, 128), -8);
        assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
        assert_eq!(
            &memory.data(&store)[128 + object::TYPE_ID..128 + object::TYPE_ID + 16],
            &[8; 16]
        );
        assert_eq!(&memory.data(&store)[2048..2051], b"new");
        host.lock().unwrap().deny = true;
        let pending = begin_read(&mut store, &instance, 64, 1);
        complete(&host);
        let mut before = memory.data(&store).to_vec();
        for offset in [132, 260, 516] {
            word(&mut before, offset, 0);
        }
        assert_eq!(take_read(&mut store, &instance, pending, 128), -2);
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
        host.lock().unwrap().deny = false;
        host.lock().unwrap().object.id = [0; 16];
        let pending = begin_read(&mut store, &instance, 64, 1);
        complete(&host);
        let mut before = memory.data(&store).to_vec();
        for offset in [132, 260, 516] {
            word(&mut before, offset, 0);
        }
        assert_eq!(take_read(&mut store, &instance, pending, 128), -4);
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
        assert!(host.lock().unwrap().completions.is_empty());
    }
}

#[test]
fn object_take_rejects_output_aliases_and_can_reuse_owned_input_storage() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        object_outputs(memory.data_mut(&mut store));
        let pending = begin_read(&mut store, &instance, 64, 1);
        complete(&host);
        for member in
            [128 + object::FIELDS_DATA, 128 + object::VALUES_DATA, 128 + object::BYTES_DATA]
        {
            for target in [0, 128, 256, 512, 2048, u32::MAX] {
                if [
                    (128 + object::FIELDS_DATA, 256),
                    (128 + object::VALUES_DATA, 512),
                    (128 + object::BYTES_DATA, 2048),
                ]
                .contains(&(member, target))
                {
                    continue;
                }
                object_outputs(memory.data_mut(&mut store));
                word(memory.data_mut(&mut store), member, target);
                let before = memory.data(&store).to_vec();
                assert!(matches!(take_read(&mut store, &instance, pending, 128), -3 | -11));
                assert_eq!(
                    memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                    None,
                    "unexpected memory change"
                );
                assert_eq!(host.lock().unwrap().reads, 1);
                assert_eq!(host.lock().unwrap().completions.len(), 1);
            }
        }
        object_outputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 128 + object::BYTES_DATA, 32);
        assert_eq!(take_read(&mut store, &instance, pending, 128), 0);
        assert_eq!(&memory.data(&store)[32..35], b"abc");
        assert_eq!(host.lock().unwrap().calls.len(), 1);
        assert!(host.lock().unwrap().completions.is_empty());
    }
}

#[test]
fn object_values_and_tombstones_use_exact_wasm32_records() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        object_outputs(memory.data_mut(&mut store));
        record_buffer(
            memory.data_mut(&mut store),
            128 + object::VALUES_DATA,
            512,
            7,
            value::SIZEOF,
        );
        word(memory.data_mut(&mut store), 128 + object::BYTES_CAPACITY, 8);
        host.lock().unwrap().object.fields[0].values = Some(vec![
            Value::Text("abc".into()),
            Value::Boolean(true),
            Value::Integer(-7),
            Value::Float(3.5),
            Value::Bytes(vec![0, 255]),
            Value::Reference { volume_id: [6; 16], database_id: [5; 15], object_id: [9; 16] },
            Value::File { volume_id: [6; 16], entry_id: [7; 15] },
        ]);
        assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
        for index in 0..7 {
            assert_eq!(
                number(memory.data(&store), 512 + index * value::SIZEOF as usize + value::KIND),
                index as u32 + 1
            );
            assert_eq!(
                number(memory.data(&store), 512 + index * value::SIZEOF as usize + value::RESERVED),
                0
            );
        }
        assert_eq!(&memory.data(&store)[2048..2053], &[b'a', b'b', b'c', 0, 255]);
        assert_eq!(
            &memory.data(&store)[512 + 6 * value::SIZEOF as usize + value::ID1
                ..512 + 6 * value::SIZEOF as usize + value::ID1 + 16],
            &[6; 16]
        );
        assert_eq!(
            &memory.data(&store)[512 + 6 * value::SIZEOF as usize + value::ENTRY_ID
                ..512 + 6 * value::SIZEOF as usize + value::ENTRY_ID + 15],
            &[7; 15]
        );
        host.lock().unwrap().object.fields[0].values = None;
        assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
        assert_eq!(number(memory.data(&store), 256 + field::PRESENT), 0);
        host.lock().unwrap().object.fields[0].values = Some(vec![]);
        assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
        assert_eq!(number(memory.data(&store), 256 + field::PRESENT), 1);
        assert_eq!(number(memory.data(&store), 128 + object::VALUES_COUNT), 0);
    }
}

#[test]
#[ignore = "requires independent wasm32 C/Rust compilers; use build-wasm-probe.sh"]
fn compiler_variable_results_use_separate_begin_and_take_exports() {
    for language in ["c", "rust"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/wasm-lowering/result-{language}.wasm"));
        compiled_result_guest(language, &std::fs::read(path).unwrap());
    }
}

#[test]
fn frozen_v1_rc2_result_guest_runs_on_current_runtime() {
    // Execute retained compiler output against today's runtime, without rebuilding it.
    for (language, bytes) in super::frozen_guest_tests::RESULT {
        compiled_result_guest(language, bytes);
    }
}

fn compiled_result_guest(language: &str, bytes: &[u8]) {
    for &backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime.compile_app(bytes).unwrap();
        let (mut store, instance, host) = instantiate(&runtime, &module);
        let probe = instance.get_typed_func::<(), u32>(&mut store, "probe_results").unwrap();
        assert_eq!(probe.call(&mut store, ()).unwrap(), 0, "{language}/{backend:?}");
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let begin = instance
            .get_typed_func::<(u64, u32, u32), u64>(&mut store, "probe_result_begin")
            .unwrap();
        let take = instance
            .get_typed_func::<(u64, u32, u32, u32), i32>(&mut store, "probe_result_take")
            .unwrap();
        for (kind, edited) in [(0, 0), (1, 0), (2, 0), (1, 1)] {
            let resource = if kind == 0 {
                playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot)
            } else {
                let app = &mut store.data_mut().app;
                let resource = if kind == 2 {
                    crate::runtime::file_handles::Kind::Writer
                } else {
                    crate::runtime::file_handles::Kind::Reader
                };
                let reservation = app.files.reserve(&mut app.capabilities, resource).unwrap();
                app.files
                    .insert(&mut app.capabilities, &reservation, 10 + u64::from(kind), [6; 16])
                    .unwrap()
            };
            let counts = {
                let host = host.lock().unwrap();
                (host.reads, host.writes)
            };
            let pending = begin.call(&mut store, (resource >> 32, resource as u32, kind)).unwrap();
            assert_ne!(pending, 0);
            assert_eq!(
                take.call(&mut store, (pending >> 32, pending as u32, kind, edited)).unwrap(),
                -6
            );
            assert_eq!(
                {
                    let host = host.lock().unwrap();
                    (host.reads, host.writes)
                },
                counts
            );
            if kind == 0 {
                complete(&host);
            } else {
                complete_file(&host);
            }
            assert_eq!(
                take.call(&mut store, (pending >> 32, pending as u32, kind, edited)).unwrap(),
                0,
                "{language}/{backend:?}/{kind}"
            );
            assert_eq!(
                take.call(&mut store, (pending >> 32, pending as u32, kind, edited)).unwrap(),
                -8
            );
        }
        let host = host.lock().unwrap();
        assert_eq!((host.reads, host.writes, host.calls.len() + host.file_calls.len()), (3, 1, 4));
        assert_eq!(host.file, b"hello");
        assert_eq!(host.staged, b"new");
        assert!(host.file_results.is_empty());
        assert!(host.completions.is_empty());
    }
}

#[test]
fn maximum_object_results_fit_and_one_over_limits_preserve_outputs() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        object_outputs(memory.data_mut(&mut store));
        record_buffer(
            memory.data_mut(&mut store),
            128 + object::VALUES_DATA,
            512,
            4096,
            value::SIZEOF,
        );
        buffer(memory.data_mut(&mut store), 128 + object::BYTES_DATA, 524288, 1048576);
        host.lock().unwrap().object.fields[0].values = Some(vec![Value::Boolean(true); 4096]);
        assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
        assert_eq!(number(memory.data(&store), 128 + object::VALUES_COUNT), 4096);
        assert_eq!(
            number(memory.data(&store), 512 + 4095 * value::SIZEOF as usize + value::SCALAR),
            1
        );
        host.lock().unwrap().object.fields[0].values =
            Some(vec![Value::Bytes(vec![0x75; 1048576])]);
        assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
        assert_eq!(number(memory.data(&store), 128 + object::BYTES_COUNT), 1048576);
        assert!(memory.data(&store)[524288..524288 + 1048576].iter().all(|b| *b == 0x75));
        let pending = begin_read(&mut store, &instance, 64, 1);
        complete(&host);
        let reads = host.lock().unwrap().reads;
        word(memory.data_mut(&mut store), 128 + object::BYTES_CAPACITY, 1048577);
        let before = memory.data(&store).to_vec();
        assert_eq!(take_read(&mut store, &instance, pending, 128), -9);
        assert_eq!(host.lock().unwrap().reads, reads);
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
        word(memory.data_mut(&mut store), 128 + object::BYTES_CAPACITY, 1048576);
        assert_eq!(take_read(&mut store, &instance, pending, 128), 0);
        host.lock().unwrap().object.fields[0].values = Some(vec![Value::Boolean(true); 4097]);
        let pending = begin_read(&mut store, &instance, 64, 1);
        complete(&host);
        let mut before = memory.data(&store).to_vec();
        for offset in [132, 260] {
            word(&mut before, offset, 0);
        }
        for index in 0..4096 {
            word(&mut before, 512 + index * value::SIZEOF as usize + value::WRITTEN, 0);
        }
        assert_eq!(take_read(&mut store, &instance, pending, 128), -9);
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
    }
}

#[test]
fn maximum_metadata_arrays_fit() {
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let generation = store.data().app.generation;
        let participants = (0_u32..1024)
            .map(|index| {
                let mut instance_id = [0; 16];
                instance_id[..4].copy_from_slice(&index.to_le_bytes());
                Participant {
                    account_id: [1; 16],
                    device_id: [2; 16],
                    instance_id,
                    build_id: [5; 32],
                }
            })
            .collect();
        store.data_mut().app.set_participants(u64::from(generation), participants).unwrap();
        record_buffer(memory.data_mut(&mut store), 128, 65536, 1024, participant::SIZEOF);
        let presence =
            instance.get_typed_func::<(u32, u32, u32, u32), i32>(&mut store, "presence").unwrap();
        assert_eq!(presence.call(&mut store, (0, 0, participant::SIZEOF, 160)).unwrap(), -11);
        assert_eq!(number(memory.data(&store), 160), 1024);
        assert_eq!(presence.call(&mut store, (65536, 1024, participant::SIZEOF, 160)).unwrap(), 0);
        assert_eq!(
            number(
                memory.data(&store),
                65536 + 1023 * participant::SIZEOF as usize + participant::INSTANCE_ID
            ),
            1023
        );
        assert_eq!(
            &memory.data(&store)[65536 + participant::BUILD_ID..65536 + participant::BUILD_ID + 32],
            &[5; 32]
        );
        for (pointer, count, output, status) in [
            (65536, 1025, 160, -9),
            (65536, 1024, 65536, -3),
            (0, 1, 160, -3),
            (4194303, 1, 160, -3),
        ] {
            let before = memory.data(&store).to_vec();
            assert_eq!(
                presence.call(&mut store, (pointer, count, participant::SIZEOF, output)).unwrap(),
                status
            );
            assert_eq!(
                memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
                None,
                "unexpected memory change"
            );
        }
        store.data_mut().app.context.startup_arguments =
            (0..32).map(|i| (format!("arg{i:02}"), "x".repeat(900))).collect();
        metadata_outputs(memory.data_mut(&mut store), "args", 32, 32768);
        let args = instance.get_typed_func::<u32, i32>(&mut store, "args").unwrap();
        assert_eq!(args.call(&mut store, 128).unwrap(), 0);
        assert_eq!(number(memory.data(&store), 128 + arguments::BYTES_COUNT), 32 * 905);
    }
}

#[test]
#[ignore = "operator-only optimized CPU timing, excludes storage and rendering"]
fn async_object_read_controlled_memory_cpu_timing() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        object_outputs(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 128 + object::BYTES_CAPACITY, 4096);
        host.lock().unwrap().object.fields[0].values = Some(vec![Value::Text("x".repeat(4096))]);
        let mut samples = Vec::with_capacity(1000);
        for index in 0..1100 {
            store.set_fuel(TEST_FUEL).unwrap();
            let start = std::time::Instant::now();
            assert_eq!(read(&mut store, &instance, &host, 64, 1, 128), 0);
            if index >= 100 {
                samples.push(start.elapsed().as_nanos());
            }
        }
        samples.sort_unstable();
        println!(
            "{backend:?}: 1000 4-KiB reads, ns p50={} p95={} p99={} max={}",
            samples[499], samples[949], samples[989], samples[999]
        );
    }
}

#[test]
fn volume_role_tail_preserves_released_array_strides() {
    use crate::runtime::syscall::{AvailableVolume, VolumeRights};
    for &backend in backends() {
        for (stride, capacity) in [(80, 80), (88, 84), (96, 88)] {
            let (mut store, instance, _) = fixture(backend);
            let available = (0_u8..3)
                .map(|index| AvailableVolume {
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id: if index == 0 { [6; 16] } else { [0x40 + index; 16] },
                    name: format!("V{index}"),
                    tags: vec![],
                    rights: if index == 0 { VolumeRights::ALL } else { VolumeRights::READ },
                    is_root: index == 0,
                    role: u32::from(index) + 1,
                })
                .collect();
            store.data_mut().app.replace_volumes(available).unwrap();
            let memory = instance.get_memory(&mut store, "memory").unwrap();
            let bytes = memory.data_mut(&mut store);
            bytes[4096..4096 + 3 * stride + 16].fill(0xcc);
            word(bytes, 128, volumes::SIZEOF);
            record_buffer(bytes, 128 + volumes::VOLUMES_DATA, 4096, 3, stride as u32);
            for index in 0..3 {
                word(bytes, 4096 + index * stride, capacity as u32);
            }
            record_buffer(bytes, 128 + volumes::TAGS_DATA, 8192, 0, range::SIZEOF);
            buffer(bytes, 128 + volumes::BYTES_DATA, 8256, 64);
            let call = instance.get_typed_func::<u32, i32>(&mut store, "volumes").unwrap();
            assert_eq!(call.call(&mut store, 128).unwrap(), 0);
            let bytes = memory.data(&store);
            assert_eq!(number(bytes, 128 + volumes::VOLUMES_COUNT), 3);
            let written = capacity.min(84);
            for index in 0..3 {
                let row = 4096 + index * stride;
                assert_eq!(number(bytes, row), capacity as u32);
                assert_eq!(number(bytes, row + volume::WRITTEN), written as u32);
                let id = if index == 0 { [6; 16] } else { [0x40 + index as u8; 16] };
                assert_eq!(&bytes[row + volume::ID..row + volume::ID + 16], &id);
                if capacity >= 84 {
                    assert_eq!(number(bytes, row + volume::ROLE), index as u32 + 1);
                }
                assert!(bytes[row + written..row + stride].iter().all(|byte| *byte == 0xcc));
            }
            assert!(
                bytes[4096 + 3 * stride..4096 + 3 * stride + 16].iter().all(|byte| *byte == 0xcc)
            );
        }
    }
}

#[test]
fn maximum_volume_and_tag_arrays_preserve_ranges_and_capability_slots() {
    use crate::runtime::syscall::{AvailableVolume, VolumeRights};
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let volumes = (0_u32..256)
            .map(|index| {
                let mut id = [0; 16];
                id[..4].copy_from_slice(&(1000 + index).to_le_bytes());
                if index == 0 {
                    id = [6; 16];
                }
                AvailableVolume {
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id,
                    name: format!("V{index}"),
                    tags: (0..64).map(|tag| format!("t{tag:02}")).collect(),
                    rights: if index == 0 { VolumeRights::ALL } else { VolumeRights::READ },
                    is_root: index == 0,
                    role: if index < 6 { index + 1 } else { 0 },
                }
            })
            .collect();
        store.data_mut().app.replace_volumes(volumes).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        word(memory.data_mut(&mut store), 128, volumes::SIZEOF);
        record_buffer(
            memory.data_mut(&mut store),
            128 + volumes::VOLUMES_DATA,
            65536,
            256,
            volume::SIZEOF,
        );
        record_buffer(
            memory.data_mut(&mut store),
            128 + volumes::TAGS_DATA,
            1048576,
            16384,
            range::SIZEOF,
        );
        buffer(memory.data_mut(&mut store), 128 + volumes::BYTES_DATA, 2097152, 1048576);
        let call = instance.get_typed_func::<u32, i32>(&mut store, "volumes").unwrap();
        assert_eq!(call.call(&mut store, 128).unwrap(), 0);
        assert_eq!(number(memory.data(&store), 128 + volumes::VOLUMES_COUNT), 256);
        assert_eq!(number(memory.data(&store), 128 + volumes::TAGS_COUNT), 16384);
        for index in 0..6 {
            let record = 65536 + index * volume::SIZEOF as usize;
            assert_eq!(number(memory.data(&store), record + volume::WRITTEN), 84);
            assert_eq!(number(memory.data(&store), record + volume::ROLE), index as u32 + 1);
        }
        assert_eq!(
            number(
                memory.data(&store),
                65536 + 255 * volume::SIZEOF as usize + volume::TAGS_OFFSET
            ),
            255 * 64
        );
        assert_eq!(
            number(memory.data(&store), 65536 + 255 * volume::SIZEOF as usize + volume::TAGS_COUNT),
            64
        );
        let final_tag =
            number(memory.data(&store), 1048576 + 16383 * range::SIZEOF as usize + range::OFFSET)
                as usize;
        assert_eq!(&memory.data(&store)[2097152 + final_tag..2097152 + final_tag + 3], b"t63");
        word(memory.data_mut(&mut store), 128 + volumes::VOLUMES_CAPACITY, 257);
        let before = memory.data(&store).to_vec();
        assert_eq!(call.call(&mut store, 128).unwrap(), -9);
        assert_eq!(
            memory.data(&store).iter().zip(&before).position(|(a, b)| a != b),
            None,
            "unexpected memory change"
        );
    }
}

fn complete_file(host: &Arc<Mutex<Results>>) {
    use crate::runtime::host::files::{Action, Output};
    let mut state = host.lock().unwrap();
    let call = state.file_calls.last().unwrap().clone();
    let token = (1 << 63) | state.file_calls.len() as u64;
    let output = match &call.action {
        Action::Read { offset, length } => {
            state.reads += 1;
            let bytes = state.file.get(*offset as usize..).unwrap_or_default();
            Output::Read { bytes: bytes[..bytes.len().min(*length as usize)].to_vec() }
        }
        Action::Write { offset: 0, bytes } => {
            state.writes += 1;
            state.staged = bytes.clone();
            Output::Written
        }
        _ => panic!("unexpected File result probe operation"),
    };
    state.file_results.insert(token, Ok(Arc::new(output)));
}
