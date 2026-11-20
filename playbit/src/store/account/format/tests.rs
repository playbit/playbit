#![allow(
    clippy::unwrap_used,
    reason = "Tests assert independent account vectors and refusal boundaries."
)]
use super::*;
use crate::store::{Id, PersonalVolumeSelection};

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../../tests/fixtures/account-plaintext-v1.json"))
        .unwrap()
}
fn bytes(name: &str) -> Vec<u8> {
    let fixture = fixture();
    fixture[name]
        .as_str()
        .unwrap()
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn extension(bytes: &mut Vec<u8>, tag: u16, value: &[u8]) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
    bytes.extend_from_slice(value);
}
#[test]
fn literal_account_vectors_match_native_and_browser_without_regeneration() {
    let fixture = fixture();
    let value = Record::PersonalVolumes {
        session: fixture["session"].as_str().unwrap().into(),
        grants: vec![PersonalVolumeSelection {
            volume: Id::from_bytes([3; 16]),
            write: false,
            incarnation: Id::from_bytes([4; 16]),
        }],
    };
    assert_eq!(value.encode().unwrap(), bytes("personal_hex"));
    assert!(
        matches!(Record::decode(&bytes("personal_hex")).unwrap(), Record::PersonalVolumes { grants, .. } if grants.len() == 1)
    );
    assert_eq!(
        Record::DisplayName { name: "Notes".into() }.encode().unwrap(),
        bytes("display_hex")
    );
    assert!(Frame::decode(&bytes("optional_hex")).unwrap().admit().unwrap().is_none());
}
#[test]
fn optional_known_payload_and_header_fields_survive_older_edits() {
    let mut frame = Frame::parse(&bytes("personal_hex")).unwrap();
    extension(&mut frame.payload.0, 77, &[255, 1, 2]);
    let payload_tail = frame.payload.0[frame.payload.0.len() - 9..].to_vec();
    let mut original = record::encode(&frame).unwrap();
    extension(&mut original, 99, &[255, 4]);
    let Record::PersonalVolumes { session, .. } = Record::decode(&original).unwrap() else {
        panic!()
    };
    let next = Record::PersonalVolumes { session, grants: vec![] };
    let rewritten = next.rewrite(&original).unwrap();
    assert_eq!(&rewritten[rewritten.len() - 8..], &original[original.len() - 8..]);
    let retained = Frame::parse(&rewritten).unwrap();
    assert!(retained.payload.0.ends_with(&payload_tail));
    let value = Record::decode(&original).unwrap();
    assert_eq!(value.rewrite(&original).unwrap(), original);
    let nested = bytes("personal_nested_optional_hex");
    let value = Record::decode(&nested).unwrap();
    assert_eq!(value.rewrite(&nested).unwrap(), nested);
    assert!(
        next.rewrite(&nested).is_err(),
        "changing a sequence cannot discard nested opaque data"
    );
}
#[test]
fn unknown_authority_and_requirements_do_not_acquire_a_projection() {
    let mut frame = Frame::parse(&bytes("optional_hex")).unwrap();
    frame.authority = true;
    assert!(Frame::decode(&record::encode(&frame).unwrap()).unwrap().unsupported);
    frame.authority = false;
    frame.required_write.push("account.consent.v2".into());
    assert!(Frame::decode(&record::encode(&frame).unwrap()).unwrap().unsupported);
    let mut known = Frame::parse(&bytes("personal_hex")).unwrap();
    known.authority = false;
    assert!(Frame::decode(&record::encode(&known).unwrap()).is_err());
    frame.required_write.clear();
    frame.logical_key = "settings/display-name".into();
    assert!(Frame::decode(&record::encode(&frame).unwrap()).is_err());
    let mut known = bytes("personal_hex");
    extension(&mut known, 99 | record::READ_REQUIRED, &[255]);
    assert!(Frame::decode(&known).unwrap().unsupported);
}
#[test]
fn malformed_identity_feature_lists_and_truncations_reject() {
    let original = bytes("personal_hex");
    for length in 0..original.len() {
        assert!(Frame::decode(&original[..length]).is_err());
    }
    let mut frame = Frame::parse(&original).unwrap();
    for names in [
        vec!["x".into(), "x".into()],
        vec!["bad name".into()],
        vec!["x".repeat(65)],
        (0..17).map(|n| format!("feature.{n:02}")).collect(),
    ] {
        frame.required_read = names;
        assert!(Frame::decode(&record::encode(&frame).unwrap()).is_err());
    }
    let mut duplicate = original.clone();
    extension(&mut duplicate, 6, &[6, 0]);
    assert!(Frame::decode(&duplicate).is_err());
    let mut oversized = original;
    extension(&mut oversized, 99, &vec![255; 4091]);
    assert!(Frame::decode(&oversized).is_err());
}

