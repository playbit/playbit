#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::test_support::{call, capabilities, operation_handle, read, write};
use super::*;
use crate::runtime::{ExecutionBackend, managed::ManagedGuest};
use std::sync::{Arc, Mutex};

const ROOT: Id = [1; 16];
const TYPE: Id = [2; 16];
const OBJECT: Id = [9; 16];
const FIRST_TOKEN: u64 = 0xaabb_ccdd_0000_1234;

#[derive(Default)]
struct Records {
    begins: Vec<(u64, Id, Id, u64)>,
    initial_fields: Vec<Vec<FieldChange>>,
    polls: Vec<u64>,
    results: BTreeMap<u64, HostResult<Id>>,
    begin_error: Option<HostError>,
}

struct CreationHost(Arc<Mutex<Records>>);

impl Host for CreationHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::ObjectCreateBegin, Opcode::ObjectCreatePoll]
    }

    fn begin_data(
        &mut self,
        generation: u64,
        call: std::sync::Arc<crate::runtime::host::DataCall>,
    ) -> HostResult<u64> {
        let crate::runtime::host::DataOperation::Create { type_id, fields, .. } = &call.operation
        else {
            return Err(HostError::Unsupported);
        };
        let volume = call.volume;
        let type_id = *type_id;
        let changes = fields.as_slice();

        let mut records = self.0.lock().unwrap();
        if let Some(error) = records.begin_error {
            return Err(error);
        }
        let token = FIRST_TOKEN + records.begins.len() as u64;
        records.begins.push((generation, volume, type_id, token));
        records.initial_fields.push(changes.to_vec());
        Ok(token)
    }

    fn peek_data(
        &mut self,
        token: u64,
    ) -> HostResult<std::sync::Arc<crate::runtime::host::DataResult>> {
        let result: HostResult<[u8; 16]> = {
            let mut records = self.0.lock().unwrap();
            records.polls.push(token);
            records.results.get(&token).copied().unwrap_or(Err(HostError::WouldBlock))
        };
        result.map(|id| std::sync::Arc::new(crate::runtime::host::DataResult::Created(id)))
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [3; 16],
            is_service: false,
            device_id: [4; 16],
            instance_id: [7; 16],
            session_id: Some([8; 32]),
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: ROOT,
            name: "Root".into(),
            tags: vec![],
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        }],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: BTreeSet::new(),
        participants: vec![],
    }
}

fn wasm() -> Vec<u8> {
    wat::parse_str(include_str!("../../../tests/fixtures/v1-creation.wat")).unwrap()
}

fn guest(backend: ExecutionBackend) -> (GuestInstance, Arc<Mutex<Records>>) {
    let records = Arc::new(Mutex::new(Records::default()));
    let guest = Runtime::with_backend(backend)
        .unwrap()
        .create_guest(&wasm(), context(), Box::new(CreationHost(records.clone())))
        .unwrap();
    (guest, records)
}

fn begin_status(guest: &mut GuestInstance) -> i32 {
    let volume = guest.volume_handle(ROOT).unwrap();
    let (_entry, _stale_entry) = super::test_support::entry_pointers(guest, volume);
    write(guest, 4096, &TYPE);
    call(guest, "begin", (_entry, 4096u32, 4200u32))
}

fn begin(guest: &mut GuestInstance) -> OperationHandle {
    assert_eq!(begin_status(guest), 0);
    operation_handle(guest, 4200)
}

fn poll(guest: &mut GuestInstance, handle: OperationHandle) -> (i32, Vec<u8>) {
    write(guest, 4300, &[0xa5; 16]);
    let status = call(guest, "poll", (u64::from(handle.generation), handle.slot, 4300u32));
    (status, read(guest, 4300, 16))
}

