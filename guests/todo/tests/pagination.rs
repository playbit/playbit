//! The shared TODO app consumes small host pages through native and both Wasm backends.
#![allow(unsafe_code, reason = "Tests own callbacks and assert fixtures.")]
#[path = "../src/lib.rs"]
mod native;
use playbit::runtime::{
    ExecutionBackend, Frame, Runtime, ViewNode,
    host::{DataCall, DataKind, DataResult, QueryLimits},
    native::NativeInstance,
    syscall::{
        AvailableVolume, Event, EventKind, FieldChange, GuestContext, GuestInstance, Host,
        HostError, HostResult, Identity, ObjectPage, ObjectRecord, Opcode, Value, VolumeRights,
    },
};
use playbit_api::ui::CheckState;
use playbit_test_fixtures::Guest;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

struct Records {
    rows: BTreeMap<[u8; 16], ObjectRecord>,
    calls: Vec<(Option<[u8; 16]>, u32)>,
    fail_after: Option<[u8; 16]>,
    edits: Vec<[u8; 16]>,
    creates: usize,
    reads: Vec<[u8; 16]>,
    fail_read: bool,
}
#[derive(Default)]
struct Completions {
    next: u64,
    results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
    ready: Vec<(u64, u64, bool)>,
}
struct Fixture {
    records: Arc<Mutex<Records>>,
    limits: QueryLimits,
    completions: Arc<Mutex<Completions>>,
}
impl Host for Fixture {
    fn query_limits(&self) -> QueryLimits {
        self.limits
    }
    fn supported_operations(&self) -> &[Opcode] {
        &[
            Opcode::EntryResolveBegin,
            Opcode::EntryResolveTake,
            Opcode::ObjectQueryBegin,
            Opcode::ObjectQueryTake,
            Opcode::ObjectReadBegin,
            Opcode::ObjectReadTake,
            Opcode::ObjectEditBegin,
            Opcode::ObjectEditTake,
            Opcode::ObjectCreateBegin,
            Opcode::ObjectCreatePoll,
        ]
    }
    fn begin_data(&mut self, generation: u64, call: Arc<DataCall>) -> HostResult<u64> {
        // This backend is memory only. Tests control delivery independently of
        // execution, including delayed completions and short-output retention.
        let creation = call.operation.kind() == DataKind::Create;
        let result = call.as_ref().clone().execute(self).map(Arc::new);
        let mut completions = self.completions.lock().unwrap();
        completions.next += 1;
        let token = completions.next;
        completions.results.insert(token, result);
        completions.ready.push((generation, token, creation));
        Ok(token)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        self.completions.lock().unwrap().results.get(&token).unwrap().clone()
    }
    fn release_data(&mut self, token: u64) {
        self.completions.lock().unwrap().results.remove(&token);
    }
    fn resolve_entry(&mut self, volume: [u8; 16], name: &str) -> HostResult<[u8; 15]> {
        assert_eq!((volume, name), ([4; 16], "data"));
        Ok([5; 15])
    }
    fn query_objects(
        &mut self,
        volume: playbit::runtime::host::EntryAddress,
        type_id: [u8; 16],
        after: Option<[u8; 16]>,
        limit: u32,
        projection: Option<&[[u8; 16]]>,
    ) -> HostResult<ObjectPage> {
        assert_eq!(volume.volume_id, [4; 16]);
        assert_eq!(volume.entry_id, [5; 15]);
        assert_eq!(type_id, [1; 16]);
        assert_eq!(projection, Some([[2; 16], [3; 16]].as_slice()));
        assert!(limit > 0 && limit <= self.limits.records);
        let mut records = self.records.lock().unwrap();
        records.calls.push((after, limit));
        if after.is_some() && after == records.fail_after {
            return Err(HostError::NotFound);
        }
        let mut page = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for (&id, row) in &records.rows {
            if after.is_some_and(|after| id <= after) {
                continue;
            }
            let length: usize = row
                .fields
                .iter()
                .flat_map(|f| f.values.as_deref().unwrap_or_default())
                .map(|v| match v {
                    Value::Text(text) => text.len(),
                    Value::Boolean(_) => 1,
                    _ => panic!("unexpected fixture value"),
                })
                .sum();
            if page.len() == limit as usize || bytes + length > self.limits.value_bytes as usize {
                if page.is_empty() {
                    return Err(HostError::LimitExceeded);
                }
                more = true;
                break;
            }
            bytes += length;
            page.push(row.clone());
        }
        Ok(ObjectPage {
            next: more.then(|| page.last().unwrap().id),
            objects: page.iter().map(|row| row.id).collect(),
            records: Some(page),
        })
    }
    fn read_object(
        &mut self,
        volume: playbit::runtime::host::EntryAddress,
        object: [u8; 16],
        fields: &[[u8; 16]],
    ) -> HostResult<ObjectRecord> {
        assert_eq!(volume.volume_id, [4; 16]);
        assert_eq!(volume.entry_id, [5; 15]);
        assert_eq!(fields, &[[2; 16], [3; 16]]);
        let mut records = self.records.lock().unwrap();
        records.reads.push(object);
        if records.fail_read {
            return Err(HostError::NotFound);
        }
        records.rows.get(&object).cloned().ok_or(HostError::NotFound)
    }
    fn edit_fields(
        &mut self,
        _: playbit::runtime::host::EntryAddress,
        object: [u8; 16],
        fields: &[FieldChange],
    ) -> HostResult<()> {
        let mut records = self.records.lock().unwrap();
        let row = records.rows.get_mut(&object).ok_or(HostError::NotFound)?;
        for change in fields {
            *row.fields.iter_mut().find(|field| field.field_id == change.field_id).unwrap() =
                change.clone();
        }
        records.edits.push(object);
        Ok(())
    }
    fn create_object(
        &mut self,
        _: playbit::runtime::host::EntryAddress,
        _: [u8; 16],
        fields: &[FieldChange],
    ) -> HostResult<[u8; 16]> {
        let mut records = self.records.lock().unwrap();
        records.creates += 1;
        let id = [200; 16];
        records.rows.insert(id, ObjectRecord { id, type_id: [1; 16], fields: fields.to_vec() });
        Ok(id)
    }
}
fn row(id: u8, length: usize) -> ObjectRecord {
    ObjectRecord {
        id: [id; 16],
        type_id: [1; 16],
        fields: vec![
            FieldChange {
                field_id: [2; 16],
                values: Some(vec![Value::Text(format!("{id:03}{}", "x".repeat(length - 3)))]),
            },
            FieldChange { field_id: [3; 16], values: Some(vec![Value::Boolean(false)]) },
        ],
    }
}
fn context() -> GuestContext {
    GuestContext {
        app_name: "Test app".into(),
        app_build: None,
        identity: Identity {
            account_id: [5; 16],
            is_service: false,
            device_id: [6; 16],
            instance_id: [8; 16],
            session_id: None,
        },
        volumes: vec![
            AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [4; 16],
                name: "Root".into(),
                tags: vec![],
                rights: VolumeRights::ALL,
                is_root: false,
                role: playbit_api::context::VolumeRole::Session as u32,
            },
            playbit::runtime::syscall::AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [9; 16],
                name: "Fixture Root".into(),
                tags: vec![],
                rights: playbit::runtime::syscall::VolumeRights::READ,
                is_root: true,
                role: 0,
            },
        ],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: Default::default(),
        participants: vec![],
    }
}
enum Backend {
    Native(NativeInstance),
    Wasm(GuestInstance),
}
struct Observed {
    logs: Vec<String>,
    view: Option<ViewNode>,
}
impl Observed {
    fn logs(&self) -> &[String] {
        &self.logs
    }
    fn view(&self) -> Option<&ViewNode> {
        self.view.as_ref()
    }
}
struct App {
    backend: Backend,
    completions: Arc<Mutex<Completions>>,
}
impl App {
    fn new(
        backend: Option<ExecutionBackend>,
        records: Arc<Mutex<Records>>,
        limits: QueryLimits,
    ) -> Self {
        let completions = Arc::new(Mutex::new(Completions::default()));
        let host = Box::new(Fixture { records, limits, completions: completions.clone() });
        let backend = match backend {
            None => Backend::Native(unsafe {
                NativeInstance::new(context(), host, playbit_api::native_app()).unwrap()
            }),
            Some(backend) => Backend::Wasm(
                Runtime::with_backend(backend)
                    .unwrap()
                    .create_guest(Guest::Todo.wasm(), context(), host)
                    .unwrap(),
            ),
        };
        Self { backend, completions }
    }
    fn turn(&mut self) -> Frame {
        let ready = std::mem::take(&mut self.completions.lock().unwrap().ready);
        for (generation, token, creation) in ready {
            match &mut self.backend {
                Backend::Native(app) => {
                    if creation {
                        app.notify_creation(generation, token)
                    } else {
                        app.notify_data(generation, token)
                    }
                }
                Backend::Wasm(app) => {
                    if creation {
                        app.notify_creation(generation, token)
                    } else {
                        app.notify_data(generation, token)
                    }
                }
            }
            .unwrap();
        }
        match &mut self.backend {
            Backend::Native(app) => app.pump().unwrap(),
            Backend::Wasm(app) => app.pump().unwrap(),
        }
    }
    fn pump(&mut self) -> Observed {
        // Test helper drains exact readiness, with no production timer. Keep all
        // logs and the newest scene across the finite sequence of page turns.
        let (logs, view) = self.turn().into_native_parts();
        let mut frame = Observed { logs, view };
        for _ in 0..130 {
            if self.completions.lock().unwrap().ready.is_empty() {
                return frame;
            }
            let (logs, view) = self.turn().into_native_parts();
            frame.logs.extend(logs);
            if view.is_some() {
                frame.view = view;
            }
        }
        panic!("TODO dependency chain did not finish");
    }
    fn event(&mut self, kind: EventKind, target: u32, data: Vec<u8>) {
        let data = if kind == EventKind::TextChanged {
            playbit_api::abi::text::TextInput::encode(
                core::str::from_utf8(&data).expect("fixture text"),
                &[],
            )
            .expect("fixture edit")
        } else {
            data
        };
        let event = Event { kind, target, data };
        match &mut self.backend {
            Backend::Native(app) => app.queue_event(app.generation(), event).unwrap(),
            Backend::Wasm(app) => app.queue_event(app.generation(), event).unwrap(),
        }
    }
}
fn checkboxes(view: &ViewNode) -> BTreeMap<String, (u32, CheckState)> {
    match view {
        ViewNode::Theme { child, .. } => checkboxes(child),
        ViewNode::Column(children) | ViewNode::Row(children) => {
            children.iter().flat_map(checkboxes).collect()
        }
        ViewNode::Checkbox { id, label, state, .. } => {
            BTreeMap::from([(label.clone(), (*id, *state))])
        }
        _ => BTreeMap::new(),
    }
}
fn draft(view: &ViewNode) -> Option<&str> {
    match view {
        ViewNode::Theme { child, .. } => draft(child),
        ViewNode::Column(children) | ViewNode::Row(children) => children.iter().find_map(draft),
        ViewNode::TextField { value, .. } => Some(value),
        _ => None,
    }
}
fn checkbox_disabled(view: &ViewNode, target: u32) -> Option<bool> {
    match view {
        ViewNode::Theme { child, .. } => checkbox_disabled(child, target),
        ViewNode::Column(children) | ViewNode::Row(children) => {
            children.iter().find_map(|child| checkbox_disabled(child, target))
        }
        ViewNode::Checkbox { id, disabled, .. } if *id == target => Some(*disabled),
        _ => None,
    }
}
fn records(count: u8, length: usize) -> Arc<Mutex<Records>> {
    Arc::new(Mutex::new(Records {
        rows: (1..=count).map(|id| ([id; 16], row(id, length))).collect(),
        calls: vec![],
        fail_after: None,
        edits: vec![],
        creates: 0,
        reads: vec![],
        fail_read: false,
    }))
}
const BACKENDS: [Option<ExecutionBackend>; 3] =
    [None, Some(ExecutionBackend::Native), Some(ExecutionBackend::Pulley)];

