#![allow(clippy::unwrap_used, reason = "Tests assert successful setup and completion.")]
use super::*;
use crate::runtime::host::{
    AvailableVolume, FieldChange, GuestContext, Host, Identity, ObjectRecord, Value, VolumeRights,
};
use playbit_api::abi::syscall::{EventKind, Id, MAX_PENDING_OPERATIONS};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

const ROOT: Id = [1; 16];
const OBJECT: Id = [2; 16];
const FIELD: Id = [3; 16];
const TYPE: Id = [4; 16];

#[derive(Default)]
pub(in crate::runtime) struct Records {
    pub(in crate::runtime) calls: Vec<Arc<DataCall>>,
    pub(in crate::runtime) results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
    pub(in crate::runtime) peeks: Vec<u64>,
    pub(in crate::runtime) released: Vec<u64>,
    file_capacity: Arc<crate::runtime::host::files::capacity::Capacity>,
    file_roots: BTreeMap<Id, crate::runtime::host::files::Root>,
    next_root: u64,
    pub(in crate::runtime) file_calls: Vec<Arc<crate::runtime::host::files::Call>>,
    pub(in crate::runtime) file_results:
        BTreeMap<u64, HostResult<Arc<crate::runtime::host::files::Output>>>,
    pub(in crate::runtime) file_retention:
        BTreeMap<u64, crate::runtime::host::files::capacity::Retention>,
    pub(in crate::runtime) file_released: Vec<(u64, bool)>,
    pub(in crate::runtime) file_cleanups:
        Vec<(u64, bool, crate::runtime::host::files::capacity::Retention)>,
    pub(in crate::runtime) file_peeks: usize,
}
pub(in crate::runtime) struct TestHost(pub(in crate::runtime) Rc<RefCell<Records>>);
impl Host for TestHost {
    fn file_capacity(&self) -> Arc<crate::runtime::host::files::capacity::Capacity> {
        self.0.borrow().file_capacity.clone()
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::BloblogAppendBegin,
            Opcode::BloblogAppendTake,
            Opcode::BloblogReadBegin,
            Opcode::BloblogReadTake,
            Opcode::KvKeysBegin,
            Opcode::KvKeysTake,
            Opcode::KvReadBegin,
            Opcode::KvReadTake,
            Opcode::KvPutBegin,
            Opcode::KvPutTake,
            Opcode::KvDeleteBegin,
            Opcode::KvDeleteTake,
            Opcode::EntryResolveBegin,
            Opcode::EntryResolveTake,
            Opcode::SchemaReadBegin,
            Opcode::SchemaReadTake,
            Opcode::SchemaEstablishBegin,
            Opcode::SchemaEstablishTake,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
            Opcode::ObjectQueryBegin,
            Opcode::ObjectQueryTake,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
            Opcode::FileOpenBegin,
            Opcode::FileOpenTake,
            Opcode::FileStatusBegin,
            Opcode::FileStatusTake,
            Opcode::FileCloseBegin,
            Opcode::FileCloseTake,
            Opcode::FileListOpenBegin,
            Opcode::FileListOpenTake,
            Opcode::FileListNextBegin,
            Opcode::FileListNextTake,
            Opcode::FileListClose,
            Opcode::OperationDispose,
        ]
    }
    fn file_roots(
        &mut self,
        volumes: &[AvailableVolume],
    ) -> HostResult<Vec<crate::runtime::host::files::Root>> {
        let mut records = self.0.borrow_mut();
        records.file_roots.retain(|id, _| volumes.iter().any(|volume| volume.id == *id));
        let mut roots = Vec::new();
        for volume in volumes {
            let writable = volume.rights.contains(VolumeRights::WRITE);
            let root = if let Some(root) =
                records.file_roots.get(&volume.id).filter(|root| root.writable == writable)
            {
                root.clone()
            } else {
                records.next_root += 1;
                let mut grant = volume.id;
                if records.next_root != 1 {
                    grant[8..].copy_from_slice(&records.next_root.to_le_bytes());
                }
                let root = crate::runtime::host::files::Root {
                    replaces: None,
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    volume: volume.id,
                    grant,
                    engine: 9 + records.next_root,
                    writable,
                    credit: records
                        .file_capacity
                        .one(crate::runtime::host::files::capacity::Kind::File)?,
                };
                records.file_roots.insert(volume.id, root.clone());
                root
            };
            roots.push(root);
        }
        Ok(roots)
    }
    fn begin_file(
        &mut self,
        _: u64,
        call: Arc<crate::runtime::host::files::Call>,
        retention: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<u64> {
        let mut records = self.0.borrow_mut();
        records.file_calls.push(call);
        let token = (1 << 63) | records.file_calls.len() as u64;
        records.file_retention.insert(token, retention);
        Ok(token)
    }
    fn peek_file(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::files::Output>> {
        let mut records = self.0.borrow_mut();
        records.file_peeks += 1;
        records.file_results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_file(&mut self, token: u64, claimed: bool) {
        let mut records = self.0.borrow_mut();
        records.file_released.push((token, claimed));
        if records.file_results.remove(&token).is_some() {
            records.file_retention.remove(&token);
        }
    }
    fn discard_file(
        &mut self,
        file: u64,
        list: bool,
        retention: crate::runtime::host::files::capacity::Retention,
    ) -> HostResult<()> {
        self.0.borrow_mut().file_cleanups.push((file, list, retention));
        Ok(())
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.borrow_mut();
        records.calls.push(call);
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        let mut records = self.0.borrow_mut();
        records.peeks.push(token);
        records.results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        let mut records = self.0.borrow_mut();
        records.results.remove(&token);
        records.released.push(token);
    }
}

pub(in crate::runtime) fn state() -> (State, Rc<RefCell<Records>>, VolumeHandle) {
    let records = Rc::new(RefCell::new(Records::default()));
    let context = GuestContext {
        app_name: "Async fixture".into(),
        app_build: None,
        identity: Identity {
            display_name: String::new(),
            account_id: [5; 16],
            is_service: false,
            device_id: [6; 16],
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
    };
    let state = State::new(context, Box::new(TestHost(Rc::clone(&records)))).unwrap();
    let volume = state.volume_handle(ROOT).unwrap();
    (state, records, volume)
}
fn read() -> DataOperation {
    DataOperation::Read { entry_id: [5; 15], object_id: OBJECT, fields: vec![FIELD] }
}

#[test]
fn schema_work_yields_retains_short_results_and_consumes_terminal_denial() {
    let (mut state, records, volume) = state();
    let read = begin(
        &mut state,
        volume,
        DataOperation::SchemaRead { entry_id: [5; 15] },
        Opcode::SchemaReadBegin,
    )
    .unwrap();
    assert_eq!(
        take(&mut state, read, Opcode::SchemaReadTake, |_, _| Ok(())),
        Err(HostError::WouldBlock)
    );
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::Schema(Default::default()))));
    assert_eq!(
        take(&mut state, read, Opcode::SchemaReadTake, |_, _| Err::<(), _>(
            HostError::BufferTooSmall
        )),
        Err(HostError::BufferTooSmall)
    );
    assert!(records.borrow().released.is_empty());
    assert_eq!(take(&mut state, read, Opcode::SchemaReadTake, |_, _| Ok(())), Ok(()));
    assert_eq!(records.borrow().released, vec![1]);
    let schema = begin(
        &mut state,
        volume,
        DataOperation::SchemaEstablish { entry_id: [5; 15], schema: Default::default() },
        Opcode::SchemaEstablishBegin,
    )
    .unwrap();
    records.borrow_mut().results.insert(2, Err(HostError::AccessDenied));
    assert_eq!(
        take(&mut state, schema, Opcode::SchemaEstablishTake, |_, _| Ok(())),
        Err(HostError::AccessDenied)
    );
    assert_eq!(records.borrow().released, vec![1, 2]);
    assert_eq!(
        take(&mut state, schema, Opcode::SchemaEstablishTake, |_, _| Ok(())),
        Err(HostError::StaleHandle)
    );
    assert_eq!(records.borrow().calls.len(), 2);
}
fn object(title: &str) -> Arc<DataResult> {
    Arc::new(DataResult::Object(ObjectRecord {
        id: OBJECT,
        type_id: TYPE,
        fields: vec![FieldChange {
            field_id: FIELD,
            values: Some(vec![Value::Text(title.into())]),
        }],
    }))
}

