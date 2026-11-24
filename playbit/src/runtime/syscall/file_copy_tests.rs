#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use super::test_support::{call, capabilities, operation_handle, read, write};
use super::*;
use crate::runtime::{ExecutionBackend, managed::ManagedGuest};
use std::sync::{Arc, Mutex};

const SOURCE: Id = [1; 16];
const DESTINATION: Id = [6; 16];
const FIRST_TOKEN: u64 = 91;
const REQUEST: FileCopyRequest = FileCopyRequest {
    source: EntryAddress { volume_id: SOURCE, entry_id: [2; 15] },
    destination: EntryAddress { volume_id: DESTINATION, entry_id: [4; 15] },
};

#[derive(Default)]
struct Records {
    copies: Vec<(u64, FileCopyRequest, u64)>,
    creations: Vec<u64>,
    polls: Vec<(EventKind, u64)>,
    results: BTreeMap<u64, HostResult<[u8; 15]>>,
    creation_results: BTreeMap<u64, HostResult<Id>>,
    begin_error: Option<HostError>,
}
struct CopyHost(Arc<Mutex<Records>>);
impl Host for CopyHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::FileCopyBegin,
            Opcode::FileCopyPoll,
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
        let _ = call.volume;
        let _ = type_id;
        let _ = fields;

        let mut records = self.0.lock().unwrap();
        let token = FIRST_TOKEN + records.creations.len() as u64;
        records.creations.push(token);
        Ok(token)
    }
    fn peek_data(
        &mut self,
        token: u64,
    ) -> HostResult<std::sync::Arc<crate::runtime::host::DataResult>> {
        let result: HostResult<[u8; 16]> = {
            let mut records = self.0.lock().unwrap();
            records.polls.push((EventKind::ObjectCreationReady, token));
            records.creation_results.get(&token).copied().unwrap_or(Err(HostError::WouldBlock))
        };
        result.map(|id| std::sync::Arc::new(crate::runtime::host::DataResult::Created(id)))
    }
    fn begin_file_copy(&mut self, generation: u64, request: FileCopyRequest) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        if let Some(error) = records.begin_error {
            return Err(error);
        }
        let token = FIRST_TOKEN + records.copies.len() as u64;
        records.copies.push((generation, request, token));
        Ok(token)
    }
    fn poll_file_copy(&mut self, token: u64) -> HostResult<[u8; 15]> {
        let mut records = self.0.lock().unwrap();
        records.polls.push((EventKind::FileCopyReady, token));
        records.results.get(&token).copied().unwrap_or(Err(HostError::WouldBlock))
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            display_name: String::new(),
            account_id: [10; 16],
            is_service: false,
            device_id: [11; 16],
            instance_id: [14; 16],
            session_id: None,
        },
        volumes: vec![
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: SOURCE,
                name: "S".into(),
                tags: vec![],
                rights: VolumeRights::READ,
                is_root: false,
                role: 0,
            },
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: DESTINATION,
                name: "D".into(),
                tags: vec![],
                rights: VolumeRights::WRITE,
                is_root: true,
                role: 0,
            },
        ],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: BTreeSet::new(),
        participants: vec![],
    }
}
fn wasm() -> Vec<u8> {
    wat::parse_str(include_str!("../../../tests/fixtures/v1-file-copy.wat")).unwrap()
}
fn guest(backend: ExecutionBackend) -> (GuestInstance, Arc<Mutex<Records>>) {
    let records = Arc::new(Mutex::new(Records::default()));
    let guest = Runtime::with_backend(backend)
        .unwrap()
        .create_guest(&wasm(), context(), Box::new(CopyHost(records.clone())))
        .unwrap();
    (guest, records)
}
fn references(guest: &GuestInstance) -> Vec<u8> {
    let mut bytes = Vec::new();
    for address in [REQUEST.source, REQUEST.destination] {
        let handle = guest.volume_handle(address.volume_id).unwrap();
        bytes.extend(32u32.to_le_bytes());
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(handle.slot.to_le_bytes());
        bytes.extend(handle.generation.to_le_bytes());
        bytes.extend(address.entry_id);
        bytes.push(0);
    }
    bytes
}
fn begin_status(guest: &mut GuestInstance, references: &[u8]) -> i32 {
    write(guest, 4096, references);
    write(guest, 4200, &[0xa5; 16]);
    let status = call(guest, "begin", (4096u32, 4128u32, 4200u32));
    if status < 0 {
        assert_eq!(read(guest, 4200, 16), [0xa5; 16]);
    }
    status
}
fn begin(guest: &mut GuestInstance) -> OperationHandle {
    let references = references(guest);
    assert_eq!(begin_status(guest, &references), 0);
    operation_handle(guest, 4200)
}
fn create(guest: &mut GuestInstance) -> OperationHandle {
    let volume = guest.volume_handle(DESTINATION).unwrap();
    let (_entry, _stale_entry) = super::test_support::entry_pointers(guest, volume);
    write(guest, 4096, &[7; 16]);
    assert_eq!(call(guest, "create_begin", (_entry, 4096u32, 4200u32)), 0);
    operation_handle(guest, 4200)
}
fn poll(guest: &mut GuestInstance, name: &str, handle: OperationHandle) -> (i32, Vec<u8>) {
    write(guest, 4300, &[0xa5; 16]);
    let status = call(guest, name, (u64::from(handle.generation), handle.slot, 4300u32));
    (status, read(guest, 4300, if name == "create_poll" { 16 } else { 15 }))
}
fn event_handle(event: &Event) -> OperationHandle {
    Reader::new(&event.data).unwrap().operation_handle().unwrap()
}

