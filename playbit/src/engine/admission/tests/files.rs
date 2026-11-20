//! Real authenticated socket exchanges, including filesystem worker completion.
use super::*;
use crate::{
    engine::{
        file_service,
        host_mount::HostMount,
        session_instance::{Context, SessionInstance, State as InstanceState},
    },
    runtime::host::{
        HostResult,
        files::{Action, Call, Output},
    },
    store::Id,
};
use playbit_api::abi::{files::*, syscall::Error};

fn claim(f: &mut Fixture, write: bool) -> Context {
    claim_personal(f, write, false)
}
fn claim_personal(f: &mut Fixture, write: bool, personal: bool) -> Context {
    claim_config(f, write, personal, false)
}
fn claim_config(
    f: &mut Fixture,
    write: bool,
    personal: bool,
    restore_host_mounts: bool,
) -> Context {
    let root = crate::engine::tests::object_volume(&f.engine.services.store, "Files", false);
    let session = f.engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
    let extra_volumes = if personal {
        let volume = f.engine.services.store.create_volume("Personal files", false).unwrap().id;
        let grants = vec![crate::store::PersonalVolumeGrant { volume, write: true }];
        f.engine.services.store.select_personal_volumes(&session.id, grants.clone()).unwrap();
        grants
    } else {
        vec![]
    };
    let mount = f._directory.path().join("mount");
    std::fs::create_dir_all(&mount).unwrap();
    // Register the trusted launch descriptor directly: GUI mount selection is a
    // separate admission caller. Every File request below uses real peer auth.
    let instance = f
        .engine
        .session_instances
        .reserve(
            f.engine.session_instances.incarnation(),
            SessionInstance {
                restore_host_mounts,
                id: Id::from_bytes([0; 16]),
                launch_request: Id::generate().unwrap(),
                account: f.engine.services.store.account().id,
                session: session.id,
                build: session.resolved_build_id,
                root: session.root_volume,

                extra_volumes,
                host_mounts: vec![HostMount::resolve(&mount, Some("Files"), write).unwrap()].into(),
                owner_generation: 0,
                state: InstanceState::Reserved,
            },
        )
        .unwrap();
    let Reply::InstanceAdmission(admission) =
        f.exchange(Request::InstanceClaim { instance: instance.id })
    else {
        panic!("claimed instance")
    };
    admission.context
}

fn claim_automatic(f: &mut Fixture) -> (Context, crate::store::Session, Id, Id) {
    claim_automatic_config(f, None, false, Box::default())
}

fn claim_automatic_config(
    f: &mut Fixture,
    personal: Option<Id>,
    sync_shared: bool,
    host_mounts: Box<[HostMount]>,
) -> (Context, crate::store::Session, Id, Id) {
    use crate::package::{AutomaticVolumeRole as Role, Package, VolumeInitializers};
    use crate::store::{BuildSelector, SessionCreation};
    let store = &f.engine.services.store;
    let initializers = VolumeInitializers::from_json(include_bytes!(
        "../../../../../guests/todo/playbit-volumes.json"
    ))
    .unwrap();
    let package = crate::package::signature::testing::signed(
        store,
        Package::create(
            "automatic-files".into(),
            "Automatic files".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            Default::default(),
            &initializers,
        )
        .unwrap(),
    )
    .unwrap();
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let builds = store.profile_path().join(crate::paths::BUILDS);
    std::fs::create_dir_all(&builds).unwrap();
    std::fs::write(builds.join(&build), package.to_bytes().unwrap()).unwrap();
    let root = app.volume_id(Role::Root, None, None, None).unwrap();
    let session = store
        .create_session_binding(SessionCreation {
            app,
            selector: BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            root_volume: root,
            sync_shared,
            user_volume: None,
        })
        .unwrap();
    let shared = store.automatic_volume_id(&session.app, Some(&session.id), Role::Session).unwrap();
    let user = store.automatic_volume_id(&session.app, Some(&session.id), Role::User).unwrap();
    let grants = personal
        .map(|volume| vec![crate::store::PersonalVolumeGrant { volume, write: true }])
        .unwrap_or_default();
    if personal.is_some() {
        store.select_personal_volumes(&session.id, grants.clone()).unwrap();
    }
    let Reply::Instance(instance) = f.exchange(Request::InstanceReserve {
        restore_host_mounts: false,
        request: Id::generate().unwrap(),
        incarnation: f.engine.session_instances.incarnation(),
        session: session.id.clone(),
        extra_volumes: grants.into_boxed_slice(),
        host_mounts,
    }) else {
        panic!("automatic instance")
    };
    let Reply::InstanceAdmission(admission) =
        f.exchange(Request::InstanceClaim { instance: instance.id })
    else {
        panic!("automatic admission")
    };
    (admission.context, session, shared, user)
}

