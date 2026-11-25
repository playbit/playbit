#![allow(clippy::unwrap_used, reason = "Tests exercise the actual Wasmtime syscall adapters.")]
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
struct BloblogHost(Arc<Mutex<Records>>);
impl Host for BloblogHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::BloblogAppendBegin,
            Opcode::BloblogAppendTake,
            Opcode::BloblogReadBegin,
            Opcode::BloblogReadTake,
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

#[test]
fn wasm_bloblog_admits_owned_batches_and_retains_completions_across_memory_growth() {
    // Both Wasm backends enforce bounds/aliasing and consume only the matching completion.
    for backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(*backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime
            .compile_app(
                &wat::parse_str(
                    r#"(module
            (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64)
                (result i64)))
            (memory (export "memory") 2 4)
            (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
                (call $call (local.get 0) (local.get 1) (local.get 2) (local.get 3)
                    (local.get 4) (local.get 5) (local.get 6))))"#,
                )
                .unwrap(),
            )
            .unwrap();
        let records = Arc::new(Mutex::new(Records::default()));
        let state =
            WasmState::new(context(), Box::new(BloblogHost(records.clone())), runtime.fuel_limit)
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
        let volume = playbit_api::abi::dispatch::pack_handle(scoped.generation, scoped.slot);
        memory.data_mut(&mut store)[16..31].fill(7);
        memory.data_mut(&mut store)[32..48].fill(1);
        // The second record has an extension; stride padding is not a field.
        memory.data_mut(&mut store)[64..68].copy_from_slice(&16u32.to_le_bytes());
        memory.data_mut(&mut store)[72..76].copy_from_slice(&128u32.to_le_bytes());
        memory.data_mut(&mut store)[76..80].copy_from_slice(&3u32.to_le_bytes());
        memory.data_mut(&mut store)[80..88].fill(0xa5);
        memory.data_mut(&mut store)[88..92].copy_from_slice(&24u32.to_le_bytes());
        memory.data_mut(&mut store)[128..131].copy_from_slice(&[0, 255, 128]);
        for (count, stride, output, expected) in [
            (129, 24, 256, -9),
            (0, 24, 256, -3),
            (2, 12, 256, -3),
            (2, 18, 256, -3),
            (2, 4097, 256, -9),
        ] {
            assert_eq!(
                super::tests::save_handle(
                    invoke.call(&mut store, (0x170, volume, 16, 64, count, stride, 32)).unwrap(),
                    &mut store,
                    memory,
                    output
                ),
                expected
            );
        }
        for (size, expected) in [(15u32, -3), (25, -3), (4097, -3)] {
            memory.data_mut(&mut store)[88..92].copy_from_slice(&size.to_le_bytes());
            assert_eq!(
                super::tests::save_handle(
                    invoke.call(&mut store, (0x170, volume, 16, 64, 2, 24, 32)).unwrap(),
                    &mut store,
                    memory,
                    256
                ),
                expected
            );
        }
        memory.data_mut(&mut store)[88..92].copy_from_slice(&24u32.to_le_bytes());
        memory.data_mut(&mut store)[104] = 1;
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x170, volume, 16, 64, 2, 24, 32)).unwrap(),
                &mut store,
                memory,
                256
            ),
            -7
        );
        assert!(records.lock().unwrap().calls.is_empty());
        assert_eq!(&memory.data(&store)[256..264], &[0; 8]);
        memory.data_mut(&mut store)[104] = 0;
        assert_eq!(
            super::tests::save_handle(
                invoke.call(&mut store, (0x170, volume, 16, 64, 2, 24, 32)).unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let handle = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        memory.data_mut(&mut store)[128..131].fill(9);
        assert_eq!(
            records.lock().unwrap().calls[0].operation,
            DataOperation::BloblogAppend {
                append_id: [1; 16],
                log: [7; 15],
                blobs: vec![vec![0, 255, 128], vec![]]
            }
        );
        assert_eq!(invoke.call(&mut store, (0x171, handle, 640, 1, 520, 0, 0)).unwrap(), -6);
        let cursor = crate::store::bloblog::Cursor::parse(&format!(
            "1/{}/{}/{}/0/{}",
            crate::store::Id::from_bytes([6; 16]),
            crate::store::EntryId::from_bytes([7; 15]).unwrap(),
            crate::identifier::encode(&[1; 8]),
            crate::store::Id::from_bytes([0; 16])
        ))
        .unwrap();
        records
            .lock()
            .unwrap()
            .results
            .insert(1, Ok(Arc::new(DataResult::BloblogAppended(cursor.clone()))));
        assert_eq!(invoke.call(&mut store, (0x173, handle, 640, 104, 520, 0, 0)).unwrap(), -8);
        assert_eq!(invoke.call(&mut store, (0x171, handle, 640, 1, 520, 0, 0)).unwrap(), -11);
        assert!(records.lock().unwrap().released.is_empty());
        assert_eq!(
            u32::from_le_bytes(memory.data(&store)[520..524].try_into().unwrap()) as usize,
            cursor.as_str().len()
        );
        memory.grow(&mut store, 1).unwrap();
        assert_eq!(invoke.call(&mut store, (0x171, handle, 640, 104, 640, 0, 0)).unwrap(), -3);
        assert!(records.lock().unwrap().released.is_empty());
        assert_eq!(invoke.call(&mut store, (0x171, handle, 640, 104, 520, 0, 0)).unwrap(), 0);
        assert_eq!(
            &memory.data(&store)[640..640 + cursor.as_str().len()],
            cursor.as_str().as_bytes()
        );
        assert_eq!(
            super::tests::save_handle(
                invoke
                    .call(&mut store, (0x172, volume, 16, 640, cursor.as_str().len() as u64, 0, 0))
                    .unwrap(),
                &mut store,
                memory,
                256
            ),
            0
        );
        let handle = u64::from_le_bytes(memory.data(&store)[256..264].try_into().unwrap());
        records.lock().unwrap().results.insert(
            2,
            Ok(Arc::new(DataResult::BloblogPage(crate::store::bloblog::Page {
                entries: vec![],
                cursor: cursor.clone(),
                more: false,
            }))),
        );
        assert_eq!(invoke.call(&mut store, (0x173, handle, 640, 104, 520, 0, 0)).unwrap(), 0);
        let length = u32::from_le_bytes(memory.data(&store)[520..524].try_into().unwrap()) as usize;
        let page = playbit_api::abi::bloblog::decode_page(&memory.data(&store)[640..640 + length])
            .unwrap();
        assert_eq!(page.cursor, cursor.as_str());
        assert_eq!(page.entries.len(), 0);
        assert_eq!(records.lock().unwrap().released, [1, 2]);
    }
}
