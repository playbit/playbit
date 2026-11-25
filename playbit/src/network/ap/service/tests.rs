//! Wire fixtures prove the same validation without a transport or runtime.
#![allow(clippy::unwrap_used, reason = "Tests assert bounded protocol fixtures.")]
use super::*;

#[test]
fn all_operation_requests_keep_the_same_validated_wire_shape() {
    // Existing independent operation fixtures cover every supported request.
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../../tests/fixtures/ap-requests.json")).unwrap();
    assert_eq!(fixtures.len(), 53);
    for fixture in fixtures {
        let typed: ServiceRequest = serde_json::from_value(fixture.clone()).unwrap();
        let frame = request(&typed).unwrap();
        assert_eq!(frame.frame_type, frame_type::DIRECT);
        assert_eq!(frame.flags, PROTOCOL);
        assert!(frame.source.is_empty());
        assert_eq!(admit_request(PROTOCOL, &frame.body, OPTIONAL_FEATURES).unwrap(), typed);
        assert_eq!(frame.destination, SERVICE_DESTINATION);
        let mut body: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        body.as_object_mut().unwrap().remove("required_features");
        assert_eq!(serde_json::from_value::<ServiceRequest>(body).unwrap(), typed);
        assert_eq!(Frame::decode(&frame.encode().unwrap()).unwrap(), frame);
        let mut invalid = fixture;
        invalid["operation_id"] = serde_json::json!("invalid");
        assert!(request(&serde_json::from_value(invalid).unwrap()).is_err());
    }
}

