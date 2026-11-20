//! Saved-token authentication or explicit code confirmation on a fresh connection.
//! The exchange exclusively borrows identity; complete reply and remote FIN must
//! precede adoption. Errors/drop close the connection. Hosts own TLS/deadlines.
use super::{
    Connection, Error,
    exchange::{BODY_MAX, Exchange, Reply, Result},
};
use crate::network::ap::{self, ClientError, ClientIdentity, Frame, authentication};
use std::io::{Read, Write};
enum Method {
    SavedToken,
    Registration,
}
pub struct Authentication<'a, S: Read + Write> {
    exchange: Option<Exchange<S>>,
    identity: &'a mut ClientIdentity,
    method: Method,
    reply: Option<Frame>,
}
impl<'a, S: Read + Write> Authentication<'a, S> {
    /// Saved-identity authentication never falls back to enrollment. Request and
    /// reply bodies fit 4 KiB; size admission precedes cloning or wire allocation.
    pub fn new(connection: Connection<S>, identity: &'a mut ClientIdentity) -> Result<Self> {
        if identity.auth_token.len() > BODY_MAX {
            return Err(ap::Error::FrameTooLarge.into());
        }
        if identity.instance_id.is_empty() {
            return Err(ClientError::MissingSavedInstanceId.into());
        }
        let request = authentication::hello_request(identity)?;
        Ok(Self {
            exchange: Some(Exchange::new(connection, request)?),
            identity,
            method: Method::SavedToken,
            reply: None,
        })
    }
    /// Confirm an out-of-band code with the shared signed CSR and registration
    /// policy. The host must prepare and protect the identity key before calling;
    /// retain secure entropy through crypto and persist adopted credentials later.
    pub fn register(
        connection: Connection<S>,
        identity: &'a mut ClientIdentity,
        code: &str,
    ) -> Result<Self> {
        authentication::validate_registration_identity(identity)?;
        if !identity.has_private_key() {
            return Err(ClientError::MissingIdentityField("private key").into());
        }
        if code.is_empty() {
            return Err(ClientError::MissingIdentityField("registration challenge").into());
        }
        super::registration::admit(identity, code, 0)?;
        let csr = identity.certificate_request()?;
        let request = super::registration::request(identity, code, csr.der())?;
        Ok(Self {
            exchange: Some(Exchange::new(connection, request)?),
            identity,
            method: Method::Registration,
            reply: None,
        })
    }
    pub fn transport_mut(&mut self) -> Result<&mut S> {
        Ok(self.exchange.as_mut().ok_or(Error::Closed)?.connection.transport_mut()?)
    }
    #[cfg(test)]
    pub(super) fn token(&self) -> &[u8] {
        &self.identity.auth_token
    }
    pub fn continue_now(&self) -> bool {
        self.exchange.as_ref().is_some_and(|e| e.continue_now)
    }
    /// Combine with TLS write readiness and refresh it after every advance.
    pub fn wants_write(&self) -> bool {
        self.exchange.as_ref().is_some_and(|e| e.connection.pending)
    }
    /// One bounded transport turn, at most one write and one stream read. Drive
    /// only on readiness or continue_now. Errors and completion are terminal;
    /// success transfers the same still-open connection back to its host.
    pub fn advance(&mut self) -> Result<Option<Connection<S>>> {
        let mut exchange = self.exchange.take().ok_or(Error::Closed)?;
        match exchange.advance()? {
            Some(Reply::Frame(reply)) => {
                match self.method {
                    Method::SavedToken => {
                        authentication::validate_hello(self.identity, &reply, false)?;
                    }
                    Method::Registration => {
                        authentication::validate_registration(&reply)?;
                    }
                }
                // Rejection leaves AP waiting for registration. Never close our
                // sending side before inspecting the answer or wait for its FIN.
                exchange.finish()?;
                self.reply = Some(reply);
            }
            Some(Reply::End) => {
                let reply = self.reply.take().ok_or(ap::Error::UnexpectedEnd)?;
                let connection = exchange.release()?;
                match self.method {
                    Method::SavedToken => {
                        authentication::accept_hello(self.identity, reply, false)?;
                    }
                    Method::Registration => {
                        authentication::accept_registration(self.identity, &reply)?
                    }
                }
                return Ok(Some(connection));
            }
            None => {}
        }
        self.exchange = Some(exchange);
        Ok(None)
    }
}