#[test]
fn real_guest_copies_between_explicit_mounts_after_handle_only_notification() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        assert!(guest.pump().unwrap().logs().is_empty());
        let generation = guest.generation();
        assert_eq!(records.lock().unwrap().copies, [(generation, REQUEST, FIRST_TOKEN)]);
        assert_eq!(records.lock().unwrap().polls, [(EventKind::FileCopyReady, FIRST_TOKEN)]);
        records.lock().unwrap().results.insert(FIRST_TOKEN, Ok(REQUEST.destination.entry_id));
        guest.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        guest.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        assert_eq!(guest.store.data().app.events.len(), 1);
        let event = &guest.store.data().app.events.front().unwrap().event;
        assert_eq!((event.kind, event.target, event.data.len()), (EventKind::FileCopyReady, 0, 8));
        assert_eq!(
            Reader::new(&event.data).unwrap().operation_handle().unwrap(),
            super::handles::operation_handle(
                guest.store.data().app.capabilities.operations().next().unwrap().0.raw()
            )
        );
        assert_eq!(guest.pump().unwrap().logs(), ["Copied"]);
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        assert_eq!(guest.store.data().app.queued_bytes, 0);
        assert_eq!(guest.notify_file_copy(generation, FIRST_TOKEN), Err(HostError::StaleHandle));
    }
}

#[test]
fn copy_admits_fixed_records_and_both_scopes_before_host_effects() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let valid = references(&guest);
        write(&mut guest, 4096, &valid);
        for args in [
            (0u32, 4128u32, 4200u32),
            (4096, 4129, 4200),
            (131052, 4128, 4200),
            (4096, 131052, 4200),
        ] {
            let before = read(&mut guest, 0, 131072);
            assert_eq!(call(&mut guest, "begin", args), HostError::InvalidArgument as i32);
            assert_eq!(read(&mut guest, 0, 131072), before);
        }
        for offset in [16, 48] {
            let mut zero_id = valid.clone();
            zero_id[offset..offset + 15].fill(0);
            assert_eq!(begin_status(&mut guest, &zero_id), HostError::InvalidArgument as i32);
        }
        let mut unmounted = valid.clone();
        unmounted[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(begin_status(&mut guest, &unmounted), HostError::StaleHandle as i32);
        let mut stale = valid.clone();
        stale[12..16].copy_from_slice(&(2u32).to_le_bytes());
        assert_eq!(begin_status(&mut guest, &stale), HostError::StaleHandle as i32);
        for (index, denied) in [(0, VolumeRights::WRITE), (1, VolumeRights::READ)] {
            let previous = guest.store.data().app.context.volumes[index].rights;
            super::test_support::set_volume_rights(&mut guest.store.data_mut().app, index, denied);
            assert_eq!(begin_status(&mut guest, &valid), HostError::AccessDenied as i32);
            super::test_support::set_volume_rights(
                &mut guest.store.data_mut().app,
                index,
                previous,
            );
        }
        super::test_support::set_volume_rights(
            &mut guest.store.data_mut().app,
            0,
            VolumeRights::ALL,
        );
        let mut same_object = valid.clone();
        same_object[40..48].copy_from_slice(&valid[8..16]);
        same_object[48..63].copy_from_slice(&REQUEST.source.entry_id);
        assert_eq!(begin_status(&mut guest, &same_object), HostError::InvalidArgument as i32);
        assert!(records.lock().unwrap().copies.is_empty());
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        let handle = begin(&mut guest);
        records.lock().unwrap().results.insert(FIRST_TOKEN, Ok(REQUEST.destination.entry_id));
        for output in [0u32, 131058] {
            let before = read(&mut guest, 0, 131072);
            assert_eq!(
                call(&mut guest, "poll", (u64::from(handle.generation), handle.slot, output)),
                HostError::InvalidArgument as i32
            );
            assert_eq!(read(&mut guest, 0, 131072), before);
        }
        assert!(records.lock().unwrap().polls.is_empty());
        assert_eq!(poll(&mut guest, "poll", handle), (0, REQUEST.destination.entry_id.to_vec()));
    }
}