#[test]
fn display_name_lookup_requires_the_feature_and_bounded_distinct_ids() {
    use crate::network::ap::operation::ACCOUNT_DISPLAY_NAMES_MAX;
    use crate::store::Id;
    let id = Id::from_bytes([1; 16]);
    let operation = id.to_string();
    let lookup = |account_ids| ServiceRequest::GetAccountDisplayNames {
        operation_id: operation.clone(),
        account_ids,
    };
    let valid = (0..ACCOUNT_DISPLAY_NAMES_MAX as u8)
        .map(|byte| Id::from_bytes([byte; 16]))
        .collect::<Vec<_>>();
    let frame = request(&lookup(valid.clone())).unwrap();
    assert!(admit_request(PROTOCOL, &frame.body, &[]).is_err());
    assert!(admit_request(PROTOCOL, &frame.body, &[ACCOUNT_DISPLAY_NAMES]).is_ok());
    for invalid in [vec![], vec![id, id], [valid.clone(), vec![Id::from_bytes([99; 16])]].concat()]
    {
        assert!(request(&lookup(invalid)).is_err());
    }
    let mut oversized = serde_json::to_value(lookup(valid)).unwrap();
    // The extra value is not an ID. The sequence bound must win before decoding it.
    oversized["account_ids"].as_array_mut().unwrap().push(serde_json::json!("0"));
    let bytes = serde_json::to_vec(&oversized).unwrap();
    let refusal = admit_request(PROTOCOL, &bytes, &[ACCOUNT_DISPLAY_NAMES]).unwrap_err();
    assert_eq!(refusal.code, "invalid_request");
    assert_eq!(refusal.operation_id.as_deref(), Some(operation.as_str()));
    assert!(
        serde_json::from_value::<ServiceRequest>(oversized)
            .unwrap_err()
            .to_string()
            .contains("invalid account display-name batch size")
    );
    let mut unexpected_field: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
    unexpected_field["other"] = serde_json::json!(true);
    assert!(
        admit_request(
            PROTOCOL,
            &serde_json::to_vec(&unexpected_field).unwrap(),
            &[ACCOUNT_DISPLAY_NAMES]
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ServiceRequest>(serde_json::json!({
            "op": "get_account_display_names",
            "operation_id": operation,
            "account_ids": ["0"]
        }))
        .is_err()
    );
}
fn reply(ok: bool, operation_id: &str, error: Option<&str>) -> Frame {
    Frame::new(
        frame_type::DIRECT,
        PROTOCOL,
        SERVICE_DESTINATION,
        b"client",
        serde_json::to_vec(&ServiceResponse {
            timing: Default::default(),
            ok,
            operation_id: operation_id.into(),
            error: error.map(String::from),
            code: Some("operation_failed".into()),
            denial: None,
            data: Some(serde_json::json!({"receipt":7})),
        })
        .unwrap(),
    )
    .unwrap()
}
#[test]
fn response_checks_service_identity_correlation_and_failure_before_returning_data() {
    // An unrelated successful reply is not an acknowledgment of this operation.
    let id = crate::identifier::encode(&[0x12; 16]);
    let accepted = response(reply(true, &id, None), &id).unwrap();
    assert_eq!(accepted.data.unwrap()["receipt"], 7);
    assert_eq!(
        response(reply(true, &id, None), &crate::identifier::encode(&[0x34; 16]))
            .unwrap_err()
            .to_string(),
        "AP response operation identity differs"
    );
    for error in [None, Some("access denied")] {
        assert_eq!(
            response(reply(false, &id, error), &id).unwrap_err().to_string(),
            error.unwrap_or("service operation failed")
        );
    }
    for kind in [frame_type::HELLO, frame_type::PUBLISH, frame_type::ERROR] {
        let mut frame = reply(true, &id, None);
        frame.frame_type = kind;
        frame.flags = 0;
        assert_eq!(response(frame, &id).unwrap_err().to_string(), "invalid AP service response");
    }
    let mut frame = reply(true, &id, None);
    frame.source = b"unrelated".to_vec();
    assert!(response(frame, &id).is_err());
    for body in [vec![], b"{".to_vec(), vec![255], b"{}".to_vec()] {
        let mut frame = reply(true, &id, None);
        frame.body = body;
        assert!(response(frame, &id).is_err());
    }
}

#[test]
fn missing_build_identity_requires_a_correlated_failure_without_denial() {
    use crate::store::Id;
    let operation = crate::identifier::encode(&[0x12; 16]);
    let other_operation = crate::identifier::encode(&[0x34; 16]);
    let missing = "missing authenticated build identity";
    let denial = ServiceDenial::ParticipantRevoked {
        account_id: Id::from_bytes([1; 16]),
        volume_id: Id::from_bytes([2; 16]),
        session_id: None,
        build_id: None,
    };
    for (code, message, denial, expected_operation, unavailable) in [
        ("operation_failed", missing, None, &operation, true),
        ("operation_failed", missing, None, &other_operation, false),
        ("operation_conflict", missing, None, &operation, false),
        ("operation_failed", "registration not found", None, &operation, false),
        ("operation_failed", missing, Some(denial), &operation, false),
    ] {
        let mut frame = reply(false, &operation, Some(message));
        let mut body: ServiceResponse = serde_json::from_slice(&frame.body).unwrap();
        body.code = Some(code.into());
        body.denial = denial;
        frame.body = serde_json::to_vec(&body).unwrap();
        let error = response(frame, expected_operation).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<ServiceFailure>()
                .is_some_and(ServiceFailure::is_missing_build_identity),
            unavailable
        );
    }
}

#[test]
fn service_data_moves_the_payload_and_rejects_other_senders() {
    // Raw transfer frames keep their payload allocation; consumers perform their
    // typed payload validation after checking this service envelope.
    let bytes = vec![7; 4096];
    let pointer = bytes.as_ptr();
    let frame = Frame::new(frame_type::DIRECT, 0, SERVICE_DESTINATION, b"client", bytes).unwrap();
    let result = data(frame).unwrap();
    assert_eq!(result.as_ptr(), pointer);
    for (kind, source) in
        [(frame_type::PUBLISH, SERVICE_DESTINATION), (frame_type::DIRECT, b"other".as_slice())]
    {
        let frame = Frame::new(kind, 0, source, b"client", [7]).unwrap();
        assert_eq!(data(frame).unwrap_err().to_string(), "invalid AP service data frame");
    }
}