#[test]
fn automatic_file_close_keeps_original_authority_across_code_selection() {
    use crate::{package::Package, store::object_contract::EntryAcl};
    #[derive(Clone, Copy)]
    enum Revocation {
        None,
        Session,
        Entry,
        Consent,
    }
    for revocation in
        [Revocation::None, Revocation::Session, Revocation::Entry, Revocation::Consent]
    {
        let mut f = Fixture::new();
        let personal = matches!(revocation, Revocation::Consent)
            .then(|| f.engine.services.store.create_volume("Personal", false).unwrap().id);
        let (context, session, shared, _) =
            claim_automatic_config(&mut f, personal, false, Box::default());
        let store = f.engine.services.store.clone();
        let owner = crate::store::Access::owner(store.account().id);
        let mut sequence = 0;
        let volume = personal.unwrap_or(shared);
        let root = file(
            exchange(&mut f, context, &mut sequence, file_service::Request::VolumeRoot { volume })
                .unwrap(),
        );
        call(&mut f, context, &mut sequence, root, Action::Status).unwrap();
        let entry = store.create_blob(&owner, volume, "committed", b"original").unwrap();
        let writer = file(
            call(
                &mut f,
                context,
                &mut sequence,
                root,
                Action::Open { path: b"committed".to_vec(), mode: FILE_WRITE },
            )
            .unwrap(),
        );
        call(
            &mut f,
            context,
            &mut sequence,
            writer,
            Action::Write { offset: 0, bytes: b"accepted once".to_vec() },
        )
        .unwrap();
        let previous = Package::open(
            &store.profile_path().join(crate::paths::BUILDS).join(&session.resolved_build_id),
        )
        .unwrap();
        let next = crate::package::signature::testing::signed(
            &store,
            Package::create(
                previous.app_id().into(),
                "Updated automatic files".into(),
                previous.wasm().unwrap(),
                Default::default(),
                previous.initializers(),
            )
            .unwrap(),
        )
        .unwrap();
        let build = next.id().unwrap();
        std::fs::write(
            store.profile_path().join(crate::paths::BUILDS).join(&build),
            next.to_bytes().unwrap(),
        )
        .unwrap();
        let request = file_service::Request::Operation(Call {
            file: writer,
            action: Action::Close { flags: 0 },
        });
        let (peer, _client) = UnixStream::pair().unwrap();
        sequence += 1;
        let close_sequence = sequence;
        let close = f.engine.begin_file(context, sequence, request.clone(), &peer).unwrap();
        // The close owns its destination and bytes before another build is
        // selected; no File worker has dispatched it yet.
        store.select_development_build(&session, &build).unwrap();
        match revocation {
            Revocation::None => {}
            Revocation::Session => {
                store
                    .record_session_denial(&store.session(&session.id).unwrap(), "Removed")
                    .unwrap();
            }
            Revocation::Entry => store
                .set_entry_permissions(
                    &owner,
                    volume,
                    entry.id,
                    EntryAcl::Replace(Default::default()),
                )
                .unwrap(),
            Revocation::Consent => {
                let grants = store.personal_volume_grants(&session.id).unwrap().unwrap();
                store.select_personal_volumes(&session.id, vec![]).unwrap();
                store.select_personal_volumes(&session.id, grants).unwrap();
            }
        }
        assert_eq!(
            call(
                &mut f,
                context,
                &mut sequence,
                root,
                Action::Open { path: b"new-access".to_vec(), mode: FILE_WRITE }
            ),
            Err(Error::StaleHandle)
        );
        let stale = crate::runtime::host::DataCall {
            volume: *shared.as_bytes(),
            operation: crate::runtime::host::DataOperation::ResolveEntry { name: "data".into() },
        };
        assert!(matches!(
            f.engine.begin_service(context, 1, 0, stale, &peer),
            Err(Error::StaleHandle)
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !close.ready() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline, "accepted local File close drains");
            std::thread::yield_now();
        }
        let output = f
            .engine
            .file_service
            .as_mut()
            .unwrap()
            .take(&close, store.delivery_revision())
            .unwrap()
            .take(context, close_sequence, &request)
            .unwrap();
        let permitted = matches!(revocation, Revocation::None);
        assert_eq!(
            output,
            Output::Close {
                error: if permitted { 0 } else { Error::AccessDenied as i32 },
                publication: if permitted {
                    crate::runtime::host::files::PUBLISHED
                } else {
                    crate::runtime::host::files::NOT_PUBLISHED
                },
                recovery: None,
            }
        );
        if matches!(revocation, Revocation::Entry) {
            store.set_entry_permissions(&owner, volume, entry.id, EntryAcl::Inherit).unwrap();
        }
        assert_eq!(
            store.read_blob_entry(&owner, volume, entry.id).unwrap(),
            if permitted { b"accepted once".as_slice() } else { b"original".as_slice() }
        );
        assert!(call(&mut f, context, &mut sequence, writer, Action::Close { flags: 0 }).is_err());
        assert_eq!(
            store.read_blob_entry(&owner, volume, entry.id).unwrap(),
            if permitted { b"accepted once".as_slice() } else { b"original".as_slice() }
        );
    }
}

#[test]
fn automatic_file_use_initializes_and_keeps_current_scope_authority() {
    let mut f = Fixture::new();
    let (context, session, shared, user) = claim_automatic(&mut f);
    let store = f.engine.services.store.clone();
    let owner = crate::store::Access::owner(store.account().id);
    let mut sequence = 0;
    let mut roots = Vec::new();
    for volume in [shared, user] {
        assert!(matches!(store.volume(volume), Err(crate::store::Error::NotFound(_))));
        let root = file(
            exchange(&mut f, context, &mut sequence, file_service::Request::VolumeRoot { volume })
                .unwrap(),
        );
        // Metadata-only root admission does not initialize an unused scope.
        call(&mut f, context, &mut sequence, root, Action::Status).unwrap();
        store.create_blob(&owner, volume, "after-opening", b"scope data").unwrap();
        f.engine.maintain_files().unwrap();
        assert!(call(&mut f, context, &mut sequence, root, Action::Status).is_ok());
        let reader = file(
            call(
                &mut f,
                context,
                &mut sequence,
                root,
                Action::Open { path: b"after-opening".to_vec(), mode: FILE_READ },
            )
            .unwrap(),
        );
        assert_eq!(
            call(&mut f, context, &mut sequence, reader, Action::Read { offset: 0, length: 64 })
                .unwrap(),
            Output::Read { bytes: b"scope data".to_vec() }
        );
        roots.push(root);
    }
    store.record_session_denial(&session, "Removed").unwrap();
    f.engine.maintain_files().unwrap();
    for root in roots {
        assert_eq!(
            call(&mut f, context, &mut sequence, root, Action::Status),
            Err(Error::AccessDenied)
        );
    }
}

