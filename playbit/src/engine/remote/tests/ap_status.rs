use super::*;

#[test]
fn transport_closure_updates_status_without_any_replica_or_request() {
    for (code, expected) in [
        (crate::network::ap::APPLICATION_ERROR_RESTART, ApConnectionState::Connecting),
        (42, ApConnectionState::Offline),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = crate::engine::Engine::open(directory.path()).unwrap();
        let (runtime, endpoint, config) = crate::network::ap::testing::loopback_ap();
        let (close, closed) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            runtime.block_on(async {
                tokio::time::timeout(RETURN_BY, async {
                    let connection = endpoint.accept().await.unwrap().await.unwrap();
                    let (mut send, receive) = connection.accept_bi().await.unwrap();
                    let valid = crate::network::ap::Frame::new(
                        crate::network::ap::frame_type::HELLO,
                        0,
                        vec![],
                        vec![],
                        vec![],
                    )
                    .unwrap();
                    send.write_all(&crate::network::ap::testing::authentication_wire(valid))
                        .await
                        .unwrap();
                    closed.await.unwrap();
                    connection.close(quinn::VarInt::from_u32(code), b"status test");
                    endpoint.wait_idle().await;
                    drop((send, receive));
                })
                .await
                .unwrap();
            })
        });
        let remote = engine.remote.as_mut().unwrap();
        remote.set_test_transport(config);
        remote.identity = preissued_identity(&engine.services.store);
        remote.authenticate(&engine.services.store).unwrap();
        assert!(remote.live.is_none(), "status must not depend on an installed replica");
        assert_eq!(engine.status_snapshot().unwrap().ap_connection.state, ApConnectionState::Live);
        let token = engine.change_token();
        close.send(()).unwrap();
        collect_until(&mut engine, |engine| {
            engine.network.connection_status.snapshot().state == expected
        });
        assert_ne!(token, engine.change_token(), "closure must wake status observers");
        let status = engine.status_snapshot().unwrap().ap_connection;
        assert_eq!(status.state, expected);
        assert_eq!(status.hostname, "localhost");
        engine.disconnect_network().unwrap();
        server.join().unwrap();
    }
}