#[test]
fn ap_operation_denial_requires_typed_failure_and_an_exact_current_binding() {
    // Decode the real service frame, then qualify every dependency and binding.
    use crate::store::Id;
    let account = Id::from_bytes([1; 16]);
    let root = Id::from_bytes([2; 16]);
    let outside = Id::from_bytes([3; 16]);
    let session = crate::identifier::encode(&[0x44; 32]);
    let build = crate::identifier::encode(&[0x55; 32]);
    let operation = crate::identifier::encode(&[0x66; 16]);
    let denial = ServiceDenial::ParticipantRevoked {
        account_id: account,
        volume_id: root,
        session_id: Some(session.clone()),
        build_id: Some(build.clone()),
    };
    let decode = |denial: Option<ServiceDenial>, ok| {
        let mut frame = reply(ok, &operation, Some("session participant revoked"));
        let mut body: ServiceResponse = serde_json::from_slice(&frame.body).unwrap();
        body.denial = denial;
        body.timing =
            serde_json::from_str(r#"[{"stage":"ap.future_authority","duration_ns":"1"}]"#).unwrap();
        frame.body = serde_json::to_vec(&body).unwrap();
        response(frame, &operation).unwrap_err()
    };
    let failure = decode(Some(denial.clone()), false);
    let evidence = failure.downcast_ref::<ServiceFailure>().unwrap().denial().unwrap();
    assert!(evidence.matches_session(account, &session, &build, [root]));
    for (account, session, build, dependencies) in [
        (Id::from_bytes([7; 16]), session.clone(), build.clone(), [root]),
        (account, crate::identifier::encode(&[0x77; 32]), build.clone(), [root]),
        (account, session.clone(), crate::identifier::encode(&[0x88; 32]), [root]),
        (account, session.clone(), build.clone(), [outside]),
    ] {
        assert!(!evidence.matches_session(account, &session, &build, dependencies));
    }
    assert!(decode(None, false).downcast_ref::<ServiceFailure>().unwrap().denial().is_none());
    assert!(decode(Some(denial.clone()), true).downcast_ref::<ServiceFailure>().is_none());
    for (field, value) in
        [("account_id", "invalid"), ("volume_id", "invalid"), ("message", "extra")]
    {
        let mut frame = reply(false, &operation, Some("session participant revoked"));
        let mut body: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        body["denial"] = serde_json::to_value(&denial).unwrap();
        body["denial"][field] = value.into();
        frame.body = serde_json::to_vec(&body).unwrap();
        assert!(
            response(frame, &operation).unwrap_err().downcast_ref::<ServiceFailure>().is_none()
        );
    }
    assert!(
        serde_json::to_vec(&denial).unwrap().len() <= super::super::operation::MAX_DENIAL_BYTES
    );
    let ServiceDenial::ParticipantRevoked { account_id, volume_id, .. } = denial;
    for (session_id, build_id) in [
        (Some(session.clone()), None),
        (None, Some(build)),
        (Some("X".repeat(64)), Some(crate::identifier::encode(&[0x55; 32]))),
        (Some("44".repeat(33)), Some(crate::identifier::encode(&[0x55; 32]))),
    ] {
        assert!(
            decode(
                Some(ServiceDenial::ParticipantRevoked {
                    account_id,
                    volume_id,
                    session_id,
                    build_id
                }),
                false
            )
            .downcast_ref::<ServiceFailure>()
            .is_none()
        );
    }
    let owner = ServiceDenial::ParticipantRevoked {
        account_id,
        volume_id,
        session_id: None,
        build_id: None,
    };
    assert!(owner.validate().is_ok());
    assert!(!owner.matches_session(account, &session, &"55".repeat(32), [root]));
}

#[test]
fn incompatible_reply_is_rejected_before_decoding_payload_or_denial() {
    let id = crate::identifier::encode(&[0x12; 16]);
    for version in [0, PROTOCOL + 1, u8::MAX] {
        let mut frame = reply(true, &id, None);
        frame.flags = version;
        frame.body = b"not a current service response".to_vec();
        assert!(response(frame, &id).unwrap_err().is::<IncompatibleProtocol>());
    }
    let frame = Frame::new(
        frame_type::ERROR,
        INCOMPATIBLE_FLAGS,
        SERVICE_DESTINATION,
        b"client",
        b"not a JSON service response",
    )
    .unwrap();
    assert!(response(frame, &id).unwrap_err().is::<IncompatibleProtocol>());
}

#[test]
fn independent_baseline_vectors_admit_envelopes_before_operation_semantics() {
    // Literal older/future requests test compatibility beyond local round trips.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/fixtures/ap-baseline.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let result = admit_request(
            case["major"].as_u64().unwrap() as u8,
            case["request"].as_str().unwrap().as_bytes(),
            &[],
        );
        let expected = case["expected"].as_str().unwrap();
        match expected {
            "accepted" => assert!(result.is_ok(), "{}", case["name"]),
            "invalid_envelope" => assert!(result.unwrap_err().operation_id.is_none()),
            code => {
                let failure = result.unwrap_err();
                assert_eq!(failure.code, code, "{}", case["name"]);
                assert_eq!(failure.operation_id.is_some(), code != "incompatible_protocol");
            }
        }
    }
    let reply = Frame::new(
        frame_type::DIRECT,
        PROTOCOL,
        SERVICE_DESTINATION,
        b"client",
        serde_json::to_vec(&fixture["optional_metadata_reply"]).unwrap(),
    )
    .unwrap();
    assert!(response(reply, fixture["operation_id"].as_str().unwrap()).unwrap().ok);
}