#[test]
fn short_output_retains_one_completion_and_ready_notice() {
    let (mut state, records, volume) = state();
    let pending = begin(&mut state, volume, read(), Opcode::ObjectReadBegin).unwrap();
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectReadTake, |_, _| unreachable!()),
        Err(HostError::WouldBlock)
    );
    records.borrow_mut().results.insert(1, Ok(object("committed snapshot")));
    state.notify_data(u64::from(state.generation), 1).unwrap();
    state.notify_data(u64::from(state.generation), 1).unwrap();
    assert_eq!(state.events.iter().filter(|event| event.kind == EventKind::DataReady).count(), 1);
    let mut retained = None;
    for _ in 0..3 {
        assert_eq!(
            take::<()>(&mut state, pending, Opcode::ObjectReadTake, |_, value| {
                retained = Some(value.clone());
                Err(HostError::BufferTooSmall)
            }),
            Err(HostError::BufferTooSmall)
        );
        assert!(
            state
                .capabilities
                .operation(crate::capability::Handle::new(pending.generation, pending.slot))
                .is_ok()
        );
        assert!(records.borrow().released.is_empty());
    }
    let delivered =
        take(&mut state, pending, Opcode::ObjectReadTake, |_, value| Ok(value.clone())).unwrap();
    assert_eq!(retained, Some(delivered));
    assert_eq!(records.borrow().calls.len(), 1);
    assert_eq!(records.borrow().released, vec![1]);
    assert_eq!(state.pending_data_bytes, 0);
    assert!(!state.events.iter().any(|event| event.kind == EventKind::DataReady));
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectReadTake, |_, _| unreachable!()),
        Err(HostError::StaleHandle)
    );
}