#[test]
fn row_and_byte_limited_pages_keep_all_tasks_and_bound_total_work() {
    // Real adapters must follow both full and short pages, stop at 64 tasks,
    // and preserve checkbox identities when an insertion shifts page boundaries.
    for backend in BACKENDS {
        for (count, length, page_rows) in
            [(0, 256, 8), (1, 256, 8), (32, 256, 8), (32, 3000, 2), (65, 256, 8)]
        {
            let records = records(count, length);
            let mut app =
                App::new(backend, records.clone(), QueryLimits { records: 8, value_bytes: 8192 });
            let initial = app.pump();
            assert!(initial.logs().is_empty(), "{:?}", initial.logs());
            let before = checkboxes(initial.view().unwrap());
            assert_eq!(before.len(), usize::from(count).min(64));
            let expected_calls = usize::from(count).min(64).div_ceil(page_rows).max(1);
            assert_eq!(records.lock().unwrap().calls.len(), expected_calls);
            if count == 0 {
                continue;
            }
            let (title, &(target, _)) = before.iter().next_back().unwrap();
            records.lock().unwrap().rows.insert([0; 16], row(0, length));
            app.event(EventKind::DataChanged, 0, vec![]);
            let changed = app.pump();
            let after = checkboxes(changed.view().unwrap());
            for (title, (id, _)) in &before {
                if let Some((new_id, _)) = after.get(title) {
                    assert_eq!(new_id, id);
                }
            }
            if count <= 32 {
                app.event(EventKind::Click, target, vec![]);
                let toggled = app.pump();
                assert_eq!(checkboxes(toggled.view().unwrap())[title].1, CheckState::On);
                assert_eq!(records.lock().unwrap().edits, [[count; 16]]);
            }
        }
    }
}