#[test]
fn required_features_are_support_preconditions_with_bounded_unique_names() {
    // Explicitly supported preconditions preserve the typed operation and unknown fields fail closed.
    let id = crate::identifier::encode(&[1; 16]);
    let operation = ServiceRequest::GetCredentialVault { operation_id: id.clone() };
    let frame = request_with_features(&operation, &["test.records.v1"]).unwrap();
    assert_eq!(admit_request(PROTOCOL, &frame.body, &["test.records.v1"]).unwrap(), operation);
    assert_eq!(admit_request(PROTOCOL, &frame.body, &[]).unwrap_err().code, "unsupported_feature");
    for features in [vec!["x"; 17], vec!["x", "x"], vec![""], vec!["future/feature"]] {
        assert!(request_with_features(&operation, &features).is_err());
    }
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../../tests/fixtures/ap-requests.json")).unwrap();
    let mut registration: ServiceRequest = serde_json::from_value(
        fixtures.into_iter().find(|request| request["op"] == "register_session").unwrap(),
    )
    .unwrap();
    if let ServiceRequest::RegisterSession {
        app,
        session_id,
        root_volume_id,
        shared_volume_id,
        ..
    } = &mut registration
    {
        use crate::package::AutomaticVolumeRole as Role;
        *root_volume_id = app.volume_id(Role::Root, None, None, None).unwrap().to_string();
        *shared_volume_id = Some(
            app.volume_id(
                Role::Session,
                Some(crate::identifier::decode(session_id).unwrap()),
                None,
                None,
            )
            .unwrap()
            .to_string(),
        );
    }
    let shared = request(&registration).unwrap();
    for incomplete in [&[][..], &["shared-session.v1"], &["session-participants-acl.v1"]] {
        assert_eq!(
            admit_request(PROTOCOL, &shared.body, incomplete).unwrap_err().code,
            "unsupported_feature"
        );
    }
    assert_eq!(admit_request(PROTOCOL, &shared.body, OPTIONAL_FEATURES).unwrap(), registration);
    if let ServiceRequest::RegisterSession { shared_volume_id, .. } = &mut registration {
        *shared_volume_id = None;
    }
    let ordinary = request(&registration).unwrap();
    assert_eq!(admit_request(PROTOCOL, &ordinary.body, &[]).unwrap(), registration);
    let oversized = vec![b' '; MAX_REQUEST_BYTES + 1];
    assert!(admit_request(PROTOCOL, &oversized, &[]).unwrap_err().operation_id.is_none());
    let mut fields = serde_json::json!({"op":"describe","operation_id":id});
    for index in 0..64 {
        fields[format!("field_{index}")] = false.into();
    }
    assert!(
        admit_request(PROTOCOL, &serde_json::to_vec(&fields).unwrap(), &[])
            .unwrap_err()
            .operation_id
            .is_none()
    );
    let duplicate = format!(
        "{{\"op\":\"describe\",\"operation_id\":\"{id}\",\"required_features\":[],\"required_features\":[]}}"
    );
    assert!(admit_request(PROTOCOL, duplicate.as_bytes(), &[]).unwrap_err().operation_id.is_none());
}

