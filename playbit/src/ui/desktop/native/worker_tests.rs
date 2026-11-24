//! Exercise the actual worker's close barrier and bounded UI bridge.
#![allow(unsafe_code, clippy::unwrap_used, reason = "Tests own static app callbacks and fixtures.")]
use super::*;
use crate::store::{Access, Id, object_contract::Value};
use std::{
    collections::BTreeMap,
    os::unix::net::UnixListener,
    time::{Duration, Instant},
};
// The SDK's native descriptor resolves this app's registration symbol from
// the linked TODO crate. Keep the provider reachable in this test binary.
#[used]
static TODO_REGISTRATION: fn() -> playbit_api::App = todo::playbit_app;
const TYPE: Id = Id::from_bytes([1; 16]);
const TITLE: Id = Id::from_bytes([2; 16]);
const DONE: Id = Id::from_bytes([3; 16]);

#[test]
fn fatal_output_and_completion_retain_the_failing_worker_call() {
    // Reject an empty launch inside run, before it unwinds to the output sender.
    // Both delivery paths must retain that same stack after the owner exits.
    let directory = tempfile::tempdir().unwrap();
    let mut worker = unsafe { Worker::start(directory.path().into(), |_| Ok(Vec::new())) }.unwrap();
    let output = worker.output();
    let completion = worker.completion();
    let Output::Failed(failure) = receive(&output) else { panic!("fatal startup output") };
    let completed = completion.recv_blocking().unwrap().expect_err("empty launch fails");
    worker.shutdown();
    assert_eq!(failure.details, "A native host must launch between one and four apps");
    let origin = failure.origin.to_string();
    assert!(origin.contains("native::worker::run"), "{origin}");
    assert_eq!(origin, completed.origin.to_string());
}