#[test]
fn later_page_failure_preserves_rows_and_control_ids_until_success() {
    // Fail after the first page has seen a new ID, then recover. Neither partial
    // data nor tentative control allocation may escape the failed refresh.
    for backend in BACKENDS {
        let records = records(32, 256);
        let mut app =
            App::new(backend, records.clone(), QueryLimits { records: 8, value_bytes: 8192 });
        let first = app.pump();
        let before = checkboxes(first.view().unwrap());
        let next = before.values().map(|(id, _)| *id).max().unwrap() + 1;
        {
            let mut records = records.lock().unwrap();
            records.rows.insert([0; 16], row(0, 256));
            records.fail_after = Some([7; 16]);
        }
        app.event(EventKind::DataChanged, 0, vec![]);
        let failed = app.pump();
        assert!(failed.logs().iter().any(|s| s == "Operation failed: NotFound"));
        assert_eq!(checkboxes(failed.view().unwrap()), before);
        records.lock().unwrap().fail_after = None;
        app.event(EventKind::DataChanged, 0, vec![]);
        let recovered = app.pump();
        let after = checkboxes(recovered.view().unwrap());
        assert_eq!(after.iter().next().unwrap().1.0, next);
        for (title, identity) in before {
            assert_eq!(after[&title], identity);
        }
    }
}

