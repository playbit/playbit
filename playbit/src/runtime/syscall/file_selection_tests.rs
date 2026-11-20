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
const FIELD: Id = [3; 16];
const TOKEN: u64 = 91;
const SELECTED: FileSelection =
    FileSelection { address: EntryAddress { volume_id: ROOT, entry_id: [5; 15] } };

#[derive(Default)]
struct Records {
    begins: Vec<u64>,
    polls: Vec<u64>,
    results: BTreeMap<u64, HostResult<Option<FileSelection>>>,
    creations: u64,
    copies: u64,
    other_polls: usize,
    begin_error: Option<HostError>,
}
struct SelectionHost(Arc<Mutex<Records>>);
impl Host for SelectionHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::FileSelectBegin,
            Opcode::FileSelectPoll,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::FileCopyBegin,
            Opcode::FileCopyPoll,
        ]
    }
    fn begin_file_selection(&mut self, generation: u64) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        if let Some(error) = records.begin_error {
            return Err(error);
        }
        let token = TOKEN + records.begins.len() as u64;
        records.begins.push(generation);
        Ok(token)
    }
    fn poll_file_selection(&mut self, token: u64) -> HostResult<Option<FileSelection>> {
        let mut records = self.0.lock().unwrap();
        records.polls.push(token);
        records.results.get(&token).copied().unwrap_or(Err(HostError::WouldBlock))
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
        let token = TOKEN + records.creations;
        records.creations += 1;
        Ok(token)
    }
    fn begin_file_copy(&mut self, _: u64, _: FileCopyRequest) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        let token = TOKEN + records.copies;
        records.copies += 1;
        Ok(token)
    }
    fn peek_data(
        &mut self,
        _: u64,
    ) -> HostResult<std::sync::Arc<crate::runtime::host::DataResult>> {
        let result: HostResult<[u8; 16]> = {
            self.0.lock().unwrap().other_polls += 1;
            Err(HostError::WouldBlock)
        };
        result.map(|id| std::sync::Arc::new(crate::runtime::host::DataResult::Created(id)))
    }
    fn poll_file_copy(&mut self, _: u64) -> HostResult<[u8; 15]> {
        self.0.lock().unwrap().other_polls += 1;
        Err(HostError::WouldBlock)
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [10; 16],
            is_service: false,
            device_id: [11; 16],
            instance_id: [14; 16],
            session_id: Some([15; 32]),
        },
        volumes: vec![
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: ROOT,
                name: "Root".into(),
                tags: vec![],
                rights: VolumeRights::ALL,
                is_root: true,
                role: 0,
            },
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [6; 16],
                name: "Write only".into(),
                tags: vec![],
                rights: VolumeRights::WRITE,
                is_root: false,
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
    wat::parse_str(include_str!("../../../tests/fixtures/v1-file-selection.wat")).unwrap()
}
fn guest(backend: ExecutionBackend) -> (GuestInstance, Arc<Mutex<Records>>) {
    let records = Arc::new(Mutex::new(Records::default()));
    let guest = Runtime::with_backend(backend)
        .unwrap()
        .create_guest(&wasm(), context(), Box::new(SelectionHost(records.clone())))
        .unwrap();
    (guest, records)
}
fn begin_status(guest: &mut GuestInstance) -> i32 {
    write(guest, 4096, &TYPE);
    write(guest, 4112, &FIELD);
    write(guest, 4200, &[0xa5; 16]);
    let status = call(guest, "begin", 4200u32);
    if status < 0 {
        assert_eq!(read(guest, 4200, 16), [0xa5; 16]);
    }
    status
}
fn begin(guest: &mut GuestInstance) -> OperationHandle {
    assert_eq!(begin_status(guest), 0);
    operation_handle(guest, 4200)
}
fn poll(guest: &mut GuestInstance, handle: OperationHandle) -> (i32, Vec<u8>) {
    write(guest, 4304, &selection_bytes(0xa5, 0));
    let status = call(guest, "poll", (u64::from(handle.generation), handle.slot, 4304u32));
    (status, read(guest, 4304, 40))
}

fn selection_bytes(payload: u8, written: u32) -> Vec<u8> {
    let mut bytes = vec![payload; 40];
    bytes[..4].copy_from_slice(&40u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&written.to_le_bytes());
    bytes
}

