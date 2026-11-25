//! Completion-driven native operation conformance with a controlled local host.
use super::*;
use playbit::runtime::syscall::{EntryAddress, FileCopyRequest, FileSelection, HostResult};
use playbit_api::{
    abi::{data as data_abi, pending as ffi, syscall::Error},
    context::{Completion, Context, FileReference, OperationHandle},
};
use std::sync::{Arc, Mutex};
unsafe extern "C" {
    fn probe_pending_begin(
        context: *mut abi::Context,
        volume: data_abi::EntryHandle,
        output: *mut OperationHandle,
    );
    fn probe_pending_wait(context: *mut abi::Context, handles: *const OperationHandle);
    fn probe_pending_finish(
        context: *mut abi::Context,
        handles: *const OperationHandle,
        canceled: u32,
        volume: data_abi::VolumeHandle,
    );
    fn probe_pending_layout(index: u32) -> u64;
}
struct Records {
    tokens: Vec<(usize, u64)>,
    initial_fields: Vec<Vec<playbit::runtime::host::FieldChange>>,
    polls: [u32; 3],
    begin_error: Option<HostError>,
    created: HostResult<[u8; 16]>,
    copied: HostResult<[u8; 15]>,
    selected: HostResult<Option<FileSelection>>,
}
impl Default for Records {
    fn default() -> Self {
        Self {
            tokens: vec![],
            initial_fields: vec![],
            polls: [0; 3],
            begin_error: None,
            created: Err(HostError::WouldBlock),
            copied: Err(HostError::WouldBlock),
            selected: Err(HostError::WouldBlock),
        }
    }
}
struct PendingHost(Arc<Mutex<Records>>);
impl PendingHost {
    fn begin(&self, kind: usize) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        if let Some(error) = records.begin_error {
            return Err(error);
        }
        let token = records.tokens.len() as u64 + 100;
        records.tokens.push((kind, token));
        Ok(token)
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
        call: std::sync::Arc<playbit::runtime::host::DataCall>,
    ) -> HostResult<u64> {
        let playbit::runtime::host::DataOperation::Create { type_id, fields, .. } = &call.operation
        else {
            return Err(HostError::Unsupported);
        };
        let volume = call.volume;
        let ty = *type_id;
        let changes = fields.as_slice();

        assert_eq!((volume, ty), ([54; 16], [1; 16]));
        let token = self.begin(0)?;
        self.0.lock().unwrap().initial_fields.push(changes.to_vec());
        Ok(token)
    }
    fn begin_file_copy(&mut self, _: u64, request: FileCopyRequest) -> HostResult<u64> {
        assert_eq!(request.source, EntryAddress { volume_id: [54; 16], entry_id: [1; 15] });
        assert_eq!(request.destination, EntryAddress { volume_id: [54; 16], entry_id: [2; 15] });
        self.begin(1)
    }
    fn begin_file_selection(&mut self, _: u64) -> HostResult<u64> {
        self.begin(2)
    }
    fn peek_data(
        &mut self,
        token: u64,
    ) -> HostResult<std::sync::Arc<playbit::runtime::host::DataResult>> {
        let result: HostResult<[u8; 16]> = {
            let mut r = self.0.lock().unwrap();
            assert!(r.tokens.contains(&(0, token)));
            r.polls[0] += 1;
            r.created
        };
        result.map(|id| std::sync::Arc::new(playbit::runtime::host::DataResult::Created(id)))
    }
    fn poll_file_copy(&mut self, token: u64) -> HostResult<[u8; 15]> {
        let mut r = self.0.lock().unwrap();
        assert!(r.tokens.contains(&(1, token)));
        r.polls[1] += 1;
        r.copied
    }
    fn poll_file_selection(&mut self, token: u64) -> HostResult<Option<FileSelection>> {
        let mut r = self.0.lock().unwrap();
        assert!(r.tokens.contains(&(2, token)));
        r.polls[2] += 1;
        r.selected
    }
}
#[derive(Default)]
struct Probe {
    stage: u32,
    canceled: bool,
    volume: data_abi::VolumeHandle,
    c: [OperationHandle; 3],
    rust: [OperationHandle; 3],
    expected: Option<[Error; 3]>,
    foreign: [OperationHandle; 3],
}
fn volume(context: &mut Context<'_>) -> data_abi::EntryHandle {
    context.attach_files(&playbit_api::context::FileResources::default()).unwrap();
    let mut records = [data_abi::Volume::default(); 1];
    let mut bytes = [0; 16];
    let volume = context
        .available_volumes(&mut records, &mut [], &mut bytes)
        .unwrap()
        .iter()
        .next()
        .unwrap()
        .handle;
    context.flush_files().unwrap();
    data_abi::EntryHandle { volume: volume.0, entry_id: [7; 15], reserved: 0, ..Default::default() }
}
fn begin(context: &mut Context<'_>, volume: data_abi::EntryHandle) -> [OperationHandle; 3] {
    let source = FileReference {
        volume: volume.volume,
        entry_id: [1; 15],
        reserved: 0,
        ..Default::default()
    };
    let target = FileReference { entry_id: [2; 15], ..source };
    let values = [playbit_api::context::Value::boolean(true)];
    let changes = [playbit_api::context::FieldChange::set([2; 16], &values).unwrap()];
    [
        context.begin_create_object(volume, [1; 16], &changes).unwrap(),
        context.begin_file_copy(source, target).unwrap(),
        context.begin_file_selection().unwrap(),
    ]
}
unsafe extern "C" fn init(_: *mut abi::Context, user: *mut c_void, state: *mut *mut c_void) -> i32 {
    unsafe {
        state.write(user);
    }
    0
}
unsafe extern "C" fn destroy(_: *mut c_void) {}
unsafe extern "C" fn turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    match probe.stage {
        0 => {
            let mut context_state = playbit_api::context::ContextState::default();
            let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
            let volume = volume(&mut context);
            probe.volume = volume.volume_handle();
            unsafe {
                probe_pending_begin(raw, volume, probe.c.as_mut_ptr());
            }
            probe.rust = begin(&mut context, volume);
            let mut events = context.raw_events();
            assert_eq!(events.next().unwrap().kind, playbit_api::abi::syscall::EventKind::Started);
            assert!(events.next().is_none());
        }
        1 => {
            unsafe {
                probe_pending_wait(raw, probe.c.as_ptr());
            }
            let mut context_state = playbit_api::context::ContextState::default();
            let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
            assert_eq!(context.poll_create_object(probe.rust[0]), Ok(Completion::Pending));
            assert_eq!(context.poll_file_copy(probe.rust[1]), Ok(Completion::Pending));
            assert_eq!(context.poll_file_selection(probe.rust[2]), Ok(Completion::Pending));
        }
        2 => {
            unsafe {
                probe_pending_finish(
                    raw,
                    probe.c.as_ptr(),
                    u32::from(probe.canceled),
                    probe.volume,
                );
            }
            let mut context_state = playbit_api::context::ContextState::default();
            let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
            let mut events = context.raw_events();
            let mut seen = [false; 3];
            for _ in 0..3 {
                let event = events.next().unwrap();
                let handle = event.operation().unwrap();
                let index = probe.rust.iter().position(|h| *h == handle).unwrap();
                seen[index] = true;
                match index {
                    0 => assert_eq!(
                        context.poll_create_object(handle),
                        Ok(Completion::Ready([3; 16]))
                    ),
                    1 => assert_eq!(context.poll_file_copy(handle), Ok(Completion::Ready([2; 15]))),
                    2 => {
                        let ready = context.poll_file_selection(handle).unwrap();
                        if probe.canceled {
                            assert_eq!(ready, Completion::Ready(None));
                        } else {
                            let Completion::Ready(Some(file)) = ready else {
                                panic!("selected file")
                            };
                            assert_eq!(file.entry_id, [3; 15]);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            assert_eq!(seen, [true; 3]);
            assert!(events.next().is_none());
        }
        _ => panic!("unexpected probe stage"),
    }
    probe.stage += 1;
    0
}
fn instance(probe: &mut Probe, records: Arc<Mutex<Records>>, turn: abi::Turn) -> NativeInstance {
    unsafe {
        NativeInstance::new(
            context(50),
            Box::new(PendingHost(records)),
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
fn notify(instance: &mut NativeInstance, records: &Arc<Mutex<Records>>) {
    let generation = instance.generation();
    for (kind, token) in &records.lock().unwrap().tokens {
        for _ in 0..2 {
            // Duplicate completion delivery must coalesce.
            match kind {
                0 => instance.notify_creation(generation, *token).unwrap(),
                1 => instance.notify_file_copy(generation, *token).unwrap(),
                2 => instance.notify_file_selection(generation, *token).unwrap(),
                _ => unreachable!(),
            }
        }
    }
}
#[test]
fn c_and_rust_complete_notified_work_once_including_canceled_selection() {
    // C polls consume their queued notices; Rust decodes and consumes the remaining
    // notices. Invalid outputs/kinds never reach the host or consume work.
    let layout = [
        size_of::<OperationHandle>(),
        align_of::<OperationHandle>(),
        size_of::<u32>(),
        size_of::<ffi::Selection>(),
        align_of::<ffi::Selection>(),
        offset_of!(ffi::Selection, volume),
    ];
    for (index, expected) in layout.into_iter().enumerate() {
        assert_eq!(unsafe { probe_pending_layout(index as u32) }, expected as u64);
    }
    for canceled in [false, true] {
        let mut probe = Probe { canceled, ..Probe::default() };
        let records = Arc::new(Mutex::new(Records::default()));
        let mut app = instance(&mut probe, records.clone(), turn);
        app.pump().unwrap();
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().tokens.len(), 6);
        assert_eq!(
            records.lock().unwrap().initial_fields,
            vec![
                vec![playbit::runtime::host::FieldChange {
                    field_id: [2; 16],
                    values: Some(vec![playbit::runtime::host::Value::Boolean(true)]),
                }];
                2
            ]
        );
        assert_eq!(records.lock().unwrap().polls, [2; 3]);
        {
            let mut r = records.lock().unwrap();
            r.created = Ok([3; 16]);
            r.copied = Ok([2; 15]);
            r.selected = Ok((!canceled).then_some(FileSelection {
                address: EntryAddress { volume_id: [54; 16], entry_id: [3; 15] },
            }));
        }
        notify(&mut app, &records);
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().polls, [4; 3]);
        assert_eq!(app.notify_creation(app.generation(), 100), Err(HostError::StaleHandle));
    }
}
unsafe extern "C" {
    fn probe_pending_errors(
        context: *mut abi::Context,
        handles: *const OperationHandle,
        expected: *const i32,
    );
}
unsafe extern "C" fn error_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    if probe.stage == 0 {
        return unsafe { turn(raw, state) };
    }
    let expected = probe.expected.unwrap();
    let codes = expected.map(|e| e as i32);
    unsafe {
        probe_pending_errors(raw, probe.c.as_ptr(), codes.as_ptr());
    }
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    assert_eq!(context.poll_create_object(probe.rust[0]), Err(expected[0]));
    assert_eq!(context.poll_file_copy(probe.rust[1]), Err(expected[1]));
    assert_eq!(context.poll_file_selection(probe.rust[2]), Err(expected[2]));
    assert_eq!(context.poll_create_object(probe.rust[0]), Err(Error::StaleHandle));
    assert_eq!(context.poll_file_copy(probe.rust[1]), Err(Error::StaleHandle));
    assert_eq!(context.poll_file_selection(probe.rust[2]), Err(Error::StaleHandle));
    assert_eq!(
        context.raw_events().map(|event| event.operation().unwrap()).collect::<Vec<_>>(),
        probe.rust
    );
    0
}
#[test]
fn terminal_host_errors_consume_work_and_preserve_outputs() {
    // All terminal failures retire queued readiness as well as their handles.
    for error in [
        Error::AccessDenied,
        Error::NotFound,
        Error::InvalidArgument,
        Error::Unavailable,
        Error::Internal,
    ] {
        let mut probe = Probe { expected: Some([error; 3]), ..Probe::default() };
        let records = Arc::new(Mutex::new(Records::default()));
        let mut app = instance(&mut probe, records.clone(), error_turn);
        app.pump().unwrap();
        {
            let mut r = records.lock().unwrap();
            r.created = Err(error);
            r.copied = Err(error);
            r.selected = Err(error);
        }
        notify(&mut app, &records);
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().polls, [2; 3]);
    }
}
unsafe extern "C" {
    fn probe_pending_withdrawn(context: *mut abi::Context, handles: *const OperationHandle);
}
unsafe extern "C" fn withdrawn_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    if probe.stage == 0 {
        return unsafe { turn(raw, state) };
    }
    unsafe {
        probe_pending_withdrawn(raw, probe.c.as_ptr());
    }
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    assert_eq!(context.poll_create_object(probe.rust[0]), Ok(Completion::Ready([3; 16])));
    assert_eq!(context.poll_file_copy(probe.rust[1]), Ok(Completion::Ready([2; 15])));
    assert_eq!(context.poll_file_selection(probe.rust[2]), Err(Error::AccessDenied));
    assert_eq!(
        context.raw_events().map(|event| event.operation().unwrap()).collect::<Vec<_>>(),
        probe.rust
    );
    0
}
#[test]
fn withdrawn_mount_keeps_known_receipts_and_selection_never_grants_access() {
    // Completed creation/copy are known writes. Selection still checks the
    // current mount before returning a file and cannot create read authority.
    let mut probe = Probe::default();
    let records = Arc::new(Mutex::new(Records::default()));
    let mut app = instance(&mut probe, records.clone(), withdrawn_turn);
    app.pump().unwrap();
    {
        let mut r = records.lock().unwrap();
        r.created = Ok([3; 16]);
        r.copied = Ok([2; 15]);
        r.selected = Ok(Some(FileSelection {
            address: EntryAddress { volume_id: [54; 16], entry_id: [3; 15] },
        }));
    }
    notify(&mut app, &records);
    let mut volumes = context(50).volumes;
    volumes[0].rights = VolumeRights(0);
    app.replace_volumes(volumes).unwrap();
    app.pump().unwrap();
    assert_eq!(records.lock().unwrap().polls, [2; 3]);
}
#[test]
fn wrong_destination_or_invalid_selection_is_terminal() {
    // Host results must still name the requested destination/entry. A selection
    // outside the current mounts is denied without adopting a new mount.
    for (selected, expected) in [
        (
            FileSelection { address: EntryAddress { volume_id: [54; 16], entry_id: [0; 15] } },
            Error::Internal,
        ),
        (
            FileSelection { address: EntryAddress { volume_id: [99; 16], entry_id: [3; 15] } },
            Error::AccessDenied,
        ),
    ] {
        let mut probe = Probe {
            expected: Some([Error::Unavailable, Error::Internal, expected]),
            ..Probe::default()
        };
        let records = Arc::new(Mutex::new(Records::default()));
        let mut app = instance(&mut probe, records.clone(), error_turn);
        app.pump().unwrap();
        {
            let mut r = records.lock().unwrap();
            r.created = Err(HostError::Unavailable);
            r.copied = Ok([9; 15]);
            r.selected = Ok(Some(selected));
        }
        notify(&mut app, &records);
        app.pump().unwrap();
        assert_eq!(records.lock().unwrap().polls, [2; 3]);
    }
}
unsafe extern "C" fn capacity_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let volume = volume(&mut context);
    let mut handles = [OperationHandle::default(); 64];
    for h in &mut handles {
        *h = context.begin_create_object(volume, [1; 16], &[]).unwrap();
    }
    assert_eq!(context.begin_create_object(volume, [1; 16], &[]), Err(Error::LimitExceeded));
    assert_eq!(context.begin_file_selection(), Err(Error::LimitExceeded));
    let source = FileReference {
        volume: volume.volume,
        entry_id: [1; 15],
        reserved: 0,
        ..Default::default()
    };
    assert_eq!(
        context.begin_file_copy(source, FileReference { entry_id: [2; 15], ..source }),
        Err(Error::LimitExceeded)
    );
    assert_eq!(context.poll_create_object(handles[0]), Ok(Completion::Ready([3; 16])));
    let replacement = context.begin_create_object(volume, [1; 16], &[]).unwrap();
    assert_eq!(replacement.slot(), handles[0].slot());
    assert_ne!(replacement.generation(), handles[0].generation());
    assert_eq!(context.poll_create_object(handles[0]), Err(Error::StaleHandle));
    0
}
#[test]
fn pending_bound_is_shared_and_reused_slots_reject_stale_handles() {
    // The 65th admission fails before reaching any host; consumption makes one
    // slot available but its old handle can never alias the replacement.
    let mut probe = Probe::default();
    let records = Arc::new(Mutex::new(Records { created: Ok([3; 16]), ..Records::default() }));
    let mut app = instance(&mut probe, records.clone(), capacity_turn);
    app.pump().unwrap();
    assert_eq!(records.lock().unwrap().tokens.len(), 65);
    assert_eq!(records.lock().unwrap().polls, [1, 0, 0]);
}
unsafe extern "C" fn begin_error_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let mut context_state = playbit_api::context::ContextState::default();
    let volume = volume(&mut unsafe { Context::from_raw(raw, &mut context_state) }.unwrap().0);
    let ty = [1; 16];
    let file = FileReference {
        volume: volume.volume,
        entry_id: [1; 15],
        reserved: 0,
        ..Default::default()
    };
    let destination = FileReference { entry_id: [2; 15], ..file };
    let mut output = OperationHandle::new(999, 777);
    for _ in 0..65 {
        // Failed begins must neither allocate a slot nor modify the fixed output.
        assert_eq!(
            unsafe {
                probe_handle(
                    ffi::pb_object_create_begin(
                        raw,
                        &volume,
                        &ty,
                        core::ptr::null(),
                        0,
                        size_of::<data_abi::FieldInput>() as u32,
                    ),
                    &mut output,
                )
            },
            Error::Unavailable as i64
        );
        assert_eq!(
            unsafe { probe_handle(ffi::pb_file_copy_begin(raw, &file, &destination), &mut output) },
            Error::Unavailable as i64
        );
        assert_eq!(
            unsafe { probe_handle(ffi::pb_file_select_begin(raw), &mut output) },
            Error::Unavailable as i64
        );
        assert_eq!(output, OperationHandle::new(999, 777));
    }
    0
}
#[test]
fn failed_begin_never_reserves_pending_capacity_or_changes_output() {
    // Repeating more failures than the pending bound must keep reporting the host failure.
    let mut probe = Probe::default();
    let records = Arc::new(Mutex::new(Records {
        begin_error: Some(HostError::Unavailable),
        ..Records::default()
    }));
    let mut app = instance(&mut probe, records.clone(), begin_error_turn);
    app.pump().unwrap();
    assert!(records.lock().unwrap().tokens.is_empty());
}
#[test]
fn ready_event_decoding_rejects_wrong_kind_and_payload() {
    // Typed event decoding is a bounded data-format check, independent of transport.
    use playbit_api::{abi::syscall::EventKind, context::Event};
    let bytes = [2, 0, 0, 0, 1, 0, 0, 0];
    let event = Event { generation: 1, kind: EventKind::FileCopyReady, target: 0, data: &bytes };
    assert_eq!(event.operation(), Ok(OperationHandle::new(1, 2)));
    for invalid in [
        Event { object_id: 0, kind: EventKind::Click, ..event },
        Event { target: 1, ..event },
        Event { data: &bytes[..7], ..event },
        Event { data: &[0; 8], ..event },
        Event { data: &[1; 9], ..event },
    ] {
        assert!(invalid.operation().is_err());
    }
}
#[test]
#[ignore = "operator CPU diagnostic for completion-driven native operations"]
fn pending_completion_cpu_timing() {
    // Include construction, C/Rust begin+pending polls, readiness and terminal
    // delivery. Controlled host work excludes actual copy/selection/creation I/O.
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let mut probe = Probe::default();
        let records = Arc::new(Mutex::new(Records::default()));
        let start = std::time::Instant::now();
        let mut app = instance(&mut probe, records.clone(), turn);
        app.pump().unwrap();
        app.pump().unwrap();
        {
            let mut r = records.lock().unwrap();
            r.created = Ok([3; 16]);
            r.copied = Ok([2; 15]);
            r.selected = Ok(Some(FileSelection {
                address: EntryAddress { volume_id: [54; 16], entry_id: [3; 15] },
            }));
        }
        notify(&mut app, &records);
        app.pump().unwrap();
        drop(app);
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "native C+Rust six-operation completion lifecycle, 1000 samples: p50={:?} p95={:?} p99={:?} max={:?}",
        samples[499], samples[949], samples[989], samples[999]
    );
}
unsafe extern "C" fn foreign_turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    let probe = unsafe { &mut *state.cast::<Probe>() };
    if probe.stage == 0 {
        return unsafe { turn(raw, state) };
    }
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    assert_eq!(context.poll_create_object(probe.foreign[0]), Ok(Completion::Pending));
    assert_eq!(context.poll_file_copy(probe.foreign[1]), Ok(Completion::Pending));
    assert_eq!(context.poll_file_selection(probe.foreign[2]), Ok(Completion::Pending));
    assert_eq!(context.poll_create_object(probe.rust[0]), Ok(Completion::Pending));
    assert_eq!(context.poll_file_copy(probe.rust[1]), Ok(Completion::Pending));
    assert_eq!(context.poll_file_selection(probe.rust[2]), Ok(Completion::Pending));
    0
}
#[test]
fn numeric_handles_are_local_and_notifications_require_the_instance_epoch() {
    // The same numeric handle resolves only in the calling guest table.
    // Notifications separately reject another instance's epoch.
    let mut a = Probe::default();
    let mut b = Probe::default();
    let ar = Arc::new(Mutex::new(Records::default()));
    let br = Arc::new(Mutex::new(Records::default()));
    let mut first = instance(&mut a, ar.clone(), foreign_turn);
    let mut second = instance(&mut b, br.clone(), foreign_turn);
    first.pump().unwrap();
    second.pump().unwrap();
    assert_eq!(a.rust.map(|h| h.slot()), b.rust.map(|h| h.slot()));
    assert_eq!(a.rust, b.rust);
    b.foreign = a.rust;
    second.pump().unwrap();
    assert_eq!(b.foreign, a.rust);
    assert_eq!(ar.lock().unwrap().polls, [0; 3]);
    assert_eq!(br.lock().unwrap().polls, [2; 3]);
    assert_eq!(second.notify_creation(first.generation(), 100), Err(HostError::StaleHandle));
}
unsafe extern "C" fn unsupported_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let volume = volume(&mut context);
    let source = FileReference {
        volume: volume.volume,
        entry_id: [1; 15],
        reserved: 0,
        ..Default::default()
    };
    assert_eq!(context.begin_create_object(volume, [1; 16], &[]), Err(Error::Unsupported));
    assert_eq!(
        context.begin_file_copy(source, FileReference { entry_id: [2; 15], ..source }),
        Err(Error::Unsupported)
    );
    assert_eq!(context.begin_file_selection(), Err(Error::Unsupported));
    let pending = OperationHandle::new(volume.volume_handle().generation(), 1);
    assert_eq!(context.poll_create_object(pending), Err(Error::Unsupported));
    assert_eq!(context.poll_file_copy(pending), Err(Error::Unsupported));
    assert_eq!(context.poll_file_selection(pending), Err(Error::Unsupported));
    0
}
#[test]
fn absent_pending_capabilities_are_explicit() {
    // A local-only host must not acquire creation/copy/selection UI implicitly.
    let mut app = unsafe {
        NativeInstance::new(
            context(50),
            Box::new(LocalHost),
            abi::App {
                init: Some(init),
                turn: Some(unsupported_turn),
                destroy: Some(destroy),
                user_data: std::ptr::null_mut(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
        .unwrap()
    };
    app.pump().unwrap();
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