#[test]
fn automatic_shared_changes_advance_process_event_data_revision() {
    use crate::package::AutomaticVolumeRole as Role;
    use crate::store::volume_index::EntryAddress;
    let mut f = Fixture::new();
    let (context, session, shared, _) = claim_automatic(&mut f);
    let store = f.engine.services.store.clone();
    let mut client = f.client();
    ipc::write(&mut client, &encode_request(&Request::InstanceEvents { context }).unwrap())
        .unwrap();
    for _ in 0..8 {
        f.turn();
    }
    let Reply::InstanceEvents(first) =
        ipc::Response::new(codec::read(&mut client, codec::MAX_ARCHIVE).unwrap())
            .unwrap()
            .into_owned()
            .unwrap()
    else {
        panic!("initial notice")
    };
    assert!(first.stop.is_none());
    store.open_automatic_volume(&session.id, Role::Session).unwrap();
    for _ in 0..8 {
        f.turn();
    }
    let Reply::InstanceEvents(initialized) =
        ipc::Response::new(codec::read(&mut client, codec::MAX_ARCHIVE).unwrap())
            .unwrap()
            .into_owned()
            .unwrap()
    else {
        panic!("Shared initialization notice")
    };
    assert!(initialized.data_revision > first.data_revision);
    let owner = crate::store::Access::owner(store.account().id);
    let entry = store.resolve_entry(&owner, shared, "data").unwrap();
    store
        .create_object(
            &owner,
            EntryAddress { volume: shared, entry: entry.id },
            Id::from_bytes([1; 16]),
        )
        .unwrap();
    for _ in 0..8 {
        f.turn();
    }
    let Reply::InstanceEvents(backing) =
        ipc::Response::new(codec::read(&mut client, codec::MAX_ARCHIVE).unwrap())
            .unwrap()
            .into_owned()
            .unwrap()
    else {
        panic!("Shared backing notice")
    };
    assert!(backing.data_revision > initialized.data_revision);
    assert_eq!(backing.volumes, initialized.volumes);

    let previous = crate::package::Package::open(
        &store.profile_path().join(crate::paths::BUILDS).join(&session.resolved_build_id),
    )
    .unwrap();
    let next = crate::package::signature::testing::signed(
        &store,
        crate::package::Package::create(
            previous.app_id().into(),
            "Updated event source".into(),
            previous.wasm().unwrap(),
            Default::default(),
            previous.initializers(),
        )
        .unwrap(),
    )
    .unwrap();
    let build = next.id().unwrap();
    std::fs::write(
        store.profile_path().join(crate::paths::BUILDS).join(&build),
        next.to_bytes().unwrap(),
    )
    .unwrap();
    store.select_development_build(&session, &build).unwrap();
    for _ in 0..8 {
        f.turn();
    }
    let Reply::InstanceEvents(selected) =
        ipc::Response::new(codec::read(&mut client, codec::MAX_ARCHIVE).unwrap())
            .unwrap()
            .into_owned()
            .unwrap()
    else {
        panic!("selected build notice")
    };
    assert_eq!(
        selected.stop,
        Some(crate::engine::service_events::StopReason::SelectedBuildChanged)
    );
    assert!(selected.volumes.is_empty());
    assert_ne!(selected.revision, backing.revision);
}

fn exchange(
    f: &mut Fixture,
    context: Context,
    sequence: &mut u64,
    request: file_service::Request,
) -> HostResult<Output> {
    *sequence += 1;
    let Reply::FileCall(outcome) = f.exchange(Request::FileCall {
        context,
        operation: *sequence,
        request: Box::new(request.clone()),
    }) else {
        panic!("correlated file outcome")
    };
    outcome.take(context, *sequence, &request)
}
fn host_root(f: &mut Fixture, context: Context, sequence: &mut u64) -> HostResult<Output> {
    let mount = f.engine.session_instances.descriptor(context.instance).unwrap().host_mounts[0].id;
    exchange(f, context, sequence, file_service::Request::HostRoot { mount })
}
fn call(
    f: &mut Fixture,
    context: Context,
    sequence: &mut u64,
    file: u64,
    action: Action,
) -> HostResult<Output> {
    exchange(f, context, sequence, file_service::Request::Operation(Call { file, action }))
}
fn file(output: Output) -> u64 {
    let Output::File { handle } = output else { panic!("file handle") };
    handle
}

