use super::*;
use crate::{
    engine::{Engine, Request, cancellation::Canceled, reply::TransferKind},
    network::ap::{Frame, frame_type, operation::SERVICE_DESTINATION, service, testing},
};

#[test]
fn interrupted_agreement_preserves_bootstrap_and_reconnect_without_admitting_data() {
    use crate::network::ap::{APPLICATION_ERROR_RESTART, registration};

    for (bootstrap, cancel) in [(true, false), (false, false), (true, true), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        let store = engine.services.store.clone();
        let remote = engine.remote.as_mut().unwrap();
        remote.identity = preissued_identity(&store);
        let (runtime, endpoint, config) = testing::loopback_ap();
        remote.set_test_transport(config);
        let scope = crate::engine::live::ScopeKey {
            entry: None,
            volume: store.account().id,
            session: None,
        };
        if !bootstrap || cancel {
            let mut retry = Reconnect::new(if bootstrap { vec![] } else { vec![scope] });
            retry.bootstrap = bootstrap;
            retry.account = !bootstrap;
            remote.reconnect = Some(retry);
        }
        let canceled = Arc::new(Canceled::default());
        remote.set_cancellation(Some(canceled.clone()));
        let data = remote.sync_connection.clone();
        let (done, finished) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            runtime.block_on(async {
                tokio::time::timeout(RETURN_BY, async {
                    let hello = async || {
                        let connection = endpoint.accept().await.unwrap().await.unwrap();
                        let (mut send, mut read) = connection.accept_bi().await.unwrap();
                        let request = testing::read_frame(&mut read).await.unwrap();
                        assert_eq!(request.frame_type, frame_type::HELLO);
                        let reply = Frame::new(frame_type::HELLO, 0, [], [], []).unwrap();
                        send.write_all(&testing::authentication_wire(reply)).await.unwrap();
                        (connection, send, read)
                    };
                    let _old_transport = if bootstrap { None } else { Some(hello().await) };
                    let (connection, _send, _read) = hello().await;
                    let (_agreement, mut read) = connection.accept_bi().await.unwrap();
                    let request = testing::read_frame(&mut read).await.unwrap();
                    let flags = if bootstrap { 0 } else { registration::TERMS_REVISION };
                    assert_eq!((request.frame_type, request.flags), (frame_type::AGREEMENT, flags));
                    assert_eq!(request.body.as_slice(), registration::terms_digest());
                    assert!(data.lock().unwrap().is_none());
                    if cancel {
                        canceled.raise();
                    } else {
                        connection.close(
                            quinn::VarInt::from_u32(APPLICATION_ERROR_RESTART),
                            b"AP restart",
                        );
                    }
                    finished.await.unwrap();
                })
                .await
                .unwrap();
            })
        });
        if !bootstrap {
            remote.authenticate(&store).unwrap();
            store.accept_agreement(store.account().id, registration::terms_digest()).unwrap();
        }
        let started = Instant::now();
        let error =
            if bootstrap { remote.sync(&store) } else { remote.reconnect(&store) }.unwrap_err();
        done.send(()).unwrap();
        server.join().unwrap();
        let retry =
            remote.reconnect.as_ref().expect("the agreement request interrupted an active retry");
        assert_eq!(retry.bootstrap, bootstrap);
        if !bootstrap {
            assert!(retry.account);
            assert_eq!(retry.scopes, [scope]);
        }
        if cancel {
            assert!(error.downcast_ref::<CanceledOperation>().is_some(), "{error}");
            assert_eq!(retry.failures, 0);
            assert!(retry.after <= Instant::now());
        } else {
            assert!(
                error
                    .downcast_ref::<ClientError>()
                    .is_some_and(ClientError::is_connection_retryable),
                "{error}"
            );
            assert_eq!(retry.failures, 1);
            let backoff = Duration::from_millis(250);
            assert!((started + backoff..=Instant::now() + backoff).contains(&retry.after));
        }
        assert!(remote.connection.is_none() && remote.sync_connection.lock().unwrap().is_none());
        assert!(!store.agreement_recorded().unwrap());
        assert_eq!(store.agreement_pending().unwrap(), !bootstrap);
        remote.disconnect();
    }
}

