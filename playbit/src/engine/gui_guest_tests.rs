#![allow(
    clippy::unwrap_used,
    reason = "Tests own disposable profiles, contexts and Wasm instances."
)]
use super::*;
use crate::engine::{Engine, Request, reply::Reply};
use std::os::unix::net::UnixStream;

// Instantiate each VM on its owning test thread. Preparation may use a scoped
// worker while the test services real engine admission and compiler IPC.
fn construct(
    profile: &Path,
    admission: Admission,
    package: Package,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<Guest> {
    Guest::prepare(profile, admission, package, wake, None)?.instantiate()
}

#[test]
fn native_host_supports_scorekeepers_full_automatic_volume_requirement() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../guests/scorekeeper");
    let manifest = crate::package::Manifest::read(&source).unwrap();
    let full = playbit_api::abi::data::AUTOMATIC_VOLUMES_FEATURE;
    assert!(manifest.required_host_features.iter().any(|feature| feature == full));
    assert!(crate::engine::guest_features().contains(&full));
}

#[test]
fn signed_todo_with_session_volume_requirement_runs_in_process_host() {
    use crate::package::{AutomaticVolumeRole as Role, Manifest};
    use crate::store::{BuildSelector, SessionCreation};
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../guests/todo");
    let package = Manifest::read(&source)
        .unwrap()
        .bundle(&source, playbit_test_fixtures::Guest::Todo.wasm().to_vec())
        .unwrap();
    assert!(
        package
            .required_host_features()
            .iter()
            .any(|feature| feature == playbit_api::abi::data::SESSION_VOLUME_FEATURE)
    );
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let store = &engine.services.store;
    let package = crate::package::signature::testing::signed(store, package).unwrap();
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let builds = directory.path().join(crate::paths::BUILDS);
    std::fs::create_dir_all(&builds).unwrap();
    std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
    let root = app.volume_id(Role::Root, None, None, None).unwrap();
    let session = store
        .create_session_binding(SessionCreation {
            app,
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            root_volume: root,
            user_volume: None,
            sync_shared: true,
        })
        .unwrap();
    let (peer, _client) = UnixStream::pair().unwrap();
    let Reply::Instance(instance) = engine
        .dispatch_instance(
            Request::InstanceReserve {
                request: Id::generate().unwrap(),
                incarnation: engine.session_instances.incarnation(),
                session: session.id,
                extra_volumes: Box::default(),
                host_mounts: Box::default(),
                restore_host_mounts: false,
            },
            &peer,
        )
        .unwrap()
    else {
        panic!("TODO reservation")
    };
    let Reply::InstanceAdmission(admission) =
        engine.dispatch_instance(Request::InstanceClaim { instance: instance.id }, &peer).unwrap()
    else {
        panic!("TODO admission")
    };
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let wake = engine.completion.wake.clone();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || wake.notify()),
        &mut queue,
        &mut engine,
    );
    assert!(guest.pump().unwrap().view.is_some());
    let drain = guest.file_drain();
    drop(guest);
    let deadline = Instant::now() + Duration::from_secs(10);
    while drain.files_executing() {
        assert!(Instant::now() < deadline, "TODO File roots retire");
        queue.run_until(&mut engine, Some(deadline)).unwrap();
    }
}

#[test]
fn read_only_shared_recipient_starts_without_world_file_backing() {
    // A real TODO guest must start from a second account's Shared read grant.
    // Root aliases stay lazy, while actual File use retains its data authority.
    use crate::engine::file_service;
    use crate::package::{AutomaticVolumeRole as Role, Manifest};
    use crate::runtime::host::{
        HostResult,
        files::{Action, Call, Output},
    };
    use crate::store::{Access, BuildSelector, DatabaseId, LocalStore, SessionCreation};
    use playbit_api::abi::{files::FILE_WRITE, syscall::Error};

    fn file_request(
        engine: &mut Engine,
        context: Context,
        operation: &mut u64,
        request: file_service::Request,
        peer: &UnixStream,
    ) -> HostResult<Output> {
        *operation += 1;
        let ticket = engine.begin_file(context, *operation, request.clone(), peer)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            engine.maintain_files().unwrap();
            if ticket.ready() {
                let revision = engine.services.store.delivery_revision();
                return engine
                    .file_service
                    .as_mut()
                    .unwrap()
                    .take(&ticket, revision)
                    .unwrap()
                    .take(context, *operation, &request);
            }
            assert!(Instant::now() < deadline, "recipient File request must finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let owner_directory = tempfile::tempdir().unwrap();
    let owner = LocalStore::open(owner_directory.path()).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../guests/todo");
    let package = Manifest::read(&source)
        .unwrap()
        .bundle(&source, playbit_test_fixtures::Guest::Todo.wasm().to_vec())
        .unwrap();
    let package = crate::package::signature::testing::signed(&owner, package).unwrap();
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let root = app.volume_id(Role::Root, None, None, None).unwrap();
    let builds = owner_directory.path().join(crate::paths::BUILDS);
    std::fs::create_dir_all(&builds).unwrap();
    std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
    let session = owner
        .create_session_binding(SessionCreation {
            app,
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build.clone(),
            root_volume: root,
            sync_shared: true,
            user_volume: None,
        })
        .unwrap();
    let shared = owner.open_automatic_volume(&session.id, Role::Session).unwrap().id;
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let store = engine.services.store.clone();
    owner
        .grant_volume(&Access::owner(owner.account().id), shared, store.account().id, false)
        .unwrap();
    store
        .import_invitation_link(
            &owner.invitation(&session.id, engine.channel()).unwrap(),
            &owner.session_envelope(&session.id).unwrap(),
            engine.channel(),
        )
        .unwrap();
    // Substitute the normal authenticated full-cut receipt. Only Shared data
    // is transferred; no World credentials or unrelated app scopes are added.
    let mut stage = store.prepare_received_volume(shared).unwrap();
    let mut after = None;
    loop {
        let page = owner.replication_database_page(shared, after).unwrap();
        for database in page.databases {
            owner
                .with_replica(shared, database, |source| {
                    stage.with_database(database, |target| {
                        if database == DatabaseId::Index {
                            crate::store::merge_index_replica(
                                directory.path(),
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        } else {
                            crate::store::merge_replica(
                                directory.path(),
                                source,
                                target,
                                kvdb::Lww,
                                |_| Ok(()),
                            )
                        }
                    })
                })
                .unwrap();
            stage.finish_database(database).unwrap();
        }
        let Some(next) = page.next else { break };
        after = Some(next);
    }
    use crate::store::volume_index::namespace::{receipt, source::cut};
    let accepted = stage
        .with_database(DatabaseId::Index, |index| cut::accepted(&index.begin_read()?))
        .unwrap();
    stage
        .with_database(DatabaseId::Index, |index| receipt::retain_fixture(index, None, &accepted))
        .unwrap();
    stage.qualify_authority();
    let access = Access::session_version(store.account().id, &session.id, &build);
    store.adopt_received_volume(&access, &stage).unwrap();
    drop(stage);
    let builds = directory.path().join(crate::paths::BUILDS);
    std::fs::create_dir_all(&builds).unwrap();
    std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
    assert!(store.volume(root).is_err());
    let (peer, _client) = UnixStream::pair().unwrap();
    let Reply::Instance(instance) = engine
        .dispatch_instance(
            Request::InstanceReserve {
                request: Id::generate().unwrap(),
                incarnation: engine.session_instances.incarnation(),
                session: session.id.clone(),
                extra_volumes: Box::default(),
                host_mounts: Box::default(),
                restore_host_mounts: false,
            },
            &peer,
        )
        .unwrap()
    else {
        panic!("recipient reservation")
    };
    let Reply::InstanceAdmission(admission) =
        engine.dispatch_instance(Request::InstanceClaim { instance: instance.id }, &peer).unwrap()
    else {
        panic!("recipient admission")
    };
    let context = admission.context;
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(|| {}),
        &mut queue,
        &mut engine,
    );
    guest.pump().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !format!("{:?}", guest.scene()).contains("Try Playbit offline") {
        assert!(Instant::now() < deadline, "recipient must display existing Shared data");
        queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(10))).unwrap();
        if guest.ready(Instant::now()) {
            guest.pump().unwrap();
        }
    }
    assert!(store.volume(root).is_err(), "discovery must not create the publisher's World");
    let drain = guest.file_drain();
    drop(guest);
    while drain.work_executing() {
        assert!(Instant::now() < deadline, "recipient guest work must retire");
        queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(10))).unwrap();
    }
    let mut operation = 1000;
    let Output::File { handle: world } = file_request(
        &mut engine,
        context,
        &mut operation,
        file_service::Request::VolumeRoot { volume: root },
        &peer,
    )
    .unwrap() else {
        panic!("logical World root")
    };
    assert_eq!(
        file_request(
            &mut engine,
            context,
            &mut operation,
            file_service::Request::Operation(Call { file: world, action: Action::Status }),
            &peer
        ),
        Err(Error::AccessDenied)
    );
    assert!(store.volume(root).is_err());
    let Output::File { handle: shared_root } = file_request(
        &mut engine,
        context,
        &mut operation,
        file_service::Request::VolumeRoot { volume: shared },
        &peer,
    )
    .unwrap() else {
        panic!("Shared root")
    };
    assert!(matches!(
        file_request(
            &mut engine,
            context,
            &mut operation,
            file_service::Request::Operation(Call { file: shared_root, action: Action::Status }),
            &peer
        ),
        Ok(Output::Status { .. })
    ));
    assert_eq!(
        file_request(
            &mut engine,
            context,
            &mut operation,
            file_service::Request::Operation(Call {
                file: shared_root,
                action: Action::Open { path: b"forbidden".to_vec(), mode: FILE_WRITE }
            }),
            &peer
        ),
        Err(Error::AccessDenied)
    );
    store.record_session_denial(&session, "fixture revocation").unwrap();
    assert_eq!(
        file_request(
            &mut engine,
            context,
            &mut operation,
            file_service::Request::Operation(Call { file: shared_root, action: Action::Status }),
            &peer
        ),
        Err(Error::AccessDenied)
    );
}

