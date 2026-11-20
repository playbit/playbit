#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::*;
use crate::runtime::{ExecutionBackend, managed::ManagedGuest};

struct WakeHost;
impl Host for WakeHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [1; 16],
            is_service: true,
            device_id: [2; 16],
            instance_id: [5; 16],
            session_id: Some([6; 32]),
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [7; 16],
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        }],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: BTreeSet::from(["wake.after".into()]),
        participants: vec![],
    }
}

const CALLER: &str = r#"(module
    (import "playbit" "pb_syscall" (func $dispatch_wake (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (import "playbit" "pb_syscall" (func $dispatch_event (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $wake (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_wake (i32.const 11)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $event (param i32 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_event (i32.const 4)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0))))
    (memory (export "memory") 2)
    (func (export "wake") (param i32) (result i32) local.get 0 call $wake)
    (func (export "event") (param i32 i32 i32) (result i32)
        local.get 0 local.get 1 local.get 2 call $event)
    (func (export "pb_main"))
)"#;

fn wake(guest: &mut GuestInstance, delay: u32) -> i32 {
    guest
        .instance
        .get_typed_func::<u32, i32>(&mut guest.store, "wake")
        .unwrap()
        .call(&mut guest.store, delay)
        .unwrap()
}
fn poll(guest: &mut GuestInstance) -> HostResult<Event> {
    let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
    let mut before = [0xa5; 24];
    before[..4].copy_from_slice(&24_u32.to_le_bytes());
    before[4..8].fill(0);
    memory.write(&mut guest.store, 64, &before).unwrap();
    let status = guest
        .instance
        .get_typed_func::<(u32, u32, u32), i32>(&mut guest.store, "event")
        .unwrap()
        .call(&mut guest.store, (64, 96, 65536))
        .unwrap();
    let mut header = [0; 24];
    memory.read(&guest.store, 64, &mut header).unwrap();
    if status < 0 {
        assert_eq!(header, before);
        return Err(HostError::try_from(status).unwrap());
    }
    assert_eq!(u64::from_le_bytes(header[8..16].try_into().unwrap()), guest.generation());
    let kind = match u32::from_le_bytes(header[16..20].try_into().unwrap()) {
        1 => EventKind::Started,
        4 => EventKind::DataChanged,
        10 => EventKind::Wake,
        kind => panic!("unexpected wake fixture event {kind}"),
    };
    let target = u32::from_le_bytes(header[20..].try_into().unwrap());
    let mut data = vec![0; status as usize];
    memory.read(&guest.store, 96, &mut data).unwrap();
    Ok(Event { kind, target, data })
}

fn guest(backend: ExecutionBackend) -> GuestInstance {
    Runtime::with_backend(backend)
        .unwrap()
        .create_guest(&wat::parse_str(CALLER).unwrap(), context(), Box::new(WakeHost))
        .unwrap()
}

#[test]
fn readiness_tracks_started_notifications_and_due_wakes_without_consuming_them() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = guest(backend);
        assert!(guest.has_ready_work(Instant::now()));
        poll(&mut guest).unwrap();
        assert!(!guest.has_ready_work(Instant::now()));
        guest
            .queue_event(
                guest.generation(),
                Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
            )
            .unwrap();
        assert!(guest.has_ready_work(Instant::now()));
        assert!(guest.has_ready_work(Instant::now()));
        poll(&mut guest).unwrap();
        assert!(!guest.has_ready_work(Instant::now()));
        assert_eq!(wake(&mut guest, 1000), 0);
        let deadline = guest.store.data().app.wake_deadline.unwrap();
        assert!(!guest.has_ready_work(deadline - Duration::from_nanos(1)));
        assert!(guest.has_ready_work(deadline));
        assert_eq!(guest.store.data().app.wake_deadline, Some(deadline));
        assert!(guest.store.data().app.events.is_empty());
        // Advance only the test's deadline, avoiding a timing-dependent sleep.
        guest.store.data_mut().app.wake_deadline = Some(Instant::now());
        guest.pump().unwrap();
        assert!(guest.has_ready_work(Instant::now()));
        assert_eq!(poll(&mut guest).unwrap().kind, EventKind::Wake);
        assert!(!guest.has_ready_work(Instant::now()));
    }
}

#[test]
fn wake_after_validates_before_replacing_deadline_or_touching_memory() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = guest(backend);
        let memory = guest.instance.get_memory(&mut guest.store, "memory").unwrap();
        let untouched = memory.data(&guest.store).to_vec();
        let before = Instant::now();
        assert_eq!(wake(&mut guest, 1000), 0);
        let deadline = guest.store.data().app.wake_deadline.unwrap();
        assert!(deadline >= before + Duration::from_secs(1));
        for delay in [0, 999, abi::MAX_WAKE_DELAY_MS + 1, u32::MAX] {
            assert_eq!(wake(&mut guest, delay), HostError::InvalidArgument as i32);
            assert_eq!(guest.store.data().app.wake_deadline, Some(deadline));
            assert_eq!(memory.data(&guest.store), untouched);
        }
        assert_eq!(wake(&mut guest, abi::MAX_WAKE_DELAY_MS), 0);
        assert!(guest.store.data().app.wake_deadline.unwrap() > deadline);
        assert_eq!(memory.data(&guest.store), untouched);
    }
}