#[test]
fn canceling_a_join_waiting_for_its_envelope_returns_remote_without_disconnect() {
    let creator_dir = tempfile::tempdir().unwrap();
    let mut creator = Engine::open(creator_dir.path()).unwrap();
    let root = creator.services.store.create_volume("Shared", true).unwrap();
    let session = creator.create_session(crate::engine::tests::button_package(), root.id).unwrap();
    let link = creator.invitation(&session.id).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    engine.services.store.preview_invitation(&link, engine.channel()).unwrap();
    let ap = testing::silent_authenticating_server();
    let remote = engine.remote.as_mut().unwrap();
    remote.set_test_transport(ap.config);
    remote.identity = preissued_identity(&engine.services.store);
    remote.connect(&engine.services.store).unwrap();
    let connection = remote.connection.clone().unwrap();
    let retry_volume = Id::generate().unwrap();
    remote
        .initializations
        .failed(retry_volume, "waiting for initial database synchronization", Instant::now())
        .unwrap();
    engine.synchronization = Some(
        Synchronization::new(
            &engine.services.store,
            ClientScope::FullClient,
            crate::trace::Interval::new("replication.sync"),
        )
        .unwrap(),
    );
    engine.pending_join = Some(session.id.clone());
    engine.start_network_work();
    ap.request_held.recv_timeout(RETURN_BY).unwrap();
    engine.dispatch_reply(Request::CancelTransfer).unwrap();
    collect_until(&mut engine, |engine| engine.remote.is_some());
    assert_eq!(engine.transfer_result.kind, TransferKind::Paused);
    assert!(engine.synchronization.is_some());
    assert!(engine.services.store.session(&session.id).is_err());
    assert!(!engine.updates.contains_key(&session.id));
    let remote = engine.remote.as_ref().unwrap();
    assert!(Arc::ptr_eq(remote.connection.as_ref().unwrap(), &connection));
    assert!(!connection.is_closed());
    assert!(remote.initializations.wake_after(Instant::now()).is_some());
    engine.disconnect_network().unwrap();
    assert!(ap.closed.recv_timeout(RETURN_BY).unwrap());
}

#[test]
fn canceled_bootstrap_admission_preserves_existing_live_state_and_transport() {
    // This fixture represents an already registered owner. Hold only the next
    // admission read, which can be canceled without closing live replication.
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root = engine.services.store.create_volume("Root", true).unwrap();
    let session = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
    let store = engine.services.store.clone();
    for pending in store.pending_operations().unwrap() {
        store.acknowledge_pending(&pending).unwrap();
    }
    let mut remote = engine.remote.take().unwrap();
    let ap = testing::silent_authenticating_server();
    remote.set_test_transport(ap.config);
    remote.identity = preissued_identity(&store);
    remote.connect(&store).unwrap();
    remote.ensure_live(&store).unwrap();
    let publisher = remote.live.as_ref().unwrap().publisher();
    let connection = remote.connection.clone().unwrap();
    let canceled = Arc::new(Canceled::default());
    remote.set_cancellation(Some(canceled.clone()));
    let apps = directory.path().join(crate::paths::BUILDS);
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = remote.bootstrap(&store, &session, &apps.join(&session.resolved_build_id));
        done.send((remote, result)).unwrap();
    });
    ap.request_held.recv_timeout(RETURN_BY).unwrap();
    canceled.raise();
    let (mut remote, result) = completed.recv_timeout(RETURN_BY).unwrap();
    assert!(result.unwrap_err().downcast_ref::<CanceledOperation>().is_some());
    assert!(Arc::ptr_eq(&publisher, &remote.live.as_ref().unwrap().publisher()));
    assert!(Arc::ptr_eq(&connection, remote.connection.as_ref().unwrap()));
    assert!(!connection.is_closed());
    remote.disconnect();
    assert!(ap.closed.recv_timeout(RETURN_BY).unwrap());
}