#[test]
fn submission_reserves_boolean_byte_in_host_payload_budget() {
    // An 8-KiB host can return 8191 title bytes plus the Boolean. Reject the
    // next byte before create, retain the draft, and accept the exact boundary.
    for backend in BACKENDS {
        let records = records(0, 256);
        let mut app =
            App::new(backend, records.clone(), QueryLimits { records: 8, value_bytes: 8192 });
        app.pump();
        app.event(EventKind::Submit, 1, vec![b'x'; 8192]);
        let rejected = app.pump();
        assert!(rejected.logs().iter().any(|s| s.contains("8191 UTF-8 bytes")));
        assert_eq!(records.lock().unwrap().creates, 0);
        assert_eq!(draft(rejected.view().unwrap()), Some("x".repeat(8192).as_str()));
        app.event(EventKind::Submit, 1, vec![b'x'; 8191]);
        let accepted = app.pump();
        assert_eq!(records.lock().unwrap().creates, 1);
        assert_eq!(draft(accepted.view().unwrap()), Some(""));
        assert_eq!(checkboxes(accepted.view().unwrap()).keys().next().unwrap().len(), 8191);
    }
}

#[test]
fn invalid_task_fields_and_oversized_titles_keep_previous_scene() {
    // The runtime admits general object values, while this app requires singular
    // title/completion fields and bounded titles. Exercise errors on page two.
    for backend in BACKENDS {
        for fault in 0..4 {
            let records = records(15, 256);
            let mut app =
                App::new(backend, records.clone(), QueryLimits { records: 8, value_bytes: 65536 });
            let initial = app.pump();
            let before = checkboxes(initial.view().unwrap());
            {
                let mut records = records.lock().unwrap();
                let row = records.rows.get_mut(&[9; 16]).unwrap();
                row.fields[0].values = Some(match fault {
                    0 => vec![Value::Text("x".repeat(8193))],
                    1 => vec![Value::Boolean(true)],
                    2 => vec![Value::Text("a".into()), Value::Text("b".into())],
                    _ => vec![Value::Text("a".into()); 20],
                });
            }
            app.event(EventKind::DataChanged, 0, vec![]);
            let failed = app.pump();
            let expected = if fault == 0 || fault == 3 {
                "Operation failed: LimitExceeded"
            } else {
                "Operation failed: InvalidArgument"
            };
            assert!(failed.logs().iter().any(|line| line == expected), "{:?}", failed.logs());
            assert_eq!(checkboxes(failed.view().unwrap()), before);
        }
    }
}

