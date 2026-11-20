#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::runtime::host::{DataCall, DataResult};
use crate::runtime::{
    TEST_FUEL,
    syscall::{
        AvailableVolume, Event, FieldChange, GuestContext, Host, Id, Identity, Opcode, Value,
        VolumeRights,
    },
    wasm::{ExecutionBackend, Runtime},
};
use playbit_api::abi::syscall::EventKind;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use wasmtime::{Instance, Store};

#[derive(Default)]
struct Effects {
    calls: usize,
    changes: Vec<FieldChange>,
    admitted: Vec<Arc<DataCall>>,
    results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
}
struct RecordingHost(Arc<Mutex<Effects>>);
impl Host for RecordingHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
        ]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut effects = self.0.lock().unwrap();
        effects.admitted.push(call);
        Ok(effects.admitted.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        self.0.lock().unwrap().results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        self.0.lock().unwrap().results.remove(&token);
    }
    fn create_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        ty: Id,
        changes: &[FieldChange],
    ) -> HostResult<Id> {
        assert_eq!(volume.volume_id, [6; 16]);
        assert_eq!(ty, [8; 16]);
        self.edit_fields(volume, [9; 16], changes)?;
        Ok([9; 16])
    }
    fn edit_fields(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object: Id,
        changes: &[FieldChange],
    ) -> HostResult<()> {
        assert_eq!(volume.volume_id, [6; 16]);
        assert_eq!(object, [9; 16]);
        let mut effects = self.0.lock().unwrap();
        effects.calls += 1;
        effects.changes = changes.to_vec();
        Ok(())
    }
}
pub(super) fn entry_pointers(
    store: &mut Store<WasmState>,
    instance: &Instance,
    handle: playbit_api::abi::syscall::VolumeHandle,
) -> (u32, u32) {
    entry_pointers_at(store, instance, handle, 60000)
}

pub(super) fn entry_pointers_at(
    store: &mut Store<WasmState>,
    instance: &Instance,
    handle: playbit_api::abi::syscall::VolumeHandle,
    pointer: usize,
) -> (u32, u32) {
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    let required = pointer.checked_add(64).unwrap();
    if memory.data_size(&*store) < required {
        let pages = (required - memory.data_size(&*store)).div_ceil(65536);
        memory.grow(&mut *store, pages as u64).unwrap();
    }
    let data = memory.data_mut(store);
    for (offset, generation) in
        [(pointer, handle.generation), (pointer + 32, handle.generation + 1)]
    {
        data[offset..offset + 4].copy_from_slice(&32_u32.to_le_bytes());
        data[offset + 4..offset + 8].fill(0);
        data[offset + 8..offset + 12].copy_from_slice(&handle.slot.to_le_bytes());
        data[offset + 12..offset + 16].copy_from_slice(&generation.to_le_bytes());
        data[offset + 16..offset + 31].fill(5);
        data[offset + 31] = 0;
    }
    (pointer as u32, (pointer + 32) as u32)
}

pub(super) fn save_handle(
    result: i64,
    store: &mut Store<WasmState>,
    memory: wasmtime::Memory,
    output: u64,
) -> i64 {
    if result > 0 {
        memory.write(store, output as usize, &(result as u64).to_le_bytes()).unwrap();
        0
    } else {
        result
    }
}

pub(super) fn set_rights(store: &mut Store<WasmState>, rights: VolumeRights) {
    let state = &mut store.data_mut().app;
    state.context.volumes[0].rights = rights;
    let volume = state.context.volumes[0].clone();
    let handle = state.volume_handle(volume.id).unwrap();
    state
        .capabilities
        .replace_volume(crate::capability::Handle::new(handle.generation, handle.slot), volume)
        .unwrap();
}