#[test]
fn wrong_handle_or_kind_cannot_consume_and_revocation_precedes_peek() {
    let (mut state, records, volume) = state();
    let pending = begin(&mut state, volume, read(), Opcode::ObjectReadBegin).unwrap();
    let stale = OperationHandle { generation: pending.generation + 1, ..pending };
    for (handle, opcode) in [(stale, Opcode::ObjectReadTake), (pending, Opcode::ObjectQueryTake)] {
        assert_eq!(
            take::<()>(&mut state, handle, opcode, |_, _| unreachable!()),
            Err(HostError::StaleHandle)
        );
    }
    assert!(records.borrow().peeks.is_empty());
    assert!(records.borrow().released.is_empty());
    records.borrow_mut().results.insert(1, Ok(object("private")));
    super::super::test_support::set_volume_rights(&mut state, 0, VolumeRights(0));
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectReadTake, |_, _| unreachable!()),
        Err(HostError::AccessDenied)
    );
    assert!(records.borrow().peeks.is_empty());
    assert_eq!(records.borrow().released, vec![1]);
}

#[test]
fn terminal_mutation_failure_is_consumed_without_replay() {
    let (mut state, records, volume) = state();
    let pending = begin(
        &mut state,
        volume,
        DataOperation::Edit {
            entry_id: [5; 15],
            object_id: OBJECT,
            fields: vec![FieldChange { field_id: FIELD, values: Some(vec![Value::Boolean(true)]) }],
        },
        Opcode::ObjectEditBegin,
    )
    .unwrap();
    records.borrow_mut().results.insert(1, Err(HostError::Unavailable));
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectEditTake, |_, _| unreachable!()),
        Err(HostError::Unavailable)
    );
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectEditTake, |_, _| unreachable!()),
        Err(HostError::StaleHandle)
    );
    assert_eq!(records.borrow().calls.len(), 1);
    assert_eq!(records.borrow().peeks, vec![1]);
    assert_eq!(records.borrow().released, vec![1]);
}