#[test]
fn package_requirements_reach_both_native_context_paths_before_guest_execution() {
    use crate::package::codec::{self, Source};
    let original = crate::engine::tests::button_package();
    let mut sources = BTreeMap::new();
    for entry in &original.index().entries {
        sources.insert(
            entry.path.clone(),
            Source::Bytes(original.read(&entry.path, 0, entry.length as usize).unwrap()),
        );
    }
    let manifest = serde_json::json!({
        "app_id": original.app_id(), "name": original.name(),
        "required_host_features": ["future.operation"],
    });
    sources.insert(codec::MANIFEST.into(), Source::Bytes(serde_json::to_vec(&manifest).unwrap()));
    let mut bytes = Vec::new();
    codec::write(&sources, &mut bytes).unwrap();
    let package = Package::from_bytes(&bytes).unwrap();
    let (directory, mut engine, admission, package) = fixture_package(package);
    let (_, context) = crate::engine::session_binding::Binding::prepare(
        &engine.services,
        &admission.session.id,
        &package,
    )
    .unwrap();
    assert!(context.required_capabilities.contains("future.operation"));
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let prepared = std::thread::scope(|scope| {
        let task = scope
            .spawn(|| Guest::prepare(directory.path(), admission, package, Arc::new(|| {}), None));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !task.is_finished() {
            assert!(Instant::now() < deadline, "package context preparation must finish");
            queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(10))).unwrap();
        }
        task.join().unwrap().unwrap()
    });
    let failure = prepared.instantiate().err().unwrap();
    assert!(failure.to_string().contains("Unsupported"), "{failure}");
}

#[test]
fn ipc_compilation_hands_off_the_selected_metering_mode() {
    // Each owner uses a running engine and the same finite package. Force a
    // metered cache miss through real IPC, then exercise the handed-off image.
    use crate::runtime::RuntimeError;
    let package = Package::new(
        "fuel".into(),
        "Local fuel".into(),
        wat::parse_str(
            r#"(module (memory (export "memory") 1)
              (func (export "pb_main") (local $remaining i32)
                i32.const 50000 local.set $remaining
                (loop $work local.get $remaining i32.const 1 i32.sub
                  local.tee $remaining br_if $work)))"#,
        )
        .unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    for limit in [std::num::NonZeroU64::new(1000), None] {
        let (directory, mut engine, admission, package) = fixture_package(package.clone());
        let listener = std::os::unix::net::UnixListener::bind(
            directory.path().join(crate::paths::ENGINE_SOCKET),
        )
        .unwrap();
        let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
        let prepared = std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                let mut prepared =
                    Guest::prepare(directory.path(), admission, package, Arc::new(|| {}), limit)
                        .unwrap();
                prepared.load_compiled().unwrap();
                prepared
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while !task.is_finished() {
                assert!(Instant::now() < deadline, "mode-specific compiler IPC must finish");
                queue
                    .run_until(&mut engine, Some(Instant::now() + Duration::from_millis(10)))
                    .unwrap();
            }
            task.join().unwrap()
        });
        assert_eq!(prepared.runtime.fuel_limit(), limit);
        assert_eq!(engine.runtime.fuel_limit(), None);
        let descriptor =
            &prepared.compiled.as_ref().expect("compiler handed off an image").descriptor;
        assert_eq!(descriptor.compatibility, prepared.runtime.compatibility());
        assert_eq!(descriptor.compatibility == engine.runtime.compatibility(), limit.is_none());
        assert!(
            engine.runtime.compiled_descriptor(descriptor.wasm, descriptor.compatibility).is_some()
        );
        let mut guest = prepared.instantiate().unwrap();
        if limit.is_some() {
            let failure = guest.pump().err().expect("limited owner exhausts its budget");
            assert!(matches!(
                failure.downcast_ref::<RuntimeError>(),
                Some(RuntimeError::FuelExhausted)
            ));
            let failure = guest.pump().err().expect("exhausted owner stays halted");
            assert!(matches!(
                failure.downcast_ref::<RuntimeError>(),
                Some(RuntimeError::GuestHalted)
            ));
        } else {
            for _ in 0..2 {
                assert!(guest.pump().unwrap().logs.is_empty());
            }
        }
    }
}

