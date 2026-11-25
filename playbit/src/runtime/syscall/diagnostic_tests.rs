//! Regression oracles remain active in release tests when diagnostic scans are off.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::test_support::{call, read, write};
use super::*;
use crate::runtime::ExecutionBackend;

struct DiagnosticHost;
impl Host for DiagnosticHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
}

fn context(arguments: usize, volumes: usize) -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            display_name: String::new(),
            account_id: [1; 16],
            is_service: false,
            device_id: [2; 16],
            instance_id: [3; 16],
            session_id: None,
        },
        volumes: (0..volumes)
            .map(|index| AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: (index as u128 + 1).to_le_bytes(),
                name: format!("資料 {index}"),
                tags: vec!["project".into(), "é".into()],
                rights: VolumeRights::ALL,
                is_root: index == 0,
                role: 0,
            })
            .collect(),
        startup_arguments: (0..arguments)
            .map(|index| (format!("arg-{index}"), "資料 é".repeat(index + 1)))
            .collect(),
        trace_catalog: None,
        required_capabilities: BTreeSet::new(),
        participants: Vec::new(),
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn text_range<'a>(bytes: &'a [u8], record: &[u8], offset: usize) -> &'a str {
    let start = u32_at(record, offset) as usize;
    let length = u32_at(record, offset + 4) as usize;
    std::str::from_utf8(&bytes[start..start + length]).unwrap()
}
fn descriptor(guest: &mut GuestInstance, capacities: &[usize]) {
    let volume = capacities.len() == 3;
    let size = if volume { 64u32 } else { 36 };
    let mut words = vec![size, 0];
    if volume {
        words.extend([0, 0]);
    }
    let pointers = if volume { [1024u32, 24576, 32768] } else { [1024u32, 20000, 0] };
    for (index, (&pointer, &capacity)) in pointers.iter().zip(capacities).enumerate() {
        words.extend([pointer, capacity as u32, u32::MAX]);
        if index + 1 != capacities.len() {
            let stride = if !volume {
                24
            } else if index == 0 {
                80
            } else {
                16
            };
            words.push(stride);
            for row in 0..capacity {
                write(
                    guest,
                    pointer as usize + row * stride as usize,
                    &[stride.to_le_bytes(), 0u32.to_le_bytes()].concat(),
                );
            }
        }
    }
    let bytes = words.into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>();
    write(guest, 64, &bytes);
}
fn check_counts(guest: &mut GuestInstance, expected: &[usize]) {
    let base = if expected.len() == 3 { 80 } else { 72 };
    for (index, count) in expected.iter().enumerate() {
        assert_eq!(u32_at(&read(guest, base + index * 16 + 8, 4), 0) as usize, *count);
    }
}