fn data(services: &Services, volume: Id) -> crate::store::volume_index::EntryAddress {
    let entry = services
        .store
        .resolve_entry(&Access::owner(services.store.account().id), volume, "data")
        .unwrap();
    crate::store::volume_index::EntryAddress { volume, entry: entry.id }
}
fn prepare(services: &Services) -> Result<Vec<Launch>, String> {
    let root = services.store.create_volume("Worker test", false).unwrap().id;
    let access = Access::owner(services.store.account().id);
    let declaration = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap()
    .for_role(crate::package::AutomaticVolumeRole::Session);
    let crate::store::InitialDatabaseKind::ObjectDb { schema, .. } = &declaration.databases[0].kind
    else {
        panic!()
    };
    services
        .store
        .create_database(
            &access,
            root,
            "data",
            crate::store::volume_index::EntryKind::ObjectDb { schema: schema.clone() },
        )
        .unwrap();
    services
        .store
        .create_object_with_fields(
            &access,
            data(services, root),
            TYPE,
            &BTreeMap::from([
                (TITLE, Some(vec![Value::Text("Initial".into())])),
                (DONE, Some(vec![Value::Boolean(false)])),
            ]),
        )
        .unwrap();
    Ok(vec![Launch { title: "TODO".into(), root, app: playbit_api::native_app() }])
}
fn receive(output: &async_channel::Receiver<Output>) -> Output {
    // Test-only observation timeout; production waits on channel readiness.
    let limit = Instant::now() + Duration::from_secs(10);
    loop {
        match output.try_recv() {
            Ok(Output::UiZoom { scale, .. }) => {
                // Startup now restores the local preference before opening a scene.
                assert_eq!(scale, crate::store::profile::UI_ZOOM_ONE);
            }
            Ok(output) => return output.resolve(),
            Err(async_channel::TryRecvError::Empty) if Instant::now() < limit => {
                std::thread::sleep(Duration::from_millis(1))
            }
            _ => panic!("native worker did not deliver its expected result"),
        }
    }
}
fn settle_native(apps: &mut Apps, generation: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while apps.drain_pending(generation).unwrap() {
        assert!(Instant::now() < deadline, "native dependency did not finish");
        if apps.step(generation, Instant::now()).unwrap().ready {
            apps.advance(generation, |_| {}).unwrap();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
fn initialized(output: &async_channel::Receiver<Output>) -> Handle {
    let Output::Opened { handle, .. } = receive(output) else { panic!("opened") };
    loop {
        let Output::Frame { result: Ok(Some(view)), .. } = receive(output) else { continue };
        if format!("{view:?}").contains("Initial") {
            return handle;
        }
    }
}
fn submit(text: &str) -> Input {
    Input { kind: EventKind::Submit, target: 1, data: text.as_bytes().to_vec(), revision: Some(1) }
}
fn rows(path: &std::path::Path) -> usize {
    let services = Services::open(path, || {}).unwrap();
    let root = services.store.volumes().unwrap()[0].id;
    services
        .store
        .query_objects(
            &Access::owner(services.store.account().id),
            data(&services, root),
            TYPE,
            None,
            64,
        )
        .unwrap()
        .objects
        .len()
}
#[test]
#[cfg(feature = "process")]
fn service_guest_owner_delivers_completions_and_drains_without_an_engine_scene_exchange() {
    use crate::engine::{Engine, Request, admission::AdmissionQueue, ipc, reply::Reply};

    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().to_owned();
    let owner_profile = profile.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let engine_stop = stop.clone();
    let (prepared, ready) = mpsc::sync_channel(1);
    let engine = std::thread::spawn(move || {
        let mut engine = Engine::open(&owner_profile).unwrap();
        let root = crate::engine::tests::object_volume(
            &engine.services.store,
            "Guest worker fixture",
            false,
        );
        let wasm = wat::parse_str(r#"(module
            (import "playbit" "pb_syscall" (func $sys (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
            (memory (export "memory") 1)
            (global $began (mut i32) (i32.const 0))
            (global $pending (mut i64) (i64.const 0))
            (data (i32.const 528) "\05\05\05\05\05\05\05\05\05\05\05\05\05\05\05")
            (data (i32.const 4096) "completed")
            (func (export "pb_main")
                (i32.store (i32.const 1024) (i32.const 24))
                (i32.store (i32.const 1028) (i32.const 0))
                (loop $events
                    (if (i64.ne (call $sys (i32.const 4) (i64.const 1024) (i64.const 1056) (i64.const 1024) (i64.const 0) (i64.const 0) (i64.const 0)) (i64.const -6)) (then (br $events))))
                (if (i32.eqz (global.get $began)) (then
                    ;; Enumerate the fixture's root and bundle volumes, including root tags.
                    (i32.store (i32.const 64) (i32.const 64))
                    (i32.store (i32.const 80) (i32.const 1536))
                    (i32.store (i32.const 84) (i32.const 2))
                    (i32.store (i32.const 92) (i32.const 80))
                    (i32.store (i32.const 96) (i32.const 1728))
                    (i32.store (i32.const 100) (i32.const 1))
                    (i32.store (i32.const 108) (i32.const 16))
                    (i32.store (i32.const 112) (i32.const 1792))
                    (i32.store (i32.const 116) (i32.const 128))
                    (i32.store (i32.const 1536) (i32.const 80))
                    (i32.store (i32.const 1616) (i32.const 80))
                    (i32.store (i32.const 1728) (i32.const 16))
                    (if (i64.ne (call $sys (i32.const 7) (i64.const 64)
                        (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)
                        (i64.const 0)) (i64.const 0)) (then unreachable))
                    (if (i32.eqz (i32.and (i32.load (i32.const 1588)) (i32.const 1)))
                        (then unreachable))
                    (i32.store (i32.const 512) (i32.const 32))
                    (i64.store (i32.const 520) (i64.load (i32.const 1544)))
                    (global.set $pending (call $sys (i32.const 258) (i64.const 512)
                        (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
                    (if (i64.le_s (global.get $pending) (i64.const 0)) (then unreachable))
                    (global.set $began (i32.const 1))
                    return))
                (i32.store (i32.const 256) (i32.const 52))
                (i32.store (i32.const 276) (i32.const 48))
                (i32.store (i32.const 292) (i32.const 56))
                (if (i64.eq (call $sys (i32.const 259) (global.get $pending) (i64.const 256) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)) (i64.const 0)) (then
                    (drop (call $sys (i32.const 2) (i64.const 4096) (i64.const 9) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))))"#).unwrap();
        let package = crate::package::Package::new(
            "test".into(),
            "Guest worker".into(),
            wasm,
            BTreeMap::new(),
        )
        .unwrap();
        let session = engine.create_session(package, root.id).unwrap();
        let listener = UnixListener::bind(owner_profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let mut queue = AdmissionQueue::new(listener, &engine).unwrap();
        prepared.send(session.id).unwrap();
        while !engine_stop.load(Ordering::Acquire) {
            // A bounded test observation deadline permits the fixture to retire.
            queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(50))).unwrap();
        }
    });
    let session = ready.recv_timeout(Duration::from_secs(5)).unwrap();

    let Reply::InstanceStatus { incarnation, .. } =
        ipc::call_archive(&profile, &Request::InstanceInspect { request: None })
            .unwrap()
            .into_owned()
            .unwrap()
    else {
        panic!()
    };
    let Reply::Instance(instance) = ipc::call_archive(
        &profile,
        &Request::InstanceReserve {
            restore_host_mounts: false,
            request: Id::generate().unwrap(),
            incarnation,
            session,
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
        },
    )
    .unwrap()
    .into_owned()
    .unwrap() else {
        panic!()
    };
    let mut worker = Worker::start_guest(profile, PathBuf::from("/nonexistent"), None, move || {
        Ok(Some(instance))
    })
    .unwrap();
    let output = worker.output();
    let completion = worker.completion();
    let Output::Opened { handle, title, .. } = receive(&output) else { panic!() };
    assert_eq!(title, "Guest worker");
    handle.close();
    worker.request_stop();
    let mut completed = false;
    loop {
        match receive(&output) {
            Output::Frame { logs, result, .. } => {
                assert!(result.is_ok());
                completed |= logs.iter().any(|line| line == "completed");
            }
            Output::Closed(generation) => {
                assert_eq!(generation, handle.generation);
                break;
            }
            _ => panic!("Unexpected service guest output"),
        }
    }
    assert!(completed);
    completion.recv_blocking().unwrap().unwrap();
    worker.shutdown();
    stop.store(true, Ordering::Release);
    engine.join().unwrap();
}

#[test]
#[cfg(feature = "process")]
fn automatic_engine_restart_and_revocation_preserve_owner_admission() {
    use crate::engine::{Engine, Request, admission::AdmissionQueue, ipc, reply::Reply};
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().to_owned();
    let mut view_state = playbit_api::context::ContextState::default();
    let mut view = view_state.view();
    view.column_start();
    view.button(1, "Press", playbit_api::ui::ButtonStyle::Default, false);
    view.column_end();
    let view = view.finish();
    let bytes: String = view.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let wasm = wat::parse_str(format!(r#"(module
        (import "playbit" "pb_syscall" (func $sys (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "{bytes}")
        (data (i32.const 8) "clicked")
        (func (export "pb_main")
            (i32.store (i32.const 8192) (i32.const 24))
            (i32.store (i32.const 8196) (i32.const 0))
            (loop $events
                (if (i64.eq (call $sys (i32.const 4) (i64.const 8192) (i64.const 8224) (i64.const 1024) (i64.const 0) (i64.const 0) (i64.const 0)) (i64.const -6)) (then return))
                (if (i32.eq (i32.load offset=16 (i32.const 8192)) (i32.const 2)) (then
                    (drop (call $sys (i32.const 2) (i64.const 8) (i64.const 7) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))
                (drop (call $sys (i32.const 3) (i64.const 1024) (i64.const {length}) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
                (br $events))))"#, length=view.len())).unwrap();
    let package = crate::package::Package::new(
        "test".into(),
        "Recovery fixture".into(),
        wasm,
        BTreeMap::new(),
    )
    .unwrap();
    let start = |package: Option<crate::package::Package>| {
        let owner_profile = profile.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let engine_stop = stop.clone();
        let (prepared, ready) = mpsc::sync_channel(1);
        let (deny, denials) = mpsc::sync_channel::<String>(1);
        let engine = std::thread::spawn(move || {
            let mut engine = Engine::open(&owner_profile).unwrap();
            let session = package.map(|package| {
                let root = engine.services.store.create_volume("Recovery fixture", false).unwrap();
                engine.create_session(package, root.id).unwrap().id
            });
            let listener =
                UnixListener::bind(owner_profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
            let mut queue = AdmissionQueue::new(listener, &engine).unwrap();
            prepared.send(session).unwrap();
            while !engine_stop.load(Ordering::Acquire) {
                if let Ok(id) = denials.try_recv() {
                    let session = engine.services.store.session(&id).unwrap();
                    engine
                        .services
                        .store
                        .record_session_denial(&session, "fixture revocation")
                        .unwrap();
                }
                queue
                    .run_until(&mut engine, Some(Instant::now() + Duration::from_millis(50)))
                    .unwrap();
            }
        });
        (stop, engine, ready.recv_timeout(Duration::from_secs(5)).unwrap(), deny)
    };
    let (stop, engine, session, _) = start(Some(package));
    let session = session.unwrap();
    let Reply::InstanceStatus { incarnation, .. } =
        ipc::call_archive(&profile, &Request::InstanceInspect { request: None })
            .unwrap()
            .into_owned()
            .unwrap()
    else {
        panic!()
    };
    let Reply::Instance(instance) = ipc::call_archive(
        &profile,
        &Request::InstanceReserve {
            restore_host_mounts: false,
            request: Id::generate().unwrap(),
            incarnation,
            session: session.clone(),
            extra_volumes: Box::default(),
            host_mounts: Box::default(),
        },
    )
    .unwrap()
    .into_owned()
    .unwrap() else {
        panic!()
    };
    // Hold launch admission while the fixture endpoint is absent. Recovery
    // must wait rather than executing this test harness as a helper process.
    let lock_path = crate::paths::Paths::for_profile(&profile).unwrap().launch_lock;
    let launch_lock = std::fs::File::create(&lock_path).unwrap();
    launch_lock.try_lock().unwrap();
    let mut worker =
        Worker::start_guest(profile.clone(), std::env::current_exe().unwrap(), None, move || {
            Ok(Some(instance))
        })
        .unwrap();
    let output = worker.output();
    let done = worker.completion();
    let Output::Opened { handle, .. } = receive(&output) else { panic!() };
    let click = || Input { kind: EventKind::Click, target: 1, data: vec![], revision: None };
    assert!(handle.send(click(), None));
    // Event headers start with the generation; kind is at byte eight. Prove
    // this fixture recognizes a real click before checking stale-input loss.
    loop {
        let Output::Frame { logs, .. } = receive(&output) else {
            panic!("missing initial click response");
        };
        if logs.iter().any(|line| line == "clicked") {
            break;
        }
    }
    assert!(handle.send(click(), None));
    let deadline = Instant::now() + Duration::from_secs(5);
    while output.is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    // The one-slot output blocks subsequent turns, leaving an accepted queue.
    let mut accepted = 0;
    for _ in 0..MAX_INPUTS {
        if handle.send(click(), None) {
            accepted += 1;
        }
    }
    assert!(accepted > 0);
    stop.store(true, Ordering::Release);
    engine.join().unwrap();
    let active = loop {
        match receive(&output) {
            Output::Recovering { generation, active, .. } => {
                assert_eq!(generation, handle.generation);
                break active;
            }
            Output::Frame { .. } => (),
            _ => panic!("the surviving owner must begin automatic recovery"),
        }
    };
    assert!(done.is_empty(), "a recoverable worker retains its termination barrier");
    assert!(!handle.send(click(), None));
    std::fs::remove_file(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
    let (stop, engine, _, deny) = start(None);
    let Output::Opened { replaces, handle: fresh, logs, .. } = receive(&output) else {
        panic!("missing reconstructed app");
    };
    assert_eq!(replaces, Some(handle.generation));
    assert_ne!(fresh.generation, handle.generation);
    assert!(!logs.iter().any(|line| line == "clicked"));
    assert!(!active.load(Ordering::Acquire));
    // Keep the reconstructed owner on this engine: a second GUI owner in the
    // same process would correctly violate the one-instance admission contract.
    deny.send(session).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = None;
    let mut failure = None;
    let mut recovery = None;
    let mut terminal = None;
    while Instant::now() < deadline {
        while let Ok(output) = output.try_recv() {
            match output {
                Output::Frame { logs, .. } => assert!(
                    !logs.iter().any(|line| line == "clicked"),
                    "old queued clicks must never reach the fresh VM"
                ),
                Output::Recovering { active, .. } => recovery = Some(active),
                Output::ContextUnavailable(generation) => terminal = Some(generation),
                Output::Failed(error) => failure = Some(error),
                _ => (),
            }
        }
        if completed.is_none()
            && let Ok(result) = done.try_recv()
        {
            completed = Some(result);
        }
        if completed.is_some() && terminal.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    worker.request_stop();
    worker.shutdown();
    stop.store(true, Ordering::Release);
    engine.join().unwrap();
    drop(launch_lock);
    std::fs::remove_file(lock_path).unwrap();
    assert!(completed.is_some(), "revoked owner must retire on its live engine");
    assert!(completed.unwrap().is_ok(), "revocation is not a fatal worker failure: {failure:?}");
    assert!(failure.is_none(), "revocation must not emit global failure");
    assert_eq!(terminal, Some(fresh.generation), "completion belongs to the stopped owner");
    assert!(recovery.is_none_or(|active| !active.load(Ordering::Acquire)));
    assert!(!fresh.send(click(), None));
}

#[test]
fn submit_during_a_refresh_starts_its_mutation_before_deactivation() {
    let directory = tempfile::tempdir().unwrap();
    let services = Services::open(directory.path(), || {}).unwrap();
    let launch = prepare(&services).unwrap().remove(0);
    let mut apps = Apps::new(Arc::new(|| {}));
    let generation = unsafe { apps.attach(&services, launch.root, launch.app) }.unwrap();
    apps.advance(generation, |_| {}).unwrap();
    settle_native(&mut apps, generation); // Resolve the stable root entry first.
    // Start a refresh, but do not deliver its completion. Submit must still
    // begin its mutation while the query is outstanding.
    let access = Access::owner(services.store.account().id);
    let object = services
        .store
        .query_objects(&access, data(&services, launch.root), TYPE, None, 64)
        .unwrap()
        .objects[0]
        .object_id;
    services
        .store
        .edit_fields(
            &access,
            data(&services, launch.root),
            object,
            &BTreeMap::from([(TITLE, Some(vec![Value::Text("Refreshed".into())]))]),
        )
        .unwrap();
    apps.advance(generation, |_| {}).unwrap();
    let input = submit("Saved while loading");
    apps.input(generation, Event { kind: input.kind, target: input.target, data: input.data })
        .unwrap();
    apps.deactivate(generation).unwrap();
    let mut logs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while apps.drain_pending(generation).unwrap() {
        assert!(Instant::now() < deadline);
        if apps.step(generation, Instant::now()).unwrap().ready {
            apps.advance(generation, |frame| logs.extend_from_slice(frame.logs())).unwrap();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(logs.iter().filter(|line| *line == "TODO added").count(), 1);
    assert!(!logs.iter().any(|line| line.contains("Conflict")));
    apps.detach(generation).unwrap();
    drop(apps);
    drop(services);
    assert_eq!(rows(directory.path()), 2);
}
#[test]
fn checkbox_during_a_refresh_keeps_its_known_edit_through_quit() {
    for quit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let services = Services::open(directory.path(), || {}).unwrap();
        let launch = prepare(&services).unwrap().remove(0);
        let access = Access::owner(services.store.account().id);
        let mut apps = Apps::new(Arc::new(|| {}));
        let generation = unsafe { apps.attach(&services, launch.root, launch.app) }.unwrap();
        apps.advance(generation, |_| {}).unwrap();
        settle_native(&mut apps, generation);
        let object = services
            .store
            .query_objects(&access, data(&services, launch.root), TYPE, None, 64)
            .unwrap()
            .objects[0]
            .object_id;
        services
            .store
            .edit_fields(
                &access,
                data(&services, launch.root),
                object,
                &BTreeMap::from([(
                    TITLE,
                    Some(vec![Value::Text("Changed during refresh".into())]),
                )]),
            )
            .unwrap();
        // This turn starts the actual query but does not deliver its completion.
        // Validate the click against the retained scene before deactivating.
        let before = format!("{:?}", apps.scene(generation).unwrap());
        apps.advance(generation, |_| {}).unwrap();
        assert!(apps.drain_pending(generation).unwrap());
        assert_eq!(format!("{:?}", apps.scene(generation).unwrap()), before);
        apps.input(generation, Event { kind: EventKind::Click, target: 100, data: vec![] })
            .unwrap();
        if quit {
            apps.deactivate(generation).unwrap();
        }
        let mut logs = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while apps.drain_pending(generation).unwrap() {
            assert!(Instant::now() < deadline);
            if apps.step(generation, Instant::now()).unwrap().ready {
                apps.advance(generation, |frame| logs.extend_from_slice(frame.logs())).unwrap();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(logs.iter().filter(|line| *line == "TODO changed").count(), 1);
        assert!(!logs.iter().any(|line| line.contains("Conflict")), "{logs:?}");
        if !quit {
            assert!(!logs.iter().any(|line| line.contains("Operation failed")), "{logs:?}");
        }
        let record =
            services.store.read_object(&access, data(&services, launch.root), object).unwrap();
        assert_eq!(record.fields[&DONE].values, Some(vec![Value::Boolean(true)]));
        assert_eq!(
            record.fields[&TITLE].values,
            Some(vec![Value::Text("Changed during refresh".into())])
        );
        if !quit {
            assert!(format!("{:?}", apps.scene(generation).unwrap()).contains("state: On"));
            apps.deactivate(generation).unwrap();
        }
        apps.detach(generation).unwrap();
        drop(apps);
        drop(services);
        let reopened = Services::open(directory.path(), || {}).unwrap();
        let record =
            reopened.store.read_object(&access, data(&reopened, launch.root), object).unwrap();
        assert_eq!(record.fields[&DONE].values, Some(vec![Value::Boolean(true)]));
    }
}

#[test]
fn shutdown_drains_admitted_edits_even_with_full_ui_output() {
    // Stop reading UI output, enqueue edits, then join. Closing the output channel
    // must unblock the owner without dropping already admitted durable effects.
    let directory = tempfile::tempdir().unwrap();
    let mut worker = unsafe { Worker::start(directory.path().into(), prepare) }.unwrap();
    let output = worker.output();
    let handle = initialized(&output);
    // Leave output undrained while accepted local typing precedes one save.
    for index in 0..7 {
        let mut input = submit(&format!("Queued {index}"));
        input.kind = EventKind::TextChanged;
        input.data = playbit_api::abi::text::TextInput::encode(
            core::str::from_utf8(&input.data).expect("fixture text"),
            &[],
        )
        .expect("fixture edit");
        assert!(handle.send(input, None));
    }
    assert!(handle.send(submit("Saved before Quit"), None));
    worker.shutdown();
    assert!(!handle.send(submit("After close"), None));
    assert_eq!(rows(directory.path()), 2);
    assert!(!directory.path().join(crate::paths::ENGINE_SOCKET).exists());
    worker.shutdown(); // Idempotent; no second callback or join.
}
#[test]
fn rejected_close_drains_another_windows_accepted_edit_before_returning_failure() {
    // Drive two actual TODO instances after queuing their input. The rejected
    // close occurs while the sibling save is still behind its typing event.
    let directory = tempfile::tempdir().unwrap();
    let services = Services::open(directory.path(), || {}).unwrap();
    let mut launches = prepare(&services).unwrap();
    launches.push(Launch {
        title: "Second window".into(),
        root: launches[0].root,
        app: playbit_api::native_app(),
    });
    let (wake, wake_events) = mpsc::sync_channel(1);
    let storage_wake = wake.clone();
    let mut apps = Apps::new(Arc::new(move || {
        let _ = storage_wake.try_send(());
    }));
    let (output, events) = async_channel::bounded(OUTPUT_CAPACITY);
    let slots = unsafe { attach(&mut apps, &services, launches, &wake, &output) }.unwrap();
    let failed = slots[0].handle.clone();
    let sibling = slots[1].handle.clone();
    settle_native(&mut apps, failed.generation);
    settle_native(&mut apps, sibling.generation);
    assert!(
        failed.send(
            Input { kind: EventKind::Click, target: 999, data: vec![], revision: None },
            None
        )
    );
    failed.close();
    let mut typing = submit("Saved by sibling");
    typing.kind = EventKind::TextChanged;
    typing.data = playbit_api::abi::text::TextInput::encode("Saved by sibling", &[]).unwrap();
    assert!(sibling.send(typing, None));
    assert!(sibling.send(submit("Saved by sibling"), None));

    let error =
        drive(&mut apps, slots, wake_events, &output, &AtomicBool::new(false), |_| Ok(false))
            .unwrap_err();
    assert!(error.details.contains("InvalidArgument"), "{}", error.details);
    let mut closed = Vec::new();
    while let Ok(output) = events.try_recv() {
        if let Output::Closed(generation) = output.resolve() {
            closed.push(generation);
        }
    }
    assert_eq!(closed, [sibling.generation], "only the successful window confirmed its close");
    assert!(!sibling.send(submit("After fatal close"), None));
    drop(apps);
    drop(services);
    assert_eq!(rows(directory.path()), 2);
}

#[test]
fn window_close_rejects_new_input_and_confirms_after_durable_drain() {
    // The close barrier is ordered after admitted input, including input whose
    // UI response is still queued. It closes every sender clone atomically.
    let directory = tempfile::tempdir().unwrap();
    let mut worker = unsafe { Worker::start(directory.path().into(), prepare) }.unwrap();
    let output = worker.output();
    let handle = initialized(&output);
    let cloned = handle.clone();
    assert!(handle.send(submit("Before window close"), None));
    handle.close();
    assert!(!cloned.send(submit("After window close"), None));
    loop {
        match receive(&output) {
            Output::Closed(generation) => {
                assert_eq!(generation, handle.generation);
                break;
            }
            Output::Frame { result, .. } => {
                assert!(result.is_ok());
            }
            Output::Failed(error) => panic!("native worker failed during close: {error}"),
            _ => panic!("unexpected worker output"),
        }
    }
    worker.shutdown();
    assert_eq!(rows(directory.path()), 2);
}
#[test]
fn admission_is_bounded_and_close_keeps_the_accepted_queue() {
    use playbit_api::abi::{
        syscall::MAX_EVENT_BYTES,
        text::{MAX_TEXT_BYTES, MAX_TEXT_EDITS, TextEdit, TextInput},
    };
    // Exercise queue admission without a consumer so capacity and payload limits
    // are deterministic; close retains accepted work and prohibits all later work.
    let (inputs, receive) = async_channel::bounded(MAX_INPUTS);
    let (wake, _) = mpsc::sync_channel(1);
    let (errors, error_receive) = async_channel::bounded(1);
    let handle = Handle {
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        audio_instance: None,
        session: None,
        generation: 1,
        #[cfg(all(target_os = "macos", feature = "process"))]
        startup: None,
        inputs,
        wake,
        failures: Arc::new(AtomicU64::new(0)),
        pending: Arc::new(AtomicUsize::new(0)),
        errors,
    };
    assert!(!handle.send(submit(&"x".repeat(MAX_TEXT_BYTES + 1)), None));
    assert!(receive.is_empty());
    let maximum = "x".repeat(MAX_TEXT_BYTES);
    assert!(handle.send(submit(&maximum), None));
    assert_eq!(receive.try_recv().unwrap().input.data.len(), MAX_TEXT_BYTES);
    // Native edits may include their full metadata budget in addition to the text.
    let mut changed = submit(&maximum);
    changed.kind = EventKind::TextChanged;
    changed.data = TextInput::encode(
        &maximum,
        &[TextEdit { start: 0, end: 0, inserted_len: 1 }; MAX_TEXT_EDITS],
    )
    .unwrap();
    assert_eq!(changed.data.len(), MAX_EVENT_BYTES);
    assert!(handle.send(changed, None));
    let mut changed = receive.try_recv().unwrap().input;
    assert_eq!(changed.data.len(), MAX_EVENT_BYTES);
    changed.data.push(b'x');
    assert!(!handle.send(changed, None));
    assert!(receive.is_empty());
    for _ in 0..MAX_INPUTS {
        assert!(handle.send(submit("Accepted"), None));
    }
    assert!(!handle.send(submit("Over capacity"), None));
    assert_eq!(error_receive.len(), 1, "one retained actionable admission error");
    handle.close();
    assert!(!handle.send(submit("Closed"), None));
    for _ in 0..MAX_INPUTS {
        assert_eq!(receive.try_recv().unwrap().input.data, b"Accepted");
    }
    assert!(receive.is_closed());
    assert!(receive.is_empty());
}

#[test]
fn display_callback_after_owner_retirement_does_not_report_a_failed_action() {
    // The worker retires its receiver before the GUI has adopted Recovering.
    let (mut handle, inputs) = Handle::fixture(1);
    let (errors, notices) = async_channel::bounded(1);
    handle.errors = errors;
    inputs.close();
    let frame = Input {
        kind: EventKind::Frame,
        target: 1,
        data: 16667u32.to_le_bytes().to_vec(),
        revision: None,
    };
    assert!(!handle.send(frame, None));
    assert!(notices.is_empty());
    assert!(!handle.send(submit("User action"), None));
    assert_eq!(notices.try_recv().unwrap(), ADMISSION_ERROR);
}

#[test]
fn display_input_waits_until_previous_input_response_is_adopted() {
    // The real native worker completes a TODO edit. Keeping that response on
    // the GUI side must still prevent an obsolete frame from entering the queue.
    let directory = tempfile::tempdir().unwrap();
    let mut worker = unsafe { Worker::start(directory.path().into(), prepare) }.unwrap();
    let output = worker.output();
    // Opened precedes the named-entry resolution and initial query. Input
    // must target the actual initialized TODO scene, as the GUI does.
    let handle = initialized(&output);
    let frame = || Input {
        kind: EventKind::Frame,
        target: 99,
        data: 16667u32.to_le_bytes().to_vec(),
        revision: None,
    };
    let surface = || Input {
        kind: EventKind::SurfaceChanged,
        target: 0,
        data: playbit_api::abi::environment::Surface {
            width: 640,
            height: 480,
            pixel_scale: 2.0,
            inputs: 3,
        }
        .encode()
        .unwrap()
        .to_vec(),
        revision: None,
    };
    assert!(handle.send(submit("Updated"), None));
    assert!(!handle.send(frame(), None), "no frame queued behind ordinary input");
    for _ in 0..MAX_INPUTS * 2 {
        assert!(
            !handle.send_scene_input(surface()),
            "resize waits without filling the action queue"
        );
    }
    assert!(handle.errors.is_empty(), "deferred geometry is not a failed action");
    let response = (0..32)
        .find_map(|_| {
            let response = receive(&output);
            match &response {
                Output::Frame { ack: InputAck { target: 1, .. }, .. } => Some(response),
                Output::Frame { result: Ok(_), .. } => None,
                _ => panic!("unexpected native output"),
            }
        })
        .expect("submission response");
    assert!(matches!(&response, Output::Frame { result: Ok(_), .. }));
    assert!(handle.input_pending(), "queue drain is not GUI acknowledgment");
    assert!(!handle.send(frame(), None));
    assert!(!handle.send_scene_input(surface()));
    drop(response);
    assert!(!handle.input_pending());
    assert!(handle.send_scene_input(surface()), "layout can retry after GUI adoption");
    worker.shutdown();
    assert_eq!(rows(directory.path()), 2);
}

#[test]
fn slow_ui_does_not_stop_native_input_execution_and_receipts_remain_ordered() {
    let directory = tempfile::tempdir().unwrap();
    let mut worker = unsafe { Worker::start(directory.path().into(), prepare) }.unwrap();
    let output = worker.output();
    let handle = initialized(&output);
    for n in 1..=MAX_INPUTS {
        assert!(
            handle.send(
                Input {
                    kind: EventKind::TextChanged,
                    target: 1,
                    data: playbit_api::abi::text::TextInput::encode(&format!("draft-{n}"), &[])
                        .expect("fixture edit"),
                    revision: Some(n as u64),
                },
                None
            )
        );
    }
    // Do not consume UI output until every queued input reaches the worker.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.inputs.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let drained = handle.inputs.is_empty();
    if !drained {
        worker.shutdown();
    }
    assert!(drained, "worker must not wait for the UI to consume a prior response");
    assert!(!handle.send(submit("Over capacity"), None));
    for n in 1..=MAX_INPUTS {
        loop {
            match receive(&output) {
                Output::Frame { ack, result: Ok(view), .. } if ack.target == 1 => {
                    assert_eq!(ack.revision, n as u64);
                    assert!(format!("{view:?}").contains(&format!("draft-{n}")));
                    break;
                }
                Output::Frame { result: Ok(_), .. } => {}
                _ => panic!("unexpected native output"),
            }
        }
    }
    assert!(!handle.input_pending());
    worker.shutdown();
}

#[test]
fn a_missing_owner_scene_keeps_its_menu_input_for_normal_error_handling() {
    let (handle, queue) = Handle::fixture(1);
    let action = Input { kind: EventKind::Click, target: 7, data: Vec::new(), revision: None };
    assert!(handle.send_menu(action, 0, crate::ui::desktop::popup::menu_digest(&[])));
    let message =
        queue.try_recv().unwrap().if_current(|| None).expect("only a known newer scene cancels");
    assert_eq!(message.input.target, 7);
    drop(message);
    assert!(!handle.input_pending());
}

#[test]
fn failed_input_invalidates_already_queued_effects_without_replay() {
    // Force an invalid input before a queued submission. It must invalidate that
    // submission's epoch; only a later explicit action may create the object.
    let directory = tempfile::tempdir().unwrap();
    let services = Services::open(directory.path(), || {}).unwrap();
    let launches = prepare(&services).unwrap();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let (wake, wake_events) = mpsc::sync_channel(1);
    let (output, receive) = async_channel::bounded(1);
    let slots = unsafe { attach(&mut apps, &services, launches, &wake, &output) }.unwrap();
    assert!(matches!(receive.try_recv().unwrap(), Output::Opened { .. }));
    let slot = &slots[0];
    settle_native(&mut apps, slot.handle.generation);
    let invalid = Input { kind: EventKind::Click, target: 999, data: vec![], revision: None };
    for input in [invalid, submit("Must not be replayed")] {
        advance(
            &mut apps,
            slot,
            Some(Message {
                pending: PendingInput::new(&slot.handle.pending),
                input,
                epoch: 0,
                source: InputSource::Window,
            }),
            &output,
        );
        assert!(matches!(receive.try_recv().unwrap(), Output::Frame { result: Err(_), .. }));
    }
    let root = services.store.volumes().unwrap()[0].id;
    let access = Access::owner(services.store.account().id);
    assert_eq!(
        services
            .store
            .query_objects(&access, data(&services, root), TYPE, None, 64)
            .unwrap()
            .objects
            .len(),
        1
    );
    advance(
        &mut apps,
        slot,
        Some(Message {
            pending: PendingInput::new(&slot.handle.pending),
            input: submit("Explicit retry"),
            epoch: slot.handle.epoch(),
            source: InputSource::Window,
        }),
        &output,
    );
    assert!(matches!(receive.try_recv().unwrap(), Output::Frame { result: Ok(_), .. }));
    settle_native(&mut apps, slot.handle.generation);
    assert_eq!(
        services
            .store
            .query_objects(&access, data(&services, root), TYPE, None, 64)
            .unwrap()
            .objects
            .len(),
        2
    );
    // Closing also drains an obsolete menu choice queued before that failure.
    // Its admission credit is released even though no window takes a receipt.
    let count = slot.handle.pending.clone();
    let digest = crate::ui::desktop::popup::menu_digest(&[]);
    let selection = Input { kind: EventKind::Click, target: 999, data: Vec::new(), revision: None };
    assert!(slot.handle.send_menu(selection, 0, digest));
    slot.handle.close();
    let (closing, closed) = async_channel::bounded(OUTPUT_CAPACITY);
    drive(&mut apps, slots, wake_events, &closing, &AtomicBool::new(false), |_| Ok(false)).unwrap();
    assert_eq!(count.load(Ordering::Acquire), 0);
    let mut confirmed = false;
    while let Ok(output) = closed.try_recv() {
        match output.resolve() {
            Output::Closed(_) => confirmed = true,
            Output::Frame { result: Ok(_), pending: None, .. } => {}
            _ => panic!("unexpected close output"),
        }
    }
    assert!(confirmed);
}

#[cfg(feature = "connected-engine")]
#[test]
fn connected_owner_preserves_local_input_and_shutdown_when_ap_is_unavailable() {
    // Use the real connected composition without enrollment. Network bootstrap
    // must fail visibly while native edits still commit through session authority.
    let directory = tempfile::tempdir().unwrap();
    let mut worker = unsafe {
        Worker::start_connected(directory.path().into(), |connected| {
            let services = &connected.services;
            let launch = prepare(services)?.remove(0);
            let package = crate::package::Package::new(
                "test".into(),
                "Connected TODO".into(),
                b"\0asm\x01\0\0\0".to_vec(),
                BTreeMap::new(),
            )
            .unwrap();
            let access = Access::owner(services.store.account().id);
            services.store.provision_version(&access, launch.root, &package.id().unwrap()).unwrap();
            let session = services.create_session(package.clone(), launch.root).unwrap();
            Ok(vec![super::super::SessionLaunch { session: session.id, package, app: launch.app }])
        })
    }
    .unwrap();
    let output = worker.output();
    let Output::Opened { handle, view, .. } = receive(&output) else { panic!() };
    assert!(view.is_some());
    // Consume exact completion readiness before submitting through an enabled
    // control. Observe network failure independently of local initialization.
    let mut saw_error = false;
    loop {
        match receive(&output) {
            Output::Connection(status) if status.message != "Connecting…" => saw_error = true,
            Output::Frame { result: Ok(Some(view)), .. }
                if format!("{view:?}").contains("Initial") =>
            {
                break;
            }
            Output::Frame { result, .. } => assert!(result.is_ok()),
            Output::Connection(_) => {}
            _ => panic!("unexpected setup output"),
        }
    }
    assert!(handle.send(submit("Offline native edit"), None));
    let mut saw_frame = false;
    while !saw_error || !saw_frame {
        match receive(&output) {
            Output::Connection(status) => {
                if status.message != "Connecting…" {
                    assert!(status.message.contains("Sync unavailable"));
                    assert!(status.retry.is_none(), "unenrolled fixture cannot retry sign-in");
                    saw_error = true;
                }
            }
            Output::Frame { result, .. } => {
                assert!(result.is_ok());
                saw_frame = true;
            }
            _ => panic!("unexpected connected output"),
        }
    }
    worker.shutdown();
    let services = Services::open(directory.path(), || {}).unwrap();
    let session = services.store.sessions().unwrap().remove(0);
    assert_eq!(
        services
            .store
            .query_objects(
                &Access::owner(services.store.account().id),
                data(&services, session.root_volume),
                TYPE,
                None,
                64,
            )
            .unwrap()
            .objects
            .len(),
        2
    );
    assert!(!directory.path().join(crate::paths::ENGINE_SOCKET).exists());
}

#[cfg(feature = "connected-engine")]
#[test]
fn setup_close_and_quit_release_waiting_preparation() {
    // Both closing the setup window and quitting during an unanswered prompt
    // must wake the network preparation owner and join without opening an app.
    for quit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut worker = unsafe {
            Worker::start_with_setup(directory.path().into(), |_, _, ui| {
                ui.ask(setup::Prompt {
                    id: 100,
                    title: "Sign in",
                    explanation: "Fixture",
                    field: Some(("", "Email")),
                    action: "Continue",
                    secondary: None,
                    error: "",
                })?;
                Err("Fixture must never pass setup".into())
            })
        }
        .unwrap();
        let output = worker.output();
        let Output::Opened { handle, .. } = receive(&output) else { panic!() };
        assert_eq!(handle.generation, 0);
        if !quit {
            handle.close();
            assert!(matches!(receive(&output), Output::Closed(0)));
        }
        worker.shutdown();
        assert!(
            Services::open(directory.path(), || {}).unwrap().store.sessions().unwrap().is_empty()
        );
    }
}

#[cfg(feature = "connected-engine")]
#[test]
fn setup_acknowledges_drafts_and_rejects_stale_actions() {
    // A second click queued for the first page must not submit the second page.
    // Oversized text reports an editable error instead of terminating setup.
    let directory = tempfile::tempdir().unwrap();
    let (answers, received) = mpsc::sync_channel(1);
    let mut worker = unsafe {
        Worker::start_with_setup(directory.path().into(), move |_, _, ui| {
            let first = ui.ask(setup::Prompt {
                id: 100,
                title: "Email",
                explanation: "Fixture",
                field: Some(("", "Email")),
                action: "Continue",
                secondary: None,
                error: "",
            })?;
            let second = ui.ask(setup::Prompt {
                id: 200,
                title: "Code",
                explanation: "Fixture",
                field: Some(("", "Code")),
                action: "Continue",
                secondary: None,
                error: "",
            })?;
            answers.send((first.value, second.value)).unwrap();
            Err("Fixture completed".into())
        })
    }
    .unwrap();
    let output = worker.output();
    let Output::Opened { handle, .. } = receive(&output) else { panic!() };
    let epoch = handle.epoch();
    assert!(
        handle.send(
            Input {
                kind: EventKind::TextChanged,
                target: 100,
                data: playbit_api::abi::text::TextInput::encode(&"x".repeat(4097), &[])
                    .expect("fixture edit"),
                revision: Some(1)
            },
            Some(epoch)
        )
    );
    assert!(matches!(receive(&output), Output::Frame { result: Ok(_), .. }));
    assert!(
        handle.send(
            Input {
                kind: EventKind::TextChanged,
                target: 100,
                data: playbit_api::abi::text::TextInput::encode("test@example.invalid", &[])
                    .expect("fixture edit"),
                revision: Some(2)
            },
            Some(epoch)
        )
    );
    assert!(matches!(
        receive(&output),
        Output::Frame { ack: InputAck { target: 100, revision: 2, .. }, result: Ok(_), .. }
    ));
    for _ in 0..2 {
        assert!(handle.send(
            Input { kind: EventKind::Click, target: 101, data: vec![], revision: None },
            Some(epoch)
        ));
    }
    assert!(matches!(receive(&output), Output::Frame { result: Ok(_), .. }));
    assert!(received.try_recv().is_err());
    assert!(handle.send(
        Input { kind: EventKind::Submit, target: 200, data: b"123456".to_vec(), revision: Some(1) },
        None
    ));
    let values = received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(values, ("test@example.invalid".into(), "123456".into()));
    worker.shutdown();
}

#[path = "frame_input_tests.rs"]
mod frame_input_tests;
#[cfg(feature = "process")]
#[test]
fn tool_pointer_release_uses_the_target_canvas_size_at_execution() {
    use crate::runtime::syscall::{HostError, validate_input};
    use playbit_api::abi::syscall::PointerInput;
    let (handle, queue) = Handle::fixture(7);
    let canvas = |id, width, height| ViewNode::Canvas {
        id,
        width,
        height,
        frames: playbit_api::ui::CanvasFrames::Static,
        extent: playbit_api::ui::CanvasExtent::Inline,
        clear_color: None,
        items: vec![],
    };
    let scene = |width, height| {
        ViewNode::Row(vec![canvas(1, 300, 200), ViewNode::Column(vec![canvas(7, width, height)])])
    };
    let press = PointerInput { x: 90, y: 50, phase: 1, buttons: 1 };
    let release = PointerInput { phase: 2, buttons: 0, ..press };
    let input = |target, pointer: PointerInput| Input {
        kind: EventKind::Pointer,
        target,
        data: pointer.encode().into(),
        revision: None,
    };
    let execute = |scene: &ViewNode| {
        let message = queue.try_recv().unwrap();
        message.source.event(message.input, || Some(scene))
    };
    let old = scene(100, 60);
    assert!(handle.send_tool_input(input(7, press)));
    let event = execute(&old);
    assert_eq!(PointerInput::decode(&event.data).unwrap(), press);
    validate_input(&old, &event).unwrap();
    assert!(handle.send_tool_input(input(7, release)));
    let current = scene(64, 32);
    let event = execute(&current);
    assert_eq!(
        PointerInput::decode(&event.data).unwrap(),
        PointerInput { x: 64, y: 32, ..release }
    );
    validate_input(&current, &event).unwrap();
    assert!(handle.send(input(7, release), None));
    let physical = execute(&current);
    assert_eq!(PointerInput::decode(&physical.data).unwrap(), release);
    assert_eq!(validate_input(&current, &physical), Err(HostError::InvalidArgument));
    assert!(handle.send_tool_input(input(8, release)));
    assert_eq!(validate_input(&current, &execute(&current)), Err(HostError::InvalidArgument));
}

#[cfg(feature = "process")]
#[test]
fn gui_owner_controls_bound_identity_logs_input_and_quit_without_replay() {
    use super::super::control::Server;
    use crate::{
        engine::session_instance::{LaunchOwner, SessionInstance, State},
        ui::app_control::{self, Exchange, Operation, Reply},
    };
    use std::{net::Shutdown, os::unix::net::UnixStream};
    // The fixture has no consumer: accepted input stays in the actual bounded
    // UI queue, making admission, lost replies and cancellation observable.
    let directory = tempfile::tempdir().unwrap();
    let id = Id::generate().unwrap();
    let (inputs, admitted) = async_channel::bounded(MAX_INPUTS);
    let (wake, awakened) = mpsc::sync_channel(1);
    let (errors, _) = async_channel::bounded(1);
    let handle = Handle {
        #[cfg(all(feature = "native-audio", target_os = "macos"))]
        audio_instance: None,
        session: None,
        generation: 7,
        #[cfg(all(target_os = "macos", feature = "process"))]
        startup: None,
        pending: Arc::new(AtomicUsize::new(0)),
        inputs,
        wake,
        failures: Arc::new(AtomicU64::new(0)),
        errors,
    };
    let (outputs, lifecycle) = async_channel::bounded(1);
    let server = Server::start(directory.path(), id, outputs).unwrap();
    let publisher = server.publisher();
    let (peer, _other_peer) = UnixStream::pair().unwrap();
    let identity = crate::engine::process_owner::Owner::capture(&peer).unwrap().identity;
    let mut owner = LaunchOwner {
        presentation: 0,
        instance: SessionInstance {
            restore_host_mounts: false,
            id,
            launch_request: Id::generate().unwrap(),
            account: Id::from_bytes([1; 16]),
            session: crate::identifier::encode(&[0xab; 32]),
            build: crate::identifier::encode(&[0xcd; 32]),
            root: Id::from_bytes([2; 16]),

            extra_volumes: vec![],
            host_mounts: Box::default(),
            owner_generation: 1,
            state: State::Owned,
        },
        process: Some(identity),
    };
    let call = |owner: &LaunchOwner, operation| {
        Exchange::connect(directory.path(), owner, operation).unwrap().receive()
    };
    // Claim can be visible before the first admitted turn. Force that window
    // without sleeps: the same verified generation refuses controls until open.
    for operation in [
        Operation::Frame,
        Operation::Input { kind: 3, target: 100, data: b"Before open".to_vec() },
        Operation::Show { presentation: 0 },
        Operation::Quit,
    ] {
        assert_eq!(
            call(&owner, operation).unwrap_err().to_string(),
            "The app controls are not ready yet; this request was not applied"
        );
        assert!(admitted.is_empty() && lifecycle.is_empty());
    }
    publisher.opened(1, crate::identifier::encode(&[0xcd; 32]), handle.clone());
    publisher.frame(Some(vec![0x80; 1024]), &["Initial owner log".into()]);
    let Reply::Frame { scene, generation, instance, .. } = call(&owner, Operation::Frame).unwrap()
    else {
        panic!()
    };
    assert_eq!(scene, vec![0x80; 1024]);
    assert_eq!((generation, instance), (1, id));
    // A fragmented or oversized peer occupies only its bounded socket slot;
    // other controls and the local input owner remain usable.
    use std::io::Write;
    let mut partial =
        UnixStream::connect(app_control::socket_path(directory.path(), id).unwrap()).unwrap();
    partial
        .write_all(
            &((app_control::REQUEST_LIMIT + crate::engine::ipc::codec::PREFIX_LEN + 1) as u32)
                .to_le_bytes(),
        )
        .unwrap();
    assert!(matches!(call(&owner, Operation::Frame).unwrap(), Reply::Frame { .. }));
    drop(partial);
    for request in [
        app_control::Request {
            instance: Id::generate().unwrap(),
            generation: 1,
            operation: Operation::Quit,
        },
        app_control::Request {
            instance: id,
            generation: 1,
            operation: Operation::Input {
                kind: 3,
                target: 100,
                data: vec![0; playbit_api::abi::syscall::MAX_EVENT_BYTES + 1],
            },
        },
    ] {
        let mut stream =
            UnixStream::connect(app_control::socket_path(directory.path(), id).unwrap()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let bytes =
            crate::engine::ipc::codec::encode(&request, app_control::REQUEST_LIMIT).unwrap();
        crate::engine::ipc::codec::write(&mut stream, &bytes).unwrap();
        let bytes = crate::engine::ipc::codec::read(&mut stream, app_control::REPLY_LIMIT).unwrap();
        let reply = rkyv::api::low::from_bytes::<Reply, rkyv::rancor::Error>(&bytes).unwrap();
        assert!(matches!(reply, Reply::Error { .. }));
    }
    assert!(admitted.is_empty() && lifecycle.is_empty());
    let Reply::Logs(batch) = call(&owner, Operation::Logs { after: None }).unwrap() else {
        panic!()
    };
    assert_eq!(batch.lines[0].message, "Initial owner log");
    assert!(batch.lines[0].timestamp_micros.is_some());
    assert_eq!(batch.lines[0].severity, app_control::LogSeverity::Info);
    let cursor = batch.cursor;
    let wait = Exchange::connect(directory.path(), &owner, Operation::Logs { after: Some(cursor) })
        .unwrap();
    let waiting = std::thread::spawn(move || wait.receive().unwrap());
    publisher.frame(None, &["Readiness wakes logs".into()]);
    let Reply::Logs(batch) = waiting.join().unwrap() else { panic!() };
    assert_eq!(batch.lines[0].message, "Readiness wakes logs");
    assert_eq!(batch.cursor.next, 2);
    // A silent stream cancels by socket readiness, without a polling deadline.
    let silent =
        Exchange::connect(directory.path(), &owner, Operation::Logs { after: Some(batch.cursor) })
            .unwrap();
    silent.cancellation_handle().unwrap().shutdown(Shutdown::Both).unwrap();
    assert!(silent.receive().is_err());
    let many: Vec<String> = (0..300).map(|n| format!("Log {n}")).collect();
    publisher.frame(None, &many);
    let Reply::Logs(batch) = call(&owner, Operation::Logs { after: None }).unwrap() else {
        panic!()
    };
    assert_eq!(batch.lines.len(), 256);
    assert_eq!(batch.dropped, 46);
    assert_eq!(batch.lines[0].message, "Log 44");
    let mut wrong = owner.clone();
    wrong.instance.owner_generation = 2;
    assert!(
        call(&wrong, Operation::Input { kind: 3, target: 100, data: b"Wrong generation".to_vec() })
            .is_err()
    );
    assert!(admitted.is_empty());
    #[cfg(target_os = "macos")]
    {
        assert!(call(&wrong, Operation::Screenshot).is_err());
        let pending = Exchange::connect(directory.path(), &owner, Operation::Screenshot).unwrap();
        let Output::Screenshot { owner_generation: 1, capture, .. } =
            lifecycle.recv_blocking().unwrap()
        else {
            panic!("expected selected generation to receive capture")
        };
        assert!(call(&owner, Operation::Screenshot).is_err(), "a capture is already in flight");
        assert!(matches!(call(&owner, Operation::Frame).unwrap(), Reply::Frame { .. }));
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(app_control::MAX_SCENE_BYTES + 64, 0);
        capture.complete(Ok(png.clone()));
        assert!(matches!(pending.receive().unwrap(), Reply::Screenshot {
            instance, generation: 1, png: result
        } if instance == id && result == png));
    }
    for operation in [
        Operation::key(7, "up", true).unwrap(),
        Operation::pointer(7, "down", 24, 16, None).unwrap(),
        Operation::key(7, "up", false).unwrap(),
    ] {
        let Operation::Input { kind, target, ref data } = operation else { unreachable!() };
        let expected = data.clone();
        assert!(matches!(call(&owner, operation).unwrap(), Reply::Accepted { .. }));
        awakened.recv_timeout(Duration::from_secs(5)).unwrap();
        let message = admitted.try_recv().unwrap();
        assert!(matches!(message.source, InputSource::Tool));
        let input = message.input;
        assert_eq!((input.kind as u32, input.target, input.data), (kind, target, expected));
    }
    for kind in [EventKind::Key, EventKind::Pointer] {
        assert!(
            call(&owner, Operation::Input { kind: kind as u32, target: 7, data: vec![] }).is_err()
        );
    }
    assert!(admitted.is_empty());
    wrong.process = None;
    assert!(Exchange::connect(directory.path(), &wrong, Operation::Quit).is_err());
    assert!(lifecycle.is_empty());
    assert!(matches!(
        call(&owner, Operation::Input { kind: 3, target: 100, data: b"Accepted once".to_vec() })
            .unwrap(),
        Reply::Accepted { .. }
    ));
    awakened.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(admitted.try_recv().unwrap().input.data, b"Accepted once");
    // Discard the acknowledgment through a cloned stream. The exchange then
    // sees EOF after actual admission, even if the reply was already buffered;
    // it must not claim a pre-send failure or submit a replacement request.
    let lost = Exchange::connect(
        directory.path(),
        &owner,
        Operation::Input { kind: 3, target: 100, data: b"Lost acknowledgment".to_vec() },
    )
    .unwrap();
    let mut discarded = lost.cancellation_handle().unwrap();
    crate::engine::ipc::codec::read(&mut discarded, app_control::REPLY_LIMIT).unwrap();
    drop(discarded);
    let error = lost.receive().unwrap_err();
    assert!(
        !error.to_string().contains("no request was sent"),
        "Losing a reply after admission must not claim the request was unsent"
    );
    awakened.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(admitted.try_recv().unwrap().input.data, b"Lost acknowledgment");
    assert!(admitted.is_empty());
    let mut other = owner.clone();
    other.instance.id = Id::generate().unwrap();
    assert!(app_control::select(vec![owner.clone(), other.clone()], None).is_err());
    assert_eq!(app_control::select(vec![owner.clone(), other], Some(id)).unwrap().instance.id, id);
    let mut old_generation = owner.clone();
    old_generation.instance.owner_generation = 2;
    assert!(call(&old_generation, Operation::Show { presentation: 0 }).is_err());
    assert!(matches!(
        call(&owner, Operation::Show { presentation: 0 }).unwrap(),
        Reply::Accepted { .. }
    ));
    assert!(matches!(
        lifecycle.recv_blocking().unwrap(),
        Output::ShowRequested { owner_generation: 1, presentation: 0 }
    ));
    assert!(admitted.is_empty(), "Show is local UI admission, never guest input");
    assert!(matches!(call(&owner, Operation::Quit).unwrap(), Reply::Accepted { .. }));
    assert!(matches!(lifecycle.recv_blocking().unwrap(), Output::QuitRequested));
    assert!(call(&owner, Operation::Quit).is_err());
    assert!(call(&owner, Operation::Show { presentation: 0 }).is_err());
    assert!(lifecycle.is_empty(), "Show rejected after Quit admission");
    assert!(call(&owner, Operation::Input { kind: 2, target: 101, data: vec![] }).is_err());
    assert!(admitted.is_empty());
    // Reconstruction is explicit and gives tools a new generation; an old log
    // cursor can report reset without admitting an old input or another Quit.
    publisher.stopped(Some("Waiting for explicit reconstruction"));
    let Reply::Logs(failure) = call(&owner, Operation::Logs { after: Some(batch.cursor) }).unwrap()
    else {
        panic!()
    };
    assert_eq!(failure.lines[0].severity, app_control::LogSeverity::Error);
    assert!(failure.lines[0].timestamp_micros.is_some());
    let Reply::Logs(replayed) =
        call(&owner, Operation::Logs { after: Some(batch.cursor) }).unwrap()
    else {
        panic!()
    };
    assert_eq!(failure.lines, replayed.lines, "source metadata survives rereading");
    let paused = Exchange::connect(
        directory.path(),
        &owner,
        Operation::Logs { after: Some(failure.cursor) },
    )
    .unwrap();
    let paused_reader = std::thread::spawn(move || paused.receive());
    assert!(call(&owner, Operation::Frame).is_err());
    let prior_owner = owner.clone();
    owner.instance.owner_generation = 2;
    publisher.opened(2, crate::identifier::encode(&[0xef; 32]), handle);
    publisher.frame(Some(vec![1, 2, 3]), &["Fresh owner".into()]);
    let Reply::Logs(resumed) = paused_reader.join().unwrap().unwrap() else { panic!() };
    assert!(resumed.reset && resumed.cursor.generation == 2);
    let Reply::Logs(batch) = call(&owner, Operation::Logs { after: Some(cursor) }).unwrap() else {
        panic!()
    };
    assert!(batch.reset);
    assert_eq!(batch.lines[0].message, "Fresh owner");
    let Reply::Logs(batch) = call(&prior_owner, Operation::Logs { after: Some(cursor) }).unwrap()
    else {
        panic!()
    };
    assert!(batch.reset && batch.cursor.generation == 2);
    assert!(
        call(
            &prior_owner,
            Operation::Input { kind: 3, target: 100, data: b"Retired input".to_vec() }
        )
        .is_err()
    );
    drop(server);
    let socket = app_control::socket_path(directory.path(), id).unwrap();
    assert!(!socket.exists());
    // A surviving verified process can lose its scoped endpoint. Both missing
    // and refused connections fail before a request, without another Quit.
    assert!(
        crate::engine::process_owner::Owner::recover(owner.process.as_ref().unwrap())
            .unwrap()
            .is_some()
    );
    for kind in [std::io::ErrorKind::NotFound, std::io::ErrorKind::ConnectionRefused] {
        if kind == std::io::ErrorKind::ConnectionRefused {
            drop(UnixListener::bind(&socket).unwrap());
            assert!(socket.exists());
        }
        let error = Exchange::connect(directory.path(), &owner, Operation::Quit).err().unwrap();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), kind);
        assert!(error.to_string().contains("control endpoint is unavailable; no request was sent"));
        assert!(lifecycle.is_empty(), "Failed connect must not admit another Quit");
    }
    std::fs::remove_file(socket).unwrap();
}

#[test]
#[cfg(feature = "process")]
fn guest_startup_retries_transient_failure_and_cancels_without_a_window() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().to_owned();
    let paths = crate::paths::Paths::for_profile(&profile).unwrap();
    std::fs::create_dir_all(paths.launch_lock.parent().unwrap()).unwrap();
    let lock = std::fs::File::create(paths.launch_lock).unwrap();
    lock.try_lock().unwrap();
    let (wake, receive_wake) = mpsc::sync_channel(1);
    let (send, output) = async_channel::bounded(1);
    let stop = Arc::new(AtomicBool::new(false));
    let owner_stop = stop.clone();
    let owner_wake = wake.clone();
    let owner = std::thread::spawn(move || {
        initialize_guest_engine(
            &profile,
            &std::env::current_exe().unwrap(),
            &owner_wake,
            &receive_wake,
            &send,
            &owner_stop,
        )
    });
    let Output::Recovering { generation, active, .. } = receive(&output) else { panic!() };
    assert_eq!(generation, 0);
    assert!(active.load(Ordering::Acquire));
    stop.store(true, Ordering::Release);
    wake.try_send(()).unwrap();
    assert!(matches!(owner.join().unwrap(), Ok(GuestEngine::Canceled)));
    assert!(!active.load(Ordering::Acquire));
    assert!(output.is_empty(), "retry notices are emitted once per outage");
}

#[cfg(feature = "process")]
#[test]
fn guest_startup_quit_during_engine_probe_skips_instance_admission() {
    use crate::engine::{ipc, reply::Reply};

    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().to_owned();
    let paths = crate::paths::Paths::for_profile(&profile).unwrap();
    std::fs::create_dir_all(paths.launch_lock.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
    let mut worker = Worker::start_guest(profile, PathBuf::from("/nonexistent"), None, || {
        panic!("Quit during the engine probe must precede session resolution")
    })
    .unwrap();
    let (mut connection, _) = listener.accept().unwrap();
    ipc::read(&mut connection).unwrap();
    worker.request_stop();
    let reply = Reply::Protocol { protocol: ipc::LOCAL_PROTOCOL, version: crate::VERSION.into() };
    ipc::write(&mut connection, &ipc::codec::encode(&reply, 512).unwrap()).unwrap();
    assert!(matches!(receive(&worker.output()), Output::Closed(0)));
    worker.completion().recv_blocking().unwrap().unwrap();
    worker.shutdown();
}

#[cfg(feature = "connected-engine")]
#[test]
fn setup_reset_releases_store_and_reopens_an_independent_empty_profile() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let other = root.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("data"), b"keep").unwrap();
    let (done, receive) = std::sync::mpsc::sync_channel(1);
    // SAFETY: this fixture admits no callbacks or sessions.
    let mut worker = unsafe {
        Worker::start_with_setup(profile.clone(), move |owner, reset, _| {
            let old = owner.as_ref().unwrap().services.store.account().id;
            reset(owner, crate::ui::desktop::sign_in::flow::ProfileChange::Reset)?;
            let fresh = owner.as_ref().unwrap();
            assert!(!fresh.enrolled());
            assert_ne!(fresh.services.store.account().id, old);
            done.send(()).unwrap();
            Err("Fixture completed".into())
        })
        .unwrap()
    };
    receive.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    worker.shutdown();
    assert_eq!(std::fs::read(other.join("data")).unwrap(), b"keep");
}

#[cfg(feature = "connected-engine")]
#[test]
fn portable_replacement_opens_the_choice_and_resumes_confirmed_retry() {
    // A restored mismatch enters the same portable setup window. An interrupted
    // confirmed choice must finish through the reset adapter, not consume its
    // journal before the adapter validates which account is being replaced.
    let (directory, original, identity) = replacement_profile();
    let profile = directory.path().canonicalize().unwrap();
    let incoming = Id::from_bytes(identity.account_id().unwrap());
    let initializers = crate::package::VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap();
    let package = crate::package::Package::create(
        "test".into(),
        "Replacement fixture".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        Default::default(),
        &initializers,
    )
    .unwrap();
    let path = directory.path().join("app.pbapp");
    std::fs::write(&path, package.to_bytes().unwrap()).unwrap();
    let portable = super::super::portable::Portable {
        path,
        expected: package.digest(),
        app: playbit_api::native_app(),
    };
    // SAFETY: setup is canceled before this descriptor can execute callbacks.
    let mut worker = unsafe {
        Worker::start_with_setup(profile.clone(), move |owner, reset, ui| {
            portable.prepare(owner, reset, ui)
        })
        .unwrap()
    };
    let Output::SignIn { enrolled, profile: account, .. } = receive(&worker.output()) else {
        panic!("retained data must reach the account decision")
    };
    assert!(!enrolled);
    assert!(account.replacement_required);
    worker.shutdown();
    let (done, result) = mpsc::sync_channel(1);
    // SAFETY: the fixture never admits guest callbacks or a session.
    let mut worker = unsafe {
        Worker::start_with_setup(profile.clone(), move |owner, reset, _| {
            assert_eq!(owner.as_ref().unwrap().services.store.account().id, original);
            let pending = profile.join("control/sign-out-pending");
            let seed = owner.as_ref().unwrap().services.store.account_seed().unwrap();
            let intent = serde_json::json!({"Replace": {"seed": seed, "identity": identity, "account_key": null}});
            std::fs::write(&pending, serde_json::to_vec(&intent).unwrap()).unwrap();
            let outcome = reset(owner, crate::ui::desktop::sign_in::flow::ProfileChange::Replace(true)).map(|()| {
                let fresh = owner.as_ref().unwrap();
                assert_eq!(fresh.services.store.account().id, incoming);
                assert!(fresh.services.store.volumes().unwrap().is_empty());
                assert!(!pending.exists());
            });
            done.send(outcome).unwrap();
            Err("Fixture completed".into())
        })
        .unwrap()
    };
    let outcome = result.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.shutdown();
    outcome.unwrap();
}

#[cfg(feature = "connected-engine")]
fn replacement_profile() -> (tempfile::TempDir, Id, crate::network::ap::ClientIdentity) {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().canonicalize().unwrap();
    let store = crate::store::LocalStore::open(&profile).unwrap();
    store.create_volume("Retained data", true).unwrap();
    let original = store.account().id;
    let seed = store.account_seed().unwrap();
    let incoming = Id::from_bytes([42; 16]);
    let mut identity =
        crate::network::ap::ClientIdentity::from_seed("b@example.test", "", seed).unwrap();
    identity.auth_token = vec![1; 72];
    identity.auth_token[..16].copy_from_slice(incoming.as_bytes());
    identity.instance_id = Id::from_bytes([2; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![2];
    identity.save(&profile.join("instance.json")).unwrap();
    drop(store);
    (directory, original, identity)
}

#[cfg(feature = "connected-engine")]
#[test]
fn profile_change_reopen_failure_retries_only_the_committed_choice() {
    // Fail the adapter's reopen dependency after each decision commits. Retrying
    // the same decision opens it without replay; the opposite choice cannot mutate it.
    use crate::ui::desktop::sign_in::flow::ProfileChange;
    for change in
        [ProfileChange::Replace(false), ProfileChange::Replace(true), ProfileChange::SignOut]
    {
        let (directory, original, identity) = replacement_profile();
        let profile = directory.path();
        let incoming = Id::from_bytes(identity.account_id().unwrap());
        let (pending, seed) = {
            let store = crate::store::LocalStore::open(profile).unwrap();
            (store.pending_operations().unwrap(), store.account_seed().unwrap())
        };
        assert!(!pending.is_empty());
        std::fs::create_dir_all(profile.join("blobs")).unwrap();
        std::fs::write(profile.join("blobs/unsynced"), b"Local-only data").unwrap();
        let mut owner = Some(crate::engine::connected::Connected::open(profile, || {}).unwrap());
        let mut committed = None;
        let result = super::super::network::change_profile(
            profile,
            &mut owner,
            change,
            &mut committed,
            || Err("Reopen dependency unavailable".into()),
        );
        assert_eq!(result.unwrap_err(), change.failure_message(true));
        let diagnostics =
            std::fs::read_to_string(crate::paths::Paths::diagnostic_log(profile).unwrap()).unwrap();
        assert!(diagnostics.contains("reopen changed sign-in profile"));
        assert!(diagnostics.contains("Reopen dependency unavailable"));
        assert!(owner.is_none());
        assert_eq!(committed, Some(change));
        let opposite = ProfileChange::Replace(change != ProfileChange::Replace(true));
        let result = super::super::network::change_profile(
            profile,
            &mut owner,
            opposite,
            &mut committed,
            || panic!("opposite choice must not reopen"),
        );
        assert_eq!(result.unwrap_err(), change.failure_message(true));
        super::super::network::change_profile(profile, &mut owner, change, &mut committed, || {
            crate::engine::connected::Connected::open(profile, || {}).map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(committed.is_none());
        let owner = owner.unwrap();
        let replace = change == ProfileChange::Replace(true);
        assert_eq!(owner.services.store.account().id, if replace { incoming } else { original });
        assert_eq!(owner.enrolled(), replace);
        assert_eq!(owner.services.store.volumes().unwrap().len(), usize::from(!replace));
        if !replace {
            assert_eq!(owner.services.store.pending_operations().unwrap(), pending);
            assert_eq!(owner.services.store.account_seed().unwrap(), seed);
            assert_eq!(std::fs::read(profile.join("blobs/unsynced")).unwrap(), b"Local-only data");
        }
    }
}

#[cfg(feature = "connected-engine")]
#[test]
fn profile_change_failure_keeps_diagnostics_and_does_not_claim_completion() {
    // Hold the real launch lock to fail before mutation. The UI gets actionable
    // local guidance, while the bounded diagnostic sink records the lock error.
    use crate::ui::desktop::sign_in::flow::ProfileChange;
    for change in [ProfileChange::SignOut, ProfileChange::Reset, ProfileChange::Replace(true)] {
        let (directory, original, _) = replacement_profile();
        let profile = directory.path();
        let path = crate::paths::Paths::for_profile(profile).unwrap().launch_lock;
        let lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        lock.try_lock().unwrap();
        let mut owner = None;
        let mut committed = None;
        let error = super::super::network::change_profile(
            profile,
            &mut owner,
            change,
            &mut committed,
            || panic!("uncommitted change cannot reopen"),
        )
        .unwrap_err();
        assert_eq!(error, change.failure_message(false));
        assert!(error.contains("Quit and reopen Playbit"));
        assert!(committed.is_none());
        let diagnostics =
            std::fs::read_to_string(crate::paths::Paths::diagnostic_log(profile).unwrap()).unwrap();
        assert!(diagnostics.contains("change sign-in profile"));
        assert!(diagnostics.contains(&format!("change={change:?} error=")));
        let store = crate::store::LocalStore::open(profile).unwrap();
        assert_eq!(store.account().id, original);
        assert_eq!(store.volumes().unwrap().len(), 1);
    }
}

#[test]
#[cfg(feature = "process")]
fn owner_teardown_drains_data_through_a_late_context_stop_and_uncertain_reply() {
    // Exercise the exact owner barrier with real GuestHost lanes. The initial
    // failure is absent and File is idle; a later stale read must not let the
    // owner finish before its accepted mutation has a terminal outcome.
    use crate::engine::{
        ArchivedRequest, Request, guest_host::GuestHost, ipc, ipc::codec, reply::Reply,
        service_call::Outcome, service_events::StopReason, session_instance::Context,
    };
    use crate::runtime::{
        host::{DataCall, DataOperation, DataResult, FieldChange, Value as HostValue},
        syscall::{Host, HostError, HostResult},
    };
    use std::os::unix::net::UnixStream;

    fn request(listener: &UnixListener) -> (UnixStream, Request) {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "accepted Data fixture must progress");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let bytes = ipc::read(&mut stream).unwrap();
        let archived = codec::access::<ArchivedRequest>(&bytes).unwrap();
        let request =
            rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>(archived).unwrap();
        (stream, request)
    }
    fn reply(mut stream: UnixStream, request: Request, result: HostResult<DataResult>) {
        let Request::Syscall { context, operation, mutation, call } = request else {
            panic!("accepted Data exchange")
        };
        let (status, result) = match result {
            Ok(result) => (0, Some(result)),
            Err(error) => (error as i32, None),
        };
        let bytes = codec::encode(
            &Reply::Syscall(Outcome {
                context,
                volume: Id::from_bytes(call.volume),
                operation,
                mutation,
                kind: call.operation.kind() as u32,
                revision: Some((
                    match context {
                        crate::engine::service_call::Context::SessionInstance(context) => {
                            context.incarnation
                        }
                        crate::engine::service_call::Context::Tool { incarnation, .. } => {
                            incarnation
                        }
                    },
                    1,
                )),
                status,
                result,
            }),
            codec::MAX_ARCHIVE,
        )
        .unwrap();
        ipc::write(&mut stream, &bytes).unwrap();
    }
    for uncertain in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        crate::paths::private_directory(directory.path(), "control").unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        let context = Context {
            instance: Id::from_bytes([1; 16]),
            handle: Id::from_bytes([2; 16]),
            incarnation: Id::from_bytes([3; 16]),
            owner_generation: 1,
        };
        let (wake, receive) = mpsc::sync_channel(1);
        let (mut host, inbox) = GuestHost::start(
            directory.path().into(),
            context,
            Arc::new(move || {
                let _ = wake.try_send(());
            }),
        )
        .unwrap();
        let (entered, started) = mpsc::sync_channel(1);
        let (mark, marking) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut events, event_request) = request(&listener);
            assert!(
                matches!(event_request, Request::InstanceEvents { context: actual } if actual == context)
            );
            let bytes = codec::encode(
                &Reply::InstanceEvents(crate::engine::service_events::Notice {
                    display_name: String::new(),
                    context,
                    sequence: 1,
                    revision: Some((context.incarnation, 1)),
                    data_revision: 1,
                    mount_revision: 0,
                    stop: None,
                    gap: false,
                    volumes: vec![crate::runtime::host::AvailableVolume {
                        backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                        source_epoch: [0; 16],
                        id: [4; 16],
                        name: "Root".into(),
                        tags: vec!["root".into()],
                        rights: crate::runtime::host::VolumeRights::ALL,
                        is_root: true,
                        role: 0,
                    }],
                }),
                4096,
            )
            .unwrap();
            ipc::write(&mut events, &bytes).unwrap();
            let mut read = None;
            let mut edit = None;
            for _ in 0..2 {
                let exchange = request(&listener);
                let Request::Syscall { mutation, .. } = &exchange.1 else { panic!("Data request") };
                if *mutation == 0 {
                    read = Some(exchange);
                } else {
                    edit = Some(exchange);
                }
            }
            entered.send(()).unwrap();
            marking.recv_timeout(Duration::from_secs(5)).unwrap();
            let (stream, request) = read.unwrap();
            reply(stream, request, Err(HostError::StaleHandle));
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            let (stream, request) = edit.unwrap();
            if uncertain {
                drop(stream);
            } else {
                reply(stream, request, Ok(DataResult::Edited));
            }
            (listener, events)
        });
        let first = host
            .begin_data(
                9,
                Arc::new(DataCall {
                    volume: [4; 16],
                    operation: DataOperation::SchemaRead { entry_id: [5; 15] },
                }),
            )
            .unwrap();
        let second = host
            .begin_data(
                9,
                Arc::new(DataCall {
                    volume: [4; 16],
                    operation: DataOperation::Edit {
                        entry_id: [5; 15],
                        object_id: [5; 16],
                        fields: vec![FieldChange {
                            field_id: [6; 16],
                            values: Some(vec![HostValue::Boolean(true)]),
                        }],
                    },
                }),
            )
            .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(inbox.stop_reason(), None);
        assert_eq!(inbox.failure(), None);
        assert!(!inbox.files_executing());
        assert!(inbox.work_executing());
        host.release_data(first);
        host.release_data(second);
        drop(host);
        let (completed, completion) = mpsc::sync_channel(1);
        let draining = inbox.clone();
        let owner = std::thread::spawn(move || {
            completed.send(drain_guest_work(&draining, &receive)).unwrap();
        });
        mark.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while inbox.stop_reason() != Some(StopReason::ContextUnavailable) {
            assert!(Instant::now() < deadline, "late stale read must reach owner classification");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(inbox.work_executing());
        assert!(!inbox.files_executing());
        // A bounded observation deadline distinguishes an idle barrier from
        // an owner that has already returned through the File-only branch.
        let premature = completion.recv_timeout(Duration::from_millis(100)).ok();
        release.send(()).unwrap();
        if premature.is_none() {
            completion.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        }
        owner.join().unwrap();
        let (listener, _events) = server.join().unwrap();
        assert!(premature.is_none(), "owner completion must wait for accepted finite Data");
        assert!(!inbox.work_executing());
        assert_eq!(
            inbox.stop_reason(),
            if uncertain { None } else { Some(StopReason::ContextUnavailable) }
        );
        if uncertain {
            assert!(inbox.failure().unwrap().contains("Accepted changes may have persisted"));
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}
