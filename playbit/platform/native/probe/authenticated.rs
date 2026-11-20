//! Authenticated native reads preserve one atomic host snapshot and its provenance.
use super::*;
use playbit::runtime::syscall::{
    AuthenticatedFieldRead, FieldProvenance, FieldRevision, HostResult, Principal, Value,
};
use playbit_api::{
    abi::{authenticated as ffi, data, syscall::Error},
    context::{
        AuthenticatedField, AuthenticatedReadError, Completion, Context, OperationHandle,
        Principal as ApiPrincipal, Provenance, ValueRef,
    },
};
use std::sync::{Arc, Mutex};
unsafe extern "C" {
    fn probe_authenticated_read(
        context: *mut abi::Context,
        volume: *const data::EntryHandle,
        expected: *const ffi::Metadata,
        values: u32,
        bytes: u32,
    );
    fn probe_authenticated_layout(index: u32) -> u64;
}
struct Records {
    calls: usize,
    denied: bool,
    field: Option<AuthenticatedFieldRead>,
}
struct FieldHost {
    records: Arc<Mutex<Records>>,
    next: u64,
    snapshots: std::collections::BTreeMap<u64, HostResult<Arc<playbit::runtime::host::DataResult>>>,
}
impl FieldHost {
    fn new(records: Arc<Mutex<Records>>) -> Self {
        Self { records, next: 0, snapshots: Default::default() }
    }
}
impl Host for FieldHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::ObjectFieldReadAuthenticatedBegin, Opcode::ObjectFieldReadAuthenticatedTake]
    }
    fn begin_data(
        &mut self,
        _: u64,
        call: Arc<playbit::runtime::host::DataCall>,
    ) -> HostResult<u64> {
        use playbit::runtime::host::{DataOperation, DataResult};
        assert_eq!(call.volume, [54; 16]);
        assert_eq!(
            call.operation,
            DataOperation::AuthenticatedField {
                entry_id: [7; 15],
                object_id: [2; 16],
                field_id: [4; 16]
            }
        );
        let mut records = self.records.lock().unwrap();
        records.calls += 1;
        let snapshot = if records.denied {
            Err(HostError::AccessDenied)
        } else {
            Ok(Arc::new(DataResult::AuthenticatedField(records.field.clone())))
        };
        self.next += 1;
        self.snapshots.insert(self.next, snapshot);
        Ok(self.next)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<playbit::runtime::host::DataResult>> {
        self.snapshots.get(&token).unwrap().clone()
    }
    fn release_data(&mut self, token: u64) {
        assert!(self.snapshots.remove(&token).is_some());
    }
}
// This helper drives two real calls against the controlled memory-only host.
// It does not wait, replay a pending request or stand in for a production host.
fn snapshot<'a>(
    context: &mut Context<'_>,
    volume: data::EntryHandle,
    object: [u8; 16],
    field: [u8; 16],
    values: &'a mut [data::Value],
    bytes: &'a mut [u8],
) -> Result<Option<AuthenticatedField<'a>>, AuthenticatedReadError> {
    let handle = context.begin_read_authenticated_field(volume, object, field)?;
    let Completion::Ready(field) = context.take_authenticated_field(handle, values, bytes)? else {
        panic!("controlled snapshot must be ready")
    };
    Ok(field)
}
unsafe fn take_raw(
    raw: *mut abi::Context,
    volume: data::EntryHandle,
    output: *mut ffi::Field,
) -> i64 {
    let mut handle = OperationHandle::default();
    let begin = unsafe {
        probe_handle(
            ffi::pb_object_field_read_authenticated_begin(raw, &volume, &[2; 16], &[4; 16]),
            &mut handle,
        )
    };
    if begin < 0 {
        return begin;
    }
    unsafe { ffi::pb_object_field_read_authenticated_take(raw, handle, output) }
}

