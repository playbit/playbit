//! First World or Shared use may register a session after the synchronization registration cut.
#![allow(clippy::unwrap_used, reason = "Tests assert isolated fixture operations.")]
use super::*;
use crate::network::ap::{Frame, frame_type, operation::SERVICE_DESTINATION, service, testing};
use crate::package::{AutomaticVolumeRole as Role, Package, VolumeInitializers};
use crate::store::{InitialDatabase, InitialDatabaseKind, RootInitialization, SessionCreation};

fn lazy_session(store: &LocalStore, profile: &Path, role: Role) -> Session {
    let declaration = RootInitialization {
        permissions: Default::default(),
        databases: vec![InitialDatabase {
            id: crate::store::EntryId::from_bytes([8; 15]).unwrap(),
            name: "data".into(),
            kind: InitialDatabaseKind::KvDb {},
        }],
        blobs: vec![],
    };
    let package = crate::package::signature::testing::signed(
        store,
        Package::create(
            "late-root".into(),
            "Late Root".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::new(),
            &VolumeInitializers::single(role, declaration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let build = package.id().unwrap();
    let builds = profile.join(crate::paths::BUILDS);
    std::fs::create_dir_all(&builds).unwrap();
    let path = builds.join(&build);
    std::fs::write(&path, package.to_bytes().unwrap()).unwrap();
    store.remember_imported_app(&package).unwrap();
    store.queue_build_upload(&build, path).unwrap();
    let app = package.identity().unwrap().clone();
    store
        .create_session_binding(SessionCreation {
            root_volume: app.volume_id(Role::Root, None, None, None).unwrap(),
            app,
            selector: crate::store::BuildSelector::Exact { build_id: build.clone() },
            resolved_build_id: build,
            user_volume: None,
            sync_shared: role == Role::Session,
        })
        .unwrap()
}

fn connected_remote(
    directory: &Path,
    store: &LocalStore,
    config: crate::network::ap::ClientConfig,
) -> (Remote, crate::engine::completion::Completion) {
    let completion = crate::engine::completion::Completion::unix().unwrap();
    let mut remote = Remote::open(directory, completion.wake.clone()).unwrap();
    let mut identity =
        ClientIdentity::from_seed("late-root@test.invalid", "Test", store.account_seed().unwrap())
            .unwrap();
    identity.auth_token = store.account().id.as_bytes().iter().copied().chain([0; 56]).collect();
    identity.instance_id = Id::from_bytes([1; 16]).to_string().into_bytes();
    identity.ca_certificate = vec![1];
    identity.instance_certificate = vec![1];
    remote.identity = identity;
    remote.set_test_transport(config);
    remote.connect(store).unwrap();
    (remote, completion)
}

#[test]
fn first_use_after_registration_phase_precedes_remote_session_admission() {
    // Finish the real registration phase, then use World or Shared. Exercise each
    // later consumer against an AP that requires registration, rejects it on demand,
    // and preserves a genuine missing-record error after successful registration.
    for (consumer, role) in [
        ("listeners", Role::Root),
        ("session", Role::Root),
        ("resolution", Role::Root),
        ("files", Role::Root),
        ("owner", Role::Root),
        ("late-session", Role::Root),
        ("canceled-registration", Role::Root),
        ("unsupported-shared", Role::Session),
        ("invalid-shared", Role::Session),
        ("listeners", Role::Session),
        ("session", Role::Session),
        ("resolution", Role::Session),
    ] {
        for reject_registration in [false, true] {
            if reject_registration
                && matches!(
                    consumer,
                    "owner"
                        | "files"
                        | "late-session"
                        | "canceled-registration"
                        | "unsupported-shared"
                        | "invalid-shared"
                )
            {
                continue;
            }
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(LocalStore::open(directory.path()).unwrap());
            let session =
                (consumer != "late-session").then(|| lazy_session(&store, directory.path(), role));
            let expected = Arc::new(Mutex::new(session.clone()));
            let server_expected = expected.clone();
            let (hold, held) = std::sync::mpsc::sync_channel(1);
            let (release, released) = tokio::sync::oneshot::channel();
            let (runtime, endpoint, config) = testing::loopback_ap();
            let server = std::thread::spawn(move || {
                runtime.block_on(async move {
                    tokio::time::timeout(Duration::from_secs(10), async move {
                        let connection = endpoint.accept().await.unwrap().await.unwrap();
                        let (mut auth, auth_read) = connection.accept_bi().await.unwrap();
                        auth.write_all(&testing::authentication_wire(
                            Frame::new(frame_type::HELLO, 0, [], [], []).unwrap(),
                        ))
                        .await
                        .unwrap();
                        let mut streams = vec![(auth, auth_read)];
                        let mut registered = false;
                        let mut released = Some(released);
                        let mut requests = Vec::new();
                        while let Some((mut send, mut read, frame)) =
                            testing::next_request_with_receipt(&connection).await
                        {
                            let request = service::admit_request(
                                service::PROTOCOL,
                                &frame.body,
                                &["shared-session.v1"],
                            )
                            .unwrap();
                            let mut error = None;
                            let mut code = "operation_failed";
                            let mut data = None;
                            let name = match &request {
                                ServiceRequest::Describe { .. } => {
                                    assert_eq!(role, Role::Session, "World remains baseline");
                                    let mut description = service::ServiceDescription::baseline();
                                    description.features = if consumer == "unsupported-shared" {
                                        vec![]
                                    } else {
                                        vec!["shared-session.v1".into()]
                                    };
                                    data = Some(serde_json::to_value(description).unwrap());
                                    "describe"
                                }
                                ServiceRequest::UploadBuild { content_length, .. } => {
                                    assert!(*content_length < 65536);
                                    assert_eq!(
                                        read.read_to_end(65536).await.unwrap().len() as u64,
                                        *content_length
                                    );
                                    "upload"
                                }
                                ServiceRequest::RegisterVolume { volume_id, .. } => {
                                    assert_eq!(
                                        volume_id,
                                        &server_expected
                                            .lock()
                                            .unwrap()
                                            .as_ref()
                                            .unwrap()
                                            .replica_volume()
                                            .to_string()
                                    );
                                    "volume"
                                }
                                ServiceRequest::RegisterSession {
                                    root_volume_id,
                                    shared_volume_id,
                                    session_id,
                                    ..
                                } => {
                                    let expected = server_expected.lock().unwrap();
                                    let expected = expected.as_ref().unwrap();
                                    assert_eq!(root_volume_id, &expected.root_volume.to_string());
                                    assert_eq!(
                                        shared_volume_id,
                                        &expected.shared_volume.map(|id| id.to_string())
                                    );
                                    assert_eq!(session_id, &expected.id);
                                    if matches!(consumer, "unsupported-shared" | "invalid-shared") {
                                        error = Some("invalid AP operation request");
                                        code = "invalid_request";
                                    } else if reject_registration {
                                        error = Some("fixture registration rejected");
                                    } else {
                                        registered = true;
                                    }
                                    "register"
                                }
                                ServiceRequest::PeerListen { .. } => {
                                    if !registered {
                                        error = Some("registration not found");
                                    }
                                    "listen"
                                }
                                ServiceRequest::OpenSession { .. } => {
                                    // The service may genuinely lose a record even after
                                    // acknowledgment. That error must remain visible.
                                    error = Some("registration not found");
                                    "open"
                                }
                                ServiceRequest::ResolveSession { .. } => {
                                    error = Some("registration not found");
                                    "resolve"
                                }
                                other => panic!(
                                    "unexpected request kind: {:?}",
                                    std::mem::discriminant(other)
                                ),
                            };
                            assert!(requests.len() < 16);
                            requests.push(name);
                            let response = ServiceResponse {
                                timing: Default::default(),
                                ok: error.is_none(),
                                operation_id: request.operation_id().into(),
                                error: error.map(Into::into),
                                code: error.map(|_| code.into()),
                                denial: None,
                                data,
                            };
                            if consumer == "canceled-registration" && name == "register" {
                                hold.send(()).unwrap();
                                released.take().unwrap().await.unwrap();
                            }
                            send.write_all(
                                &Frame::new(
                                    frame_type::DIRECT,
                                    service::PROTOCOL,
                                    SERVICE_DESTINATION,
                                    SERVICE_DESTINATION,
                                    serde_json::to_vec(&response).unwrap(),
                                )
                                .unwrap()
                                .encode()
                                .unwrap(),
                            )
                            .await
                            .unwrap();
                            if name != "listen" || error.is_some() {
                                send.finish().unwrap();
                            }
                            streams.push((send, read));
                        }
                        requests
                    })
                    .await
                    .unwrap()
                })
            });
            let (mut remote, _completion) = connected_remote(directory.path(), &store, config);
            let mut state = Synchronization::new(
                &store,
                ClientScope::FullClient,
                crate::trace::Interval::new("replication.sync"),
            )
            .unwrap();
            assert!(state.volumes.is_empty());
            assert!(!state.advance(&mut remote, &store).unwrap());
            assert!(store.pending_operations().unwrap().is_empty());
            // This case creates and queues a genuinely new signed build/session
            // after the cut, so build_uploaded cannot skip its dependency upload.
            let session = session.unwrap_or_else(|| lazy_session(&store, directory.path(), role));
            *expected.lock().unwrap() = Some(session.clone());
            assert!(store.is_local_session(&session.id).unwrap());
            store.open_automatic_volume(&session.id, role).unwrap();
            assert_eq!(store.is_local_session(&session.id).unwrap(), role == Role::Session);
            let pending = store.pending_operations().unwrap().into_iter().find(|pending| matches!(
                &pending.operation, PendingKind::RegisterSession { session: pending, .. } if pending.id == session.id
            )).unwrap();
            let entry = crate::store::volume_index::EntryAddress {
                volume: session.replica_volume(),
                entry: crate::store::EntryId::from_bytes([8; 15]).unwrap(),
            };
            let access = Access::session_version(
                store.account().id,
                &session.id,
                &session.resolved_build_id,
            );
            store.kv_put(&access, entry, b"retained", b"local first use").unwrap();
            let unrelated = store.create_volume("Unrelated pending registration", true).unwrap();
            let sibling = (consumer != "listeners" && role == Role::Root).then(|| {
                store
                    .create_session_binding(SessionCreation {
                        app: session.app.clone(),
                        selector: session.selector.clone(),
                        resolved_build_id: session.resolved_build_id.clone(),
                        root_volume: session.root_volume,
                        user_volume: None,
                        sync_shared: false,
                    })
                    .unwrap()
            });
            let result = match consumer {
                "listeners" => {
                    assert!(!state.advance(&mut remote, &store).unwrap());
                    if state.first_error.is_none() {
                        assert!(!state.advance(&mut remote, &store).unwrap());
                    }
                    assert!(
                        state.synchronized.is_empty(),
                        "failed admission cannot authorize file sync"
                    );
                    state.first_error.take().map_or(Ok(()), Err)
                }
                "session" | "late-session" | "unsupported-shared" | "invalid-shared" => {
                    remote.sync_session(&store, &session)
                }
                "canceled-registration" => {
                    let canceled = Arc::new(crate::engine::cancellation::Canceled::default());
                    remote.set_cancellation(Some(canceled.clone()));
                    std::thread::scope(|scope| {
                        let task = scope.spawn(|| remote.sync_session(&store, &session));
                        held.recv_timeout(Duration::from_secs(10)).unwrap();
                        assert!(store.pending_operations().unwrap().contains(&pending));
                        canceled.raise();
                        // An accepted durable registration still owns its ACK.
                        // Cancellation stops the following OpenSession, not this commit.
                        release.send(()).unwrap();
                        task.join().unwrap()
                    })
                }
                "resolution" => remote.resolve_session(&store, &session.id).map(|_| ()),
                "files" => {
                    remote.sync_blob_selected(&store, session.root_volume, std::iter::empty())
                }
                "owner" => {
                    remote.sync_owned_volume(&store, store.volume(session.root_volume).unwrap())
                }
                _ => unreachable!(),
            };
            if consumer == "canceled-registration" {
                assert!(result.as_ref().unwrap_err().is::<CanceledOperation>());
            }
            let error = result.err().map(|error| error.to_string());
            if consumer == "unsupported-shared" {
                let expected = "This session needs an updated sync service. Your local data has been kept. Try again after the service is updated.";
                assert_eq!(error.as_deref(), Some(expected));
                // The cached absent capability fails an explicit retry without
                // another discovery request or changing the retained operation.
                assert_eq!(
                    remote.sync_session(&store, &session).unwrap_err().to_string(),
                    expected
                );
            }
            if reject_registration {
                // The backing registration succeeded. A rejected session must not
                // block owner-only file work or consume another session's intent.
                remote
                    .sync_blob_selected(&store, session.replica_volume(), std::iter::empty())
                    .unwrap();
            }
            // A selected dependency cannot publish an unrelated volume or bypass
            // the provider's existing concrete scope. Rejection retains its intent.
            *remote.scope.write() = ClientScope::Provider(vec![]);
            assert!(
                remote
                    .sync_registration_dependencies(&store, unrelated.id, None)
                    .unwrap_err()
                    .to_string()
                    .contains("outside client scope")
            );
            remote.disconnect();
            let requests = server.join().unwrap();
            let session_admission = !matches!(consumer, "owner" | "files");
            let mut expected = if consumer == "late-session" {
                vec!["volume", "upload"]
            } else {
                vec!["upload", "volume"]
            };
            if role == Role::Session {
                expected.push("describe");
            }
            if session_admission && consumer != "unsupported-shared" {
                expected.push("register");
            }
            let expected_error = if reject_registration {
                Some("fixture registration rejected")
            } else {
                match consumer {
                    "listeners" => {
                        expected.extend(["listen", "open"]);
                        Some("registration not found")
                    }
                    "session" | "late-session" => {
                        expected.push("open");
                        Some("registration not found")
                    }
                    "resolution" => {
                        expected.push("resolve");
                        Some("registration not found")
                    }
                    "unsupported-shared" => error.as_deref(),
                    "invalid-shared" => Some("invalid AP operation request"),
                    "canceled-registration" => Some(CANCELED),
                    "files" => None,
                    "owner" => Some("live replication is unavailable"),
                    _ => unreachable!(),
                }
            };
            assert_eq!(
                requests, expected,
                "{consumer}, {role:?}, reject={reject_registration}: {error:?}"
            );
            assert_eq!(error.as_deref(), expected_error, "{consumer}");
            let remaining = store.pending_operations().unwrap();
            let session_pending = reject_registration
                || !session_admission
                || matches!(consumer, "unsupported-shared" | "invalid-shared");
            assert_eq!(
                remaining.len(),
                1 + usize::from(sibling.is_some()) + usize::from(session_pending)
            );
            assert_eq!(remaining.contains(&pending), session_pending);
            if let Some(sibling) = sibling {
                assert!(remaining.iter().any(|op| matches!(&op.operation,
                    PendingKind::RegisterSession { session, .. } if session.id == sibling.id)));
            }
            assert!(remaining.iter().any(|pending| matches!(&pending.operation,
                PendingKind::RegisterVolume { volume } if volume.id == unrelated.id)));
            assert_eq!(store.session(&session.id).unwrap(), session);
            assert_eq!(
                store.kv_read(&access, entry, b"retained", 0, 64, None).unwrap().bytes,
                b"local first use"
            );
        }
    }
}
