//! Real scalar imports, cached Pulley wrappers, fresh bounded state per input.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct FuzzHost {
    calls: Arc<AtomicUsize>,
    data: BTreeMap<u64, Arc<crate::runtime::host::DataResult>>,
    next: u64,
}
impl FuzzHost {
    fn new(calls: Arc<AtomicUsize>) -> Self {
        Self { calls, data: BTreeMap::new(), next: 3 }
    }
}
impl Host for FuzzHost {
    fn allocate_generation(&mut self) -> HostResult<u32> {
        Ok(1)
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::SchemaReadBegin,
            Opcode::SchemaReadTake,
            Opcode::SchemaEstablishBegin,
            Opcode::SchemaEstablishTake,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
            Opcode::ObjectQueryBegin,
            Opcode::ObjectQueryTake,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::FileCopyBegin,
            Opcode::FileCopyPoll,
            Opcode::FileReadBegin,
            Opcode::FileReadTake,
            Opcode::FileWriteBegin,
            Opcode::FileWriteTake,
            Opcode::FileSelectBegin,
            Opcode::FileSelectPoll,
        ]
    }
    fn schema(&mut self, _: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
        Ok(Schema::default())
    }
    fn establish_schema(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        _: &Schema,
    ) -> HostResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn create_object(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        _: Id,
        _: &[FieldChange],
    ) -> HostResult<Id> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok([2; 16])
    }
    fn begin_data(&mut self, _: u64, call: Arc<crate::runtime::host::DataCall>) -> HostResult<u64> {
        use crate::runtime::host::{DataOperation, DataResult};
        // Controlled memory-only completions, without a backend call or I/O.
        let result = match &call.operation {
            DataOperation::Create { .. } => DataResult::Created([2; 16]),
            DataOperation::SchemaRead { .. } => DataResult::Schema(Schema::default()),
            DataOperation::SchemaEstablish { .. } => DataResult::SchemaEstablished,
            DataOperation::Read { object_id, fields, .. } => DataResult::Object(ObjectRecord {
                id: *object_id,
                type_id: [2; 16],
                fields: fields
                    .iter()
                    .map(|id| FieldChange { field_id: *id, values: None })
                    .collect(),
            }),
            DataOperation::Edit { .. } => DataResult::Edited,
            DataOperation::Query { projection, .. } => DataResult::Page(ObjectPage {
                objects: vec![],
                next: None,
                records: projection.as_ref().map(|_| vec![]),
            }),
            DataOperation::FileRead { offset, .. } => {
                DataResult::File(FileChunk { total_bytes: *offset, bytes: vec![] })
            }
            DataOperation::FileWrite { .. } => DataResult::Written,
            _ => return Err(HostError::Unsupported),
        };
        if call.operation.is_mutation() {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
        self.next = self.next.checked_add(1).ok_or(HostError::LimitExceeded)?;
        assert!(self.data.insert(self.next, Arc::new(result)).is_none());
        Ok(self.next)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<crate::runtime::host::DataResult>> {
        self.data.get(&token).cloned().ok_or(HostError::StaleHandle)
    }
    fn release_data(&mut self, token: u64) {
        self.data.remove(&token);
    }
    fn begin_file_copy(&mut self, _: u64, _: FileCopyRequest) -> HostResult<u64> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(4)
    }
    fn poll_file_copy(&mut self, _: u64) -> HostResult<[u8; 15]> {
        Ok([2; 15])
    }
    fn begin_file_selection(&mut self, _: u64) -> HostResult<u64> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(4)
    }
    fn poll_file_selection(&mut self, _: u64) -> HostResult<Option<FileSelection>> {
        Ok(None)
    }
    fn read_object(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        object_id: Id,
        fields: &[Id],
    ) -> HostResult<ObjectRecord> {
        Ok(ObjectRecord {
            id: object_id,
            type_id: [2; 16],
            fields: fields.iter().map(|id| FieldChange { field_id: *id, values: None }).collect(),
        })
    }
    fn read_authenticated_field(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        _: Id,
        _: Id,
    ) -> HostResult<Option<AuthenticatedFieldRead>> {
        Ok(None)
    }
    fn edit_fields(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        _: Id,
        _: &[FieldChange],
    ) -> HostResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn query_objects(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        _: Id,
        _: Option<Id>,
        _: u32,
        projection: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        Ok(ObjectPage { objects: Vec::new(), next: None, records: projection.map(|_| Vec::new()) })
    }
    fn read_file(
        &mut self,
        _: crate::runtime::host::EntryAddress,
        offset: u64,
        _: u32,
    ) -> HostResult<FileChunk> {
        Ok(FileChunk { total_bytes: offset, bytes: Vec::new() })
    }
    fn write_file(&mut self, _: crate::runtime::host::EntryAddress, _: &[u8]) -> HostResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [1; 16],
            is_service: false,
            device_id: [2; 16],
            instance_id: [3; 16],
            session_id: None,
        },
        volumes: vec![AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [1; 16],
            name: "Root".into(),
            tags: Vec::new(),
            rights: VolumeRights::ALL,
            is_root: true,
            role: 0,
        }],
        startup_arguments: [("test".into(), "value".into())].into(),
        trace_catalog: None,
        required_capabilities: BTreeSet::new(),
        participants: Vec::new(),
    }
}
fn state(calls: Arc<AtomicUsize>) -> State {
    let mut state = State::new(context(), Box::new(FuzzHost::new(calls)))
        .expect("valid deterministic fuzz context");
    let volume = state.volume_handle([1; 16]).expect("fixture root volume");
    let call = Arc::new(crate::runtime::host::DataCall {
        volume: [1; 16],
        operation: crate::runtime::host::DataOperation::Create {
            entry_id: [1; 15],
            type_id: [2; 16],
            fields: vec![],
        },
    });
    let bytes = call.retained_bytes().expect("bounded fixture");
    state.pending_data_bytes = bytes;
    state.work.active = Some(1);
    for pending in [
        PendingOperation::Data { token: 1, volume, call, take: Opcode::ObjectCreatePoll, bytes },
        PendingOperation::FileCopy { token: 2, destination_entry: [2; 15] },
        PendingOperation::FileSelection { token: 3 },
    ] {
        let handle = reserve_operation(&mut state).expect("bounded fixture reservation");
        register_operation(&mut state, handle, pending).expect("unique fixture operation");
    }
    state.work.active = None;
    state.pending_update = Some(PendingUpdate { generation: 2, build_id: [4; 32] });
    state
}

