//! Direct operation calls exercise the same checks without constructing a VM.
#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::runtime::host::{DataCall, DataOperation, DataResult};
use std::sync::{Arc, Mutex};

const ROOT: Id = [1; 16];
const OBJECT: Id = [2; 16];
const TYPE: Id = [3; 16];
const FIELD: Id = [4; 16];

#[derive(Default)]
struct Calls {
    mutations: usize,
    polls: usize,
    bad_record: bool,
    admitted: Vec<Arc<DataCall>>,
}
struct RecordingHost(Arc<Mutex<Calls>>);
impl Host for RecordingHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::SchemaEstablishBegin,
            Opcode::SchemaEstablishTake,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::FileSelectBegin,
            Opcode::FileSelectPoll,
        ]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.lock().unwrap();
        if call.operation.is_mutation() {
            records.mutations += 1;
        }
        records.admitted.push(call);
        Ok(records.admitted.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        let mut records = self.0.lock().unwrap();
        records.polls += 1;
        // Controlled completion records; no storage backend executes at Begin.
        let result = match &records.admitted[token as usize - 1].operation {
            DataOperation::Create { .. } => DataResult::Created(OBJECT),
            DataOperation::Edit { .. } => DataResult::Edited,
            DataOperation::SchemaEstablish { .. } => DataResult::SchemaEstablished,
            DataOperation::Read { object_id, .. } => DataResult::Object(ObjectRecord {
                id: *object_id,
                type_id: TYPE,
                fields: vec![FieldChange {
                    field_id: FIELD,
                    values: Some(vec![Value::Float(if records.bad_record {
                        f64::NAN
                    } else {
                        1.0
                    })]),
                }],
            }),
            _ => return Err(HostError::Unsupported),
        };
        Ok(Arc::new(result))
    }
    fn begin_file_selection(&mut self, _: u64) -> HostResult<u64> {
        self.0.lock().unwrap().mutations += 1;
        Ok(2)
    }
    fn poll_file_selection(&mut self, _: u64) -> HostResult<Option<FileSelection>> {
        self.0.lock().unwrap().polls += 1;
        Ok(Some(FileSelection { address: EntryAddress { volume_id: ROOT, entry_id: [5; 15] } }))
    }
}

fn fixture() -> (State, VolumeHandle, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let state = State::new(
        GuestContext {
            app_name: "Test app".into(),
            app_build: None,
            identity: Identity {
                account_id: [9; 16],
                is_service: false,
                device_id: [8; 16],
                instance_id: [7; 16],
                session_id: None,
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
        },
        Box::new(RecordingHost(calls.clone())),
    )
    .unwrap();
    let handle = state.volume_handle(state.context.volumes[0].id).unwrap();
    (state, handle, calls)
}
fn _entry(volume: VolumeHandle) -> EntryHandle {
    EntryHandle {
        volume: playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot),
        entry_id: [5; 15],
        ..Default::default()
    }
}
fn field(value: Value) -> FieldChange {
    FieldChange { field_id: FIELD, values: Some(vec![value]) }
}