#[test]
fn creation_copies_initial_fields_before_returning_its_handle() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let volume = guest.volume_handle(ROOT).unwrap();
        let (entry, _) = super::test_support::entry_pointers(&mut guest, volume);
        write(&mut guest, 4096, &TYPE);
        // One wasm32 field points to one text value, which points to its bytes.
        let mut field = [0; 40];
        field[..4].copy_from_slice(&40u32.to_le_bytes());
        field[8..24].fill(5);
        field[24..28].copy_from_slice(&4608u32.to_le_bytes());
        field[28..32].copy_from_slice(&1u32.to_le_bytes());
        field[32..36].copy_from_slice(&80u32.to_le_bytes());
        field[36..40].copy_from_slice(&1u32.to_le_bytes());
        let mut value = [0; 80];
        value[..4].copy_from_slice(&80u32.to_le_bytes());
        value[8..12].copy_from_slice(&1u32.to_le_bytes());
        value[12..16].copy_from_slice(&5u32.to_le_bytes());
        value[72..76].copy_from_slice(&4704u32.to_le_bytes());
        write(&mut guest, 4480, &field);
        write(&mut guest, 4608, &value);
        write(&mut guest, 4704, b"draft");
        assert_eq!(call(&mut guest, "begin_fields", (entry, 4096u32, 4480u32, 1u32, 4200u32)), 0);
        write(&mut guest, 4480, &[0; 40]);
        write(&mut guest, 4608, &[0; 80]);
        write(&mut guest, 4704, b"other");
        assert_eq!(
            records.lock().unwrap().initial_fields,
            vec![vec![FieldChange {
                field_id: [5; 16],
                values: Some(vec![Value::Text("draft".into())]),
            }]]
        );
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            1
        );
    }
}

#[test]
fn creation_admits_fixed_outputs_and_permissions_before_host_effects() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let volume = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, volume);
        // Invalid input spans fail before host admission.
        for (type_id, output) in [(0, 4200), (131060, 4200)] {
            let before = read(&mut guest, 0, 131072);
            assert_eq!(
                call(&mut guest, "begin", (_entry, type_id as u32, output as u32)),
                HostError::InvalidArgument as i32
            );
            assert_eq!(read(&mut guest, 0, 131072), before);
        }
        super::test_support::set_volume_rights(
            &mut guest.store.data_mut().app,
            0,
            VolumeRights::READ,
        );
        assert_eq!(begin_status(&mut guest), HostError::AccessDenied as i32);
        assert!(records.lock().unwrap().begins.is_empty());
        assert!(guest.store.data().app.capabilities.operations().next().is_none());

        super::test_support::set_volume_rights(
            &mut guest.store.data_mut().app,
            0,
            VolumeRights::ALL,
        );
        let handle = begin(&mut guest);
        records.lock().unwrap().results.insert(FIRST_TOKEN, Ok(OBJECT));
        for output in [0u32, 131057] {
            let before = read(&mut guest, 0, 131072);
            assert_eq!(
                call(&mut guest, "poll", (u64::from(handle.generation), handle.slot, output)),
                HostError::InvalidArgument as i32
            );
            assert_eq!(read(&mut guest, 0, 131072), before);
        }
        assert!(records.lock().unwrap().polls.is_empty());
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            1
        );
        assert_eq!(poll(&mut guest, handle), (0, OBJECT.to_vec()));
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
    }
}

#[test]
fn pending_bound_and_consumed_handles_never_alias_later_operations() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let handles: Vec<_> = (0..abi::MAX_PENDING_OPERATIONS).map(|_| begin(&mut guest)).collect();
        assert_eq!(begin_status(&mut guest), HostError::LimitExceeded as i32);
        assert_eq!(records.lock().unwrap().begins.len(), 64);
        records.lock().unwrap().results.insert(FIRST_TOKEN, Ok(OBJECT));
        assert_eq!(poll(&mut guest, handles[0]).0, 0);
        let next = begin(&mut guest);
        assert_eq!(next.slot, handles[0].slot);
        assert_ne!(next.generation, handles[0].generation);
        assert_eq!(poll(&mut guest, handles[0]).0, HostError::StaleHandle as i32);
        assert_eq!(records.lock().unwrap().polls, [FIRST_TOKEN]);

        records.lock().unwrap().results.insert(FIRST_TOKEN + 1, Ok(OBJECT));
        assert_eq!(poll(&mut guest, handles[1]).0, 0);
        let reused = begin(&mut guest);
        assert_eq!(reused.slot, handles[1].slot);
        assert_ne!(reused.generation, handles[1].generation);
    }
}