#[test]
fn discovery_is_bounded_extensible_and_independent_of_build_identity() {
    // A future optional name/metadata does not disturb baseline algorithm support.
    let baseline = ServiceDescription::baseline();
    baseline.validate().unwrap();
    assert!(serde_json::to_vec(&baseline).unwrap().len() < DESCRIPTION_LIMIT);
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../../tests/fixtures/ap-requests.json")).unwrap();
    assert_eq!(baseline.operations.len(), fixtures.len());
    for fixture in fixtures {
        assert!(baseline.operations.iter().any(|op| op == fixture["op"].as_str().unwrap()));
        admit_request(PROTOCOL, &serde_json::to_vec(&fixture).unwrap(), &[]).unwrap();
    }
    let mut additive = serde_json::to_value(&baseline).unwrap();
    additive["operations"].as_array_mut().unwrap().push("future_operation".into());
    additive["features"].as_array_mut().unwrap().push("test.records.v1".into());
    additive["future_metadata"] = serde_json::json!({"ignored":true});
    serde_json::from_value::<ServiceDescription>(additive).unwrap().validate().unwrap();
    let mut invalid = baseline.clone();
    invalid.operations.push("describe".into());
    assert!(invalid.validate().is_err());
    let mut invalid = baseline;
    invalid.algorithms.kvdb_pull = "kvdb.pull.v999".into();
    assert!(invalid.validate().is_err());
}