#[test]
fn typed_data_rejects_malformed_or_unauthorized_mutations_before_host_effects() {
    let (mut state, handle, calls) = fixture();
    for changes in [
        vec![],
        vec![field(Value::Float(f64::NAN))],
        vec![field(Value::Integer(1)), field(Value::Integer(2))],
        vec![field(Value::File { volume_id: ROOT, entry_id: [0; 15] })],
    ] {
        assert_eq!(
            pending_data::begin(
                &mut state,
                handle,
                DataOperation::Edit {
                    entry_id: [5; 15],
                    object_id: OBJECT,
                    fields: changes.to_vec()
                },
                Opcode::ObjectEditBegin
            ),
            Err(HostError::InvalidArgument)
        );
    }
    let large = vec![field(Value::Boolean(true)); abi::MAX_FIELDS as usize + 1];
    assert_eq!(
        pending_data::begin(
            &mut state,
            handle,
            DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields: large },
            Opcode::ObjectCreateBegin
        ),
        Err(HostError::LimitExceeded)
    );
    let changes = [field(Value::Text("retained".into()))];
    let stale = VolumeHandle { generation: handle.generation + 1, ..handle };
    assert_eq!(
        pending_data::begin(
            &mut state,
            stale,
            DataOperation::Edit { entry_id: [5; 15], object_id: OBJECT, fields: changes.to_vec() },
            Opcode::ObjectEditBegin
        ),
        Err(HostError::StaleHandle)
    );
    super::test_support::set_volume_rights(&mut state, 0, VolumeRights::READ);
    assert_eq!(
        pending_data::begin(
            &mut state,
            handle,
            DataOperation::Edit { entry_id: [5; 15], object_id: OBJECT, fields: changes.to_vec() },
            Opcode::ObjectEditBegin
        ),
        Err(HostError::AccessDenied)
    );
    assert_eq!(calls.lock().unwrap().mutations, 0);
    super::test_support::set_volume_rights(&mut state, 0, VolumeRights::ALL);
    pending_data::begin(
        &mut state,
        handle,
        DataOperation::Edit { entry_id: [5; 15], object_id: OBJECT, fields: changes.to_vec() },
        Opcode::ObjectEditBegin,
    )
    .unwrap();
    assert_eq!(calls.lock().unwrap().mutations, 1);
}

#[test]
fn typed_schema_and_host_results_are_checked_without_a_writer() {
    let (mut state, handle, calls) = fixture();
    let mut schema = Schema {
        types: vec![TypeDeclaration {
            id: TYPE,
            name: "Item".into(),
            deprecated: false,
            fields: vec![FieldDeclaration {
                id: TYPE,
                name: "value".into(),
                value_type: ValueType::Text,
                repeated: false,
                deprecated: false,
            }],
        }],
    };
    assert_eq!(
        pending_data::begin(
            &mut state,
            handle,
            crate::runtime::host::DataOperation::SchemaEstablish {
                entry_id: [5; 15],
                schema: schema.clone()
            },
            Opcode::SchemaEstablishBegin
        ),
        Err(HostError::InvalidArgument)
    );
    assert_eq!(calls.lock().unwrap().mutations, 0);
    schema.types[0].fields[0].id = FIELD;
    let pending = pending_data::begin(
        &mut state,
        handle,
        crate::runtime::host::DataOperation::SchemaEstablish { entry_id: [5; 15], schema },
        Opcode::SchemaEstablishBegin,
    )
    .unwrap();
    pending_data::take(&mut state, pending, Opcode::SchemaEstablishTake, |_, result| {
        assert!(matches!(result, crate::runtime::host::DataResult::SchemaEstablished));
        Ok(0)
    })
    .unwrap();
    let read = DataOperation::Read { entry_id: [5; 15], object_id: OBJECT, fields: vec![FIELD] };
    let pending =
        pending_data::begin(&mut state, handle, read.clone(), Opcode::ObjectReadBegin).unwrap();
    pending_data::take(&mut state, pending, Opcode::ObjectReadTake, |_, _| Ok(())).unwrap();
    calls.lock().unwrap().bad_record = true;
    let pending =
        pending_data::begin(&mut state, handle, read.clone(), Opcode::ObjectReadBegin).unwrap();
    assert_eq!(
        pending_data::take(&mut state, pending, Opcode::ObjectReadTake, |_, _| Ok(())),
        Err(HostError::Internal)
    );
    assert!(state.capabilities.operations().next().is_none());
    state.supported.remove(&Opcode::ObjectReadBegin);
    assert_eq!(
        pending_data::begin(&mut state, handle, read, Opcode::ObjectReadBegin),
        Err(HostError::Unsupported)
    );
}