#[test]
fn wake_is_one_shot_bound_to_generation_and_cannot_be_injected() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = guest(backend);
        poll(&mut guest).unwrap();
        assert_eq!(wake(&mut guest, 1000), 0);
        let deadline = guest.store.data().app.wake_deadline.unwrap();
        assert!(!guest.wake_due(deadline - Duration::from_nanos(1)));
        assert!(guest.wake_due(deadline));
        queue_due_wake(&mut guest.store.data_mut().app, deadline - Duration::from_nanos(1));
        assert!(guest.store.data().app.events.is_empty());
        queue_due_wake(&mut guest.store.data_mut().app, deadline);
        queue_due_wake(&mut guest.store.data_mut().app, deadline + Duration::from_secs(1));
        assert!(!guest.wake_due(deadline));
        assert_eq!(guest.store.data().app.events.len(), 1);
        let invalid = guest
            .instance
            .get_typed_func::<(u32, u32, u32), i32>(&mut guest.store, "event")
            .unwrap();
        assert_eq!(
            invalid.call(&mut guest.store, (0, 0, 0)).unwrap(),
            HostError::InvalidArgument as i32
        );
        assert_eq!(guest.store.data().app.events.len(), 1);
        let event = poll(&mut guest).unwrap();
        assert_eq!(event.kind, EventKind::Wake);
        assert_eq!(event.target, 0);
        assert!(event.data.is_empty());
        assert_eq!(guest.store.data().app.queued_bytes, 0);
        assert_eq!(poll(&mut guest), Err(HostError::WouldBlock));
        assert_eq!(
            guest.queue_event(
                guest.generation(),
                Event { kind: EventKind::Wake, target: 0, data: vec![] }
            ),
            Err(HostError::InvalidArgument)
        );
    }
}

#[test]
fn replacing_an_unconsumed_wake_removes_it_without_losing_other_events() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = guest(backend);
        assert_eq!(wake(&mut guest, 1000), 0);
        let deadline = guest.store.data().app.wake_deadline.unwrap();
        queue_due_wake(&mut guest.store.data_mut().app, deadline);
        assert_eq!(guest.store.data().app.queued_bytes, 40);
        assert_eq!(wake(&mut guest, 2000), 0);
        assert_eq!(guest.store.data().app.queued_bytes, 20);
        assert_eq!(guest.store.data().app.events.front().unwrap().event.kind, EventKind::Started);
        assert!(guest.store.data().app.wake_deadline.unwrap() > deadline);
    }
}