#[test]
fn lost_recovery_claim_reply_cannot_construct_again_in_that_incarnation() {
    use crate::engine::{ArchivedRequest, ipc::codec};
    use std::os::unix::net::UnixListener;
    let (directory, engine, admission, _package) = fixture();
    drop(engine);
    let listener = UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
    let next = Id::generate().unwrap();
    let instance = admission.instance.id;
    let session = admission.session.clone();
    let server = std::thread::spawn(move || {
        for index in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let bytes = ipc::read(&mut stream).unwrap();
            let archived = codec::access::<ArchivedRequest>(&bytes).unwrap();
            let request =
                rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>(archived).unwrap();
            if index == 2 {
                assert!(
                    matches!(request, Request::InstanceReconstruct { context, .. } if context.instance == instance)
                );
                // Ownership may be durable, but no reply reaches the host. A
                // second attempt must inspect and stop, never issue this again.
                continue;
            }
            let reply = if index == 1 || index == 4 {
                assert!(matches!(request, Request::PrepareSession { .. }));
                Reply::SessionPrepared(crate::engine::reply::PreparedSession {
                    session: session.clone(),
                    invitation: String::new(),
                    availability: String::new(),
                    update: None,
                })
            } else {
                assert!(matches!(request, Request::InstanceInspect { request: None }));
                Reply::InstanceStatus { incarnation: next, instance: None }
            };
            let bytes = codec::encode(&reply, 4096).unwrap();
            ipc::write(&mut stream, &bytes).unwrap();
        }
    });
    let mut attempted = (admission.context.incarnation, admission.instance.build.clone());
    assert!(
        Guest::recover(directory.path(), &admission, &mut attempted, Arc::new(|| {}), None, None)
            .is_err()
    );
    assert_eq!(attempted.0, next);
    let failure =
        Guest::recover(directory.path(), &admission, &mut attempted, Arc::new(|| {}), None, None)
            .err()
            .unwrap();
    assert!(failure.to_string().contains("This engine incarnation cannot reconstruct"));
    server.join().unwrap();
}

#[test]
fn surviving_owner_reconstructs_only_after_engine_restart_with_fresh_authority() {
    use crate::engine::admission::AdmissionQueue;
    use std::{
        os::unix::net::UnixListener,
        sync::atomic::{AtomicBool, Ordering},
    };
    let (directory, engine, admission, _package) = fixture();
    let prior = admission.clone();
    let profile = directory.path().to_owned();
    let (ready, receive) = std::sync::mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    drop(engine);
    let engine_stop = stop.clone();
    let service = std::thread::spawn(move || {
        let mut engine = Engine::open(&profile).unwrap();
        assert!(!engine.session_instances.owns_context(prior.context));
        let (peer, _client) = UnixStream::pair().unwrap();
        assert!(
            engine
                .dispatch_instance(Request::InstanceStopReason { context: prior.context }, &peer)
                .is_err(),
            "an old-incarnation probe cannot authorize normal owner completion"
        );
        let owners = engine.session_instances.list(&prior.instance.session).unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].instance.state, crate::engine::session_instance::State::Unknown);
        assert!(owners[0].process.is_some(), "the survivor retains verified kernel identity");
        let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let mut queue = AdmissionQueue::new(listener, &engine).unwrap();
        ready.send(engine.session_instances.incarnation()).unwrap();
        while !engine_stop.load(Ordering::Acquire) {
            queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(50))).unwrap();
        }
    });
    let incarnation = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut attempted = (admission.context.incarnation, admission.instance.build.clone());
    let mut guest = Guest::recover(
        directory.path(),
        &admission,
        &mut attempted,
        Arc::new(|| {}),
        None,
        Some(super::super::service_events::StopReason::ContextUnavailable),
    )
    .unwrap();
    assert!(fresh_context(admission.context, guest.admission.context, false));
    assert_eq!(guest.admission.context.incarnation, incarnation);
    assert_eq!(guest.admission.instance.id, admission.instance.id);
    assert_eq!(guest.pump().unwrap().logs, vec!["turn"]);
    let current = guest.admission.clone();
    let failure =
        Guest::recover(directory.path(), &current, &mut attempted, Arc::new(|| {}), None, None)
            .err()
            .unwrap();
    assert!(failure.to_string().contains("This engine incarnation cannot reconstruct"));
    drop(guest);
    stop.store(true, Ordering::Release);
    service.join().unwrap();
}

#[test]
fn selected_build_reconstructs_once_and_preserves_session_mounts() {
    selected_build_reconstruction(
        super::super::service_events::StopReason::SelectedBuildChanged,
        false,
    );
}

#[test]
fn stale_selected_build_reconstructs_once_and_preserves_session_mounts() {
    selected_build_reconstruction(
        super::super::service_events::StopReason::ContextUnavailable,
        false,
    );
}

#[test]
fn hidden_stale_selected_build_reconstructs_once_and_preserves_session_mounts() {
    selected_build_reconstruction(
        super::super::service_events::StopReason::ContextUnavailable,
        true,
    );
}

fn selected_build_reconstruction(
    stop_reason: super::super::service_events::StopReason,
    hidden: bool,
) {
    // A selected-build reconstruction retains mounts and the local positive
    // limit: finite work without data initialization must trap and stay halted.
    use crate::engine::admission::AdmissionQueue;
    use crate::runtime::RuntimeError;
    use std::{
        os::unix::net::UnixListener,
        sync::atomic::{AtomicBool, Ordering},
    };
    let package = Package::new(
        "fuel".into(),
        "Local fuel".into(),
        wat::parse_str(
            r#"(module (memory (export "memory") 1)
              (func (export "pb_main") (local $remaining i32)
                i32.const 50000 local.set $remaining
                (loop $work local.get $remaining i32.const 1 i32.sub
                  local.tee $remaining br_if $work)))"#,
        )
        .unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    let candidate = package.renamed("Updated local guest").unwrap();
    let (directory, mut engine, prior, _) = fixture_package_extra(package, true);
    let extra = prior.instance.extra_volumes[0].volume;
    let package =
        crate::package::signature::testing::signed(&engine.services.store, candidate).unwrap();
    let target = engine.create_session(package, prior.instance.root).unwrap();
    engine.services.store.select_personal_volumes(&target.id, vec![]).unwrap();
    let mut selected = prior.session.clone();
    selected.selector = target.selector;
    selected.resolved_build_id = target.resolved_build_id.clone();
    engine.services.store.adopt_session(&prior.session, &selected).unwrap();
    if hidden {
        engine.services.store.forget_session(&prior.instance.session).unwrap();
        assert!(engine.session_instances.owns_context(prior.context));
        assert!(
            !engine
                .services
                .store
                .retained_sessions()
                .unwrap()
                .iter()
                .any(|session| session.id == prior.instance.session)
        );
    }
    assert_eq!(
        engine.instance_stop_reason(prior.context),
        crate::engine::service_events::StopReason::SelectedBuildChanged
    );
    // The real engine rejects the retired binding before its independent
    // event stream has supplied the selected-build classification.
    let (peer, _client) = UnixStream::pair().unwrap();
    let call = crate::runtime::host::DataCall {
        volume: *prior.instance.root.as_bytes(),
        operation: crate::runtime::host::DataOperation::SchemaRead { entry_id: [5; 15] },
    };
    let ticket = engine.begin_service(prior.context, 1, 0, call.clone(), &peer).unwrap();
    engine.maintain_service().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ticket.ready() {
        assert!(Instant::now() < deadline, "old selected-build call must settle");
        std::thread::sleep(Duration::from_millis(1));
    }
    let outcome = engine.service_executor.take(ticket).unwrap();
    assert!(matches!(
        outcome.take(prior.context, 1, 0, &call),
        Err(crate::runtime::syscall::HostError::StaleHandle)
    ));
    let profile = directory.path().to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let engine_stop = stop.clone();
    let (ready, receive) = std::sync::mpsc::sync_channel(1);
    let service = std::thread::spawn(move || {
        let listener = UnixListener::bind(profile.join(crate::paths::ENGINE_SOCKET)).unwrap();
        let mut queue = AdmissionQueue::new(listener, &engine).unwrap();
        ready.send(()).unwrap();
        while !engine_stop.load(Ordering::Acquire) {
            queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(50))).unwrap();
        }
    });
    receive.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut attempted = (prior.context.incarnation, prior.instance.build.clone());
    let fuel_limit = std::num::NonZeroU64::new(1000);
    let guest = Guest::recover(
        directory.path(),
        &prior,
        &mut attempted,
        Arc::new(|| {}),
        fuel_limit,
        Some(stop_reason),
    );
    let mut guest = match guest {
        Ok(guest) => guest,
        Err(error) => {
            stop.store(true, Ordering::Release);
            service.join().unwrap();
            panic!("selected build must recover: {error}");
        }
    };
    assert_eq!(guest.admission.context.incarnation, prior.context.incarnation);
    assert!(fresh_context(prior.context, guest.admission.context, true));
    assert_eq!(guest.admission.instance.id, prior.instance.id);
    assert_eq!(guest.admission.instance.build, target.resolved_build_id);
    assert_eq!(guest.admission.instance.extra_volumes, prior.instance.extra_volumes);
    assert!(guest.admission.volumes.iter().any(|volume| volume.id == *extra.as_bytes()));
    assert_eq!(guest.title(), "Updated local guest");
    let failure = guest.pump().err().expect("reconstructed owner retains its fuel limit");
    assert!(matches!(failure.downcast_ref::<RuntimeError>(), Some(RuntimeError::FuelExhausted)));
    let failure = guest.pump().err().expect("exhausted reconstructed owner stays halted");
    assert!(matches!(failure.downcast_ref::<RuntimeError>(), Some(RuntimeError::GuestHalted)));
    assert!(
        Guest::recover(
            directory.path(),
            &prior,
            &mut attempted,
            Arc::new(|| {}),
            fuel_limit,
            Some(super::super::service_events::StopReason::SelectedBuildChanged)
        )
        .is_err()
    );
    // A fresh attempt tracker cannot reuse the consumed old generation either.
    let mut another_attempt = (prior.context.incarnation, prior.instance.build.clone());
    assert!(
        Guest::recover(
            directory.path(),
            &prior,
            &mut another_attempt,
            Arc::new(|| {}),
            fuel_limit,
            Some(super::super::service_events::StopReason::SelectedBuildChanged)
        )
        .is_err()
    );
    drop(guest);
    stop.store(true, Ordering::Release);
    service.join().unwrap();
}