use crate::runtime::{ExecutionBackend, Runtime, TEST_FUEL};
use ValType::{I32, I64};
use std::cell::RefCell;
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
};

// Exercise the actual stable boundary, including unknown opcodes and noncanonical
// scalar slots. No typed wrapper may truncate or sanitize fuzz inputs.
fn module() -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([I32, I64, I64, I64, I64, I64, I64], [I64]);
    types.ty().function([], []);
    let mut imports = ImportSection::new();
    imports.import("playbit", "pb_syscall", EntityType::Function(0));
    let mut functions = FunctionSection::new();
    functions.function(1);
    let mut exports = ExportSection::new();
    exports.export("pb_syscall", ExportKind::Func, 0);
    exports.export("pb_main", ExportKind::Func, 1);
    exports.export("memory", ExportKind::Memory, 0);
    let mut code = CodeSection::new();
    let mut body = Function::new([]);
    body.instruction(&Instruction::End);
    code.function(&body);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 32,
        maximum: Some(32),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&exports);
    module.section(&code);
    module.finish()
}
struct Fixture {
    guest: GuestInstance,
    calls: Arc<AtomicUsize>,
}
fn output_layout(opcode: Opcode) -> Option<crate::runtime::linear_memory::record::Layout> {
    use crate::runtime::linear_memory::layout::{
        authenticated as a, data as d, environment as e, files as f, operations as o, pending as p,
        schema as s,
    };
    Some(match opcode {
        Opcode::AvailableVolumes => d::volumes::LAYOUT,
        Opcode::Capabilities => e::capabilities::LAYOUT,
        Opcode::StartupArguments => e::arguments::LAYOUT,
        Opcode::EventPoll => o::event::LAYOUT,
        Opcode::Identity => o::identity::LAYOUT,
        Opcode::ObjectReadTake => d::object::LAYOUT,
        Opcode::ObjectQueryTake => d::page::LAYOUT,
        Opcode::SchemaReadTake => s::schema::LAYOUT,
        Opcode::FileSelectPoll => p::selection::LAYOUT,
        Opcode::FileStatusTake => f::file_status::LAYOUT,
        Opcode::FileCloseTake => f::file_close_result::LAYOUT,
        Opcode::FileListNextTake => f::file_list_entry::LAYOUT,
        Opcode::EntryResolveTake => d::entry_handle::LAYOUT,
        Opcode::ObjectFieldReadAuthenticatedTake => a::field::LAYOUT,
        _ => return None,
    })
}
type Group = (usize, crate::runtime::linear_memory::record::Layout, u32);
fn output_groups(opcode: Opcode) -> &'static [Group] {
    use crate::runtime::linear_memory::layout::{data as d, environment as e, schema as s};
    match opcode {
        Opcode::AvailableVolumes => &[(16, d::volume::LAYOUT, 256), (32, d::range::LAYOUT, 16384)],
        Opcode::Capabilities => &[(8, d::range::LAYOUT, 256)],
        Opcode::StartupArguments => &[(8, e::argument::LAYOUT, 32)],
        Opcode::ObjectReadTake => &[(24, d::field::LAYOUT, 256), (40, d::value::LAYOUT, 4096)],
        Opcode::ObjectQueryTake => &[
            (32, d::object_info::LAYOUT, 64),
            (48, d::field::LAYOUT, 16384),
            (64, d::value::LAYOUT, abi::MAX_PAGE_VALUES),
        ],
        Opcode::SchemaReadTake => &[(8, s::type_record::LAYOUT, 256), (24, s::field::LAYOUT, 4096)],
        Opcode::ObjectFieldReadAuthenticatedTake => &[(12, d::value::LAYOUT, 4096)],
        _ => &[],
    }
}
fn cleared_output(input: &[u8], length: usize) -> HostResult<Vec<u8>> {
    use crate::runtime::linear_memory::{Memory, layout::authenticated::metadata, output::Buffer};
    let opcode = Opcode::from_raw(u32::from_le_bytes(input[..4].try_into().expect("opcode")))
        .ok_or(HostError::Unsupported)?;
    let layout = output_layout(opcode).ok_or(HostError::Unsupported)?;
    let slot = usize::from(!matches!(
        opcode,
        Opcode::AvailableVolumes
            | Opcode::Capabilities
            | Opcode::StartupArguments
            | Opcode::EventPoll
            | Opcode::Identity
    ));
    let pointer = u64::from_le_bytes(input[4 + slot * 8..12 + slot * 8].try_into().expect("slot"));
    let pointer = u32::try_from(pointer).map_err(|_| HostError::InvalidArgument)?;
    let mut data = vec![0; length];
    data[..input.len() - 52].copy_from_slice(&input[52..]);
    let mut memory = Memory { data: &mut data, fuel: None };
    let descriptor = memory.record(pointer, layout, false, None)?;
    let mut buffers = Vec::new();
    for &(offset, layout, limit) in output_groups(opcode) {
        buffers.push(Buffer::records(&mut memory, descriptor.sub(offset, 16), layout, limit)?);
    }
    if opcode == Opcode::ObjectFieldReadAuthenticatedTake {
        let metadata =
            memory.record(descriptor.u32(memory.data, 8), metadata::LAYOUT, false, None)?;
        metadata.clear(memory.data);
    }
    descriptor.clear(memory.data);
    for buffer in buffers {
        buffer.clear(memory.data);
    }
    Ok(data)
}
impl Fixture {
    fn new() -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = Runtime::with_backend_and_fuel_limit(
            ExecutionBackend::Pulley,
            std::num::NonZeroU64::new(TEST_FUEL),
        )
        .expect("Pulley fuzz runtime");
        let guest = runtime
            .create_guest(&module(), context(), Box::new(FuzzHost::new(calls.clone())))
            .expect("scalar syscall imports link");
        Self { guest, calls }
    }
    fn run(&mut self, input: &[u8]) {
        let opcode = u32::from_le_bytes(input[..4].try_into().expect("opcode"));
        let mut params = vec![wasmtime::Val::I32(opcode as i32)];
        for slot in input[4..52].as_chunks::<8>().0 {
            params.push(wasmtime::Val::I64(i64::from_le_bytes(*slot)));
        }
        self.calls.store(0, Ordering::Relaxed);
        self.guest.store.data_mut().app = state(self.calls.clone());
        self.guest.store.set_fuel(TEST_FUEL).expect("fuel enabled");
        let memory = self
            .guest
            .instance
            .get_memory(&mut self.guest.store, "memory")
            .expect("fixture memory");
        let data = memory.data_mut(&mut self.guest.store);
        data.fill(0);
        data[..input.len() - 52].copy_from_slice(&input[52..]);
        let function = self
            .guest
            .instance
            .get_func(&mut self.guest.store, "pb_syscall")
            .expect("operation wrapper");
        let mut results = [wasmtime::Val::I64(0)];
        function
            .call(&mut self.guest.store, &params, &mut results)
            .expect("bounded wrapper has ample fuel and cannot trap");
        let status = results[0].i64().expect("integer status");
        if status < 0 && status != HostError::BufferTooSmall as i64 {
            let data = memory.data(&self.guest.store);
            let unchanged = data[..input.len() - 52] == input[52..]
                && data[input.len() - 52..].iter().all(|byte| *byte == 0);
            if !unchanged {
                // A failed call may clear only initialized extents of valid caller
                // records. Payload, size, strides and unknown extension bytes stay exact.
                let expected = cleared_output(input, data.len()).expect("admitted record output");
                assert_eq!(data, expected, "failed operation changed non-extent guest bytes");
            }
        }
        if status < 0 {
            assert_eq!(
                self.calls.load(Ordering::Relaxed),
                0,
                "failed operation reached a host mutation"
            );
        }
        let state = &self.guest.store.data().app;
        state.assert_invariants();
        assert_eq!(
            state.queued_bytes,
            state.events.iter().map(|queued| 20 + queued.event.data.len()).sum::<usize>()
        );
        assert!(
            state.capabilities.count(crate::runtime::capabilities::OPERATION)
                <= abi::MAX_PENDING_OPERATIONS
        );
        assert_eq!(state.capabilities.table.reserved(), 0, "every failed Begin cancels admission");
        assert_eq!(state.view.is_some(), state.view_bytes.is_some());
    }
}
thread_local! {static FIXTURE:RefCell<Fixture>=RefCell::new(Fixture::new());}
/// Input: opcode u32, six scalar u64 arguments, then initial guest memory.
/// Every call starts with fresh authority/queues/handles and zeroed 2-MiB memory.
pub fn wasm_operations(input: &[u8]) {
    if !(52..=abi::MAX_DATA_BYTES).contains(&input.len()) {
        return;
    }
    FIXTURE.with(|fixture| fixture.borrow_mut().run(input));
}