pub(super) fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [1; 16],
            is_service: false,
            device_id: [2; 16],
            instance_id: [3; 16],
            session_id: Some([4; 32]),
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [6; 16],
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        }],
        startup_arguments: Default::default(),
        trace_catalog: None,
        required_capabilities: Default::default(),
        participants: vec![],
    }
}
const MODULE: &str = r#"(module
    (import "playbit" "pb_syscall" (func $dispatch_platform (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_log (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_view (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_event (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_identity (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_wake (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_defer (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_create (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_edit (param i32 i64 i64 i64 i64 i64 i64) (result i64)))

    ;; Test guests retain a handle returned in the scalar result for later exports.
    (func $save_handle (param $output i32) (param $result i64) (result i32)
        (if (result i32) (i64.lt_s (local.get $result) (i64.const 0))
            (then (i32.wrap_i64 (local.get $result)))
            (else (i64.store (local.get $output) (local.get $result)) (i32.const 0))))
    (func $platform  (result i32)
        (i32.wrap_i64 (call $dispatch_platform (i32.const 12)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $log (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_log (i32.const 2)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $view (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_view (i32.const 3)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $event (param i32 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_event (i32.const 4)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $identity (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_identity (i32.const 5)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $wake (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_wake (i32.const 11)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $defer (param i64) (result i32)
        (i32.wrap_i64 (call $dispatch_defer (i32.const 9)
            (local.get 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $create_begin (param i32 i32 i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 4))) (call $dispatch_create (i32.const 276)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.extend_i32_u (local.get 2)) (i64.extend_i32_u (local.get 3))
            (i64.const 40) (i64.const 0))))
    (func $edit_begin (param i32 i32 i32 i32 i32) (result i32)
        (call $save_handle (i32.wrap_i64 (i64.extend_i32_u (local.get 4))) (call $dispatch_edit (i32.const 306)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1))
            (i64.extend_i32_u (local.get 2)) (i64.extend_i32_u (local.get 3))
            (i64.const 40) (i64.const 0))))
    (memory (export "memory") 1 256)
    (func (export "platform") (result i32) call $platform)
    (func (export "pb_main"))
    (func (export "log") (param i32 i32) (result i32) local.get 0 local.get 1 call $log)
    (func (export "view") (param i32 i32) (result i32) local.get 0 local.get 1 call $view)
    (func (export "event") (param i32 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $event)
    (func (export "identity") (param i32) (result i32) local.get 0 call $identity)
    (func (export "wake") (param i32) (result i32) local.get 0 call $wake)
    (func (export "defer") (param i64) (result i32) local.get 0 call $defer)
    (func (export "create_begin") (param i32 i32 i32 i32 i32) (result i32)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 call $create_begin)
    (func (export "edit_begin") (param i32 i32 i32 i32 i32) (result i32)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 call $edit_begin)
    (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
        local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6
        call $dispatch_create))"#;

fn fixture(backend: ExecutionBackend) -> (Store<WasmState>, Instance, Arc<Mutex<Effects>>) {
    let runtime =
        Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
            .unwrap();
    let module = runtime.compile_app(&wat::parse_str(MODULE).unwrap()).unwrap();
    let mut linker = Linker::new(&runtime.engine);
    define(&mut linker).unwrap();
    let effects = Arc::new(Mutex::new(Effects::default()));
    let mut store = Store::new(
        &runtime.engine,
        WasmState::new(context(), Box::new(RecordingHost(effects.clone())), runtime.fuel_limit)
            .unwrap(),
    );
    store.set_fuel(TEST_FUEL).unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    (store, instance, effects)
}

#[test]
fn audio_admits_pcm_before_effects() {
    // Both executors reject invalid pointers and excessive PCM before
    // unsupported host admission; the borrowed source stays intact.
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.data_mut(&mut store)[64..72].fill(0xaa);
        let invoke = instance
            .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut store, "invoke")
            .unwrap();
        for (source, channels, frames, expected) in
            [(65535, 2, 1, -3), (128, 2, u32::MAX, -9), (128, 0, 1, -3), (128, 1, 1, -7)]
        {
            assert_eq!(
                invoke
                    .call(&mut store, (0x1a6, 0, channels, 44_100, source, frames as u64, 0))
                    .unwrap(),
                expected
            );
            assert_eq!(&memory.data(&store)[64..72], &[0xaa; 8]);
        }
        for (op, args, expected) in [
            (0x1a0, [48_000, 65, 0, 0, 0, 0], -3),
            (0x1a0, [48_000, 0, 0, 0, 0, 0], -7),
            (0x1a1, [0, 65, 0, 0, 0, 0], -3),
            (0x1a9, [0, 1234, 65, 0, 0, 0], -3),
        ] {
            let [a, b, c, d, e, f] = args;
            assert_eq!(invoke.call(&mut store, (op, a, b, c, d, e, f)).unwrap(), expected);
            assert_eq!(&memory.data(&store)[64..72], &[0xaa; 8]);
        }
    }
}
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
#[test]
fn audio_dispatch_owns_resources_and_controls_independent_instances_on_both_backends() {
    // Real Wasm calls exercise successful adapter lowering and typed capability
    // checks. A prepared open result avoids initializing any output device.
    use crate::runtime::{audio, audio_handles};
    use playbit_api::abi::{audio as contract, dispatch::pack_handle};
    let _serial = audio::TEST_SERIAL.lock().unwrap();
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let operation = audio_handles::prepare_open(&mut store.data_mut().app, 44_100).unwrap();
        let operation = pack_handle(operation.generation, operation.slot);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let invoke = instance
            .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut store, "invoke")
            .unwrap();
        let call = |store: &mut Store<WasmState>, opcode: Opcode, [a, b, c, d, e, f]: [u64; 6]| {
            invoke.call(store, (opcode as u32, a, b, c, d, e, f)).unwrap()
        };
        let read = |store: &Store<WasmState>, offset: usize| {
            u64::from_le_bytes(memory.data(store)[offset..offset + 8].try_into().unwrap())
        };
        memory.data_mut(&mut store)[64..88].fill(0xaa);
        assert_eq!(call(&mut store, Opcode::AudioOpenTake, [operation, 65, 0, 0, 0, 0]), -3);
        assert_eq!(read(&store, 64), 0xaaaaaaaaaaaaaaaa);
        let output = call(&mut store, Opcode::AudioOpenTake, [operation, 0, 0, 0, 0, 0]);
        assert!(output > 0);
        let output = output as u64;
        assert_eq!(call(&mut store, Opcode::AudioOpenTake, [operation, 0, 0, 0, 0, 0]), -8);
        assert_eq!(read(&store, 64), 0xaaaaaaaaaaaaaaaa);
        assert_eq!(call(&mut store, Opcode::AudioSampleRate, [output, 0, 0, 0, 0, 0]), 44_100);
        assert_eq!(
            call(
                &mut store,
                Opcode::AudioSetVolume,
                [output, (-0.0_f32).to_bits() as u64, 0, 0, 0, 0]
            ),
            0
        );
        assert_eq!(call(&mut store, Opcode::AudioVolume, [output, 0, 0, 0, 0, 0]), 0);
        assert_eq!(
            call(
                &mut store,
                Opcode::AudioSetVolume,
                [output, 0.5_f32.to_bits() as u64, 0, 0, 0, 0]
            ),
            0
        );
        assert_eq!(
            call(&mut store, Opcode::AudioVolume, [output, 0, 0, 0, 0, 0]),
            0.5_f32.to_bits() as i64
        );
        assert_eq!(
            call(
                &mut store,
                Opcode::AudioSetVolume,
                [output, f32::NAN.to_bits() as u64, 0, 0, 0, 0]
            ),
            -3
        );
        let sound = call(&mut store, Opcode::SoundOpenPcm, [output, 2, 44_100, 128, 4, 0]);
        assert!(sound > 0);
        let sound = sound as u64;
        assert_eq!(call(&mut store, Opcode::SoundOpenPcm, [sound, 2, 44_100, 128, 4, 0]), -8);
        assert_eq!(read(&store, 72), 0xaaaaaaaaaaaaaaaa);
        assert_eq!(call(&mut store, Opcode::AudioSampleRate, [sound, 0, 0, 0, 0, 0]), -8);
        for id in [1, 1234, 0x80000000] {
            assert_eq!(
                call(
                    &mut store,
                    Opcode::SoundPlay,
                    [
                        sound,
                        id,
                        contract::UNITY_GAIN as u64,
                        0,
                        90_703,
                        contract::SOUND_LOOP as u64
                    ]
                ),
                0
            );
        }
        for length in [0, 1] {
            assert_eq!(call(&mut store, Opcode::SoundPlay, [sound, 2, 65_536, 0, length, 0]), -3);
        }
        assert_eq!(call(&mut store, Opcode::SoundPlay, [sound, 2, 65_536, 0, 90_703, 2]), -3);
        assert_eq!(call(&mut store, Opcode::SoundSetGain, [sound, 1, 16_384, 0, 0, 0]), 0);
        assert_eq!(
            call(
                &mut store,
                Opcode::SoundSetBalance,
                [sound, 1, (-0.5_f32).to_bits() as u64, 0, 0, 0]
            ),
            0
        );
        assert_eq!(call(&mut store, Opcode::SoundSeek, [sound, 1, 45_352, 0, 0, 0]), 0);
        assert_eq!(call(&mut store, Opcode::SoundTell, [sound, 1, 80, 0, 0, 0]), 0);
        assert_eq!(read(&store, 80), 45_352);
        assert_eq!(call(&mut store, Opcode::SoundTell, [sound, 1234, 80, 0, 0, 0]), 0);
        assert_eq!(read(&store, 80), 0);
        assert_eq!(call(&mut store, Opcode::SoundSeek, [sound, 1, 90_703, 0, 0, 0]), 0);
        assert_eq!(call(&mut store, Opcode::SoundTell, [sound, 1, 80, 0, 0, 0]), 0);
        assert_eq!(read(&store, 80), 90_703);
        assert_eq!(call(&mut store, Opcode::SoundSeek, [sound, 1, 113_379, 0, 0, 0]), -3);
        assert_eq!(call(&mut store, Opcode::SoundSetGain, [sound, 1, 65_537, 0, 0, 0]), -3);
        assert_eq!(
            call(
                &mut store,
                Opcode::SoundSetBalance,
                [sound, 1, f32::NAN.to_bits() as u64, 0, 0, 0]
            ),
            -3
        );
        memory.data_mut(&mut store)[80..88].fill(0xaa);
        assert_eq!(call(&mut store, Opcode::SoundTell, [sound, 1, 81, 0, 0, 0]), -3);
        assert_eq!(call(&mut store, Opcode::SoundTell, [sound, 456, 80, 0, 0, 0]), -3);
        assert_eq!(call(&mut store, Opcode::SoundTell, [output, 1, 80, 0, 0, 0]), -8);
        assert_eq!(read(&store, 80), 0xaaaaaaaaaaaaaaaa);
        for id in [1, 1234] {
            assert_eq!(call(&mut store, Opcode::SoundStop, [sound, id, 0, 0, 0, 0]), 0);
            assert_eq!(call(&mut store, Opcode::SoundTell, [sound, id, 80, 0, 0, 0]), -3);
        }
        assert_eq!(call(&mut store, Opcode::SoundClose, [sound, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(&mut store, Opcode::SoundTell, [sound, 0x80000000, 80, 0, 0, 0]), -8);
        let child = call(&mut store, Opcode::SoundOpenPcm, [output, 2, 44_100, 128, 4, 0]);
        assert!(child > 0);
        let child = child as u64;
        assert_ne!(child, sound);
        assert_eq!(call(&mut store, Opcode::SoundPlay, [child, 1234, 65_536, 0, 90_703, 0]), 0);
        assert_eq!(call(&mut store, Opcode::AudioClose, [output, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(&mut store, Opcode::AudioSampleRate, [output, 0, 0, 0, 0, 0]), -8);
        assert_eq!(call(&mut store, Opcode::AudioVolume, [output, 0, 0, 0, 0, 0]), -8);
        assert_eq!(call(&mut store, Opcode::SoundTell, [child, 1234, 80, 0, 0, 0]), -8);
        assert_eq!(call(&mut store, Opcode::SoundClose, [child, 0, 0, 0, 0, 0]), -8);
        assert_eq!(read(&store, 80), 0xaaaaaaaaaaaaaaaa);
    }
}

fn word(memory: &mut [u8], offset: usize, value: u32) {
    memory[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn field(memory: &mut [u8]) {
    memory[32..48].fill(8); // Type ID.
    word(memory, 64, 40);
    memory[72..88].fill(7); // Field ID.
    word(memory, 88, 128);
    word(memory, 92, 1);
    word(memory, 96, 80);
    word(memory, 100, 1);
    word(memory, 128, 80);
    word(memory, 136, 1); // Text.
    word(memory, 140, 3);
    word(memory, 200, 256);
    memory[256..259].copy_from_slice(b"abc");
}
pub(super) fn backends() -> &'static [ExecutionBackend] {
    if cfg!(target_os = "ios") {
        &[ExecutionBackend::Pulley]
    } else {
        &[ExecutionBackend::Native, ExecutionBackend::Pulley]
    }
}

#[test]
fn named_control_imports_preserve_outputs_events_and_scoped_identity() {
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let identity = instance.get_typed_func::<u32, i32>(&mut store, "identity").unwrap();
        word(memory.data_mut(&mut store), 65528, 104);
        for offset in [0, 3, 65528, u32::MAX] {
            assert_eq!(identity.call(&mut store, offset).unwrap(), -3);
        }
        word(memory.data_mut(&mut store), 32, 104);
        assert_eq!(identity.call(&mut store, 32).unwrap(), 0);
        assert_eq!(&memory.data(&store)[48..64], &[1; 16]);
        assert_eq!(&memory.data(&store)[96..128], &[4; 32]);
        let generation = store.data().app.generation;
        assert_eq!(
            u64::from_le_bytes(memory.data(&store)[40..48].try_into().unwrap()),
            u64::from(generation)
        );
        store.data_mut().app.events.clear();
        store.data_mut().app.queued_bytes = 0;
        crate::runtime::syscall::push_event(
            &mut store.data_mut().app,
            Event { kind: EventKind::DataChanged, target: 7, data: b"hello".to_vec() },
        )
        .unwrap();
        let event = instance.get_typed_func::<(u32, u32, u32), i32>(&mut store, "event").unwrap();
        word(memory.data_mut(&mut store), 32, 24);
        word(memory.data_mut(&mut store), 36, 0);
        let before = memory.data(&store).to_vec();
        for args in [(32, 36, 8), (0, 128, 8), (32, 65534, 8), (32, 128, 65537)] {
            assert!(event.call(&mut store, args).unwrap() < 0);
            assert_eq!(memory.data(&store), before);
            assert_eq!(store.data().app.events.len(), 1);
        }
        assert_eq!(event.call(&mut store, (32, 128, 4)).unwrap(), 5);
        assert_eq!(memory.data(&store), before);
        assert_eq!(event.call(&mut store, (32, 128, 5)).unwrap(), 5);
        assert!(store.data().app.events.is_empty());
        assert_eq!(&memory.data(&store)[128..133], b"hello");
        assert_eq!(event.call(&mut store, (32, 128, 5)).unwrap(), -6);
    }
}

fn complete(effects: &Arc<Mutex<Effects>>) {
    let (token, call) = {
        let records = effects.lock().unwrap();
        (records.admitted.len() as u64, records.admitted.last().unwrap().clone())
    };
    let result = call.as_ref().clone().execute(&mut RecordingHost(effects.clone())).map(Arc::new);
    assert!(effects.lock().unwrap().results.insert(token, result).is_none());
}
fn take(store: &mut Store<WasmState>, instance: Instance, offset: usize, create: bool) -> i64 {
    let memory = instance.get_memory(&mut *store, "memory").unwrap();
    let handle = u64::from_le_bytes(memory.data(&*store)[offset..offset + 8].try_into().unwrap());
    let opcode = if create { Opcode::ObjectCreatePoll } else { Opcode::ObjectEditTake };
    instance
        .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut *store, "invoke")
        .unwrap()
        .call(store, (opcode as u32, handle, if create { 544 } else { 0 }, 0, 0, 0, 0))
        .unwrap()
}
#[test]
fn nested_creation_and_edit_validate_before_async_admission_on_both_backends() {
    for &backend in backends() {
        let (mut store, instance, effects) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        field(memory.data_mut(&mut store));
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_entry, _stale_entry) = super::tests::entry_pointers(&mut store, &instance, volume);
        let create = instance
            .get_typed_func::<(u32, u32, u32, u32, u32), i32>(&mut store, "create_begin")
            .unwrap();

        word(memory.data_mut(&mut store), 200, 65535);
        assert_eq!(create.call(&mut store, (_entry, 32, 64, 1, 512)).unwrap(), -3);
        assert!(effects.lock().unwrap().admitted.is_empty());
        word(memory.data_mut(&mut store), 200, 256);
        assert_eq!(create.call(&mut store, (_entry, 32, 64, 1, 512)).unwrap(), 0);
        memory.data_mut(&mut store)[256..259].copy_from_slice(b"new");
        assert_eq!(take(&mut store, instance, 512, true), -6);
        assert_eq!(effects.lock().unwrap().calls, 0);
        complete(&effects);
        assert_eq!(take(&mut store, instance, 512, true), 0);
        assert_eq!(take(&mut store, instance, 512, true), -8);
        assert_eq!(&memory.data(&store)[544..560], &[9; 16]);
        assert_eq!(
            effects.lock().unwrap().changes[0].values,
            Some(vec![Value::Text("abc".into())])
        );
        let edit = instance
            .get_typed_func::<(u32, u32, u32, u32, u32), i32>(&mut store, "edit_begin")
            .unwrap();
        assert_eq!(edit.call(&mut store, (_entry, 544, 64, 1, 528)).unwrap(), 0);
        memory.data_mut(&mut store)[256..259].copy_from_slice(b"bad");
        assert_eq!(take(&mut store, instance, 528, false), -6);
        assert_eq!(effects.lock().unwrap().calls, 1);
        complete(&effects);
        assert_eq!(take(&mut store, instance, 528, false), 0);
        assert_eq!(take(&mut store, instance, 528, false), -8);
        assert_eq!(effects.lock().unwrap().calls, 2);
        assert_eq!(
            effects.lock().unwrap().changes[0].values,
            Some(vec![Value::Text("new".into())])
        );
    }
}

#[test]
fn stale_denied_unsupported_and_exhausted_calls_never_reach_the_host() {
    for &backend in backends() {
        let (mut store, instance, effects) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        field(memory.data_mut(&mut store));
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_entry, _stale_entry) = super::tests::entry_pointers(&mut store, &instance, volume);
        let create = instance
            .get_typed_func::<(u32, u32, u32, u32, u32), i32>(&mut store, "create_begin")
            .unwrap();
        assert_eq!(create.call(&mut store, (_stale_entry, 32, 64, 1, 512)).unwrap(), -8);
        super::tests::set_rights(&mut store, VolumeRights::READ);
        assert_eq!(create.call(&mut store, (_entry, 32, 64, 1, 512)).unwrap(), -2);
        store.data_mut().app.supported.remove(&Opcode::ObjectCreateBegin);
        assert_eq!(create.call(&mut store, (_entry, 32, 64, 1, 512)).unwrap(), -7);
        store.data_mut().app.supported.insert(Opcode::ObjectCreateBegin);
        store.set_fuel(1000).unwrap();
        let status = create.call(&mut store, (_entry, 32, 64, 1, 512));
        assert!(matches!(status, Ok(-9)) || status.is_err());
        assert_eq!(effects.lock().unwrap().calls, 0);
        assert_eq!(&memory.data(&store)[512..528], &[0; 16]);
    }
}

#[test]
fn logging_scene_wake_and_update_share_checked_control_semantics() {
    // Both Wasm executors validate inputs and retain exact output across a noisy turn.
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.write(&mut store, 32, b"hello").unwrap();
        let log = instance.get_typed_func::<(u32, u32), i32>(&mut store, "log").unwrap();
        assert_eq!(log.call(&mut store, (32, 5)).unwrap(), 0);
        for args in [(0, 1), (65535, 2), (u32::MAX, 1)] {
            assert_eq!(log.call(&mut store, args).unwrap(), -3);
        }
        memory.write(&mut store, 32, &[255]).unwrap();
        assert_eq!(log.call(&mut store, (32, 1)).unwrap(), -3);
        assert_eq!(store.data().app.logs, ["hello"]);
        memory.grow(&mut store, 2).unwrap();
        let message = "é".repeat(48 * 1024);
        memory.write(&mut store, 32, message.as_bytes()).unwrap();
        for _ in 0..2 {
            assert_eq!(log.call(&mut store, (32, message.len() as u32)).unwrap(), 0);
        }
        for _ in 0..1000 {
            assert_eq!(log.call(&mut store, (u32::MAX, 0)).unwrap(), 0);
        }
        assert_eq!(store.data().app.logs.len(), 1003);
        assert_eq!(store.data().app.logs[1..3], [message.clone(), message.clone()]);
        assert!(store.data().app.logs[3..].iter().all(String::is_empty));
        assert_eq!(&memory.data(&store)[32..32 + message.len()], message.as_bytes());
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.column_end();
        let scene = encoder.finish();
        memory.write(&mut store, 512, &scene).unwrap();
        let view = instance.get_typed_func::<(u32, u32), i32>(&mut store, "view").unwrap();
        assert_eq!(view.call(&mut store, (512, scene.len() as u32)).unwrap(), 0);
        let accepted = store.data().app.view.clone();
        assert_eq!(view.call(&mut store, (512, 3)).unwrap(), -3);
        assert_eq!(store.data().app.view, accepted);
        let wake = instance.get_typed_func::<u32, i32>(&mut store, "wake").unwrap();
        assert_eq!(wake.call(&mut store, 0).unwrap(), -3);
        assert!(store.data().app.wake_deadline.is_none());
        assert_eq!(wake.call(&mut store, playbit_api::abi::syscall::MIN_WAKE_DELAY_MS).unwrap(), 0);
        assert!(store.data().app.wake_deadline.is_some());
        store.data_mut().app.pending_update =
            Some(crate::runtime::syscall::PendingUpdate { generation: 7, build_id: [4; 32] });
        let defer = instance.get_typed_func::<u64, i32>(&mut store, "defer").unwrap();
        assert_eq!(defer.call(&mut store, 8).unwrap(), -10);
        assert_eq!(defer.call(&mut store, 7).unwrap(), 0);
        assert_eq!(store.data().app.deferred_update, Some(7));
        store.data_mut().app.halt();
        assert_eq!(log.call(&mut store, (512, 3)).unwrap(), -8);
    }
}

#[test]
fn clock_reads_preserve_raw_timestamps_and_host_call_fuel_without_memory() {
    let wasm = wat::parse_str(
        r#"(module
            (import "playbit" "pb_syscall"
                (func $syscall (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (func (export "clock") (result i64)
                i32.const 16
                i64.const 0 i64.const 0 i64.const 0
                i64.const 0 i64.const 0 i64.const 0
                call $syscall))"#,
    )
    .unwrap();
    for &backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime.compile_app(&wasm).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        for fuel in [500, 2000] {
            let effects = Arc::new(Mutex::new(Effects::default()));
            let mut store = Store::new(
                &runtime.engine,
                WasmState::new(
                    context(),
                    Box::new(RecordingHost(effects.clone())),
                    runtime.fuel_limit,
                )
                .unwrap(),
            );
            store.set_fuel(fuel).unwrap();
            let instance = linker.instantiate(&mut store, &module).unwrap();
            let clock = instance.get_typed_func::<(), u64>(&mut store, "clock").unwrap();
            let before = crate::trace::clock_ns();
            let timestamp = clock.call(&mut store, ()).unwrap();
            let after = crate::trace::clock_ns();
            assert!(timestamp > 0 && before <= timestamp && timestamp <= after);
            let remaining = store.get_fuel().unwrap();
            assert!(remaining <= fuel.saturating_sub(HOST_CALL_FUEL), "clock must charge fuel");
            if fuel == 500 {
                assert_eq!(remaining, 0);
                assert!(
                    clock.call(&mut store, ()).is_err(),
                    "exhausted fuel must trap on the next function entry"
                );
            }
            let effects = effects.lock().unwrap();
            assert_eq!(effects.calls, 0);
            assert!(effects.changes.is_empty() && effects.admitted.is_empty());
        }
    }
}

#[test]
fn nested_input_fuel_exhaustion_prevents_creation_after_outer_admission() {
    for &backend in backends() {
        let (mut store, instance, effects) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        field(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 140, 4096);
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_entry, _stale_entry) = super::tests::entry_pointers(&mut store, &instance, volume);
        let create = instance
            .get_typed_func::<(u32, u32, u32, u32, u32), i32>(&mut store, "create_begin")
            .unwrap();
        store.set_fuel(1100).unwrap(); // Outer records fit, the 4-KiB nested payload does not.
        let result = create.call(&mut store, (_entry, 32, 64, 1, 8192));
        assert!(matches!(result, Ok(-9)) || result.is_err());
        assert_eq!(effects.lock().unwrap().calls, 0);
        assert_eq!(&memory.data(&store)[8192..8208], &[0; 16]);
    }
}

#[test]
fn qualification_linker_rejects_shared_64bit_and_imported_memory() {
    for &backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        for declaration in
            ["(memory (export \"memory\") 1 256 shared)", "(memory (export \"memory\") i64 1)"]
        {
            let wasm =
                wat::parse_str(MODULE.replace("(memory (export \"memory\") 1 256)", declaration))
                    .unwrap();
            assert!(runtime.compile_app(&wasm).is_err());
        }
        let imported =
            MODULE.replace("(memory (export \"memory\") 1 256)", "(export \"memory\" (memory 0))");
        let imported =
            imported.replacen("(module", r#"(module (import "outside" "memory" (memory 1))"#, 1);
        let module = runtime.compile_app(&wat::parse_str(imported).unwrap()).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        assert!(linker.instantiate_pre(&module).is_err());
    }
}

#[test]
#[ignore = "operator-only optimized CPU timing, excludes compilation, storage and rendering"]
fn async_creation_controlled_memory_cpu_timing() {
    for &backend in backends() {
        let (mut store, instance, effects) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        field(memory.data_mut(&mut store));
        word(memory.data_mut(&mut store), 140, 4096);
        memory.data_mut(&mut store)[256..4352].fill(b'x');
        let volume = store.data().app.volume_handle([6; 16]).unwrap();
        let (_entry, _stale_entry) = super::tests::entry_pointers(&mut store, &instance, volume);
        let create = instance
            .get_typed_func::<(u32, u32, u32, u32, u32), i32>(&mut store, "create_begin")
            .unwrap();
        let mut samples = Vec::with_capacity(1000);
        for index in 0..1100 {
            store.set_fuel(TEST_FUEL).unwrap();
            let start = std::time::Instant::now();
            assert_eq!(create.call(&mut store, (_entry, 32, 64, 1, 8192)).unwrap(), 0);
            complete(&effects);
            assert_eq!(take(&mut store, instance, 8192, true), 0);
            let elapsed = start.elapsed().as_nanos();
            if index >= 100 {
                samples.push(elapsed);
            }
        }
        assert_eq!(effects.lock().unwrap().calls, 1100);
        samples.sort_unstable();
        println!(
            "{backend:?}: 1000 4-KiB creates, ns p50={} p95={} p99={} max={}",
            samples[499], samples[949], samples[989], samples[999]
        );
    }
}

#[test]
#[ignore = "requires independent wasm32 C/Rust compilers; use build-wasm-probe.sh"]
fn compiler_mutations_use_separate_begin_and_take_exports() {
    for language in ["c", "rust"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/wasm-lowering/memory-{language}.wasm"));
        compiled_memory_guest(language, &std::fs::read(path).unwrap());
    }
}

#[test]
fn frozen_v1_rc2_memory_guest_runs_on_current_runtime() {
    // Execute retained compiler output against today's runtime, without rebuilding it.
    for (language, bytes) in super::frozen_guest_tests::MEMORY {
        compiled_memory_guest(language, bytes);
    }
}

fn compiled_memory_guest(language: &str, bytes: &[u8]) {
    for &backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime.compile_app(bytes).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        let effects = Arc::new(Mutex::new(Effects::default()));
        let state =
            WasmState::new(context(), Box::new(RecordingHost(effects.clone())), runtime.fuel_limit)
                .unwrap();
        let volume = state.app.volume_handle([6; 16]).unwrap();
        let volume = playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot);
        let mut store = Store::new(&runtime.engine, state);
        store.set_fuel(TEST_FUEL).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let expected =
            crate::runtime::syscall::environment::host_platform(&store.data().app).unwrap();
        let begin = instance
            .get_typed_func::<(u64, i32, u32), u64>(&mut store, "probe_memory_begin")
            .unwrap();
        let take = instance
            .get_typed_func::<(u32, u32, u32), i32>(&mut store, "probe_memory_take")
            .unwrap();
        for edit in 0..2 {
            let operation = begin.call(&mut store, (volume, expected, edit)).unwrap();
            assert_ne!(operation, 0, "{language}/{backend:?}");
            assert_eq!(
                take.call(&mut store, ((operation >> 32) as u32, operation as u32, edit)).unwrap(),
                -6
            );
            assert_eq!(effects.lock().unwrap().calls, edit as usize);
            complete(&effects);
            assert_eq!(
                take.call(&mut store, ((operation >> 32) as u32, operation as u32, edit)).unwrap(),
                0,
                "{language}/{backend:?}"
            );
            assert_eq!(
                take.call(&mut store, ((operation >> 32) as u32, operation as u32, edit)).unwrap(),
                -8
            );
        }
        let effects = effects.lock().unwrap();
        assert_eq!(effects.calls, 2);
        assert_eq!(
            effects.changes,
            [FieldChange { field_id: [7; 16], values: Some(vec![Value::Boolean(true)]) }]
        );
    }
}

#[test]
fn host_platform_is_the_runtime_target_on_both_wasm_backends() {
    // The identical Wasm module queries its host, not its own wasm32 target.
    for &backend in backends() {
        let (mut store, instance, effects) = fixture(backend);
        let platform = instance.get_typed_func::<(), i32>(&mut store, "platform").unwrap();
        let expected =
            crate::runtime::syscall::environment::host_platform(&store.data().app).unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(expected, playbit_api::abi::environment::HostPlatform::MacOs as i32);
        assert_eq!(platform.call(&mut store, ()).unwrap(), expected);
        assert_eq!(effects.lock().unwrap().calls, 0, "platform lookup needs no data service");
    }
}

#[test]
fn surface_notifications_are_validated_before_queueing_and_poll_exact_bytes() {
    // A surface is host state, with no control ID or data-service access.
    use playbit_api::abi::environment::Surface;
    for &backend in backends() {
        let (mut store, instance, effects) = fixture(backend);
        let generation = store.data().app.generation();
        let surface = Surface {
            width: 336,
            height: 416,
            pixel_scale: 2.0,
            inputs: Surface::POINTER | Surface::KEYBOARD,
        };
        let data = surface.encode().unwrap();
        let event = Event { kind: EventKind::SurfaceChanged, target: 0, data: data.to_vec() };
        let view = crate::runtime::ViewNode::Column(vec![]);
        assert!(crate::runtime::syscall::validate_input(&view, &event).is_ok());
        for bad in [
            Event { target: 1, ..event.clone() },
            Event { data: vec![0; 16], ..event.clone() },
            Event { data: data[..15].to_vec(), ..event.clone() },
        ] {
            assert!(crate::runtime::syscall::validate_input(&view, &bad).is_err());
            assert!(store.data_mut().app.queue_event(generation, bad).is_err());
            assert_eq!(store.data().app.events.len(), 1, "only Started remains");
        }
        assert!(store.data_mut().app.queue_event(generation + 1, event.clone()).is_err());
        store.data_mut().app.queue_event(generation, event).unwrap();
        let poll = instance.get_typed_func::<(u32, u32, u32), i32>(&mut store, "event").unwrap();
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        word(memory.data_mut(&mut store), 32, 24);
        assert_eq!(poll.call(&mut store, (32, 64, 16)).unwrap(), 0);
        assert_eq!(poll.call(&mut store, (32, 64, 15)).unwrap(), 16);
        assert_eq!(store.data().app.events.len(), 1, "short output does not consume");
        assert_eq!(poll.call(&mut store, (32, 64, 16)).unwrap(), 16);
        let memory = instance.get_memory(&mut store, "memory").unwrap().data(&store);
        assert_eq!(memory[48..52], 15_u32.to_le_bytes());
        assert_eq!(Surface::decode(&memory[64..80]).unwrap(), surface);
        assert_eq!(effects.lock().unwrap().calls, 0);
    }
}

#[test]
fn host_limits_are_scalar_and_instance_scoped_on_both_wasm_backends() {
    // Scalar calls leave memory untouched; unsigned overflow and async misuse fail.
    struct SmallHost;
    impl Host for SmallHost {
        fn view_bytes_limit(&self) -> u32 {
            22
        }
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
        fn query_limits(&self) -> crate::runtime::host::QueryLimits {
            crate::runtime::host::QueryLimits { records: 8, value_bytes: 8192 }
        }
    }
    for &backend in backends() {
        let runtime =
            Runtime::with_backend_and_fuel_limit(backend, std::num::NonZeroU64::new(TEST_FUEL))
                .unwrap();
        let module = runtime.compile_app(&wat::parse_str(r#"(module
            (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (memory (export "memory") 1)
            (func (export "pb_main"))
            (func (export "view") (param i64) (result i64)
                (call $call (i32.const 3) (i64.const 32) (local.get 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
            (func (export "limit") (param i64) (result i64)
                (call $call (i32.const 13) (local.get 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))"#).unwrap()).unwrap();
        let mut linker = Linker::new(&runtime.engine);
        define(&mut linker).unwrap();
        let mut store = Store::new(
            &runtime.engine,
            WasmState::new(context(), Box::new(SmallHost), runtime.fuel_limit).unwrap(),
        );
        store.set_fuel(TEST_FUEL).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let limit = instance.get_typed_func::<u64, i64>(&mut store, "limit").unwrap();
        for (kind, expected) in
            [(1, 8), (2, 8192), (3, 22), (0, -7), (u32::MAX as u64, -7), (1u64 << 32, -3)]
        {
            assert_eq!(limit.call(&mut store, kind).unwrap(), expected);
        }
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.column_end();
        let scene = encoder.finish();
        assert_eq!(scene.len(), 22);
        instance.get_memory(&mut store, "memory").unwrap().data_mut(&mut store)[32..54]
            .copy_from_slice(&scene);
        let view = instance.get_typed_func::<u64, i64>(&mut store, "view").unwrap();
        assert_eq!(view.call(&mut store, 22).unwrap(), 0);
        assert_eq!(view.call(&mut store, 23).unwrap(), HostError::LimitExceeded as i64);
        assert_eq!(store.data().app.view_bytes.as_deref(), Some(scene.as_slice()));
    }
}

#[test]
fn random_fill_checks_spans_slots_fuel_and_halted_instances_on_both_backends() {
    // Exercise the real OS source and rejected calls through the scalar import.
    for &backend in backends() {
        let (mut store, instance, _) = fixture(backend);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        memory.grow(&mut store, 1).unwrap();
        let invoke = instance
            .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(&mut store, "invoke")
            .unwrap();
        let fill = |store: &mut Store<WasmState>, pointer, length| {
            invoke.call(store, (20, pointer, length, 0, 0, 0, 0)).unwrap()
        };
        memory.data_mut(&mut store).fill(42);
        assert_eq!(fill(&mut store, 32, 65536), 0);
        let bytes = memory.data(&store);
        assert!(bytes[32..65568].iter().any(|&byte| byte != 42));
        assert!(bytes[..32].iter().chain(&bytes[65568..]).all(|&byte| byte == 42));
        let before = bytes.to_vec();
        assert_eq!(fill(&mut store, u32::MAX as u64, 0), 0);
        for (pointer, length, error) in [
            (0, 1, -3),
            (131072, 1, -3),
            (u32::MAX as u64, 32, -3),
            (32, 65537, -9),
            (1 << 32, 0, -3),
            (32, 1 << 32, -3),
        ] {
            assert_eq!(fill(&mut store, pointer, length), error);
        }
        for slot in 3..7 {
            let mut arguments = [20, 32, 32, 0, 0, 0, 0];
            arguments[slot] = 1;
            let [op, a, b, c, d, e, f] = arguments;
            assert_eq!(invoke.call(&mut store, (op as u32, a, b, c, d, e, f)).unwrap(), -3);
        }
        assert_eq!(memory.data(&store), before);
        store.set_fuel(HOST_CALL_FUEL + 64).unwrap();
        assert_eq!(fill(&mut store, 32, 65536), -9);
        assert_eq!(memory.data(&store), before);
        store.set_fuel(TEST_FUEL).unwrap();
        store.data_mut().app.halt();
        assert_eq!(fill(&mut store, 32, 32), -8);
        assert_eq!(memory.data(&store), before);
    }
}
