#![allow(clippy::unwrap_used, reason = "Tests execute both actual Wasmtime KV adapters.")]
use super::{
    tests::{backends, context},
    *,
};
use crate::runtime::{
    TEST_FUEL,
    host::{DataCall, DataOperation, DataResult},
    syscall::{Host, Opcode},
    wasm::Runtime,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use wasmtime::Store;

#[derive(Default)]
struct Records {
    calls: Vec<Arc<DataCall>>,
    results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
    released: Vec<u64>,
}
struct KvHost(Arc<Mutex<Records>>);
impl Host for KvHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::KvKeysBegin,
            Opcode::KvKeysTake,
            Opcode::KvReadBegin,
            Opcode::KvReadTake,
            Opcode::KvPutBegin,
            Opcode::KvPutTake,
            Opcode::KvDeleteBegin,
            Opcode::KvDeleteTake,
        ]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        records.calls.push(call);
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        self.0.lock().unwrap().results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        self.0.lock().unwrap().released.push(token);
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (memory (export "memory") 2 4)
    (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
        (call $call (local.get 0) (local.get 1) (local.get 2) (local.get 3)
            (local.get 4) (local.get 5) (local.get 6))))"#;

#[test]
fn wasm_kv_checks_regions_copies_inputs_and_retains_short_completions() {
    for backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(*backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime.compile_app(&wat::parse_str(MODULE).unwrap()).unwrap();
        let records = Arc::new(Mutex::new(Records::default()));
        let state =
            WasmState::new(context(), Box::new(KvHost(records.clone())), runtime.fuel_limit)
                .unwrap();
        let scoped = state.app.volume_handle([6; 16]).unwrap();
        let mut store = Store::new(&runtime.engine, state);
        store.set_fuel(TEST_FUEL).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let invoke = instance
            .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut store, "invoke")
            .unwrap();
        memory.data_mut(&mut store)[8..12].copy_from_slice(&32_u32.to_le_bytes());
        memory.data_mut(&mut store)[16..20].copy_from_slice(&scoped.slot.to_le_bytes());
        memory.data_mut(&mut store)[20..24].copy_from_slice(&scoped.generation.to_le_bytes());
        memory.data_mut(&mut store)[24..39].fill(7);
        memory.data_mut(&mut store)[128..131].copy_from_slice(&[0, 255, 1]);
        memory.data_mut(&mut store)[192..195].copy_from_slice(&[0, 128, 255]);
        assert!(records.lock().unwrap().calls.is_empty());
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x144, 8, 128, 3, 192, 3, 0)).unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let handle = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        memory.data_mut(&mut store)[128..131].fill(9);
        memory.data_mut(&mut store)[192..195].fill(9);
        assert_eq!(
            records.lock().unwrap().calls[0].operation,
            DataOperation::KvPut {
                entry_id: [7; 15],
                key: vec![0, 255, 1],
                value: vec![0, 128, 255]
            }
        );
        assert_eq!(invoke.call(&mut store, (0x145, handle, 0, 0, 0, 0, 0)).unwrap(), -6);
        records.lock().unwrap().results.insert(1, Ok(Arc::new(DataResult::KvWritten)));
        assert_eq!(invoke.call(&mut store, (0x145, handle, 0, 0, 0, 0, 0)).unwrap(), 0);
        memory.data_mut(&mut store)[48..52].copy_from_slice(&56u32.to_le_bytes());
        memory.data_mut(&mut store)[96..100].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x140, 8, 48, 0, 0, 0, 0)).unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let handle = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        let root = crate::store::key_value::Root { level: 0, digest: [3; 32] };
        records.lock().unwrap().results.insert(
            2,
            Ok(Arc::new(DataResult::KvKeys(crate::store::key_value::Page {
                root: Some(root),
                next: None,
                keys: vec![crate::store::key_value::Key {
                    bytes: vec![0, 255, 1],
                    len_bytes: 3,
                    preview: vec![],
                }],
            }))),
        );
        assert_eq!(invoke.call(&mut store, (0x141, handle, 640, 4, 520, 0, 0)).unwrap(), -11);
        assert_eq!(u32::from_le_bytes(memory.data(&store)[520..524].try_into().unwrap()), 53);
        assert_eq!(records.lock().unwrap().released, vec![1]);
        memory.grow(&mut store, 1).unwrap();
        assert_eq!(invoke.call(&mut store, (0x141, handle, 640, 64, 520, 0, 0)).unwrap(), 0);
        let page = playbit_api::abi::kv::decode_page(&memory.data(&store)[640..693]).unwrap();
        assert_eq!(
            page.keys.collect::<Vec<_>>(),
            [playbit_api::abi::kv::Key { bytes: &[0, 255, 1], len_bytes: 3 }]
        );
        memory.data_mut(&mut store)[48..112].fill(0);
        memory.data_mut(&mut store)[48..52].copy_from_slice(&64u32.to_le_bytes());
        memory.data_mut(&mut store)[64..68].copy_from_slice(&3u32.to_le_bytes());
        memory.data_mut(&mut store)[128..131].copy_from_slice(&[0, 255, 1]);
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x142, 8, 128, 3, 48, 0, 0)).unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let handle = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        records.lock().unwrap().results.insert(3, Err(HostError::AccessDenied));
        memory.data_mut(&mut store)[640..704].fill(0xa5);
        assert_eq!(invoke.call(&mut store, (0x143, handle, 640, 64, 520, 0, 0)).unwrap(), -2);
        assert_eq!(&memory.data(&store)[640..704], &[0xa5; 64]);
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x146, 8, 128, 3, 0, 0, 0)).unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let handle = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        records.lock().unwrap().results.insert(4, Ok(Arc::new(DataResult::KvDeleted)));
        assert_eq!(invoke.call(&mut store, (0x147, handle, 0, 0, 0, 0, 0)).unwrap(), 0);
        assert_eq!(records.lock().unwrap().released, vec![1, 2, 3, 4]);
    }
}