#[test]
fn shared_capability_requires_authenticated_account_features_and_preserves_legacy_bytes() {
    use crate::{
        package::AutomaticVolumeRole,
        store::{BuildSelector, Invitation, Session, SharedVolume, UpdateAuthority, Volume},
    };
    let app = crate::package::AppIdentity { publisher: Id([1; 16]), app_id: "test".into() };
    let id = crate::identifier::encode(&[2; 32]);
    let root = app.volume_id(AutomaticVolumeRole::Root, None, None, None).unwrap();
    let shared = app.volume_id(AutomaticVolumeRole::Session, Some([2; 32]), None, None).unwrap();
    let build = crate::identifier::encode(&[3; 32]);
    let credentials =
        super::super::record::Credentials { database_key: [4; 32], blob_key: [5; 32] };
    let mut invitation = Invitation {
        seed: [6; 16],
        session: Session {
            app: app.clone(),
            id,
            selector: BuildSelector::App { app },
            resolved_build_id: build,
            root_volume: root,
            shared_volume: None,
            creator: Id([1; 16]),
            update_authority: UpdateAuthority::App,
        },
        root: Some(Volume {
            id: root,
            name: "World".into(),
            owner: Id([1; 16]),
            share_key_with_service: true,
            personal: false,
        }),
        root_credentials: Some(credentials.clone()),
        shared: None,
    };
    let legacy = Record::Invitation { invitation: invitation.clone(), app_name: None };
    let legacy_bytes = legacy.encode().unwrap();
    assert_eq!(Record::decode(&legacy_bytes).unwrap().encode().unwrap(), legacy_bytes);
    let frame = Frame::parse(&legacy_bytes).unwrap();
    assert!(frame.required_read.is_empty());
    assert!(frame.required_write.is_empty());
    // The old Session/Invitation tag sets remain sufficient for legacy data.
    let legacy_invitation = record::encode(&invitation).unwrap();
    assert!(View::record(&legacy_invitation).unwrap().fields(Mode::Write, &[1, 2, 3, 4]).is_ok());

    invitation.root = None;
    invitation.root_credentials = None;
    invitation.session.shared_volume = Some(shared);
    invitation.shared = Some(SharedVolume {
        volume: Volume {
            id: shared,
            name: "Shared".into(),
            owner: Id([1; 16]),
            share_key_with_service: true,
            personal: false,
        },
        credentials,
    });
    let value = Record::Invitation { invitation, app_name: None };
    let bytes = value.encode().unwrap();
    let mut frame = Frame::parse(&bytes).unwrap();
    assert_eq!(frame.required_read, [SHARED_SESSION]);
    assert_eq!(frame.required_write, [SHARED_SESSION]);
    assert!(
        matches!(Record::decode(&bytes).unwrap(), Record::Invitation { invitation, .. } if invitation.session.shared_volume == Some(shared))
    );
    frame.required_read.clear();
    assert!(Record::decode(&record::encode(&frame).unwrap()).is_err());
    frame.required_read.push(SHARED_SESSION.into());
    frame.required_write.clear();
    assert!(Record::decode(&record::encode(&frame).unwrap()).is_err());
}
