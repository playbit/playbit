#[path = "../../../playbit/tests/support/wasm_owner.rs"]
mod wasm_owner;
use playbit::engine::Engine;
use playbit::package::Package;
use playbit::runtime::syscall::{Event, EventKind};
use playbit::store::{Access, Id, object_contract::Value};
use playbit_test_fixtures::Guest;
use std::{collections::BTreeMap, path::Path};
use wasm_owner::WasmOwner;

#[test]
fn entry_permission_rejection_preserves_draft_without_creating_an_object() {
    use playbit::runtime::{ViewNode, decode_view};
    use playbit::store::object_contract::{Acl, EntryAcl, Grant, Permission, Principal, Subject};
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let access = Access::owner(engine.services.store.account().id);
    let declaration =
        playbit::package::VolumeInitializers::from_json(include_bytes!("../playbit-volumes.json"))
            .unwrap()
            .for_role(playbit::package::AutomaticVolumeRole::Session);
    let root = engine
        .services
        .store
        .initialize_root(Id::generate().unwrap(), "TODO permission rejection", false, &declaration)
        .unwrap();
    let database = database_address(&engine.services.store, root.id);
    let package =
        Package::new("todo".into(), "TODO".into(), Guest::Todo.wasm().to_vec(), BTreeMap::new())
            .unwrap();
    let package =
        playbit::package::signature::testing::signed(&engine.services.store, package).unwrap();
    let session = engine.create_session(package, root.id).unwrap();
    let mut owner = WasmOwner::open_with_role(
        &engine,
        &session.id,
        playbit_api::context::VolumeRole::Session as u32,
    );
    engine
        .services
        .store
        .set_entry_permissions(
            &access,
            root.id,
            database.entry,
            EntryAcl::Replace(Acl {
                grants: vec![Grant {
                    subject: Subject::Principal(Principal::Human(
                        engine.services.store.account().id,
                    )),
                    permissions: vec![Permission::Read],
                }],
            }),
        )
        .unwrap();
    let draft = "Retained after entry permission rejection";
    owner
        .input(Event { kind: EventKind::Submit, target: 1, data: draft.as_bytes().to_vec() })
        .unwrap();
    fn text_field(node: &ViewNode) -> Option<&str> {
        match node {
            ViewNode::TextField { id: 1, value, .. } => Some(value),
            ViewNode::Theme { child, .. } => text_field(child),
            ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
                children.iter().find_map(text_field)
            }
            _ => None,
        }
    }
    let view = decode_view(owner.scene()).unwrap();
    assert_eq!(text_field(&view), Some(draft));
    assert_eq!(
        engine
            .services
            .store
            .query_objects(&access, database, Id::from_bytes([1; 16]), None, 64)
            .unwrap()
            .objects
            .len(),
        1
    );
    engine
        .services
        .store
        .set_entry_permissions(&access, root.id, database.entry, EntryAcl::Inherit)
        .unwrap();
    owner
        .input(Event { kind: EventKind::Submit, target: 1, data: draft.as_bytes().to_vec() })
        .unwrap();
    assert_eq!(
        engine
            .services
            .store
            .query_objects(&access, database, Id::from_bytes([1; 16]), None, 64)
            .unwrap()
            .objects
            .len(),
        2
    );
}