struct Probe {
    records: Arc<Mutex<Records>>,
    metadata: ffi::Metadata,
}
fn record() -> AuthenticatedFieldRead {
    AuthenticatedFieldRead {
        volume_id: [54; 16],
        entry_id: [7; 15],
        object_id: [2; 16],
        type_id: [3; 16],
        field_id: [4; 16],
        revision: FieldRevision([7; 32]),
        values: Some(vec![Value::Text("hello".into()), Value::Integer(42)]),
        provenance: FieldProvenance::Unproven,
    }
}
fn volume(ctx: &mut Context<'_>) -> data::EntryHandle {
    ctx.attach_files(&playbit_api::context::FileResources::default()).unwrap();
    let mut records = [data::Volume::default(); 1];
    let mut bytes = [0; 16];
    let volume = ctx
        .available_volumes(&mut records, &mut [], &mut bytes)
        .unwrap()
        .iter()
        .next()
        .unwrap()
        .handle;
    ctx.flush_files().unwrap();
    data::EntryHandle { volume: volume.0, entry_id: [7; 15], reserved: 0, ..Default::default() }
}
unsafe extern "C" fn init(_: *mut abi::Context, user: *mut c_void, state: *mut *mut c_void) -> i32 {
    unsafe {
        state.write(user);
    }
    0
}
unsafe extern "C" fn destroy(_: *mut c_void) {}
fn instance(probe: &mut Probe, turn: abi::Turn) -> NativeInstance {
    unsafe {
        NativeInstance::new(
            context(50),
            Box::new(FieldHost::new(probe.records.clone())),
            abi::App {
                init: Some(init),
                turn: Some(turn),
                destroy: Some(destroy),
                user_data: (probe as *mut Probe).cast(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
        .unwrap()
    }
}
unsafe extern "C" fn normal_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let vol = volume(&mut unsafe { Context::from_raw(raw, &mut context_state) }.unwrap().0);
    let nonempty = probe
        .records
        .lock()
        .unwrap()
        .field
        .as_ref()
        .and_then(|r| r.values.as_ref())
        .is_some_and(|v| !v.is_empty());
    unsafe {
        probe_authenticated_read(
            raw,
            &vol,
            &probe.metadata,
            if nonempty { 2 } else { 0 },
            if nonempty { 5 } else { 0 },
        );
    }
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let mut values = [data::Value::default(); 2];
    let mut bytes = [0; 32];
    let read = snapshot(&mut context, vol, [2; 16], [4; 16], &mut values, &mut bytes).unwrap();
    if probe.metadata.state == 0 {
        assert!(read.is_none());
        return 0;
    }
    let read = read.unwrap();
    assert_eq!(
        (read.volume_id, read.object_id, read.type_id, read.field.id),
        ([54; 16], [2; 16], [3; 16], [4; 16])
    );
    assert_eq!(read.revision, [7; 32]);
    let expected = match probe.metadata.provenance {
        0 => Provenance::Unproven,
        kind => {
            let principal = if probe.metadata.principal_kind == 1 {
                ApiPrincipal::Human([5; 16])
            } else {
                ApiPrincipal::Service([5; 16])
            };
            if kind == 1 {
                Provenance::ApplicationEdit { principal, device_id: [6; 16] }
            } else {
                Provenance::Rekey { principal, device_id: [6; 16] }
            }
        }
    };
    assert_eq!(read.provenance, expected);
    match read.field.values() {
        None => assert_eq!(probe.metadata.state, 1),
        Some(values) => {
            assert_eq!(probe.metadata.state, 2);
            let values: Vec<_> = values.collect();
            if nonempty {
                assert_eq!(values, vec![ValueRef::Text("hello"), ValueRef::Integer(42)]);
            } else {
                assert!(values.is_empty());
            }
        }
    }
    0
}
#[test]
fn c_and_rust_distinguish_absence_tombstone_empty_values_and_verified_provenance() {
    // Every provenance/principal combination stays attached to the same revision
    // and values; the current reader never becomes an invented author.
    let layout = [
        size_of::<ffi::Metadata>(),
        align_of::<ffi::Metadata>(),
        offset_of!(ffi::Metadata, revision),
        offset_of!(ffi::Metadata, principal_id),
        offset_of!(ffi::Metadata, state),
        offset_of!(ffi::Metadata, provenance),
        offset_of!(ffi::Metadata, principal_kind),
        size_of::<ffi::Field>(),
        align_of::<ffi::Field>(),
        offset_of!(ffi::Field, values_data),
        offset_of!(ffi::Field, bytes_data),
    ];
    for (i, n) in layout.into_iter().enumerate() {
        assert_eq!(unsafe { probe_authenticated_layout(i as u32) }, n as u64);
    }
    for provenance in 0..3 {
        for principal_kind in if provenance == 0 { 0..1 } else { 1..3 } {
            for state in 0..4 {
                let mut field = record();
                field.values = match state {
                    0 | 1 => None,
                    2 => Some(vec![]),
                    _ => field.values,
                };
                let principal = if principal_kind == 1 {
                    Principal::Human([5; 16])
                } else {
                    Principal::Service([5; 16])
                };
                field.provenance = match provenance {
                    0 => FieldProvenance::Unproven,
                    1 => FieldProvenance::ApplicationEdit { principal, device_id: [6; 16] },
                    _ => FieldProvenance::Rekey { principal, device_id: [6; 16] },
                };
                let metadata = if state == 0 {
                    ffi::Metadata::default()
                } else {
                    ffi::Metadata {
                        volume_id: [54; 16],
                        entry_id: [7; 15],
                        object_id: [2; 16],
                        type_id: [3; 16],
                        field_id: [4; 16],
                        revision: [7; 32],
                        state: state.min(2),
                        provenance,
                        principal_kind,
                        principal_id: if provenance == 0 { [0; 16] } else { [5; 16] },
                        device_id: if provenance == 0 { [0; 16] } else { [6; 16] },
                        reserved: 0,
                        entry_reserved: 0,
                        ..Default::default()
                    }
                };
                let records = Arc::new(Mutex::new(Records {
                    calls: 0,
                    denied: false,
                    field: if state == 0 { None } else { Some(field) },
                }));
                let mut probe = Probe { records: records.clone(), metadata };
                let mut app = instance(&mut probe, normal_turn);
                app.pump().unwrap();
                assert_eq!(records.lock().unwrap().calls, 3); // C snapshot, invalid-output fixture snapshot, Rust snapshot.
            }
        }
    }
}
unsafe extern "C" fn retry_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let vol = volume(&mut context);
    let retained = context.begin_read_authenticated_field(vol, [2; 16], [4; 16]).unwrap();
    assert!(matches!(
        context.take_authenticated_field(retained, &mut [], &mut []),
        Err(AuthenticatedReadError::BufferTooSmall { values: 2, bytes: 5 })
    ));
    {
        let mut records = probe.records.lock().unwrap();
        let field = records.field.as_mut().unwrap();
        field.revision = FieldRevision([8; 32]);
        field.values = Some(vec![Value::Integer(99)]);
        field.provenance =
            FieldProvenance::Rekey { principal: Principal::Service([9; 16]), device_id: [10; 16] };
    }
    let mut values = [data::Value::default(); 2];
    let mut bytes = [0; 32];
    let Completion::Ready(Some(held)) =
        context.take_authenticated_field(retained, &mut values, &mut bytes).unwrap()
    else {
        panic!("retained snapshot")
    };
    assert_eq!(held.revision, [7; 32]);
    assert_eq!(
        held.field.values().unwrap().collect::<Vec<_>>(),
        [ValueRef::Text("hello"), ValueRef::Integer(42)]
    );
    let read =
        snapshot(&mut context, vol, [2; 16], [4; 16], &mut values, &mut bytes).unwrap().unwrap();
    assert_eq!(read.revision, [8; 32]);
    assert_eq!(read.field.values().unwrap().collect::<Vec<_>>(), [ValueRef::Integer(99)]);
    assert_eq!(
        read.provenance,
        Provenance::Rekey { principal: ApiPrincipal::Service([9; 16]), device_id: [10; 16] }
    );
    probe.records.lock().unwrap().denied = true;
    let mut previous_metadata = probe.metadata;
    previous_metadata.written = 0;
    let mut output = ffi::Field {
        metadata: &mut probe.metadata,
        values_data: values.as_mut_ptr(),
        values_capacity: 2,
        values_count: 77,
        values_stride: size_of::<data::Value>() as u32,
        bytes_data: bytes.as_mut_ptr(),
        bytes_capacity: 32,
        bytes_count: 88,
        size: size_of::<ffi::Field>() as u32,
        written: 0,
    };
    let mut previous_values = values;
    for value in &mut previous_values {
        value.written = 0;
    }
    let previous_bytes = bytes;
    assert_eq!(unsafe { take_raw(raw, vol, &mut output) }, Error::AccessDenied as i64);
    assert_eq!(probe.metadata, previous_metadata);
    assert_eq!(output.written, 0);
    assert_eq!((output.values_count, output.bytes_count), (77, 88));
    assert_eq!(values, previous_values);
    assert_eq!(bytes, previous_bytes);
    0
}
#[test]
fn short_take_retains_atomic_snapshot_and_new_begin_rechecks_authority() {
    // Short Take retains one snapshot; a distinct Begin obtains new values and provenance.
    let records = Arc::new(Mutex::new(Records { calls: 0, denied: false, field: Some(record()) }));
    let mut probe = Probe {
        records: records.clone(),
        metadata: ffi::Metadata { revision: [55; 32], ..ffi::Metadata::default() },
    };
    let mut app = instance(&mut probe, retry_turn);
    app.pump().unwrap();
    assert_eq!(records.lock().unwrap().calls, 3);
}
unsafe extern "C" fn invalid_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut ctx, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let vol = volume(&mut ctx);
    let mut values = [data::Value { kind: 55, ..data::Value::default() }; 2];
    let mut bytes = [55; 32];
    let mut previous_metadata = probe.metadata;
    previous_metadata.written = 0;
    let mut output = ffi::Field {
        metadata: &mut probe.metadata,
        values_data: values.as_mut_ptr(),
        values_capacity: 2,
        values_count: 77,
        values_stride: size_of::<data::Value>() as u32,
        bytes_data: bytes.as_mut_ptr(),
        bytes_capacity: 32,
        bytes_count: 88,
        size: size_of::<ffi::Field>() as u32,
        written: 0,
    };
    let expected = if probe.metadata.state == 99 { Error::LimitExceeded } else { Error::Internal };
    assert_eq!(unsafe { take_raw(raw, vol, &mut output) }, expected as i64);
    assert_eq!(probe.metadata, previous_metadata);
    assert_eq!(output.written, 0);
    assert_eq!((output.values_count, output.bytes_count), (77, 88));
    assert_eq!(values[0].kind, 55);
    assert_eq!(bytes, [55; 32]);
    0
}
#[test]
fn inconsistent_or_oversized_host_snapshots_preserve_all_outputs() {
    // Host identity, provenance and value validation must precede even short-output metadata.
    for case in 0..9 {
        let mut field = record();
        match case {
            0 => field.volume_id = [9; 16],
            1 => field.object_id = [9; 16],
            2 => field.field_id = [9; 16],
            3 => field.type_id = [0; 16],
            4 => {
                field.provenance = FieldProvenance::ApplicationEdit {
                    principal: Principal::Human([0; 16]),
                    device_id: [6; 16],
                }
            }
            5 => {
                field.provenance = FieldProvenance::Rekey {
                    principal: Principal::Service([5; 16]),
                    device_id: [0; 16],
                }
            }
            6 => field.values = Some(vec![Value::Float(f64::NAN)]),
            7 => field.values = Some(vec![Value::Integer(0); 4097]),
            8 => field.values = Some(vec![Value::Bytes(vec![0; 1024 * 1024 + 1])]),
            _ => unreachable!(),
        }
        let records = Arc::new(Mutex::new(Records { calls: 0, denied: false, field: Some(field) }));
        let mut probe = Probe {
            records: records.clone(),
            metadata: ffi::Metadata {
                state: if case >= 7 { 99 } else { 55 },
                ..ffi::Metadata::default()
            },
        };
        let mut app = instance(&mut probe, invalid_turn);
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().calls, 1);
    }
}
unsafe extern "C" fn limits_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut ctx, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let vol = volume(&mut ctx);
    let byte_len =
        probe.records.lock().unwrap().field.as_ref().unwrap().values.as_ref().unwrap().len();
    let mut values = vec![data::Value::default(); byte_len];
    let mut bytes = vec![0; 1024 * 1024];
    let field =
        snapshot(&mut ctx, vol, [2; 16], [4; 16], &mut values, &mut bytes).unwrap().unwrap();
    assert_eq!(field.field.values().unwrap().len(), byte_len);
    if byte_len == 1 {
        assert_eq!(
            field.field.values().unwrap().next().unwrap(),
            ValueRef::Bytes(&vec![42; 1024 * 1024])
        );
    }
    let mut too_many = vec![data::Value::default(); 4097];
    assert!(matches!(
        snapshot(&mut ctx, vol, [2; 16], [4; 16], &mut too_many, &mut bytes),
        Err(AuthenticatedReadError::Host(Error::LimitExceeded))
    ));
    bytes.push(0);
    assert!(matches!(
        snapshot(&mut ctx, vol, [2; 16], [4; 16], &mut values, &mut bytes),
        Err(AuthenticatedReadError::Host(Error::LimitExceeded))
    ));
    0
}
#[test]
fn maximum_value_count_and_payload_fit_bounded_caller_storage() {
    // Maximum payload and value count are distinct limits; over-capacity Take
    // rejects its output before inspecting or consuming the retained snapshot.
    for values in [vec![Value::Integer(42); 4096], vec![Value::Bytes(vec![42; 1024 * 1024])]] {
        let mut field = record();
        field.values = Some(values);
        let records = Arc::new(Mutex::new(Records { calls: 0, denied: false, field: Some(field) }));
        let mut probe = Probe { records: records.clone(), metadata: ffi::Metadata::default() };
        let mut app = instance(&mut probe, limits_turn);
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().calls, 3);
    }
}
unsafe extern "C" fn unavailable_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut ctx, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let vol = volume(&mut ctx);
    let expected = if probe.metadata.state == 1 { Error::Unsupported } else { Error::AccessDenied };
    assert!(
        matches!(snapshot(&mut ctx, vol,[2;16],[4;16],&mut [],&mut []),Err(AuthenticatedReadError::Host(e)) if e==expected)
    );
    0
}
#[test]
fn unsupported_and_unreadable_mounts_never_reach_an_authenticated_host() {
    // Core capability/mount admission remains shared with Wasm before every read.
    for unsupported in [true, false] {
        let records =
            Arc::new(Mutex::new(Records { calls: 0, denied: false, field: Some(record()) }));
        let mut probe = Probe {
            records: records.clone(),
            metadata: ffi::Metadata { state: u32::from(unsupported), ..ffi::Metadata::default() },
        };
        let mut context = context(50);
        context.volumes[0].rights = VolumeRights(0);
        let host: Box<dyn Host> = if unsupported {
            Box::new(LocalHost)
        } else {
            Box::new(FieldHost::new(records.clone()))
        };
        let mut app = unsafe {
            NativeInstance::new(
                context,
                host,
                abi::App {
                    init: Some(init),
                    turn: Some(unavailable_turn),
                    destroy: Some(destroy),
                    user_data: (&mut probe as *mut Probe).cast(),
                    size: size_of::<abi::App>() as u32,
                    written: 0,
                },
            )
            .unwrap()
        };
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().calls, 0);
    }
}
unsafe extern "C" fn timing_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let vol = volume(&mut context);
    let mut values = [data::Value::default(); 1];
    let mut bytes = [0; 4096];
    let field =
        snapshot(&mut context, vol, [2; 16], [4; 16], &mut values, &mut bytes).unwrap().unwrap();
    assert_eq!(field.revision, [7; 32]);
    assert_eq!(field.field.values().unwrap().next().unwrap(), ValueRef::Bytes(&[42; 4096]));
    0
}
#[test]
#[ignore = "operator CPU diagnostic; controlled host excludes cryptographic/storage work"]
fn authenticated_read_cpu_timing() {
    // Whole native turns include host snapshot cloning, metadata/value copying
    // and safe SDK validation. They do not authenticate ciphertext in this fixture.
    let mut field = record();
    field.values = Some(vec![Value::Bytes(vec![42; 4096])]);
    let records = Arc::new(Mutex::new(Records { calls: 0, denied: false, field: Some(field) }));
    let mut probe = Probe { records: records.clone(), metadata: ffi::Metadata::default() };
    let mut app = instance(&mut probe, timing_turn);
    let mut times = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = std::time::Instant::now();
        app.pump().unwrap();
        times.push(start.elapsed());
    }
    assert_eq!(records.lock().unwrap().calls, 1000);
    times.sort_unstable();
    eprintln!(
        "4-KiB authenticated native read/SDK validation, 1000 turns: p50={:?} p95={:?} p99={:?} max={:?}",
        times[499], times[949], times[989], times[999]
    );
}

