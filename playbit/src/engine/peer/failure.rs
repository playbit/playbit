//! A private peer's failure becomes authority only after owner/scope admission.
use super::*;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Write};

const MAX_BYTES: usize = 1024;
const MESSAGE_BYTES: usize = 128;
const _: () = assert!(MESSAGE_BYTES * 6 + 128 <= MAX_BYTES);

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "cause", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Failure {
    ParticipantRevoked { volume: Id, account: Id },
    PrivateFileOwnerRead {},
    PrivateFileOwnerPreparationBusy {},
    Operation { message: String },
}
impl Failure {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err("private peer failure exceeds its byte limit".into());
        }
        let failure: Self = serde_json::from_slice(bytes)?;
        if matches!(&failure, Self::Operation { message } if message.len() > MESSAGE_BYTES) {
            return Err("private peer failure exceeds its message limit".into());
        }
        Ok(failure)
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParticipantRevoked { .. } => {
                formatter.write_str("peer reports participant revoked")
            }
            Self::PrivateFileOwnerRead {} => {
                formatter.write_str(crate::store::Error::PRIVATE_FILE_OWNER_READ)
            }
            Self::PrivateFileOwnerPreparationBusy {} => formatter.write_str(personal::OWNER_BUSY),
            Self::Operation { message } => formatter.write_str(message),
        }
    }
}
impl std::error::Error for Failure {}

struct Message(String);
impl Write for Message {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.0.len().checked_add(text.len()).is_none_or(|n| n > MESSAGE_BYTES) {
            return Err(fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}
pub(super) fn encode(error: &crate::store::Error) -> Result<Vec<u8>> {
    let failure = match error {
        crate::store::Error::ParticipantRevoked { volume, account } => {
            Failure::ParticipantRevoked { volume: *volume, account: *account }
        }
        crate::store::Error::AccessDenied(crate::store::Error::PRIVATE_FILE_OWNER_READ) => {
            Failure::PrivateFileOwnerRead {}
        }
        crate::store::Error::LocalContention(personal::OWNER_BUSY) => {
            Failure::PrivateFileOwnerPreparationBusy {}
        }
        _ => {
            let mut message = Message(String::with_capacity(MESSAGE_BYTES));
            write!(&mut message, "{error}")
                .map_err(|_| "private peer failure exceeds its message limit")?;
            Failure::Operation { message: message.0 }
        }
    };
    let bytes = serde_json::to_vec(&failure)?;
    assert!(bytes.len() <= MAX_BYTES, "bounded message escaping and fixed IDs fit one failure");
    Ok(bytes)
}

#[derive(Debug)]
pub(in crate::engine) struct VerifiedDenial {
    _volume: Id,
    _account: Id,
}
impl fmt::Display for VerifiedDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session participant revoked")
    }
}
impl std::error::Error for VerifiedDenial {}

#[allow(
    clippy::too_many_arguments,
    reason = "Explicit identities from the authenticated request; no second registry."
)]
fn denial_authorized(
    volume: Id,
    account: Id,
    requested: Id,
    root: Id,

    requester: Id,
    responder: Id,
    owner: Id,
) -> bool {
    account == requester && (volume == requested || volume == root) && responder == owner
}

pub(super) fn qualify<T>(
    result: Result<T>,
    store: &LocalStore,
    session: &Session,
    event: &PeerEvent,
) -> Result<T> {
    let Err(error) = result else { return result };
    let Some(Failure::ParticipantRevoked { volume, account }) = error.downcast_ref::<Failure>()
    else {
        return Err(error);
    };
    let requested = Id::parse(&event.volume_id)?;
    // Reject outside volumes before looking up any local descriptor.
    if ![requested, session.replica_volume()].contains(volume) {
        return Err("private peer denial is outside its dependency volumes".into());
    }
    if !denial_authorized(
        *volume,
        *account,
        requested,
        session.replica_volume(),
        store.account().id,
        Id::parse(&event.responder_account_id)?,
        store.volume(*volume)?.owner,
    ) {
        return Err("private peer denial lacks owner/account authority".into());
    }
    Err(VerifiedDenial { _volume: *volume, _account: *account }.into())
}

#[cfg(test)]
mod tests;