#[test]
fn keyboard_reduced_phone_surface_keeps_text_input_and_all_tasks_without_storage() {
    use playbit_api::context::Surface;
    for backend in BACKENDS {
        let records = records(32, 256);
        let mut app =
            App::new(backend, records.clone(), QueryLimits { records: 8, value_bytes: 8192 });
        let initial = app.pump();
        let rows = checkboxes(initial.view().unwrap());
        app.event(EventKind::TextChanged, 1, b"Phone draft".to_vec());
        app.pump();
        let calls = records.lock().unwrap().calls.len();
        for height in [600, 368, 280, 1, 600] {
            let bytes = Surface {
                width: 343,
                height,
                pixel_scale: 3.0,
                inputs: Surface::TOUCH | Surface::KEYBOARD,
            }
            .encode()
            .unwrap();
            app.event(EventKind::SurfaceChanged, 0, bytes.to_vec());
            let frame = app.pump();
            assert!(frame.logs().is_empty());
            assert_eq!(draft(frame.view().unwrap()), Some("Phone draft"));
            assert_eq!(checkboxes(frame.view().unwrap()), rows);
        }
        let records = records.lock().unwrap();
        assert_eq!(records.calls.len(), calls);
        assert!(records.edits.is_empty());
        assert_eq!(records.creates, 0);
    }
}
#[test]
fn change_notice_during_save_uses_one_fenced_refresh_without_row_readback() {
    for backend in BACKENDS {
        let records = records(2, 32);
        let mut app =
            App::new(backend, records.clone(), QueryLimits { records: 1, value_bytes: 8192 });
        let initial = app.pump();
        let target = checkboxes(initial.view().unwrap()).values().next().unwrap().0;
        app.event(EventKind::TextChanged, 1, b"Independent next draft".to_vec());
        app.pump();
        app.event(EventKind::Click, target, vec![]);
        let accepted = app.turn();
        assert_eq!(checkbox_disabled(accepted.view().unwrap(), target), Some(true));
        let queries = records.lock().unwrap().calls.len();
        // A concurrent title change must be adopted from storage together with
        // the known committed edit, without an optimistic patch or another edit.
        records.lock().unwrap().rows.get_mut(&[2; 16]).unwrap().fields[0].values =
            Some(vec![Value::Text("Concurrent authoritative title".into())]);
        app.event(EventKind::DataChanged, 0, vec![]);
        let reading = app.turn();
        // The edit has committed, but the authoritative query is still pending.
        // Keep the old row disabled until the saved value can replace it.
        assert_eq!(
            checkbox_disabled(reading.view().unwrap(), target),
            Some(true),
            "{backend:?}: {:?}",
            reading.logs()
        );
        assert_eq!(
            checkboxes(reading.view().unwrap()).values().find(|(id, _)| *id == target),
            Some(&(target, CheckState::Off))
        );
        let paged = app.turn();
        assert_eq!(checkbox_disabled(paged.view().unwrap(), target), Some(true));
        assert_eq!(checkboxes(paged.view().unwrap()), checkboxes(reading.view().unwrap()));
        let complete = app.pump();
        assert_eq!(checkbox_disabled(complete.view().unwrap(), target), Some(false));
        assert_eq!(
            checkboxes(complete.view().unwrap()).values().find(|(id, _)| *id == target),
            Some(&(target, CheckState::On))
        );
        assert!(
            checkboxes(complete.view().unwrap()).contains_key("Concurrent authoritative title")
        );
        assert_eq!(draft(complete.view().unwrap()), Some("Independent next draft"));
        assert_eq!(
            reading
                .logs()
                .iter()
                .chain(complete.logs())
                .filter(|line| *line == "TODO changed")
                .count(),
            1
        );
        let records = records.lock().unwrap();
        assert_eq!(records.edits, [[1; 16]]);
        assert!(records.reads.is_empty());
        assert_eq!(records.calls.len(), queries + 2);
    }
}