#[test]
fn withdrawn_mount_keeps_accepted_writes_pending_and_delivers_each_receipt_once() {
    // Exercise actual mount replacement between Begin and Take for all guest
    // writes. A retained acknowledgment is not authority to begin another call.
    for remove in [false, true] {
        for (operation, begin_opcode, take_opcode, receipt) in [
            (
                DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields: vec![] },
                Opcode::ObjectCreateBegin,
                Opcode::ObjectCreatePoll,
                DataResult::Created(OBJECT),
            ),
            (
                DataOperation::Edit {
                    entry_id: [5; 15],
                    object_id: OBJECT,
                    fields: vec![FieldChange {
                        field_id: FIELD,
                        values: Some(vec![Value::Boolean(true)]),
                    }],
                },
                Opcode::ObjectEditBegin,
                Opcode::ObjectEditTake,
                DataResult::Edited,
            ),
            (
                DataOperation::SchemaEstablish { entry_id: [5; 15], schema: Default::default() },
                Opcode::SchemaEstablishBegin,
                Opcode::SchemaEstablishTake,
                DataResult::SchemaEstablished,
            ),
        ] {
            let (mut state, records, _) = state();
            let mut volumes = state.context.volumes.clone();
            volumes.push(AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [9; 16],
                name: "Extra".into(),
                tags: vec![],
                rights: VolumeRights::ALL,
                is_root: false,
                role: 0,
            });
            state.replace_volumes(volumes.clone()).unwrap();
            let volume = state.volume_handle([9; 16]).unwrap();
            let pending = begin(&mut state, volume, operation.clone(), begin_opcode).unwrap();
            let charge = state.pending_data_bytes;
            if remove {
                volumes.pop();
            } else {
                volumes[1].rights = VolumeRights::READ;
            }
            state.replace_volumes(volumes).unwrap();
            assert_eq!(
                take::<()>(&mut state, pending, take_opcode, |_, _| unreachable!()),
                Err(HostError::WouldBlock)
            );
            assert_eq!(state.pending_data_bytes, charge);
            assert!(
                state
                    .capabilities
                    .operation(crate::capability::Handle::new(pending.generation, pending.slot))
                    .is_ok()
            );
            assert!(records.borrow().released.is_empty());
            records.borrow_mut().results.insert(1, Ok(Arc::new(receipt.clone())));
            assert_eq!(
                take(&mut state, pending, take_opcode, |_, result| Ok(result.clone())),
                Ok(receipt)
            );
            assert_eq!(state.pending_data_bytes, 0);
            assert_eq!(
                take::<()>(&mut state, pending, take_opcode, |_, _| unreachable!()),
                Err(HostError::StaleHandle)
            );
            assert_eq!(
                begin(&mut state, volume, operation, begin_opcode),
                Err(if remove { HostError::StaleHandle } else { HostError::AccessDenied })
            );
            assert_eq!(records.borrow().calls.len(), 1);
            assert_eq!(records.borrow().released, vec![1]);
        }
    }
}

#[test]
fn slot_and_owned_capacity_limits_precede_host_admission() {
    let (mut state, records, volume) = state();
    let mut handles = vec![];
    for _ in 0..MAX_PENDING_OPERATIONS {
        handles.push(begin(&mut state, volume, read(), Opcode::ObjectReadBegin).unwrap());
    }
    assert_eq!(
        begin(&mut state, volume, read(), Opcode::ObjectReadBegin),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(records.borrow().calls.len(), MAX_PENDING_OPERATIONS);
    records.borrow_mut().results.insert(1, Err(HostError::NotFound));
    assert_eq!(
        take::<()>(&mut state, handles[0], Opcode::ObjectReadTake, |_, _| unreachable!()),
        Err(HostError::NotFound)
    );
    let next = begin(&mut state, volume, read(), Opcode::ObjectReadBegin).unwrap();
    assert_eq!(next.slot, handles[0].slot);
    assert_ne!(next.generation, handles[0].generation);
    state.pending_data_bytes = MAX_RETAINED_DATA_BYTES;
    assert_eq!(
        begin(&mut state, volume, read(), Opcode::ObjectReadBegin),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(records.borrow().calls.len(), MAX_PENDING_OPERATIONS + 1);
}

#[test]
fn malformed_completion_is_terminal_and_unused_capacity_is_charged() {
    let (mut state, records, volume) = state();
    let pending = begin(&mut state, volume, read(), Opcode::ObjectReadBegin).unwrap();
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::Edited)));
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectReadTake, |_, _| unreachable!()),
        Err(HostError::Internal)
    );
    assert_eq!(records.borrow().released, vec![1]);
    let mut fields = Vec::with_capacity(64);
    fields.push(FIELD);
    let call = DataCall {
        volume: ROOT,
        operation: DataOperation::Read { entry_id: [5; 15], object_id: OBJECT, fields },
    };
    assert!(call.retained_bytes().unwrap() >= std::mem::size_of::<DataCall>() + 64 * 16);
}

#[test]
fn a_write_slot_never_delivers_read_plaintext_as_a_receipt() {
    // Wrong host result shape is consumed terminally even after mount denial.
    let (mut state, records, volume) = state();
    let pending = begin(
        &mut state,
        volume,
        DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields: vec![] },
        Opcode::ObjectCreateBegin,
    )
    .unwrap();
    super::super::test_support::set_volume_rights(&mut state, 0, VolumeRights(0));
    records.borrow_mut().results.insert(1, Ok(object("must not be delivered")));
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectCreatePoll, |_, _| unreachable!()),
        Err(HostError::Internal)
    );
    assert_eq!(records.borrow().released, vec![1]);
    assert_eq!(state.pending_data_bytes, 0);
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::ObjectCreatePoll, |_, _| unreachable!()),
        Err(HostError::StaleHandle)
    );
}