#[test]
fn file_socket_journey_checks_mounts_handles_replays_and_close_publication() {
    let mut f = Fixture::new();
    let context = claim(&mut f, true);
    let mut sequence = 0;
    assert_eq!(
        exchange(
            &mut f,
            context,
            &mut sequence,
            file_service::Request::HostRoot { mount: Id::from_bytes([0; 16]) }
        ),
        Err(Error::InvalidArgument)
    );
    let root = file(host_root(&mut f, context, &mut sequence).unwrap());
    let request = file_service::Request::Operation(Call { file: root, action: Action::Status });
    let Reply::FileCall(outcome) = f.exchange(Request::FileCall {
        context,
        operation: sequence,
        request: Box::new(request.clone()),
    }) else {
        panic!("replay outcome")
    };
    assert_eq!(outcome.take(context, sequence, &request), Err(Error::StaleHandle));
    assert_eq!(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"../escape".to_vec(), mode: FILE_WRITE }
        ),
        Err(Error::InvalidArgument)
    );
    let writer = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"new".to_vec(), mode: FILE_WRITE },
        )
        .unwrap(),
    );
    assert_eq!(
        call(
            &mut f,
            context,
            &mut sequence,
            writer,
            Action::Write { offset: 2, bytes: b"data".to_vec() }
        ),
        Ok(Output::Written)
    );
    assert_eq!(
        call(
            &mut f,
            context,
            &mut sequence,
            writer,
            Action::Close { flags: FILE_CLOSE_SYNC | FILE_CLOSE_DISCARD }
        ),
        Err(Error::InvalidArgument)
    );
    assert_eq!(
        call(&mut f, context, &mut sequence, writer, Action::Close { flags: 0 }),
        Ok(Output::Close {
            error: 0,
            publication: crate::runtime::host::files::PUBLISHED,
            recovery: None
        })
    );
    assert_eq!(std::fs::read(f._directory.path().join("mount/new")).unwrap(), b"\0\0data");
    let reader = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"new".to_vec(), mode: FILE_READ },
        )
        .unwrap(),
    );
    let Output::Status { flags, .. } =
        call(&mut f, context, &mut sequence, reader, Action::Status).unwrap()
    else {
        panic!("reader status")
    };
    assert_ne!(
        flags & FILE_CONTENTS_IMMUTABLE,
        0,
        "qualification filesystem supports cloning into managed private storage"
    );
    std::fs::write(f._directory.path().join("mount/new"), b"changed source").unwrap();
    assert_eq!(
        call(&mut f, context, &mut sequence, reader, Action::Read { offset: 2, length: 4 }),
        Ok(Output::Read { bytes: b"data".to_vec() })
    );
    let Output::List { handle: list } =
        call(&mut f, context, &mut sequence, root, Action::ListOpen).unwrap()
    else {
        panic!("list")
    };
    assert_eq!(
        call(&mut f, context, &mut sequence, list, Action::ListNext { index: 0 }),
        Ok(Output::Entry { kind: FILE_KIND_FILE, name: b"new".to_vec() })
    );
    exchange(&mut f, context, &mut sequence, file_service::Request::ListClose { list }).unwrap();
    assert!(call(&mut f, context, &mut sequence, list, Action::ListNext { index: 0 }).is_err());
    f.engine.session_instances.owner_exited(context.instance).unwrap();
    let other = claim(&mut f, false);
    let mut other_sequence = 0;
    assert_eq!(
        call(&mut f, other, &mut other_sequence, reader, Action::Status),
        Err(Error::StaleHandle)
    );
    let read_only = file(host_root(&mut f, other, &mut other_sequence).unwrap());
    assert_eq!(
        call(
            &mut f,
            other,
            &mut other_sequence,
            read_only,
            Action::Open { path: b"denied".to_vec(), mode: FILE_WRITE }
        ),
        Err(Error::AccessDenied)
    );
    assert!(!f._directory.path().join("mount/denied").exists());
}