#[test]
fn typed_metadata_survives_short_buffers_and_round_trips_on_both_backends() {
    // Unicode lengths, zero/maximum argument counts and maximum volume count.
    let wasm = wat::parse_str(
        r#"(module
        (import "playbit" "pb_syscall" (func $dispatch_args
            (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (import "playbit" "pb_syscall" (func $dispatch_volumes
            (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $args (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_args (i32.const 6)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0)
                (i64.const 0) (i64.const 0))))
    (func $volumes (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_volumes (i32.const 7)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0)
                (i64.const 0) (i64.const 0))))
        (memory (export "memory") 2)
        (func (export "pb_main"))
        (func (export "arguments") (param i32) (result i32) local.get 0 call $args)
        (func (export "volumes") (param i32) (result i32) local.get 0 call $volumes))"#,
    )
    .unwrap();
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime = Runtime::with_backend(backend).unwrap();
        for (arguments, volumes) in [(0, 1), (4, 4), (abi::MAX_STARTUP_ARGUMENTS, abi::MAX_VOLUMES)]
        {
            let context = context(arguments, volumes);
            let mut guest =
                runtime.create_guest(&wasm, context.clone(), Box::new(DiagnosticHost)).unwrap();
            for name in ["arguments", "volumes"] {
                let expected = if name == "arguments" {
                    vec![
                        arguments,
                        context.startup_arguments.iter().map(|(k, v)| k.len() + v.len()).sum(),
                    ]
                } else {
                    vec![
                        volumes,
                        volumes * 2,
                        context
                            .volumes
                            .iter()
                            .map(|v| v.name.len() + v.tags.iter().map(String::len).sum::<usize>())
                            .sum(),
                    ]
                };
                // Each insufficient array independently reports all counts and leaves payloads untouched.
                for (index, count) in expected.iter().enumerate().filter(|(_, n)| **n > 0) {
                    for short in [0, count - 1] {
                        let mut capacities = expected.clone();
                        capacities[index] = short;
                        write(&mut guest, 1024, &vec![0xa5; 65536]);
                        descriptor(&mut guest, &capacities);
                        let before = read(&mut guest, 1024, 65536);
                        assert_eq!(call(&mut guest, name, 64u32), HostError::BufferTooSmall as i32);
                        check_counts(&mut guest, &expected);
                        assert_eq!(read(&mut guest, 1024, 65536), before);
                    }
                }
                descriptor(&mut guest, &expected);
                assert_eq!(call(&mut guest, name, 64u32), 0);
                check_counts(&mut guest, &expected);
                if name == "arguments" {
                    let records = read(&mut guest, 1024, arguments * 24);
                    let bytes = read(&mut guest, 20000, expected[1]);
                    for (record, (key, value)) in
                        records.as_chunks::<24>().0.iter().zip(&context.startup_arguments)
                    {
                        assert_eq!(text_range(&bytes, record, 8), key);
                        assert_eq!(text_range(&bytes, record, 16), value);
                    }
                } else {
                    let records = read(&mut guest, 1024, volumes * 80);
                    let tags = read(&mut guest, 24576, expected[1] * 16);
                    let bytes = read(&mut guest, 32768, expected[2]);
                    for (record, volume) in records.as_chunks::<80>().0.iter().zip(&context.volumes)
                    {
                        let handle = guest.volume_handle(volume.id).unwrap();
                        assert_eq!(&record[8..12], &handle.slot.to_le_bytes());
                        assert_eq!(u32_at(record, 12), handle.generation);
                        assert_eq!(&record[16..32], &volume.id);
                        assert_eq!(text_range(&bytes, record, 32), volume.name);
                        assert_eq!(u32_at(record, 48), volume.rights.0);
                        assert_eq!(
                            u32_at(record, 52),
                            u32::from(volume.is_root) | playbit_api::abi::data::VOLUME_PLAYBIT
                        );
                        let start = u32_at(record, 40) as usize;
                        let count = u32_at(record, 44) as usize;
                        assert_eq!(count, volume.tags.len());
                        for (record, tag) in tags[start * 16..(start + count) * 16]
                            .as_chunks::<16>()
                            .0
                            .iter()
                            .zip(&volume.tags)
                        {
                            assert_eq!(text_range(&bytes, record, 8), tag);
                        }
                    }
                }
                write(&mut guest, 131068, &64_u32.to_le_bytes());
                for pointer in [0u32, 65, 131068] {
                    let before = read(&mut guest, 0, 131072);
                    assert_eq!(call(&mut guest, name, pointer), HostError::InvalidArgument as i32);
                    assert_eq!(read(&mut guest, 0, 131072), before);
                }
            }
        }
    }
}

fn check_accounting(state: &State) {
    // This independent recount is deliberately a release assertion in tests.
    assert_eq!(
        state.queued_bytes,
        state
            .events
            .iter()
            .map(|queued| abi::EVENT_QUEUE_OVERHEAD + queued.event.data.len())
            .sum::<usize>()
    );
    state.assert_invariants();
}

#[test]
fn every_queue_mutation_preserves_bytes_and_unrelated_events() {
    // Cover mixed payload sizes, full admission, selective operation consumption,
    // wake replacement, failed requests and non-consuming peeks in one sequence.
    for retained in [0, 1, 32, MAX_QUEUED_EVENTS - 4] {
        let mut state = State::new(context(0, 1), Box::new(DiagnosticHost)).unwrap();
        check_accounting(&state);
        events::take_event(&mut state).unwrap();
        for index in 0..retained {
            push_event(
                &mut state,
                Event {
                    object_id: 0,
                    kind: EventKind::DataChanged,
                    target: index as u32,
                    data: vec![42; index % 17],
                },
            )
            .unwrap();
            check_accounting(&state);
        }
        let pending = PendingOperation::FileSelection { token: 7 };
        let slot = reserve_operation(&mut state).unwrap();
        let handle = register_operation(&mut state, slot, pending.clone()).unwrap();
        let mut request = [0; 8];
        Writer::new(&mut request).operation_handle(handle).unwrap();
        for (kind, data) in [
            (EventKind::FileSelectionReady, request.to_vec()),
            (EventKind::FileSelectionReady, vec![0; 8]),
            (EventKind::DataChanged, request.to_vec()),
        ] {
            push_event(&mut state, Event { object_id: 0, kind, target: 0, data }).unwrap();
            check_accounting(&state);
        }
        control::wake_after(&mut state, 1000, Instant::now()).unwrap();
        let deadline = state.wake_deadline.unwrap();
        queue_due_wake(&mut state, deadline);
        check_accounting(&state);
        let before = state.queued_bytes;
        if state.events.len() == MAX_QUEUED_EVENTS {
            assert_eq!(
                push_event(
                    &mut state,
                    Event { object_id: 0, kind: EventKind::DataChanged, target: 0, data: vec![] }
                ),
                Err(HostError::LimitExceeded)
            );
            assert_eq!(state.queued_bytes, before);
        }
        consume_operation(&mut state, handle, pending);
        assert_eq!(state.events.len(), retained + 3);
        assert_eq!(state.queued_bytes, before - (8 + abi::EVENT_QUEUE_OVERHEAD));
        check_accounting(&state);
        let before = state.queued_bytes;
        assert_eq!(
            control::wake_after(&mut state, 0, Instant::now()),
            Err(HostError::InvalidArgument)
        );
        assert_eq!(state.queued_bytes, before);
        control::wake_after(&mut state, 1000, Instant::now()).unwrap();
        assert_eq!(state.queued_bytes, before - abi::EVENT_QUEUE_OVERHEAD);
        assert!(state.events.iter().all(|queued| queued.event.kind != EventKind::Wake));
        check_accounting(&state);
        for index in 0..retained + 2 {
            let before = state.queued_bytes;
            let expected = events::peek_event(&state).unwrap().clone();
            assert_eq!(state.queued_bytes, before);
            let event = events::take_event(&mut state).unwrap();
            assert_eq!(event, expected);
            let kind = event.kind as u32;
            let target = event.target;
            let data = event.data.as_slice();
            if index < retained {
                assert_eq!(
                    (kind, target, data),
                    (EventKind::DataChanged as u32, index as u32, vec![42; index % 17].as_slice())
                );
            } else if index == retained {
                assert_eq!((kind, data), (EventKind::FileSelectionReady as u32, [0; 8].as_slice()));
            } else {
                assert_eq!((kind, data), (EventKind::DataChanged as u32, request.as_slice()));
            }
            check_accounting(&state);
        }
        assert_eq!(state.queued_bytes, 0);
        assert_eq!(events::take_event(&mut state), Err(HostError::WouldBlock));
    }
}

