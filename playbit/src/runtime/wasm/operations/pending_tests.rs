#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::tests::{backends, context};
use super::*;
use crate::runtime::{
    TEST_FUEL,
    syscall::{FileCopyRequest, FileSelection, Host, Opcode, VolumeRights},
    wasm::{ExecutionBackend, Runtime},
};
use std::sync::{Arc, Mutex};
use wasmtime::{Instance, Store};
struct Results {
    starts: [usize; 3],
    polls: [usize; 3],
    ready: bool,
    error: Option<HostError>,
    copy_id: [u8; 15],
    selection: Option<FileSelection>,
}
impl Default for Results {
    fn default() -> Self {
        Self {
            starts: [0; 3],
            polls: [0; 3],
            ready: false,
            error: None,
            copy_id: [10; 15],
            selection: Some(FileSelection {
                address: crate::runtime::host::EntryAddress {
                    volume_id: [6; 16],
                    entry_id: [5; 15],
                },
            }),
        }
    }
}
struct PendingHost(Arc<Mutex<Results>>);
impl PendingHost {
    fn begin(&self, kind: usize) -> HostResult<u64> {
        let mut r = self.0.lock().unwrap();
        r.starts[kind] += 1;
        Ok(r.starts[kind] as u64)
    }
    fn poll(&self, kind: usize) -> HostResult<()> {
        let mut r = self.0.lock().unwrap();
        r.polls[kind] += 1;
        if !r.ready {
            return Err(HostError::WouldBlock);
        }
        if let Some(error) = r.error {
            return Err(error);
        }
        Ok(())
    }
}
impl Host for PendingHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::FileCopyBegin,
            Opcode::FileCopyPoll,
            Opcode::FileSelectBegin,
            Opcode::FileSelectPoll,
        ]
    }
    fn begin_data(
        &mut self,
        _generation: u64,
        call: std::sync::Arc<crate::runtime::host::DataCall>,
    ) -> HostResult<u64> {
        let crate::runtime::host::DataOperation::Create { type_id, fields, .. } = &call.operation
        else {
            return Err(HostError::Unsupported);
        };
        let volume = call.volume;
        let ty = *type_id;
        let _ = fields;

        assert_eq!((volume, ty), ([6; 16], [8; 16]));
        self.begin(0)
    }
    fn peek_data(
        &mut self,
        _: u64,
    ) -> HostResult<std::sync::Arc<crate::runtime::host::DataResult>> {
        let result: HostResult<[u8; 16]> = {
            self.poll(0)?;
            Ok([9; 16])
        };
        result.map(|id| std::sync::Arc::new(crate::runtime::host::DataResult::Created(id)))
    }
    fn begin_file_copy(&mut self, _: u64, request: FileCopyRequest) -> HostResult<u64> {
        assert_eq!((request.source.entry_id, request.destination.entry_id), ([9; 15], [10; 15]));
        self.begin(1)
    }
    fn poll_file_copy(&mut self, _: u64) -> HostResult<[u8; 15]> {
        self.poll(1)?;
        Ok(self.0.lock().unwrap().copy_id)
    }
    fn begin_file_selection(&mut self, _: u64) -> HostResult<u64> {
        self.begin(2)
    }
    fn poll_file_selection(&mut self, _: u64) -> HostResult<Option<FileSelection>> {
        self.poll(2)?;
        Ok(self.0.lock().unwrap().selection)
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $dispatch_create (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_copy (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_select (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_create_poll (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_copy_poll (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_select_poll (param i32 i64 i64 i64 i64 i64 i64) (result i64)))

    ;; Test guests retain a handle returned in the scalar result for later exports.
    (func $save_handle (param $output i32) (param $result i64) (result i32)
        (if (result i32) (i64.lt_s (local.get $result) (i64.const 0))
            (then (i32.wrap_i64 (local.get $result)))
            (else (i64.store (local.get $output) (local.get $result)) (i32.const 0))))
    (func $create (param i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 2))) (call $dispatch_create (i32.const 276)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.const 0) (i64.const 0) (i64.const 40) (i64.const 0))))
    (func $copy (param i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 2))) (call $dispatch_copy (i32.const 278)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $select (param i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 0))) (call $dispatch_select (i32.const 290)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $create_poll (param i64 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_create_poll (i32.const 277)
            (i64.or (i64.shl (local.get 0) (i64.const 32)) (i64.extend_i32_u (local.get 1))) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $copy_poll (param i64 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_copy_poll (i32.const 279)
            (i64.or (i64.shl (local.get 0) (i64.const 32)) (i64.extend_i32_u (local.get 1))) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $select_poll (param i64 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_select_poll (i32.const 291)
            (i64.or (i64.shl (local.get 0) (i64.const 32)) (i64.extend_i32_u (local.get 1))) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (memory (export "memory") 1 256)
    (func (export "create") (param i32 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $create)
    (func (export "copy") (param i32 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $copy)
    (func (export "select") (param i32) (result i32) local.get 0 call $select)
    (func (export "create_poll") (param i64 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $create_poll)
    (func (export "copy_poll") (param i64 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $copy_poll)
    (func (export "select_poll") (param i64 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $select_poll))"#;
fn fixture(
    backend: ExecutionBackend,
    bytes: &[u8],
) -> (Store<WasmState>, Instance, Arc<Mutex<Results>>) {
    let runtime =
        Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
            .unwrap();
    let module = runtime.compile_app(bytes).unwrap();
    let results = Arc::new(Mutex::new(Results::default()));
    let state =
        WasmState::new(context(), Box::new(PendingHost(results.clone())), runtime.fuel_limit)
            .unwrap();
    let mut store = Store::new(&runtime.engine, state);
    store.set_fuel(TEST_FUEL).unwrap();
    let mut linker = Linker::new(&runtime.engine);
    define(&mut linker).unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    (store, instance, results)
}
fn inputs(store: &mut Store<WasmState>, instance: &Instance) {
    let volume = store.data().app.volume_handle([6; 16]).unwrap();
    let (_entry, _stale_entry) = super::tests::entry_pointers(&mut *store, instance, volume);
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    let data = memory.data_mut(store);
    data[32..48].fill(8);
    data[48..64].fill(7);
    for (base, object) in [(128, 9), (176, 10)] {
        data[base..base + 4].copy_from_slice(&32_u32.to_le_bytes());
        data[base + 4..base + 8].fill(0);
        data[base + 8..base + 12].copy_from_slice(&volume.slot.to_le_bytes());
        data[base + 12..base + 16].copy_from_slice(&volume.generation.to_le_bytes());
        data[base + 16..base + 31].fill(object);
        data[base + 31] = 0;
    }
    data[256..272].fill(0xcc);
    data[512..560].fill(0xcc);
    data[512..516].copy_from_slice(&40_u32.to_le_bytes());
    data[516..520].fill(0);
    data[65528..65532].copy_from_slice(&40_u32.to_le_bytes());
}
fn begin(store: &mut Store<WasmState>, instance: &Instance, kind: usize, output: u32) -> i32 {
    let volume = store.data().app.volume_handle([6; 16]).unwrap();
    let (_entry, _stale_entry) = super::tests::entry_pointers(&mut *store, instance, volume);
    match kind {
        0 => instance
            .get_typed_func::<(u32, u32, u32), i32>(&mut *store, "create")
            .unwrap()
            .call(store, (_entry, 32, output))
            .unwrap(),
        1 => instance
            .get_typed_func::<(u32, u32, u32), i32>(&mut *store, "copy")
            .unwrap()
            .call(store, (128, 176, output))
            .unwrap(),
        _ => instance
            .get_typed_func::<u32, i32>(&mut *store, "select")
            .unwrap()
            .call(store, output)
            .unwrap(),
    }
}
fn handle(store: &mut Store<WasmState>, instance: &Instance) -> (u64, u32) {
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    let data = memory.data(store);
    (
        u64::from(u32::from_le_bytes(data[260..264].try_into().unwrap())),
        u32::from_le_bytes(data[256..260].try_into().unwrap()),
    )
}
fn poll(
    store: &mut Store<WasmState>,
    instance: &Instance,
    kind: usize,
    handle: (u64, u32),
    output: u32,
) -> i32 {
    instance
        .get_typed_func::<(u64, u32, u32), i32>(
            &mut *store,
            ["create_poll", "copy_poll", "select_poll"][kind],
        )
        .unwrap()
        .call(store, (handle.0, handle.1, output))
        .unwrap()
}
#[test]
fn pending_take_output_admission_precedes_result_consumption() {
    for &backend in backends() {
        for kind in 0..3 {
            let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
            inputs(&mut store, &instance);
            let memory = instance.get_memory(&mut store, "memory").unwrap();

            assert_eq!(begin(&mut store, &instance, kind, 256), 0);
            let h = handle(&mut store, &instance);
            let before = memory.data(&store).to_vec();
            for output in [0, 65528] {
                assert_eq!(poll(&mut store, &instance, kind, h, output), -3);
            }
            if kind == 2 {
                assert_eq!(poll(&mut store, &instance, kind, h, 513), -3);
            }
            assert_eq!(host.lock().unwrap().polls, [0; 3]);
            assert_eq!(memory.data(&store), before);
            assert_eq!(poll(&mut store, &instance, kind, h, 512), HostError::WouldBlock as i32);
            assert_eq!(memory.data(&store), before);
            assert_eq!(
                store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
                1
            );
            assert_eq!(
                poll(&mut store, &instance, (kind + 1) % 3, h, 512),
                HostError::StaleHandle as i32
            );
            assert_eq!(
                poll(&mut store, &instance, kind, (h.0 + 1, h.1), 512),
                HostError::StaleHandle as i32
            );
            let epoch = u64::from(store.data().app.generation);
            match kind {
                0 => store.data_mut().app.notify_creation(epoch, 1),
                1 => store.data_mut().app.notify_file_copy(epoch, 1),
                _ => store.data_mut().app.notify_file_selection(epoch, 1),
            }
            .unwrap();
            assert_eq!(store.data().app.events.len(), 2);
            host.lock().unwrap().ready = true;
            assert_eq!(poll(&mut store, &instance, kind, h, 512), 0);
            assert!(store.data().app.capabilities.operations().next().is_none());
            assert_eq!(store.data().app.events.len(), 1);
            let data = memory.data(&store);
            if kind < 2 {
                let length = if kind == 0 { 16 } else { 15 };
                assert_eq!(&data[512..512 + length], &vec![if kind == 0 { 9 } else { 10 }; length]);
            } else {
                assert_eq!(&data[512..520], &[40, 0, 0, 0, 40, 0, 0, 0]);
                assert_eq!(&data[520..528], &[1, 0, 0, 0, 0, 0, 0, 0]);
                assert_eq!(&data[536..551], &[5; 15]);
                assert_eq!(data[551], 0);
            }
            let mut before = memory.data(&store).to_vec();
            if kind == 2 {
                before[516..520].fill(0);
            }
            assert_eq!(poll(&mut store, &instance, kind, h, 512), HostError::StaleHandle as i32);
            assert_eq!(memory.data(&store), before);
        }
    }
}
#[test]
fn terminal_errors_and_cancellation_consume_once_without_partial_outputs() {
    for &backend in backends() {
        for kind in 0..3 {
            let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
            inputs(&mut store, &instance);
            assert_eq!(begin(&mut store, &instance, kind, 256), 0);
            let h = handle(&mut store, &instance);
            let memory = instance.get_memory(&mut store, "memory").unwrap();
            let before = memory.data(&store).to_vec();
            {
                let mut r = host.lock().unwrap();
                r.ready = true;
                r.error = Some(HostError::AccessDenied);
            }
            assert_eq!(poll(&mut store, &instance, kind, h, 512), -2);
            assert_eq!(memory.data(&store), before);
            assert!(store.data().app.capabilities.operations().next().is_none());
            assert_eq!(poll(&mut store, &instance, kind, h, 512), HostError::StaleHandle as i32);
        }
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        inputs(&mut store, &instance);
        {
            let mut r = host.lock().unwrap();
            r.ready = true;
            r.selection = None;
        }
        assert_eq!(begin(&mut store, &instance, 2, 256), 0);
        let h = handle(&mut store, &instance);
        assert_eq!(poll(&mut store, &instance, 2, h, 512), 0);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        assert_eq!(&memory.data(&store)[512..520], &[40, 0, 0, 0, 40, 0, 0, 0]);
        assert_eq!(&memory.data(&store)[520..552], &[0; 32]);
    }
}
#[test]
fn malformed_or_unmounted_completions_do_not_confer_authority() {
    for &backend in backends() {
        for variant in 0..4 {
            let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
            inputs(&mut store, &instance);
            let kind = if variant == 0 { 1 } else { 2 };
            assert_eq!(begin(&mut store, &instance, kind, 256), 0);
            let h = handle(&mut store, &instance);
            {
                let mut r = host.lock().unwrap();
                r.ready = true;
                match variant {
                    0 => r.copy_id = [99; 15],
                    1 => r.selection.as_mut().unwrap().address.volume_id = [0; 16],
                    2 => r.selection.as_mut().unwrap().address.entry_id = [0; 15],
                    _ => r.selection.as_mut().unwrap().address.volume_id = [99; 16],
                }
            }
            let memory = instance.get_memory(&mut store, "memory").unwrap();
            let before = memory.data(&store).to_vec();
            assert_eq!(
                poll(&mut store, &instance, kind, h, 512),
                if variant == 3 { -2 } else { HostError::Internal as i32 }
            );
            assert_eq!(memory.data(&store), before);
            assert!(store.data().app.capabilities.operations().next().is_none());
            assert_eq!(store.data().app.context.volumes.len(), 1);
        }
    }
}
#[test]
fn accepted_mutation_receipts_survive_revocation_after_output_admission() {
    // Both Wasm backends retain pending accepted create/copy work through grant
    // withdrawal, while invalid outputs never inspect or consume the result.
    for &backend in backends() {
        for kind in 0..2 {
            let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
            inputs(&mut store, &instance);
            assert_eq!(begin(&mut store, &instance, kind, 256), 0);
            let h = handle(&mut store, &instance);
            super::tests::set_rights(&mut store, VolumeRights::READ);
            assert_eq!(poll(&mut store, &instance, kind, h, 0), -3);
            assert_eq!(
                store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
                1
            );
            let memory = instance.get_memory(&mut store, "memory").unwrap();
            let before = memory.data(&store).to_vec();
            assert_eq!(poll(&mut store, &instance, kind, h, 512), -6);
            assert_eq!(memory.data(&store), before);
            assert_eq!(
                store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
                1
            );
            host.lock().unwrap().ready = true;
            assert_eq!(poll(&mut store, &instance, kind, h, 512), 0);
            let length = if kind == 0 { 16 } else { 15 };
            assert_eq!(
                &memory.data(&store)[512..512 + length],
                &vec![if kind == 0 { 9 } else { 10 }; length]
            );
            if kind == 1 {
                assert_eq!(memory.data(&store)[527], before[527]);
            }
            assert_eq!(host.lock().unwrap().polls[kind], 2);
            assert!(store.data().app.capabilities.operations().next().is_none());
            assert_eq!(begin(&mut store, &instance, kind, 256), -2);
        }
    }
}
#[test]
fn pending_limit_is_shared_and_reused_slots_reject_old_handles() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        inputs(&mut store, &instance);
        let mut first = (0, 0);
        for index in 0..64 {
            assert_eq!(begin(&mut store, &instance, index % 3, 256), 0);
            if index == 0 {
                first = handle(&mut store, &instance);
            }
        }
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let before = memory.data(&store).to_vec();
        assert_eq!(begin(&mut store, &instance, 0, 256), -9);
        assert_eq!(memory.data(&store), before);
        assert_eq!(host.lock().unwrap().starts.iter().sum::<usize>(), 64);
        host.lock().unwrap().ready = true;
        assert_eq!(poll(&mut store, &instance, 0, first, 512), 0);
        assert_eq!(begin(&mut store, &instance, 0, 256), 0);
        let h = handle(&mut store, &instance);
        assert_eq!(h.1, first.1);
        assert_ne!(h.0, first.0);
        assert_eq!(poll(&mut store, &instance, 0, first, 512), HostError::StaleHandle as i32);
    }
}
#[test]
#[ignore = "requires independently compiled C and Rust wasm32 consumers"]
fn compiler_pending_records_reach_named_imports() {
    for language in ["c", "rust"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/wasm-lowering/pending-{language}.wasm"));
        compiled_pending_guest(language, &std::fs::read(path).unwrap());
    }
}

#[test]
fn frozen_v1_rc2_pending_guest_runs_on_current_runtime() {
    // Execute retained compiler output against today's runtime, without rebuilding it.
    for (language, bytes) in super::frozen_guest_tests::PENDING {
        compiled_pending_guest(language, bytes);
    }
}

fn compiled_pending_guest(language: &str, bytes: &[u8]) {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, bytes);
        host.lock().unwrap().ready = true;
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_entry, _stale_entry) = super::tests::entry_pointers(&mut store, &instance, volume);
        let call = instance.get_typed_func::<(u64, u32), u32>(&mut store, "probe_pending").unwrap();
        assert_eq!(
            call.call(&mut store, (u64::from(volume.generation), volume.slot)).unwrap(),
            0,
            "{backend:?} {language}"
        );
        assert_eq!(host.lock().unwrap().starts, [1; 3]);
        assert_eq!(host.lock().unwrap().polls, [1; 3]);
        assert!(store.data().app.capabilities.operations().next().is_none());
    }
}

#[test]
fn invalid_file_references_and_halted_instances_cannot_start_or_consume_work() {
    for &backend in backends() {
        let (mut store, instance, host) = fixture(backend, &wat::parse_str(MODULE).unwrap());
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        for base in [128, 176] {
            for offset in [8, 16] {
                inputs(&mut store, &instance);
                let data = memory.data_mut(&mut store);
                if offset == 8 {
                    data[base + offset] ^= 0x80;
                } else {
                    data[base + offset..base + offset + 15].fill(0);
                }
                let before = memory.data(&store).to_vec();
                assert_eq!(
                    begin(&mut store, &instance, 1, 256),
                    if offset == 8 { HostError::StaleHandle as i32 } else { -3 }
                );
                assert_eq!(memory.data(&store), before);
            }
        }
        assert_eq!(host.lock().unwrap().starts, [0; 3]);
        inputs(&mut store, &instance);
        assert_eq!(begin(&mut store, &instance, 0, 256), 0);
        let h = handle(&mut store, &instance);
        store.data_mut().app.halt();
        let before = memory.data(&store).to_vec();
        for kind in 0..3 {
            assert_eq!(begin(&mut store, &instance, kind, 256), HostError::StaleHandle as i32);
            assert_eq!(poll(&mut store, &instance, kind, h, 512), HostError::StaleHandle as i32);
        }
        assert_eq!(memory.data(&store), before);
        assert_eq!(host.lock().unwrap().starts, [1, 0, 0]);
        assert_eq!(host.lock().unwrap().polls, [0; 3]);
    }
}