#[test]
fn same_wasm_receives_selected_reference_or_explicit_cancellation() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        for selection in [Some(SELECTED), None] {
            let (mut guest, records) = guest(backend);
            let generation = guest.generation();
            assert!(guest.pump().unwrap().logs().is_empty());
            assert_eq!(records.lock().unwrap().begins, [generation]);
            assert_eq!(records.lock().unwrap().polls, [TOKEN]);
            records.lock().unwrap().results.insert(TOKEN, Ok(selection));
            guest.notify_file_selection(generation, TOKEN).unwrap();
            guest.notify_file_selection(generation, TOKEN).unwrap();
            assert_eq!(guest.store.data().app.events.len(), 1);
            let event = &guest.store.data().app.events.front().unwrap().event;
            assert_eq!(
                (event.kind, event.target, event.data.len()),
                (EventKind::FileSelectionReady, 0, 8)
            );
            assert_eq!(
                Reader::new(&event.data).unwrap().operation_handle().unwrap(),
                super::handles::operation_handle(
                    guest.store.data().app.capabilities.operations().next().unwrap().0.raw()
                )
            );
            assert_eq!(
                guest.pump().unwrap().logs(),
                [if selection.is_some() { "Selected" } else { "Cancelled" }]
            );
            assert!(guest.store.data().app.capabilities.operations().next().is_none());
            assert_eq!(guest.store.data().app.queued_bytes, 0);
            assert_eq!(guest.notify_file_selection(generation, TOKEN), Err(HostError::StaleHandle));
        }
    }
}

#[test]
fn selection_admits_fixed_records_before_opening_or_consuming() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        assert!(records.lock().unwrap().begins.is_empty());
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        let handle = begin(&mut guest);
        records.lock().unwrap().results.insert(TOKEN, Ok(None));
        guest.notify_file_selection(guest.generation(), TOKEN).unwrap();
        for output in [0u32, 4305, 131044] {
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
        assert_eq!(guest.store.data().app.events.len(), 2);
        assert_eq!(poll(&mut guest, handle), (0, selection_bytes(0, 40)));
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        assert_eq!(guest.store.data().app.events.len(), 1);

        let handle = begin(&mut guest);
        records.lock().unwrap().results.insert(TOKEN + 1, Ok(Some(SELECTED)));
        let (status, bytes) = poll(&mut guest, handle);
        assert_eq!(status, 0);
        let volume = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, volume);
        assert_eq!(&bytes[..4], &40u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &40u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &1u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &[0; 4]);
        assert_eq!(&bytes[16..20], &volume.slot.to_le_bytes());
        assert_eq!(&bytes[20..24], &volume.generation.to_le_bytes());
        assert_eq!(&bytes[24..39], &SELECTED.address.entry_id);
        assert_eq!(bytes[39], 0);
    }
}

#[test]
fn selection_rechecks_mounts_and_rejects_bad_host_bindings_without_reference_leaks() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let outcomes = [
            (
                Ok(Some(FileSelection {
                    address: EntryAddress { entry_id: [0; 15], ..SELECTED.address },
                })),
                HostError::Internal,
            ),
            (
                Ok(Some(FileSelection {
                    address: EntryAddress { volume_id: [0; 16], ..SELECTED.address },
                })),
                HostError::Internal,
            ),
            (
                Ok(Some(FileSelection {
                    address: EntryAddress { volume_id: [8; 16], ..SELECTED.address },
                })),
                HostError::AccessDenied,
            ),
            (
                Ok(Some(FileSelection {
                    address: EntryAddress { volume_id: [6; 16], ..SELECTED.address },
                })),
                HostError::AccessDenied,
            ),
            (Err(HostError::AccessDenied), HostError::AccessDenied),
            (Err(HostError::NotFound), HostError::NotFound),
            (Err(HostError::Unavailable), HostError::Unavailable),
        ];
        for (index, (result, expected)) in outcomes.into_iter().enumerate() {
            let handle = begin(&mut guest);
            let token = TOKEN + index as u64;
            assert_eq!(poll(&mut guest, handle).0, HostError::WouldBlock as i32);
            records.lock().unwrap().results.insert(token, result);
            guest.notify_file_selection(guest.generation(), token).unwrap();
            assert_eq!(poll(&mut guest, handle), (expected as i32, selection_bytes(0xa5, 0)));
            assert!(guest.store.data().app.capabilities.operations().next().is_none());
            assert_eq!(guest.store.data().app.events.len(), 1);
            assert_eq!(poll(&mut guest, handle).0, HostError::StaleHandle as i32);
        }
        let handle = begin(&mut guest);
        let token = TOKEN + outcomes.len() as u64;
        records.lock().unwrap().results.insert(token, Ok(Some(SELECTED)));
        super::test_support::set_volume_rights(
            &mut guest.store.data_mut().app,
            0,
            VolumeRights::WRITE,
        );
        assert_eq!(
            poll(&mut guest, handle),
            (HostError::AccessDenied as i32, selection_bytes(0xa5, 0))
        );
        assert!(guest.store.data().app.capabilities.operations().next().is_none());
        records.lock().unwrap().begin_error = Some(HostError::Unavailable);
        let slot = guest.store.data().app.capabilities.table.reserved();
        assert_eq!(begin_status(&mut guest), HostError::Unavailable as i32);
        assert_eq!(guest.store.data().app.capabilities.table.reserved(), slot);
    }
}

