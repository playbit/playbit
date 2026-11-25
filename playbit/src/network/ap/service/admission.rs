//! Admit the bounded support envelope before decoding operation semantics.
//! Raw values borrow the input; unsupported operations never allocate a Value tree.
use super::PROTOCOL;
use crate::network::ap::operation::ServiceRequest;
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::value::RawValue;

pub const MAX_REQUEST_BYTES: usize = 384 * 1024;
pub const MAX_REQUIRED_FEATURES: usize = 16;
pub const MAX_FEATURE_NAME_BYTES: usize = 64;
const MAX_REQUEST_FIELDS: usize = 64;
pub const OPERATION_NAMES: &[&str] = &[
    "describe",
    "issue_publisher_certificate",
    "get_publisher_key_status",
    "revoke_publisher_key",
    "register_file_provider_account_push",
    "invalidate_file_provider_account_push",
    "resolve_volume_grantee",
    "register_volume_invitation_key",
    "get_volume_invitation_key",
    "put_volume_invitation_policy",
    "put_volume_invitation",
    "list_volume_invitation_recipients",
    "watch_volume_invitations",
    "volume_peer_listen",
    "volume_peer_subscribe",
    "volume_peer_request",
    "register_volume",
    "register_session",
    "upload_build",
    "provision_replica",
    "open_session",
    "watch_invitation",
    "get_app",
    "get_app_display",
    "list_apps",
    "list_app_builds",
    "set_app_enabled",
    "delete_app",
    "publish_app",
    "resolve_session",
    "read_invitation",
    "advance_session",
    "get_credential_vault",
    "put_credential_vault",
    "sync_account",
    "subscribe_account",
    "publish_account",
    "put_account_display_name",
    "sync_database",
    "world_replica",
    "subscribe_replica",
    "publish_replica",
    "publish_bloblog",
    "get_bloblog",
    "peer_listen",
    "peer_request",
    "peer_reply",
    "upload_blob",
    "download_blob",
    "volume_usage",
    "get_blob",
    "get_build_identity",
];

#[derive(Debug)]
pub struct RequestFailure {
    pub operation_id: Option<String>,
    pub code: &'static str,
    message: &'static str,
}
impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for RequestFailure {}
impl RequestFailure {
    pub fn session_selector_conflict(operation: &str) -> Self {
        Self::correlated(
            operation,
            "session_selector_conflict",
            "session changed; resolve before updating again",
        )
    }
    fn envelope() -> Self {
        Self { operation_id: None, code: "invalid_request", message: "invalid AP request envelope" }
    }
    fn correlated(id: &str, code: &'static str, message: &'static str) -> Self {
        Self { operation_id: Some(id.into()), code, message }
    }
}

pub fn valid_name(name: &str, feature: bool) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FEATURE_NAME_BYTES
        && name.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'_'
                || feature && matches!(b, b'.' | b'-')
        })
}
pub fn validate_features(features: &[&str]) -> Result<(), &'static str> {
    if features.len() > MAX_REQUIRED_FEATURES {
        return Err("too many required AP features");
    }
    for (index, name) in features.iter().enumerate() {
        if !valid_name(name, true) || features[..index].contains(name) {
            return Err("invalid or duplicate required AP feature");
        }
    }
    Ok(())
}

struct Envelope<'a>(Vec<(String, &'a RawValue)>);
impl<'de> Deserialize<'de> for Envelope<'de> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Envelope<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a bounded AP request object with unique fields")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields: Vec<(String, &'de RawValue)> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.len() == MAX_REQUEST_FIELDS
                        || key.len() > MAX_FEATURE_NAME_BYTES
                        || fields.iter().any(|(existing, _)| existing == &key)
                    {
                        return Err(de::Error::custom("duplicate or excessive AP request fields"));
                    }
                    fields.push((key, map.next_value()?));
                }
                Ok(Envelope(fields))
            }
        }
        deserializer.deserialize_map(Fields)
    }
}