#[test]
fn reconstruction_keeps_ownership_until_accepted_service_work_is_terminal() {
    use crate::runtime::host::{DataCall, DataOperation};
    let (_directory, mut engine, prior, mut package) = fixture();
    let (peer, _client) = UnixStream::pair().unwrap();
    let ticket = engine
        .begin_service(
            prior.context,
            1,
            0,
            DataCall {
                volume: *prior.instance.root.as_bytes(),
                operation: DataOperation::SchemaRead { entry_id: [5; 15] },
            },
            &peer,
        )
        .unwrap();
    package = package.renamed("Changed selected build").unwrap();
    let target = engine.create_session(package, prior.instance.root).unwrap();
    let mut selected = prior.session.clone();
    selected.selector = target.selector;
    selected.resolved_build_id = target.resolved_build_id.clone();
    engine.services.store.adopt_session(&prior.session, &selected).unwrap();
    let request = Request::InstanceReconstruct {
        context: prior.context,
        target: Box::new(crate::engine::session_instance::ReconstructionTarget {
            incarnation: prior.context.incarnation,
            build: target.resolved_build_id,
        }),
    };
    let pending = engine.dispatch_instance(request.clone(), &peer).unwrap();
    assert!(matches!(pending, Reply::InstanceDrainPending { context } if context == prior.context));
    let encoded = crate::engine::ipc::codec::encode(&pending, 4096).unwrap();
    assert!(
        crate::engine::ipc::codec::access::<crate::engine::reply::ArchivedReply>(&encoded)
            .unwrap()
            .matches_request(&request)
    );
    assert!(engine.session_instances.owns_context(prior.context));
    engine.maintain_service().unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while !ticket.ready() {
        assert!(Instant::now() < limit);
        std::thread::yield_now();
    }
    let Reply::InstanceAdmission(next) = engine.dispatch_instance(request.clone(), &peer).unwrap()
    else {
        panic!("terminal prior work permits explicit reconstruction")
    };
    assert!(fresh_context(prior.context, next.context, true));
    assert!(!engine.session_instances.owns_context(prior.context));
    assert!(
        engine.dispatch_instance(request, &peer).is_err(),
        "the old generation is consumed once"
    );
    let fresh_call = engine
        .begin_service(
            next.context,
            1,
            0,
            DataCall {
                volume: *prior.instance.root.as_bytes(),
                operation: DataOperation::SchemaRead { entry_id: [5; 15] },
            },
            &peer,
        )
        .unwrap();
    assert!(matches!(
        engine.begin_service(
            prior.context,
            1,
            0,
            DataCall {
                volume: *prior.instance.root.as_bytes(),
                operation: DataOperation::SchemaRead { entry_id: [5; 15] },
            },
            &peer
        ),
        Err(crate::runtime::syscall::HostError::StaleHandle)
    ));
    // Sequence collection does not discard the old accepted operation's
    // terminal mailbox, ticket or separately retained resource credit.
    let old_result = engine.service_executor.take(ticket).unwrap();
    assert_eq!(old_result.context, prior.context.into());
    assert_eq!(old_result.operation, 1);
    drop(fresh_call);
    engine.maintain_service().unwrap();
}

fn fixture() -> (tempfile::TempDir, Engine, Admission, Package) {
    fixture_package(crate::engine::tests::button_package())
}
fn fixture_package(package: Package) -> (tempfile::TempDir, Engine, Admission, Package) {
    fixture_package_extra(package, false)
}
fn fixture_package_extra(
    package: Package,
    extra: bool,
) -> (tempfile::TempDir, Engine, Admission, Package) {
    fixture_package_access(package, extra, false, false)
}
fn fixture_package_access(
    package: Package,
    extra: bool,
    host: bool,
    restore_host_mounts: bool,
) -> (tempfile::TempDir, Engine, Admission, Package) {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root =
        crate::engine::tests::object_volume(&engine.services.store, "GUI owner fixture", false);
    let package =
        crate::package::signature::testing::signed(&engine.services.store, package).unwrap();
    let session = engine.create_session(package.clone(), root.id).unwrap();
    let extra_volumes = if extra {
        let personal = engine.services.store.create_volume("Personal", false).unwrap();
        let grants = vec![crate::store::PersonalVolumeGrant { volume: personal.id, write: true }];
        engine.services.store.select_personal_volumes(&session.id, grants.clone()).unwrap();
        grants
    } else {
        vec![]
    };
    let host_mounts = if host {
        let mount = directory.path().join("host-files");
        std::fs::create_dir(&mount).unwrap();
        vec![
            crate::engine::host_mount::HostMount::from_cli(
                &format!("{}:Documents:rw", mount.display()),
                directory.path(),
            )
            .unwrap(),
        ]
        .into_boxed_slice()
    } else {
        Box::default()
    };
    let (peer, _client) = UnixStream::pair().unwrap();
    let Reply::Instance(instance) = engine
        .dispatch_instance(
            Request::InstanceReserve {
                request: Id::generate().unwrap(),
                incarnation: engine.session_instances.incarnation(),
                session: session.id,
                extra_volumes: extra_volumes.into_boxed_slice(),
                host_mounts,
                restore_host_mounts,
            },
            &peer,
        )
        .unwrap()
    else {
        panic!("reservation")
    };
    let Reply::InstanceAdmission(admission) =
        engine.dispatch_instance(Request::InstanceClaim { instance: instance.id }, &peer).unwrap()
    else {
        panic!("claim")
    };
    (directory, engine, admission, package)
}

