#![allow(clippy::unwrap_used, reason = "Tests qualify actual stored bindings and service frames.")]
use super::*;
use crate::network::ap::{
    Frame, frame_type,
    operation::{SERVICE_DESTINATION, ServiceDenial, ServiceResponse},
};

#[test]
fn local_scope_files_remain_readable_without_remote_transfer_authority() {
    // Goal: device-only file access stays useful while remote transfer fails closed.
    // Method: read a real Local blob, reject its transfer scope and compare an ordinary volume.
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let package = crate::package::Package::new(
        "test".into(),
        "Test".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        Default::default(),
    )
    .unwrap();
    let package = crate::package::signature::testing::signed(&store, package).unwrap();
    let builds = directory.path().join(crate::paths::BUILDS);
    std::fs::create_dir_all(&builds).unwrap();
    std::fs::write(builds.join(package.id().unwrap()), package.to_bytes().unwrap()).unwrap();
    store.remember_imported_app(&package).unwrap();
    let local = store
        .open_app_volume(package.identity().unwrap(), crate::package::AutomaticVolumeRole::Local)
        .unwrap();
    let access = Access::owner(store.account().id);
    let entry = store.create_blob(&access, local.id, "file", b"local bytes").unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let transport = BlobTransport::new_local(runtime.handle().clone(), Default::default());
    let (_, file) = transport
        .verified_file(
            store.clone(),
            access,
            None,
            EntryAddress { volume: local.id, entry: entry.id },
        )
        .unwrap();
    assert_eq!(file.read_range(0, 32).unwrap(), b"local bytes");
    assert!(shared_scope(&store, None, local.id).is_err());
    let ordinary = store.create_volume("Ordinary", false).unwrap();
    assert_eq!(shared_scope(&store, None, ordinary.id).unwrap(), None);
}

#[test]
fn ap_operation_denial_persists_only_for_the_original_current_blob_binding() {
    // Supplied file failures cannot invalidate an unrelated or updated session.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let root = store.create_volume("Files", true).unwrap();
    let build = crate::identifier::encode(&[0x11; 32]);
    let session = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            crate::store::BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let request = ServiceRequest::DownloadBlob {
        operation_id: crate::identifier::encode(&[0x22; 16]),
        session_id: Some(session.id.clone()),
        build_id: Some(build.clone()),
        version: BlobVersion {
            address: crate::store::volume_index::EntryAddress {
                volume: root.id,
                entry: crate::store::EntryId::from_bytes([0x33; 15]).unwrap(),
            },
            revision: crate::store::EntryId::from_bytes([0x44; 15]).unwrap(),
            len_bytes: 0,
            digest: [1; 32],
            encrypted_key: [2; 32],
        },
    };
    let denial = |account, volume, session_id, build_id| ServiceDenial::ParticipantRevoked {
        account_id: account,
        volume_id: volume,
        session_id,
        build_id,
    };
    let fail = |denial| -> Result<()> {
        let response = ServiceResponse {
            ok: false,
            operation_id: request.operation_id().into(),
            error: Some("session participant revoked".into()),
            code: Some("operation_failed".into()),
            denial,
            data: None,
            timing: Default::default(),
        };
        let frame = Frame::new(
            frame_type::DIRECT,
            crate::network::ap::service::PROTOCOL,
            SERVICE_DESTINATION,
            b"client",
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
        crate::network::ap::service::response(frame, request.operation_id()).map(|_| ())
    };
    for evidence in [
        None,
        Some(denial(store.account().id, root.id, None, None)),
        Some(denial(
            Id::from_bytes([9; 16]),
            root.id,
            Some(session.id.clone()),
            Some(build.clone()),
        )),
        Some(denial(
            store.account().id,
            Id::from_bytes([9; 16]),
            Some(session.id.clone()),
            Some(build.clone()),
        )),
        Some(denial(
            store.account().id,
            root.id,
            Some(crate::identifier::encode(&[0x99; 32])),
            Some(build.clone()),
        )),
        Some(denial(
            store.account().id,
            root.id,
            Some(session.id.clone()),
            Some(crate::identifier::encode(&[0x99; 32])),
        )),
    ] {
        assert!(checked_session_result(&store, &request, fail(evidence)).is_err());
        assert!(store.session_denial(&session.id).unwrap().is_none());
    }
    let valid =
        denial(store.account().id, session.root_volume, Some(session.id.clone()), Some(build));
    let mut stale = request.clone();
    if let ServiceRequest::DownloadBlob { build_id, .. } = &mut stale {
        *build_id = Some(crate::identifier::encode(&[0x99; 32]));
    }
    assert!(checked_session_result(&store, &stale, fail(Some(valid.clone()))).is_err());
    assert!(store.session_denial(&session.id).unwrap().is_none());
    let mut updated = session.clone();
    updated.resolved_build_id = crate::identifier::encode(&[0x99; 32]);
    updated.selector =
        crate::store::BuildSelector::Exact { build_id: updated.resolved_build_id.clone() };
    store.adopt_session(&session, &updated).unwrap();
    assert!(!store.receive_ap_session_denial(&valid, &session).unwrap());
    assert!(checked_session_result(&store, &request, fail(Some(valid.clone()))).is_err());
    assert!(store.session_denial(&session.id).unwrap().is_none());
    store.adopt_session(&updated, &session).unwrap();
    assert!(matches!(
        checked_session_result(&store, &request, fail(Some(valid)))
            .unwrap_err()
            .downcast_ref::<Error>(),
        Some(Error::AccessDenied("session participant revoked"))
    ));
    assert!(store.session_denial(&session.id).unwrap().is_some());
    drop(store);
    assert!(
        LocalStore::open(directory.path()).unwrap().session_denial(&session.id).unwrap().is_some()
    );
}