#[test]
fn creation_and_copy_share_sixty_four_slots_with_separate_host_token_kinds() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let mut handles = Vec::new();
        for index in 0..abi::MAX_PENDING_OPERATIONS {
            handles.push(if index % 2 == 0 { create(&mut guest) } else { begin(&mut guest) });
        }
        let valid = references(&guest);
        assert_eq!(begin_status(&mut guest, &valid), HostError::LimitExceeded as i32);
        {
            let records = records.lock().unwrap();
            assert_eq!((records.copies.len(), records.creations.len()), (32, 32));
        }
        assert_eq!(poll(&mut guest, "poll", handles[0]).0, HostError::StaleHandle as i32);
        assert_eq!(poll(&mut guest, "create_poll", handles[1]).0, HostError::StaleHandle as i32);
        assert!(records.lock().unwrap().polls.is_empty());
        let generation = guest.generation();
        guest.notify_creation(generation, FIRST_TOKEN).unwrap();
        guest.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        assert_eq!(event_handle(&guest.store.data().app.events[1].event), handles[0]);
        assert_eq!(event_handle(&guest.store.data().app.events[2].event), handles[1]);
        records.lock().unwrap().creation_results.insert(FIRST_TOKEN, Ok([9; 16]));
        assert_eq!(poll(&mut guest, "create_poll", handles[0]).0, 0);
        assert_eq!(guest.notify_creation(generation, FIRST_TOKEN), Err(HostError::StaleHandle));
        guest.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        let later = begin(&mut guest);
        assert_eq!(later.slot, handles[0].slot);
        assert_ne!(later.generation, handles[0].generation);
        assert_eq!(poll(&mut guest, "poll", handles[0]).0, HostError::StaleHandle as i32);
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            64
        );
    }
}

#[test]
fn terminal_copy_results_consume_handles_hints_and_reject_wrong_destination_ids() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let generation = guest.generation();
        for (index, result) in [
            Err(HostError::AccessDenied),
            Err(HostError::NotFound),
            Err(HostError::Conflict),
            Err(HostError::Unavailable),
            Err(HostError::Internal),
            Ok([99; 15]),
        ]
        .into_iter()
        .enumerate()
        {
            let handle = begin(&mut guest);
            let token = FIRST_TOKEN + index as u64;
            assert_eq!(poll(&mut guest, "poll", handle).0, HostError::WouldBlock as i32);
            guest.notify_file_copy(generation, token).unwrap();
            records.lock().unwrap().results.insert(token, result);
            let expected = result.err().unwrap_or(HostError::Internal);
            assert_eq!(poll(&mut guest, "poll", handle), (expected as i32, vec![0xa5; 15]));
            assert!(guest.store.data().app.capabilities.operations().next().is_none());
            assert_eq!(guest.store.data().app.queued_bytes, 20);
            assert_eq!(poll(&mut guest, "poll", handle).0, HostError::StaleHandle as i32);
            assert_eq!(guest.notify_file_copy(generation, token), Err(HostError::StaleHandle));
        }
        records.lock().unwrap().begin_error = Some(HostError::Unavailable);
        let slot = guest.store.data().app.capabilities.table.reserved();
        let valid = references(&guest);
        assert_eq!(begin_status(&mut guest, &valid), HostError::Unavailable as i32);
        assert_eq!(guest.store.data().app.capabilities.table.reserved(), slot);
    }
}

#[test]
fn poll_admits_output_and_preserves_accepted_copy_receipts_after_mount_withdrawal() {
    // Copy completion carries the original destination ID, not plaintext or
    // authority for another write. Invalid output still precedes host polling.
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        for index in [0, 1] {
            let (mut guest, records) = guest(backend);
            let handle = begin(&mut guest);
            super::test_support::set_volume_rights(
                &mut guest.store.data_mut().app,
                index,
                VolumeRights::default(),
            );
            assert_eq!(
                call(&mut guest, "poll", (u64::from(handle.generation), handle.slot, 0u32)),
                HostError::InvalidArgument as i32
            );
            assert_eq!(
                guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
                1
            );
            assert!(records.lock().unwrap().polls.is_empty());
            assert_eq!(poll(&mut guest, "poll", handle).0, HostError::WouldBlock as i32);
            assert_eq!(
                guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
                1
            );
            records.lock().unwrap().results.insert(FIRST_TOKEN, Ok(REQUEST.destination.entry_id));
            assert_eq!(
                poll(&mut guest, "poll", handle),
                (0, REQUEST.destination.entry_id.to_vec())
            );
            assert!(guest.store.data().app.capabilities.operations().next().is_none());
            assert_eq!(records.lock().unwrap().polls.len(), 2);
            let valid = references(&guest);
            assert_eq!(begin_status(&mut guest, &valid), HostError::AccessDenied as i32);
            assert_eq!(poll(&mut guest, "poll", handle).0, HostError::StaleHandle as i32);
        }
    }
}