#[test]
fn file_reconstruction_drains_accepted_close_then_retires_old_capabilities() {
    let mut f = Fixture::new();
    let context = claim(&mut f, true);
    let mut sequence = 0;
    let root = file(host_root(&mut f, context, &mut sequence).unwrap());
    let writer = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"committed".to_vec(), mode: FILE_WRITE },
        )
        .unwrap(),
    );
    call(
        &mut f,
        context,
        &mut sequence,
        writer,
        Action::Write { offset: 0, bytes: b"accepted".to_vec() },
    )
    .unwrap();
    let queued = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"queued-before-reload".to_vec(), mode: FILE_WRITE },
        )
        .unwrap(),
    );
    let (peer, _client) = UnixStream::pair().unwrap();
    let original = f.engine.session_instances.descriptor(context.instance).unwrap();
    let invalid = Request::InstanceReconstruct {
        context,
        target: Box::new(crate::engine::session_instance::ReconstructionTarget {
            incarnation: context.incarnation,
            build: original.build,
        }),
    };
    assert!(f.engine.dispatch_instance(invalid, &peer).is_err());
    assert!(call(&mut f, context, &mut sequence, writer, Action::Status).is_ok());
    sequence += 1;
    let ticket = f
        .engine
        .begin_file(
            context,
            sequence,
            file_service::Request::Operation(Call {
                file: writer,
                action: Action::Close { flags: 0 },
            }),
            &peer,
        )
        .unwrap();
    // Deliberately do not run maintenance: close is accepted but not dispatched.
    let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
    let prior = f.engine.services.store.session(&binding.session).unwrap();
    let mut package = crate::engine::tests::button_package();
    package = package.renamed("File reconstruction target").unwrap();
    let target = f.engine.create_session(package, binding.root).unwrap();
    let mut selected = prior.clone();
    selected.selector = target.selector;
    selected.resolved_build_id = target.resolved_build_id.clone();
    f.engine.services.store.adopt_session(&prior, &selected).unwrap();
    let reconstruct_pending = Request::InstanceReconstruct {
        context,
        target: Box::new(crate::engine::session_instance::ReconstructionTarget {
            incarnation: context.incarnation,
            build: target.resolved_build_id.clone(),
        }),
    };
    assert!(matches!(
        f.engine.dispatch_instance(reconstruct_pending, &peer).unwrap(),
        Reply::InstanceDrainPending { .. }
    ));
    // These requests were accepted into the frontend FIFO before its selected-
    // build notice, but reached the engine afterward. Original authority still
    // permits draining writes and close; the old build cannot acquire access.
    assert_eq!(call(&mut f, context, &mut sequence, root, Action::Status), Err(Error::StaleHandle));
    assert_eq!(host_root(&mut f, context, &mut sequence), Err(Error::StaleHandle));
    call(
        &mut f,
        context,
        &mut sequence,
        queued,
        Action::Write { offset: 0, bytes: b"drained".to_vec() },
    )
    .unwrap();
    assert!(matches!(
        call(&mut f, context, &mut sequence, queued, Action::Close { flags: 0 }).unwrap(),
        Output::Close { error: 0, publication: crate::runtime::host::files::PUBLISHED, .. }
    ));
    assert_eq!(
        std::fs::read(f._directory.path().join("mount/queued-before-reload")).unwrap(),
        b"drained"
    );
    let reconstruct = || Request::InstanceReconstruct {
        context,
        target: Box::new(crate::engine::session_instance::ReconstructionTarget {
            incarnation: context.incarnation,
            build: target.resolved_build_id.clone(),
        }),
    };
    drop(ticket); // An abandoned reply does not cancel the accepted publication.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.engine.file_pending() {
        f.engine.maintain_service().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(std::fs::read(f._directory.path().join("mount/committed")).unwrap(), b"accepted");
    let admission = loop {
        match f.engine.dispatch_instance(reconstruct(), &peer).unwrap() {
            Reply::InstanceAdmission(admission) => break admission,
            Reply::InstanceDrainPending { .. } => {
                assert!(Instant::now() < deadline);
                f.engine.maintain_service().unwrap();
                std::thread::yield_now();
            }
            _ => panic!("reconstruction"),
        }
    };
    assert_ne!(admission.context.handle, context.handle);
    assert_eq!(call(&mut f, context, &mut sequence, root, Action::Status), Err(Error::StaleHandle));
    let mut fresh_sequence = 0;
    assert_eq!(
        call(&mut f, admission.context, &mut fresh_sequence, root, Action::Status),
        Err(Error::StaleHandle)
    );
}

#[test]
fn queued_volume_root_delivery_checks_revocation_and_exact_owner() {
    // A completed metadata-only root has not been delivered yet. Losing its
    // grant or exact owner must withhold the handle, despite no data fence.
    for retire in [false, true] {
        let mut f = Fixture::new();
        let context = claim(&mut f, true);
        let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
        let request = file_service::Request::VolumeRoot { volume: binding.root };
        let (peer, _client) = UnixStream::pair().unwrap();
        let wrong = Context { incarnation: Id::generate().unwrap(), ..context };
        assert!(matches!(
            f.engine.begin_file(wrong, 1, request.clone(), &peer),
            Err(Error::StaleHandle)
        ));
        let ticket = f.engine.begin_file(context, 1, request.clone(), &peer).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ticket.ready() {
            f.engine.maintain_service().unwrap();
            assert!(Instant::now() < deadline, "queued root must complete");
            std::thread::yield_now();
        }
        if retire {
            f.engine.session_instances.owner_exited(context.instance).unwrap();
        } else {
            let session = f.engine.services.store.session(&binding.session).unwrap();
            f.engine.services.store.record_session_denial(&session, "revoked").unwrap();
        }
        f.engine.maintain_service().unwrap();
        let expected = if retire { Error::StaleHandle } else { Error::AccessDenied };
        assert_eq!(
            f.engine
                .file_service
                .as_mut()
                .unwrap()
                .take(&ticket, f.engine.services.store.delivery_revision())
                .unwrap()
                .take(context, 1, &request),
            Err(expected)
        );
        assert!(matches!(
            f.engine.begin_file(context, 2, request, &peer),
            Err(error) if error == expected
        ));
    }
}