#[test]
fn future_failure_codes_refuse_without_becoming_authority() {
    // Unknown codes remain correlated failures with no denial or retry evidence.
    let id = crate::identifier::encode(&[2; 16]);
    let mut frame = reply(false, &id, Some("unsupported future behavior"));
    let mut value: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
    value["code"] = "future_failure".into();
    frame.body = serde_json::to_vec(&value).unwrap();
    let error = response(frame, &id).unwrap_err();
    let failure = error.downcast_ref::<ServiceFailure>().unwrap();
    assert_eq!(failure.code(), "future_failure");
    assert!(failure.denial().is_none());
    value["denial"] = serde_json::to_value(ServiceDenial::ParticipantRevoked {
        account_id: crate::store::Id::from_bytes([1; 16]),
        volume_id: crate::store::Id::from_bytes([2; 16]),
        session_id: None,
        build_id: None,
    })
    .unwrap();
    let frame = Frame::new(
        frame_type::DIRECT,
        PROTOCOL,
        SERVICE_DESTINATION,
        b"client",
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(
        response(frame, &id).unwrap_err().downcast_ref::<ServiceFailure>().is_none(),
        "an unknown failure code cannot deliver persistent negative authority"
    );
}

#[test]
fn unsupported_service_failures_require_correlation_and_preserve_the_code() {
    let id = crate::identifier::encode(&[2; 16]);
    for code in
        ["unsupported_operation", "unsupported_feature", "operation_failed", "invalid_request"]
    {
        let mut frame = reply(false, &id, Some("access denied"));
        let mut value: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        value["code"] = code.into();
        frame.body = serde_json::to_vec(&value).unwrap();
        let unrelated = response(frame.clone(), &crate::identifier::encode(&[3; 16])).unwrap_err();
        assert_eq!(unrelated.to_string(), "AP response operation identity differs");
        assert!(unrelated.downcast_ref::<ServiceFailure>().is_none());
        let error = response(frame, &id).unwrap_err();
        let failure = error.downcast_ref::<ServiceFailure>().unwrap();
        assert_eq!(failure.code(), code);
        assert!(failure.denial().is_none());
        assert_eq!(
            failure.to_string(),
            if matches!(code, "operation_failed" | "invalid_request") {
                "access denied"
            } else {
                INCOMPATIBLE_PROTOCOL
            }
        );
    }
}

#[test]
fn independent_public_peer_wire_matches_native_framing_and_discovery() {
    // This fixture was produced by the standalone peer without importing Playbit codecs.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/fixtures/ap-baseline.json")).unwrap();
    let bytes = |name: &str| {
        fixture["reference_wire"][name]
            .as_str()
            .unwrap()
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
    };
    let hello = Frame::decode(&bytes("hello_request")).unwrap();
    assert_eq!(hello.frame_type, frame_type::HELLO);
    assert_eq!(hello.body.len(), 72);
    assert_eq!(&hello.body[..16], &[7; 16]);
    let (auth, timing) =
        super::super::timing::auth::decode(Frame::decode(&bytes("hello_response")).unwrap())
            .unwrap();
    assert_eq!(auth.flags, 0);
    assert_eq!(auth.body, hello.body);
    assert!(timing.entries().is_empty());
    let response = response(
        Frame::decode(&bytes("describe_response")).unwrap(),
        fixture["operation_id"].as_str().unwrap(),
    )
    .unwrap();
    let description: ServiceDescription = serde_json::from_value(response.data.unwrap()).unwrap();
    description.validate().unwrap();
}

#[test]
fn optional_future_timing_never_changes_operation_success_or_typed_failure() {
    let id = crate::identifier::encode(&[0x12; 16]);
    for ok in [false, true] {
        let mut frame = reply(ok, &id, Some("access denied"));
        let mut body: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        body["timing"] = serde_json::json!([
            {"stage":"ap.future_storage","duration_ns":"18446744073709551615"},
            {"stage":"ap.operation","duration_ns":"7"}
        ]);
        frame.body = serde_json::to_vec(&body).unwrap();
        if ok {
            assert_eq!(response(frame, &id).unwrap().data.unwrap()["receipt"], 7);
        } else {
            let error = response(frame, &id).unwrap_err();
            let failure = error.downcast_ref::<ServiceFailure>().unwrap();
            assert_eq!(failure.code, "operation_failed");
            assert!(failure.denial().is_none());
        }
    }
}

#[test]
fn account_provider_push_requires_exact_bounded_endpoint_fields() {
    let id = crate::identifier::encode(&[1; 16]);
    for op in ["register_file_provider_account_push", "invalidate_file_provider_account_push"] {
        let valid = serde_json::json!({
            "op": op, "operation_id": id, "token": "01",
            "bundle": "playbit.engine", "sandbox": false,
        });
        admit_request(PROTOCOL, &serde_json::to_vec(&valid).unwrap(), &[]).unwrap();
        for token in [String::new(), "x0".into(), "0".into(), "00".repeat(129)] {
            let mut request = valid.clone();
            request["token"] = token.into();
            assert_eq!(
                admit_request(PROTOCOL, &serde_json::to_vec(&request).unwrap(), &[])
                    .unwrap_err()
                    .code,
                "invalid_request"
            );
        }
        let mut request = valid;
        request["bundle"] = "other.application".into();
        assert_eq!(
            admit_request(PROTOCOL, &serde_json::to_vec(&request).unwrap(), &[]).unwrap_err().code,
            "invalid_request"
        );
    }
}
