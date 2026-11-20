//! Request email-code delivery, then release transport while the user responds.
//! A confirmed delivery is not authentication. No identity mutation or implicit
//! development-code selection occurs; the host owns user interaction and storage.
use super::{
    Connection, Error,
    exchange::{BODY_MAX, Exchange, Reply, Result},
};
use crate::network::ap::{self, ClientIdentity, Frame, RegistrationChallenge, authentication};
use std::io::{Read, Write};

pub struct RegistrationRequest<S: Read + Write> {
    exchange: Option<Exchange<S>>,
}
impl<S: Read + Write> RegistrationRequest<S> {
    pub fn new(connection: Connection<S>, identity: &ClientIdentity) -> Result<Self> {
        authentication::validate_registration_identity(identity)?;
        if !identity.has_private_key() {
            return Err(ap::ClientError::MissingIdentityField("private key").into());
        }
        Ok(Self { exchange: Some(Exchange::new(connection, request(identity, "", &[])?)?) })
    }
    pub fn transport_mut(&mut self) -> Result<&mut S> {
        Ok(self.exchange.as_mut().ok_or(Error::Closed)?.connection.transport_mut()?)
    }
    pub fn continue_now(&self) -> bool {
        self.exchange.as_ref().is_some_and(|e| e.continue_now)
    }
    pub fn wants_write(&self) -> bool {
        self.exchange.as_ref().is_some_and(|e| e.connection.pending)
    }
    /// One bounded turn. Successful delivery or any failure drops this connection.
    /// Do not wait for FIN: AP keeps the stream open for later code confirmation.
    pub fn advance(&mut self) -> Result<Option<RegistrationChallenge>> {
        let mut exchange = self.exchange.take().ok_or(Error::Closed)?;
        match exchange.advance()? {
            Some(Reply::Frame(reply)) => Ok(Some(RegistrationChallenge {
                automatic_challenge: authentication::registration_challenge(&reply)?,
            })),
            Some(Reply::End) => Err(ap::Error::UnexpectedEnd.into()),
            None => {
                self.exchange = Some(exchange);
                Ok(None)
            }
        }
    }
}
pub(super) fn admit(identity: &ClientIdentity, code: &str, csr: usize) -> Result<()> {
    ap::registration::request_length(
        identity.email.len(),
        identity.instance_name.len(),
        code.len(),
        csr,
        BODY_MAX,
    )
    .map_err(ap::ClientError::from)?;
    Ok(())
}
pub(super) fn request(identity: &ClientIdentity, code: &str, csr: &[u8]) -> Result<Frame> {
    admit(identity, code, csr.len())?;
    Ok(ap::registration::request(&identity.email, &identity.instance_name, code, csr)
        .map_err(ap::ClientError::from)?)
}