#[test]
fn all_three_operation_kinds_share_bounds_but_never_handles_or_notifications() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, records) = guest(backend);
        let volume = guest.volume_handle(ROOT).unwrap();
        let (_entry, _stale_entry) = super::test_support::entry_pointers(&mut guest, volume);
        let mut copy = Vec::new();
        for entry_id in [[4; 15], [5; 15]] {
            copy.extend(32u32.to_le_bytes());
            copy.extend(0u32.to_le_bytes());
            copy.extend(volume.slot.to_le_bytes());
            copy.extend(volume.generation.to_le_bytes());
            copy.extend(entry_id);
            copy.push(0);
        }
        write(&mut guest, 6000, &copy);
        let polls = ["poll", "create_poll", "copy_poll"];
        let handles: Vec<_> = (0..64)
            .map(|index| {
                let status = match index % 3 {
                    0 => begin_status(&mut guest),
                    1 => call(&mut guest, "create_begin", (_entry, 4096u32, 4200u32)),
                    2 => call(&mut guest, "copy_begin", (6000u32, 6032u32, 4200u32)),
                    _ => unreachable!(),
                };
                assert_eq!(status, 0);
                operation_handle(&mut guest, 4200)
            })
            .collect();
        assert_eq!(begin_status(&mut guest), HostError::LimitExceeded as i32);
        {
            let records = records.lock().unwrap();
            assert_eq!(records.begins.len() as u64 + records.creations + records.copies, 64);
        }
        write(&mut guest, 4304, &selection_bytes(0xa5, 0));
        for (index, handle) in handles.iter().take(3).enumerate() {
            for (other, name) in polls.iter().enumerate() {
                if other != index {
                    assert_eq!(
                        call(
                            &mut guest,
                            name,
                            (u64::from(handle.generation), handle.slot, 4304u32)
                        ),
                        HostError::StaleHandle as i32
                    );
                }
            }
        }
        assert_eq!(records.lock().unwrap().other_polls, 0);
        assert!(records.lock().unwrap().polls.is_empty());
        let generation = guest.generation();
        guest.notify_file_selection(generation, TOKEN).unwrap();
        guest.notify_creation(generation, TOKEN).unwrap();
        guest.notify_file_copy(generation, TOKEN).unwrap();
        let ready: Vec<_> = guest
            .store
            .data()
            .app
            .events
            .iter()
            .skip(1)
            .map(|queued| {
                let event = &queued.event;
                (event.kind, Reader::new(&event.data).unwrap().operation_handle().unwrap().slot)
            })
            .collect();
        assert_eq!(
            ready,
            [
                (EventKind::FileSelectionReady, handles[0].slot),
                (EventKind::ObjectCreationReady, handles[1].slot),
                (EventKind::FileCopyReady, handles[2].slot)
            ]
        );
        records.lock().unwrap().results.insert(TOKEN, Ok(None));
        assert_eq!(poll(&mut guest, handles[0]).0, 0);
        let next = begin(&mut guest);
        assert_eq!(next.slot, handles[0].slot);
        assert_ne!(next.generation, handles[0].generation);
        assert_eq!(poll(&mut guest, handles[0]).0, HostError::StaleHandle as i32);
    }
}

