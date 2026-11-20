//! Typed negative evidence. Diagnostic text never establishes revocation.
use super::*;
use crate::store::Id;
use std::fmt;

pub const MAX_DENIAL_BYTES: usize = 384;
const _: () = assert!(MAX_DENIAL_BYTES <= super::super::MAX_FRAME_LEN);

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "cause", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceDenial {
    ParticipantRevoked {
        account_id: Id,
        volume_id: Id,
        session_id: Option<String>,
        build_id: Option<String>,
    },
}
impl ServiceDenial {
    pub fn validate(&self) -> Result<(), &'static str> {
        let Self::ParticipantRevoked { session_id, build_id, .. } = self;
        match (session_id, build_id) {
            (None, None) => Ok(()),
            (Some(session), Some(build))
                if crate::identifier::valid(session, 32) && crate::identifier::valid(build, 32) =>
            {
                Ok(())
            }
            _ => Err("invalid AP denial binding"),
        }
    }

    /// The caller supplies the original request's current session/build binding.
    /// The service frame and operation identity have already been verified.
    pub fn matches_session(
        &self,
        account: Id,
        session: &str,
        build: &str,
        dependencies: impl IntoIterator<Item = Id>,
    ) -> bool {
        let Self::ParticipantRevoked { account_id, volume_id, session_id, build_id } = self;
        self.validate().is_ok()
            && session_id.as_deref() == Some(session)
            && build_id.as_deref() == Some(build)
            && dependency_matches(*account_id, *volume_id, account, dependencies)
    }
}

fn dependency_matches(
    reported: Id,
    volume: Id,
    account: Id,
    dependencies: impl IntoIterator<Item = Id>,
) -> bool {
    reported == account && dependencies.into_iter().any(|dependency| dependency == volume)
}

impl fmt::Display for ServiceDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session participant revoked")
    }
}
impl std::error::Error for ServiceDenial {}