struct Name(String);
impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoundedName;
        impl<'de> Visitor<'de> for BoundedName {
            type Value = Name;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an AP feature name of at most 64 bytes")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Name, E> {
                if !valid_name(value, true) {
                    return Err(E::custom("invalid AP feature name"));
                }
                Ok(Name(value.into()))
            }
        }
        deserializer.deserialize_str(BoundedName)
    }
}

fn envelope_string(raw: Option<&str>, maximum: usize) -> Option<String> {
    let raw = raw?;
    // Every ASCII byte can occupy at most six bytes as a JSON Unicode escape.
    if raw.len() > maximum * 6 + 2 {
        return None;
    }
    serde_json::from_str(raw).ok()
}

struct Features(Vec<String>);
impl<'de> Deserialize<'de> for Features {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Names;
        impl<'de> Visitor<'de> for Names {
            type Value = Features;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("at most 16 required AP features")
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut sequence: S,
            ) -> Result<Self::Value, S::Error> {
                let mut names = Vec::new();
                while let Some(Name(name)) = sequence.next_element::<Name>()? {
                    if names.len() == MAX_REQUIRED_FEATURES || !valid_name(&name, true) {
                        return Err(de::Error::custom("invalid required AP features"));
                    }
                    names.push(name);
                }
                Ok(Features(names))
            }
        }
        deserializer.deserialize_seq(Names)
    }
}

// fuzz: ap_codec
pub fn admit_request(
    major: u8,
    bytes: &[u8],
    supported: &[&str],
) -> Result<ServiceRequest, RequestFailure> {
    if major != PROTOCOL {
        return Err(RequestFailure {
            operation_id: None,
            code: "incompatible_protocol",
            message: super::INCOMPATIBLE_PROTOCOL,
        });
    }
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(RequestFailure::envelope());
    }
    let Envelope(fields) = serde_json::from_slice(bytes).map_err(|_| RequestFailure::envelope())?;
    let field = |name| fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.get());
    let id = envelope_string(field("operation_id"), 22)
        .filter(|id| crate::identifier::valid(id, 16))
        .ok_or_else(RequestFailure::envelope)?;
    let op = envelope_string(field("op"), MAX_FEATURE_NAME_BYTES)
        .filter(|name| valid_name(name, false))
        .ok_or_else(RequestFailure::envelope)?;
    let invalid =
        || RequestFailure::correlated(&id, "invalid_request", "invalid AP operation request");
    if !OPERATION_NAMES.contains(&op.as_str()) {
        return Err(RequestFailure::correlated(
            &id,
            "unsupported_operation",
            "unsupported AP operation",
        ));
    }
    let Features(features) = match field("required_features") {
        Some(value) => serde_json::from_str(value).map_err(|_| invalid())?,
        None => Features(Vec::new()),
    };
    let selected: Vec<&str> = features.iter().map(String::as_str).collect();
    validate_features(&selected).map_err(|_| invalid())?;
    if features.iter().any(|feature| !supported.contains(&feature.as_str())) {
        return Err(RequestFailure::correlated(
            &id,
            "unsupported_feature",
            "unsupported required AP feature",
        ));
    }
    decode_payload(&fields, bytes.len(), &id)
}

fn decode_payload(
    fields: &[(String, &RawValue)],
    capacity: usize,
    id: &str,
) -> Result<ServiceRequest, RequestFailure> {
    let invalid =
        || RequestFailure::correlated(id, "invalid_request", "invalid AP operation request");
    let mut payload = Vec::with_capacity(capacity);
    payload.push(b'{');
    for (key, value) in fields {
        if key == "required_features" {
            continue;
        }
        if payload.len() > 1 {
            payload.push(b',');
        }
        serde_json::to_writer(&mut payload, key).map_err(|_| invalid())?;
        payload.push(b':');
        payload.extend_from_slice(value.get().as_bytes());
    }
    payload.push(b'}');
    let request: ServiceRequest = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    request.validate().map_err(|_| invalid())?;
    Ok(request)
}