#[test]
fn ordered_mount_snapshots_narrow_rights_retire_slots_and_preserve_local_guest() {
    let (directory, mut engine, admission, package) =
        fixture_package_access(crate::engine::tests::button_package(), true, false, true);
    let extra = admission.instance.extra_volumes[0].volume;
    let binding = admission.instance.clone();
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let wake = engine.completion.wake.clone();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || wake.notify()),
        &mut queue,
        &mut engine,
    );
    guest.pump().unwrap();
    let initial = guest.guest.volume_handle(*extra.as_bytes()).unwrap();
    let root = guest.guest.volume_handle(*binding.root.as_bytes()).unwrap();
    let generation = guest.generation();
    fn adopt(
        queue: &mut crate::engine::admission::AdmissionQueue,
        engine: &mut Engine,
        guest: &mut Guest,
        done: impl Fn(&Guest) -> bool,
    ) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if guest.ready(Instant::now()) {
                guest.pump().unwrap();
            }
            if done(guest) {
                break;
            }
            assert!(Instant::now() < deadline);
            queue.run_until(engine, Some(deadline)).unwrap();
        }
    }
    // The event lane sends full snapshots, including removals after coalescing.
    engine
        .services
        .store
        .select_personal_volumes(
            &binding.session,
            vec![crate::store::PersonalVolumeGrant { volume: extra, write: false }],
        )
        .unwrap();
    adopt(&mut queue, &mut engine, &mut guest, |g| {
        g.admission.volumes.iter().any(|v| {
            v.id == *extra.as_bytes() && v.rights == crate::runtime::host::VolumeRights::READ
        })
    });
    assert_eq!(guest.guest.volume_handle(*extra.as_bytes()), Some(initial));
    assert_eq!(
        guest.admission.volumes.iter().find(|v| v.id == *extra.as_bytes()).unwrap().rights,
        crate::runtime::host::VolumeRights::READ
    );
    engine.services.store.select_personal_volumes(&binding.session, vec![]).unwrap();
    adopt(&mut queue, &mut engine, &mut guest, |g| {
        g.guest.volume_handle(*extra.as_bytes()).is_none()
    });
    assert!(guest.guest.volume_handle(*extra.as_bytes()).is_none());
    engine
        .services
        .store
        .select_personal_volumes(
            &binding.session,
            vec![crate::store::PersonalVolumeGrant { volume: extra, write: true }],
        )
        .unwrap();
    adopt(&mut queue, &mut engine, &mut guest, |g| {
        g.guest.volume_handle(*extra.as_bytes()).is_some_and(|handle| handle != initial)
    });
    assert_ne!(guest.guest.volume_handle(*extra.as_bytes()).unwrap(), initial);
    assert_eq!(guest.guest.volume_handle(*binding.root.as_bytes()), Some(root));
    assert_eq!(guest.generation(), generation);
    drop(guest);
    drop(queue);
    // A saved configuration uses this session's current consent when reopening,
    // retaining an allowed mount while narrowing write or removing it entirely.
    let mut prior = binding.id;
    for selected in [
        vec![crate::store::PersonalVolumeGrant { volume: extra, write: true }],
        vec![crate::store::PersonalVolumeGrant { volume: extra, write: false }],
        vec![],
    ] {
        engine.services.store.select_personal_volumes(&binding.session, selected.clone()).unwrap();
        engine.session_instances.owner_exited(prior).unwrap();
        let (peer, _client) = UnixStream::pair().unwrap();
        let Reply::Instance(restored) = engine
            .dispatch_instance(
                Request::InstanceRestore {
                    request: Id::generate().unwrap(),
                    incarnation: engine.session_instances.incarnation(),
                    instance: prior,
                },
                &peer,
            )
            .unwrap()
        else {
            panic!("stopped saved GUI configuration restores through current session consent")
        };
        assert_eq!(restored.extra_volumes, selected);
        assert_eq!(restored.session, binding.session);
        assert_eq!(restored.root, binding.root);
        assert_eq!(restored.build, binding.build);
        assert!(restored.restore_host_mounts);
        prior = restored.id;
    }
}

#[test]
fn idle_owner_is_ready_to_report_event_stream_failure_without_input() {
    let (directory, mut engine, admission, package) = fixture();
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let (wake, notified) = std::sync::mpsc::sync_channel(1);
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || {
            let _ = wake.try_send(());
        }),
        &mut queue,
        &mut engine,
    );
    guest.pump().unwrap();
    assert!(!guest.ready(Instant::now()));
    assert!(!guest.accepted_work_pending());
    while notified.try_recv().is_ok() {}
    drop(queue); // Engine/event-owner loss, without any outstanding data call.
    notified.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(guest.ready(Instant::now()));
    assert!(guest.failure().is_some());
    assert!(guest.pump().is_err());
}