#[test]
fn creation_shares_retained_credit_and_its_ready_event_owns_the_same_slot() {
    let (mut state, records, volume) = state();
    let fields =
        vec![FieldChange { field_id: FIELD, values: Some(vec![Value::Text("initial".into())]) }];
    let pending = begin(
        &mut state,
        volume,
        DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields },
        Opcode::ObjectCreateBegin,
    )
    .unwrap();
    assert!(state.pending_data_bytes > 0);
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::Created(OBJECT))));
    state.notify_creation(u64::from(state.generation), 1).unwrap();
    assert!(state.events.iter().any(|event| event.kind == EventKind::ObjectCreationReady));
    assert_eq!(state.notify_data(u64::from(state.generation), 1), Err(HostError::StaleHandle));
    assert_eq!(
        super::super::handles::poll_operation(&mut state, pending, Opcode::ObjectCreatePoll),
        Ok(OBJECT)
    );
    assert_eq!(state.pending_data_bytes, 0);
    assert_eq!(records.borrow().released, vec![1]);
    assert_eq!(records.borrow().calls.len(), 1);
}

#[test]
fn full_initial_fields_cannot_bypass_retained_input_memory_limit() {
    let (mut state, records, volume) = state();
    let mut admitted = 0;
    loop {
        let fields = vec![FieldChange {
            field_id: FIELD,
            values: Some(vec![Value::Bytes(vec![0; playbit_api::abi::syscall::MAX_DATA_BYTES])]),
        }];
        match begin(
            &mut state,
            volume,
            DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields },
            Opcode::ObjectCreateBegin,
        ) {
            Ok(_) => admitted += 1,
            Err(HostError::LimitExceeded) => break,
            result => panic!("unexpected admission: {result:?}"),
        }
    }
    assert!(admitted > 0 && admitted < MAX_PENDING_OPERATIONS);
    assert_eq!(records.borrow().calls.len(), admitted);
    assert!(state.pending_data_bytes <= MAX_RETAINED_DATA_BYTES);
    assert!(
        state.pending_data_bytes
            > MAX_RETAINED_DATA_BYTES - playbit_api::abi::syscall::MAX_DATA_BYTES
    );
}

#[test]
fn guest_disposal_releases_local_credit_without_repeating_accepted_work() {
    let (mut state, records, volume) = state();
    begin(&mut state, volume, read(), Opcode::ObjectReadBegin).unwrap();
    drop(state);
    assert_eq!(records.borrow().calls.len(), 1);
    assert!(records.borrow().peeks.is_empty());
    assert_eq!(records.borrow().released, vec![1]);
}