struct DelayedHost {
    snapshots: FieldHost,
    ready: Arc<std::sync::atomic::AtomicBool>,
}
impl Host for DelayedHost {
    fn supported_operations(&self) -> &[Opcode] {
        self.snapshots.supported_operations()
    }
    fn begin_data(
        &mut self,
        generation: u64,
        call: Arc<playbit::runtime::host::DataCall>,
    ) -> HostResult<u64> {
        self.snapshots.begin_data(generation, call)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<playbit::runtime::host::DataResult>> {
        if !self.ready.load(std::sync::atomic::Ordering::Acquire) {
            return Err(HostError::WouldBlock);
        }
        self.snapshots.peek_data(token)
    }
    fn release_data(&mut self, token: u64) {
        self.snapshots.release_data(token);
    }
}
struct DelayedProbe {
    stage: u32,
    operation: OperationHandle,
    revoked: bool,
}
unsafe extern "C" fn delayed_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<DelayedProbe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let mut events = context.raw_events();
    if probe.stage == 0 {
        assert_eq!(events.next().unwrap().kind, playbit_api::abi::syscall::EventKind::Started);
        let volume = volume(&mut context);
        probe.operation = context.begin_read_authenticated_field(volume, [2; 16], [4; 16]).unwrap();
        assert!(matches!(
            context.take_authenticated_field(probe.operation, &mut [], &mut []),
            Ok(Completion::Pending)
        ));
    } else {
        let event = events.next().unwrap();
        assert_eq!(event.kind, playbit_api::abi::syscall::EventKind::DataReady);
        assert_eq!(event.operation().unwrap(), probe.operation);
        let mut values = [data::Value::default(); 2];
        let mut bytes = [0; 32];
        let result = context.take_authenticated_field(probe.operation, &mut values, &mut bytes);
        if probe.revoked {
            assert!(matches!(result, Err(AuthenticatedReadError::Host(Error::AccessDenied))));
            assert_eq!(values, [data::Value::default(); 2]);
            assert_eq!(bytes, [0; 32]);
        } else {
            let Completion::Ready(Some(field)) = result.unwrap() else { panic!("ready field") };
            assert_eq!(field.revision, [7; 32]);
            assert_eq!(
                field.field.values().unwrap().collect::<Vec<_>>(),
                [ValueRef::Text("hello"), ValueRef::Integer(42)]
            );
        }
        assert!(matches!(
            context.take_authenticated_field(probe.operation, &mut [], &mut []),
            Err(AuthenticatedReadError::Host(Error::StaleHandle))
        ));
        assert!(events.next().is_none());
    }
    probe.stage += 1;
    0
}
#[test]
fn delayed_native_snapshot_yields_notifies_once_and_rechecks_current_read_grant() {
    for revoked in [false, true] {
        let records =
            Arc::new(Mutex::new(Records { calls: 0, denied: false, field: Some(record()) }));
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut probe = DelayedProbe { stage: 0, operation: OperationHandle::default(), revoked };
        let mut app = unsafe {
            NativeInstance::new(
                context(50),
                Box::new(DelayedHost {
                    snapshots: FieldHost::new(records.clone()),
                    ready: ready.clone(),
                }),
                abi::App {
                    init: Some(init),
                    turn: Some(delayed_turn),
                    destroy: Some(destroy),
                    user_data: (&mut probe as *mut DelayedProbe).cast(),
                    size: size_of::<abi::App>() as u32,
                    written: 0,
                },
            )
            .unwrap()
        };
        app.pump().unwrap();
        assert_eq!(probe.stage, 1);
        records.lock().unwrap().field.as_mut().unwrap().revision = FieldRevision([99; 32]);
        if revoked {
            let mut volumes = context(50).volumes;
            volumes[0].rights = VolumeRights(0);
            app.replace_volumes(volumes).unwrap();
        }
        ready.store(true, std::sync::atomic::Ordering::Release);
        for _ in 0..2 {
            app.notify_data(app.generation(), 1).unwrap();
        }
        app.pump().unwrap();
        assert_eq!(probe.stage, 2);
        assert_eq!(records.lock().unwrap().calls, 1);
    }
}

unsafe fn probe_handle<T>(result: i64, output: *mut T) -> i64 {
    if result < 0 {
        return result;
    }
    if result == 0 {
        return -4;
    }
    // Probe callers supply an aligned eight-byte scalar handle destination.
    unsafe {
        output.cast::<u64>().write(result as u64);
    }
    0
}
