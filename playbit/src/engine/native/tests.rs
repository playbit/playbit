//! Exercise ownership, scene admission and feed scheduling with real local services.
#![allow(unsafe_code, clippy::unwrap_used, reason = "Tests own callback state and live fixtures.")]
use super::*;
use playbit_api::{
    abi::operations as abi,
    context::{Context, ContextState},
    ui::CheckState,
};
use std::ffi::c_void;
#[derive(Default)]
struct Probe {
    files: playbit_api::context::FileResources,
    context: ContextState,
    init: usize,
    turns: usize,
    destroyed: usize,
    changes: usize,
    clicks: usize,
    fail: bool,
    identity: Option<abi::Identity>,
    volumes: Vec<playbit_api::abi::data::Volume>,
}
unsafe extern "C" fn init(
    _: *mut abi::Context,
    data: *mut c_void,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: each fixture retains its probe until Apps is cleared.
    unsafe {
        (*data.cast::<Probe>()).init += 1;
        output.write(data);
    }
    0
}
unsafe extern "C" fn turn(raw: *mut abi::Context, data: *mut c_void) -> i32 {
    // SAFETY: the executor lends this live context/probe during the callback.
    let probe = unsafe { &mut *data.cast::<Probe>() };
    let (mut context, mut scene) = unsafe { Context::from_raw(raw, &mut probe.context).unwrap() };
    context.attach_files(&probe.files).unwrap();
    probe.identity = Some(context.identity().unwrap());
    let mut records = [playbit_api::abi::data::Volume::default(); 8];
    let mut tags = [playbit_api::abi::data::Range::default(); 8];
    let mut bytes = [0; 1024];
    let count =
        context.available_volumes(&mut records, &mut tags, &mut bytes).unwrap().iter().len();
    probe.volumes = records[..count].to_vec();
    probe.turns += 1;
    if probe.fail {
        return HostError::Unavailable as i32;
    }
    for event in context.raw_events() {
        match event.kind {
            EventKind::DataChanged => probe.changes += 1,
            EventKind::Click => probe.clicks += 1,
            EventKind::Started => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
    scene.column_start();
    scene.checkbox(1, "Native app", &mut CheckState::Off, false);
    scene.checkbox(2, "Disabled", &mut CheckState::Off, true);
    scene.column_end();
    scene.submit(&mut context).unwrap();
    0
}
unsafe extern "C" fn destroy(data: *mut c_void) {
    // SAFETY: app state borrows the fixture, which outlives clear/detach.
    unsafe {
        (*data.cast::<Probe>()).destroyed += 1;
    }
}
fn table(probe: &mut Probe) -> App {
    App {
        size: core::mem::size_of::<App>() as u32,
        written: 0,
        init: Some(init),
        turn: Some(turn),
        destroy: Some(destroy),
        user_data: (probe as *mut Probe).cast(),
    }
}
fn input(target: u32) -> Event {
    Event { kind: EventKind::Click, target, data: vec![] }
}
#[cfg(all(target_os = "macos", feature = "desktop-ui", feature = "connected-engine"))]
#[test]
fn standalone_shared_creation_retains_authenticated_build_upload_before_registration() {
    use crate::{
        network::ap::publisher::{KeyStatus, KeyStatusClaims},
        package::{AutomaticVolumeRole, Package, VolumeInitializers},
        store::PendingKind,
    };
    use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};

    let directory = tempfile::tempdir().unwrap();
    let publisher = Services::open(directory.path().join("publisher"), || {}).unwrap();
    let package = crate::package::signature::testing::signed(
        &publisher.store,
        Package::create(
            "standalone-upload".into(),
            "Standalone upload".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            Default::default(),
            &VolumeInitializers::single(
                AutomaticVolumeRole::Session,
                VolumeInitializers::default().for_role(AutomaticVolumeRole::Session),
            )
            .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let app = package.identity().unwrap().clone();
    let build = package.id().unwrap();
    let bytes = package.to_bytes().unwrap();
    let paired = directory.path().join("native-app.pbapp");
    std::fs::write(&paired, &bytes).unwrap();
    let services = Services::open(directory.path().join("consumer"), || {}).unwrap();
    assert_ne!(services.store.account().id, app.publisher);
    // Only publisher trust is delivered with the fixture. Signing/authentication
    // did not run developer preparation or enqueue a prior build upload.
    let now = crate::store::now_unix_secs();
    let issuer = SigningKey::from_bytes(&[7; 32]);
    let claims = &package.publisher_certificate().unwrap().claims;
    let status = KeyStatus::issue(
        KeyStatusClaims {
            issuer: issuer.verifying_key().to_bytes(),
            publisher: claims.publisher,
            signer: claims.signer,
            epoch: 1,
            checked_at: now,
            revoked: false,
        },
        issuer.to_pkcs8_der().unwrap().as_bytes(),
    )
    .unwrap();
    services
        .store
        .cache_publisher_status(issuer.verifying_key().to_bytes(), &status.to_bytes(), now)
        .unwrap();
    assert!(services.store.pending_operations().unwrap().is_empty());
    let seed = [0x65; crate::store::SEED_BYTES];
    let session = services.create_local_session(&Package::open(&paired).unwrap(), seed).unwrap();
    assert_eq!(session.app, app);
    assert_eq!(session.creator, services.store.account().id);
    let cached = services.profile_path().join(crate::paths::BUILDS).join(&build);
    assert_eq!(std::fs::read(&cached).unwrap(), bytes);
    let pending = services.store.pending_operations().unwrap();
    assert_eq!(pending.len(), 1);
    let upload = pending[0].clone();
    assert!(matches!(&upload.operation, PendingKind::UploadBuild { build_id, path }
        if build_id == &build && path == &std::path::PathBuf::from("builds").join(&build)));
    assert_eq!(services.create_local_session(&package, seed).unwrap(), session);
    assert_eq!(services.store.pending_operations().unwrap(), pending);
    std::fs::remove_file(paired).unwrap();
    drop(services);

    let services = Services::open(directory.path().join("consumer"), || {}).unwrap();
    let retained = Package::open(&cached).unwrap();
    assert_eq!(services.create_local_session(&retained, seed).unwrap(), session);
    assert_eq!(services.store.pending_operations().unwrap(), pending);
    // First Shared use adds registrations only after the original upload intent
    // is durable. Remote's existing registration ordering publishes UploadBuild
    // before RegisterSession, using these exact retained signed bytes.
    let shared =
        services.store.open_automatic_volume(&session.id, AutomaticVolumeRole::Session).unwrap();
    let registrations = services.store.pending_operations().unwrap();
    assert_eq!(registrations.len(), 3);
    assert!(registrations.contains(&upload));
    assert!(registrations.iter().any(|operation| matches!(&operation.operation,
        PendingKind::RegisterVolume { volume } if volume.id == shared.id)));
    assert!(registrations.iter().any(|operation| matches!(&operation.operation,
        PendingKind::RegisterSession { session: registered, .. } if registered == &session)));
    assert_eq!(services.create_local_session(&retained, seed).unwrap(), session);
    assert_eq!(services.store.pending_operations().unwrap(), registrations);
}

#[test]
fn attach_after_reopening_profile_through_symlink() {
    let path = tempfile::tempdir().unwrap();
    let profile = path.path().join("profile");
    let services = Services::open(&profile, || {}).unwrap();
    let root = services.store.create_volume("Root", false).unwrap().id;
    drop(services);
    let alias = path.path().join("alias");
    std::os::unix::fs::symlink(&profile, &alias).unwrap();
    let services = Services::open(&alias, || {}).unwrap();
    let mut probe = Probe::default();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    // SAFETY: the probe remains in place until the app is detached.
    let generation = unsafe { apps.attach(&services, root, table(&mut probe)).unwrap() };
    apps.advance(generation, |frame| assert!(frame.view().is_some())).unwrap();
    assert_eq!(probe.init, 1);
    assert_eq!(probe.turns, 1);
    apps.detach(generation).unwrap();
    assert_eq!(probe.destroyed, 1);
}
#[test]
fn capacity_generations_scene_validation_and_shutdown() {
    // Real store roots bind authority; capacity rejection invokes no foreign code.
    let path = tempfile::tempdir().unwrap();
    let services = Services::open(path.path(), || {}).unwrap();
    let root = services.store.create_volume("Root", false).unwrap().id;
    let mut probes: [Probe; 5] = std::array::from_fn(|_| Probe::default());
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let mut generations = Vec::new();
    for probe in &mut probes[..4] {
        // SAFETY: all probes remain in place through clear.
        generations.push(unsafe { apps.attach(&services, root, table(probe)).unwrap() });
    }
    assert_eq!(
        unsafe { apps.attach(&services, root, table(&mut probes[4])) },
        Err(HostError::LimitExceeded)
    );
    assert!(apps.step(generations[0], Instant::now()).unwrap().ready);
    assert_eq!(apps.input(generations[0], input(1)), Err(HostError::Conflict));
    apps.advance(generations[0], |frame| assert!(frame.view().is_some())).unwrap();
    assert_eq!(apps.input(generations[0], input(2)), Err(HostError::InvalidArgument));
    assert_eq!(apps.input(generations[0], input(999)), Err(HostError::InvalidArgument));
    apps.input(generations[0], input(1)).unwrap();
    apps.advance(generations[0], |_| {}).unwrap();
    assert_eq!(probes[0].clicks, 1);
    apps.detach(generations[0]).unwrap();
    assert_eq!(probes[0].destroyed, 1);
    let new = unsafe { apps.attach(&services, root, table(&mut probes[4])).unwrap() };
    assert!(!generations.contains(&new));
    assert_eq!(apps.step(generations[0], Instant::now()), Err(HostError::StaleHandle));
    apps.clear();
    assert_eq!(probes.iter().map(|p| p.destroyed).sum::<usize>(), 1);
}
#[test]
fn saturated_input_drains_before_deferred_change_and_halt_stays_idle() {
    // A full input queue must not deadlock behind a coalesced data notification.
    let path = tempfile::tempdir().unwrap();
    let services = Services::open(path.path(), || {}).unwrap();
    let root = services.store.create_volume("Root", false).unwrap().id;
    let mut receiver = services.store.take_change_receiver().unwrap();
    let mut probe = Probe::default();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let generation = unsafe { apps.attach(&services, root, table(&mut probe)).unwrap() };
    apps.advance(generation, |_| {}).unwrap();
    let mut accepted = 0;
    for _ in 0..1024 {
        match apps.input(generation, input(1)) {
            Ok(()) => accepted += 1,
            Err(HostError::LimitExceeded) => break,
            result => panic!("unexpected admission {result:?}"),
        }
    }
    assert!(accepted > 0 && accepted < 1024);
    apps.committed(&receiver.drain());
    apps.advance(generation, |_| {}).unwrap();
    assert_eq!(probe.clicks, accepted);
    assert_eq!(probe.changes, 0);
    assert!(apps.step(generation, Instant::now()).unwrap().ready);
    apps.advance(generation, |_| {}).unwrap();
    assert_eq!(probe.changes, 1);
    assert!(!apps.step(generation, Instant::now()).unwrap().ready);
    probe.fail = true;
    assert_eq!(
        apps.advance(generation, |_| panic!("no failed output")),
        Err(HostError::Unavailable)
    );
    let mut batch = receiver.drain();
    batch.status.gap_generation += 1;
    apps.committed(&batch);
    assert!(!apps.step(generation, Instant::now()).unwrap().ready);
    assert_eq!(apps.advance(generation, |_| {}), Err(HostError::Conflict));
    assert_eq!(probe.turns, 4);
    apps.clear();
    assert_eq!(probe.destroyed, 1);
}
#[test]
fn root_notifications_and_sticky_gap_recovery_are_coalesced() {
    // Independent roots stay idle; each newly observed gap wakes all apps once.
    let path = tempfile::tempdir().unwrap();
    let services = Services::open(path.path(), || {}).unwrap();
    let roots = [
        services.store.create_volume("A", false).unwrap().id,
        services.store.create_volume("B", false).unwrap().id,
    ];
    let feed = crate::store::change_feed::CommittedChangeFeed::with_wake(
        StoreId::generate().unwrap(),
        || {},
    );
    let mut receiver = feed.take_receiver().unwrap();
    let mut probes = [Probe::default(), Probe::default()];
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let a = unsafe { apps.attach(&services, roots[0], table(&mut probes[0])).unwrap() };
    let b = unsafe { apps.attach(&services, roots[1], table(&mut probes[1])).unwrap() };
    for generation in [a, b] {
        apps.advance(generation, |_| {}).unwrap();
    }
    for _ in 0..3 {
        feed.publish(
            roots[0],
            crate::store::DatabaseId::Entry(crate::store::EntryId::from_bytes([7; 15]).unwrap()),
            kvdb::sync::CommittedDelta::Bytes(vec![1; 32]),
        );
    }
    apps.committed(&receiver.drain());
    assert!(apps.step(a, Instant::now()).unwrap().ready);
    assert!(!apps.step(b, Instant::now()).unwrap().ready);
    apps.advance(a, |_| {}).unwrap();
    assert_eq!(probes[0].changes, 1);
    let mut batch = receiver.drain();
    batch.status.gap_generation = 1;
    apps.committed(&batch);
    for generation in [a, b] {
        apps.advance(generation, |_| {}).unwrap();
    }
    apps.committed(&batch);
    for generation in [a, b] {
        assert!(!apps.step(generation, Instant::now()).unwrap().ready);
    }
    assert_eq!((probes[0].changes, probes[1].changes), (2, 1));
    apps.clear();
}

#[cfg(feature = "connected-engine")]
fn session_fixture() -> (
    tempfile::TempDir,
    crate::engine::connected::Connected,
    crate::store::Session,
    crate::package::Package,
) {
    let directory = tempfile::tempdir().unwrap();
    let connected = crate::engine::connected::Connected::open(directory.path(), || {}).unwrap();
    let root = connected.services.store.create_volume("Root", true).unwrap();
    let package = crate::package::Package::new(
        "test".into(),
        "Session probe".into(),
        wat::parse_str("(module (func (export \"pb_main\")))").unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    let session = connected.services.create_session(package.clone(), root.id).unwrap();
    (directory, connected, session, package)
}

#[cfg(feature = "connected-engine")]
#[test]
fn session_identity_consent_and_generation_follow_retained_authority() {
    let (_directory, connected, session, package) = session_fixture();
    let personal = connected.services.store.create_volume("Personal", false).unwrap();
    let mut probe = Probe::default();
    let (send, notifications) = std::sync::mpsc::sync_channel(1);
    let mut apps = Apps::new(std::sync::Arc::new(move || {
        let _ = send.try_send(());
    }));
    let generation = unsafe {
        apps.attach_session(
            &connected.services,
            &session.id,
            package.clone(),
            table(&mut probe),
            connected.blob_transport(),
        )
        .unwrap()
    };
    apps.advance(generation, |_| {}).unwrap();
    let identity = probe.identity.unwrap();
    assert_eq!(identity.account_id, *connected.services.store.account().id.as_bytes());
    assert_eq!(
        identity.session_id,
        super::super::session_context::decode_session_id(&session.id).unwrap()
    );
    assert_eq!(identity.flags, 2);
    assert_ne!(identity.instance_id, *session.root_volume.as_bytes());
    assert_ne!(identity.instance_id, [0; 16]);
    assert_eq!(probe.volumes.len(), 2);
    let root_handle =
        probe.volumes.iter().find(|v| v.id == *session.root_volume.as_bytes()).unwrap().handle;
    for write in [false, true] {
        connected
            .services
            .store
            .select_personal_volumes(
                &session.id,
                vec![crate::store::PersonalVolumeGrant { volume: personal.id, write }],
            )
            .unwrap();
        apps.advance(generation, |_| {}).unwrap();
        assert!(
            probe.volumes.iter().all(|v| v.id != *personal.id.as_bytes()),
            "new consent cannot expand an existing instance's ceiling"
        );
        assert_eq!(probe.identity.unwrap(), identity);
        assert_eq!(probe.init, 1, "consent must not reinitialize app-owned state");
    }
    let mut fresh = Probe::default();
    let newer = unsafe {
        apps.attach_session(
            &connected.services,
            &session.id,
            package,
            table(&mut fresh),
            connected.blob_transport(),
        )
        .unwrap()
    };
    apps.advance(newer, |_| {}).unwrap();
    let initial = *fresh.volumes.iter().find(|v| v.id == *personal.id.as_bytes()).unwrap();
    assert_eq!(initial.rights, 3);
    connected.services.store.select_personal_volumes(&session.id, vec![]).unwrap();
    connected
        .services
        .store
        .select_personal_volumes(
            &session.id,
            vec![crate::store::PersonalVolumeGrant { volume: personal.id, write: true }],
        )
        .unwrap();
    loop {
        apps.advance(newer, |_| {}).unwrap();
        if fresh.volumes.iter().any(|volume| {
            volume.id == *personal.id.as_bytes() && volume.grant_id != initial.grant_id
        }) {
            break;
        }
        notifications.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    }
    let restored =
        *fresh.volumes.iter().find(|volume| volume.id == *personal.id.as_bytes()).unwrap();
    assert_ne!(
        restored.grant_id, initial.grant_id,
        "coalesced withdrawal never revives a File grant"
    );

    connected
        .services
        .store
        .select_personal_volumes(
            &session.id,
            vec![crate::store::PersonalVolumeGrant { volume: personal.id, write: false }],
        )
        .unwrap();
    loop {
        apps.advance(newer, |_| {}).unwrap();
        if fresh
            .volumes
            .iter()
            .any(|volume| volume.id == *personal.id.as_bytes() && volume.rights == 1)
        {
            break;
        }
        notifications.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    }
    let narrowed = fresh.volumes.iter().find(|v| v.id == *personal.id.as_bytes()).unwrap();
    assert_eq!(narrowed.rights, 1);
    assert_ne!(narrowed.grant_id, restored.grant_id);
    connected.services.store.select_personal_volumes(&session.id, vec![]).unwrap();
    apps.advance(generation, |_| {}).unwrap();
    apps.advance(newer, |_| {}).unwrap();
    assert_eq!(probe.volumes.len(), 2);
    assert_eq!(
        probe.volumes.iter().find(|v| v.id == *session.root_volume.as_bytes()).unwrap().handle,
        root_handle
    );
    assert_eq!(fresh.volumes.len(), 2);
    // Account consent revisions coalesce a read refresh without enlarging the
    // original ceiling or reinitializing either owner.
    assert_eq!(probe.changes, 3);
    assert!(fresh.changes >= 3, "withdrawal, regrant, and narrowing each refresh the app");
    assert_eq!(fresh.init, 1);
    apps.clear();
    assert_eq!(probe.destroyed, 1);
    assert_eq!(fresh.destroyed, 1);
}

#[cfg(feature = "connected-engine")]
#[test]
fn wrong_package_and_revocation_never_invoke_native_callbacks() {
    let (_directory, connected, session, package) = session_fixture();
    let mut wrong = package.clone();
    wrong = wrong.renamed("Different build").unwrap();
    let mut probe = Probe::default();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    assert_eq!(
        unsafe {
            apps.attach_session(
                &connected.services,
                &session.id,
                wrong,
                table(&mut probe),
                connected.blob_transport(),
            )
        },
        Err(HostError::Conflict)
    );
    assert_eq!(probe.init, 0);
    let generation = unsafe {
        apps.attach_session(
            &connected.services,
            &session.id,
            package,
            table(&mut probe),
            connected.blob_transport(),
        )
        .unwrap()
    };
    connected
        .services
        .store
        .record_session_denial(&session, "revoked before initialization")
        .unwrap();
    assert_eq!(
        apps.advance(generation, |_| panic!("denied callback cannot return a frame")),
        Err(HostError::AccessDenied)
    );
    assert_eq!(probe.init, 0);
    assert_eq!(probe.turns, 0);
    assert!(!apps.step(generation, Instant::now()).unwrap().ready);
    assert!(apps.step(generation, Instant::now()).unwrap().wake_after.is_none());
    apps.clear();
    assert_eq!(probe.destroyed, 0, "uninitialized state has no destroy callback");
}

#[cfg(feature = "connected-engine")]
#[test]
fn a_live_session_denial_stops_queued_input_and_destroys_state_once() {
    let (_directory, connected, session, package) = session_fixture();
    let mut probe = Probe::default();
    let mut apps = Apps::new(std::sync::Arc::new(|| {}));
    let generation = unsafe {
        apps.attach_session(
            &connected.services,
            &session.id,
            package,
            table(&mut probe),
            connected.blob_transport(),
        )
        .unwrap()
    };
    apps.advance(generation, |_| {}).unwrap();
    apps.input(generation, input(1)).unwrap();
    connected
        .services
        .store
        .record_session_denial(&session, "revoked after input admission")
        .unwrap();
    assert_eq!(
        apps.advance(generation, |_| panic!("revoked app cannot render")),
        Err(HostError::AccessDenied)
    );
    assert_eq!(probe.turns, 1);
    assert_eq!(probe.clicks, 0);
    assert_eq!(apps.input(generation, input(1)), Err(HostError::AccessDenied));
    // Restoring access requires explicit attachment of a new generation. The
    // owner must never replay the previously queued input behind the UI's back.
    let denial_generation = connected.services.store.session_denial_generation(&session).unwrap();
    assert!(connected.services.store.clear_session_denial(&session, denial_generation).unwrap());
    assert_eq!(apps.advance(generation, |_| {}), Err(HostError::AccessDenied));
    apps.clear();
    assert_eq!(probe.destroyed, 1);
}

#[test]
fn received_bootstrap_refreshes_shared_native_readers_without_publication_or_notices() {
    let path = tempfile::tempdir().unwrap();
    let services = Services::open(path.path(), || {}).unwrap();
    let root = services.store.create_volume("Shared root", false).unwrap().id;
    let unrelated = services.store.create_volume("Independent root", false).unwrap().id;
    let access = Access::owner(services.store.account().id);
    let mut receiver = services.store.take_change_receiver().unwrap();
    while !receiver.drain().records.is_empty() {}
    let mut probes = [Probe::default(), Probe::default(), Probe::default()];
    let mut apps = Apps::new(Arc::new(|| {}));
    let generations = [root, root, unrelated]
        .into_iter()
        .zip(&mut probes)
        .map(|(root, probe)| {
            // SAFETY: callback state remains in place until the owner is cleared.
            unsafe { apps.attach(&services, root, table(probe)).unwrap() }
        })
        .collect::<Vec<_>>();
    for &generation in &generations {
        apps.advance(generation, |_| {}).unwrap();
    }
    let identities = probes.iter().map(|probe| probe.identity.unwrap()).collect::<Vec<_>>();
    let staged = services.store.prepare_received_volume(root).unwrap();
    for database in [crate::store::DatabaseId::Index, crate::store::DatabaseId::Revocations] {
        services
            .store
            .with_replica(root, database, |source| {
                staged.with_database(database, |target| {
                    if database == crate::store::DatabaseId::Index {
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
    staged
        .with_database(crate::store::DatabaseId::Revocations, |database| {
            let mut transaction = database.begin_write(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )?;
            transaction.put(
                StoreId::from_bytes([55; 16]).to_string().as_bytes(),
                &crate::store::encode("Removed participant")?,
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();
    let outgoing = services.store.publication_generation();
    services.store.adopt_received_volume(&access, &staged).unwrap();
    assert_eq!(services.store.publication_generation(), outgoing);
    assert!(receiver.drain().records.is_empty(), "received initialization never echoes to AP");
    for (index, &generation) in generations.iter().enumerate() {
        assert_eq!(apps.step(generation, Instant::now()).unwrap().ready, index < 2);
        if index < 2 {
            apps.advance(generation, |_| {}).unwrap();
        }
    }
    assert_eq!(probes.iter().map(|probe| probe.changes).collect::<Vec<_>>(), [1, 1, 0]);
    for (index, &generation) in generations.iter().enumerate() {
        assert_eq!(probes[index].identity.unwrap(), identities[index]);
        assert_eq!(probes[index].init, 1);
        assert_eq!(probes[index].clicks, 0, "observing storage cannot replay input");
        assert!(!apps.step(generation, Instant::now()).unwrap().ready);
    }
    apps.clear();
}