#[test]
fn file_policy_revocation_withholds_ready_bytes_and_prevents_queued_publication() {
    let mut f = Fixture::new();
    let context = claim(&mut f, true);
    let mut sequence = 0;
    std::fs::write(f._directory.path().join("mount/source"), b"private").unwrap();
    let root = file(host_root(&mut f, context, &mut sequence).unwrap());
    let reader = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"source".to_vec(), mode: FILE_READ },
        )
        .unwrap(),
    );
    let writer = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"unpublished".to_vec(), mode: FILE_WRITE },
        )
        .unwrap(),
    );
    call(
        &mut f,
        context,
        &mut sequence,
        writer,
        Action::Write { offset: 0, bytes: b"private".to_vec() },
    )
    .unwrap();
    let (peer, _client) = UnixStream::pair().unwrap();
    let read_request = file_service::Request::Operation(Call {
        file: reader,
        action: Action::Read { offset: 0, length: 7 },
    });
    sequence += 1;
    let read_sequence = sequence;
    let read = f.engine.begin_file(context, sequence, read_request.clone(), &peer).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !read.ready() {
        f.engine.maintain_service().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    // Keep a successful read uncollected; accept close but do not dispatch it.
    let close_request =
        file_service::Request::Operation(Call { file: writer, action: Action::Close { flags: 0 } });
    sequence += 1;
    let close_sequence = sequence;
    let close = f.engine.begin_file(context, sequence, close_request.clone(), &peer).unwrap();
    let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
    let session = f.engine.services.store.session(&binding.session).unwrap();
    f.engine.services.store.record_session_denial(&session, "revoked").unwrap();
    f.engine.maintain_service().unwrap();
    assert_eq!(
        f.engine
            .file_service
            .as_mut()
            .unwrap()
            .take(&read, f.engine.services.store.delivery_revision())
            .unwrap()
            .take(context, read_sequence, &read_request),
        Err(Error::AccessDenied)
    );
    while !close.ready() {
        f.engine.maintain_service().unwrap();
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        f.engine
            .file_service
            .as_mut()
            .unwrap()
            .take(&close, f.engine.services.store.delivery_revision())
            .unwrap()
            .take(context, close_sequence, &close_request),
        Ok(Output::Close {
            error: Error::AccessDenied as i32,
            publication: crate::runtime::host::files::NOT_PUBLISHED,
            recovery: None,
        })
    );
    assert!(!f._directory.path().join("mount/unpublished").exists());
    // Explicit discard remains available without restoring access.
    call(&mut f, context, &mut sequence, reader, Action::Close { flags: FILE_CLOSE_DISCARD })
        .unwrap();
    assert_eq!(
        call(&mut f, context, &mut sequence, root, Action::Status),
        Err(Error::AccessDenied)
    );
}

#[test]
fn volume_file_socket_journey_admits_only_bound_roots_and_publishes_bytes() {
    let mut f = Fixture::new();
    let context = claim(&mut f, true);
    let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
    let volume = binding.root;
    let mut sequence = 0;
    assert_eq!(
        exchange(
            &mut f,
            context,
            &mut sequence,
            file_service::Request::VolumeRoot { volume: Id::from_bytes([99; 16]) }
        ),
        Err(Error::AccessDenied)
    );
    let root = file(
        exchange(&mut f, context, &mut sequence, file_service::Request::VolumeRoot { volume })
            .unwrap(),
    );
    let writer = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"saved/note".to_vec(), mode: FILE_WRITE },
        )
        .unwrap(),
    );
    assert_eq!(
        call(
            &mut f,
            context,
            &mut sequence,
            writer,
            Action::Write { offset: 0, bytes: b"volume file".to_vec() }
        )
        .unwrap(),
        Output::Written
    );
    assert_eq!(
        call(&mut f, context, &mut sequence, writer, Action::Close { flags: FILE_CLOSE_SYNC })
            .unwrap(),
        Output::Close {
            error: 0,
            publication: crate::runtime::host::files::PUBLISHED,
            recovery: None
        }
    );
    let reader = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"saved/note".to_vec(), mode: FILE_READ },
        )
        .unwrap(),
    );
    assert_eq!(
        call(&mut f, context, &mut sequence, reader, Action::Read { offset: 0, length: 64 })
            .unwrap(),
        Output::Read { bytes: b"volume file".to_vec() }
    );
    let access = crate::store::Access::owner(f.engine.services.store.account().id);
    let entry = f.engine.services.store.resolve_entry(&access, volume, "saved/note").unwrap();
    assert_eq!(
        f.engine.services.store.read_blob_entry(&access, volume, entry.id).unwrap(),
        b"volume file"
    );
}

#[test]
fn coalesced_personal_withdrawal_regrant_never_reauthorizes_old_file_roots() {
    let mut f = Fixture::new();
    let unrelated = f.engine.services.store.create_volume("Unrelated", false).unwrap().id;
    let context = claim_personal(&mut f, true, true);
    let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
    let personal = binding.extra_volumes[0].volume;
    let grants = binding.extra_volumes.clone();
    let mut sequence = 0;
    let old = file(
        exchange(
            &mut f,
            context,
            &mut sequence,
            file_service::Request::VolumeRoot { volume: personal },
        )
        .unwrap(),
    );
    let _root = file(
        exchange(
            &mut f,
            context,
            &mut sequence,
            file_service::Request::VolumeRoot { volume: binding.root },
        )
        .unwrap(),
    );
    // Unrelated volume commits must not trigger blocking grant lookups on
    // the engine loop; index changes in every binding dependency must recheck.
    f.engine.maintain_files().unwrap();
    let checks = |f: &Fixture| f.engine.file_service.as_ref().unwrap().volume_grant_checks;
    let before = checks(&f);
    let access = crate::store::Access::owner(binding.account);
    f.engine.services.store.create_blob(&access, unrelated, "note", b"unrelated").unwrap();
    f.engine.maintain_files().unwrap();
    assert_eq!(checks(&f), before);
    for volume in [binding.root, personal] {
        let before = checks(&f);
        f.engine.services.store.create_blob(&access, volume, "note", b"dependent").unwrap();
        f.engine.maintain_files().unwrap();
        // The owner pass covers its root. Only the personal volume needs
        // the additional per-volume check, even when a dependency changed.
        assert_eq!(checks(&f), before + 1);
    }
    // Object payload updates in any dependency do not alter its binding
    // authority. Database creation above/below still changes the index.
    use crate::store::{
        object_contract::{FieldDeclaration, Schema, TypeDeclaration, Value, ValueType},
        volume_index::{EntryAddress, EntryKind},
    };
    use std::collections::BTreeMap;
    let type_id = Id::from_bytes([1; 16]);
    let field_id = Id::from_bytes([2; 16]);
    let schema = Schema {
        types: BTreeMap::from([(
            type_id,
            TypeDeclaration {
                name: "Note".into(),
                deprecated: false,
                fields: BTreeMap::from([(
                    field_id,
                    FieldDeclaration {
                        name: "Text".into(),
                        value_type: ValueType::Text,
                        repeated: false,
                        deprecated: false,
                    },
                )]),
            },
        )]),
    };
    for volume in [binding.root, personal] {
        let entry = f
            .engine
            .services
            .store
            .create_database(
                &access,
                volume,
                "objects",
                EntryKind::ObjectDb { schema: schema.clone() },
            )
            .unwrap();
        let address = EntryAddress { volume, entry: entry.id };
        let object = f.engine.services.store.create_object(&access, address, type_id).unwrap();
        f.engine.maintain_files().unwrap();
        let before = checks(&f);
        f.engine
            .services
            .store
            .edit_fields(
                &access,
                address,
                object.object_id,
                &BTreeMap::from([(field_id, Some(vec![Value::Text("Changed".into())]))]),
            )
            .unwrap();
        f.engine.maintain_files().unwrap();
        assert_eq!(checks(&f), before);
    }
    // Both commits precede any engine maintenance or notice delivery.
    f.engine.services.store.select_personal_volumes(&binding.session, vec![]).unwrap();
    f.engine.services.store.select_personal_volumes(&binding.session, grants).unwrap();
    assert_eq!(call(&mut f, context, &mut sequence, old, Action::Status), Err(Error::AccessDenied));
    let new = file(
        exchange(
            &mut f,
            context,
            &mut sequence,
            file_service::Request::VolumeRoot { volume: personal },
        )
        .unwrap(),
    );
    assert!(matches!(
        call(&mut f, context, &mut sequence, new, Action::Status),
        Ok(Output::Status { kind: FILE_KIND_DIRECTORY, .. })
    ));
    assert_eq!(call(&mut f, context, &mut sequence, old, Action::Status), Err(Error::AccessDenied));
}