#[test]
fn selection_queue_pressure_and_stale_forged_or_wrong_kind_callbacks_are_safe() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (mut guest, _) = guest(backend);
        let handle = begin(&mut guest);
        let generation = guest.generation();
        for value in [
            OperationHandle { generation: handle.generation + 1, ..handle },
            OperationHandle { slot: 99, ..handle },
        ] {
            assert_eq!(
                poll(&mut guest, value),
                (HostError::StaleHandle as i32, selection_bytes(0xa5, 0))
            );
        }
        assert_eq!(guest.notify_file_selection(generation + 1, TOKEN), Err(HostError::StaleHandle));
        assert_eq!(guest.notify_file_selection(generation, TOKEN + 1), Err(HostError::StaleHandle));
        assert_eq!(guest.notify_creation(generation, TOKEN), Err(HostError::StaleHandle));
        assert_eq!(guest.notify_file_copy(generation, TOKEN), Err(HostError::StaleHandle));
        assert_eq!(
            guest.queue_event(
                generation,
                Event {
                    kind: EventKind::FileSelectionReady,
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
                .queue_event(generation, Event { kind: EventKind::Click, target: 1, data: vec![] })
                .unwrap();
        }
        assert_eq!(guest.notify_file_selection(generation, TOKEN), Err(HostError::LimitExceeded));
        assert_eq!(
            guest.store.data().app.capabilities.count(crate::runtime::capabilities::OPERATION),
            1
        );
        assert_eq!(call(&mut guest, "event", (1024u32, 1056u32, 1024u32)), 0);
        guest.notify_file_selection(generation, TOKEN).unwrap();
        guest.notify_file_selection(generation, TOKEN).unwrap();
        assert_eq!(guest.store.data().app.events.len(), MAX_QUEUED_EVENTS);
    }
}

#[test]
fn replacement_cannot_receive_old_native_selection_even_with_reused_host_token() {
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let previous = Arc::new(Mutex::new(Records::default()));
        let mut managed = ManagedGuest::create(
            Runtime::with_backend(backend).unwrap(),
            &wasm(),
            context(),
            Box::new(SelectionHost(previous)),
            |_| Ok(()),
        )
        .unwrap();
        let generation = managed.generation();
        managed.notify_file_selection(generation, TOKEN).unwrap();
        let replacement = Arc::new(Mutex::new(Records::default()));
        managed
            .replace(&wasm(), None, BTreeSet::new(), Box::new(SelectionHost(replacement.clone())))
            .unwrap();
        assert_eq!(managed.context().identity, context().identity);
        assert_eq!(managed.context().volumes, context().volumes);
        assert_eq!(managed.notify_file_selection(generation, TOKEN), Err(HostError::StaleHandle));
        assert_eq!(
            managed.notify_creation(managed.generation(), TOKEN),
            Err(HostError::StaleHandle)
        );
        replacement.lock().unwrap().results.insert(TOKEN, Ok(Some(SELECTED)));
        managed.notify_file_selection(managed.generation(), TOKEN).unwrap();
        managed.pump().unwrap();
        assert_eq!(managed.logs().iter().map(String::as_str).collect::<Vec<_>>(), ["Selected"]);
    }
}

#[test]
fn selection_capabilities_are_explicit_and_unsupported_hosts_do_not_advertise_them() {
    struct Unsupported;
    impl Host for Unsupported {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
    }
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let runtime = Runtime::with_backend(backend).unwrap();
        for (host, supported) in [
            (Box::new(Unsupported) as Box<dyn Host>, false),
            (Box::new(SelectionHost(Arc::new(Mutex::new(Records::default())))), true),
        ] {
            let mut guest = runtime.create_guest(&wasm(), context(), host).unwrap();
            let capabilities = capabilities(&mut guest);
            assert_eq!(capabilities.contains("file.select.begin"), supported);
            assert_eq!(capabilities.contains("file.select.poll"), supported);
        }
        let mut required = context();
        required.required_capabilities.insert("file.select.begin".into());
        assert!(runtime.create_guest(&wasm(), required, Box::new(Unsupported)).is_err());
    }
}
