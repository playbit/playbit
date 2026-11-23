use super::*;
use crate::engine::{
    Engine, Request,
    reply::{Reply, TransferKind, TransferStatus},
};

#[test]
fn offline_startup_retains_enrollment_and_local_work_without_connection_attempts() {
    fn diagnostic(engine: &mut Engine, request: Request) -> Reply {
        let ticket = engine.begin_network_request(request).unwrap();
        collect_until(engine, |engine| engine.network_request_complete(&ticket));
        engine.take_network_request(ticket).unwrap()
    }

    let directory = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::open(directory.path()).unwrap();
        let remote = engine.remote.as_mut().unwrap();
        remote.identity = preissued_identity(&engine.services.store);
        remote.identity.save(&remote.identity_path).unwrap();
        engine.dispatch_reply(Request::WorkOffline { enabled: true }).unwrap();
    }
    let mut engine = Engine::open(directory.path()).unwrap();
    assert!(engine.enrolled());
    assert!(!engine.bootstrap_pending);
    assert_eq!(
        engine.status_snapshot().unwrap().ap_connection.state,
        ApConnectionState::OfflineMode
    );
    engine.start_network_work();
    assert!(engine.background.is_none());
    assert!(engine.network_wake_after(Instant::now()).is_none());
    let reply = engine
        .dispatch_reply(Request::CreateVolume {
            name: "Local work".into(),
            share_key_with_service: false,
        })
        .unwrap();
    let Reply::Volume(volume) = reply else { panic!("created volume") };
    let source = directory.path().join("source");
    fs::write(&source, b"offline bytes").unwrap();
    let created = diagnostic(
        &mut engine,
        Request::FileCreate {
            session: None,
            volume: volume.id,
            name: "retained.txt".into(),
            path: source,
        },
    );
    let Reply::Diagnostic(created) = created else { panic!("created file") };
    let crate::engine::reply::DiagnosticReply::FileCreated { address, .. } = created else {
        panic!("file identity")
    };
    let destination = directory.path().join("exported");
    diagnostic(
        &mut engine,
        Request::FileRead { session: None, address, path: destination.clone() },
    );
    assert_eq!(fs::read(destination).unwrap(), b"offline bytes");
    assert!(
        matches!(diagnostic(&mut engine, Request::DuplicateVolume { volume: volume.id }), Reply::Diagnostic(reply)
        if matches!(reply, crate::engine::reply::DiagnosticReply::VolumeDuplicated { .. }))
    );
    let remote = engine.remote.as_ref().unwrap();
    assert!(remote.address_lookup.is_none());
    assert!(remote.connection.is_none());
    assert!(remote.reconnect_after(Instant::now()).is_none());
}

#[test]
fn offline_blocks_transport_and_enrollment_without_changing_identity() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    engine.dispatch_reply(Request::WorkOffline { enabled: true }).unwrap();
    let remote = engine.remote.as_mut().unwrap();
    let identity = remote.identity.email.clone();
    assert!(
        remote.connect(&engine.services.store).unwrap_err().to_string().contains("Work Offline")
    );
    assert!(remote.prepare_sign_in().is_err());
    assert!(remote.request_code(&engine.services.store, "offline@example.invalid").is_err());
    assert_eq!(remote.identity.email, identity);
    assert!(remote.address_lookup.is_none());
    assert!(remote.config.is_none());
    assert_eq!(remote.connection_status.snapshot().state, ApConnectionState::OfflineMode);
}

#[test]
fn online_transition_schedules_existing_bootstrap_and_binds_its_reply() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    engine.remote.as_mut().unwrap().identity = preissued_identity(&engine.services.store);
    engine.dispatch_reply(Request::WorkOffline { enabled: true }).unwrap();
    let request = Request::WorkOffline { enabled: false };
    let reply = engine.dispatch_reply(request.clone()).unwrap();
    let archive = crate::engine::ipc::Response::new(
        crate::engine::ipc::codec::encode(&reply, crate::engine::ipc::codec::MAX_ARCHIVE).unwrap(),
    )
    .unwrap();
    assert!(archive.archived().matches_request(&request));
    assert!(!archive.archived().matches_request(&Request::WorkOffline { enabled: true }));
    assert!(engine.bootstrap_pending);
    assert_eq!(
        engine.status_snapshot().unwrap().ap_connection.state,
        ApConnectionState::Connecting
    );
    engine.bootstrap_pending = false;
    engine.dispatch_reply(request).unwrap();
    assert!(!engine.bootstrap_pending, "an unchanged mode does not enqueue another bootstrap");
    assert!(!engine.services.store.work_offline().unwrap());
}

#[test]
fn offline_intent_fences_a_late_worker_and_clears_reconnecting_progress() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let (send, remote) = crate::engine::background::tests::transfer(&mut engine);
    engine.dispatch_reply(Request::WorkOffline { enabled: true }).unwrap();
    // The returned owner's old status cannot override newer local intent.
    remote.begin_connection();
    send.send((
        remote,
        Ok(crate::engine::background::NetworkResult::Synchronized),
        BTreeMap::new(),
    ))
    .unwrap();
    crate::engine::background::tests::notify(&engine);
    engine.complete_background().unwrap();
    assert!(!engine.bootstrap_pending);
    assert!(engine.synchronization.is_none());
    assert!(engine.network_wake_after(Instant::now()).is_none());
    assert_eq!(engine.transfer_status().kind, TransferKind::Paused);
    assert_eq!(
        engine.status_snapshot().unwrap().ap_connection.state,
        ApConnectionState::OfflineMode
    );

    engine.dispatch_reply(Request::WorkOffline { enabled: false }).unwrap();
    engine.transfer_result = TransferStatus {
        kind: TransferKind::Running,
        message: Some("Reconnecting…".into()),
        ..Default::default()
    };
    engine.dispatch_reply(Request::Disconnect).unwrap();
    assert_eq!(engine.transfer_status().kind, TransferKind::Paused);
    assert!(
        !engine.services.store.work_offline().unwrap(),
        "diagnostic disconnect does not change remembered mode"
    );
}

#[test]
fn offline_idle_completions_and_diagnostic_disconnect_preserve_terminal_results() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    for kind in [TransferKind::Failed, TransferKind::Complete] {
        engine.transfer_result =
            TransferStatus { kind, message: Some("Retained result".into()), ..Default::default() };
        engine.dispatch_reply(Request::Disconnect).unwrap();
        assert_eq!(engine.transfer_result.kind, kind);
        engine.dispatch_reply(Request::WorkOffline { enabled: true }).unwrap();
        engine.transfer_result = TransferStatus {
            kind,
            message: Some("Local operation result".into()),
            ..Default::default()
        };
        engine.complete_background().unwrap();
        assert_eq!(engine.transfer_result.kind, kind);
        assert_eq!(engine.transfer_result.message.as_deref(), Some("Local operation result"));
    }
}
