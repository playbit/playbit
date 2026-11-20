//! C-authored app exercising the production local data host with durable reopen.
use super::*;
use playbit::{
    engine::{
        Services,
        data_host::DataHost,
        in_process_host::{InProcessHost, Inbox},
    },
    store::{
        Access, Id, object_contract as data,
        volume_index::{EntryAddress, EntryKind},
    },
};
use playbit_api::abi::data as ffi;
use playbit_api::context::{Completion, OperationHandle};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const TYPE: Id = Id::from_bytes([1; 16]);
const DONE: Id = Id::from_bytes([2; 16]);
#[repr(C)]
struct DataProbe {
    base: Probe,
    volume: [u8; 16],
    entry: [u8; 15],
    object: [u8; 16],
    field: [u8; 16],
    kind: [u8; 16],
    checked: u32,
}
unsafe extern "C" {
    fn data_probe_app(probe: *mut DataProbe) -> abi::App;
    fn data_probe_layout(index: u32) -> u64;
}
pub(super) struct Directory(pub(super) std::path::PathBuf);
impl Directory {
    pub(super) fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("playbit-native-data-{}", Id::generate().unwrap()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

// Fixture scheduling belongs outside callbacks. Rust owns production admission
// and completions; this driver only waits for wake and forwards exact readiness.
struct Driver {
    instance: NativeInstance,
    inbox: Inbox,
    wake: mpsc::Receiver<()>,
}
impl Driver {
    unsafe fn new(context: GuestContext, backend: DataHost, app: abi::App) -> Self {
        let (sender, wake) = mpsc::sync_channel(1);
        let (host, inbox) = InProcessHost::local(
            backend,
            Arc::new(move || {
                let _ = sender.try_send(()); // A wake already queued covers all retained results.
            }),
        )
        .unwrap();
        // SAFETY: the caller retains the callback table, user data and scenes.
        let instance = unsafe { NativeInstance::new(context, Box::new(host), app).unwrap() };
        Self { instance, inbox, wake }
    }
    fn pump(&mut self) -> Result<playbit::runtime::Frame, RuntimeError> {
        let frame = self.instance.pump()?;
        self.complete(frame)
    }
    fn complete(
        &mut self,
        mut frame: playbit::runtime::Frame,
    ) -> Result<playbit::runtime::Frame, RuntimeError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        for _ in 0..128 {
            if self.inbox.idle() {
                return Ok(frame);
            }
            for (generation, token, creation) in self.inbox.ready() {
                if creation {
                    self.instance.notify_creation(generation, token).unwrap();
                } else {
                    self.instance.notify_data(generation, token).unwrap();
                }
            }
            if self.instance.has_ready_work(Instant::now()) {
                frame = self.instance.pump()?;
            } else {
                self.wake
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("finite accepted data must wake its fixture owner");
            }
        }
        panic!("fixture exceeded its bounded data continuation count");
    }
    fn generation(&self) -> u64 {
        self.instance.generation()
    }
    fn queue_event(&mut self, generation: u64, event: Event) -> Result<(), HostError> {
        self.instance.queue_event(generation, event)
    }
    fn changed(&mut self) {
        self.queue_event(
            self.generation(),
            Event { kind: EventKind::DataChanged, target: 0, data: vec![] },
        )
        .unwrap();
    }
}

struct Fixture {
    services: Services,
    access: Access,
    volume: Id,
    address: EntryAddress,
    object: Id,
}
impl Fixture {
    fn open(path: &Path) -> Self {
        let services = Services::open(path, || {}).unwrap();
        let store = &services.store;
        let access = Access::owner(store.account().id);
        let volumes = store.volumes().unwrap();
        let volume = match volumes.as_slice() {
            [] => store.create_volume("Native C fixture", false).unwrap().id,
            [volume] => volume.id,
            _ => panic!("fixture has exactly one volume"),
        };
        let schema = data::Schema {
            types: BTreeMap::from([(
                TYPE,
                data::TypeDeclaration {
                    name: "Item".into(),
                    deprecated: false,
                    fields: BTreeMap::from([(
                        DONE,
                        data::FieldDeclaration {
                            name: "done".into(),
                            value_type: data::ValueType::Boolean,
                            repeated: false,
                            deprecated: false,
                        },
                    )]),
                },
            )]),
        };
        let entry = match store.resolve_entry(&access, volume, "items") {
            Ok(entry) => entry,
            Err(playbit::store::Error::NotFound(_)) => store
                .create_database(
                    &access,
                    volume,
                    "items",
                    EntryKind::ObjectDb { schema: schema.clone() },
                )
                .unwrap(),
            Err(error) => panic!("fixture database: {error}"),
        };
        let address = EntryAddress { volume, entry: entry.id };
        assert_eq!(store.schema(&access, address).unwrap(), schema);
        let objects = store.query_objects(&access, address, TYPE, None, 2).unwrap();
        let object = match objects.objects.as_slice() {
            [] => {
                store
                    .create_object_with_fields(
                        &access,
                        address,
                        TYPE,
                        &BTreeMap::from([(DONE, Some(vec![data::Value::Boolean(false)]))]),
                    )
                    .unwrap()
                    .object_id
            }
            [object] => object.object_id,
            _ => panic!("fixture has exactly one item"),
        };
        Self { services, access, volume, address, object }
    }
    fn probe(&self, off: &[u8], on: &[u8]) -> DataProbe {
        DataProbe {
            base: Probe::new(off, on),
            volume: *self.volume.as_bytes(),
            entry: *self.address.entry.as_bytes(),
            object: *self.object.as_bytes(),
            field: *DONE.as_bytes(),
            kind: *TYPE.as_bytes(),
            checked: 0,
        }
    }
    unsafe fn app(&self, probe: &mut DataProbe, changed: Arc<AtomicBool>) -> Driver {
        let mut context = context(50);
        context.identity.account_id = *self.services.store.account().id.as_bytes();
        context.volumes[0].id = *self.volume.as_bytes();
        context.volumes[0].tags = vec!["fixture".into()];
        // SAFETY: the test retains the probe and scenes through app destruction.
        unsafe {
            Driver::new(
                context,
                DataHost {
                    store: self.services.store.clone(),
                    access: self.access.clone(),
                    changed,
                },
                data_probe_app(probe),
            )
        }
    }
    fn done(&self) -> bool {
        let object =
            self.services.store.read_object(&self.access, self.address, self.object).unwrap();
        let value = object.fields.get(&DONE).unwrap().values.as_ref().unwrap();
        let [data::Value::Boolean(value)] = value.as_slice() else { panic!("boolean fixture") };
        *value
    }
}
#[test]
fn c_data_records_match_rust_layouts() {
    let layout = [
        size_of::<ffi::VolumeHandle>(),
        align_of::<ffi::VolumeHandle>(),
        size_of::<ffi::Range>(),
        size_of::<ffi::Volume>(),
        offset_of!(ffi::Volume, rights),
        size_of::<ffi::Volumes>(),
        size_of::<ffi::ValueInput>(),
        offset_of!(ffi::ValueInput, data),
        size_of::<ffi::FieldInput>(),
        size_of::<ffi::Value>(),
        offset_of!(ffi::Value, bytes_offset),
        size_of::<ffi::Field>(),
        size_of::<ffi::Object>(),
        offset_of!(ffi::Object, bytes_data),
        size_of::<DataProbe>(),
        offset_of!(ffi::Volume, grant_id),
        offset_of!(ffi::Volumes, revision),
        offset_of!(ffi::Volumes, volumes_data),
    ];
    for (index, expected) in layout.into_iter().enumerate() {
        // SAFETY: scalar C layout query accesses no caller storage.
        assert_eq!(unsafe { data_probe_layout(index as u32) }, expected as u64, "layout {index}");
    }
}
#[test]
fn c_app_commits_through_scoped_data_host_and_reopens_durable_state() {
    let directory = Directory::new();
    let off = scene(false);
    let on = scene(true);
    let fixture = Fixture::open(&directory.0);
    let changed = Arc::new(AtomicBool::new(false));
    let mut probe = fixture.probe(&off, &on);
    // SAFETY: the probe and scene storage outlive this app.
    let mut app = unsafe { fixture.app(&mut probe, changed.clone()) };
    assert!(!checked(&app.pump().unwrap()));
    assert!(!changed.load(Ordering::Acquire), "rejected edits must not reach the mutating host");
    assert!(!fixture.done());
    app.queue_event(app.generation(), Event { kind: EventKind::Click, target: 1, data: vec![] })
        .unwrap();
    assert!(checked(&app.pump().unwrap()));
    assert!(changed.load(Ordering::Acquire));
    assert!(fixture.done());
    drop(app);
    assert_eq!(probe.base.destroyed, 1);
    drop(fixture);
    let reopened = Fixture::open(&directory.0);
    assert!(reopened.done());
    let mut probe = reopened.probe(&off, &on);
    // SAFETY: the new app borrows this distinct live probe and the same scene storage.
    let mut app = unsafe { reopened.app(&mut probe, Arc::new(AtomicBool::new(false))) };
    assert!(checked(&app.pump().unwrap()));
    assert!(!directory.0.join("control/engine.sock").exists());
    drop(app);
    assert_eq!(probe.base.destroyed, 1);
}

#[test]
fn c_accepted_edit_and_readback_drain_after_deactivation() {
    let directory = Directory::new();
    let fixture = Fixture::open(&directory.0);
    let off = scene(false);
    let on = scene(true);
    let mut probe = fixture.probe(&off, &on);
    // SAFETY: the probe and scene buffers outlive this callback owner.
    let mut app = unsafe { fixture.app(&mut probe, Arc::new(AtomicBool::new(false))) };
    assert!(!checked(&app.pump().unwrap()));
    app.queue_event(app.generation(), Event { kind: EventKind::Click, target: 1, data: vec![] })
        .unwrap();
    let frame = app.instance.pump().unwrap();
    assert!(!app.inbox.idle(), "the accepted edit has not yet been Taken");
    app.instance.deactivate();
    assert!(checked(&app.complete(frame).unwrap()));
    assert!(fixture.done());
    assert!(!app.instance.drain_pending());
    assert!(!app.instance.drain_failed());
    assert_eq!(probe.checked, 1);
    drop(app);
    assert_eq!(probe.base.destroyed, 1);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RustPhase {
    Read,
    Edit,
    Idle,
}
struct RustState {
    probe: *mut DataProbe,
    volume: ffi::EntryHandle,
    operation: OperationHandle,
    phase: RustPhase,
    checked: bool,
}
unsafe extern "C" fn sdk_init(
    raw: *mut abi::Context,
    data: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: the test retains DataProbe; the executor lends this callback context.
    let probe = unsafe { &mut *data.cast::<DataProbe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    context.attach_files(&playbit_api::context::FileResources::default()).unwrap();
    let mut records = [ffi::Volume::default(); 1];
    let mut tags = [ffi::Range::default(); 1];
    let mut bytes = [0; 128];
    let volumes = context.available_volumes(&mut records, &mut tags, &mut bytes).unwrap();
    let root = volumes.iter().find(|volume| volume.is_root).unwrap();
    assert_eq!(root.name, "Root");
    assert_eq!(root.tags().collect::<Vec<_>>(), ["fixture"]);
    assert_eq!(root.id, probe.volume);
    let volume = ffi::EntryHandle {
        volume: root.handle.0,
        entry_id: probe.entry,
        reserved: 0,
        ..Default::default()
    };
    drop(volumes);
    context.flush_files().unwrap();
    let operation = context.begin_read_object(volume, probe.object, &[probe.field]).unwrap();
    let state =
        Box::new(RustState { probe, volume, operation, phase: RustPhase::Read, checked: false });
    probe.base.initialized += 1;
    probe.base.generation = context.identity().unwrap().generation;
    // SAFETY: output belongs to the executor; destroy takes ownership exactly once.
    unsafe {
        output.write(Box::into_raw(state).cast());
    }
    0
}
fn sdk_read(context: &mut playbit_api::context::Context<'_>, state: &RustState) -> bool {
    // SAFETY: init stores the test's live configuration through destruction.
    let probe = unsafe { &*state.probe };
    assert!(matches!(
        context.take_read_object(state.operation, &mut [], &mut [], &mut []),
        Err(playbit_api::context::ReadError::BufferTooSmall { records: 1, values: 1, bytes: 0 })
    ));
    let mut fields = [ffi::Field::default(); 1];
    let mut values = [ffi::Value::default(); 1];
    let Completion::Ready(record) =
        context.take_read_object(state.operation, &mut fields, &mut values, &mut []).unwrap()
    else {
        panic!("notified read must be ready")
    };
    assert_eq!(record.type_id, probe.kind);
    let field = record.field(probe.field).unwrap();
    let mut values = field.values().unwrap();
    let playbit_api::context::ValueRef::Boolean(done) = values.next().unwrap() else {
        panic!("boolean fixture")
    };
    assert!(values.next().is_none());
    done
}
unsafe extern "C" fn sdk_turn(raw: *mut abi::Context, data: *mut c_void) -> i32 {
    // SAFETY: init transferred ownership of this RustState to the executor.
    let state = unsafe { &mut *data.cast::<RustState>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    for event in context.raw_events() {
        match event.kind {
            EventKind::DataReady => {
                assert_eq!(event.operation().unwrap(), state.operation);
                match state.phase {
                    RustPhase::Read => {
                        state.checked = sdk_read(&mut context, state);
                        state.phase = RustPhase::Idle;
                    }
                    RustPhase::Edit => {
                        assert_eq!(
                            context.take_edit_object(state.operation).unwrap(),
                            Completion::Ready(())
                        );
                        let probe = unsafe { &*state.probe };
                        state.operation = context
                            .begin_read_object(state.volume, probe.object, &[probe.field])
                            .unwrap();
                        state.phase = RustPhase::Read;
                    }
                    RustPhase::Idle => panic!("unexpected ready event"),
                }
            }
            EventKind::Click => {
                assert_eq!(state.phase, RustPhase::Idle);
                let values = [playbit_api::context::Value::boolean(!state.checked)];
                let probe = unsafe { &*state.probe };
                let changes =
                    [playbit_api::context::FieldChange::set(probe.field, &values).unwrap()];
                state.operation =
                    context.begin_edit_object(state.volume, probe.object, &changes).unwrap();
                state.phase = RustPhase::Edit;
            }
            EventKind::DataChanged => {
                assert_eq!(state.phase, RustPhase::Idle);
                let probe = unsafe { &*state.probe };
                state.operation =
                    context.begin_read_object(state.volume, probe.object, &[probe.field]).unwrap();
                state.phase = RustPhase::Read;
            }
            _ => {}
        }
    }
    let probe = unsafe { &mut *state.probe };
    probe.base.turns += 1;
    if state.phase != RustPhase::Idle {
        return 0;
    }
    probe.checked = u32::from(state.checked);
    // SAFETY: test-owned scenes outlive both apps.
    let (ptr, len) = if state.checked {
        (probe.base.on, probe.base.on_len)
    } else {
        (probe.base.off, probe.base.off_len)
    };
    context.submit_view(unsafe { std::slice::from_raw_parts(ptr, len as usize) }).unwrap();
    0
}
unsafe extern "C" fn sdk_destroy(data: *mut c_void) {
    // SAFETY: the executor transfers back the Box allocated in init exactly once.
    let state = unsafe { Box::from_raw(data.cast::<RustState>()) };
    unsafe {
        (*state.probe).base.destroyed += 1;
    }
}
#[test]
fn rust_sdk_and_c_app_share_checked_store_operations_with_independent_contexts() {
    let directory = Directory::new();
    let fixture = Fixture::open(&directory.0);
    let off = scene(false);
    let on = scene(true);
    let mut c_probe = fixture.probe(&off, &on);
    let mut rust_probe = fixture.probe(&off, &on);
    let changed = Arc::new(AtomicBool::new(false));
    // SAFETY: both test-owned configurations and scene arrays outlive the apps.
    let mut c_app = unsafe { fixture.app(&mut c_probe, changed.clone()) };
    let mut context = context(70);
    context.identity.account_id = *fixture.services.store.account().id.as_bytes();
    context.volumes[0].id = *fixture.volume.as_bytes();
    context.volumes[0].tags = vec!["fixture".into()];
    let mut rust_app = unsafe {
        Driver::new(
            context,
            DataHost {
                store: fixture.services.store.clone(),
                access: fixture.access.clone(),
                changed,
            },
            abi::App {
                init: Some(sdk_init),
                turn: Some(sdk_turn),
                destroy: Some(sdk_destroy),
                user_data: (&mut rust_probe as *mut DataProbe).cast(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
    };
    assert!(!checked(&c_app.pump().unwrap()));
    assert!(!checked(&rust_app.pump().unwrap()));
    assert_ne!(c_app.generation(), rust_app.generation());
    c_app
        .queue_event(c_app.generation(), Event { kind: EventKind::Click, target: 1, data: vec![] })
        .unwrap();
    assert!(checked(&c_app.pump().unwrap()));
    assert!(
        !checked(&rust_app.pump().unwrap()),
        "Frame keeps the previous snapshot until invalidation"
    );
    rust_app.changed();
    assert!(checked(&rust_app.pump().unwrap()));
    rust_app
        .queue_event(
            rust_app.generation(),
            Event { kind: EventKind::Click, target: 1, data: vec![] },
        )
        .unwrap();
    assert!(!checked(&rust_app.pump().unwrap()));
    assert!(checked(&c_app.pump().unwrap()), "Frame does not reread peer changes");
    c_app.changed();
    assert!(!checked(&c_app.pump().unwrap()));
    assert!(!fixture.done());
    drop(c_app);
    drop(rust_app);
    assert_eq!((c_probe.base.destroyed, rust_probe.base.destroyed), (1, 1));
}

struct DeniedProbe {
    entry: [u8; 15],
    object: [u8; 16],
    read_denied: bool,
    read: Option<OperationHandle>,
    edit: Option<OperationHandle>,
    read_finished: bool,
    edit_finished: bool,
}
unsafe extern "C" fn denied_init(
    raw: *mut abi::Context,
    data: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: the test lends this probe and context for the callback.
    let probe = unsafe { &mut *data.cast::<DeniedProbe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    context.attach_files(&playbit_api::context::FileResources::default()).unwrap();
    let mut records = [ffi::Volume::default(); 1];
    let mut tags = [ffi::Range::default(); 1];
    let mut bytes = [0; 128];
    let volumes = context.available_volumes(&mut records, &mut tags, &mut bytes).unwrap();
    let handle = ffi::EntryHandle {
        volume: volumes.iter().find(|volume| volume.is_root).unwrap().handle.0,
        entry_id: probe.entry,
        reserved: 0,
        ..Default::default()
    };
    drop(volumes);
    context.flush_files().unwrap();
    let values = [playbit_api::context::Value::boolean(true)];
    let changes = [playbit_api::context::FieldChange::set(*DONE.as_bytes(), &values).unwrap()];
    let edit = context.begin_edit_object(handle, probe.object, &changes);
    if probe.read_denied {
        probe.edit = Some(edit.unwrap());
    } else {
        assert_eq!(edit, Err(HostError::AccessDenied));
        probe.edit_finished = true;
    }
    probe.read =
        Some(context.begin_read_object(handle, probe.object, &[*DONE.as_bytes()]).unwrap());
    // SAFETY: the executor retains this borrowed probe until destruction.
    unsafe {
        output.write(data);
    }
    0
}
unsafe extern "C" fn denied_turn(raw: *mut abi::Context, data: *mut c_void) -> i32 {
    // SAFETY: the test retains this borrowed probe; this is the live callback context.
    let probe = unsafe { &mut *data.cast::<DeniedProbe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    for event in context.raw_events() {
        if event.kind != EventKind::DataReady {
            continue;
        }
        let handle = event.operation().unwrap();
        if Some(handle) == probe.edit {
            assert_eq!(context.take_edit_object(handle), Err(HostError::AccessDenied));
            probe.edit = None;
            probe.edit_finished = true;
        } else {
            assert_eq!(Some(handle), probe.read);
            let mut fields = [ffi::Field::default(); 1];
            let mut values = [ffi::Value::default(); 1];
            let result = context.take_read_object(handle, &mut fields, &mut values, &mut []);
            if probe.read_denied {
                assert!(matches!(
                    result,
                    Err(playbit_api::context::ReadError::Host(HostError::AccessDenied))
                ));
            } else {
                assert!(matches!(result, Ok(Completion::Ready(_))));
            }
            probe.read = None;
            probe.read_finished = true;
        }
    }
    0
}
unsafe extern "C" fn no_destroy(_: *mut c_void) {}
#[test]
fn native_edits_obey_mount_rights_and_current_store_principal() {
    let directory = Directory::new();
    let fixture = Fixture::open(&directory.0);
    for wrong_principal in [false, true] {
        let mut context = context(80);
        context.volumes[0].id = *fixture.volume.as_bytes();
        if !wrong_principal {
            context.volumes[0].rights = VolumeRights::READ;
        }
        let mut probe = DeniedProbe {
            entry: *fixture.address.entry.as_bytes(),
            object: *fixture.object.as_bytes(),
            read_denied: wrong_principal,
            read: None,
            edit: None,
            read_finished: false,
            edit_finished: false,
        };
        let access = if wrong_principal {
            Access::owner(Id::from_bytes([0xff; 16]))
        } else {
            fixture.access.clone()
        };
        // SAFETY: the callbacks synchronously borrow probe, which outlives app.
        let mut app = unsafe {
            Driver::new(
                context,
                DataHost {
                    store: fixture.services.store.clone(),
                    access,
                    changed: Arc::new(AtomicBool::new(false)),
                },
                abi::App {
                    init: Some(denied_init),
                    turn: Some(denied_turn),
                    destroy: Some(no_destroy),
                    user_data: (&mut probe as *mut DeniedProbe).cast(),
                    size: size_of::<abi::App>() as u32,
                    written: 0,
                },
            )
        };
        app.pump().unwrap();
        assert!(probe.read_finished && probe.edit_finished);
        assert!(!fixture.done());
        drop(app);
    }
}

#[test]
#[ignore = "Operator-only durable CPU timing; excludes renderer and physical presentation."]
fn persistent_native_input_to_validated_scene_timing() {
    use std::time::Instant;
    let directory = Directory::new();
    let setup = Instant::now();
    let fixture = Fixture::open(&directory.0);
    let setup = setup.elapsed();
    let off = scene(false);
    let on = scene(true);
    let mut probe = fixture.probe(&off, &on);
    let startup = Instant::now();
    // SAFETY: app borrows only the test-owned probe and live scenes.
    let mut app = unsafe { fixture.app(&mut probe, Arc::new(AtomicBool::new(false))) };
    assert!(!checked(&app.pump().unwrap()));
    let startup = startup.elapsed();
    let mut timings = Vec::with_capacity(1000);
    for index in 0..1000 {
        let start = Instant::now();
        app.queue_event(
            app.generation(),
            Event { kind: EventKind::Click, target: 1, data: vec![] },
        )
        .unwrap();
        let frame = app.pump().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(checked(&frame), index % 2 == 0);
        timings.push(elapsed);
    }
    timings.sort_unstable();
    eprintln!(
        "durable native C checkbox: store+provision={setup:?}, app+first scene={startup:?}; 1000 queue+worker+commit+readback+validated-scene samples: p50={:?} p95={:?} p99={:?} max={:?}",
        timings[499], timings[949], timings[989], timings[999]
    );
    drop(app);
    assert_eq!(probe.base.destroyed, 1);
}

#[test]
fn c_app_establishes_its_schema_creates_data_and_discovers_it_after_reopen() {
    let directory = Directory::new();
    let off = scene(false);
    let on = scene(true);
    let services = Services::open(&directory.0, || {}).unwrap();
    let access = Access::owner(services.store.account().id);
    let volume = services.store.create_volume("Native C fixture", false).unwrap().id;
    let entry = services
        .store
        .create_database(
            &access,
            volume,
            "items",
            EntryKind::ObjectDb { schema: data::Schema::default() },
        )
        .unwrap();
    let address = EntryAddress { volume, entry: entry.id };
    let mut fixture =
        Fixture { services, access, volume, address, object: Id::from_bytes([0; 16]) };
    let mut probe = fixture.probe(&off, &on);
    // SAFETY: configuration and scenes outlive the app, including bootstrap.
    let mut app = unsafe { fixture.app(&mut probe, Arc::new(AtomicBool::new(false))) };
    assert!(!checked(&app.pump().unwrap()));
    fixture.object = Id::from_bytes(probe.object);
    assert_ne!(probe.object, [0; 16]);
    assert!(!fixture.done());
    app.queue_event(app.generation(), Event { kind: EventKind::Click, target: 1, data: vec![] })
        .unwrap();
    assert!(checked(&app.pump().unwrap()));
    let object = probe.object;
    drop(app);
    drop(fixture);
    let services = Services::open(&directory.0, || {}).unwrap();
    let access = Access::owner(services.store.account().id);
    let fixture = Fixture { services, access, volume, address, object: Id::from_bytes([0; 16]) };
    let mut probe = fixture.probe(&off, &on);
    let changed = Arc::new(AtomicBool::new(false));
    // SAFETY: reopened app has a fresh live configuration and borrowed scenes.
    let mut app = unsafe { fixture.app(&mut probe, changed.clone()) };
    assert!(checked(&app.pump().unwrap()));
    assert_eq!(probe.object, object);
    assert!(!changed.load(Ordering::Acquire), "reopening only discovers existing data");
    drop(app);
}

#[test]
fn c_catalog_layouts_match_rust() {
    use playbit_api::abi::schema as s;
    unsafe extern "C" {
        fn catalog_probe_layout(index: u32) -> u64;
    }
    let expected = [
        size_of::<*const u8>(),
        size_of::<s::FieldInput>(),
        offset_of!(s::FieldInput, reference_type),
        size_of::<s::TypeInput>(),
        offset_of!(s::TypeInput, fields),
        size_of::<s::Field>(),
        size_of::<s::Type>(),
        size_of::<s::Schema>(),
        size_of::<ffi::Query>(),
        offset_of!(ffi::Query, fields),
        size_of::<ffi::ObjectInfo>(),
        size_of::<ffi::Page>(),
        offset_of!(ffi::Page, objects_data),
    ];
    for (index, size) in expected.into_iter().enumerate() {
        // SAFETY: C query reads no caller-owned memory.
        assert_eq!(unsafe { catalog_probe_layout(index as u32) }, size as u64, "layout {index}");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogPhase {
    Establish,
    Schema,
    Create(usize),
    Projected,
    Ids,
    Done,
}
struct CatalogProbe {
    ids: Vec<[u8; 16]>,
    found: Vec<[u8; 16]>,
    volume: ffi::EntryHandle,
    operation: OperationHandle,
    phase: CatalogPhase,
    after: Option<[u8; 16]>,
}
unsafe extern "C" fn catalog_init(
    raw: *mut abi::Context,
    data: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    use playbit_api::context::{SchemaField, SchemaType, ValueType};
    // SAFETY: test owns the probe and the executor lends the callback context.
    let probe = unsafe { &mut *data.cast::<CatalogProbe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    context.attach_files(&playbit_api::context::FileResources::default()).unwrap();
    let mut volumes = [ffi::Volume::default(); 1];
    let mut tags = [ffi::Range::default(); 1];
    let mut names = [0; 128];
    probe.volume.volume = context
        .available_volumes(&mut volumes, &mut tags, &mut names)
        .unwrap()
        .iter()
        .find(|v| v.is_root)
        .unwrap()
        .handle
        .0;
    context.flush_files().unwrap();
    let fields = [
        SchemaField::new(*DONE.as_bytes(), "done", ValueType::Boolean, false, false).unwrap(),
        SchemaField::new([3; 16], "title", ValueType::Text, false, false).unwrap(),
        SchemaField::new([4; 16], "count", ValueType::Integer, false, false).unwrap(),
        SchemaField::new([5; 16], "score", ValueType::Float, true, true).unwrap(),
        SchemaField::new([6; 16], "bytes", ValueType::Bytes, false, false).unwrap(),
        SchemaField::new([7; 16], "parent", ValueType::Reference(*TYPE.as_bytes()), false, false)
            .unwrap(),
        SchemaField::new([8; 16], "file", ValueType::File, false, false).unwrap(),
    ];
    let types = [SchemaType::new(*TYPE.as_bytes(), "Item", &fields, false).unwrap()];
    probe.operation = context.begin_establish_schema(probe.volume, &types).unwrap();
    // All nested declarations die here; Begin must own them before execution.
    unsafe {
        output.write(data);
    }
    0
}
fn catalog_create(
    context: &mut playbit_api::context::Context<'_>,
    probe: &mut CatalogProbe,
    index: usize,
) {
    use playbit_api::context::{FieldChange, Value};
    let done = [Value::boolean(index % 2 == 1)];
    let text = [Value::text(["one", "two", "three"][index]).unwrap()];
    let changes = [
        FieldChange::set(*DONE.as_bytes(), &done).unwrap(),
        FieldChange::set([3; 16], &text).unwrap(),
    ];
    probe.operation =
        context.begin_create_object(probe.volume, *TYPE.as_bytes(), &changes).unwrap();
    probe.phase = CatalogPhase::Create(index);
}
fn catalog_query(
    context: &mut playbit_api::context::Context<'_>,
    probe: &mut CatalogProbe,
    projected: bool,
) {
    let projection = [*DONE.as_bytes(), [3; 16]];
    probe.operation = context
        .begin_query_objects(
            probe.volume,
            playbit_api::context::Query {
                type_id: *TYPE.as_bytes(),
                after: probe.after,
                limit: if projected { 2 } else { 4 },
                fields: projected.then_some(projection.as_slice()),
            },
        )
        .unwrap();
    probe.phase = if projected { CatalogPhase::Projected } else { CatalogPhase::Ids };
}
fn catalog_schema(context: &mut playbit_api::context::Context<'_>, probe: &mut CatalogProbe) {
    use playbit_api::{
        abi::schema as s,
        context::{ReadError, ValueType},
    };
    assert!(matches!(
        context.take_read_schema(probe.operation, &mut [], &mut [], &mut []),
        Err(ReadError::BufferTooSmall { records: 1, values: 7, bytes: 38 })
    ));
    let mut types = [s::Type::default(); 1];
    let mut fields = [s::Field::default(); 7];
    let mut names = [0; 128];
    let Completion::Ready(schema) =
        context.take_read_schema(probe.operation, &mut types, &mut fields, &mut names).unwrap()
    else {
        panic!("notified schema must be ready")
    };
    let ty = schema.types().next().unwrap();
    assert_eq!((ty.id, ty.name, ty.deprecated), (*TYPE.as_bytes(), "Item", false));
    let declarations = ty
        .fields()
        .map(|field| (field.id, field.name, field.value_type, field.repeated, field.deprecated))
        .collect::<Vec<_>>();
    for expected in [
        (*DONE.as_bytes(), "done", ValueType::Boolean, false, false),
        ([3; 16], "title", ValueType::Text, false, false),
        ([4; 16], "count", ValueType::Integer, false, false),
        ([5; 16], "score", ValueType::Float, true, true),
        ([6; 16], "bytes", ValueType::Bytes, false, false),
        ([7; 16], "parent", ValueType::Reference(*TYPE.as_bytes()), false, false),
        ([8; 16], "file", ValueType::File, false, false),
    ] {
        assert!(declarations.contains(&expected));
    }
    catalog_create(context, probe, 0);
}
fn catalog_page(context: &mut playbit_api::context::Context<'_>, probe: &mut CatalogProbe) {
    use playbit_api::context::{PageStorage, QueryError, ValueRef};
    let count = (3 - probe.found.len()).min(2);
    assert!(matches!(context.take_query_objects(probe.operation, PageStorage {
        objects: &mut [], fields: &mut [], values: &mut [], bytes: &mut [],
    }), Err(QueryError::BufferTooSmall { objects, fields, values, .. })
        if objects == count && fields == count * 2 && values == count * 2));
    let mut objects = [ffi::ObjectInfo::default(); 2];
    let mut fields = [ffi::Field::default(); 4];
    let mut values = [ffi::Value::default(); 4];
    let mut bytes = [0; 32];
    let Completion::Ready(page) = context
        .take_query_objects(
            probe.operation,
            PageStorage {
                objects: &mut objects,
                fields: &mut fields,
                values: &mut values,
                bytes: &mut bytes,
            },
        )
        .unwrap()
    else {
        panic!("notified page must be ready")
    };
    for item in page.iter() {
        let index = probe.ids.iter().position(|id| *id == item.id).unwrap();
        let record = item.record.unwrap();
        assert_eq!(record.type_id, *TYPE.as_bytes());
        assert_eq!(
            record.field(*DONE.as_bytes()).unwrap().values().unwrap().next(),
            Some(ValueRef::Boolean(index % 2 == 1))
        );
        assert_eq!(
            record.field([3; 16]).unwrap().values().unwrap().next(),
            Some(ValueRef::Text(["one", "two", "three"][index]))
        );
        probe.found.push(item.id);
    }
    probe.after = page.next;
    if probe.after.is_some() {
        catalog_query(context, probe, true);
    } else {
        let mut expected = probe.ids.clone();
        expected.sort_unstable();
        assert_eq!(probe.found, expected);
        catalog_query(context, probe, false);
    }
}
unsafe extern "C" fn catalog_turn(raw: *mut abi::Context, data: *mut c_void) -> i32 {
    use playbit_api::context::{FieldChange, PageStorage, Value};
    // SAFETY: the test retains this borrowed probe and callback context.
    let probe = unsafe { &mut *data.cast::<CatalogProbe>() };
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) =
        unsafe { playbit_api::context::Context::from_raw(raw, &mut context_state).unwrap() };
    for event in context.raw_events() {
        if !matches!(event.kind, EventKind::DataReady | EventKind::ObjectCreationReady) {
            continue;
        }
        assert_eq!(event.operation().unwrap(), probe.operation);
        match probe.phase {
            CatalogPhase::Establish => {
                assert_eq!(
                    context.take_establish_schema(probe.operation).unwrap(),
                    Completion::Ready(())
                );
                probe.operation = context.begin_read_schema(probe.volume).unwrap();
                probe.phase = CatalogPhase::Schema;
            }
            CatalogPhase::Schema => catalog_schema(&mut context, probe),
            CatalogPhase::Create(index) => {
                let Completion::Ready(id) = context.poll_create_object(probe.operation).unwrap()
                else {
                    panic!("notified create must be ready")
                };
                probe.ids.push(id);
                if index < 2 {
                    catalog_create(&mut context, probe, index + 1);
                } else {
                    let value = [Value::boolean(false)];
                    let repeated = [
                        FieldChange::set(*DONE.as_bytes(), &value).unwrap(),
                        FieldChange::set(*DONE.as_bytes(), &value).unwrap(),
                    ];
                    assert_eq!(
                        context.begin_create_object(probe.volume, *TYPE.as_bytes(), &repeated),
                        Err(HostError::InvalidArgument)
                    );
                    catalog_query(&mut context, probe, true);
                }
            }
            CatalogPhase::Projected => catalog_page(&mut context, probe),
            CatalogPhase::Ids => {
                let mut objects = [ffi::ObjectInfo::default(); 4];
                let Completion::Ready(page) = context
                    .take_query_objects(
                        probe.operation,
                        PageStorage {
                            objects: &mut objects,
                            fields: &mut [],
                            values: &mut [],
                            bytes: &mut [],
                        },
                    )
                    .unwrap()
                else {
                    panic!("notified IDs must be ready")
                };
                assert_eq!(page.iter().map(|o| o.id).collect::<Vec<_>>(), probe.found);
                assert!(page.iter().all(|o| o.record.is_none()));
                probe.phase = CatalogPhase::Done;
            }
            CatalogPhase::Done => panic!("unexpected terminal catalog event"),
        }
    }
    0
}
#[test]
fn typed_sdk_establishes_creates_and_pages_projected_records() {
    let directory = Directory::new();
    let services = Services::open(&directory.0, || {}).unwrap();
    let access = Access::owner(services.store.account().id);
    let volume = services.store.create_volume("SDK catalog", false).unwrap().id;
    let entry = services
        .store
        .create_database(
            &access,
            volume,
            "items",
            EntryKind::ObjectDb { schema: data::Schema::default() },
        )
        .unwrap();
    let address = EntryAddress { volume, entry: entry.id };
    let mut context = context(90);
    context.volumes[0].id = *volume.as_bytes();
    let mut probe = CatalogProbe {
        ids: Vec::new(),
        found: Vec::new(),
        volume: ffi::EntryHandle {
            volume: 0,
            entry_id: *entry.id.as_bytes(),
            reserved: 0,
            ..Default::default()
        },
        operation: OperationHandle::default(),
        phase: CatalogPhase::Establish,
        after: None,
    };
    // SAFETY: callbacks borrow the test-owned probe until app destruction.
    let mut app = unsafe {
        Driver::new(
            context,
            DataHost {
                store: services.store.clone(),
                access: access.clone(),
                changed: Arc::new(AtomicBool::new(false)),
            },
            abi::App {
                init: Some(catalog_init),
                turn: Some(catalog_turn),
                destroy: Some(no_destroy),
                user_data: (&mut probe as *mut CatalogProbe).cast(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
    };
    app.pump().unwrap();
    assert_eq!(probe.phase, CatalogPhase::Done);
    drop(app);
    assert_eq!(
        services.store.query_objects(&access, address, TYPE, None, 4).unwrap().objects.len(),
        3
    );
    drop(services);
    let reopened = Services::open(&directory.0, || {}).unwrap();
    assert_eq!(
        reopened.store.query_objects(&access, address, TYPE, None, 4).unwrap().objects.len(),
        3
    );
}