#[test]
fn saved_query_invalidation_keeps_controls_disabled_and_failure_releases_them() {
    for backend in BACKENDS {
        for retry in [false, true] {
            let records = records(2, 32);
            let mut app = App::new(backend, records.clone(), QueryLimits::default());
            let initial = app.pump();
            let before = checkboxes(initial.view().unwrap());
            let target = before.values().next().unwrap().0;
            app.event(EventKind::Click, target, vec![]);
            app.turn();
            app.event(EventKind::DataChanged, 0, vec![]);
            app.turn();
            // Deliver an invalidated snapshot or a terminal query error after
            // the edit committed. Neither result contains an accepted row.
            {
                let mut completions = app.completions.lock().unwrap();
                assert_eq!(completions.results.len(), 1);
                *completions.results.values_mut().next().unwrap() =
                    Err(if retry { HostError::Unavailable } else { HostError::NotFound });
            }
            if retry {
                app.event(EventKind::DataChanged, 0, vec![]);
            }
            let failed = app.turn();
            assert_eq!(checkboxes(failed.view().unwrap()), before);
            assert_eq!(
                checkbox_disabled(failed.view().unwrap(), target),
                Some(retry),
                "{backend:?}, retry={retry}: {:?}",
                failed.logs()
            );
            if !retry {
                assert!(failed.logs().iter().any(|line| line == "Operation failed: NotFound"));
                app.event(EventKind::DataChanged, 0, vec![]);
            }
            let recovered = app.pump();
            assert_eq!(checkbox_disabled(recovered.view().unwrap(), target), Some(false));
            assert_eq!(
                checkboxes(recovered.view().unwrap()).values().find(|(id, _)| *id == target),
                Some(&(target, CheckState::On))
            );
            let records = records.lock().unwrap();
            assert_eq!(records.edits, [[1; 16]]);
            assert!(records.reads.is_empty());
        }
    }
}