#[test]
fn real_guest_waits_then_completes_from_generation_scoped_handle_only_event() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        assert!(guest.pump().unwrap().logs().is_empty());
        let generation = guest.generation();
        assert_eq!(records.lock().unwrap().begins, [(generation, ROOT, TYPE, FIRST_TOKEN)]);
        assert_eq!(records.lock().unwrap().polls, [FIRST_TOKEN]);
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            1
        );
        records.lock().unwrap().results.insert(FIRST_TOKEN, Ok(OBJECT));
        guest.notify_creation(generation, FIRST_TOKEN).unwrap();
        guest.notify_creation(generation, FIRST_TOKEN).unwrap();
        assert_eq!(guest.store.data().app.events.len(), 1);
        let event = &guest.store.data().app.events.front().unwrap().event;
        assert_eq!(event.kind, EventKind::ObjectCreationReady);
        assert_eq!(event.target, 0);
        assert_eq!(event.data.len(), 8);
        assert_eq!(
            Reader::new(&event.data).unwrap().operation_handle().unwrap(),
            super::handles::operation_handle(
                guest.store.data().app.capabilities.operations().next().unwrap().0.raw()
            )
        );
        assert_eq!(guest.pump().unwrap().logs(), ["Created"]);
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        assert!(guest.store.data().app.events.is_empty());
        assert_eq!(guest.store.data().app.queued_bytes, 0);
        assert_eq!(guest.notify_creation(generation, FIRST_TOKEN), Err(HostError::StaleHandle));
        assert_eq!(
            guest.queue_event(
                generation,
                Event { kind: EventKind::ObjectCreationReady, target: 0, data: vec![0; 12] }
            ),
            Err(HostError::InvalidArgument)
        );
    }
}

#[test]
fn terminal_errors_consume_handles_and_queued_hints_but_would_block_retains_them() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let generation = guest.generation();
        for (index, error) in [
            HostError::AccessDenied,
            HostError::NotFound,
            HostError::InvalidArgument,
            HostError::Internal,
            HostError::Unavailable,
            HostError::Unsupported,
            HostError::StaleHandle,
            HostError::LimitExceeded,
            HostError::Conflict,
        ]
        .into_iter()
        .enumerate()
        {
            let handle = begin(&mut guest);
            let token = FIRST_TOKEN + index as u64;
            assert_eq!(poll(&mut guest, handle).0, HostError::WouldBlock as i32);
            guest.notify_creation(generation, token).unwrap();
            records.lock().unwrap().results.insert(token, Err(error));
            assert_eq!(poll(&mut guest, handle), (error as i32, vec![0xa5; 16]));
            assert!(guest.store.data().app.capabilities.operations().next().is_none());
            assert_eq!(guest.store.data().app.events.len(), 1); // Started remains.
            assert_eq!(guest.store.data().app.queued_bytes, 20);
            assert_eq!(poll(&mut guest, handle).0, HostError::StaleHandle as i32);
            assert_eq!(guest.notify_creation(generation, token), Err(HostError::StaleHandle));
        }
        assert_eq!(records.lock().unwrap().polls.len(), 18);
        records.lock().unwrap().begin_error = Some(HostError::Unavailable);
        let slot = guest.store.data().app.capabilities.table.reserved();
        assert_eq!(begin_status(&mut guest), HostError::Unavailable as i32);
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        assert_eq!(guest.store.data().app.capabilities.table.reserved(), slot);
    }
}

#[test]
fn forged_and_stale_operation_handles_never_reach_host() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let _valid = begin(&mut guest);
        let generation = guest.generation();
        for (generation, slot) in [(generation, 0u32), (generation, 2), (generation + 1, 1), (0, 1)]
        {
            assert_eq!(
                poll(
                    &mut guest,
                    OperationHandle { generation: generation.try_into().unwrap(), slot }
                ),
                (HostError::StaleHandle as i32, vec![0xa5; 16])
            );
        }
        assert_eq!(guest.notify_creation(generation + 1, FIRST_TOKEN), Err(HostError::StaleHandle));
        assert_eq!(guest.notify_creation(generation, FIRST_TOKEN + 1), Err(HostError::StaleHandle));
        assert!(records.lock().unwrap().polls.is_empty());
    }
}