#[test]
fn full_queue_preserves_due_wake_until_a_turn_can_drain_it() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = guest(backend);
        for _ in 1..MAX_QUEUED_EVENTS {
            guest
                .queue_event(
                    guest.generation(),
                    Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
                )
                .unwrap();
        }
        assert_eq!(wake(&mut guest, 1000), 0);
        let deadline = guest.store.data().app.wake_deadline.unwrap();
        queue_due_wake(&mut guest.store.data_mut().app, deadline);
        assert_eq!(guest.store.data().app.events.len(), MAX_QUEUED_EVENTS);
        assert!(guest.wake_due(deadline));
        poll(&mut guest).unwrap();
        queue_due_wake(&mut guest.store.data_mut().app, deadline);
        assert!(!guest.wake_due(deadline));
        assert_eq!(guest.store.data().app.events.back().unwrap().event.kind, EventKind::Wake);
        assert_eq!(guest.store.data().app.queued_bytes, 20 * MAX_QUEUED_EVENTS);
    }
}

fn waking_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
        (import "playbit" "pb_syscall" (func $dispatch_event (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (import "playbit" "pb_syscall" (func $dispatch_wake (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (import "playbit" "pb_syscall" (func $dispatch_log (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
    (func $event (param i32 i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_event (i32.const 4)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.extend_i32_u (local.get 2)) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $wake (param i32) (result i32)
        (i32.wrap_i64 (call $dispatch_wake (i32.const 11)
            (i64.extend_i32_u (local.get 0)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    (func $log (param i32 i32) (result i32)
        (i32.wrap_i64 (call $dispatch_log (i32.const 2)
            (i64.extend_i32_u (local.get 0)) (i64.extend_i32_u (local.get 1)) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
        (memory (export "memory") 2)
        (data (i32.const 32) "Woke")
        (data (i32.const 128) "\18\00\00\00\00\00\00\00")
        (func (export "pb_main")
            (local $kind i32)
            (block $done (loop $next
                i32.const 128 i32.const 0 i32.const 0 call $event
                i32.const 0 i32.lt_s br_if $done
                i32.const 144 i32.load local.set $kind
                local.get $kind i32.const 1 i32.eq
                if i32.const 1000 call $wake drop end
                local.get $kind i32.const 10 i32.eq
                if i32.const 32 i32.const 4 call $log drop end
                br $next
            ))
        )
    )"#,
    )
    .unwrap()
}

#[test]
fn pump_delivers_due_wake_to_the_actual_guest_once() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = Runtime::with_backend(backend)
            .unwrap()
            .create_guest(&waking_wasm(), context(), Box::new(WakeHost))
            .unwrap();
        assert!(guest.pump().unwrap().logs().is_empty());
        assert!(guest.store.data().app.wake_deadline.is_some());
        assert!(guest.pump().unwrap().logs().is_empty());
        guest.store.data_mut().app.wake_deadline = Some(Instant::now());
        assert_eq!(guest.pump().unwrap().logs(), ["Woke"]);
        assert!(guest.pump().unwrap().logs().is_empty());
    }
}

#[test]
fn successful_reload_drops_deadline_and_failed_reload_preserves_it() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let mut guest = ManagedGuest::create(
            Runtime::with_backend(backend).unwrap(),
            &waking_wasm(),
            context(),
            Box::new(WakeHost),
            |_| Ok(()),
        )
        .unwrap();
        let generation = guest.generation();
        let future = Instant::now() + Duration::from_secs(2);
        assert!(guest.wake_due(future));
        let trapped = wat::parse_str(
            CALLER
                .replace("(func (export \"pb_main\"))", "(func (export \"pb_main\") unreachable)"),
        )
        .unwrap();
        assert!(guest.replace(&trapped, None, BTreeSet::new(), Box::new(WakeHost)).is_err());
        assert_eq!(guest.generation(), generation);
        assert!(guest.wake_due(future));
        guest
            .replace(&wat::parse_str(CALLER).unwrap(), None, BTreeSet::new(), Box::new(WakeHost))
            .unwrap();
        assert_ne!(guest.generation(), generation);
        assert_eq!(guest.context().identity, context().identity);
        assert!(!guest.wake_due(future));
    }
}
