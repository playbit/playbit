#![allow(clippy::unwrap_used, reason = "Tests assert owned lookup and retained completions.")]
use super::tests::{backends, context};
use super::*;
use crate::runtime::{
    TEST_FUEL,
    host::{DataCall, DataOperation, DataResult},
    syscall::{Host, Opcode},
    wasm::Runtime,
};
use std::sync::{Arc, Mutex};
use wasmtime::Store;

#[derive(Default)]
struct Records {
    calls: Vec<Arc<DataCall>>,
    ready: bool,
    released: Vec<u64>,
}
struct EntryHost(Arc<Mutex<Records>>);
impl Host for EntryHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::EntryResolveBegin, Opcode::EntryResolveTake]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        records.calls.push(call);
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        assert_eq!(token, 1);
        if self.0.lock().unwrap().ready {
            Ok(Arc::new(DataResult::EntryResolved([7; 15])))
        } else {
            Err(HostError::WouldBlock)
        }
    }
    fn release_data(&mut self, token: u64) {
        self.0.lock().unwrap().released.push(token);
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (memory (export "memory") 1 1)
    (func (export "invoke") (param i32 i64 i64 i64 i64) (result i64)
        (call $call (local.get 0) (local.get 1) (local.get 2) (local.get 3)
            (local.get 4) (i64.const 0) (i64.const 0))))"#;

#[test]
fn wasm_lookup_owns_utf8_and_retains_the_complete_stable_capability() {
    for backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(*backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime.compile_app(&wat::parse_str(MODULE).unwrap()).unwrap();
        let records = Arc::new(Mutex::new(Records::default()));
        let state =
            WasmState::new(context(), Box::new(EntryHost(records.clone())), runtime.fuel_limit)
                .unwrap();
        let scoped = state.app.volume_handle([6; 16]).unwrap();
        let volume = playbit_api::abi::dispatch::pack_handle(scoped.generation, scoped.slot);
        let mut store = Store::new(&runtime.engine, state);
        store.set_fuel(TEST_FUEL).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let invoke = instance
            .get_typed_func::<(u32, u64, u64, u64, u64), i64>(&mut store, "invoke")
            .unwrap();
        // Maximum valid path, including UTF-8, must match native admission.
        let name = ("a".repeat(255) + "/").repeat(15) + &"b".repeat(253) + "/é";
        assert_eq!(name.len(), playbit_api::abi::files::MAX_FILE_PATH_BYTES);
        memory.data_mut(&mut store)[1024..1024 + name.len()].copy_from_slice(name.as_bytes());
        memory.data_mut(&mut store)[256..280].fill(0x55);
        {
            let (handle, output, expected) = (volume + 1, 256, -8);
            assert_eq!(
                super::tests::save_handle(
                    invoke.call(&mut store, (0x106, handle, 1024, name.len() as u64, 0)).unwrap(),
                    &mut store,
                    memory,
                    output
                ),
                expected
            );
        }
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x106, volume, 1024, 4097, 0)).unwrap(),
                &mut store,
                memory,
                256
            ),
            -9
        );
        assert!(records.lock().unwrap().calls.is_empty());
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x106, volume, 1024, name.len() as u64, 0)).unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let pending = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        memory.data_mut(&mut store)[1024..1024 + name.len()].fill(0);
        assert_eq!(
            records.lock().unwrap().calls[0].operation,
            DataOperation::ResolveEntry { name }
        );
        memory.data_mut(&mut store)[512..544].fill(0x55);
        memory.data_mut(&mut store)[512..516].copy_from_slice(&32_u32.to_le_bytes());
        memory.data_mut(&mut store)[516..520].fill(0);
        let before = memory.data(&store)[512..544].to_vec();
        assert_eq!(invoke.call(&mut store, (0x107, pending, 512, 0, 0)).unwrap(), -6);
        assert_eq!(&memory.data(&store)[512..544], before);
        records.lock().unwrap().ready = true;
        assert_eq!(invoke.call(&mut store, (0x107, pending, 513, 0, 0)).unwrap(), -3);
        assert!(records.lock().unwrap().released.is_empty());
        assert_eq!(invoke.call(&mut store, (0x107, pending, 512, 0, 0)).unwrap(), 0);
        let result = &memory.data(&store)[512..544];
        assert_eq!(&result[..4], &32_u32.to_le_bytes());
        assert_eq!(&result[4..8], &32_u32.to_le_bytes());
        assert_eq!(&result[8..12], &scoped.slot.to_le_bytes());
        assert_eq!(&result[12..16], &scoped.generation.to_le_bytes());
        assert_eq!(&result[16..31], &[7; 15]);
        assert_eq!(result[31], 0);
        assert_eq!(records.lock().unwrap().released, [1]);
        assert_eq!(invoke.call(&mut store, (0x107, pending, 512, 0, 0)).unwrap(), -8);
    }
}