#[test]
fn toggle_reads_only_committed_record_and_failed_readback_keeps_accepted_row() {
    // The returned title proves readback is used, not an optimistic Boolean
    // patch. Fail the next read after commit, then recover through DataChanged.
    for backend in BACKENDS {
        let records = records(32, 256);
        let mut app =
            App::new(backend, records.clone(), QueryLimits { records: 8, value_bytes: 8192 });
        let initial = app.pump();
        let before = checkboxes(initial.view().unwrap());
        let (old_title, &(target, _)) = before.iter().next_back().unwrap();
        let queries = records.lock().unwrap().calls.len();
        records.lock().unwrap().rows.get_mut(&[32; 16]).unwrap().fields[0].values =
            Some(vec![Value::Text("Authoritative title".into())]);
        app.event(EventKind::Click, target, vec![]);
        let changed = app.pump();
        let after = checkboxes(changed.view().unwrap());
        assert!(!after.contains_key(old_title));
        assert_eq!(after["Authoritative title"], (target, CheckState::On));
        for (title, identity) in before.iter().filter(|(title, _)| *title != old_title) {
            assert_eq!(after[title], *identity);
        }
        assert_eq!(records.lock().unwrap().calls.len(), queries);
        assert_eq!(records.lock().unwrap().reads, [[32; 16]]);
        records.lock().unwrap().fail_read = true;
        app.event(EventKind::Click, target, vec![]);
        let failed = app.pump();
        assert!(failed.logs().iter().any(|line| line == "Operation failed: NotFound"));
        assert_eq!(checkboxes(failed.view().unwrap()), after);
        assert_eq!(records.lock().unwrap().calls.len(), queries);
        assert_eq!(records.lock().unwrap().reads, [[32; 16], [32; 16]]);
        records.lock().unwrap().fail_read = false;
        app.event(EventKind::DataChanged, 0, vec![]);
        let recovered = app.pump();
        assert_eq!(
            checkboxes(recovered.view().unwrap())["Authoritative title"],
            (target, CheckState::Off)
        );
        assert!(records.lock().unwrap().calls.len() > queries);
    }
}

#[test]
fn draft_typing_during_pending_save_survives_durable_completion() {
    for backend in BACKENDS {
        let records = records(1, 32);
        let mut app = App::new(backend, records.clone(), QueryLimits::default());
        app.pump();
        app.event(EventKind::Submit, 1, b"First task".to_vec());
        let saving = app.turn();
        assert_eq!(draft(saving.view().unwrap()), Some("First task"));
        assert_eq!(checkboxes(saving.view().unwrap()).len(), 1);
        app.event(EventKind::TextChanged, 1, b"Next private draft".to_vec());
        let completed = app.pump();
        assert_eq!(draft(completed.view().unwrap()), Some("Next private draft"));
        assert!(checkboxes(completed.view().unwrap()).contains_key("First task"));
        assert_eq!(records.lock().unwrap().creates, 1);
        assert!(app.completions.lock().unwrap().results.is_empty());
    }
}

#[test]
fn deleted_task_rejects_accepted_old_control_without_creating_an_object() {
    for backend in BACKENDS {
        let records = records(2, 32);
        let mut app = App::new(backend, records.clone(), QueryLimits::default());
        let initial = app.pump();
        let target = checkboxes(initial.view().unwrap()).values().next().unwrap().0;
        records.lock().unwrap().rows.remove(&[1; 16]);
        app.event(EventKind::DataChanged, 0, vec![]);
        app.event(EventKind::Click, target, vec![]);
        let failed = app.pump();
        assert!(failed.logs().iter().any(|line| line == "Operation failed: NotFound"));
        let records = records.lock().unwrap();
        assert!(records.edits.is_empty());
        assert_eq!(records.creates, 0);
        assert_eq!(records.rows.len(), 1);
    }
}