#[test]
fn capabilities_advertise_only_implemented_creation_paths() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        let capabilities = capabilities(&mut guest);
        assert!(capabilities.contains("object.create.begin"));
        assert!(capabilities.contains("object.create.poll"));
        assert!(!capabilities.contains("object.create"));
        let volume = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, volume);
        assert_eq!(
            guest
                .instance
                .get_typed_func::<(u32, u64, u64, u64, u64, u64, u64), i64>(
                    &mut guest.store,
                    "invoke",
                )
                .unwrap()
                .call(
                    &mut guest.store,
                    (
                        0x110u32,
                        playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot),
                        4096u64,
                        0u64,
                        0u64,
                        4300u64,
                        0u64
                    )
                )
                .unwrap(),
            HostError::Unsupported as i64
        );
        let mut required = context();
        required.required_capabilities =
            ["object.create.begin".into(), "object.create.poll".into()].into();
        assert!(
            Runtime::with_backend(backend)
                .unwrap()
                .create_guest(&wasm(), required.clone(), Box::new(CreationHost(Arc::default())))
                .is_ok()
        );
        struct NoData;
        impl Host for NoData {
            fn supported_operations(&self) -> &[Opcode] {
                &[]
            }
        }
        assert!(
            Runtime::with_backend(backend)
                .unwrap()
                .create_guest(&wasm(), required, Box::new(NoData))
                .is_err()
        );
    }
}

#[test]
fn replacement_rejects_old_creation_callback_even_when_host_token_is_reused() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let previous = Arc::new(Mutex::new(Records::default()));
        let mut managed = ManagedGuest::create(
            Runtime::with_backend(backend).unwrap(),
            &wasm(),
            context(),
            Box::new(CreationHost(previous.clone())),
            |_| Ok(()),
        )
        .unwrap();
        let old_generation = managed.generation();
        previous.lock().unwrap().results.insert(FIRST_TOKEN, Ok(OBJECT));
        managed.notify_creation(old_generation, FIRST_TOKEN).unwrap();
        let current = Arc::new(Mutex::new(Records::default()));
        managed
            .replace(&wasm(), None, BTreeSet::new(), Box::new(CreationHost(current.clone())))
            .unwrap();
        let generation = managed.generation();
        assert_ne!(generation, old_generation);
        assert_eq!(
            managed.notify_creation(old_generation, FIRST_TOKEN),
            Err(HostError::StaleHandle)
        );
        managed.pump().unwrap();
        assert!(managed.logs().is_empty());
        assert_eq!(current.lock().unwrap().polls, [FIRST_TOKEN]);
        current.lock().unwrap().results.insert(FIRST_TOKEN, Ok(OBJECT));
        managed.notify_creation(generation, FIRST_TOKEN).unwrap();
        managed.pump().unwrap();
        assert_eq!(managed.logs().back().unwrap(), "Created");
    }
}

#[test]
fn creation_notification_can_retry_after_event_queue_is_drained() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        let handle = begin(&mut guest);
        let generation = guest.generation();
        for _ in 1..MAX_QUEUED_EVENTS {
            guest
                .queue_event(
                    generation,
                    Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
                )
                .unwrap();
        }
        assert_eq!(guest.notify_creation(generation, FIRST_TOKEN), Err(HostError::LimitExceeded));
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            1
        );
        assert_eq!(call(&mut guest, "event", (1024u32, 1056u32, 1024u32)), 0);
        guest.notify_creation(generation, FIRST_TOKEN).unwrap();
        assert_eq!(guest.store.data().app.events.len(), MAX_QUEUED_EVENTS);
        assert_eq!(
            Reader::new(&guest.store.data().app.events.back().unwrap().event.data)
                .unwrap()
                .operation_handle()
                .unwrap(),
            handle
        );
        guest.notify_creation(generation, FIRST_TOKEN).unwrap();
        assert_eq!(guest.store.data().app.events.len(), MAX_QUEUED_EVENTS);
    }
}