#[test]
fn typed_creation_preserves_its_receipt_after_write_authority_is_withdrawn() {
    // The original receipt remains consumable once; current rights still gate
    // new creations, including after the accepted slot has been retired.
    let (mut state, handle, calls) = fixture();
    let operation = pending_data::begin(
        &mut state,
        handle,
        DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields: vec![] },
        Opcode::ObjectCreateBegin,
    )
    .unwrap();
    let ready = super::handles::raw_operation(operation).raw().to_le_bytes().to_vec();
    push_event(&mut state, Event { kind: EventKind::ObjectCreationReady, target: 0, data: ready })
        .unwrap();
    super::test_support::set_volume_rights(&mut state, 0, VolumeRights::READ);
    assert_eq!(
        handles::poll_operation(&mut state, operation, Opcode::ObjectCreatePoll),
        Ok(OBJECT)
    );
    assert_eq!(state.events.len(), 1);
    assert!(state.capabilities.operations().next().is_none());
    assert_eq!(calls.lock().unwrap().polls, 1);
    assert_eq!(
        pending_data::begin(
            &mut state,
            handle,
            DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields: vec![] },
            Opcode::ObjectCreateBegin
        ),
        Err(HostError::AccessDenied)
    );
    assert_eq!(
        handles::poll_operation(&mut state, operation, Opcode::ObjectCreatePoll),
        Err(HostError::StaleHandle)
    );
    super::test_support::set_volume_rights(&mut state, 0, VolumeRights::ALL);
    let current = pending_data::begin(
        &mut state,
        handle,
        DataOperation::Create { entry_id: [5; 15], type_id: TYPE, fields: vec![] },
        Opcode::ObjectCreateBegin,
    )
    .unwrap();
    assert_ne!(operation, current);
    assert_eq!(handles::poll_operation(&mut state, current, Opcode::ObjectCreatePoll), Ok(OBJECT));
    assert_eq!(state.queued_bytes, 20);
    assert_eq!(calls.lock().unwrap().polls, 2);
    assert_eq!(
        handles::poll_operation(&mut state, current, Opcode::ObjectCreatePoll),
        Err(HostError::StaleHandle)
    );
    state.assert_invariants();
}

#[test]
fn typed_file_selection_consumes_once() {
    let (mut state, handle, calls) = fixture();
    let operation = file_ops::select_begin(&mut state).unwrap();
    assert_eq!(calls.lock().unwrap().polls, 0);
    let file = file_ops::select_poll(&mut state, operation).unwrap().unwrap();
    assert_eq!(file.volume, handle);
    assert_eq!(file_ops::select_poll(&mut state, operation), Err(HostError::StaleHandle));
}

#[test]
fn typed_metadata_and_event_reads_borrow_without_consuming_state() {
    let (mut state, handle, _) = fixture();
    assert!(environment::capabilities(&state).unwrap().iter().any(|name| name == "event.poll"));
    assert_eq!(environment::identity(&state).unwrap().0, u64::from(state.generation));
    assert!(environment::presence(&state).unwrap().is_empty());
    assert!(environment::startup_arguments(&state).unwrap().is_empty());
    let (root, volume) = environment::available_volumes(&state).unwrap().next().unwrap();
    assert_eq!(root, handle);
    assert_eq!(volume.id, ROOT);
    assert_eq!(events::peek_event(&state).unwrap().kind, EventKind::Started);
    assert_eq!(state.events.len(), 1);
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::Started);
    assert_eq!(events::take_event(&mut state), Err(HostError::WouldBlock));
    assert_eq!(state.queued_bytes, 0);
}