#[test]
fn local_guest_yields_and_receives_a_real_engine_syscall_completion() {
    // Independent scalar Wasm consumer: begin an empty-root schema read, yield,
    // drain its ready event and consume the retained snapshot on a later turn.
    let wasm = wat::parse_str(
        r#"(module
        (import "playbit" "pb_syscall"
            (func $sys (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (memory (export "memory") 1)
        (global $stage (mut i32) (i32.const 0))
        (global $pending (mut i64) (i64.const 0))
        (data (i32.const 528) "\05\05\05\05\05\05\05\05\05\05\05\05\05\05\05")
        (data (i32.const 4096) "began")
        (data (i32.const 4112) "completed")
        (func $drain
            (i32.store (i32.const 1024) (i32.const 24))
            (i32.store (i32.const 1028) (i32.const 0))
            (loop $events
                (if (i64.eq (call $sys (i32.const 4) (i64.const 1024)
                    (i64.const 1056) (i64.const 1024) (i64.const 0)
                    (i64.const 0) (i64.const 0)) (i64.const -6))
                    (then return))
                (br $events)))
        (func (export "pb_main") (local $status i64)
            (call $drain)
            (if (i32.eqz (global.get $stage)) (then
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
                (global.set $pending (call $sys (i32.const 258)
                    (i64.const 512)
                    (i64.const 0) (i64.const 0) (i64.const 0)
                    (i64.const 0) (i64.const 0)))
                (if (i64.le_s (global.get $pending) (i64.const 0)) (then unreachable))
                (global.set $stage (i32.const 1))
                (drop (call $sys (i32.const 2) (i64.const 4096) (i64.const 5)
                    (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
                return))
            (if (i32.eq (global.get $stage) (i32.const 1)) (then
                (i32.store (i32.const 256) (i32.const 52))
                (i32.store (i32.const 276) (i32.const 48))
                (i32.store (i32.const 292) (i32.const 56))
                (local.set $status (call $sys (i32.const 259)
                    (global.get $pending) (i64.const 256)
                    (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
                (if (i64.eq (local.get $status) (i64.const -6)) (then return))
                (if (i64.ne (local.get $status) (i64.const 0)) (then unreachable))
                (global.set $stage (i32.const 2))
                (drop (call $sys (i32.const 2) (i64.const 4112) (i64.const 9)
                    (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))))))"#,
    )
    .unwrap();
    let package =
        Package::new("test".into(), "Async GUI owner fixture".into(), wasm, BTreeMap::new())
            .unwrap();
    let (directory, mut engine, admission, package) = fixture_package(package);
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    // This test owns both loops on one thread. Completion wakes that loop;
    // production wakes the separate GUI owner through its own bounded handle.
    let wake = engine.completion.wake.clone();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || wake.notify()),
        &mut queue,
        &mut engine,
    );
    assert_eq!(guest.pump().unwrap().logs, vec!["began"]);
    assert!(guest.accepted_work_pending());
    assert!(!guest.ready(Instant::now()));
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let mut logs = Vec::new();
    guest.deactivate();
    assert_eq!(
        guest.input(Event { kind: EventKind::Click, target: 1, data: vec![] }),
        Err(HostError::Conflict)
    );
    while guest.accepted_work_pending() {
        assert!(Instant::now() < deadline, "the readiness-driven exchange must complete");
        queue.run_until(&mut engine, Some(deadline)).unwrap();
        if guest.ready(Instant::now()) {
            logs.extend(guest.pump().unwrap().logs);
        }
    }
    assert_eq!(logs, vec!["completed"]);
    assert!(!guest.drain_failed());
    assert!(!guest.ready(Instant::now()));
}

#[test]
fn guest_execution_and_input_do_not_create_an_engine_vm() {
    let (directory, mut engine, admission, package) = fixture();
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(|| {}),
        &mut queue,
        &mut engine,
    );
    let first = guest.pump().unwrap();
    assert!(first.view.is_some());
    assert_eq!(first.logs, vec!["turn"]);
    assert!(!guest.ready(Instant::now()));
    assert!(!guest.accepted_work_pending());
    assert!(guest.input(Event { kind: EventKind::Click, target: 999, data: vec![] }).is_err());
    guest.input(Event { kind: EventKind::Click, target: 1, data: vec![] }).unwrap();
    assert!(guest.ready(Instant::now()));
    let frame = guest.pump().unwrap();
    assert!(frame.view.is_some());
    assert_eq!(frame.logs, vec!["turn"]);
    assert_ne!(guest.admission().instance.id, guest.admission().session.root_volume);
}

#[test]
fn claim_binding_and_exact_package_are_required_before_guest_execution() {
    let (directory, _engine, admission, package) = fixture();
    let mut wrong_instance = admission.clone();
    wrong_instance.context.instance = Id::generate().unwrap();
    assert!(construct(directory.path(), wrong_instance, package.clone(), Arc::new(|| {})).is_err());
    let mut wrong_generation = admission.clone();
    wrong_generation.context.owner_generation += 1;
    assert!(
        construct(directory.path(), wrong_generation, package.clone(), Arc::new(|| {})).is_err()
    );
    let mut wrong_session = admission.clone();
    wrong_session.session.root_volume = Id::generate().unwrap();
    assert!(construct(directory.path(), wrong_session, package.clone(), Arc::new(|| {})).is_err());
    let mut different = package;
    different = different.renamed("A different build").unwrap();
    assert!(construct(directory.path(), admission, different, Arc::new(|| {})).is_err());
}

#[test]
fn package_loading_rejects_different_content_and_symlinks() {
    let (directory, _engine, admission, package) = fixture();
    let build = &admission.instance.build;
    assert_eq!(read_package(directory.path(), build).unwrap().id().unwrap(), *build);
    assert!(read_package(directory.path(), "../elsewhere").is_err());
    let path = directory.path().join(crate::paths::BUILDS).join(build);
    let mut different = package;
    different = different.renamed("Changed").unwrap();
    std::fs::write(&path, different.to_bytes().unwrap()).unwrap();
    assert!(read_package(directory.path(), build).is_err());
    let alternate = directory.path().join("alternate-package");
    std::fs::rename(&path, &alternate).unwrap();
    std::os::unix::fs::symlink(&alternate, &path).unwrap();
    assert!(read_package(directory.path(), build).is_err());
}

#[test]
fn consumed_readiness_without_consuming_result_reports_failed_drain() {
    let wasm = wat::parse_str(r#"(module
        (import "playbit" "pb_syscall" (func $sys (param i32 i64 i64 i64 i64 i64 i64) (result i64)))
        (memory (export "memory") 1)
        (global $began (mut i32) (i32.const 0))
        (global $pending (mut i64) (i64.const 0))
        (data (i32.const 528) "\05\05\05\05\05\05\05\05\05\05\05\05\05\05\05")
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
                (global.set $began (i32.const 1))))))"#).unwrap();
    let package =
        Package::new("test".into(), "Unconsumed completion".into(), wasm, BTreeMap::new()).unwrap();
    let (directory, mut engine, admission, package) = fixture_package(package);
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let wake = engine.completion.wake.clone();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || wake.notify()),
        &mut queue,
        &mut engine,
    );
    guest.pump().unwrap();
    guest.deactivate();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !guest.drain_failed() {
        assert!(Instant::now() < deadline, "unconsumed completion was not reported");
        queue.run_until(&mut engine, Some(deadline)).unwrap();
        if guest.ready(Instant::now()) {
            guest.pump().unwrap();
        }
    }
    assert!(guest.accepted_work_pending(), "failed drain is never reported as completed");
}

#[test]
fn rejected_first_claim_retires_reservation_across_engine_restart() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root = engine.services.store.create_volume("Launch race", false).unwrap();
    let mut package = crate::engine::tests::button_package();
    let session = engine.create_session(package.clone(), root.id).unwrap();
    let (peer, _client) = UnixStream::pair().unwrap();
    let Reply::Instance(instance) = engine
        .dispatch_instance(
            Request::InstanceReserve {
                restore_host_mounts: false,
                request: Id::generate().unwrap(),
                incarnation: engine.session_instances.incarnation(),
                session: session.id.clone(),
                extra_volumes: Box::default(),
                host_mounts: Box::default(),
            },
            &peer,
        )
        .unwrap()
    else {
        panic!("reservation")
    };
    package = package.renamed("New selection").unwrap();
    let target = engine.create_session(package, root.id).unwrap();
    let mut selected = session.clone();
    selected.selector = target.selector;
    selected.resolved_build_id = target.resolved_build_id;
    engine.services.store.adopt_session(&session, &selected).unwrap();
    assert!(
        engine.dispatch_instance(Request::InstanceClaim { instance: instance.id }, &peer).is_err()
    );
    assert_eq!(
        engine.session_instances.descriptor(instance.id).unwrap().state,
        crate::engine::session_instance::State::LaunchFailed
    );
    drop(engine);
    let mut engine = Engine::open(directory.path()).unwrap();
    assert!(engine.session_instances.list(&session.id).unwrap().is_empty());
    assert!(
        engine
            .dispatch_instance(
                Request::InstanceReserve {
                    restore_host_mounts: false,
                    request: Id::generate().unwrap(),
                    incarnation: engine.session_instances.incarnation(),
                    session: session.id,
                    extra_volumes: Box::default(),
                    host_mounts: Box::default(),
                },
                &peer
            )
            .is_ok()
    );
}

// Socket and namespace preparation is Send; instantiate the VM only on its
// eventual owner thread while the fixture services the engine's real queue.
fn create_serviced(
    profile: &Path,
    admission: Admission,
    package: Package,
    wake: Arc<dyn Fn() + Send + Sync>,
    queue: &mut crate::engine::admission::AdmissionQueue,
    engine: &mut Engine,
) -> Guest {
    let prepared = std::thread::scope(|scope| {
        let task = scope.spawn(|| Guest::prepare(profile, admission, package, wake, None));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !task.is_finished() {
            assert!(Instant::now() < deadline, "File root preparation must finish");
            queue.run_until(engine, Some(Instant::now() + Duration::from_millis(10))).unwrap();
        }
        task.join().unwrap().unwrap()
    });
    prepared.instantiate().unwrap()
}