#[test]
fn copy_ready_queue_pressure_stale_generations_and_forged_handles_are_safe() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let handle = begin(&mut guest);
        let generation = guest.generation();
        for value in [
            OperationHandle { generation: handle.generation + 1, ..handle },
            OperationHandle { slot: 999, ..handle },
        ] {
            assert_eq!(
                poll(&mut guest, "poll", value),
                (HostError::StaleHandle as i32, vec![0xa5; 15])
            );
        }
        assert_eq!(
            guest.notify_file_copy(generation + 1, FIRST_TOKEN),
            Err(HostError::StaleHandle)
        );
        assert_eq!(
            guest.notify_file_copy(generation, FIRST_TOKEN + 1),
            Err(HostError::StaleHandle)
        );
        assert_eq!(guest.notify_creation(generation, FIRST_TOKEN), Err(HostError::StaleHandle));
        assert_eq!(
            guest.queue_event(
                generation,
                Event {
                    kind: EventKind::FileCopyReady,
                    target: 0,
                    data: [
                        handle.slot.to_le_bytes().as_slice(),
                        handle.generation.to_le_bytes().as_slice()
                    ]
                    .concat()
                }
            ),
            Err(HostError::InvalidArgument)
        );
        for _ in 1..MAX_QUEUED_EVENTS {
            guest
                .queue_event(
                    generation,
                    Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
                )
                .unwrap();
        }
        assert_eq!(guest.notify_file_copy(generation, FIRST_TOKEN), Err(HostError::LimitExceeded));
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            1
        );
        assert_eq!(call(&mut guest, "event", (1024u32, 1056u32, 1024u32)), 0);
        guest.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        guest.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        assert_eq!(guest.store.data().app.events.len(), MAX_QUEUED_EVENTS);
        assert_eq!(event_handle(&guest.store.data().app.events.back().unwrap().event), handle);
        assert!(records.lock().unwrap().polls.is_empty());
    }
}

#[test]
fn managed_replacement_rejects_old_copy_notifications_even_when_tokens_repeat() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let old = Arc::new(Mutex::new(Records::default()));
        let mut managed = ManagedGuest::create(
            Runtime::with_backend(backend).unwrap(),
            &wasm(),
            context(),
            Box::new(CopyHost(old)),
            |_| Ok(()),
        )
        .unwrap();
        let generation = managed.generation();
        managed.notify_file_copy(generation, FIRST_TOKEN).unwrap();
        let next = Arc::new(Mutex::new(Records::default()));
        managed.replace(&wasm(), None, BTreeSet::new(), Box::new(CopyHost(next.clone()))).unwrap();
        assert_eq!(managed.notify_file_copy(generation, FIRST_TOKEN), Err(HostError::StaleHandle));
        assert_eq!(
            managed.notify_creation(managed.generation(), FIRST_TOKEN),
            Err(HostError::StaleHandle)
        );
        assert!(managed.logs().is_empty());
        next.lock().unwrap().results.insert(FIRST_TOKEN, Ok(REQUEST.destination.entry_id));
        managed.notify_file_copy(managed.generation(), FIRST_TOKEN).unwrap();
        managed.pump().unwrap();
        assert_eq!(managed.logs().back().unwrap(), "Copied");
    }
}

#[test]
fn copy_capabilities_are_discovered_and_required_before_guest_start() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        let capabilities = capabilities(&mut guest);
        assert!(capabilities.contains("file.copy.begin"));
        assert!(capabilities.contains("file.copy.poll"));
        struct NoFiles;
        impl Host for NoFiles {
            fn supported_operations(&self) -> &[Opcode] {
                &[]
            }
        }
        let mut context = context();
        context.required_capabilities = ["file.copy.begin".into(), "file.copy.poll".into()].into();
        assert!(
            Runtime::with_backend(backend)
                .unwrap()
                .create_guest(&wasm(), context, Box::new(NoFiles))
                .is_err()
        );
    }
}