#[cfg(feature = "wasm-runtime")]
#[test]
fn wasm_begin_copies_inputs_and_short_take_never_reexecutes_storage() {
    use super::super::test_support::{invoke as call, operation_handle, read as memory, write};
    use crate::runtime::{ExecutionBackend, Runtime};
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let (state, records, _) = state();
        let context = state.context.clone();
        drop(state);
        let wasm = wat::parse_str(r#"(module
            (import "playbit" "pb_syscall" (func $sys (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (memory (export "memory") 2)
            (func (export "pb_main"))
            (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
                (call $sys (local.get 0) (local.get 1) (local.get 2)
                    (local.get 3) (local.get 4) (local.get 5) (local.get 6))))"#).unwrap();
        let mut guest = Runtime::with_backend(backend)
            .unwrap()
            .create_guest(&wasm, context, Box::new(TestHost(Rc::clone(&records))))
            .unwrap();
        let volume = guest.volume_handle(ROOT).unwrap();
        let (entry, _) = crate::runtime::syscall::test_support::entry_pointers(&mut guest, volume);
        let packed = u64::from(entry);
        write(&mut guest, 4096, &OBJECT);
        write(&mut guest, 4128, &FIELD);
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::ObjectReadBegin as u32, packed, 4096u64, 4128u64, 1u64, 4200u64, 0u64)
            ),
            0
        );
        let pending = operation_handle(&mut guest, 4200);
        let packed = playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot);
        write(&mut guest, 4096, &[9; 16]);
        write(&mut guest, 4128, &[9; 16]);
        assert_eq!(records.borrow().calls[0].operation, read());
        records.borrow_mut().results.insert(1, Ok(object("snapshot")));
        let mut descriptor = vec![0; 68];
        descriptor[..4].copy_from_slice(&68_u32.to_le_bytes());
        for (index, value) in
            [8192u32, 1, 0, 40, 9216, 1, 0, 80, 10240, 0, 0].into_iter().enumerate()
        {
            descriptor[24 + index * 4..28 + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        write(&mut guest, 4300, &descriptor);
        write(&mut guest, 8192, &40_u32.to_le_bytes());
        write(&mut guest, 9216, &80_u32.to_le_bytes());
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::ObjectReadTake as u32, packed, 4300u64, 0u64, 0u64, 0u64, 0u64)
            ),
            HostError::BufferTooSmall as i32
        );
        assert_eq!(memory(&mut guest, 4364, 4), 8u32.to_le_bytes());
        assert!(records.borrow().released.is_empty());
        // Bytes overlapping the field output must fail before peeking/consuming.
        write(&mut guest, 4356, &8192u32.to_le_bytes());
        write(&mut guest, 4360, &16u32.to_le_bytes());
        let peeks = records.borrow().peeks.len();
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::ObjectReadTake as u32, packed, 4300u64, 0u64, 0u64, 0u64, 0u64)
            ),
            HostError::InvalidArgument as i32
        );
        assert_eq!(records.borrow().peeks.len(), peeks);
        assert!(records.borrow().released.is_empty());
        write(&mut guest, 4356, &10240u32.to_le_bytes());
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::ObjectReadTake as u32, packed, 4300u64, 0u64, 0u64, 0u64, 0u64)
            ),
            0
        );
        assert_eq!(memory(&mut guest, 10240, 8), b"snapshot");
        assert_eq!(records.borrow().calls.len(), 1);
        assert_eq!(records.borrow().released, vec![1]);
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::ObjectReadTake as u32, packed, 4300u64, 0u64, 0u64, 0u64, 0u64)
            ),
            HostError::StaleHandle as i32
        );
    }
}

#[cfg(feature = "wasm-runtime")]
#[test]
fn wasm_file_begin_and_take_admit_spans_and_keep_short_results_on_both_backends() {
    use super::super::test_support::{invoke as call, operation_handle, read as memory, write};
    use crate::runtime::host::files::Output;
    for mode in [crate::runtime::ExecutionBackend::Native, crate::runtime::ExecutionBackend::Pulley]
    {
        let (state, records, _) = state();
        let context = state.context.clone();
        let runtime = crate::runtime::Runtime::with_backend(mode).unwrap();
        let wasm = wat::parse_str(r#"(module
            (import "playbit" "pb_syscall" (func $call (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (memory (export "memory") 1)
            (func (export "pb_main"))
            (func (export "invoke") (param i32 i64 i64 i64 i64 i64 i64) (result i64)
                local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6
                call $call))"#).unwrap();
        let mut guest =
            runtime.create_guest(&wasm, context, Box::new(TestHost(Rc::clone(&records)))).unwrap();
        let app = &mut guest.store.data_mut().app;
        let root = app.files.enumerate(&mut app.capabilities, &[ROOT]).unwrap()[0];
        let root = root.0;
        write(&mut guest, 4096, b"note");
        // Invalid path memory starts no I/O.
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileOpenBegin as u32, root, 65535u64, 4u64, 1u64, 4200u64, 0u64)
            ),
            HostError::InvalidArgument as i32
        );
        assert!(records.borrow().file_calls.is_empty());
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileOpenBegin as u32, root, 4096u64, 4u64, 1u64, 4200u64, 0u64)
            ),
            0
        );
        let pending = operation_handle(&mut guest, 4200);
        let pending = playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot);
        records
            .borrow_mut()
            .file_results
            .insert((1 << 63) | 1, Ok(Arc::new(Output::File { handle: 11 })));
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileOpenTake as u32, pending, 4216u64, 0u64, 0u64, 0u64, 0u64)
            ),
            0
        );
        let file = operation_handle(&mut guest, 4216);
        let file = playbit_api::abi::dispatch::pack_handle(file.generation, file.slot);
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileReadBegin as u32, file, 2u64, 8u64, 4200u64, 0u64, 0u64)
            ),
            0
        );
        let pending = operation_handle(&mut guest, 4200);
        let pending = playbit_api::abi::dispatch::pack_handle(pending.generation, pending.slot);
        records
            .borrow_mut()
            .file_results
            .insert((1 << 63) | 2, Ok(Arc::new(Output::Read { bytes: b"snapshot".to_vec() })));
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileReadTake as u32, pending, 8192u64, 0u64, 4312u64, 0u64, 0u64)
            ),
            HostError::BufferTooSmall as i32
        );
        assert_eq!(memory(&mut guest, 4312, 4), 8u32.to_le_bytes());
        let peeks = records.borrow().file_peeks;
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileReadTake as u32, pending, 4312u64, 8u64, 4312u64, 0u64, 0u64)
            ),
            HostError::InvalidArgument as i32
        );
        assert_eq!(records.borrow().file_peeks, peeks);
        assert_eq!(
            call(
                &mut guest,
                "invoke",
                (Opcode::FileReadTake as u32, pending, 8192u64, 8u64, 4312u64, 0u64, 0u64)
            ),
            0
        );
        assert_eq!(memory(&mut guest, 8192, 8), b"snapshot");
        assert_eq!(records.borrow().file_calls.len(), 2);
        assert_eq!(
            records.borrow().file_released,
            vec![((1 << 63) | 1, true), ((1 << 63) | 2, false)]
        );
    }
}

