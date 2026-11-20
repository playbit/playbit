//! Same TODO source through native calls and Wasm imports, backed by durable services.
#![allow(unsafe_code, reason = "Tests own app lifetimes and assert fixtures.")]
#[path = "../../../playbit/tests/support/wasm_owner.rs"]
mod wasm_owner;
use playbit::engine::Engine;
use wasm_owner::WasmOwner;
#[path = "../src/lib.rs"]
mod native;
use playbit::{
    engine::{Services, native::Apps},
    package::Package,
    runtime::{
        ViewNode, decode_view,
        syscall::{Event, EventKind},
    },
    store::{Access, Id, object_contract::Value},
};
use playbit_api::ui::CheckState;
use playbit_test_fixtures::Guest;
use std::{collections::BTreeMap, path::Path, time::Instant};

const TYPE: Id = Id::from_bytes([1; 16]);
const TITLE: Id = Id::from_bytes([2; 16]);
const DONE: Id = Id::from_bytes([3; 16]);

fn root(services: &Services, rows: usize) -> Id {
    let store = &services.store;
    let access = Access::owner(store.account().id);
    let root = store.create_volume("TODO parity", false).unwrap().id;
    let declaration =
        playbit::package::VolumeInitializers::from_json(include_bytes!("../playbit-volumes.json"))
            .unwrap()
            .for_role(playbit::package::AutomaticVolumeRole::Session);
    let playbit::store::InitialDatabaseKind::ObjectDb { schema, .. } =
        &declaration.databases[0].kind
    else {
        panic!()
    };
    let entry = store
        .create_database(
            &access,
            root,
            "data",
            playbit::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    let database = playbit::store::volume_index::EntryAddress { volume: root, entry: entry.id };
    for index in 0..rows {
        store
            .create_object_with_fields(
                &access,
                database,
                TYPE,
                &BTreeMap::from([
                    (
                        TITLE,
                        Some(vec![Value::Text(format!("Task {index:02} {}", "x".repeat(248)))]),
                    ),
                    (DONE, Some(vec![Value::Boolean(false)])),
                ]),
            )
            .unwrap();
    }
    root
}
fn package(services: &Services) -> Package {
    let package = playbit::package::Manifest::from_source(include_str!("../playbit.toml"))
        .unwrap()
        .bundle(Path::new(env!("CARGO_MANIFEST_DIR")), Guest::Todo.wasm().to_vec())
        .unwrap();
    playbit::package::signature::testing::signed(&services.store, package).unwrap()
}
#[derive(Debug, Default, PartialEq)]
struct Snapshot {
    theme: Option<playbit_api::ui::Theme>,
    draft: String,
    add_disabled: bool,
    texts: Vec<String>,
    rows: BTreeMap<String, CheckState>,
}
fn snapshot(view: &ViewNode) -> Snapshot {
    // Independent volumes assign random object IDs, so compare rows by title;
    // the actual target is separately resolved from each accepted scene.
    let mut result = Snapshot::default();
    let mut pending = vec![view];
    while let Some(node) = pending.pop() {
        match node {
            ViewNode::Theme { theme, child } => {
                result.theme = Some(theme.clone());
                pending.push(child);
            }
            ViewNode::Column(nodes) | ViewNode::Row(nodes) => pending.extend(nodes.iter().rev()),
            ViewNode::TextField { value, .. } => result.draft = value.clone(),
            ViewNode::Button { disabled, .. } => result.add_disabled = *disabled,
            ViewNode::Text { text, .. } => result.texts.push(text.clone()),
            ViewNode::Checkbox { label, state, .. } => {
                assert!(result.rows.insert(label.clone(), *state).is_none());
            }
            _ => panic!("unexpected TODO scene node: {node:?}"),
        }
    }
    result
}
fn target(view: &ViewNode, title: &str) -> u32 {
    let mut pending = vec![view];
    while let Some(node) = pending.pop() {
        match node {
            ViewNode::Theme { child, .. } => pending.push(child),
            ViewNode::Column(nodes) | ViewNode::Row(nodes) => pending.extend(nodes),
            ViewNode::Checkbox { id, label, .. } if label == title => return *id,
            _ => {}
        }
    }
    panic!("missing task {title}");
}
fn input(kind: EventKind, target: u32, data: &[u8]) -> Event {
    let encoded;
    let data = if kind == EventKind::TextChanged {
        encoded = playbit_api::abi::text::TextInput::encode(
            core::str::from_utf8(data).expect("fixture text"),
            &[],
        )
        .expect("fixture edit");
        encoded.as_slice()
    } else {
        data
    };
    Event { kind, target, data: data.to_vec() }
}
fn settle(apps: &mut Apps, generation: u64) {
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let ready = apps.step(generation, Instant::now()).unwrap().ready;
        if !ready && !apps.drain_pending(generation).unwrap() {
            break;
        }
        assert!(Instant::now() < deadline, "native completion did not arrive");
        if ready {
            apps.advance(generation, |_| {}).unwrap();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
fn native_input(apps: &mut Apps, generation: u64, event: Event) {
    apps.input(generation, event).unwrap();
    apps.advance(generation, |_| {}).unwrap();
    settle(apps, generation);
}

#[test]
fn native_and_wasm_todo_agree_at_one_32_and_64_rows_and_reopen() {
    // Exercise the same shared Rust app through both ABIs with durable, independently
    // provisioned roots. Includes Unicode drafts, add/rejection, toggle and reopen.
    for rows in [1, 32, 64] {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let native_root = root(&engine.services, rows);
        let wasm_root = root(&engine.services, rows);
        let session = engine.create_session(package(&engine.services), wasm_root).unwrap();
        let mut apps = Apps::new(std::sync::Arc::new(|| {}));
        // SAFETY: callbacks are static, each attachment owns its state, and apps
        // are destroyed on this thread before the engine services.
        let generation = unsafe {
            apps.attach_fixture_role(
                &engine.services,
                native_root,
                playbit_api::native_app(),
                playbit_api::context::VolumeRole::Session,
            )
        }
        .unwrap();
        apps.advance(generation, |_| {}).unwrap();
        settle(&mut apps, generation);
        let mut owner = WasmOwner::open_with_role(
            &engine,
            &session.id,
            playbit_api::context::VolumeRole::Session as u32,
        );
        let compare = |owner: &mut WasmOwner, apps: &Apps| {
            assert_eq!(owner.session.id, session.id);
            let wasm = decode_view(owner.scene()).unwrap();
            let native = apps.scene(generation).unwrap().unwrap();
            assert_eq!(snapshot(native), snapshot(&wasm), "{rows}-row parity");
            wasm
        };
        let wasm = compare(&mut owner, &apps);
        let title = snapshot(&wasm).rows.keys().next().unwrap().clone();
        let native_target = target(apps.scene(generation).unwrap().unwrap(), &title);
        native_input(&mut apps, generation, input(EventKind::Click, native_target, &[]));
        owner.input(input(EventKind::Click, target(&wasm, &title), &[])).unwrap();
        let state = snapshot(&compare(&mut owner, &apps));
        assert_eq!(state.rows[&title], CheckState::On);
        for event in [
            input(EventKind::TextChanged, 1, "  Shared 🦀 draft  ".as_bytes()),
            input(EventKind::Submit, 1, "  Shared 🦀 draft  ".as_bytes()),
            input(EventKind::Submit, 1, &vec![b'x'; 8193]),
            input(EventKind::Submit, 1, &vec![b'x'; 65536]),
            input(EventKind::TextChanged, 1, b"Recovery task"),
            input(EventKind::Submit, 1, b"Recovery task"),
        ] {
            native_input(&mut apps, generation, event.clone());
            owner.input(event).unwrap();
            compare(&mut owner, &apps);
        }
        let before = snapshot(apps.scene(generation).unwrap().unwrap());
        assert_eq!(before.rows.len(), if rows == 64 { 64 } else { rows + 2 });
        apps.clear();
        owner.quit();
        drop(owner);
        drop(engine);
        let engine = Engine::open(directory.path()).unwrap();
        let generation = unsafe {
            apps.attach_fixture_role(
                &engine.services,
                native_root,
                playbit_api::native_app(),
                playbit_api::context::VolumeRole::Session,
            )
        }
        .unwrap();
        apps.advance(generation, |_| {}).unwrap();
        settle(&mut apps, generation);
        let mut owner = WasmOwner::open_with_role(
            &engine,
            &session.id,
            playbit_api::context::VolumeRole::Session as u32,
        );
        let reopened = snapshot(apps.scene(generation).unwrap().unwrap());
        assert_eq!(reopened.rows, before.rows);
        assert_eq!(reopened.draft, "");
        assert_eq!(reopened, snapshot(&decode_view(owner.scene()).unwrap()));
        owner.quit();
        apps.clear();
    }
}

#[test]
fn four_native_todos_keep_drafts_private_and_follow_committed_changes() {
    // All four apps share durable data but must have independent app allocations,
    // volume handles, drafts and lifetimes. Refresh only from committed feed work.
    let directory = tempfile::tempdir().unwrap();
    let services = Services::open(directory.path(), || {}).unwrap();
    let root = root(&services, 1);
    let mut changes = services.store.take_change_receiver().unwrap();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let generations: Vec<_> = (0..4)
        .map(|_| unsafe {
            apps.attach_fixture_role(
                &services,
                root,
                playbit_api::native_app(),
                playbit_api::context::VolumeRole::Session,
            )
            .unwrap()
        })
        .collect();
    for &generation in &generations {
        apps.advance(generation, |_| {}).unwrap();
        settle(&mut apps, generation);
    }
    for (index, &generation) in generations.iter().enumerate() {
        native_input(
            &mut apps,
            generation,
            input(EventKind::TextChanged, 1, format!("Draft {index}").as_bytes()),
        );
    }
    native_input(&mut apps, generations[0], input(EventKind::Click, 2, &[]));
    for _ in 0..16 {
        apps.committed(&changes.drain());
        for &generation in &generations {
            if apps.step(generation, Instant::now()).unwrap().ready {
                apps.advance(generation, |_| {}).unwrap();
            }
            settle(&mut apps, generation);
        }
        if services.store.pending_change_count() == 0 {
            break;
        }
    }
    assert_eq!(services.store.pending_change_count(), 0);
    for (index, &generation) in generations.iter().enumerate() {
        let state = snapshot(apps.scene(generation).unwrap().unwrap());
        assert_eq!(state.rows.len(), 2);
        assert_eq!(state.rows["Draft 0"], CheckState::Off);
        assert_eq!(state.draft, if index == 0 { String::new() } else { format!("Draft {index}") });
        assert!(!apps.step(generation, Instant::now()).unwrap().ready);
    }
    apps.detach(generations[0]).unwrap();
    let replacement = unsafe {
        apps.attach_fixture_role(
            &services,
            root,
            playbit_api::native_app(),
            playbit_api::context::VolumeRole::Session,
        )
        .unwrap()
    };
    apps.advance(replacement, |_| {}).unwrap();
    settle(&mut apps, replacement);
    assert!(snapshot(apps.scene(replacement).unwrap().unwrap()).draft.is_empty());
    assert_eq!(snapshot(apps.scene(generations[1]).unwrap().unwrap()).draft, "Draft 1");
    apps.clear();
}

#[test]
fn surface_changes_keep_drafts_and_data_while_native_and_wasm_layouts_agree() {
    // Switch both actual adapters between compact/wide views and a phone's
    // keyboard-reduced viewport. The same text field, draft and task survive.
    use playbit_api::abi::environment::Surface;
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let native_root = root(&engine.services, 1);
    let wasm_root = root(&engine.services, 1);
    let session = engine.create_session(package(&engine.services), wasm_root).unwrap();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let generation = unsafe {
        apps.attach_fixture_role(
            &engine.services,
            native_root,
            playbit_api::native_app(),
            playbit_api::context::VolumeRole::Session,
        )
    }
    .unwrap();
    apps.advance(generation, |_| {}).unwrap();
    settle(&mut apps, generation);
    let mut owner = WasmOwner::open_with_role(
        &engine,
        &session.id,
        playbit_api::context::VolumeRole::Session as u32,
    );
    let draft = input(EventKind::TextChanged, 1, b"Private draft survives resize");
    native_input(&mut apps, generation, draft.clone());
    owner.input(draft).unwrap();
    let before = snapshot(apps.scene(generation).unwrap().unwrap());
    for (width, height, inputs) in [
        (343, 600, Surface::TOUCH | Surface::KEYBOARD),
        (343, 280, Surface::TOUCH | Surface::KEYBOARD),
        (343, 1, Surface::TOUCH | Surface::KEYBOARD),
        (343, 600, Surface::TOUCH | Surface::KEYBOARD),
        (448, 368, Surface::TOUCH),
        (640, 416, Surface::POINTER | Surface::KEYBOARD),
        (479, 416, Surface::POINTER | Surface::KEYBOARD),
        (480, 416, Surface::POINTER | Surface::KEYBOARD),
        (0, 416, Surface::POINTER | Surface::KEYBOARD),
        (640, 416, Surface::POINTER | Surface::KEYBOARD),
    ] {
        let bytes = Surface { width, height, pixel_scale: 2.0, inputs }.encode().unwrap();
        let event = input(EventKind::SurfaceChanged, 0, &bytes);
        native_input(&mut apps, generation, event.clone());
        owner.input(event).unwrap();
        let native = apps.scene(generation).unwrap().unwrap();
        let wasm = decode_view(owner.scene()).unwrap();
        assert_eq!(snapshot(native), before);
        assert_eq!(snapshot(&wasm), before);
        for scene in [native, &wasm] {
            let scene = match scene {
                ViewNode::Theme { child, .. } => child.as_ref(),
                scene => scene,
            };
            let ViewNode::Column(children) = scene else {
                panic!("TODO root");
            };
            assert_eq!(matches!(children[1], ViewNode::Column(_)), width < 480);
            assert_eq!(matches!(children[1], ViewNode::Row(_)), width >= 480);
        }
    }
    owner.quit();
    apps.clear();
}

#[test]
fn trace_declarations_generate_the_ids_bundled_with_both_execution_modes() {
    mod names {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/playbit/metadata.rs"));
    }
    let schema = playbit::package::Manifest::from_source(include_str!("../playbit.toml"))
        .unwrap()
        .trace
        .unwrap();
    for (id, name) in [
        (names::TODO_TURN, "todo.turn"),
        (names::TODO_INPUT, "todo.input"),
        (names::TODO_REFRESH, "todo.refresh"),
        (names::TODO_ROWS, "todo.rows"),
    ] {
        assert_eq!(schema.names[usize::from(id)].name, name);
    }
}

#[path = "native/performance.rs"]
mod performance;