#[test]
fn oversized_submission_is_consumed_without_orphans_and_next_task_succeeds() {
    let directory = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut engine = Engine::open(directory.path()).unwrap();
    let access = Access::owner(engine.services.store.account().id);
    let root = engine.services.store.create_volume("TODO input regression", false).unwrap();
    let declaration = playbit::package::read_volume_initializers(source)
        .unwrap()
        .for_role(playbit::package::AutomaticVolumeRole::Session);
    let playbit::store::InitialDatabaseKind::ObjectDb { schema, .. } =
        &declaration.databases[0].kind
    else {
        panic!()
    };
    engine
        .services
        .store
        .create_database(
            &access,
            root.id,
            "data",
            playbit::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let title_field = Id::from_bytes([2; 16]);
    let retained = "r".repeat(4335);
    for title in [retained.as_str(), "Existing one", "Existing two", "Existing three"] {
        engine
            .services
            .store
            .create_object_with_fields(
                &access,
                database_address(&engine.services.store, root.id),
                Id::from_bytes([1; 16]),
                &BTreeMap::from([
                    (title_field, Some(vec![Value::Text(title.into())])),
                    (Id::from_bytes([3; 16]), Some(vec![Value::Boolean(false)])),
                ]),
            )
            .unwrap();
    }
    let package =
        Package::new("todo".into(), "TODO".into(), Guest::Todo.wasm().to_vec(), BTreeMap::new())
            .unwrap();
    let package =
        playbit::package::signature::testing::signed(&engine.services.store, package).unwrap();
    let session = engine.create_session(package, root.id).unwrap();
    let mut owner = WasmOwner::open_with_role(
        &engine,
        &session.id,
        playbit_api::context::VolumeRole::Session as u32,
    );
    for data in [
        format!("Bob oversized task: {}", "x".repeat(9000)).into_bytes(),
        vec![b'x'; 65536],
        "🦀".repeat(2049).into_bytes(),
    ] {
        owner
            .input(Event { kind: EventKind::Submit, target: 1, data })
            .expect("oversized input must not trap");
        let view = owner.scene().to_vec();
        assert!(
            view.windows(b"Use at most 8,192 UTF-8 bytes".len())
                .any(|bytes| bytes == b"Use at most 8,192 UTF-8 bytes")
        );
        assert!(
            view.windows(b"Task is too long".len())
                .position(|bytes| bytes == b"Task is too long")
                .unwrap()
                < view
                    .windows(retained.len())
                    .position(|bytes| bytes == retained.as_bytes())
                    .unwrap(),
            "validation stays beside the input, before long task rows"
        );
        assert_eq!(
            engine
                .services
                .store
                .query_objects(
                    &access,
                    database_address(&engine.services.store, root.id),
                    Id::from_bytes([1; 16]),
                    None,
                    64
                )
                .unwrap()
                .objects
                .len(),
            4,
            "rejection creates no empty object"
        );
    }
    owner.data_changed();
    assert!(
        owner
            .scene()
            .to_vec()
            .windows(b"Task is too long".len())
            .any(|bytes| bytes == b"Task is too long"),
        "background events retain feedback"
    );
    owner
        .input(Event {
            kind: EventKind::Submit,
            target: 1,
            data: b"Short task after rejection".to_vec(),
        })
        .unwrap();
    let view = owner.scene().to_vec();
    assert!(!view.windows(b"Task is too long".len()).any(|bytes| bytes == b"Task is too long"));
    // 8,120 bytes fit the old event buffer but overflowed its serialized edit.
    // Several accepted titles also exceed the old fixed 32 KiB scene buffer.
    let accepted = ["b".repeat(8120), "a".repeat(8192), "c".repeat(8192), "d".repeat(8192)];
    for title in &accepted {
        owner
            .input(Event { kind: EventKind::Submit, target: 1, data: title.as_bytes().to_vec() })
            .unwrap();
    }
    let objects = engine
        .services
        .store
        .query_objects(
            &access,
            database_address(&engine.services.store, root.id),
            Id::from_bytes([1; 16]),
            None,
            64,
        )
        .unwrap()
        .objects;
    assert_eq!(objects.len(), 9);
    let titles = objects
        .iter()
        .map(|object| match object.fields[&title_field].values.as_deref() {
            Some([Value::Text(title)]) => title.as_str(),
            _ => panic!("empty task object"),
        })
        .collect::<Vec<_>>();
    assert!(titles.contains(&retained.as_str()), "existing 4,335-byte title stays intact");
    for title in &accepted {
        assert!(titles.contains(&title.as_str()), "accepted title fits its serialized edit");
    }
    assert!(titles.contains(&"Short task after rejection"));
    owner.quit();
    drop(owner);
    drop(engine);
    let reopened = Engine::open(directory.path()).unwrap();
    let mut restored = WasmOwner::open_with_role(
        &reopened,
        &session.id,
        playbit_api::context::VolumeRole::Session as u32,
    );
    let view = restored.scene().to_vec();
    assert_eq!(restored.session.id, session.id);
    assert!(view.windows(retained.len()).any(|bytes| bytes == retained.as_bytes()));
    assert!(view.len() > 32768);
    for title in &accepted {
        assert!(view.windows(title.len()).any(|bytes| bytes == title.as_bytes()));
    }
    restored.quit();
}

#[test]
fn projected_refresh_keeps_large_pages_external_edits_and_row_limit() {
    let directory = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut engine = Engine::open(directory.path()).unwrap();
    let access = Access::owner(engine.services.store.account().id);
    let root = engine.services.store.create_volume("TODO projected refresh", false).unwrap();
    let declaration = playbit::package::read_volume_initializers(source)
        .unwrap()
        .for_role(playbit::package::AutomaticVolumeRole::Session);
    let playbit::store::InitialDatabaseKind::ObjectDb { schema, .. } =
        &declaration.databases[0].kind
    else {
        panic!()
    };
    engine
        .services
        .store
        .create_database(
            &access,
            root.id,
            "data",
            playbit::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let title_field = Id::from_bytes([2; 16]);
    let done_field = Id::from_bytes([3; 16]);
    for index in 0..64 {
        let title = format!("{index:02}{}", "x".repeat(8190));
        engine
            .services
            .store
            .create_object_with_fields(
                &access,
                database_address(&engine.services.store, root.id),
                Id::from_bytes([1; 16]),
                &BTreeMap::from([
                    (title_field, Some(vec![Value::Text(title)])),
                    (done_field, Some(vec![Value::Boolean(false)])),
                ]),
            )
            .unwrap();
    }
    let package =
        Package::new("todo".into(), "TODO".into(), Guest::Todo.wasm().to_vec(), BTreeMap::new())
            .unwrap();
    let package =
        playbit::package::signature::testing::signed(&engine.services.store, package).unwrap();
    let session = engine.create_session(package, root.id).unwrap();
    let mut owner = WasmOwner::open_with_role(
        &engine,
        &session.id,
        playbit_api::context::VolumeRole::Session as u32,
    );
    let initial = owner.scene().to_vec();
    assert!(initial.len() > 500_000, "projected result exceeds the old 64 KiB buffer");
    let first = engine
        .services
        .store
        .query_objects(
            &access,
            database_address(&engine.services.store, root.id),
            Id::from_bytes([1; 16]),
            None,
            64,
        )
        .unwrap()
        .objects[0]
        .object_id;
    engine
        .services
        .store
        .edit_fields(
            &access,
            database_address(&engine.services.store, root.id),
            first,
            &BTreeMap::from([(
                title_field,
                Some(vec![Value::Text("Externally changed task".into())]),
            )]),
        )
        .unwrap();
    owner.data_changed();
    let updated = owner.scene().to_vec();
    assert!(updated.windows(23).any(|bytes| bytes == b"Externally changed task"));
    owner.input(Event { kind: EventKind::Click, target: 100, data: vec![] }).unwrap();
    let object = engine
        .services
        .store
        .read_object(&access, database_address(&engine.services.store, root.id), first)
        .unwrap();
    assert_eq!(object.fields[&done_field].values, Some(vec![Value::Boolean(true)]));
    owner
        .input(Event {
            kind: EventKind::Submit,
            target: 1,
            data: b"Rejected sixty-fifth task".to_vec(),
        })
        .unwrap();
    let rejected = owner.scene().to_vec();
    let feedback = b"This list already contains 64 tasks.";
    assert!(rejected.windows(feedback.len()).any(|bytes| bytes == feedback));
    assert_eq!(
        engine
            .services
            .store
            .query_objects(
                &access,
                database_address(&engine.services.store, root.id),
                Id::from_bytes([1; 16]),
                None,
                64
            )
            .unwrap()
            .objects
            .len(),
        64
    );
    owner.quit();
}

#[test]
fn draft_add_return_and_background_refresh_preserve_the_intended_text() {
    use playbit::runtime::{ViewNode, decode_view};
    fn field(view: &ViewNode) -> &str {
        match view {
            ViewNode::TextField { value, .. } => value,
            ViewNode::Theme { child, .. } => field(child),
            ViewNode::Column(children) | ViewNode::Row(children) => children
                .iter()
                .find_map(|child| {
                    let value = field(child);
                    (!value.is_empty()).then_some(value)
                })
                .unwrap_or(""),
            _ => "",
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut engine = Engine::open(directory.path()).unwrap();
    let access = Access::owner(engine.services.store.account().id);
    let root = engine.services.store.create_volume("TODO draft", false).unwrap();
    let declaration = playbit::package::read_volume_initializers(source)
        .unwrap()
        .for_role(playbit::package::AutomaticVolumeRole::Session);
    let playbit::store::InitialDatabaseKind::ObjectDb { schema, .. } =
        &declaration.databases[0].kind
    else {
        panic!()
    };
    engine
        .services
        .store
        .create_database(
            &access,
            root.id,
            "data",
            playbit::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let package =
        Package::new("todo".into(), "TODO".into(), Guest::Todo.wasm().to_vec(), BTreeMap::new())
            .unwrap();
    let package =
        playbit::package::signature::testing::signed(&engine.services.store, package).unwrap();
    let session = engine.create_session(package, root.id).unwrap();
    let mut owner = WasmOwner::open_with_role(
        &engine,
        &session.id,
        playbit_api::context::VolumeRole::Session as u32,
    );
    for text in ["a", "ab", "a", "  Draft 🦀  "] {
        owner
            .input(Event {
                kind: EventKind::TextChanged,
                target: 1,
                data: playbit_api::abi::text::TextInput::encode(text, &[]).expect("fixture edit"),
            })
            .unwrap();
        let view = decode_view(owner.scene()).unwrap();
        assert_eq!(field(&view), text);
        assert!(
            engine
                .services
                .store
                .query_objects(
                    &access,
                    database_address(&engine.services.store, root.id),
                    Id::from_bytes([1; 16]),
                    None,
                    64
                )
                .unwrap()
                .objects
                .is_empty()
        );
    }
    owner.data_changed();
    assert_eq!(field(&decode_view(owner.scene()).unwrap()), "  Draft 🦀  ");
    owner.input(Event { kind: EventKind::Click, target: 2, data: vec![] }).unwrap();
    assert_eq!(field(&decode_view(owner.scene()).unwrap()), "");
    owner
        .input(Event { kind: EventKind::Submit, target: 1, data: b"Return task".to_vec() })
        .unwrap();
    let objects = engine
        .services
        .store
        .query_objects(
            &access,
            database_address(&engine.services.store, root.id),
            Id::from_bytes([1; 16]),
            None,
            64,
        )
        .unwrap()
        .objects;
    assert_eq!(objects.len(), 2);
    let mut titles = objects
        .iter()
        .map(|object| {
            assert_eq!(
                object.fields[&Id::from_bytes([3; 16])].values,
                Some(vec![Value::Boolean(false)])
            );
            match object.fields[&Id::from_bytes([2; 16])].values.as_deref() {
                Some([Value::Text(title)]) => title.as_str(),
                _ => panic!("created object lacks initial title"),
            }
        })
        .collect::<Vec<_>>();
    titles.sort();
    assert_eq!(titles, ["Draft 🦀", "Return task"]);
    owner.input(Event { kind: EventKind::Submit, target: 1, data: vec![] }).unwrap();
    assert_eq!(
        engine
            .services
            .store
            .query_objects(
                &access,
                database_address(&engine.services.store, root.id),
                Id::from_bytes([1; 16]),
                None,
                64
            )
            .unwrap()
            .objects
            .len(),
        2
    );
    owner.quit();
}

fn database_address(
    store: &playbit::store::LocalStore,
    volume: Id,
) -> playbit::store::volume_index::EntryAddress {
    let entry = store.resolve_entry(&Access::owner(store.account().id), volume, "data").unwrap();
    playbit::store::volume_index::EntryAddress { volume, entry: entry.id }
}

#[test]
fn queued_checkbox_keeps_object_identity_through_insert_reorder_and_deletion() {
    use playbit::runtime::host::{DataCall, DataOperation, DataResult};
    use playbit::runtime::syscall::{
        AvailableVolume, FieldChange, GuestContext, GuestInstance, Host, HostError, HostResult,
        Identity, ObjectPage, ObjectRecord, Opcode, Value as RuntimeValue, VolumeRights,
        validate_input,
    };
    use playbit::runtime::{ExecutionBackend, Runtime, ViewNode};
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
        time::Instant,
    };

    #[derive(Default)]
    struct Records {
        objects: BTreeMap<[u8; 16], ObjectRecord>,
        edits: Vec<[u8; 16]>,
        token: u64,
        results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
        ready: Vec<u64>,
        visible: BTreeSet<u64>,
    }
    struct Fixture(Arc<Mutex<Records>>);
    impl Host for Fixture {
        fn supported_operations(&self) -> &[Opcode] {
            &[
                Opcode::EntryResolveBegin,
                Opcode::EntryResolveTake,
                Opcode::ObjectQueryBegin,
                Opcode::ObjectQueryTake,
                Opcode::ObjectEditBegin,
                Opcode::ObjectEditTake,
            ]
        }
        fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
            assert_eq!(call.volume, [4; 16]);
            let mut records = self.0.lock().unwrap();
            let result = match &call.operation {
                DataOperation::ResolveEntry { name } => {
                    assert_eq!(name, "data");
                    Ok(DataResult::EntryResolved([6; 15]))
                }
                DataOperation::Query { entry_id, type_id, after, limit, projection } => {
                    assert_eq!(*entry_id, [6; 15]);
                    assert_eq!(*type_id, [1; 16]);
                    assert_eq!(*after, None);
                    assert_eq!(*limit, 64);
                    assert_eq!(projection.as_deref(), Some([[2; 16], [3; 16]].as_slice()));
                    Ok(DataResult::Page(ObjectPage {
                        objects: records.objects.keys().copied().collect(),
                        next: None,
                        records: Some(records.objects.values().cloned().collect()),
                    }))
                }
                DataOperation::Edit { entry_id, object_id, fields } => {
                    assert_eq!(*entry_id, [6; 15]);
                    if let Some(row) = records.objects.get_mut(object_id) {
                        for change in fields {
                            let field = row
                                .fields
                                .iter_mut()
                                .find(|field| field.field_id == change.field_id)
                                .expect("declared completion field");
                            *field = change.clone();
                        }
                        records.edits.push(*object_id);
                        Ok(DataResult::Edited)
                    } else {
                        Err(HostError::NotFound)
                    }
                }
                _ => return Err(HostError::Unsupported),
            };
            records.token = records.token.checked_add(1).unwrap();
            let token = records.token;
            records.results.insert(token, result.map(Arc::new));
            records.ready.push(token);
            Ok(token)
        }
        fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
            let records = self.0.lock().unwrap();
            if !records.visible.contains(&token) {
                return Err(HostError::WouldBlock);
            }
            records.results.get(&token).cloned().expect("retained completion")
        }
        fn release_data(&mut self, token: u64) {
            let mut records = self.0.lock().unwrap();
            let _ = records.results.remove(&token).expect("one completion release");
            records.visible.remove(&token);
        }
    }
    fn settle(guest: &mut GuestInstance, records: &Arc<Mutex<Records>>) -> (ViewNode, Vec<String>) {
        let mut view = None;
        let mut logs = vec![];
        for _ in 0..64 {
            let ready = {
                let mut records = records.lock().unwrap();
                let ready = std::mem::take(&mut records.ready);
                records.visible.extend(ready.iter().copied());
                ready
            };
            for token in ready {
                guest.notify_data(guest.generation(), token).unwrap();
            }
            if !guest.has_ready_work(Instant::now()) {
                assert!(
                    records.lock().unwrap().results.is_empty(),
                    "all admitted requests complete"
                );
                return (view.expect("settled scene"), logs);
            }
            let frame = guest.pump().unwrap();
            if let Some(scene) = frame.view() {
                view = Some(scene.clone());
            }
            logs.extend(frame.logs().iter().cloned());
        }
        panic!("bounded test completion turns exhausted");
    }
    fn object(id: u8, title: &str) -> ObjectRecord {
        ObjectRecord {
            id: [id; 16],
            type_id: [1; 16],
            fields: vec![
                FieldChange {
                    field_id: [2; 16],
                    values: Some(vec![RuntimeValue::Text(title.into())]),
                },
                FieldChange { field_id: [3; 16], values: Some(vec![RuntimeValue::Boolean(false)]) },
            ],
        }
    }
    fn checkboxes(view: &ViewNode) -> BTreeMap<String, u32> {
        match view {
            ViewNode::Theme { child, .. } => checkboxes(child),
            ViewNode::Column(children) | ViewNode::Row(children) => {
                children.iter().flat_map(checkboxes).collect()
            }
            ViewNode::Checkbox { id, label, .. } => BTreeMap::from([(label.clone(), *id)]),
            _ => BTreeMap::new(),
        }
    }
    for backend in [ExecutionBackend::Native, ExecutionBackend::Pulley] {
        let records = Arc::new(Mutex::new(Records {
            objects: BTreeMap::from([
                ([20; 16], object(20, "First retained task")),
                ([30; 16], object(30, "Intended task")),
            ]),
            token: 100,
            ..Records::default()
        }));
        let runtime = Runtime::with_backend(backend).unwrap();
        let mut guest = runtime
            .create_guest(
                Guest::Todo.wasm(),
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
                },
                Box::new(Fixture(records.clone())),
            )
            .unwrap();
        let (initial, _) = settle(&mut guest, &records);
        let initial_ids = checkboxes(&initial);
        let click =
            Event { kind: EventKind::Click, target: initial_ids["Intended task"], data: vec![] };
        validate_input(&initial, &click).unwrap();
        // The click was valid for the presented scene. An incoming insertion
        // moves its row from position1 to position2 before the guest handles it.
        records.lock().unwrap().objects.insert([10; 16], object(10, "Earlier insertion"));
        let changed = Event { kind: EventKind::DataChanged, target: 0, data: vec![] };
        guest.queue_event(guest.generation(), changed.clone()).unwrap();
        guest.queue_event(guest.generation(), click.clone()).unwrap();
        let (inserted, _) = settle(&mut guest, &records);
        let inserted_ids = checkboxes(&inserted);
        assert_eq!(inserted_ids["Intended task"], initial_ids["Intended task"]);
        assert_eq!(inserted_ids["First retained task"], initial_ids["First retained task"]);
        assert!(inserted_ids["Earlier insertion"] > *initial_ids.values().max().unwrap());
        assert_eq!(records.lock().unwrap().edits, [[30; 16]]);
        validate_input(&inserted, &click).unwrap();
        // Deletion and a replacement now precede another click queued from the
        // old scene. It must fail rather than acquire the replacement's index.
        {
            let mut records = records.lock().unwrap();
            records.objects.remove(&[30; 16]);
            records.objects.insert([25; 16], object(25, "Replacement task"));
        }
        guest.queue_event(guest.generation(), changed.clone()).unwrap();
        guest.queue_event(guest.generation(), click.clone()).unwrap();
        let (deleted, logs) = settle(&mut guest, &records);
        assert!(logs.iter().any(|line| line == "Operation failed: NotFound"));
        assert_eq!(validate_input(&deleted, &click), Err(HostError::InvalidArgument));
        assert_eq!(
            records.lock().unwrap().edits,
            [[30; 16]],
            "deleted target never edits its neighbor"
        );
        let deleted_ids = checkboxes(&deleted);
        assert!(!deleted_ids.values().any(|id| *id == click.target));
        assert!(deleted_ids["Replacement task"] > *inserted_ids.values().max().unwrap());
        assert_eq!(deleted_ids["First retained task"], initial_ids["First retained task"]);
        // Even the same object ID returning later receives a fresh control ID;
        // no historical identity map or retired-control reuse is retained.
        records.lock().unwrap().objects.insert([30; 16], object(30, "Returned task"));
        guest.queue_event(guest.generation(), changed).unwrap();
        let (returned, _) = settle(&mut guest, &records);
        let returned_ids = checkboxes(&returned);
        assert!(returned_ids["Returned task"] > *deleted_ids.values().max().unwrap());
        assert_ne!(returned_ids["Returned task"], click.target);
    }
}