#[test]
fn wasm_file_journey_discovers_real_roots_and_publishes_reads_and_lists() {
    file_journey(false, false, false);
}
#[test]
fn wasm_file_journey_discovers_cli_host_mount_and_publishes_reads_and_lists() {
    file_journey(true, false, false);
}
#[test]
fn wasm_file_journey_restores_gui_session_without_unavailable_host_mount() {
    file_journey(true, true, false);
}
#[test]
fn wasm_bundle_file_journey_reads_packaged_bytes_through_the_engine() {
    // The same independent guest discovers a bundle by backing type, reads and lists it.
    file_journey(false, false, true);
}
fn file_journey(host: bool, unavailable: bool, bundle: bool) {
    let source = include_str!("../../tests/fixtures/v1-volume-file.wat");
    let source = if bundle {
        source
            .replace("(i32.const 512)) (then", "(i32.const 768)) (then")
            .replace(
                "(i64.const 2048) (i64.const 4) (i64.const 4)",
                "(i64.const 2048) (i64.const 4) (i64.const 1)",
            )
            .replacen(
                "(global.set $stage (i32.const 1)) return))",
                "(global.set $stage (i32.const 4)) return))",
                1,
            )
    } else {
        source.to_owned()
    };
    let wasm = wat::parse_str(source).unwrap();
    let package = Package::new(
        "test".into(),
        "File journey".into(),
        wasm,
        BTreeMap::from([("note".into(), b"hello".to_vec())]),
    )
    .unwrap();
    let (directory, mut engine, admission, package) =
        fixture_package_access(package, false, host, unavailable);
    if unavailable {
        std::fs::remove_dir(directory.path().join("host-files")).unwrap();
        assert!(admission.instance.restore_host_mounts);
        assert_eq!(admission.instance.host_mounts.len(), 1);
    }
    let volume = admission.instance.root;
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let wake = engine.completion.wake.clone();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || wake.notify()),
        &mut queue,
        &mut engine,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if guest.ready(Instant::now())
            && guest.pump().unwrap().logs.contains(&"file journey complete".into())
        {
            break;
        }
        assert!(Instant::now() < deadline, "the File journey must complete through readiness");
        queue.run_until(&mut engine, Some(deadline)).unwrap();
    }
    let drain = guest.file_drain();
    drop(guest);
    while drain.files_executing() {
        assert!(Instant::now() < deadline, "destructor cleanup must retire");
        queue.run_until(&mut engine, Some(deadline)).unwrap();
    }
    let account = engine.services.store.account().id;
    drop(queue);
    drop(engine);
    if bundle {
        return;
    }
    if host && !unavailable {
        assert_eq!(std::fs::read(directory.path().join("host-files/note")).unwrap(), b"hello");
        return;
    }
    let store = crate::store::LocalStore::open(directory.path()).unwrap();
    let access = crate::store::Access::owner(account);
    let entry = store.resolve_entry(&access, volume, "note").unwrap();
    assert_eq!(store.read_blob_entry(&access, volume, entry.id).unwrap(), b"hello");
}

#[test]
fn explicitly_requested_cli_mount_fails_if_path_disappears_before_admission() {
    let package = crate::engine::tests::button_package();
    let (directory, mut engine, admission, package) =
        fixture_package_access(package, false, true, false);
    std::fs::remove_dir(directory.path().join("host-files")).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let result = std::thread::scope(|scope| {
        let task = scope
            .spawn(|| Guest::prepare(directory.path(), admission, package, Arc::new(|| {}), None));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !task.is_finished() {
            assert!(Instant::now() < deadline, "missing CLI root must terminate admission");
            queue.run_until(&mut engine, Some(Instant::now() + Duration::from_millis(10))).unwrap();
        }
        task.join().unwrap()
    });
    assert!(result.err().unwrap().to_string().contains("NotFound"));
}

#[test]
fn unavailable_gui_folder_can_be_repaired_and_removed_in_the_running_wasm_instance() {
    let (directory, mut engine, admission, package) =
        fixture_package_access(crate::engine::tests::button_package(), false, true, true);
    let binding = admission.instance.clone();
    let mount = binding.host_mounts[0].clone();
    std::fs::remove_dir(mount.path()).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET))
            .unwrap();
    let mut queue = crate::engine::admission::AdmissionQueue::new(listener, &engine).unwrap();
    let wake = engine.completion.wake.clone();
    let mut guest = create_serviced(
        directory.path(),
        admission,
        package,
        Arc::new(move || wake.notify()),
        &mut queue,
        &mut engine,
    );
    guest.pump().unwrap();
    assert!(guest.guest.volume_handle(*mount.id.as_bytes()).is_none());
    let generation = guest.generation();
    let root = guest.guest.volume_handle(*binding.root.as_bytes()).unwrap();
    let revision = engine.services.store.delivery_revision();
    std::fs::create_dir(mount.path()).unwrap();
    let (_client, server) = UnixStream::pair().unwrap();
    let Reply::Instance(repaired) = engine
        .dispatch_instance(
            Request::InstanceMounts {
                instance: binding.id,
                expected: binding.host_mounts.clone(),
                mounts: vec![
                    crate::engine::host_mount::HostMount::resolve(
                        &mount.path(),
                        Some(&mount.label),
                        mount.write,
                    )
                    .unwrap(),
                ]
                .into(),
            },
            &server,
        )
        .unwrap()
    else {
        panic!("saved repair")
    };
    let fresh = repaired.host_mounts[0].id;
    assert_ne!(fresh, mount.id);
    let mut adopt = |engine: &mut Engine, guest: &mut Guest, present: bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while guest.guest.volume_handle(*fresh.as_bytes()).is_some() != present {
            assert!(Instant::now() < deadline, "mount change wakes a running guest");
            queue.run_until(engine, Some(deadline)).unwrap();
            if guest.ready(Instant::now()) {
                guest.pump().unwrap();
            }
        }
    };
    adopt(&mut engine, &mut guest, true);
    engine
        .dispatch_instance(
            Request::InstanceMounts {
                instance: binding.id,
                expected: repaired.host_mounts,
                mounts: Box::default(),
            },
            &server,
        )
        .unwrap();
    adopt(&mut engine, &mut guest, false);
    assert_eq!(engine.services.store.delivery_revision(), revision);
    assert_eq!(guest.generation(), generation);
    assert_eq!(guest.guest.volume_handle(*binding.root.as_bytes()), Some(root));
    guest.input(Event { kind: EventKind::Click, target: 1, data: vec![] }).unwrap();
    assert!(guest.pump().unwrap().view.is_some());
}

