//! Service frame validation shared by native transports. Request schema checks
//! precede opening a stream; responses must identify the service and correlate
//! with the operation. This is protocol validation, not server authorization.
//! Callers own verified transport, per-operation frame limits and deadlines.
use super::{
    Frame, frame_type,
    operation::{SERVICE_DESTINATION, ServiceDenial, ServiceRequest, ServiceResponse},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// AP protocol version, independent of build identity or additive features.
/// Bulk data frames retain flags zero. Optional extensions do not change this.
pub const PROTOCOL: u8 = 1;
pub const INCOMPATIBLE_FLAGS: u8 = 4;
/// Exact optional AP support preconditions. Baseline algorithm names are separate.
pub const OPTIONAL_FEATURES: &[&str] = &[
    "bloblog-append-v1",
    SHARED_SESSION,
    SESSION_PARTICIPANTS_ACL,
    WORLD_REPLICA,
    VOLUME_INVITATIONS,
    super::channel::DISPLAY_NAMES_FEATURE,
];
pub const WORLD_REPLICA: &str = "world-replica.v1";
pub const VOLUME_INVITATIONS: &str = "volume-invitations.v1";
const SHARED_SESSION: &str = "shared-session.v1";
const SESSION_PARTICIPANTS_ACL: &str = "session-participants-acl.v1";
mod admission;
mod description;
pub use admission::{
    MAX_FEATURE_NAME_BYTES, MAX_REQUEST_BYTES, MAX_REQUIRED_FEATURES, RequestFailure,
    admit_request, validate_features,
};
pub use description::{DESCRIPTION_LIMIT, ServiceDescription};
pub const INCOMPATIBLE_PROTOCOL: &str = "Playbit and its sync service need compatible updates before they can connect. Your saved data has been kept.";

pub const SHARED_SERVICE_UPGRADE_REQUIRED: &str = "This session needs an updated sync service. Your local data has been kept. Try again after the service is updated.";

pub(crate) const SHARED_REGISTRATION_FEATURES: &[&str] =
    &[SHARED_SESSION, SESSION_PARTICIPANTS_ACL];

#[derive(Debug)]
pub struct IncompatibleProtocol;
impl std::fmt::Display for IncompatibleProtocol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(INCOMPATIBLE_PROTOCOL)
    }
}
impl std::error::Error for IncompatibleProtocol {}

/// Created only from a correlated service frame. Callers qualify the typed
/// evidence against their current request binding before persisting authority.
#[derive(Debug)]
pub struct ServiceFailure {
    message: String,
    denial: Option<ServiceDenial>,
    code: String,
}
impl ServiceFailure {
    pub fn code(&self) -> &str {
        &self.code
    }
    pub fn denial(&self) -> Option<&ServiceDenial> {
        self.denial.as_ref()
    }
    #[cfg(feature = "connected-engine")]
    pub(crate) fn is_capacity_exhausted(&self) -> bool {
        self.code == "operation_failed"
            && self.denial.is_none()
            && self.message == "AP request capacity exhausted"
    }
    // Released credentialed APs report this atomic KV rejection under operation_failed.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn is_kv_append_conflict(&self) -> bool {
        self.code == "operation_failed"
            && self.denial.is_none()
            && self.message
                == "unauthorized replica entry: access denied: append cannot replace or delete"
    }
    #[cfg(any(feature = "connected-engine", test))]
    pub(crate) fn is_missing_build_identity(&self) -> bool {
        self.code == "operation_failed"
            && self.denial.is_none()
            && self.message == "missing authenticated build identity"
    }
}
impl std::fmt::Display for ServiceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.code.as_str() {
            "unsupported_operation" | "unsupported_feature" => INCOMPATIBLE_PROTOCOL,
            _ => &self.message,
        })
    }
}
impl std::error::Error for ServiceFailure {}