#[test]
fn initialized_shared_revocation_and_dependency_barriers_revoke_retained_host_grants() {
    for dependency_barrier in [false, true] {
        let mut f = Fixture::new();
        let mount = f._directory.path().join("shared-host");
        std::fs::create_dir_all(&mount).unwrap();
        let mounts =
            vec![HostMount::resolve(&mount, Some("Files"), true).unwrap()].into_boxed_slice();
        let (context, session, shared, _) = claim_automatic_config(&mut f, None, true, mounts);
        let mut sequence = 0;
        // Lazy World control permits this host grant before any Shared data use.
        let host = file(host_root(&mut f, context, &mut sequence).unwrap());
        let volume = file(
            exchange(
                &mut f,
                context,
                &mut sequence,
                file_service::Request::VolumeRoot { volume: shared },
            )
            .unwrap(),
        );
        call(&mut f, context, &mut sequence, volume, Action::Status).unwrap();
        let store = f.engine.services.store.clone();
        assert!(store.shared_session_initialized(&session.id).unwrap());
        f.engine.maintain_files().unwrap();
        let owner = crate::store::Access::owner(store.account().id);
        if dependency_barrier {
            store.invalidate_authority(&owner, shared).unwrap();
        } else {
            store.revoke(&owner, shared, store.account().id, "removed").unwrap();
        }
        assert_eq!(store.session_denial(&session.id).unwrap(), None);
        f.engine.maintain_files().unwrap();
        for root in [host, volume] {
            assert_eq!(
                call(&mut f, context, &mut sequence, root, Action::Status),
                Err(Error::AccessDenied)
            );
        }
    }
}

#[test]
fn closed_change_feed_revokes_existing_host_and_volume_file_roots() {
    let mut f = Fixture::new();
    let context = claim(&mut f, true);
    let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
    let mut sequence = 0;
    let host = file(host_root(&mut f, context, &mut sequence).unwrap());
    let volume = file(
        exchange(
            &mut f,
            context,
            &mut sequence,
            file_service::Request::VolumeRoot { volume: binding.root },
        )
        .unwrap(),
    );
    f.engine.maintain_files().unwrap();
    assert!(f.engine.services.store.delivery_revision().is_some());
    // This offline fixture has no publisher; dropping its sole receiver
    // exercises the same terminal closure as publisher owner loss.
    drop(f.engine.services.store.take_change_receiver().unwrap());
    assert!(f.engine.services.store.delivery_revision().is_none());
    f.engine.maintain_files().unwrap();
    for root in [host, volume] {
        assert_eq!(
            call(&mut f, context, &mut sequence, root, Action::Status),
            Err(Error::AccessDenied)
        );
    }
}

