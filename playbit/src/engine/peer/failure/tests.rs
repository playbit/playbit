#![allow(clippy::unwrap_used, reason = "Tests assert valid scoped private peer fixtures.")]
use super::*;

#[test]
fn failures_are_typed_bounded_and_do_not_accept_raw_text_or_mixed_fields() {
    // Exercise the actual sender formatter/encoder and receiving decoder.
    let volume = Id::from_bytes([1; 16]);
    let account = Id::from_bytes([2; 16]);
    let denied = crate::store::Error::AccessDenied(crate::store::Error::PRIVATE_FILE_OWNER_READ);
    assert!(matches!(
        Failure::decode(&encode(&denied).unwrap()).unwrap(),
        Failure::PrivateFileOwnerRead {}
    ));
    assert!(
        Failure::decode(br#"{"cause":"private_file_owner_read","message":"unscoped"}"#).is_err()
    );
    // Unstructured diagnostic text does not establish an owner's policy.
    let text = crate::store::Error::System(denied.to_string());
    assert!(matches!(Failure::decode(&encode(&text).unwrap()).unwrap(), Failure::Operation { .. }));
    let busy = crate::store::Error::LocalContention(personal::OWNER_BUSY);
    let bytes = encode(&busy).unwrap();
    assert_eq!(bytes.as_slice(), br#"{"cause":"private_file_owner_preparation_busy"}"#);
    assert!(matches!(
        Failure::decode(&bytes).unwrap(),
        Failure::PrivateFileOwnerPreparationBusy {}
    ));
    let diagnostic = crate::store::Error::System(busy.to_string());
    assert!(matches!(
        Failure::decode(&encode(&diagnostic).unwrap()).unwrap(),
        Failure::Operation { .. }
    ));
    let bytes = encode(&crate::store::Error::ParticipantRevoked { volume, account }).unwrap();
    assert!(matches!(Failure::decode(&bytes).unwrap(), Failure::ParticipantRevoked {
        volume: got_volume, account: got_account
    } if got_volume == volume && got_account == account));
    let mut mixed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    mixed["message"] = "fake reason".into();
    assert!(Failure::decode(&serde_json::to_vec(&mixed).unwrap()).is_err());
    mixed.as_object_mut().unwrap().remove("message");
    mixed["account"] = "malformed".into();
    assert!(Failure::decode(&serde_json::to_vec(&mixed).unwrap()).is_err());
    assert!(Failure::decode(b"access denied: revoked volume participant").is_err());
    assert!(Failure::decode(&vec![b' '; MAX_BYTES + 1]).is_err());
    let too_long = Failure::Operation { message: "x".repeat(MESSAGE_BYTES + 1) };
    assert!(Failure::decode(&serde_json::to_vec(&too_long).unwrap()).is_err());
    assert!(encode(&crate::store::Error::System("x".repeat(MESSAGE_BYTES + 1))).is_err());
    let escaped = encode(&crate::store::Error::System("\0".repeat(MESSAGE_BYTES))).unwrap();
    assert!(escaped.len() <= MAX_BYTES);
    assert!(matches!(Failure::decode(&escaped).unwrap(), Failure::Operation { message }
        if message.len() == MESSAGE_BYTES));
    assert_eq!(
        crate::runtime::store_bridge::error(crate::store::Error::ParticipantRevoked {
            volume,
            account
        }),
        playbit_api::abi::syscall::Error::AccessDenied
    );
}

#[test]
fn only_a_dependency_owner_denial_for_this_account_becomes_verified() {
    // Real descriptors decide owner authority; no test-supplied owner predicate.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let root = store.create_volume("Root", false).unwrap();
    let requested = store.create_volume("Personal", false).unwrap();
    let foreign = store.create_volume("Outside", false).unwrap();
    let account = store.account().id;
    let session = Session {
        id: crate::identifier::encode(&[0x44; 32]),
        selector: crate::store::BuildSelector::Exact {
            build_id: crate::identifier::encode(&[0xab; 32]),
        },
        resolved_build_id: crate::identifier::encode(&[0xab; 32]),
        root_volume: root.id,
        shared_volume: None,
        app: crate::package::AppIdentity { publisher: account, app_id: "test".into() },

        creator: account,
        update_authority: crate::store::UpdateAuthority::Creator,
    };
    let event = PeerEvent {
        live: false,
        build_id: session.resolved_build_id.clone(),
        request_id: crate::identifier::encode(&[0x11; 16]),
        requester_account_id: account.to_string(),
        responder_account_id: account.to_string(),
        session_id: session.id.clone(),
        volume_id: requested.id.to_string(),
        database_id: crate::store::DatabaseId::Index,
        direction: SyncDirection::Pull,
    };
    let denial = |volume, account| -> Result<()> {
        Err(Failure::ParticipantRevoked { volume, account }.into())
    };
    for volume in [root.id, requested.id] {
        let error = qualify(denial(volume, account), &store, &session, &event).unwrap_err();
        assert!(error.downcast_ref::<VerifiedDenial>().is_some());
    }
    for result in [denial(foreign.id, account), denial(root.id, Id::from_bytes([9; 16]))] {
        let error = qualify(result, &store, &session, &event).unwrap_err();
        assert!(error.downcast_ref::<VerifiedDenial>().is_none());
    }
    let mut non_owner = event.clone();
    non_owner.responder_account_id = Id::from_bytes([9; 16]).to_string();
    let error = qualify(denial(root.id, account), &store, &session, &non_owner).unwrap_err();
    assert!(error.downcast_ref::<VerifiedDenial>().is_none());
    let ordinary = Err(Failure::Operation { message: "participant revoked".into() }.into());
    let error = qualify::<()>(ordinary, &store, &session, &event).unwrap_err();
    assert!(error.downcast_ref::<VerifiedDenial>().is_none());
    assert!(
        store.session_denial(&session.id).unwrap().is_none(),
        "only the transfer coordinator persists a verified denial, never decoding"
    );
}

#[test]
fn a_qualified_old_transfer_denial_cannot_fence_an_adopted_build() {
    // Qualification retains the authenticated transfer's captured binding.
    let directory = tempfile::tempdir().unwrap();
    let store = LocalStore::open(directory.path()).unwrap();
    let root = store.create_volume("Root", false).unwrap();
    let account = store.account().id;
    let build = crate::identifier::encode(&[0xab; 32]);
    let captured = store
        .create_session(
            &crate::package::AppIdentity { publisher: store.account().id, app_id: "test".into() },
            crate::store::BuildSelector::Exact { build_id: build.clone() },
            &build,
            root.id,
        )
        .unwrap();
    let event = PeerEvent {
        live: false,
        build_id: build,
        request_id: crate::identifier::encode(&[0x11; 16]),
        requester_account_id: account.to_string(),
        responder_account_id: account.to_string(),
        session_id: captured.id.clone(),
        volume_id: root.id.to_string(),
        database_id: crate::store::DatabaseId::Index,
        direction: SyncDirection::Pull,
    };
    let mut adopted = captured.clone();
    adopted.resolved_build_id = crate::identifier::encode(&[0xcd; 32]);
    adopted.selector =
        crate::store::BuildSelector::Exact { build_id: adopted.resolved_build_id.clone() };
    store.adopt_session(&captured, &adopted).unwrap();
    let denial = || {
        let bytes =
            encode(&crate::store::Error::ParticipantRevoked { volume: root.id, account }).unwrap();
        Err::<(), _>(Failure::decode(&bytes).unwrap().into())
    };
    let error = qualify(denial(), &store, &captured, &event).unwrap_err();
    assert!(error.downcast_ref::<VerifiedDenial>().is_some());
    assert!(!store.record_session_denial(&captured, "session participant revoked").unwrap());
    assert!(store.session_denial(&captured.id).unwrap().is_none());
    assert_eq!(store.session_denial_generation(&adopted).unwrap(), 0);
    assert!(store.record_session_denial(&adopted, "current binding denied").unwrap());
    assert!(store.session_denial(&adopted.id).unwrap().is_some());
}