pub(super) fn required_features(request: &ServiceRequest) -> &'static [&'static str] {
    match request {
        ServiceRequest::RegisterVolumeInvitationKey { .. }
        | ServiceRequest::GetVolumeInvitationKey { .. }
        | ServiceRequest::PutVolumeInvitationPolicy { .. }
        | ServiceRequest::PutVolumeInvitation { .. }
        | ServiceRequest::ListVolumeInvitationRecipients { .. }
        | ServiceRequest::WatchVolumeInvitations { .. }
        | ServiceRequest::VolumePeerListen { .. }
        | ServiceRequest::VolumePeerSubscribe { .. }
        | ServiceRequest::VolumePeerRequest { .. } => &[VOLUME_INVITATIONS],
        ServiceRequest::WorldReplica { .. } => &[WORLD_REPLICA],
        ServiceRequest::PublishBloblog { .. } | ServiceRequest::GetBloblog { .. } => {
            &["bloblog-append-v1"]
        }
        ServiceRequest::RegisterSession { shared_volume_id: Some(_), .. } => {
            SHARED_REGISTRATION_FEATURES
        }
        _ => &[],
    }
}

pub fn request(request: &ServiceRequest) -> Result<Frame> {
    request_with_features(request, required_features(request))
}

pub fn request_with_features(
    request: &ServiceRequest,
    required_features: &[&str],
) -> Result<Frame> {
    request.validate()?;
    validate_features(required_features)?;
    let mut body = serde_json::to_vec(request)?;
    if !required_features.is_empty() {
        assert_eq!(body.pop(), Some(b'}'), "requests encode as JSON objects");
        body.extend_from_slice(b",\"required_features\":");
        serde_json::to_writer(&mut body, required_features)?;
        body.push(b'}');
    }
    if body.len() > MAX_REQUEST_BYTES {
        return Err("AP request exceeds limit".into());
    }
    Ok(Frame::new(frame_type::DIRECT, PROTOCOL, [], SERVICE_DESTINATION, body)?)
}

pub fn response(frame: Frame, operation_id: &str) -> Result<ServiceResponse> {
    if frame.frame_type == frame_type::ERROR
        && frame.flags == INCOMPATIBLE_FLAGS
        && frame.source == SERVICE_DESTINATION
    {
        return Err(IncompatibleProtocol.into());
    }
    if frame.frame_type != frame_type::DIRECT || frame.source != SERVICE_DESTINATION {
        return Err("invalid AP service response".into());
    }
    if frame.flags != PROTOCOL {
        return Err(IncompatibleProtocol.into());
    }
    validated_response(serde_json::from_slice(&frame.body)?, operation_id)
}

pub(crate) fn validated_response(
    response: ServiceResponse,
    operation_id: &str,
) -> Result<ServiceResponse> {
    if response.operation_id != operation_id {
        return Err("AP response operation identity differs".into());
    }
    if let Some(denial) = &response.denial {
        denial.validate()?;
        if response.code.as_deref() != Some("operation_failed") {
            return Err("AP denial requires the operation_failed code".into());
        }
        if response.ok {
            return Err("successful AP response contains denial evidence".into());
        }
    }
    let _operation = crate::trace::Span::new("ap.operation.reply");
    for timing in response.timing.entries() {
        crate::trace::mark_value(timing.stage.name(), timing.duration_ns);
    }
    if !response.ok {
        let code = response.code.ok_or("AP failure lacks a stable code")?;
        if !admission::valid_name(&code, false) {
            return Err("invalid AP failure code".into());
        }
        return Err(ServiceFailure {
            code,
            message: response.error.unwrap_or_else(|| "service operation failed".into()),
            denial: response.denial,
        }
        .into());
    }
    Ok(response)
}

pub fn data(frame: crate::network::ap::Frame) -> Result<Vec<u8>> {
    if frame.frame_type != frame_type::DIRECT
        || frame.source != SERVICE_DESTINATION
        || ![0, PROTOCOL].contains(&frame.flags)
    {
        return Err("invalid AP service data frame".into());
    }
    Ok(frame.body)
}

#[cfg(test)]
#[path = "service/tests.rs"]
mod tests;