#[test]
fn gui_folder_removal_revokes_open_handles_and_notifies_without_store_changes() {
    let mut f = Fixture::new();
    let context = claim_config(&mut f, true, false, true);
    let prior = f.engine.session_instances.descriptor(context.instance).unwrap();
    let mut sequence = 0;
    let root = file(host_root(&mut f, context, &mut sequence).unwrap());
    let writer = file(
        call(
            &mut f,
            context,
            &mut sequence,
            root,
            Action::Open { path: b"pending".to_vec(), mode: FILE_WRITE },
        )
        .unwrap(),
    );
    call(
        &mut f,
        context,
        &mut sequence,
        writer,
        Action::Write { offset: 0, bytes: b"not published".to_vec() },
    )
    .unwrap();
    let mut events = f.client();
    ipc::write(&mut events, &encode_request(&Request::InstanceEvents { context }).unwrap())
        .unwrap();
    for _ in 0..8 {
        f.turn();
    }
    let receive = |stream: &mut UnixStream| {
        let bytes = ipc::read(stream).unwrap();
        let Reply::InstanceEvents(notice) =
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        else {
            panic!("notice")
        };
        notice
    };
    let first = receive(&mut events);
    assert_eq!(first.volumes.len(), 3);
    let removed = f.exchange(Request::InstanceMounts {
        instance: prior.id,
        expected: prior.host_mounts.clone(),
        mounts: Box::default(),
    });
    assert!(matches!(removed, Reply::Instance(_)));
    for _ in 0..8 {
        f.turn();
    }
    let changed = receive(&mut events);
    assert_eq!(changed.revision, first.revision);
    assert_eq!(changed.data_revision, first.data_revision);
    assert_eq!(changed.mount_revision, first.mount_revision + 1);
    assert_eq!(changed.volumes.len(), 2);
    assert_eq!(
        call(&mut f, context, &mut sequence, root, Action::Status),
        Err(Error::AccessDenied)
    );
    let Output::Close { error, publication, recovery } =
        call(&mut f, context, &mut sequence, writer, Action::Close { flags: 0 }).unwrap()
    else {
        panic!("close")
    };
    assert_eq!(error, Error::AccessDenied as i32);
    assert_eq!(publication, crate::runtime::host::files::NOT_PUBLISHED);
    assert!(recovery.is_none());
    assert!(!f._directory.path().join("mount/pending").exists());
    let Reply::Instance(readded) = f.exchange(Request::InstanceMounts {
        instance: prior.id,
        expected: Box::default(),
        mounts: prior.host_mounts.clone(),
    }) else {
        panic!("readded")
    };
    assert_ne!(readded.host_mounts[0].id, prior.host_mounts[0].id);
    let fresh = file(host_root(&mut f, context, &mut sequence).unwrap());
    assert!(call(&mut f, context, &mut sequence, fresh, Action::Status).is_ok());
    assert_eq!(
        call(&mut f, context, &mut sequence, root, Action::Status),
        Err(Error::AccessDenied)
    );
}

#[test]
fn guest_events_restore_an_omitted_captured_root_and_close_on_feed_loss() {
    use crate::store::{Access, DatabaseId};
    let mut f = Fixture::new();
    let context = claim_personal(&mut f, true, true);
    let binding = f.engine.session_instances.descriptor(context.instance).unwrap();
    let personal = binding.extra_volumes[0].volume;
    let store = f.engine.services.store.clone();
    let owner = Access::owner(store.account().id);
    let mut events = f.client();
    ipc::write(&mut events, &encode_request(&Request::InstanceEvents { context }).unwrap())
        .unwrap();
    let receive = |f: &mut Fixture, events: &mut UnixStream| {
        for _ in 0..8 {
            f.turn();
        }
        let bytes = ipc::read(events).unwrap();
        let Reply::InstanceEvents(notice) =
            ipc::Response::new(bytes).unwrap().into_owned().unwrap()
        else {
            panic!("event notice");
        };
        notice
    };
    let first = receive(&mut f, &mut events);
    let has_personal = |notice: &crate::engine::service_events::Notice| {
        notice.volumes.iter().any(|volume| volume.id == *personal.as_bytes())
    };
    assert!(has_personal(&first));
    store.invalidate_authority(&owner, personal).unwrap();
    let omitted = receive(&mut f, &mut events);
    assert!(!has_personal(&omitted));
    assert!(omitted.stop.is_none());
    assert!(omitted.data_revision > first.data_revision);

    // Substitute a trusted authenticated receipt, as in the store authority
    // fixtures. Only this target changes; no global consent change wakes it.
    let mut stage = store.prepare_received_volume(personal).unwrap();
    for database in [DatabaseId::Index, DatabaseId::Revocations] {
        store
            .with_replica(personal, database, |source| {
                stage.with_database(database, |target| {
                    if database == DatabaseId::Index {
                        crate::store::merge_index_replica(
                            &std::env::temp_dir(),
                            source,
                            target,
                            kvdb::Lww,
                            |_| Ok(()),
                        )
                    } else {
                        crate::store::merge_replica(
                            &std::env::temp_dir(),
                            source,
                            target,
                            kvdb::Lww,
                            |_| Ok(()),
                        )
                    }
                })
            })
            .unwrap();
    }
    store
        .with_replica(personal, DatabaseId::Index, |source| {
            use crate::store::volume_index::namespace::{receipt, source::cut};
            let accepted = cut::accepted(&source.begin_read()?)?;
            stage.with_database(DatabaseId::Index, |target| {
                receipt::retain_fixture(target, None, &accepted)
            })
        })
        .unwrap();
    stage.qualify_authority();
    store.adopt_received_volume(&owner, &stage).unwrap();
    let restored = receive(&mut f, &mut events);
    assert!(has_personal(&restored));
    assert!(restored.stop.is_none());
    assert!(restored.data_revision > omitted.data_revision);
    assert_eq!(restored.mount_revision, omitted.mount_revision);

    drop(store.take_change_receiver().unwrap());
    let stopped = receive(&mut f, &mut events);
    assert_eq!(stopped.revision, None);
    assert_eq!(stopped.stop, Some(crate::engine::service_events::StopReason::ContextUnavailable));
    assert!(stopped.volumes.is_empty());
}