#[test]
fn entry_resolution_retains_its_stable_capability_and_captured_mount() {
    let (mut state, records, volume) = state();
    let mut name = String::from("photos/café");
    let pending = begin(
        &mut state,
        volume,
        DataOperation::ResolveEntry { name: name.clone() },
        Opcode::EntryResolveBegin,
    )
    .unwrap();
    name.clear();
    assert_eq!(
        records.borrow().calls[0].operation,
        DataOperation::ResolveEntry { name: "photos/café".into() }
    );
    assert_eq!(resolve_entry_take(&mut state, pending), Err(HostError::WouldBlock));
    assert!(records.borrow().released.is_empty());
    records.borrow_mut().results.insert(1, Ok(Arc::new(DataResult::EntryResolved([7; 15]))));
    assert_eq!(
        take::<()>(&mut state, pending, Opcode::EntryResolveTake, |_, _| Err(
            HostError::BufferTooSmall
        )),
        Err(HostError::BufferTooSmall)
    );
    let entry = resolve_entry_take(&mut state, pending).unwrap();
    assert_eq!(entry.entry_id, [7; 15]);
    assert_eq!(
        playbit_api::abi::dispatch::unpack_handle(entry.volume),
        (volume.generation, volume.slot)
    );
    assert_eq!(records.borrow().released, [1]);
    let read = begin_entry(
        &mut state,
        entry,
        DataOperation::Read { entry_id: entry.entry_id, object_id: OBJECT, fields: vec![FIELD] },
        Opcode::ObjectReadBegin,
    )
    .unwrap();
    assert_eq!(records.borrow().calls[1].operation.entry_id(), Some([7; 15]));
    assert_eq!(
        take::<()>(&mut state, read, Opcode::ObjectReadTake, |_, _| Ok(())),
        Err(HostError::WouldBlock)
    );
    assert_eq!(resolve_entry_take(&mut state, pending), Err(HostError::StaleHandle));
}

#[test]
fn entry_resolution_denial_consumes_the_result_and_mismatched_ids_never_start() {
    let (mut state, records, volume) = state();
    let entry = playbit_api::context::EntryHandle {
        volume: playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot),
        entry_id: [7; 15],
        ..Default::default()
    };
    assert_eq!(
        begin_entry(
            &mut state,
            entry,
            DataOperation::Read { entry_id: [8; 15], object_id: OBJECT, fields: vec![] },
            Opcode::ObjectReadBegin
        ),
        Err(HostError::InvalidArgument)
    );
    assert!(records.borrow().calls.is_empty());
    let pending = begin(
        &mut state,
        volume,
        DataOperation::ResolveEntry { name: "data".into() },
        Opcode::EntryResolveBegin,
    )
    .unwrap();
    records.borrow_mut().results.insert(1, Err(HostError::AccessDenied));
    assert_eq!(resolve_entry_take(&mut state, pending), Err(HostError::AccessDenied));
    assert_eq!(records.borrow().released, [1]);
}