#[test]
fn canceling_an_acknowledged_build_download_removes_staging_and_keeps_transport() {
    let directory = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(directory.path()).unwrap();
    let root = engine.services.store.create_volume("Root", true).unwrap();
    let session = engine.create_session(crate::engine::tests::button_package(), root.id).unwrap();
    let store = engine.services.store.clone();
    let path = directory.path().join(crate::paths::BUILDS).join(&session.resolved_build_id);
    fs::remove_file(&path).unwrap();
    let (config, held, stopped, server) = held_build_download();
    let mut remote = engine.remote.take().unwrap();
    remote.set_test_transport(config);
    remote.identity = preissued_identity(&store);
    remote.connect(&store).unwrap();
    let connection = remote.connection.clone().unwrap();
    let canceled = Arc::new(Canceled::default());
    remote.set_cancellation(Some(canceled.clone()));
    let target = path.clone();
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = remote.ensure_build(&store, &session, &target);
        done.send((remote, result)).unwrap();
    });
    held.recv_timeout(RETURN_BY).unwrap();
    canceled.raise();
    let (mut remote, result) = completed.recv_timeout(RETURN_BY).unwrap();
    assert!(result.unwrap_err().downcast_ref::<CanceledOperation>().is_some());
    stopped.recv_timeout(RETURN_BY).unwrap();
    assert!(!path.exists());
    assert!(
        !fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
            entry.unwrap().file_name().to_string_lossy().starts_with(".download-")
        })
    );
    assert!(Arc::ptr_eq(&connection, remote.connection.as_ref().unwrap()));
    assert!(!connection.is_closed());
    remote.disconnect();
    server.join().unwrap();
}

fn held_build_download() -> (
    ClientConfig,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let (runtime, endpoint, config) = testing::loopback_ap();
    let (hold, held) = std::sync::mpsc::channel();
    let (stop, stopped) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        runtime.block_on(async {
            tokio::time::timeout(RETURN_BY * 2, async {
                let connection = endpoint.accept().await.unwrap().await.unwrap();
                let (mut auth, auth_receive) = connection.accept_bi().await.unwrap();
                let valid = Frame::new(frame_type::HELLO, 0, [], [], []).unwrap();
                auth.write_all(&testing::authentication_wire(valid)).await.unwrap();
                let (mut send, receive, frame) =
                    testing::next_request_with_receipt(&connection).await.unwrap();
                let request: ServiceRequest = serde_json::from_slice(&frame.body).unwrap();
                assert!(matches!(request, ServiceRequest::GetBlob { .. }));
                let response = ServiceResponse {
                    timing: Default::default(),
                    ok: true,
                    operation_id: request.operation_id().to_owned(),
                    error: None,
                    code: None,
                    denial: None,
                    data: Some(serde_json::json!({"content_length": 2})),
                };
                let header = Frame::new(
                    frame_type::DIRECT,
                    service::PROTOCOL,
                    SERVICE_DESTINATION,
                    SERVICE_DESTINATION,
                    serde_json::to_vec(&response).unwrap(),
                )
                .unwrap();
                send.write_all(&header.encode().unwrap()).await.unwrap();
                let chunk = Frame::new(
                    frame_type::DIRECT,
                    0,
                    SERVICE_DESTINATION,
                    SERVICE_DESTINATION,
                    [1],
                )
                .unwrap();
                send.write_all(&chunk.encode().unwrap()).await.unwrap();
                hold.send(()).unwrap();
                // The client must stop this partially received stream, without a
                // connection close or the ordinary 60-second response timeout.
                send.stopped().await.unwrap();
                assert!(connection.close_reason().is_none());
                stop.send(()).unwrap();
                connection.closed().await;
                drop((auth, auth_receive, receive));
            })
            .await
            .unwrap();
        })
    });
    (config, held, stopped, server)
}
