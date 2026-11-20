//! Shared identity/protocol failures; transport-specific variants compile only
//! with their actual native transport. No network dependency for identity errors.
use super::{Error as ProtocolError, registration};
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Certificate(#[from] rustls::Error),
    #[error(transparent)]
    CertificateRequest(#[from] rcgen::Error),
    #[cfg(feature = "network")]
    #[error(transparent)]
    Connect(#[from] quinn::ConnectError),
    #[cfg(feature = "network")]
    #[error(transparent)]
    Connection(#[from] quinn::ConnectionError),
    #[cfg(feature = "network")]
    #[error(transparent)]
    Write(#[from] quinn::WriteError),
    #[cfg(feature = "network")]
    #[error(transparent)]
    ClosedStream(#[from] quinn::ClosedStream),
    #[cfg(feature = "network")]
    #[error(transparent)]
    Read(#[from] quinn::ReadExactError),
    #[error("AP TLS configuration failed: {0}")]
    TlsConfiguration(String),
    #[error("AP connection timed out")]
    ConnectTimeout,
    #[error("AP address lookup failed: {0}")]
    AddressLookup(#[source] io::Error),
    #[error("opening the AP authentication stream timed out")]
    OpenStreamTimeout,
    #[error("the AP did not answer authentication in time")]
    AuthenticationTimeout,
    #[error("the AP did not answer the request in time")]
    ResponseTimeout,
    #[error("AP idle timeout is outside QUIC's supported range")]
    InvalidIdleTimeout,
    #[error("AP client identity is missing {0}")]
    MissingIdentityField(&'static str),
    #[error("AP requires a newly issued authenticated instance identity")]
    AuthenticationRequired,
    #[error("AP accepted a saved token without a saved instance ID")]
    MissingSavedInstanceId,
    #[error("AP returned frame type {0} during authentication")]
    UnexpectedFrame(u8),
    #[error("AP returned unknown HELLO status {0}")]
    UnexpectedHelloStatus(u8),
    #[error("AP returned unknown REGISTER status {0}")]
    UnexpectedRegisterStatus(u8),
    #[error("AP registration requires a challenge from the user")]
    ChallengeRequired,
    #[error("That code is incorrect or expired. Check your email and try again.")]
    InvalidChallenge,
    #[error(
        "Email delivery hasn’t been confirmed. Check your inbox or try sending again in a minute."
    )]
    DeliveryUnconfirmed,
    #[error("AP account is disabled")]
    AccountDisabled,
    #[error("AP registration field is too long")]
    RegistrationFieldTooLong,
    #[error("AP returned an invalid user agreement response")]
    InvalidAgreementResponse,
    #[error("AP returned an invalid registration response")]
    InvalidRegistrationResponse,
    #[error("AP authentication token has an invalid format")]
    InvalidAuthenticationToken,
    #[error("AP preissued identity keypair is invalid")]
    InvalidPreissuedIdentity,
    #[error("AP identity must be an owner-only regular file")]
    InsecureIdentityFile,
    #[error("AP root certificate file contained no certificates")]
    NoCertificates,
}
impl From<registration::Error> for ClientError {
    fn from(error: registration::Error) -> Self {
        match error {
            registration::Error::InvalidCode => Self::InvalidChallenge,
            registration::Error::FieldTooLong => Self::RegistrationFieldTooLong,
            registration::Error::UnexpectedFrame(kind) => Self::UnexpectedFrame(kind),
            registration::Error::InvalidResponse => Self::InvalidRegistrationResponse,
            registration::Error::Protocol(error) => Self::Protocol(error),
        }
    }
}
