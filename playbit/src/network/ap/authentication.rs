//! Shared authentication reply decisions, independent of socket/executor choice.
//! Callers supply frames from their verified AP connection and own deadlines and
//! protected persistence. Rejected replies never partially replace credentials.
use super::{
    ClientError, ClientIdentity, Frame, frame_type, hello, registration, registration_status,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hello {
    Authenticated,
    RegistrationRequired,
}

pub fn hello_request(identity: &ClientIdentity) -> Result<Frame, ClientError> {
    if identity.auth_token.is_empty() {
        return Err(ClientError::AuthenticationRequired);
    }
    Ok(Frame::new(frame_type::HELLO, 0, [], [], identity.auth_token.clone())?)
}

/// Registration fallback is an explicit host policy, never inferred from a
/// transport failure. Only the three declared credential-rejection statuses
/// permit clearing the saved token/instance and asking the user to enroll.
pub fn accept_hello(
    identity: &mut ClientIdentity,
    response: Frame,
    allow_registration: bool,
) -> Result<Hello, ClientError> {
    let decision = validate_hello(identity, &response, allow_registration)?;
    match decision {
        Hello::Authenticated => {
            if !response.body.is_empty() {
                identity.auth_token = response.body;
            }
        }
        Hello::RegistrationRequired => {
            identity.auth_token.clear();
            identity.instance_id.clear();
        }
    }
    Ok(decision)
}
/// Inspect a complete reply without adopting it. Stream owners can reject an
/// unauthenticated reply immediately while waiting for FIN before success.
pub(crate) fn validate_hello(
    identity: &ClientIdentity,
    response: &Frame,
    allow_registration: bool,
) -> Result<Hello, ClientError> {
    if response.frame_type != frame_type::HELLO {
        return Err(ClientError::UnexpectedFrame(response.frame_type));
    }
    if !response.source.is_empty() || !response.destination.is_empty() {
        return Err(ClientError::Protocol(super::Error::InvalidHeader));
    }
    hello_decision(response.flags, !identity.instance_id.is_empty(), allow_registration)
}
fn hello_decision(
    status: u8,
    saved_instance: bool,
    allow_registration: bool,
) -> Result<Hello, ClientError> {
    match status {
        hello::VALID if saved_instance => Ok(Hello::Authenticated),
        hello::VALID => Err(ClientError::MissingSavedInstanceId),
        hello::MISSING | hello::INVALID | hello::REVOKED if allow_registration => {
            Ok(Hello::RegistrationRequired)
        }
        hello::MISSING | hello::INVALID | hello::REVOKED => {
            Err(ClientError::AuthenticationRequired)
        }
        status => Err(ClientError::UnexpectedHelloStatus(status)),
    }
}

/// Production delivery confirmation contains no challenge; isolated development
/// services may supply one. This does not adopt credentials or authenticate.
pub fn registration_challenge(frame: &Frame) -> Result<Option<String>, ClientError> {
    let first = registration::Response::parse(frame)?;
    match first.status {
        registration_status::IN_PROGRESS => {}
        registration_status::ACCOUNT_DISABLED => return Err(ClientError::AccountDisabled),
        registration_status::DELIVERY_UNCONFIRMED => return Err(ClientError::DeliveryUnconfirmed),
        status => return Err(ClientError::UnexpectedRegisterStatus(status)),
    }
    if first.instance_id.is_empty() {
        return Ok(None);
    }
    String::from_utf8(first.instance_id.to_vec())
        .map(Some)
        .map_err(|_| ClientError::InvalidRegistrationResponse)
}

pub fn accept_registration(
    identity: &mut ClientIdentity,
    frame: &Frame,
) -> Result<(), ClientError> {
    let response = validate_registration(frame)?;
    identity.auth_token = response.auth_token.to_vec();
    identity.instance_id = response.instance_id.to_vec();
    identity.ca_certificate = response.ca_certificate.to_vec();
    identity.instance_certificate = response.instance_certificate.to_vec();
    Ok(())
}

pub(crate) fn validate_registration(
    frame: &Frame,
) -> Result<registration::Response<'_>, ClientError> {
    let response = registration::Response::parse(frame)?;
    match response.status {
        registration_status::SUCCESS => {}
        registration_status::INVALID_CHALLENGE => return Err(ClientError::InvalidChallenge),
        registration_status::ACCOUNT_DISABLED => return Err(ClientError::AccountDisabled),
        status => return Err(ClientError::UnexpectedRegisterStatus(status)),
    }
    if response.auth_token.is_empty() || response.instance_id.is_empty() {
        return Err(ClientError::InvalidRegistrationResponse);
    }
    Ok(response)
}

#[cfg(any(feature = "network", feature = "ap-websocket"))]
pub(crate) fn validate_registration_identity(identity: &ClientIdentity) -> Result<(), ClientError> {
    if identity.email.is_empty() {
        return Err(ClientError::MissingIdentityField("email"));
    }
    if identity.instance_name.is_empty() {
        return Err(ClientError::MissingIdentityField("instance name"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationChallenge {
    /// Development AP servers may return a code directly; production delivers
    /// it out of band and leaves this empty.
    pub automatic_challenge: Option<String>,
}

/// Testing enrollment remains explicitly selected and uses its existing stricter
/// issued-field shape checks; it is not a fallback from ordinary authentication.
pub fn accept_testing_hello(
    identity: &mut ClientIdentity,
    mut frame: Frame,
) -> Result<(), ClientError> {
    if frame.frame_type != frame_type::HELLO || frame.flags != hello::VALID {
        return Err(ClientError::AuthenticationRequired);
    }
    frame.frame_type = frame_type::REGISTER;
    frame.flags = registration_status::SUCCESS;
    let response = registration::Response::parse(&frame)?;
    if response.auth_token.len() != 72
        || !std::str::from_utf8(response.instance_id)
            .is_ok_and(|id| crate::identifier::valid(id, 16))
        || response.ca_certificate.is_empty()
        || response.instance_certificate.is_empty()
    {
        return Err(ClientError::InvalidRegistrationResponse);
    }
    identity.auth_token = response.auth_token.to_vec();
    identity.instance_id = response.instance_id.to_vec();
    identity.ca_certificate = response.ca_certificate.to_vec();
    identity.instance_certificate = response.instance_certificate.to_vec();
    Ok(())
}

#[cfg(test)]
#[path = "authentication/tests.rs"]
mod tests;