#[test]
fn stale_selected_build_can_wait_for_preparation_without_consuming_admission() {
    // A provisional context refusal must still reach preparation of a changed
    // selected build. A transfer reply must remain retryable, not retire owner.
    use crate::engine::{ArchivedRequest, ipc::codec};
    use std::{
        os::unix::net::UnixListener,
        sync::atomic::{AtomicBool, Ordering},
    };
    let (directory, mut engine, prior, package) = fixture();
    let target = engine
        .create_session(package.renamed("Downloading update").unwrap(), prior.instance.root)
        .unwrap();
    let mut selected = prior.session.clone();
    selected.selector = target.selector;
    selected.resolved_build_id = target.resolved_build_id;
    engine.services.store.adopt_session(&prior.session, &selected).unwrap();
    let reason = engine.instance_stop_reason(prior.context);
    let mut transfer = engine.transfer_status();
    transfer.kind = crate::engine::reply::TransferKind::Running;
    let listener = UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let stopping = stopped.clone();
    let incarnation = prior.context.incarnation;
    let context = prior.context;
    let server = std::thread::spawn(move || {
        let mut prepared = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stopping.load(Ordering::Acquire) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "bounded preparation fixture");
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let bytes = ipc::read(&mut stream).unwrap();
            let request = rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>(
                codec::access::<ArchivedRequest>(&bytes).unwrap(),
            )
            .unwrap();
            let reply = match request {
                Request::InstanceInspect { request: None } => {
                    Reply::InstanceStatus { incarnation, instance: None }
                }
                Request::InstanceStopReason { context: requested } => {
                    assert_eq!(requested, context);
                    Reply::InstanceStopReason { context, incarnation, reason }
                }
                Request::PrepareSession { .. } => {
                    prepared = true;
                    Reply::Transfer { transfer: transfer.clone() }
                }
                _ => panic!("unexpected reconstruction request"),
            };
            ipc::write(&mut stream, &codec::encode(&reply, codec::MAX_ARCHIVE).unwrap()).unwrap();
            if prepared {
                break;
            }
        }
        prepared
    });
    let original = (prior.context.incarnation, prior.instance.build.clone());
    let mut attempted = original.clone();
    let failure = Guest::recover(
        directory.path(),
        &prior,
        &mut attempted,
        Arc::new(|| {}),
        None,
        Some(crate::engine::service_events::StopReason::ContextUnavailable),
    )
    .err()
    .unwrap();
    stopped.store(true, Ordering::Release);
    let prepared = server.join().unwrap();
    assert!(prepared, "selected-build preparation must run before terminal context classification");
    assert!(failure.to_string().contains("Unexpected prepared reconstruction reply"), "{failure}");
    assert!(!crate::engine::restart::terminal(&failure.to_string()), "Transfer remains retryable");
    assert_eq!(attempted, original, "Transfer does not consume a reconstruction target");
}

#[test]
fn stopped_owner_probe_checks_exact_context_peer_and_reply_binding() {
    // Classification has no data capability. The original kernel owner and
    // every context field must match before its bounded metadata can be read.
    use crate::engine::{ipc::codec, reply::ArchivedReply, service_events::StopReason};
    use std::{
        os::unix::net::UnixListener,
        process::{Command, Stdio},
    };
    let (directory, mut engine, admission, _package) = fixture();
    let context = admission.context;
    let (peer, _client) = UnixStream::pair().unwrap();
    let request = Request::InstanceStopReason { context };
    assert!(
        engine.dispatch_reply(request.clone()).is_err(),
        "classification requires process admission"
    );
    let reply = engine.dispatch_instance(request.clone(), &peer).unwrap();
    assert!(matches!(reply, Reply::InstanceStopReason {
        context: found, incarnation, reason: StopReason::ContextUnavailable,
    } if found == context && incarnation == context.incarnation));
    let bytes = codec::encode(&reply, 4096).unwrap();
    assert!(codec::access::<ArchivedReply>(&bytes).unwrap().matches_request(&request));
    for wrong in [
        Context { instance: Id::generate().unwrap(), ..context },
        Context { handle: Id::generate().unwrap(), ..context },
        Context { incarnation: Id::generate().unwrap(), ..context },
        Context { owner_generation: context.owner_generation + 1, ..context },
    ] {
        assert!(
            engine
                .dispatch_instance(Request::InstanceStopReason { context: wrong }, &peer)
                .is_err()
        );
        let unbound = Reply::InstanceStopReason {
            context: wrong,
            incarnation: context.incarnation,
            reason: StopReason::ContextUnavailable,
        };
        let bytes = codec::encode(&unbound, 4096).unwrap();
        assert!(!codec::access::<ArchivedReply>(&bytes).unwrap().matches_request(&request));
    }
    let socket = directory.path().join("owner.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = Command::new("/usr/bin/python3")
        .args(["-c", "import socket, sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); sys.stdin.buffer.read(1)"])
        .arg(&socket).stdin(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let stranger = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "other owner must connect");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let rejected = engine.dispatch_instance(request, &stranger).is_err();
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "other-owner fixture must exit");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(rejected, "a different OS process cannot probe this owner");
    assert!(engine.session_instances.owns_context(context));
    assert!(!engine.service_executor.context_pending(context));
}

#[test]
fn stopped_owner_probe_keeps_revoked_hidden_and_visible_sessions_unopened() {
    // Exact registry ownership survives denial. Neither classification nor a
    // nonfatal stop should unhide a forgotten session or admit its revoked data.
    use crate::engine::service_events::StopReason;
    for hidden in [false, true] {
        let (_directory, mut engine, admission, _package) = fixture();
        let context = admission.context;
        if hidden {
            engine.services.store.forget_session(&admission.instance.session).unwrap();
        }
        engine
            .services
            .store
            .record_session_denial(&admission.session, "fixture revocation")
            .unwrap();
        assert!(engine.instance_volumes(context).is_err());
        let before = engine.services.store.retained_sessions().unwrap();
        let revision = engine.services.store.delivery_revision();
        let (peer, _client) = UnixStream::pair().unwrap();
        let reply =
            engine.dispatch_instance(Request::InstanceStopReason { context }, &peer).unwrap();
        assert!(matches!(reply, Reply::InstanceStopReason {
            context: found, incarnation, reason: StopReason::ContextUnavailable,
        } if found == context && incarnation == context.incarnation));
        assert_eq!(engine.services.store.retained_sessions().unwrap(), before);
        assert_eq!(engine.services.store.delivery_revision(), revision);
        assert!(engine.instance_volumes(context).is_err());
        assert!(engine.session_instances.owns_context(context));
        assert!(!engine.service_executor.context_pending(context));
        assert!(engine.background.is_none());
    }
}

#[test]
fn engine_change_during_stop_probe_never_completes_the_old_owner() {
    // The engine can restart after InstanceInspect. A context-bound reply with
    // a different current incarnation is still a recovery failure, not proof of
    // terminal revocation; no preparation or reconstruction target is consumed.
    use crate::engine::{ArchivedRequest, ipc::codec, service_events::StopReason};
    use std::os::unix::net::UnixListener;
    for reason in [StopReason::ContextUnavailable, StopReason::SelectedBuildChanged] {
        let (directory, _engine, admission, _package) = fixture();
        let context = admission.context;
        let next = Id::generate().unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "probe race must progress");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let bytes = ipc::read(&mut stream).unwrap();
                let request = rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>(
                    codec::access::<ArchivedRequest>(&bytes).unwrap(),
                )
                .unwrap();
                let reply = if index == 0 {
                    assert!(matches!(request, Request::InstanceInspect { request: None }));
                    Reply::InstanceStatus { incarnation: context.incarnation, instance: None }
                } else {
                    assert!(
                        matches!(request, Request::InstanceStopReason { context: requested } if requested == context)
                    );
                    Reply::InstanceStopReason { context, incarnation: next, reason }
                };
                ipc::write(&mut stream, &codec::encode(&reply, 4096).unwrap()).unwrap();
            }
            listener
        });
        let original = (context.incarnation, admission.instance.build.clone());
        let mut attempted = original.clone();
        let failure = Guest::recover(
            directory.path(),
            &admission,
            &mut attempted,
            Arc::new(|| {}),
            None,
            Some(StopReason::ContextUnavailable),
        )
        .err()
        .unwrap();
        let listener = server.join().unwrap();
        assert!(
            failure.to_string().contains("engine changed during app-stop classification"),
            "{failure}"
        );
        assert!(failure.downcast_ref::<RecoveryError>().is_none());
        assert_eq!(attempted, original);
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}