#[test]
fn named_presentation_features_participate_in_admission_and_discovery() {
    // A feature name is independent of opcodes; required unknown features fail
    // before instance creation, while discovery includes admitted feature names.
    use playbit_api::ui::{
        CANVAS_CLEAR_FEATURE, CANVAS_PIXELS_FEATURE, CANVAS_VIEWPORT_FEATURE,
        CANVAS_WINDOW_FEATURE, CanvasExtent, CanvasOptions, Color, WINDOW_SIZE_CONSTRAINTS_FEATURE,
        WINDOW_STYLE_FEATURE, WindowSizeConstraints, WindowStyle,
    };
    struct Features(&'static [&'static str]);
    impl Host for Features {
        fn supported_operations(&self) -> &[Opcode] {
            &[]
        }
        fn supported_features(&self) -> &[&str] {
            self.0
        }
    }
    let (baseline, _, _) = fixture();
    let mut context = baseline.context.clone();
    let features = &[
        CANVAS_CLEAR_FEATURE,
        CANVAS_PIXELS_FEATURE,
        CANVAS_VIEWPORT_FEATURE,
        CANVAS_WINDOW_FEATURE,
        WINDOW_STYLE_FEATURE,
        WINDOW_SIZE_CONSTRAINTS_FEATURE,
    ];
    context.required_capabilities.extend(features.iter().copied().map(str::to_owned));
    let mut admitted = State::new(context.clone(), Box::new(Features(features))).unwrap();
    let names = environment::capabilities(&admitted).unwrap();
    assert!(features.iter().all(|feature| names.iter().any(|name| name == feature)));
    let mut view_state = playbit_api::context::ContextState::default();
    let mut view = view_state.view();
    view.canvas_with_options(
        0,
        240,
        160,
        CanvasOptions { extent: CanvasExtent::Viewport, ..Default::default() },
        &[],
    );
    let scene = view.finish();
    assert_eq!(control::view_submit(&mut admitted, &scene), Ok(()));

    let pixels_only = Features(&[CANVAS_PIXELS_FEATURE]);
    assert!(matches!(
        crate::runtime::instance::validate_context(&context, &pixels_only),
        Err(HostError::Unsupported)
    ));
    let mut ordinary = State::new(baseline.context.clone(), Box::new(pixels_only)).unwrap();
    assert_eq!(control::view_submit(&mut ordinary, &scene), Err(HostError::Unsupported));

    // Each feature is checked even when it was omitted from the manifest.
    let mut view = view_state.view();
    view.column_start();
    view.column_end();
    let previous = view.finish();
    let mut view = view_state.view();
    view.window_style(WindowStyle::STANDARD);
    view.window_size_constraints(WindowSizeConstraints { min_width: 240.5, ..Default::default() });
    view.canvas_with_options(
        0,
        240,
        160,
        CanvasOptions {
            extent: CanvasExtent::Window,
            clear_color: Some(Color(0)),
            ..Default::default()
        },
        &[],
    );
    let styled = view.finish();
    assert_eq!(control::view_submit(&mut admitted, &styled), Ok(()));
    for supported in [
        &[CANVAS_WINDOW_FEATURE, CANVAS_CLEAR_FEATURE, WINDOW_SIZE_CONSTRAINTS_FEATURE][..],
        &[WINDOW_STYLE_FEATURE, CANVAS_CLEAR_FEATURE, WINDOW_SIZE_CONSTRAINTS_FEATURE],
        &[WINDOW_STYLE_FEATURE, CANVAS_WINDOW_FEATURE, WINDOW_SIZE_CONSTRAINTS_FEATURE],
        &[WINDOW_STYLE_FEATURE, CANVAS_WINDOW_FEATURE, CANVAS_CLEAR_FEATURE],
    ] {
        let mut partial =
            State::new(baseline.context.clone(), Box::new(Features(supported))).unwrap();
        assert_eq!(control::view_submit(&mut partial, &previous), Ok(()));
        assert_eq!(control::view_submit(&mut partial, &styled), Err(HostError::Unsupported));
        assert_eq!(partial.view_bytes.as_deref(), Some(previous.as_slice()));
    }
}

#[path = "limit_tests.rs"]
mod limits;