#[test]
#[ignore = "operator-only optimized diagnostic; never a CI timing gate"]
#[allow(
    clippy::assertions_on_constants,
    reason = "The operator diagnostic must reject a debug test binary at runtime."
)]
fn benchmark_diagnostic_scans() {
    // Measure each real operation with setup/validation outside the timed region.
    // These are syscall-helper costs, not guest, IPC or physical-input latency.
    assert!(!cfg!(debug_assertions), "use an ordinary release test build");
    for (case, size) in [
        ("arguments", 4),
        ("arguments", 32),
        ("volumes", 4),
        ("volumes", 256),
        ("wake", 32),
        ("wake", 256),
        ("consume", 32),
        ("consume", 256),
    ] {
        let mut state = State::new(
            context(
                if case == "arguments" { size } else { 0 },
                if case == "volumes" { size } else { 1 },
            ),
            Box::new(DiagnosticHost),
        )
        .unwrap();
        events::take_event(&mut state).unwrap();
        if matches!(case, "wake" | "consume") {
            for _ in 1..size {
                push_event(
                    &mut state,
                    Event {
                        object_id: 0,
                        kind: EventKind::DataChanged,
                        target: 0,
                        data: vec![42; 16],
                    },
                )
                .unwrap();
            }
        }
        let mut samples = Vec::with_capacity(10_000);
        for sample in 0..10_100 {
            let pending = PendingOperation::FileSelection { token: 7 };
            let mut request = [0; 8];
            let handle = if case == "consume" {
                let slot = reserve_operation(&mut state).unwrap();
                let handle = register_operation(&mut state, slot, pending.clone()).unwrap();
                Writer::new(&mut request).operation_handle(handle).unwrap();
                push_event(
                    &mut state,
                    Event {
                        object_id: 0,
                        kind: EventKind::FileSelectionReady,
                        target: 0,
                        data: request.to_vec(),
                    },
                )
                .unwrap();
                Some(handle)
            } else {
                if case == "wake" {
                    state.wake_deadline = Some(Instant::now());
                    let deadline = state.wake_deadline.unwrap();
                    queue_due_wake(&mut state, deadline);
                }
                None
            };
            let start = Instant::now();
            let result = match case {
                "arguments" => environment::startup_arguments(&state).map(|v| {
                    std::hint::black_box(v);
                }),
                "volumes" => environment::available_volumes(&state).map(|volumes| {
                    for volume in volumes {
                        std::hint::black_box(volume);
                    }
                }),
                "wake" => control::wake_after(&mut state, 1000, Instant::now()),
                "consume" => {
                    consume_operation(&mut state, handle.unwrap(), pending);
                    Ok(())
                }
                _ => unreachable!(),
            };
            std::hint::black_box(&result);
            let elapsed = start.elapsed().as_nanos() as u64;
            result.unwrap();
            check_accounting(&state);
            if sample >= 100 {
                samples.push(elapsed);
            }
        }
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        println!(
            "{}",
            serde_json::json!({"case":case,"size":size,"samples_ns":samples,
            "p50_ns":sorted[4999],"p99_ns":sorted[9899],"max_ns":sorted[9999]})
        );
    }
}
